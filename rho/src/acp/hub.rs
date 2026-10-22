//! The open sessions, shared by connections.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{AvailableCommand, SessionId};
use futures::future::{AbortHandle, AbortRegistration};

use crate::extension::Context;

use super::frontend::AcpFrontend;
use super::queue::Queued;
use super::{ConnId, SharedSession};

pub(super) struct Entry {
    pub(super) session: SharedSession,
    /// Runs that `session/cancel` aborts: a prompt, and a run for a
    /// triggered message (`watch_inbox`) it may be waiting for.
    pub(super) running: Vec<(u64, AbortHandle)>,
    /// Reachable while a turn holds the session, e.g. to switch profiles.
    pub(super) ctx: Context,
    pub(super) frontend: Arc<AcpFrontend>,
    /// For connections that attach while a turn holds the session.
    pub(super) commands: Vec<AvailableCommand>,
    pub(super) watch: AbortHandle,
    /// Prompts that start one by one when turns end.
    /// shortcut: in memory, lost on a restart; save them with the session
    /// if that loses messages that matter.
    pub(super) queue: VecDeque<Queued>,
    /// A turn was stopped or failed: the queue waits until one ends normally.
    pub(super) held: bool,
}

impl Entry {
    /// Nobody looks at it, nothing runs and nothing waits: it can go.
    fn unused(&self) -> bool {
        self.running.is_empty()
            && self.queue.is_empty()
            && self.frontend.viewers.lock().unwrap().is_empty()
    }

    fn publish_queue(&self) {
        self.frontend
            .set_queue(self.queue.iter().cloned().collect());
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.watch.abort();
    }
}

/// The open sessions, shared by the connections that use the same hub (all
/// connections of `rho daemon`). Every connection that opened a session sees
/// its updates; a turn keeps running when its connection drops.
#[derive(Default)]
pub struct Hub {
    pub(super) sessions: Mutex<HashMap<SessionId, Entry>>,
    next_run: AtomicU64,
    next_queued: AtomicU64,
    /// Held while a stored session opens, so two connections that open it at
    /// once share one session. shortcut: all opens wait for each other; a
    /// lock per session if session starts get slow.
    pub(super) opening: tokio::sync::Mutex<()>,
}

impl Hub {
    /// The session, if `conn` opened it.
    pub(super) fn session(
        &self,
        sid: &SessionId,
        conn: ConnId,
    ) -> Option<(SharedSession, Context, Arc<AcpFrontend>)> {
        let sessions = self.sessions.lock().unwrap();
        let e = sessions.get(sid)?;
        e.frontend
            .viewing(conn)
            .then(|| (e.session.clone(), e.ctx.clone(), e.frontend.clone()))
    }

