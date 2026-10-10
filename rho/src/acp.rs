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
    StringPropertySchema, Terminal, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UnstructuredCommandInput, Usage as AcpUsage, UsageUpdate,
};
use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Error, Responder};
use futures::future::{AbortHandle, Abortable};

use crate::agent::{Agent, AgentEvent, Session};
use crate::extension::{Answers, BoxFuture, Context, Field, FieldKind, Form, FormReply, Frontend};
use crate::provider::{self, Content, ImageSource, Role, Thinking, Usage};
use crate::store::Store;

type SharedSession = Arc<tokio::sync::Mutex<Session>>;

struct Entry {
    session: SharedSession,
    /// Runs that `session/cancel` aborts: a prompt, and a run for a
    /// triggered message (`watch_inbox`) it may be waiting for.
    running: Vec<(u64, AbortHandle)>,
    /// Reachable while a turn holds the session, e.g. to switch profiles.
    ctx: Context,
    frontend: Arc<AcpFrontend>,
}

/// What the client told us in `initialize`.
#[derive(Default)]
struct ClientCaps {
    /// Can show elicitation forms.
    forms: AtomicBool,
    /// Understands Zed's `_meta` terminal output for tool calls.
    terminal_output: AtomicBool,
}

#[derive(Default)]
struct State {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    next_run: AtomicU64,
    caps: ClientCaps,
}

impl State {
    fn session(&self, sid: &SessionId) -> Option<(SharedSession, Context, Arc<AcpFrontend>)> {
        let sessions = self.sessions.lock().unwrap();
        let e = sessions.get(sid)?;
        Some((e.session.clone(), e.ctx.clone(), e.frontend.clone()))
    }

    /// Registers a run that a cancel notification should abort, until the
    /// returned `Run` is dropped.
    fn start_run(
        self: &Arc<Self>,
        sid: &SessionId,
    ) -> Option<(SharedSession, Run, futures::future::AbortRegistration)> {
        let (abort, registration) = AbortHandle::new_pair();
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions.get_mut(sid)?;
        entry.running.push((id, abort));
        let run = Run {
            state: self.clone(),
            sid: sid.clone(),
            id,
        };
        Some((entry.session.clone(), run, registration))
    }

    /// Aborts the session's runs.
    fn cancel(&self, sid: &SessionId) {
        if let Some(entry) = self.sessions.lock().unwrap().get_mut(sid) {
            for (_, abort) in entry.running.drain(..) {
                abort.abort();
            }
        }
    }
}

/// A run registered with `State::start_run`.
struct Run {
    state: Arc<State>,
    sid: SessionId,
    id: u64,
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(entry) = self.state.sessions.lock().unwrap().get_mut(&self.sid) {
            entry.running.retain(|(id, _)| *id != self.id);
        }
    }
}

