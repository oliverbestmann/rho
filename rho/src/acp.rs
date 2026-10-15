//! Agent Client Protocol frontend: exposes the agent loop to editors (Zed).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, CloseSessionRequest, CloseSessionResponse, ConfigOptionUpdate,
    ContentBlock, ContentChunk, Cost, CreateElicitationRequest, Diff, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationSchema, ElicitationSessionScope,
    EmbeddedResourceResource, EnumOption, ImageContent, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, Meta, MultiSelectPropertySchema, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, ResumeSessionRequest,
    ResumeSessionResponse, SessionAdditionalDirectoriesCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionId, SessionInfo as AcpSessionInfo, SessionInfoUpdate,
    SessionListCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
    StringPropertySchema, Terminal, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput, Usage as AcpUsage,
    UsageUpdate,
};
use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Error, JsonRpcRequest, Responder};
use futures::StreamExt;
use futures::future::{AbortHandle, AbortRegistration, Abortable};
use futures::stream::FuturesUnordered;
use tokio::sync::broadcast;

use crate::agent::{Agent, AgentEvent, Session};
use crate::edit::EDIT;
use crate::extension::{
    Answers, BoxFuture, CommandName, Context, Field, FieldKey, FieldKind, Form, FormReply,
    Frontend, ProfileId,
};
use crate::provider::{self, Content, ImageSource, Role, Thinking, ToolCallId, ToolName, Usage};
use crate::store::{self, Store};
use crate::tools;

type SharedSession = Arc<tokio::sync::Mutex<Session>>;

/// One ACP connection, unique in the process.
type ConnId = u64;

static NEXT_CONN: AtomicU64 = AtomicU64::new(0);

