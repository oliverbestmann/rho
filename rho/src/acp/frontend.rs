//! One session's frontend, for every connection that opened it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAction, ElicitationFormMode, ElicitationSessionScope,
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionConfigOption, SessionId, SessionNotification, SessionUpdate, ToolCallUpdate,
    ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo, JsonRpcRequest};
use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use futures::stream::FuturesUnordered;
use tokio::sync::broadcast;

use crate::agent::AgentEvent;
use crate::extension::{BoxFuture, Form, FormReply, Frontend};
use crate::provider::{ToolCallId, ToolName};
use crate::tools::DynTool;

use super::update::{
    LastUsage, Shown, busy_update, form_answers, form_schema, red, terminal_output, to_update,
};
use super::{ConnId, shell};

/// Output a terminal card shows of one call, so a flood does not swamp the
/// client; the model's copy is cut separately.
const STREAM_CAP: usize = 1024 * 1024;

/// What a running terminal call has shown: `$ command`, then the output.
struct Streamed {
    text: String,
    /// The tool streamed output, so `ToolEnd` need not send it.
    from_tool: bool,
}

/// A connection that opened the session.
#[derive(Clone)]
pub(super) struct Viewer {
    pub(super) conn: ConnId,
    pub(super) cx: ConnectionTo<Client>,
    /// The client can show elicitation forms, see `Frontend::ask`.
    pub(super) forms: bool,
    /// The client shows `Bash` calls as terminals.
    pub(super) terminal: bool,
    /// The client knows the session: it gets confirmations and forms.
    pub(super) ready: bool,
    /// Updates held while the session starts, until `ready`: a client does
    /// not know a new session before the `session/new` response. Replays
    /// (`To::Only`) are not held.
    pub(super) held: Option<Vec<SessionUpdate>>,
}

/// Which viewers an update goes to.
#[derive(Clone, Copy)]
pub(super) enum To {
    All,
    Only(ConnId),
    Except(ConnId),
}

impl To {
    fn includes(self, conn: ConnId) -> bool {
        match self {
            To::All => true,
            To::Only(c) => c == conn,
            To::Except(c) => c != conn,
        }
    }
}

/// One session's frontend, for every connection that opened it.
pub(super) struct AcpFrontend {
    sid: SessionId,
    /// For `Bash` calls shown as terminals.
    cwd: PathBuf,
    notices: AtomicU64,
    pub(super) viewers: Mutex<Vec<Viewer>>,
    /// Viewers that got ready, for the confirmations and forms waiting for
    /// an answer.
    ready: broadcast::Sender<Viewer>,
    /// The `tool_call` updates (with and without terminal) of the running
    /// tool calls. A viewer that attaches during a turn replays the store,
    /// which has them only after the tools ran.
    running: Mutex<Vec<(ToolCallId, SessionUpdate, SessionUpdate)>>,
    /// How running tool calls were shown, by id. `ToolEnd` replaces the
    /// call's content, so it has to repeat the command or the questions.
    /// Terminals stay terminals here, see `Shown::for_viewer`.
    shown: Mutex<HashMap<ToolCallId, Shown>>,
    /// By id, for viewers that attach while the call runs.
    streamed: Mutex<HashMap<ToolCallId, Streamed>>,
    published: Mutex<Published>,
    /// A run is registered (see `Hub::start_run`).
    busy: AtomicBool,
    /// The session's tools by name, for replays (also while a turn holds
    /// the session). Set by `register`.
    pub(super) tools: OnceLock<HashMap<ToolName, Arc<dyn DynTool>>>,
}

impl AcpFrontend {
    pub(super) fn new(sid: &SessionId, cwd: &Path) -> Arc<Self> {
        Arc::new(Self {
            sid: sid.clone(),
            cwd: cwd.to_path_buf(),
            notices: AtomicU64::new(0),
            viewers: Mutex::default(),
            ready: broadcast::channel(16).0,
            running: Mutex::default(),
            shown: Mutex::default(),
            streamed: Mutex::default(),
            published: Mutex::default(),
            busy: AtomicBool::new(false),
            tools: OnceLock::new(),
        })
    }

    /// Tells all viewers whether a turn runs, as `_meta.rho.running` of a
    /// `session_info_update`: ACP has no update for it.
    pub(super) fn set_busy(&self, on: bool) {
        self.busy.store(on, Ordering::Relaxed);
        self.send_update(busy_update(on), To::All);
    }

