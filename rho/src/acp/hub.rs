//! The open sessions, shared by connections.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{AvailableCommand, SessionId};
use futures::future::{AbortHandle, AbortRegistration};

use crate::extension::{Context, NoteType};

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
            && !self.ctx.kept_alive()
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
/// its updates; a turn keeps running when its connection drops. A session
/// that is unused for `idle_timeout` is saved and unloaded.
pub struct Hub {
    pub(super) sessions: Mutex<HashMap<SessionId, Entry>>,
    next_run: AtomicU64,
    next_queued: AtomicU64,
    /// Held while a stored session opens, so two connections that open it at
    /// once share one session. shortcut: all opens wait for each other; a
    /// lock per session if session starts get slow.
    pub(super) opening: tokio::sync::Mutex<()>,
    idle_timeout: Duration,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            next_run: AtomicU64::default(),
            next_queued: AtomicU64::default(),
            opening: tokio::sync::Mutex::default(),
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

impl Hub {
    /// How long an unused session stays loaded, 30 minutes by default.
    pub fn with_idle_timeout(self, idle_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            ..self
        }
    }

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
        // A run saves it when it ends, see `Run::drop`.
        if entry.unused() {
            self.save(sid);
        }
    }

    /// The connection ended: it stops looking at all sessions. Their runs
    /// go on, and so does a held queue: nobody is left to let it go.
    /// Returns the sessions whose queue may start now.
    pub(super) fn detach(self: &Arc<Self>, conn: ConnId) -> Vec<SessionId> {
        let mut released = vec![];
        for (sid, e) in self.sessions.lock().unwrap().iter_mut() {
            let mut viewers = e.frontend.viewers.lock().unwrap();
            let before = viewers.len();
            viewers.retain(|v| v.conn != conn);
            if viewers.len() == before || !viewers.is_empty() {
                continue;
            }
            drop(viewers);
            if !e.queue.is_empty() {
                e.held = false;
                released.push(sid.clone());
            }
            if e.unused() {
                self.save(sid);
            }
        }
        released
    }

    /// Saves an unused session: notices since the last save, e.g. a
    /// background task's, survive a restart.
    fn save(self: &Arc<Self>, sid: &SessionId) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (hub, sid) = (self.clone(), sid.clone());
        rt.spawn(async move {
            if let Some((session, ..)) = hub.open(&sid) {
                session.lock().await.save().await;
            }
        });
    }

    fn unused(&self, sid: &SessionId) -> Option<bool> {
        self.sessions.lock().unwrap().get(sid).map(Entry::unused)
    }

    /// Runs while `sid` is open: once it was unused for `idle_timeout`,
    /// notes that, saves it and lets go of it. Polls, so the timeout is
    /// up to a 30th longer.
    pub(super) async fn unload_when_idle(hub: Weak<Self>, sid: SessionId) {
        let mut since = None;
        loop {
            let Some(timeout) = hub.upgrade().map(|h| h.idle_timeout) else {
                return;
            };
            tokio::time::sleep(timeout / 30).await;
            let Some(hub) = hub.upgrade() else { return };
            match hub.unused(&sid) {
                None => return,
                Some(false) => since = None,
                Some(true) => {
                    let since = *since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= timeout && hub.unload(&sid, timeout).await {
                        return;
                    }
                }
            }
        }
    }

    /// Unloads `sid` if it is still unused; `false` if it stays. Until the
    /// save is done it stays open: a client that opens it meanwhile
    /// attaches to it instead of loading a store the save has not reached.
    async fn unload(&self, sid: &SessionId, idle: Duration) -> bool {
        let Some((session, ctx, ..)) = self.open(sid) else {
            return true;
        };
        let mut session = session.lock().await;
        if self.unused(sid) != Some(true) {
            return false;
        }
        session.save().await;
        // A session only looked at stays as it was.
        if session.changed() {
            let minutes = idle.as_secs() / 60;
            let text = format!("Unloaded after {minutes} minutes without a client.");
            ctx.notify(NoteType::Info, "Session idle", &text, false);
            session.save().await;
        }
        let mut sessions = self.sessions.lock().unwrap();
        let unused = sessions.get(sid).is_some_and(Entry::unused);
        if unused {
            sessions.remove(sid);
        }
        unused
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
                self.hub.save(&self.sid);
            }
        }
    }
}