/// Builds the ACP agent; connect it to a transport, e.g. `Stdio::new()`.
pub fn agent(agent: Arc<Agent>) -> impl ConnectTo<Client> {
    let state = Arc::new(State::default());
    let (init_state, new_state, prompt_state, config_state, cancel_state) = (
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
    );
    let (load_state, resume_state, close_state) = (state.clone(), state.clone(), state);
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
                                .prompt_capabilities(PromptCapabilities::new().image(true))
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
                let sid = SessionId::new(crate::store::new_session_id());
                let (agent, state) = (agent.clone(), new_state.clone());
                // Extension session_start hooks are async; keep them off the dispatch loop.
                cx.clone().spawn(async move {
                    let frontend = frontend(&cx, &sid, &state, &req.cwd);
                    let started = agent
                        .start_session(
                            sid.0.to_string(),
                            req.cwd,
                            req.additional_directories,
                            frontend.clone(),
                        )
                        .await;
                    let session = match started {
                        Ok(s) => s,
                        Err(e) => return responder.respond_with_internal_error(format!("{e:#}")),
                    };
                    let (options, commands) = (
                        frontend.remember(config_options(&agent, &session)),
                        commands(&session),
                    );
                    let (ctx, dirs) = (session.ctx.clone(), session.dirs_notice());
                    register(&agent, &state, &cx, &sid, session, frontend)?;
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
                        Ok((options, commands)) => {
                            responder
                                .respond(LoadSessionResponse::new().config_options(options))?;
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
                        Ok((options, commands)) => {
                            responder
                                .respond(ResumeSessionResponse::new().config_options(options))?;
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
                let agent = list_agent.clone();
                cx.spawn(async move {
                    match list_sessions(&agent, req).await {
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
                // Dropping the entry ends the session once a running turn lets go.
                close_state.cancel(&req.session_id);
                close_state.sessions.lock().unwrap().remove(&req.session_id);
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let Some((session, run, registration)) = prompt_state.start_run(&req.session_id)
                else {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                };
                let agent = prompt_agent.clone();
                // The turn runs outside the dispatch loop so cancel notifications get through.
                cx.spawn(async move {
                    let mut session = session.lock().await;
                    let usage_before = session.usage;
                    let turn = async {
                        match slash_command(&req.prompt) {
                            Some(("compact", focus)) => agent
                                .compact(&mut session, &focus)
                                .await
                                .map(|()| provider::StopReason::EndTurn),
                            Some(("usage", _)) => {
                                session.ctx.emit(AgentEvent::Text(&session.usage_report()));
                                Ok(provider::StopReason::EndTurn)
                            }
                            Some((name, args))
                                if session.commands.iter().any(|c| c.name == name) =>
                            {
                                agent.command(&session, name, &args).await;
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
                    match result {
                        Ok(Ok(stop)) => {
                            responder.respond(PromptResponse::new(to_stop(stop)).usage(usage))
                        }
                        Ok(Err(e)) => responder.respond_with_internal_error(format!("{e:#}")),
                        Err(_aborted) => responder
                            .respond(PromptResponse::new(StopReason::Cancelled).usage(usage)),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                let Some((session, ctx, frontend)) = config_state.session(&req.session_id) else {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                };
                // A running turn picks it up with its next request.
                if *req.config_id.0 == *PROFILE_OPTION {
                    let value = req.value.as_value_id().map(|v| v.0.to_string());
                    return match ctx.set_profile(value.as_deref().unwrap_or_default()) {
                        Ok(()) => responder
                            .respond(SetSessionConfigOptionResponse::new(frontend.options())),
                        Err(e) => responder
                            .respond_with_error(Error::invalid_params().data(format!("{e:#}"))),
                    };
                }
                let agent = config_agent.clone();
                // Waits for a running turn: the change applies from the next one.
                cx.spawn(async move {
                    let mut session = session.lock().await;
                    let result = set_config(&agent, &mut session, &req);
                    session.save().await;
                    match result {
                        Ok(()) => responder.respond(SetSessionConfigOptionResponse::new(
                            frontend.remember(config_options(&agent, &session)),
                        )),
                        Err(e) => responder
                            .respond_with_error(Error::invalid_params().data(format!("{e:#}"))),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                cancel_state.cancel(&n.session_id);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

fn frontend(
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    state: &State,
    cwd: &Path,
) -> Arc<AcpFrontend> {
    Arc::new(AcpFrontend {
        cx: cx.clone(),
        sid: sid.clone(),
        notices: AtomicU64::new(0),
        forms: state.caps.forms.load(Ordering::Relaxed),
        terminal_cwd: state
            .caps
            .terminal_output
            .load(Ordering::Relaxed)
            .then(|| cwd.to_path_buf()),
        shown: Mutex::default(),
        published: Mutex::default(),
    })
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
    ];
    for c in &session.commands {
        let mut command = AvailableCommand::new(&c.name, &c.description);
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
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    session: Session,
    frontend: Arc<AcpFrontend>,
) -> Result<SharedSession, Error> {
    let ctx = session.ctx.clone();
    let session = Arc::new(tokio::sync::Mutex::new(session));
    state.sessions.lock().unwrap().insert(
        sid.clone(),
        Entry {
            session: session.clone(),
            running: vec![],
            ctx: ctx.clone(),
            frontend,
        },
    );
    cx.spawn(watch_inbox(agent.clone(), state.clone(), sid.clone(), ctx))?;
    Ok(session)
}

/// `session/load` (with `replay`) and `session/resume`: the open session, or
/// the stored one continued. Returns its config options and commands.
async fn open_stored(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: bool,
) -> anyhow::Result<(Vec<SessionConfigOption>, Vec<AvailableCommand>)> {
    // An open session takes the new directories after the replay, so the
    // notice follows the history.
    let (session, frontend, dirs) = match state.session(sid) {
        Some((session, _, frontend)) => (session, frontend, Some(additional_dirs)),
        None => {
            let frontend = frontend(cx, sid, state, &cwd);
            let session = agent
                .resume_session(&sid.0, &cwd, additional_dirs, frontend.clone())
                .await?;
            let session = register(agent, state, cx, sid, session, frontend.clone())?;
            (session, frontend, None)
        }
    };
    let mut session = session.lock().await;
    if session.ctx.cwd() != cwd {
        anyhow::bail!(
            "session {} belongs to {}",
            sid.0,
            session.ctx.cwd().display()
        );
    }
    if replay {
        replay_history(cx, sid, &session)?;
    }
    if let Some(dirs) = dirs
        && agent.set_additional_dirs(&mut session, dirs)
    {
        session.save().await;
    }
    // A new connection has not seen it; the frontend also needs it to
    // republish on a model switch.
    session.emit_usage();
    Ok((
        frontend.remember(config_options(agent, &session)),
        commands(&session),
    ))
}

/// Sends the history as session updates, as if it was happening now. Edit
/// diffs are not stored, so edits show their output only.
fn replay_history(
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    session: &Session,
) -> Result<(), Error> {
    let ctx = &session.ctx;
    for msg in &session.history {
        let mut user: Vec<ContentBlock> = vec![];
        for block in &msg.content {
            match block {
                Content::Text { text } if msg.role == Role::User => match user.last_mut() {
                    Some(ContentBlock::Text(last)) => {
                        last.text.push_str("\n\n");
                        last.text.push_str(text);
                    }
                    _ => user.push(text.as_str().into()),
                },
                Content::Image { source } => user.push(image_block(source)),
                Content::Text { text } => ctx.emit(AgentEvent::Text(text)),
                Content::Thinking { thinking, .. } => ctx.emit(AgentEvent::Thinking(thinking)),
                Content::ToolUse { id, name, input } => {
                    let description = session
                        .tools
                        .iter()
                        .find(|t| t.spec().name == *name)
                        .and_then(|t| t.describe(input));
                    ctx.emit(AgentEvent::ToolStart {
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
                } => ctx.emit(AgentEvent::ToolEnd {
                    id: tool_use_id,
                    output: content,
                    is_error: *is_error,
                    diff: None,
                    images,
                }),
                Content::RedactedThinking { .. } => {}
            }
        }
        for block in user {
            cx.send_notification(SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(block)),
            ))?;
        }
    }
    if let Some(title) = ctx.title() {
        ctx.emit(AgentEvent::Title(&title));
    }
    Ok(())
}

const LIST_PAGE: usize = 100;

/// `session/list`. Without a cwd filter, the database for rho's own cwd is
/// listed.
async fn list_sessions(
    agent: &Agent,
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
            AcpSessionInfo::new(s.id, s.cwd)
                .additional_directories(s.additional_dirs)
                .title(s.title)
                .updated_at(rfc3339(s.updated_at))
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
/// stream `session/update`s (like pi-acp does).
async fn watch_inbox(
    agent: Arc<Agent>,
    state: Arc<State>,
    sid: SessionId,
    ctx: Context,
) -> Result<(), Error> {
    loop {
        ctx.wait_inbox().await;
        if !ctx.has_triggered() {
            continue;
        }
        let Some((session, _run, registration)) = state.start_run(&sid) else {
            return Ok(());
        };
        let mut session = session.lock().await;
        // A prompt may have consumed the message while we waited for the lock.
        let turn = agent.run_triggered(&mut session);
        if let Ok(Some(Err(e))) = Abortable::new(turn, registration).await {
            ctx.notify("rho", &format!("{e:#}"), true);
        }
    }
}

struct AcpFrontend {
    cx: ConnectionTo<Client>,
    sid: SessionId,
    notices: AtomicU64,
    /// The client can show elicitation forms, see `Frontend::ask`.
    forms: bool,
    /// The session's cwd if the client shows `Bash` calls as terminals.
    terminal_cwd: Option<PathBuf>,
    /// How running tool calls were shown, by id. `ToolEnd` replaces the
    /// call's content, so it has to repeat the command or the questions.
    shown: Mutex<HashMap<String, Shown>>,
    published: Mutex<Published>,
}

impl AcpFrontend {
    fn remember(&self, options: Vec<SessionConfigOption>) -> Vec<SessionConfigOption> {
        options.clone_into(&mut self.published.lock().unwrap().options);
        options
    }

    fn options(&self) -> Vec<SessionConfigOption> {
        self.published.lock().unwrap().options.clone()
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
                let shown = Shown::of(name, input, *description, self.terminal_cwd.as_deref());
                if let Some(shown) = &shown {
                    self.shown
                        .lock()
                        .unwrap()
                        .insert(id.to_string(), shown.clone());
                }
                shown
            }
            AgentEvent::ToolEnd { id, .. } => self.shown.lock().unwrap().remove(*id),
            _ => None,
        };
        let update = to_update(ev, n, shown.as_ref(), &mut self.published.lock().unwrap());
        // Dropped updates only affect display; the turn continues.
        let _ = self
            .cx
            .send_notification(SessionNotification::new(self.sid.clone(), update));
    }

    fn confirm<'a>(&'a self, tool_call_id: Option<&'a str>, title: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let call = ToolCallUpdate::new(
                tool_call_id.unwrap_or("confirm").to_string(),
                ToolCallUpdateFields::new().title(title.to_string()),
            );
            let request = RequestPermissionRequest::new(
                self.sid.clone(),
                call,
                vec![
                    PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                    PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
                ],
            );
            match self.cx.send_request(request).block_task().await {
                Ok(r) => {
                    matches!(r.outcome, RequestPermissionOutcome::Selected(s) if &*s.option_id.0 == "allow")
                }
                Err(_) => false,
            }
        })
    }

    fn ask<'a>(&'a self, form: &'a Form) -> BoxFuture<'a, FormReply> {
        Box::pin(async move {
            if !self.forms {
                return FormReply::Unsupported;
            }
            let request = CreateElicitationRequest::new(
                ElicitationFormMode::new(
                    ElicitationSessionScope::new(self.sid.clone()),
                    form_schema(form),
                ),
                form.message.clone(),
            );
            match self.cx.send_request(request).block_task().await {
                Ok(r) => match r.action {
                    ElicitationAction::Accept(a) => FormReply::Answers(form_answers(a.content)),
                    _ => FormReply::Declined,
                },
                Err(_) => FormReply::Declined,
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
                key,
                StringPropertySchema::new()
                    .title(title)
                    .description(description),
                *required,
            ),
            FieldKind::Choice { options, multiple } => {
                let options = options
                    .iter()
                    .map(|o| EnumOption::new(&o.value, &o.title).description(o.description.clone()))
                    .collect();
                if *multiple {
                    schema.property(
                        key,
                        MultiSelectPropertySchema::titled(options)
                            .title(title)
                            .description(description),
                        *required,
                    )
                } else {
                    schema.property(
                        key,
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
            answers.insert(key, values);
        }
    }
    answers
}

const MODEL_OPTION: &str = "model";
const THINKING_OPTION: &str = "thought_level";
const PROFILE_OPTION: &str = "mode";

/// The session's profile (if there is a choice), model and thinking level
/// as ACP config options.
fn config_options(agent: &Agent, session: &Session) -> Vec<SessionConfigOption> {
    let mut options = vec![];
    let profiles = session.ctx.profiles();
    if profiles.len() > 1 {
        let choices: Vec<_> = profiles
            .iter()
            .map(|p| {
                SessionConfigSelectOption::new(p.id.clone(), p.name.clone())
                    .description(p.description.clone())
            })
            .collect();
        options.push(
            SessionConfigOption::select(
                PROFILE_OPTION,
                "Profile",
                session.ctx.profile().id,
                choices,
            )
            .category(SessionConfigOptionCategory::Mode)
            .description("What the agent may do in this session"),
        );
    }
    options.extend(model_options(agent, session));
    options
}

fn model_options(agent: &Agent, session: &Session) -> Vec<SessionConfigOption> {
    let mut models: Vec<_> = agent
        .registry
        .models()
        .into_iter()
        .map(|(spec, m)| {
            let mut description = format!("{spec}, {} context", tokens(m.context_window));
            if let Some(p) = m.price {
                description += &format!(", ${}/${} per MTok", p.input, p.output);
            }
            SessionConfigSelectOption::new(spec, m.name).description(description)
        })
        .collect();
    let mut options = vec![];
    // Without any known model there is nothing to pick from.
    if !models.is_empty() {
        // A model from `-m` that the provider does not list.
        if models.iter().all(|m| *m.value.0 != *session.model_spec) {
            models.insert(
                0,
                SessionConfigSelectOption::new(session.model_spec.clone(), &session.model_spec),
            );
        }
        options.push(
            SessionConfigOption::select(MODEL_OPTION, "Model", session.model_spec.clone(), models)
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
        MODEL_OPTION => {
            let listed = agent
                .registry
                .models()
                .iter()
                .any(|(spec, _)| spec == value);
            if !listed && value != session.model_spec {
                anyhow::bail!("unknown model {value:?}");
            }
            agent.set_model(session, value)
        }
        THINKING_OPTION => {
            session.thinking = value.parse()?;
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
        name: &str,
        input: &serde_json::Value,
        description: Option<&str>,
        terminal_cwd: Option<&Path>,
    ) -> Option<Self> {
        if let Some(text) = description {
            return Some(Self::Text(text.into()));
        }
        let command = input["command"].as_str()?;
        Some(match terminal_cwd {
            Some(cwd) if name == "Bash" => Self::Terminal {
                cwd: cwd.into(),
                command: command.into(),
            },
            _ => Self::Text(command_block(command)),
        })
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

fn to_update(
    ev: AgentEvent,
    n: u64,
    shown: Option<&Shown>,
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
                .content(vec![text.into()]),
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
                model: model.into(),
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
            last_usage.model = spec.into();
            usage_update(last_usage, context_window)
        }
        AgentEvent::ToolStart {
            id, name, input, ..
        } => {
            let kind = match name {
                "Read" => ToolKind::Read,
                "Write" | "Edit" => ToolKind::Edit,
                "Bash" => ToolKind::Execute,
                _ => ToolKind::Other,
            };
            let call = match shown {
                Some(Shown::Terminal { cwd, command }) => ToolCall::new(id.to_string(), command)
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
            SessionUpdate::ToolCall(
                call.kind(kind)
                    .status(ToolCallStatus::InProgress)
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

/// "Bash first line of the command", at most `MAX_TITLE` characters.
fn title(name: &str, input: &serde_json::Value) -> String {
    let arg = ["command", "path"].iter().find_map(|k| input[k].as_str());
    let Some(arg) = arg else {
        return name.to_string();
    };
    let title = format!("{name} {}", arg.lines().next().unwrap_or_default());
    match title.char_indices().nth(MAX_TITLE) {
        Some((end, _)) => format!("{}…", &title[..end]),
        None => title,
    }
}

fn to_stop(stop: provider::StopReason) -> StopReason {
    match stop {
        provider::StopReason::MaxTokens => StopReason::MaxTokens,
        provider::StopReason::Refusal => StopReason::Refusal,
        provider::StopReason::MaxTurns => StopReason::MaxTurnRequests,
        provider::StopReason::Stopped => StopReason::Cancelled,
        provider::StopReason::EndTurn | provider::StopReason::ToolUse => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::TextContent;
    use serde_json::json;

    use super::*;
    use crate::extension::{Choice, Diff as FileDiff};

    fn update(ev: AgentEvent, shown: Option<&Shown>) -> serde_json::Value {
        let mut last = Published::default();
        serde_json::to_value(to_update(ev, 0, shown, &mut last)).unwrap()
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
                id: "t1",
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
                id: "t1",
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
    fn commands_are_fenced_as_sh() {
        assert_eq!(command_block("ls -la"), "```sh\nls -la\n```");
        assert_eq!(command_block("echo ```"), "````sh\necho ```\n````");

        let input = json!({"command": "ls"});
        let shown = Shown::of("Bash", &input, None, None).unwrap();
        let start = AgentEvent::ToolStart {
            id: "t1",
            name: "Bash",
            input: &input,
            description: None,
        };
        let wire = update(start, Some(&shown));
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");

        // ToolEnd replaces the content, so the command comes first again.
        let end = AgentEvent::ToolEnd {
            id: "t1",
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
        let shown = Shown::of("Ask", &input, Some("**Q?**"), None).unwrap();
        let start = AgentEvent::ToolStart {
            id: "t1",
            name: "Ask",
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
        let shown = Shown::of("Bash", &input, None, Some(Path::new("/w"))).unwrap();
        let start = AgentEvent::ToolStart {
            id: "t1",
            name: "Bash",
            input: &input,
            description: None,
        };
        let wire = update(start, Some(&shown));
        assert_eq!(wire["title"], "false");
        assert_eq!(wire["kind"], "execute");
        assert_eq!(
            wire["content"],
            json!([{"type": "terminal", "terminalId": "t1"}])
        );
        assert_eq!(
            wire["_meta"],
            json!({"terminal_info": {"terminal_id": "t1", "cwd": "/w"}})
        );

        let end = AgentEvent::ToolEnd {
            id: "t1",
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
            Shown::of("BackgroundTask", &input, None, Some(Path::new("/w"))),
            Some(Shown::Text(_))
        ));
        assert!(matches!(
            Shown::of("Bash", &input, None, None),
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
        let t = title("Bash", &json!({"command": long}));
        assert_eq!(t.chars().count(), MAX_TITLE + 1);
        assert!(t.ends_with('…'));
        assert_eq!(title("Bash", &json!({"command": "ls\nmore"})), "Bash ls");
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
                model: "p/m",
                usage: request,
                context_window: 1_000_000,
                total,
                cost: 0.5,
            },
            0,
            None,
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
                spec: "p/small",
                context_window: 200_000,
            },
            1,
            None,
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
                    key: "q0".into(),
                    title: "Which db?".into(),
                    description: Some("Pick one".into()),
                    kind: FieldKind::Choice {
                        options: vec![
                            Choice {
                                value: "pg".into(),
                                title: "Postgres".into(),
                                description: Some("SQL".into()),
                            },
                            Choice {
                                value: "sqlite".into(),
                                title: "SQLite".into(),
                                description: None,
                            },
                        ],
                        multiple: false,
                    },
                    required: false,
                },
                Field {
                    key: "q0_text".into(),
                    title: "Or your own".into(),
                    description: None,
                    kind: FieldKind::Text,
                    required: false,
                },
                Field {
                    key: "q1".into(),
                    title: "Features?".into(),
                    description: None,
                    kind: FieldKind::Choice {
                        options: vec![Choice {
                            value: "a".into(),
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
                ("a".into(), vec!["x".into()]),
                ("c".into(), vec!["1".into(), "2".into()]),
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