struct Entry {
    session: SharedSession,
    /// Runs that `session/cancel` aborts: a prompt, and a run for a
    /// triggered message (`watch_inbox`) it may be waiting for.
    running: Vec<(u64, AbortHandle)>,
    /// Reachable while a turn holds the session, e.g. to switch profiles.
    ctx: Context,
    frontend: Arc<AcpFrontend>,
    /// For connections that attach while a turn holds the session.
    commands: Vec<AvailableCommand>,
    watch: AbortHandle,
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

/// What the client told us in `initialize`.
#[derive(Default)]
struct ClientCaps {
    /// Can show elicitation forms.
    forms: AtomicBool,
    /// Understands Zed's `_meta` terminal output for tool calls.
    terminal_output: AtomicBool,
}

/// The open sessions, shared by the connections that use the same hub (all
/// connections of `rho daemon`). Every connection that opened a session sees
/// its updates; a turn keeps running when its connection drops.
#[derive(Default)]
pub struct Hub {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    next_run: AtomicU64,
}

impl Hub {
    /// The session, if `conn` opened it.
    fn session(
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
    fn open(
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
    fn start_run(
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
    fn busy(&self, sid: &SessionId) -> bool {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(sid).is_some_and(|e| !e.running.is_empty())
    }

    /// Aborts the session's runs, if `conn` opened it.
    fn cancel(&self, sid: &SessionId, conn: ConnId) {
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
    fn close(&self, sid: &SessionId, conn: ConnId) {
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
    fn detach(&self, conn: ConnId) {
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

/// One connection's view: the shared hub plus what this client can do.
struct State {
    hub: Arc<Hub>,
    conn: ConnId,
    caps: ClientCaps,
}

impl State {
    fn viewer(&self, cx: &ConnectionTo<Client>, ready: bool) -> Viewer {
        Viewer {
            conn: self.conn,
            cx: cx.clone(),
            forms: self.caps.forms.load(Ordering::Relaxed),
            terminal: self.caps.terminal_output.load(Ordering::Relaxed),
            ready,
        }
    }
}

/// The handlers own the state, so it goes when the connection ends.
impl Drop for State {
    fn drop(&mut self) {
        self.hub.detach(self.conn);
    }
}

/// A run registered with `Hub::start_run`.
struct Run {
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

/// Builds the ACP agent with its own hub; connect it to a transport, e.g.
/// `Stdio::new()`.
pub fn agent(agent: Arc<Agent>) -> impl ConnectTo<Client> {
    agent_in(agent, Arc::default())
}

/// Builds the ACP agent for one connection; connections on the same `hub`
/// share open sessions.
pub fn agent_in(agent: Arc<Agent>, hub: Arc<Hub>) -> impl ConnectTo<Client> {
    let state = Arc::new(State {
        hub,
        conn: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
        caps: ClientCaps::default(),
    });
    let (init_state, new_state, prompt_state, config_state, cancel_state) = (
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
    );
    let (load_state, resume_state, list_state) = (state.clone(), state.clone(), state.clone());
    let close_state = state;
    let (prompt_agent, config_agent, init_agent) = (agent.clone(), agent.clone(), agent.clone());
    let (load_agent, resume_agent, list_agent) = (agent.clone(), agent.clone(), agent.clone());

    agent_client_protocol::Agent
        .builder()
        .name("rho")
        .on_receive_request(
            async move |req: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                let caps = &req.client_capabilities;
                init_state.caps.forms.store(
                    caps.elicitation.as_ref().is_some_and(|e| e.supports_form()),
                    Ordering::Relaxed,
                );
                init_state.caps.terminal_output.store(
                    caps.meta
                        .as_ref()
                        .and_then(|m| m.get("terminal_output"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    Ordering::Relaxed,
                );
                let saved = init_agent.sessions_db.is_some();
                let mut sessions = SessionCapabilities::new()
                    .additional_directories(SessionAdditionalDirectoriesCapabilities::new())
                    .close(SessionCloseCapabilities::new());
                if saved {
                    sessions = sessions
                        .list(SessionListCapabilities::new())
                        .resume(SessionResumeCapabilities::new());
                }
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(saved)
                                .prompt_capabilities(
                                    PromptCapabilities::new().image(true).embedded_context(true),
                                )
                                .session_capabilities(sessions),
                        )
                        .agent_info(Implementation::new("rho", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                // Unique across processes: sessions are stored by id.
                let id = store::SessionId::generate();
                let sid = SessionId::new(id.as_str());
                let (agent, state) = (agent.clone(), new_state.clone());
                // Extension session_start hooks are async; keep them off the dispatch loop.
                cx.clone().spawn(async move {
                    let frontend = AcpFrontend::new(&sid, &req.cwd);
                    // Ready at once: session_start hooks may ask before the response.
                    frontend.attach(state.viewer(&cx, true));
                    let started = agent
                        .start_session(id, req.cwd, req.additional_directories, frontend.clone())
                        .await;
                    let session = match started {
                        Ok(s) => s,
                        Err(e) => return responder.respond_with_internal_error(format!("{e:#}")),
                    };
                    let (options, commands) = (
                        frontend.remember(config_options(&session)),
                        commands(&session),
                    );
                    let (ctx, dirs) = (session.ctx.clone(), session.dirs_notice());
                    register(
                        &agent,
                        &state.hub,
                        &sid,
                        session,
                        frontend,
                        commands.clone(),
                    );
                    responder
                        .respond(NewSessionResponse::new(sid.clone()).config_options(options))?;
                    // After the response: before it the client does not know the session.
                    if let Some(dirs) = dirs {
                        ctx.notify("Additional directories", &dirs, false);
                    }
                    send_commands(&cx, &sid, commands)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let (agent, state) = (load_agent.clone(), load_state.clone());
                cx.clone().spawn(async move {
                    let sid = req.session_id;
                    let opened = open_stored(
                        &agent,
                        &state,
                        &cx,
                        &sid,
                        req.cwd,
                        req.additional_directories,
                        true,
                    );
                    match opened.await {
                        Ok((frontend, options, commands)) => {
                            responder
                                .respond(LoadSessionResponse::new().config_options(options))?;
                            frontend.ready(state.conn);
                            send_commands(&cx, &sid, commands)
                        }
                        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ResumeSessionRequest,
                        responder: Responder<ResumeSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let (agent, state) = (resume_agent.clone(), resume_state.clone());
                cx.clone().spawn(async move {
                    let sid = req.session_id;
                    let opened = open_stored(
                        &agent,
                        &state,
                        &cx,
                        &sid,
                        req.cwd,
                        req.additional_directories,
                        false,
                    );
                    match opened.await {
                        Ok((frontend, options, commands)) => {
                            responder
                                .respond(ResumeSessionResponse::new().config_options(options))?;
                            frontend.ready(state.conn);
                            send_commands(&cx, &sid, commands)
                        }
                        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        cx: ConnectionTo<Client>| {
                let (agent, hub) = (list_agent.clone(), list_state.hub.clone());
                cx.spawn(async move {
                    match list_sessions(&agent, &hub, req).await {
                        Ok(list) => responder.respond(list),
                        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        _cx| {
                close_state.hub.close(&req.session_id, close_state.conn);
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder: Responder<PromptResponse>, _cx| {
                let conn = prompt_state.conn;
                let Some((session, frontend, run, registration)) =
                    prompt_state.hub.start_run(&req.session_id, Some(conn))
                else {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                };
                // The other connections on the session see the prompt too.
                for block in &req.prompt {
                    frontend.send_update(
                        SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone())),
                        To::Except(conn),
                    );
                }
                let agent = prompt_agent.clone();
                // Not on the connection: the turn goes on if it drops, and
                // cancel notifications get through.
                tokio::spawn(async move {
                    let mut session = session.lock().await;
                    let usage_before = session.usage;
                    let turn = async {
                        match slash_command(&req.prompt) {
                            Some(("compact", focus)) => agent
                                .compact(&mut session, &focus)
                                .await
                                .map(|()| provider::StopReason::EndTurn),
                            Some(("redact", phrase)) => agent
                                .redact(&mut session, &phrase)
                                .await
                                .map(|()| provider::StopReason::EndTurn),
                            Some(("usage", _)) => {
                                session.ctx.emit(AgentEvent::Text(&session.usage_report()));
                                Ok(provider::StopReason::EndTurn)
                            }
                            Some((name, args))
                                if session.commands.iter().any(|c| c.name.as_str() == name) =>
                            {
                                agent
                                    .command(&session, &CommandName::new(name), &args)
                                    .await;
                                Ok(provider::StopReason::EndTurn)
                            }
                            _ => match to_content(req.prompt.clone()) {
                                Ok(content) => agent.run(&mut session, content).await,
                                Err(e) => Err(e),
                            },
                        }
                    };
                    let result = Abortable::new(turn, registration).await;
                    drop(run);
                    let usage = turn_usage(session.usage - usage_before);
                    // The connection may be gone; the session is saved anyway.
                    let _ = match result {
                        Ok(Ok(stop)) => {
                            responder.respond(PromptResponse::new(to_stop(stop)).usage(usage))
                        }
                        Ok(Err(e)) => responder.respond_with_internal_error(format!("{e:#}")),
                        Err(_aborted) => {
                            session.ctx.notify(INTERRUPTED, "", false);
                            responder
                                .respond(PromptResponse::new(StopReason::Cancelled).usage(usage))
                        }
                    };
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                let Some((session, ctx, frontend)) =
                    config_state.hub.session(&req.session_id, config_state.conn)
                else {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                };
                // A running turn picks it up with its next request.
                if *req.config_id.0 == *PROFILE_OPTION {
                    let value = req.value.as_value_id().map(|v| v.0.to_string());
                    return match ctx.set_profile(&ProfileId::new(value.unwrap_or_default())) {
                        Ok(()) => responder
                            .respond(SetSessionConfigOptionResponse::new(frontend.options())),
                        Err(e) => responder
                            .respond_with_error(Error::invalid_params().data(format!("{e:#}"))),
                    };
                }
                let agent = config_agent.clone();
                let conn = config_state.conn;
                // Waits for a running turn: the change applies from the next one.
                cx.spawn(async move {
                    let mut session = session.lock().await;
                    let result = set_config(&agent, &mut session, &req);
                    session.save().await;
                    match result {
                        Ok(()) => {
                            let options = frontend.remember(config_options(&session));
                            // The other viewers' selectors would stay stale.
                            frontend.send_update(
                                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                                    options.clone(),
                                )),
                                To::Except(conn),
                            );
                            responder.respond(SetSessionConfigOptionResponse::new(options))
                        }
                        Err(e) => responder
                            .respond_with_error(Error::invalid_params().data(format!("{e:#}"))),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                cancel_state.hub.cancel(&n.session_id, cancel_state.conn);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// Built-in slash commands plus the extensions'.
fn commands(session: &Session) -> Vec<AvailableCommand> {
    let mut commands = vec![
        AvailableCommand::new("compact", "Summarize the conversation to free up context").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                "optional focus for the summary",
            )),
        ),
        AvailableCommand::new("usage", "Show token, cache and cost usage of this session"),
        AvailableCommand::new(
            "redact",
            "Mask a phrase in all saved sessions, the history index and memories",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("exact phrase"),
        )),
    ];
    for c in &session.commands {
        let mut command = AvailableCommand::new(c.name.to_string(), &c.description);
        if let Some(hint) = &c.hint {
            command = command.input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new(hint),
            ));
        }
        commands.push(command);
    }
    commands
}

fn send_commands(
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    commands: Vec<AvailableCommand>,
) -> Result<(), Error> {
    cx.send_notification(SessionNotification::new(
        sid.clone(),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(commands)),
    ))
}

/// Makes an opened session reachable for prompts and triggered messages.
fn register(
    agent: &Arc<Agent>,
    hub: &Arc<Hub>,
    sid: &SessionId,
    session: Session,
    frontend: Arc<AcpFrontend>,
    commands: Vec<AvailableCommand>,
) -> SharedSession {
    let ctx = session.ctx.clone();
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let (watch, registration) = AbortHandle::new_pair();
    let inbox = watch_inbox(agent.clone(), hub.clone(), sid.clone(), ctx.clone());
    tokio::spawn(Abortable::new(inbox, registration));
    // shortcut: two connections resuming the same closed session at once get
    // one session each and the later one wins the map; fine for one editor per
    // session, needs a pending-open marker if that changes.
    hub.sessions.lock().unwrap().insert(
        sid.clone(),
        Entry {
            session: session.clone(),
            running: vec![],
            ctx,
            frontend,
            commands,
            watch,
        },
    );
    session
}

/// `session/load` (with `replay`) and `session/resume`: the open session
/// (also one opened by another connection), or the stored one continued.
/// Returns its frontend, config options and commands. Call
/// `AcpFrontend::ready` after the response.
async fn open_stored(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: bool,
) -> anyhow::Result<(
    Arc<AcpFrontend>,
    Vec<SessionConfigOption>,
    Vec<AvailableCommand>,
)> {
    let conn = state.conn;
    // An open session takes the new directories after the replay, so the
    // notice follows the history.
    let (session, ctx, frontend, commands, dirs) = match state.hub.open(sid) {
        Some((session, ctx, frontend, commands)) => {
            if ctx.cwd() != cwd {
                anyhow::bail!("session {} belongs to {}", sid.0, ctx.cwd().display());
            }
            frontend.attach(state.viewer(cx, false));
            (session, ctx, frontend, commands, Some(additional_dirs))
        }
        None => {
            let frontend = AcpFrontend::new(sid, &cwd);
            // Nothing runs yet, and session_start hooks may ask.
            frontend.attach(state.viewer(cx, true));
            let session = agent
                .resume_session(
                    &store::SessionId::new(&*sid.0),
                    &cwd,
                    additional_dirs,
                    frontend.clone(),
                )
                .await?;
            let (ctx, commands) = (session.ctx.clone(), commands(&session));
            let session = register(
                agent,
                &state.hub,
                sid,
                session,
                frontend.clone(),
                commands.clone(),
            );
            (session, ctx, frontend, commands, None)
        }
    };
    let (stored, diffs) = match ctx.store() {
        Some(store) if replay => {
            let id = store::SessionId::new(&*sid.0);
            (store.messages(&id).await?, store.diffs(&id).await?)
        }
        _ => Default::default(),
    };
    let Ok(mut session) = session.try_lock() else {
        // A turn holds the session: replay what is saved, the live updates
        // follow. shortcut: no tool descriptions in this replay and the new
        // directories are not applied; lock-free copies would fix both.
        if replay {
            let messages: Vec<_> = stored.into_iter().map(|m| (m.epoch, m.message)).collect();
            replay_history(&frontend, conn, &ctx, None, &messages, &diffs);
        }
        let options = frontend.options();
        return Ok((frontend, options, commands));
    };
    if replay {
        // Compaction keeps older epochs in the store; show them too.
        let epoch = session.epoch();
        let earlier = stored.into_iter().filter(|m| m.epoch < epoch);
        let mut messages: Vec<_> = earlier.map(|m| (m.epoch, m.message)).collect();
        messages.extend(session.history.iter().map(|m| (epoch, m.clone())));
        replay_history(&frontend, conn, &ctx, Some(&session), &messages, &diffs);
    }
    if let Some(dirs) = dirs
        && agent.set_additional_dirs(&mut session, dirs)
    {
        session.save().await;
    }
    // A new connection has not seen it; the frontend also needs it to
    // republish on a model switch.
    session.emit_usage();
    let options = frontend.remember(config_options(&session));
    Ok((frontend, options, commands))
}

/// Sends `(epoch, message)`s as session updates to `conn`, as if they were
/// happening now. A compaction summary (the first text of an epoch after the
/// first) shows as a notice, like when it was made. Edits show their diff
/// while it is in `diffs` (see `Store::save_diffs`). Tool descriptions need
/// the `session`.
fn replay_history(
    frontend: &AcpFrontend,
    conn: ConnId,
    ctx: &Context,
    session: Option<&Session>,
    messages: &[(i64, provider::Message)],
    diffs: &HashMap<ToolCallId, crate::extension::Diff>,
) {
    let emit = |ev: AgentEvent<'_>| frontend.send(ev, To::Only(conn));
    let mut epoch = 0;
    for (msg_epoch, msg) in messages {
        let mut user: Vec<ContentBlock> = vec![];
        for block in &msg.content {
            let first = std::mem::replace(&mut epoch, *msg_epoch) != *msg_epoch;
            if first
                && let Content::Text { text } = block
                && let Some(summary) = crate::compact::summary_of(text)
            {
                emit(AgentEvent::Notice {
                    title: "Compacted",
                    text: summary,
                    is_error: false,
                });
                continue;
            }
            match block {
                Content::Text { text } if msg.role == Role::User => match user.last_mut() {
                    Some(ContentBlock::Text(last)) => {
                        last.text.push_str("\n\n");
                        last.text.push_str(text);
                    }
                    _ => user.push(text.as_str().into()),
                },
                Content::Image { source } => user.push(image_block(source)),
                Content::Text { text } => emit(AgentEvent::Text(text)),
                Content::Thinking { thinking, .. } => emit(AgentEvent::Thinking(thinking)),
                Content::ToolUse { id, name, input } => {
                    let description = session
                        .and_then(|s| s.tool(name))
                        .and_then(|t| t.describe(input));
                    emit(AgentEvent::ToolStart {
                        id,
                        name,
                        input,
                        description: description.as_deref(),
                    });
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    images,
                } => emit(AgentEvent::ToolEnd {
                    id: tool_use_id,
                    output: content,
                    is_error: *is_error,
                    diff: diffs.get(tool_use_id),
                    images,
                }),
                Content::RedactedThinking { .. } => {}
            }
        }
        for block in user {
            frontend.send_update(
                SessionUpdate::UserMessageChunk(ContentChunk::new(block)),
                To::Only(conn),
            );
        }
    }
    if let Some(title) = ctx.title() {
        emit(AgentEvent::Title(&title));
    }
}

const LIST_PAGE: usize = 100;

const INTERRUPTED: &str = "Interrupted by the user";

/// `session/list`. Without a cwd filter, the database for rho's own cwd is
/// listed.
async fn list_sessions(
    agent: &Agent,
    hub: &Hub,
    req: ListSessionsRequest,
) -> anyhow::Result<ListSessionsResponse> {
    let db = agent
        .sessions_db
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("sessions are not saved"))?;
    let path = match &req.cwd {
        Some(cwd) => db(cwd),
        None => db(&std::env::current_dir()?),
    };
    let store = tokio::task::spawn_blocking(move || Store::open(path)).await??;
    let page = store
        .list(req.cwd.as_deref(), req.cursor.as_deref(), LIST_PAGE)
        .await?;
    let sessions = page
        .sessions
        .into_iter()
        .map(|s| {
            let sid = SessionId::new(s.id.to_string());
            let info = AcpSessionInfo::new(sid.clone(), s.cwd)
                .additional_directories(s.additional_dirs)
                .title(s.title)
                .updated_at(rfc3339(s.updated_at));
            // Like `AcpFrontend::set_busy`.
            if hub.busy(&sid) {
                info.meta(meta(serde_json::json!({ "rho": { "running": true } })))
            } else {
                info
            }
        })
        .collect();
    Ok(ListSessionsResponse::new(sessions).next_cursor(page.next))
}

/// Unix milliseconds as an RFC 3339 UTC timestamp.
fn rfc3339(ms: i64) -> String {
    let (secs, ms) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, t) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

/// Runs turns for extension messages sent with `trigger_turn` while no
/// prompt is active. ACP has no agent-initiated prompts, so these runs only
/// stream `session/update`s (like pi-acp does). Aborted when the session is
/// dropped.
async fn watch_inbox(agent: Arc<Agent>, hub: Arc<Hub>, sid: SessionId, ctx: Context) {
    loop {
        ctx.wait_inbox().await;
        if !ctx.has_triggered() {
            continue;
        }
        let Some((session, _, _run, registration)) = hub.start_run(&sid, None) else {
            return;
        };
        let mut session = session.lock().await;
        // A prompt may have consumed the message while we waited for the lock.
        let turn = agent.run_triggered(&mut session);
        match Abortable::new(turn, registration).await {
            Ok(Some(Err(e))) => ctx.notify("rho", &format!("{e:#}"), true),
            Err(_aborted) => ctx.notify(INTERRUPTED, "", false),
            _ => {}
        }
    }
}

/// A connection that opened the session.
#[derive(Clone)]
struct Viewer {
    conn: ConnId,
    cx: ConnectionTo<Client>,
    /// The client can show elicitation forms, see `Frontend::ask`.
    forms: bool,
    /// The client shows `Bash` calls as terminals.
    terminal: bool,
    /// The client knows the session: it gets confirmations and forms.
    ready: bool,
}

/// Which viewers an update goes to.
#[derive(Clone, Copy)]
enum To {
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
struct AcpFrontend {
    sid: SessionId,
    /// For `Bash` calls shown as terminals.
    cwd: PathBuf,
    notices: AtomicU64,
    viewers: Mutex<Vec<Viewer>>,
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
    published: Mutex<Published>,
    /// A run is registered (see `Hub::start_run`).
    busy: AtomicBool,
}

impl AcpFrontend {
    fn new(sid: &SessionId, cwd: &Path) -> Arc<Self> {
        Arc::new(Self {
            sid: sid.clone(),
            cwd: cwd.to_path_buf(),
            notices: AtomicU64::new(0),
            viewers: Mutex::default(),
            ready: broadcast::channel(16).0,
            running: Mutex::default(),
            shown: Mutex::default(),
            published: Mutex::default(),
            busy: AtomicBool::new(false),
        })
    }

    /// Tells all viewers whether a turn runs, as `_meta.rho.running` of a
    /// `session_info_update`: ACP has no update for it.
    fn set_busy(&self, on: bool) {
        self.busy.store(on, Ordering::Relaxed);
        self.send_update(busy_update(on), To::All);
    }

    /// `viewer` gets the updates from now on.
    fn attach(&self, viewer: Viewer) {
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|v| v.conn != viewer.conn);
        viewers.push(viewer);
    }

    fn viewing(&self, conn: ConnId) -> bool {
        self.viewers.lock().unwrap().iter().any(|v| v.conn == conn)
    }

    /// `conn` knows the session now: it gets the running tool calls and
    /// the questions waiting for an answer.
    fn ready(&self, conn: ConnId) {
        let mut viewers = self.viewers.lock().unwrap();
        let Some(v) = viewers.iter_mut().find(|v| v.conn == conn) else {
            return;
        };
        v.ready = true;
        for (_, terminal, plain) in self.running.lock().unwrap().iter() {
            let update = if v.terminal { terminal } else { plain };
            let _ =
                v.cx.send_notification(SessionNotification::new(self.sid.clone(), update.clone()));
        }
        if self.busy.load(Ordering::Relaxed) {
            let _ = v.cx.send_notification(SessionNotification::new(
                self.sid.clone(),
                busy_update(true),
            ));
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

    fn remember(&self, options: Vec<SessionConfigOption>) -> Vec<SessionConfigOption> {
        options.clone_into(&mut self.published.lock().unwrap().options);
        options
    }

    fn options(&self) -> Vec<SessionConfigOption> {
        self.published.lock().unwrap().options.clone()
    }

    fn send(&self, ev: AgentEvent<'_>, to: To) {
        // Before session/new answered there are no options to update.
        if matches!(ev, AgentEvent::Profile(_)) && self.options().is_empty() {
            return;
        }
        let n = self.notices.fetch_add(1, Ordering::Relaxed);
        let shown = match &ev {
            AgentEvent::ToolStart {
                id,
                name,
                input,
                description,
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
            AgentEvent::ToolEnd { id, .. } => self.shown.lock().unwrap().remove(*id),
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
        for v in self.viewers.lock().unwrap().iter() {
            if to.includes(v.conn) {
                let update = if v.terminal { &terminal } else { &plain };
                // Dropped updates only affect display; the turn continues.
                let _ = v
                    .cx
                    .send_notification(SessionNotification::new(self.sid.clone(), update.clone()));
            }
        }
    }

    fn send_update(&self, update: SessionUpdate, to: To) {
        for v in self.viewers.lock().unwrap().iter() {
            if to.includes(v.conn) {
                let _ = v
                    .cx
                    .send_notification(SessionNotification::new(self.sid.clone(), update.clone()));
            }
        }
    }
}

/// What the client was last sent, to send again with a change.
#[derive(Default)]
struct Published {
    usage: LastUsage,
    /// `config_option_update` carries the full set, so a profile switch
    /// sends these again.
    options: Vec<SessionConfigOption>,
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
            let call = ToolCallUpdate::new(
                tool_call_id
                    .map_or("confirm", ToolCallId::as_str)
                    .to_string(),
                fields,
            );
            let request = RequestPermissionRequest::new(
                self.sid.clone(),
                call,
                vec![
                    PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                    PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
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

/// The form as an elicitation schema: one property per field.
fn form_schema(form: &Form) -> ElicitationSchema {
    let mut schema = ElicitationSchema::new();
    for Field {
        key,
        title,
        description,
        kind,
        required,
    } in &form.fields
    {
        let (title, description) = (Some(title.clone()), description.clone());
        schema = match kind {
            FieldKind::Text => schema.property(
                key.to_string(),
                StringPropertySchema::new()
                    .title(title)
                    .description(description),
                *required,
            ),
            FieldKind::Choice { options, multiple } => {
                let options = options
                    .iter()
                    .map(|o| {
                        EnumOption::new(o.value.to_string(), &o.title)
                            .description(o.description.clone())
                    })
                    .collect();
                if *multiple {
                    schema.property(
                        key.to_string(),
                        MultiSelectPropertySchema::titled(options)
                            .title(title)
                            .description(description),
                        *required,
                    )
                } else {
                    schema.property(
                        key.to_string(),
                        StringPropertySchema::new()
                            .title(title)
                            .description(description)
                            .one_of(options),
                        *required,
                    )
                }
            }
        };
    }
    schema
}

/// The submitted values by field key; empty ones are dropped.
fn form_answers(content: Option<BTreeMap<String, ElicitationContentValue>>) -> Answers {
    let mut answers = Answers::new();
    for (key, value) in content.unwrap_or_default() {
        let values = match value {
            ElicitationContentValue::String(s) => vec![s],
            ElicitationContentValue::StringArray(v) => v,
            ElicitationContentValue::Integer(i) => vec![i.to_string()],
            ElicitationContentValue::Number(n) => vec![n.to_string()],
            ElicitationContentValue::Boolean(b) => vec![b.to_string()],
            _ => vec![],
        };
        let values: Vec<String> = values
            .into_iter()
            .filter(|v| !v.trim().is_empty())
            .collect();
        if !values.is_empty() {
            answers.insert(FieldKey::new(key), values);
        }
    }
    answers
}

const THINKING_OPTION: &str = "thought_level";
const PROFILE_OPTION: &str = "mode";

/// The session's profile (if there is a choice), model and thinking level
/// as ACP config options.
fn config_options(session: &Session) -> Vec<SessionConfigOption> {
    let mut options = vec![];
    let profiles = session.ctx.profiles();
    if profiles.len() > 1 {
        let choices: Vec<_> = profiles
            .iter()
            .map(|p| {
                SessionConfigSelectOption::new(p.id.to_string(), p.name.clone())
                    .description(p.description.clone())
            })
            .collect();
        options.push(
            SessionConfigOption::select(
                PROFILE_OPTION,
                "Profile",
                session.ctx.profile().id.to_string(),
                choices,
            )
            .category(SessionConfigOptionCategory::Mode)
            .description("What the agent may do in this session"),
        );
    }
    options.extend(model_options(session));
    options
}

fn model_options(session: &Session) -> Vec<SessionConfigOption> {
    let mut models: Vec<_> = session
        .ctx
        .registry()
        .models()
        .into_iter()
        .map(|(spec, m)| {
            let mut description = format!("{spec}, {} context", tokens(m.context_window));
            if let Some(p) = m.price {
                description += &format!(", ${}/${} per MTok", p.input, p.output);
            }
            // The same model can come from several providers.
            let name = format!("{} ({})", m.name, spec.provider().as_str());
            SessionConfigSelectOption::new(spec.to_string(), name).description(description)
        })
        .collect();
    let mut options = vec![];
    // Without any known model there is nothing to pick from.
    if !models.is_empty() {
        // A model from `-m` that the provider does not list.
        let current = session.model.to_string();
        if models.iter().all(|m| *m.value.0 != *current) {
            models.insert(0, SessionConfigSelectOption::new(current.clone(), &current));
        }
        options.push(
            // Per `models.json` rule, so clients remember a pick per rule.
            SessionConfigOption::select(session.model_key.clone(), "Model", current, models)
                .category(SessionConfigOptionCategory::Model)
                .description("Model for this session"),
        );
    }
    let levels: Vec<_> = Thinking::ALL
        .iter()
        .map(|t| SessionConfigSelectOption::new(t.as_str(), t.as_str()))
        .collect();
    options.push(
        SessionConfigOption::select(
            THINKING_OPTION,
            "Thinking",
            session.thinking.as_str(),
            levels,
        )
        .category(SessionConfigOptionCategory::ThoughtLevel)
        .description("Reasoning effort for this session"),
    );
    options
}

/// 1_000_000 -> "1M", 200_000 -> "200k".
fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        n if n >= 1_000 => format!("{}k", n / 1_000),
        n => n.to_string(),
    }
}

fn set_config(
    agent: &Agent,
    session: &mut Session,
    req: &SetSessionConfigOptionRequest,
) -> anyhow::Result<()> {
    let Some(value) = req.value.as_value_id().map(|v| &*v.0) else {
        anyhow::bail!("expected a string value");
    };
    match &*req.config_id.0 {
        id if id == session.model_key => {
            let spec = value.parse()?;
            let listed = session
                .ctx
                .registry()
                .models()
                .iter()
                .any(|(m, _)| *m == spec);
            if !listed && spec != session.model {
                anyhow::bail!("unknown model {value:?}");
            }
            agent.set_model(session, &spec)
        }
        THINKING_OPTION => {
            session.thinking = value.parse()?;
            session.publish_info();
            Ok(())
        }
        other => anyhow::bail!("unknown config option {other:?}"),
    }
}

/// Token usage of one prompt for `PromptResponse`; `input_tokens` is the
/// uncached input, the cache counts are separate.
fn turn_usage(u: Usage) -> AcpUsage {
    AcpUsage::new(
        u.input + u.output + u.cache_read + u.cache_write,
        u.input,
        u.output,
    )
    .cached_read_tokens(u.cache_read)
    .cached_write_tokens(u.cache_write)
}

/// The latest `AgentEvent::Usage`, so a model switch can republish it with
/// the new context window.
#[derive(Default)]
struct LastUsage {
    model: String,
    /// Context in use: the last prompt plus its output.
    used: u64,
    request: Usage,
    total: Usage,
    cost: f64,
}

fn meta(value: serde_json::Value) -> Meta {
    match value {
        serde_json::Value::Object(m) => m,
        _ => Meta::new(),
    }
}

fn busy_update(on: bool) -> SessionUpdate {
    SessionUpdate::SessionInfoUpdate(
        SessionInfoUpdate::new().meta(meta(serde_json::json!({ "rho": { "running": on } }))),
    )
}

/// `usage_update`: context used/size and the cost are standard; the token
/// and cache breakdown is in `_meta.rho`.
fn usage_update(last: &LastUsage, size: u64) -> SessionUpdate {
    let counts = |u: &Usage| {
        serde_json::json!({
            "input": u.input,
            "output": u.output,
            "cacheRead": u.cache_read,
            "cacheWrite": u.cache_write,
        })
    };
    let hit_rate = (last.request.prompt() > 0)
        .then(|| last.request.cache_read as f64 / last.request.prompt() as f64);
    let mut update = UsageUpdate::new(last.used, size).meta(meta(serde_json::json!({
        "rho": {
            "model": last.model,
            "request": counts(&last.request),
            "session": counts(&last.total),
            "cacheHitRate": hit_rate,
            "cacheTtlSecs": crate::compact::CACHE_TTL.as_secs(),
        }
    })));
    if last.cost > 0.0 {
        update = update.cost(Cost::new(last.cost, "USD"));
    }
    SessionUpdate::UsageUpdate(update)
}

/// `/name [args]` -> `Some((name, args))`.
fn slash_command(prompt: &[ContentBlock]) -> Option<(&str, String)> {
    let [ContentBlock::Text(t)] = prompt else {
        return None;
    };
    let rest = t.text.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name, args.trim().to_string()))
}

/// The prompt for the model. Fails on an image the model would reject, so
/// it never gets into the history.
fn to_content(blocks: Vec<ContentBlock>) -> anyhow::Result<Vec<Content>> {
    let image = |media_type: String, data: String| -> anyhow::Result<Content> {
        let source = ImageSource::Base64 { media_type, data };
        source.check()?;
        Ok(Content::Image { source })
    };
    let mut content = vec![];
    for b in blocks {
        content.push(match b {
            ContentBlock::Text(t) => Content::text(t.text),
            ContentBlock::Image(i) => image(i.mime_type, i.data)?,
            ContentBlock::ResourceLink(l) => Content::text(format!("@{}", l.uri)),
            ContentBlock::Resource(r) => match r.resource {
                EmbeddedResourceResource::TextResourceContents(t) => {
                    Content::text(format!("<file uri=\"{}\">\n{}\n</file>", t.uri, t.text))
                }
                EmbeddedResourceResource::BlobResourceContents(b)
                    if b.mime_type
                        .as_deref()
                        .is_some_and(|m| m.starts_with("image/")) =>
                {
                    image(b.mime_type.unwrap_or_default(), b.blob)?
                }
                _ => continue,
            },
            // We advertise no audio capability.
            _ => continue,
        });
    }
    Ok(content)
}

fn image_block(source: &ImageSource) -> ContentBlock {
    let ImageSource::Base64 { media_type, data } = source;
    ContentBlock::Image(ImageContent::new(data.clone(), media_type.clone()))
}

/// How a tool call is shown besides its title.
#[derive(Clone, Debug, PartialEq)]
enum Shown {
    /// Markdown content: the command as a fenced block, or `Tool::describe`.
    Text(String),
    /// A terminal card, with output and exit code sent on `ToolEnd` (Zed's
    /// `_meta` terminal protocol, as pi-acp does).
    Terminal { cwd: PathBuf, command: String },
}

impl Shown {
    fn of(
        name: &ToolName,
        input: &serde_json::Value,
        description: Option<&str>,
        terminal_cwd: Option<&Path>,
    ) -> Option<Self> {
        if let Some(text) = description {
            return Some(Self::Text(text.into()));
        }
        let command = input["command"].as_str()?;
        Some(match terminal_cwd {
            Some(cwd) if *name == tools::BASH => Self::Terminal {
                cwd: input["cwd"].as_str().map_or(cwd.into(), |d| cwd.join(d)),
                command: command.into(),
            },
            _ => Self::Text(command_block(command)),
        })
    }
}

impl Shown {
    /// A terminal as a `sh` block for a client without terminals.
    fn for_viewer(&self, terminal: bool) -> Self {
        match self {
            Self::Terminal { command, .. } if !terminal => Self::Text(command_block(command)),
            other => other.clone(),
        }
    }
}

/// The command as a fenced `sh` block, so Zed highlights it. The fence is
/// longer than any backtick run in the command.
fn command_block(command: &str) -> String {
    let (mut run, mut longest) = (0, 0);
    for c in command.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}sh\n{command}\n{fence}")
}

/// Exit code from the `[exit status: N]` line that `Bash` appends to the
/// output of a failed command.
fn exit_code(output: &str, is_error: bool) -> i64 {
    output
        .trim_end()
        .rsplit_once("[exit status: ")
        .and_then(|(_, rest)| rest.strip_suffix(']')?.parse().ok())
        .unwrap_or(i64::from(is_error))
}

fn status(is_error: bool) -> ToolCallStatus {
    if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    }
}

/// The client's icon for a tool. Extension tools by name: rho-agent's,
/// MCP tools are `Other`.
fn tool_kind(name: &ToolName) -> ToolKind {
    match name.as_str() {
        "Read" | "ReadImage" | "ReadSkill" => ToolKind::Read,
        "Write" | "Edit" => ToolKind::Edit,
        "Bash" | "BackgroundTask" => ToolKind::Execute,
        "Glob" | "Grep" | "GoDoc" | "History" => ToolKind::Search,
        "FetchUrl" => ToolKind::Fetch,
        "EnterPlanMode" | "PresentPlan" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

fn to_update(
    ev: AgentEvent,
    n: u64,
    shown: Option<&Shown>,
    cwd: &Path,
    published: &mut Published,
) -> SessionUpdate {
    let last_usage = &mut published.usage;
    match ev {
        AgentEvent::Profile(id) => {
            for o in &mut published.options {
                if *o.id.0 == *PROFILE_OPTION
                    && let SessionConfigKind::Select(s) = &mut o.kind
                {
                    s.current_value = id.to_string().into();
                }
            }
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(published.options.clone()))
        }
        // Rendered as a finished tool call, like pi-acp does for extension messages.
        AgentEvent::Notice {
            title,
            text,
            is_error,
        } => SessionUpdate::ToolCall(
            ToolCall::new(format!("notice-{n}"), title)
                .kind(ToolKind::Other)
                .status(status(is_error))
                .content(if text.is_empty() {
                    vec![]
                } else {
                    vec![text.into()]
                }),
        ),
        AgentEvent::Title(t) => {
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(t.to_string()))
        }
        AgentEvent::Text(t) => SessionUpdate::AgentMessageChunk(ContentChunk::new(t.into())),
        AgentEvent::Thinking(t) => SessionUpdate::AgentThoughtChunk(ContentChunk::new(t.into())),
        AgentEvent::Usage {
            model,
            usage,
            context_window,
            total,
            cost,
        } => {
            *last_usage = LastUsage {
                model: model.to_string(),
                used: usage.prompt() + usage.output,
                request: usage,
                total,
                cost,
            };
            usage_update(last_usage, context_window)
        }
        AgentEvent::Model {
            spec,
            context_window,
        } => {
            last_usage.model = spec.to_string();
            usage_update(last_usage, context_window)
        }
        AgentEvent::ToolStart {
            id, name, input, ..
        } => {
            let kind = tool_kind(name);
            let call = match shown {
                Some(Shown::Terminal { cwd, command }) => ToolCall::new(
                    id.to_string(),
                    call_description(name, input).map_or(command.clone(), |_| title(name, input)),
                )
                .content(vec![ToolCallContent::Terminal(Terminal::new(
                    id.to_string(),
                ))])
                .meta(meta(serde_json::json!({
                    "terminal_info": {"terminal_id": id, "cwd": cwd},
                }))),
                Some(Shown::Text(text)) => ToolCall::new(id.to_string(), title(name, input))
                    .content(vec![text.as_str().into()]),
                None => ToolCall::new(id.to_string(), title(name, input)),
            };
            // Lets the client follow along to the file.
            let file = [tools::READ, tools::READ_IMAGE, tools::WRITE, EDIT].contains(name);
            let locations = match input.get("path").and_then(|p| p.as_str()) {
                Some(path) if file => vec![ToolCallLocation::new(tools::resolve(cwd, path))],
                _ => vec![],
            };
            SessionUpdate::ToolCall(
                call.kind(kind)
                    .status(ToolCallStatus::InProgress)
                    .locations(locations)
                    .raw_input(input.clone()),
            )
        }
        AgentEvent::ToolEnd {
            id,
            output,
            is_error,
            diff,
            images,
        } => {
            let fields = ToolCallUpdateFields::new().status(status(is_error));
            if let Some(Shown::Terminal { .. }) = shown {
                return SessionUpdate::ToolCallUpdate(
                    ToolCallUpdate::new(id.to_string(), fields).meta(meta(serde_json::json!({
                        "terminal_output": {"terminal_id": id, "data": output},
                        "terminal_exit": {
                            "terminal_id": id,
                            "exit_code": exit_code(output, is_error),
                            "signal": null,
                        },
                    }))),
                );
            }
            let mut content = vec![];
            if let Some(Shown::Text(text)) = shown {
                content.push(text.as_str().into());
            }
            if let Some(d) = diff {
                content.push(Diff::new(&d.path, &d.new).old_text(d.old.clone()).into());
            }
            content.push(output.into());
            content.extend(images.iter().map(|i| image_block(i).into()));
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_string(),
                fields.content(content),
            ))
        }
    }
}

const MAX_TITLE: usize = 160;

/// Input keys shown in a tool call title, the first present one wins.
const TITLE_KEYS: [&str; 11] = [
    "command", "url", "pattern", "glob", "target", "query", "path", "hash", "id", "feature",
    "action",
];

/// Tools with a `description` input that says what the call is for. Not
/// every tool: in others (e.g. MCP tools) it can be content.
const DESCRIBED: [&str; 12] = [
    "AskUserQuestion",
    "Bash",
    "BackgroundTask",
    "Edit",
    "FetchUrl",
    "Glob",
    "Grep",
    "History",
    "Memory",
    "Read",
    "ReadImage",
    "Write",
];

/// The `description` the model gave a call, see `DESCRIBED`.
fn call_description<'a>(name: &ToolName, input: &'a serde_json::Value) -> Option<&'a str> {
    let text = input["description"].as_str()?.trim();
    (DESCRIBED.contains(&name.as_str()) && !text.is_empty()).then_some(text)
}

/// The call's description or "Bash first line of the command", at most
/// `MAX_TITLE` characters.
fn title(name: &ToolName, input: &serde_json::Value) -> String {
    let arg = TITLE_KEYS.iter().find_map(|k| input[k].as_str());
    let title = match (call_description(name, input), arg) {
        (Some(text), _) => text.lines().next().unwrap_or_default().to_string(),
        (None, Some(arg)) => format!("{name} {}", arg.lines().next().unwrap_or_default()),
        (None, None) => return name.to_string(),
    };
    match title.char_indices().nth(MAX_TITLE) {
        Some((end, _)) => format!("{}…", &title[..end]),
        None => title,
    }
}

fn to_stop(stop: provider::StopReason) -> StopReason {
    match stop {
        provider::StopReason::MaxTokens => StopReason::MaxTokens,
        provider::StopReason::Refusal => StopReason::Refusal,
        provider::StopReason::Stopped => StopReason::Cancelled,
        provider::StopReason::EndTurn | provider::StopReason::ToolUse => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use crate::extension::ChoiceValue;
    use crate::extension::FieldKey;
    use agent_client_protocol::schema::v1::TextContent;
    use serde_json::json;

    use super::*;
    use crate::extension::{Choice, Diff as FileDiff};
    use crate::provider::ModelSpec;

    fn update(ev: AgentEvent, shown: Option<&Shown>) -> serde_json::Value {
        let mut last = Published::default();
        serde_json::to_value(to_update(ev, 0, shown, Path::new("/w"), &mut last)).unwrap()
    }

    #[test]
    fn tool_end_sends_the_diff() {
        let diff = FileDiff {
            path: "/w/f.txt".into(),
            old: "a\n".into(),
            new: "b\n".into(),
        };
        let wire = update(
            AgentEvent::ToolEnd {
                id: &ToolCallId::new("t1"),
                output: "ok",
                is_error: false,
                diff: Some(&diff),
                images: &[],
            },
            None,
        );
        assert_eq!(
            wire["content"][0],
            json!({"type": "diff", "path": "/w/f.txt", "oldText": "a\n", "newText": "b\n"})
        );
        assert_eq!(wire["content"][1]["content"]["text"], "ok");
    }

    #[test]
    fn tool_end_shows_images_after_the_output() {
        let images = [crate::provider::ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }];
        let wire = update(
            AgentEvent::ToolEnd {
                id: &ToolCallId::new("t1"),
                output: "pic.png (image/png, 1 KB)",
                is_error: false,
                diff: None,
                images: &images,
            },
            None,
        );
        assert_eq!(
            wire["content"][1],
            json!({"type": "content", "content": {"type": "image", "mimeType": "image/png", "data": "AAAA"}})
        );
    }

    #[test]
    fn file_tools_have_a_location() {
        let start = |name: &str, input: &serde_json::Value| {
            update(
                AgentEvent::ToolStart {
                    id: &ToolCallId::new("t1"),
                    name: &ToolName::new(name),
                    input,
                    description: None,
                },
                None,
            )
        };
        let wire = start("Edit", &json!({"path": "src/a.rs"}));
        assert_eq!(wire["kind"], "edit");
        assert_eq!(wire["locations"], json!([{"path": "/w/src/a.rs"}]));
        let wire = start("ReadSkill", &json!({"name": "s", "path": "x.md"}));
        assert_eq!(wire["kind"], "read");
        assert_eq!(wire.get("locations"), None);
        assert_eq!(start("Grep", &json!({}))["kind"], "search");
    }

    #[test]
    fn commands_are_fenced_as_sh() {
        assert_eq!(command_block("ls -la"), "```sh\nls -la\n```");
        assert_eq!(command_block("echo ```"), "````sh\necho ```\n````");

        let input = json!({"command": "ls"});
        let shown = Shown::of(&ToolName::new("Bash"), &input, None, None).unwrap();
        let start = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Bash"),
            input: &input,
            description: None,
        };
        let wire = update(start, Some(&shown));
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");

        // ToolEnd replaces the content, so the command comes first again.
        let end = AgentEvent::ToolEnd {
            id: &ToolCallId::new("t1"),
            output: "out",
            is_error: false,
            diff: None,
            images: &[],
        };
        let wire = update(end, Some(&shown));
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");
        assert_eq!(wire["content"][1]["content"]["text"], "out");
    }

    #[test]
    fn tool_descriptions_replace_the_command_block() {
        let input = json!({"questions": []});
        let shown = Shown::of(&ToolName::new("Ask"), &input, Some("**Q?**"), None).unwrap();
        let start = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Ask"),
            input: &input,
            description: Some("**Q?**"),
        };
        assert_eq!(
            update(start, Some(&shown))["content"][0]["content"]["text"],
            "**Q?**"
        );
    }

    #[test]
    fn bash_is_a_terminal_when_the_client_supports_it() {
        let input = json!({"command": "false"});
        let shown = Shown::of(&ToolName::new("Bash"), &input, None, Some(Path::new("/w"))).unwrap();
        let start = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Bash"),
            input: &input,
            description: None,
        };
        let wire = update(start, Some(&shown));
        assert_eq!(wire["title"], "false");
        assert_eq!(wire["kind"], "execute");
        let described = json!({"command": "false", "description": "Fail on purpose"});
        let start_described = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Bash"),
            input: &described,
            description: None,
        };
        assert_eq!(
            update(start_described, Some(&shown))["title"],
            "Fail on purpose"
        );
        assert_eq!(
            wire["content"],
            json!([{"type": "terminal", "terminalId": "t1"}])
        );
        assert_eq!(
            wire["_meta"],
            json!({"terminal_info": {"terminal_id": "t1", "cwd": "/w"}})
        );

        let end = AgentEvent::ToolEnd {
            id: &ToolCallId::new("t1"),
            output: "boom\n[exit status: 3]",
            is_error: true,
            diff: None,
            images: &[],
        };
        let wire = update(end, Some(&shown));
        assert_eq!(wire["status"], "failed");
        assert_eq!(
            wire["_meta"]["terminal_output"]["data"],
            "boom\n[exit status: 3]"
        );
        assert_eq!(wire["_meta"]["terminal_exit"]["exit_code"], 3);
        assert!(wire.get("content").is_none());

        // Other tools and clients without the capability keep the fenced block.
        assert!(matches!(
            Shown::of(
                &ToolName::new("BackgroundTask"),
                &input,
                None,
                Some(Path::new("/w"))
            ),
            Some(Shown::Text(_))
        ));
        assert!(matches!(
            Shown::of(&ToolName::new("Bash"), &input, None, None),
            Some(Shown::Text(_))
        ));
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code("x\n[exit status: 2]", true), 2);
        assert_eq!(exit_code("x", true), 1);
        assert_eq!(exit_code("x", false), 0);
    }

    #[test]
    fn long_titles_are_cut() {
        let long = "a".repeat(300);
        let t = title(&ToolName::new("Bash"), &json!({"command": long}));
        assert_eq!(t.chars().count(), MAX_TITLE + 1);
        assert!(t.ends_with('…'));
        assert_eq!(
            title(&ToolName::new("Bash"), &json!({"command": "ls\nmore"})),
            "Bash ls"
        );
        let described = json!({"command": "ls", "description": "List files"});
        assert_eq!(title(&ToolName::new("Bash"), &described), "List files");
        let read = json!({"path": "a", "description": "Check the config"});
        assert_eq!(title(&ToolName::new("Read"), &read), "Check the config");
        // Elsewhere it may be content, e.g. an MCP tool creating a ticket.
        let mcp = json!({"id": "t", "description": "long text"});
        assert_eq!(
            title(&ToolName::new("mcp__jira__create"), &mcp),
            "mcp__jira__create t"
        );
        let grep = json!({"pattern": "fn x", "path": "src"});
        assert_eq!(title(&ToolName::new("Grep"), &grep), "Grep fn x");
        let fetch = json!({"url": "https://x.y", "raw": true});
        assert_eq!(
            title(&ToolName::new("FetchUrl"), &fetch),
            "FetchUrl https://x.y"
        );
        let feature = json!({"feature": "ssh", "reason": "sign commits"});
        assert_eq!(
            title(&ToolName::new("RequestFeature"), &feature),
            "RequestFeature ssh"
        );
        let memory = json!({"action": "search", "query": "x", "description": "Find build notes"});
        assert_eq!(title(&ToolName::new("Memory"), &memory), "Find build notes");
    }

    #[test]
    fn usage_update_has_context_cost_and_cache_breakdown() {
        let request = Usage {
            input: 10,
            output: 50,
            cache_read: 900,
            cache_write: 90,
        };
        let total = Usage {
            input: 20,
            output: 80,
            cache_read: 1000,
            cache_write: 100,
        };
        let mut last = Published::default();
        let wire = serde_json::to_value(to_update(
            AgentEvent::Usage {
                model: &ModelSpec::new("p", "m"),
                usage: request,
                context_window: 1_000_000,
                total,
                cost: 0.5,
            },
            0,
            None,
            Path::new("/w"),
            &mut last,
        ))
        .unwrap();
        assert_eq!(wire["used"], 1050);
        assert_eq!(wire["size"], 1_000_000);
        assert_eq!(wire["cost"], json!({"amount": 0.5, "currency": "USD"}));
        let rho = &wire["_meta"]["rho"];
        assert_eq!(rho["model"], "p/m");
        assert_eq!(
            rho["request"],
            json!({"input": 10, "output": 50, "cacheRead": 900, "cacheWrite": 90})
        );
        assert_eq!(rho["session"]["cacheRead"], 1000);
        assert_eq!(rho["cacheHitRate"], 0.9);
        assert_eq!(rho["cacheTtlSecs"], 300);

        // A model switch republishes the context with the new window.
        let wire = serde_json::to_value(to_update(
            AgentEvent::Model {
                spec: &ModelSpec::new("p", "small"),
                context_window: 200_000,
            },
            1,
            None,
            Path::new("/w"),
            &mut last,
        ))
        .unwrap();
        assert_eq!(
            (wire["used"].clone(), wire["size"].clone()),
            (json!(1050), json!(200_000))
        );
        assert_eq!(wire["_meta"]["rho"]["model"], "p/small");
    }

    #[test]
    fn turn_usage_reports_cache_tokens() {
        let wire = serde_json::to_value(turn_usage(Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
        }))
        .unwrap();
        assert_eq!(
            wire,
            json!({
                "totalTokens": 10,
                "inputTokens": 1,
                "outputTokens": 2,
                "cachedReadTokens": 3,
                "cachedWriteTokens": 4,
            })
        );
    }

    #[test]
    fn timestamps_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn tokens_are_short() {
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(200_000), "200k");
        assert_eq!(tokens(512), "512");
    }

    fn sample_form() -> Form {
        Form {
            message: "Two questions".into(),
            fields: vec![
                Field {
                    key: FieldKey::new("q0"),
                    title: "Which db?".into(),
                    description: Some("Pick one".into()),
                    kind: FieldKind::Choice {
                        options: vec![
                            Choice {
                                value: ChoiceValue::new("pg"),
                                title: "Postgres".into(),
                                description: Some("SQL".into()),
                            },
                            Choice {
                                value: ChoiceValue::new("sqlite"),
                                title: "SQLite".into(),
                                description: None,
                            },
                        ],
                        multiple: false,
                    },
                    required: false,
                },
                Field {
                    key: FieldKey::new("q0_text"),
                    title: "Or your own".into(),
                    description: None,
                    kind: FieldKind::Text,
                    required: false,
                },
                Field {
                    key: FieldKey::new("q1"),
                    title: "Features?".into(),
                    description: None,
                    kind: FieldKind::Choice {
                        options: vec![Choice {
                            value: ChoiceValue::new("a"),
                            title: "A".into(),
                            description: None,
                        }],
                        multiple: true,
                    },
                    required: true,
                },
            ],
        }
    }

    #[test]
    fn forms_become_elicitation_schemas() {
        let wire = serde_json::to_value(form_schema(&sample_form())).unwrap();
        assert_eq!(wire["type"], "object");
        assert_eq!(wire["required"], json!(["q1"]));
        let q0 = &wire["properties"]["q0"];
        assert_eq!(q0["type"], "string");
        assert_eq!(q0["title"], "Which db?");
        assert_eq!(
            q0["oneOf"][0],
            json!({"const": "pg", "title": "Postgres", "description": "SQL"})
        );
        assert_eq!(wire["properties"]["q0_text"]["type"], "string");
        let q1 = &wire["properties"]["q1"];
        assert_eq!(q1["type"], "array");
        assert_eq!(q1["items"]["anyOf"][0]["const"], "a");
    }

    #[test]
    fn form_answers_drop_empty_values() {
        let content = BTreeMap::from([
            ("a".to_string(), ElicitationContentValue::from("x")),
            ("b".to_string(), ElicitationContentValue::from("  ")),
            (
                "c".to_string(),
                ElicitationContentValue::StringArray(vec!["1".into(), "2".into()]),
            ),
            (
                "d".to_string(),
                ElicitationContentValue::StringArray(vec![]),
            ),
        ]);
        let answers = form_answers(Some(content));
        assert_eq!(
            answers,
            Answers::from([
                (FieldKey::new("a"), vec!["x".into()]),
                (FieldKey::new("c"), vec!["1".into(), "2".into()]),
            ])
        );
        assert!(form_answers(None).is_empty());
    }

    #[test]
    fn parses_slash_command() {
        let blocks = |s: &str| [ContentBlock::Text(TextContent::new(s))];
        let b = blocks("/compact");
        assert_eq!(slash_command(&b), Some(("compact", String::new())));
        let b = blocks(" /compact  keep the API notes ");
        assert_eq!(
            slash_command(&b),
            Some(("compact", "keep the API notes".into()))
        );
        let b = blocks("/compactly");
        assert_eq!(slash_command(&b), Some(("compactly", String::new())));
        assert_eq!(slash_command(&blocks("please /compact")), None);
        assert_eq!(slash_command(&blocks("/")), None);
    }
}
