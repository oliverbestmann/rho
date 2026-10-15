//! Agent Client Protocol frontend: exposes the agent loop to editors (Zed).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, CloseSessionRequest, CloseSessionResponse, ConfigOptionUpdate,
    ContentBlock, ContentChunk, EmbeddedResourceResource, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
    PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionAdditionalDirectoriesCapabilities, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionId,
    SessionInfo as AcpSessionInfo, SessionListCapabilities, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, UnstructuredCommandInput,
};
use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Error, JsonRpcResponse, Responder};
use futures::future::{AbortHandle, Abortable};

use crate::agent::{Agent, AgentEvent, Session};
use crate::extension::{CommandName, Context, ProfileId};
use crate::provider::{self, Content, ImageSource, Role, Thinking, ToolCallId};
use crate::store::{self, Store};
use crate::tools;

mod frontend;
mod hub;
mod update;

use frontend::{AcpFrontend, To, Viewer};
use hub::Entry;
pub use hub::Hub;
use update::{image_block, meta, to_stop, turn_usage};

type SharedSession = Arc<tokio::sync::Mutex<Session>>;

/// One ACP connection, unique in the process.
type ConnId = u64;

static NEXT_CONN: AtomicU64 = AtomicU64::new(0);

/// What the client told us in `initialize`.
#[derive(Default)]
struct ClientCaps {
    /// Can show elicitation forms.
    forms: AtomicBool,
    /// Understands Zed's `_meta` terminal output for tool calls.
    terminal_output: AtomicBool,
}

/// One connection's view: the agent, the shared hub and what this client
/// can do.
struct State {
    agent: Arc<Agent>,
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

/// Builds the ACP agent with its own hub; connect it to a transport, e.g.
/// `Stdio::new()`.
pub fn agent(agent: Arc<Agent>) -> impl ConnectTo<Client> {
    agent_in(agent, Arc::default())
}

/// Builds the ACP agent for one connection; connections on the same `hub`
/// share open sessions.
pub fn agent_in(agent: Arc<Agent>, hub: Arc<Hub>) -> impl ConnectTo<Client> {
    let state = Arc::new(State {
        agent,
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
                let saved = init_state.agent.sessions_db.is_some();
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
                let (agent, state) = (new_state.agent.clone(), new_state.clone());
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
                let (agent, state) = (load_state.agent.clone(), load_state.clone());
                cx.clone().spawn(async move {
                    let (sid, cwd, dirs) = (req.session_id, req.cwd, req.additional_directories);
                    let respond = |o| LoadSessionResponse::new().config_options(o);
                    attach(
                        &agent, &state, &cx, sid, cwd, dirs, true, responder, respond,
                    )
                    .await
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ResumeSessionRequest,
                        responder: Responder<ResumeSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let (agent, state) = (resume_state.agent.clone(), resume_state.clone());
                cx.clone().spawn(async move {
                    let (sid, cwd, dirs) = (req.session_id, req.cwd, req.additional_directories);
                    let respond = |o| ResumeSessionResponse::new().config_options(o);
                    attach(
                        &agent, &state, &cx, sid, cwd, dirs, false, responder, respond,
                    )
                    .await
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        cx: ConnectionTo<Client>| {
                let (agent, hub) = (list_state.agent.clone(), list_state.hub.clone());
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
                let agent = prompt_state.agent.clone();
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
                let agent = config_state.agent.clone();
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
    let tools = session.tools().map(|(n, t)| (n.clone(), t.clone()));
    let _ = frontend.tools.set(tools.collect());
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

/// `session/load` (with `replay`) and `session/resume`: opens the session
/// with `open_stored`, answers with `response`, then makes the connection
/// ready and sends the commands.
#[expect(clippy::too_many_arguments, reason = "the two handlers' shared body")]
async fn attach<T: JsonRpcResponse>(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: bool,
    responder: Responder<T>,
    response: impl FnOnce(Vec<SessionConfigOption>) -> T,
) -> Result<(), Error> {
    match open_stored(agent, state, cx, &sid, cwd, additional_dirs, replay).await {
        Ok((frontend, options, commands)) => {
            responder.respond(response(options))?;
            frontend.ready(state.conn);
            send_commands(cx, &sid, commands)
        }
        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
    }
}

/// The open session (also one opened by another connection), or the stored
/// one continued.
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
        // follow. shortcut: the new directories are not applied; a lock-free
        // copy of the roots would fix it.
        if replay {
            let messages: Vec<_> = stored.into_iter().map(|m| (m.epoch, m.message)).collect();
            replay_history(&frontend, conn, &ctx, &messages, &diffs);
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
        replay_history(&frontend, conn, &ctx, &messages, &diffs);
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
/// while it is in `diffs` (see `Store::save_diffs`).
fn replay_history(
    frontend: &AcpFrontend,
    conn: ConnId,
    ctx: &Context,
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
                    let tool = frontend.tools.get().and_then(|t| t.get(name));
                    let description = tool.and_then(|t| t.describe(input));
                    emit(AgentEvent::ToolStart {
                        id,
                        name,
                        input,
                        description: description.as_deref(),
                        kind: tool.map_or_else(tools::ToolKind::default, |t| t.kind()),
                        titled: tool.is_some_and(|t| t.titled()),
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

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::TextContent;

    use super::*;

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
