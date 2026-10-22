//! One session's frontend, for every connection that opened it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent_client_protocol::schema::v1::{
    ContentChunk, CreateElicitationRequest, ElicitationAction, ElicitationFormMode,
    ElicitationSessionScope, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, SessionConfigOption, SessionId, SessionNotification, SessionUpdate,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo, JsonRpcRequest};
use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use futures::stream::FuturesUnordered;
use tokio::sync::broadcast;

use crate::agent::AgentEvent;
use crate::extension::{BoxFuture, Form, FormReply, Frontend, STREAM_CAP, STREAM_CUT, red};
use crate::provider::{ToolCallId, ToolName};
use crate::tools::DynTool;

use super::queue::Queued;
use super::update::{
    LastUsage, Shown, busy_update, form_answers, form_schema, queue_update, terminal_output,
    to_update, usage_update,
};
use super::{ConnId, shell};

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

/// Which viewers an update goes to. `All` and `Record` also add it to the
/// transcript.
#[derive(Clone, Copy)]
pub(super) enum To {
    All,
    Only(ConnId),
    Except(ConnId),
    /// Nobody: the history of a session that opens, for later viewers.
    Record,
}

impl To {
    fn includes(self, conn: ConnId) -> bool {
        match self {
            To::All => true,
            To::Only(c) => c == conn,
            To::Except(c) => c != conn,
            To::Record => false,
        }
    }

    fn records(self) -> bool {
        matches!(self, To::All | To::Record)
    }
}

/// What the session showed, for viewers that attach: the current and the
/// previous epoch. They get it from here, not from the store, so it has
/// what is not saved yet (streamed text, a running call's output), and
/// nothing comes twice or goes missing between a read and a save.
#[derive(Default)]
struct Transcript {
    entries: Vec<Entry>,
    /// Where the previous and the current epoch start in `entries`.
    epochs: [usize; 2],
}

enum Entry {
    Text(String),
    Thinking(String),
    /// For terminal viewers, and for the others.
    Update {
        terminal: Box<SessionUpdate>,
        plain: Box<SessionUpdate>,
    },
    /// Output of a call shown as a terminal, for terminal viewers.
    Output {
        id: ToolCallId,
        data: String,
    },
}

impl Transcript {
    /// Adds `entry`, merged into the last one if it continues it.
    fn push(&mut self, entry: Entry) {
        // Not into the previous epoch.
        let last = (self.entries.len() > self.epochs[1])
            .then(|| self.entries.last_mut())
            .flatten();
        match (last, entry) {
            (Some(Entry::Text(s)), Entry::Text(t))
            | (Some(Entry::Thinking(s)), Entry::Thinking(t)) => s.push_str(&t),
            (
                Some(Entry::Output { id, data }),
                Entry::Output {
                    id: next,
                    data: more,
                },
            ) if *id == next => data.push_str(&more),
            (_, entry) => self.entries.push(entry),
        }
    }

    /// A compaction: the epoch before the current one is dropped.
    fn new_epoch(&mut self) {
        self.entries.drain(..self.epochs[1]);
        self.epochs = [0, self.entries.len()];
    }