    /// `name` is one of the open session's slash commands.
    pub(super) fn is_command(&self, sid: &SessionId, name: &str) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(sid)
            .is_some_and(|e| e.commands.iter().any(|c| c.name == name))
    }

    /// An open session, for a connection that attaches to it.
    pub(super) fn open(
        &self,
        sid: &SessionId,
    ) -> Option<(
        SharedSession,
        Context,
        Arc<AcpFrontend>,
        Vec<AvailableCommand>,
    )> {
        let sessions = self.sessions.lock().unwrap();
        let e = sessions.get(sid)?;
        Some((
            e.session.clone(),
            e.ctx.clone(),
            e.frontend.clone(),
            e.commands.clone(),
        ))
    }

    /// Registers a run that a cancel notification should abort, until the
    /// returned `Run` is dropped. With `conn`, only if that connection
    /// opened the session.
    pub(super) fn start_run(
        self: &Arc<Self>,
        sid: &SessionId,
        conn: Option<ConnId>,
    ) -> Option<(SharedSession, Arc<AcpFrontend>, Run, AbortRegistration)> {
        let (abort, registration) = AbortHandle::new_pair();
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions.get_mut(sid)?;
        if conn.is_some_and(|c| !entry.frontend.viewing(c)) {
            return None;
        }
        if entry.running.is_empty() {
            entry.frontend.set_busy(true);
        }
        entry.running.push((id, abort));
        let run = Run {
            hub: self.clone(),
            sid: sid.clone(),
            id,
        };
        Some((
            entry.session.clone(),
            entry.frontend.clone(),
            run,
            registration,
        ))
    }

    /// Adds a prompt to the queue of `sid`, if `conn` opened it.
    pub(super) fn enqueue(&self, sid: &SessionId, conn: ConnId, text: String) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(entry) = sessions.get_mut(sid).filter(|e| e.frontend.viewing(conn)) else {
            return false;
        };
        let id = self.next_queued.fetch_add(1, Ordering::Relaxed);
        entry.queue.push_back(Queued { id, text });
        entry.publish_queue();
        true
    }

    /// Removes queued prompt `id` of `sid`, if `conn` opened it.
    pub(super) fn unqueue(&self, sid: &SessionId, conn: ConnId, id: u64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(entry) = sessions.get_mut(sid).filter(|e| e.frontend.viewing(conn)) else {
            return false;
        };
        entry.queue.retain(|q| q.id != id);
        entry.publish_queue();
        true
    }

    /// The next queued prompt, unless the queue is held. `ended`: a turn
    /// just ended, normally or not; `None`: only if nothing runs.
    pub(super) fn take_queued(&self, sid: &SessionId, ended: Option<bool>) -> Option<String> {
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions.get_mut(sid)?;
        match ended {
            Some(normally) => entry.held &= !normally,
            None if !entry.running.is_empty() => return None,
            None => {}
        }
        if entry.held {
            return None;
        }
        let next = entry.queue.pop_front()?;
        entry.publish_queue();
        Some(next.text)
    }

    /// A run is registered for the session.
    pub(super) fn busy(&self, sid: &SessionId) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(sid).is_some_and(|e| !e.running.is_empty())
    }

    /// Aborts the session's runs, if `conn` opened it.
    pub(super) fn cancel(&self, sid: &SessionId, conn: ConnId) {
        if let Some(entry) = self.sessions.lock().unwrap().get_mut(sid)
            && entry.frontend.viewing(conn)
        {
            entry.held = true;
            for (_, abort) in entry.running.drain(..) {
                abort.abort();
            }
        }
    }

    /// `conn` stops looking at `sid` (`session/close`). The last one to go
    /// cancels the runs.
    pub(super) fn close(self: &Arc<Self>, sid: &SessionId, conn: ConnId) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(entry) = sessions.get_mut(sid) else {
            return;
        };
        let mut viewers = entry.frontend.viewers.lock().unwrap();
        let before = viewers.len();
        viewers.retain(|v| v.conn != conn);
        if viewers.len() == before || !viewers.is_empty() {
            return;
        }
        drop(viewers);
        // Nobody is left to see the queue run.
        entry.queue.clear();
        for (_, abort) in entry.running.drain(..) {
            abort.abort();
        }
        // A run lets go of it when it ends, see `Run::drop`.
        if entry.unused() {
            self.release(sid);
        }
    }

    /// The connection ended: it stops looking at all sessions. Their runs
    /// go on.
    pub(super) fn detach(self: &Arc<Self>, conn: ConnId) {
        for (sid, e) in self.sessions.lock().unwrap().iter() {
            let mut viewers = e.frontend.viewers.lock().unwrap();
            let before = viewers.len();
            viewers.retain(|v| v.conn != conn);
            if viewers.len() != before && viewers.is_empty() && e.running.is_empty() {
                self.release(sid);
            }
        }
    }

    /// Saves an unused session (notices since the last save, e.g. a
    /// background task's), then lets go of it if it is still unused. Until
    /// then it stays open: a client that opens it meanwhile attaches to it
    /// instead of loading a store the save has not reached yet.
    fn release(self: &Arc<Self>, sid: &SessionId) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (hub, sid) = (self.clone(), sid.clone());
        rt.spawn(async move {
            let Some(session) = hub.open(&sid).map(|o| o.0) else {
                return;
            };
            session.lock().await.save().await;
            let mut sessions = hub.sessions.lock().unwrap();
            if sessions.get(&sid).is_some_and(Entry::unused) {
                sessions.remove(&sid);
            }
        });
    }
}

/// A run registered with `Hub::start_run`.
pub(super) struct Run {
    hub: Arc<Hub>,
    sid: SessionId,
    id: u64,
}

impl Drop for Run {
    fn drop(&mut self) {
        let mut sessions = self.hub.sessions.lock().unwrap();
        if let Some(entry) = sessions.get_mut(&self.sid) {
            entry.running.retain(|(id, _)| *id != self.id);
            if entry.running.is_empty() {
                entry.frontend.set_busy(false);
            }
            if entry.unused() {
                self.hub.release(&self.sid);
            }
        }
    }
}
