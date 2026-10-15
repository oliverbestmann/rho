//! The open sessions, shared by connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{AvailableCommand, SessionId};
use futures::future::{AbortHandle, AbortRegistration};

use crate::extension::Context;

use super::frontend::AcpFrontend;
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
}

impl Entry {
    /// Nobody looks at it and nothing runs: it can go.
    fn unused(&self) -> bool {
        self.running.is_empty() && self.frontend.viewers.lock().unwrap().is_empty()
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
            for (_, abort) in entry.running.drain(..) {
                abort.abort();
            }
        }
    }

    /// `conn` stops looking at `sid` (`session/close`). The last one to go
    /// cancels the runs.
    pub(super) fn close(&self, sid: &SessionId, conn: ConnId) {
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
        for (_, abort) in entry.running.drain(..) {
            abort.abort();
        }
        // A run lets go of it when it ends, see `Run::drop`.
        if entry.unused() {
            sessions.remove(sid);
        }
    }

    /// The connection ended: it stops looking at all sessions. Their runs
    /// go on.
    pub(super) fn detach(&self, conn: ConnId) {
        self.sessions.lock().unwrap().retain(|_, e| {
            e.frontend
                .viewers
                .lock()
                .unwrap()
                .retain(|v| v.conn != conn);
            !e.unused()
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
                sessions.remove(&self.sid);
            }
        }
    }
}