    fn updates(&self, terminal: bool) -> impl Iterator<Item = SessionUpdate> + '_ {
        self.entries.iter().filter_map(move |e| match e {
            Entry::Text(t) => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                t.as_str().into(),
            ))),
            Entry::Thinking(t) => Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                t.as_str().into(),
            ))),
            Entry::Update { terminal: t, .. } if terminal => Some((**t).clone()),
            Entry::Update { plain, .. } => Some((**plain).clone()),
            Entry::Output { id, data } => terminal.then(|| terminal_output(id, data)),
        })
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
    /// Locked after `viewers`, so a viewer gets each update once: from
    /// here when it attaches, or live.
    transcript: Mutex<Transcript>,
    /// How running tool calls were shown, by id. `ToolEnd` replaces the
    /// call's content, so it has to repeat the command or the questions.
    /// Terminals stay terminals here, see `Shown::for_viewer`.
    shown: Mutex<HashMap<ToolCallId, Shown>>,
    /// By id, for viewers that attach while the call runs.
    streamed: Mutex<HashMap<ToolCallId, Streamed>>,
    published: Mutex<Published>,
    /// A run is registered (see `Hub::start_run`).
    busy: AtomicBool,
    /// The queue as last sent, see `Hub::enqueue`.
    queue: Mutex<Vec<Queued>>,
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
            transcript: Mutex::default(),
            shown: Mutex::default(),
            streamed: Mutex::default(),
            published: Mutex::default(),
            busy: AtomicBool::new(false),
            queue: Mutex::default(),
            tools: OnceLock::new(),
        })
    }

    /// Tells all viewers whether a turn runs, as `_meta.rho.running` of a
    /// `session_info_update`: ACP has no update for it.
    pub(super) fn set_busy(&self, on: bool) {
        self.busy.store(on, Ordering::Relaxed);
        self.send_update(busy_update(on), To::All);
    }

    /// Tells all viewers the queued prompts, as `_meta.rho.queue`.
    pub(super) fn set_queue(&self, queue: Vec<Queued>) {
        let update = queue_update(&queue);
        *self.queue.lock().unwrap() = queue;
        self.send_update(update, To::All);
    }

    /// `viewer` gets the updates from now on.
    pub(super) fn attach(&self, viewer: Viewer) {
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|v| v.conn != viewer.conn);
        viewers.push(viewer);
    }

    /// `attach`, after sending `viewer` the transcript.
    pub(super) fn attach_replayed(&self, mut viewer: Viewer) {
        let mut viewers = self.viewers.lock().unwrap();
        let conn = viewer.conn;
        for update in self.transcript.lock().unwrap().updates(viewer.terminal) {
            self.deliver(&mut viewer, update, To::Only(conn));
        }
        viewers.retain(|v| v.conn != conn);
        viewers.push(viewer);
    }

    /// Runs `record` (which sends `To::Record`) so that what it records goes
    /// before the transcript so far: the stored history goes before the
    /// notices of the session's start.
    pub(super) fn record_first(&self, record: impl FnOnce()) {
        let later = std::mem::take(&mut *self.transcript.lock().unwrap());
        record();
        let mut t = self.transcript.lock().unwrap();
        for entry in later.entries {
            t.push(entry);
        }
    }

    /// Adds `update` to the transcript, for every viewer.
    pub(super) fn record(&self, update: SessionUpdate) {
        self.transcript.lock().unwrap().push(Entry::Update {
            terminal: Box::new(update.clone()),
            plain: Box::new(update),
        });
    }

    /// A prompt from `conn`, or from the queue: the other viewers see it,
    /// later ones too.
    pub(super) fn echo(&self, update: SessionUpdate, conn: Option<ConnId>) {
        let mut viewers = self.viewers.lock().unwrap();
        self.record(update.clone());
        let to = conn.map_or(To::All, To::Except);
        for v in viewers.iter_mut().filter(|v| to.includes(v.conn)) {
            self.deliver(v, update.clone(), to);
        }
    }

    pub(super) fn new_epoch(&self) {
        self.transcript.lock().unwrap().new_epoch();
    }

    pub(super) fn viewing(&self, conn: ConnId) -> bool {
        self.viewers.lock().unwrap().iter().any(|v| v.conn == conn)
    }

    /// `conn` knows the session now: it gets the held updates (or the busy
    /// state) and the questions waiting for an answer.
    pub(super) fn ready(&self, conn: ConnId) {
        let mut viewers = self.viewers.lock().unwrap();
        let Some(v) = viewers.iter_mut().find(|v| v.conn == conn) else {
            return;
        };
        v.ready = true;
        let held = v.held.take();
        // With these it got the busy state.
        let caught_up = held.is_some();
        for update in held.into_iter().flatten() {
            self.deliver(v, update, To::Only(conn));
        }
        if !caught_up && self.busy.load(Ordering::Relaxed) {
            self.deliver(v, busy_update(true), To::Only(conn));
        }
        let queue = self.queue.lock().unwrap();
        if !caught_up && !queue.is_empty() {
            self.deliver(v, queue_update(&queue), To::Only(conn));
        }
        drop(queue);
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
        // The history: what a later viewer must see too.
        let entry = match ev {
            AgentEvent::Text(t) => Some(Entry::Text(t.into())),
            AgentEvent::Thinking(t) => Some(Entry::Thinking(t.into())),
            AgentEvent::ToolStart { .. }
            | AgentEvent::ToolEnd { .. }
            | AgentEvent::Notice { .. }
            | AgentEvent::Progress { .. } => Some(Entry::Update {
                terminal: Box::new(terminal.clone()),
                plain: Box::new(plain.clone()),
            }),
            _ => None,
        };
        let mut viewers = self.viewers.lock().unwrap();
        if let Some(entry) = entry.filter(|_| to.records()) {
            self.transcript.lock().unwrap().push(entry);
        }
        for v in viewers.iter_mut() {
            if to.includes(v.conn) {
                let update = if v.terminal { &terminal } else { &plain };
                self.deliver(v, update.clone(), to);
            }
        }
        drop(viewers);
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
                data.push_str(STREAM_CUT);
            }
            s.text.push_str(&data);
            data
        };
        self.send_terminal(id, &data, to);
    }

    fn send_terminal(&self, id: &ToolCallId, data: &str, to: To) {
        let update = terminal_output(id, data);
        let mut viewers = self.viewers.lock().unwrap();
        if to.records() {
            self.transcript.lock().unwrap().push(Entry::Output {
                id: id.clone(),
                data: data.into(),
            });
        }
        for v in viewers.iter_mut() {
            if v.terminal && to.includes(v.conn) {
                self.deliver(v, update.clone(), to);
            }
        }
    }

    /// Sends the usage last sent to all viewers to `conn` alone, if any.
    pub(super) fn resend_usage(&self, conn: ConnId, context_window: u64) {
        let published = self.published.lock().unwrap();
        if published.usage.is_empty() {
            return;
        }
        let update = usage_update(&published.usage, context_window);
        drop(published);
        self.send_update(update, To::Only(conn));
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

    fn compacted(&self) {
        self.new_epoch();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(t: &Transcript, terminal: bool) -> Vec<String> {
        t.updates(terminal)
            .map(|u| match u {
                SessionUpdate::AgentMessageChunk(c) | SessionUpdate::AgentThoughtChunk(c) => {
                    serde_json::to_value(&c.content).unwrap()["text"]
                        .as_str()
                        .unwrap()
                        .to_string()
                }
                other => serde_json::to_value(&other).unwrap()["_meta"]["terminal_output"]["data"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            })
            .collect()
    }

    #[test]
    fn transcript_merges_chunks_and_keeps_two_epochs() {
        let mut t = Transcript::default();
        let out = |id: &str, data: &str| Entry::Output {
            id: ToolCallId::new(id),
            data: data.into(),
        };
        t.push(Entry::Text("a".into()));
        t.push(Entry::Text("b".into()));
        t.push(Entry::Thinking("c".into()));
        t.push(out("t1", "x"));
        t.push(out("t1", "y"));
        t.push(out("t2", "z"));
        assert_eq!(texts(&t, true), ["ab", "c", "xy", "z"]);
        // Terminal output only for terminals.
        assert_eq!(texts(&t, false), ["ab", "c"]);

        t.new_epoch();
        t.push(Entry::Text("second".into()));
        t.new_epoch();
        t.push(Entry::Text("third".into()));
        // The first epoch is gone; text does not merge across epochs.
        assert_eq!(texts(&t, false), ["second", "third"]);
    }
}