    /// `viewer` gets the updates from now on.
    pub(super) fn attach(&self, viewer: Viewer) {
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|v| v.conn != viewer.conn);
        viewers.push(viewer);
    }

    pub(super) fn viewing(&self, conn: ConnId) -> bool {
        self.viewers.lock().unwrap().iter().any(|v| v.conn == conn)
    }

    /// `conn` knows the session now: it gets the held updates (or the
    /// running tool calls) and the questions waiting for an answer.
    pub(super) fn ready(&self, conn: ConnId) {
        let mut viewers = self.viewers.lock().unwrap();
        let Some(v) = viewers.iter_mut().find(|v| v.conn == conn) else {
            return;
        };
        v.ready = true;
        let held = v.held.take();
        // With these it got the running tool calls and the busy state.
        let caught_up = held.is_some();
        for update in held.into_iter().flatten() {
            self.deliver(v, update, To::Only(conn));
        }
        if !caught_up {
            for (id, terminal, plain) in self.running.lock().unwrap().iter() {
                let update = if v.terminal { terminal } else { plain };
                self.deliver(v, update.clone(), To::Only(conn));
                let shown = self
                    .streamed
                    .lock()
                    .unwrap()
                    .get(id)
                    .map(|s| s.text.clone());
                if let Some(text) = shown.filter(|_| v.terminal) {
                    self.deliver(v, terminal_output(id, &text), To::Only(conn));
                }
            }
            if self.busy.load(Ordering::Relaxed) {
                self.deliver(v, busy_update(true), To::Only(conn));
            }
        }
        // Nobody waiting is fine.
        let _ = self.ready.send(v.clone());
    }

    /// Sends `request` to every ready viewer that `fits`, also to those that
    /// get ready while it waits. The first answer wins; the other requests
    /// are dropped, which cancels them. Errors do not count: a client that
    /// went away does not answer for the others.
    async fn first_answer<R>(&self, fits: impl Fn(&Viewer) -> bool, request: R) -> R::Response
    where
        R: JsonRpcRequest + Clone,
    {
        // Under the lock, so a viewer is in the list or in `ready`.
        let (mut ready, viewers) = {
            let viewers = self.viewers.lock().unwrap();
            let fitting: Vec<_> = viewers
                .iter()
                .filter(|v| v.ready && fits(v))
                .cloned()
                .collect();
            (self.ready.subscribe(), fitting)
        };
        let mut asked: HashMap<ConnId, AbortHandle> = HashMap::new();
        let mut ask = |v: &Viewer| {
            let (abort, registration) = AbortHandle::new_pair();
            // Loaded again: the client may have dropped the old request.
            if let Some(old) = asked.insert(v.conn, abort) {
                old.abort();
            }
            let answer = v.cx.send_request(request.clone()).block_task();
            Abortable::new(answer, registration)
        };
        let mut pending: FuturesUnordered<_> = viewers.iter().map(&mut ask).collect();
        loop {
            tokio::select! {
                Some(answer) = pending.next() => {
                    if let Ok(Ok(response)) = answer {
                        return response;
                    }
                }
                viewer = ready.recv() => {
                    if let Ok(v) = viewer && fits(&v) {
                        pending.push(ask(&v));
                    }
                }
            }
        }
    }

    pub(super) fn remember(&self, options: Vec<SessionConfigOption>) -> Vec<SessionConfigOption> {
        options.clone_into(&mut self.published.lock().unwrap().options);
        options
    }

    pub(super) fn options(&self) -> Vec<SessionConfigOption> {
        self.published.lock().unwrap().options.clone()
    }

    pub(super) fn send(&self, ev: AgentEvent<'_>, to: To) {
        // Before session/new answered there are no options to update.
        if matches!(ev, AgentEvent::Profile(_)) && self.options().is_empty() {
            return;
        }
        if let AgentEvent::ToolStream { id, chunk, stderr } = ev {
            return self.stream(id, chunk, stderr, to);
        }
        let n = self.notices.fetch_add(1, Ordering::Relaxed);
        let shown = match &ev {
            AgentEvent::ToolStart {
                id,
                name,
                input,
                description,
                ..
            } => {
                let shown = Shown::of(name, input, *description, Some(&self.cwd));
                if let Some(shown) = &shown {
                    self.shown
                        .lock()
                        .unwrap()
                        .insert((*id).clone(), shown.clone());
                }
                shown
            }
            AgentEvent::ToolEnd { id, .. } => {
                let from_tool = self.streamed.lock().unwrap().remove(*id);
                let mut shown = self.shown.lock().unwrap().remove(*id);
                if let Some(Shown::Terminal { streamed, .. }) = &mut shown {
                    *streamed = from_tool.is_some_and(|s| s.from_tool);
                }
                shown
            }
            _ => None,
        };
        // Built even without viewers: it records what a later one is sent.
        let mut published = self.published.lock().unwrap();
        let terminal = to_update(ev, n, shown.as_ref(), &self.cwd, &mut published);
        let plain = match &shown {
            Some(s @ Shown::Terminal { .. }) => {
                to_update(ev, n, Some(&s.for_viewer(false)), &self.cwd, &mut published)
            }
            _ => terminal.clone(),
        };
        drop(published);
        // A replay (`To::Only`) shows finished calls.
        if matches!(to, To::All) {
            let mut running = self.running.lock().unwrap();
            match ev {
                AgentEvent::ToolStart { id, .. } => {
                    running.push((id.clone(), terminal.clone(), plain.clone()));
                }
                AgentEvent::ToolEnd { id, .. } => running.retain(|(r, _, _)| r != id),
                _ => {}
            }
        }
        for v in self.viewers.lock().unwrap().iter_mut() {
            if to.includes(v.conn) {
                let update = if v.terminal { &terminal } else { &plain };
                self.deliver(v, update.clone(), to);
            }
        }
        if let (AgentEvent::ToolStart { id, .. }, Some(Shown::Terminal { command, .. })) =
            (&ev, &shown)
        {
            // Later lines line up under the first, after the `$ `.
            let text = format!("$ {}\n", shell::format(command).replace('\n', "\n  "));
            self.send_terminal(id, &text, to);
            let started = Streamed {
                text,
                from_tool: false,
            };
            self.streamed.lock().unwrap().insert((*id).clone(), started);
        }
    }

    /// Streamed output of a terminal call, stderr in red, up to `STREAM_CAP`.
    fn stream(&self, id: &ToolCallId, chunk: &str, stderr: bool, to: To) {
        let data = {
            let mut streamed = self.streamed.lock().unwrap();
            // Not a terminal, or it ended.
            let Some(s) = streamed.get_mut(id) else {
                return;
            };
            s.from_tool = true;
            if s.text.len() >= STREAM_CAP {
                return;
            }
            let mut data = if stderr {
                red(chunk)
            } else {
                chunk.to_string()
            };
            if s.text.len() + data.len() >= STREAM_CAP {
                data.push_str("\n[more output not shown]\n");
            }
            s.text.push_str(&data);
            data
        };
        self.send_terminal(id, &data, to);
    }

    fn send_terminal(&self, id: &ToolCallId, data: &str, to: To) {
        let update = terminal_output(id, data);
        for v in self.viewers.lock().unwrap().iter_mut() {
            if v.terminal && to.includes(v.conn) {
                self.deliver(v, update.clone(), to);
            }
        }
    }

    pub(super) fn send_update(&self, update: SessionUpdate, to: To) {
        for v in self.viewers.lock().unwrap().iter_mut() {
            if to.includes(v.conn) {
                self.deliver(v, update.clone(), to);
            }
        }
    }

    /// Sends `update` to `v`, or holds it while the session starts.
    fn deliver(&self, v: &mut Viewer, update: SessionUpdate, to: To) {
        match &mut v.held {
            Some(held) if !matches!(to, To::Only(_)) => held.push(update),
            // Dropped updates only affect display; the turn continues.
            _ => {
                let _ =
                    v.cx.send_notification(SessionNotification::new(self.sid.clone(), update));
            }
        }
    }
}

/// What the client was last sent, to send again with a change.
#[derive(Default)]
pub(super) struct Published {
    pub(super) usage: LastUsage,
    /// `config_option_update` carries the full set, so a profile switch
    /// sends these again.
    pub(super) options: Vec<SessionConfigOption>,
}

impl Frontend for AcpFrontend {
    fn event(&self, ev: AgentEvent<'_>) {
        self.send(ev, To::All);
    }
    fn confirm<'a>(
        &'a self,
        tool_call_id: Option<&'a ToolCallId>,
        title: &'a str,
    ) -> BoxFuture<'a, bool> {
        self.confirm_with(tool_call_id, title, "Allow", "Reject")
    }

    fn confirm_with<'a>(
        &'a self,
        tool_call_id: Option<&'a ToolCallId>,
        title: &'a str,
        allow: &'a str,
        reject: &'a str,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut fields = ToolCallUpdateFields::new().title(title.to_string());
            // Clients show one line of title; the rest goes into the card.
            // Only for our own card, so a real tool call keeps its content.
            if tool_call_id.is_none()
                && let Some((first, rest)) = title.split_once('\n')
            {
                fields = ToolCallUpdateFields::new()
                    .title(first.to_string())
                    .content(vec![rest.into()]);
            }
            // A fresh id per card: clients update a known id in place, so a
            // fixed one would show a new question at the first card's place.
            let id = match tool_call_id {
                Some(id) => id.as_str().to_string(),
                None => format!("confirm-{:016x}", getrandom::u64().unwrap_or_default()),
            };
            let call = ToolCallUpdate::new(id, fields);
            let request = RequestPermissionRequest::new(
                self.sid.clone(),
                call,
                vec![
                    PermissionOption::new("allow", allow, PermissionOptionKind::AllowOnce),
                    PermissionOption::new("reject", reject, PermissionOptionKind::RejectOnce),
                ],
            );
            let r = self.first_answer(|_| true, request).await;
            matches!(r.outcome, RequestPermissionOutcome::Selected(s) if &*s.option_id.0 == "allow")
        })
    }

    fn ask<'a>(&'a self, form: &'a Form) -> BoxFuture<'a, FormReply> {
        Box::pin(async move {
            // Without viewers it waits for one.
            let unsupported = {
                let viewers = self.viewers.lock().unwrap();
                !viewers.is_empty() && !viewers.iter().any(|v| v.forms)
            };
            if unsupported {
                return FormReply::Unsupported;
            }
            let request = CreateElicitationRequest::new(
                ElicitationFormMode::new(
                    ElicitationSessionScope::new(self.sid.clone()),
                    form_schema(form),
                ),
                form.message.clone(),
            );
            match self.first_answer(|v| v.forms, request).await.action {
                ElicitationAction::Accept(a) => FormReply::Answers(form_answers(a.content)),
                _ => FormReply::Declined,
            }
        })
    }
}
