//! Agent Client Protocol frontend: exposes the agent loop to editors (Zed).

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
    SessionId, SessionInfo as AcpSessionInfo, SessionListCapabilities, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TextContent, UnstructuredCommandInput,
};
use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Error, Responder};
use futures::future::{AbortHandle, AbortRegistration, Abortable};

use crate::agent::{Agent, Session};
use crate::extension::{Context, ProfileId};
use crate::provider::{self, Content, ImageSource, NoteType};
use crate::store::{self, Store};

mod config;
mod dirs;
mod frontend;
mod hub;
mod load;
mod queue;
mod shell;
mod update;

use config::{PROFILE_OPTION, config_options, set_config};
use frontend::{AcpFrontend, To, Viewer};
use hub::Entry;
pub use hub::Hub;
use load::attach;
use update::{meta, to_stop, turn_usage};

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
    /// The web client's connection: new sessions only under `$HOME` or `/tmp`.
    web_limits: bool,
}

impl State {
    /// A viewer for `cx`, ready after the response (`AcpFrontend::ready`).
    /// While the session `starts`, its updates are held until then.
    fn viewer(&self, cx: &ConnectionTo<Client>, starts: bool) -> Viewer {
        Viewer {
            conn: self.conn,
            cx: cx.clone(),
            forms: self.caps.forms.load(Ordering::Relaxed),
            terminal: self.caps.terminal_output.load(Ordering::Relaxed),
            ready: false,
            held: starts.then(Vec::new),
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
    agent_in(agent, Arc::default(), false)
}

/// Builds the ACP agent for one connection; connections on the same `hub`
/// share open sessions. `web_limits`: see `State`.
pub fn agent_in(agent: Arc<Agent>, hub: Arc<Hub>, web_limits: bool) -> impl ConnectTo<Client> {
    let state = Arc::new(State {
        agent,
        hub,
        conn: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
        caps: ClientCaps::default(),
        web_limits,
    });
    let (init_state, new_state, prompt_state, config_state, cancel_state) = (
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
    );
    let (load_state, resume_state, list_state) = (state.clone(), state.clone(), state.clone());
    let (queue_state, unqueue_state) = (state.clone(), state.clone());
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
                if new_state.web_limits {
                    let roots = dirs::roots();
                    let checked = std::iter::once(&req.cwd)
                        .chain(&req.additional_directories)
                        .try_for_each(|d| dirs::allowed(d, &roots).map(drop));
                    if let Err(e) = checked {
                        return responder
                            .respond_with_error(Error::invalid_params().data(format!("{e:#}")));
                    }
                }
                // Unique across processes: sessions are stored by id.
                let id = store::SessionId::generate();
                let sid = SessionId::new(id.as_str());
                let (agent, state) = (new_state.agent.clone(), new_state.clone());
                // Extension session_start hooks are async; keep them off the dispatch loop.
                cx.clone().spawn(async move {
                    let frontend = AcpFrontend::new(&sid, &req.cwd);
                    // session_start hooks must not ask: questions wait for the response.
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
                        commands(&agent, &session),
                    );
                    if let Some(dirs) = session.dirs_notice() {
                        session
                            .ctx
                            .notify(NoteType::Info, "Additional directories", &dirs, false);
                    }
                    register(
                        &agent,
                        &state.hub,
                        &sid,
                        session,
                        frontend.clone(),
                        commands.clone(),
                    );
                    responder
                        .respond(NewSessionResponse::new(sid.clone()).config_options(options))?;
                    frontend.ready(state.conn);
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
                let (hub, sid) = (prompt_state.hub.clone(), req.session_id.clone());
                echo_prompt(&hub, &sid, &frontend, &req.prompt, Some(conn));
                let agent = prompt_state.agent.clone();
                // Not on the connection: the turn goes on if it drops, and
                // cancel notifications get through.
                tokio::spawn(async move {
                    let mut session = session.lock().await;
                    let usage_before = session.usage;
                    let result = turn(&agent, &mut session, req.prompt, registration).await;
                    let usage = turn_usage(session.usage - usage_before);
                    drop(session);
                    next_queued(&agent, &hub, &sid, Some(ended_normally(&result)), Some(run));
                    // The connection may be gone; the session is saved anyway.
                    let _ = match result {
                        Some(Ok(stop)) => {
                            responder.respond(PromptResponse::new(to_stop(stop)).usage(usage))
                        }
                        Some(Err(e)) => responder.respond_with_internal_error(format!("{e:#}")),
                        None => responder
                            .respond(PromptResponse::new(StopReason::Cancelled).usage(usage)),
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
        .on_receive_request(
            async move |req: dirs::DirsRequest, responder: Responder<dirs::DirsResponse>, cx| {
                cx.spawn(async move {
                    let found =
                        tokio::task::spawn_blocking(move || dirs::dirs(&req.path, &dirs::roots()))
                            .await;
                    match found {
                        Ok(Ok(r)) => responder.respond(r),
                        Ok(Err(e)) => responder
                            .respond_with_error(Error::invalid_params().data(format!("{e:#}"))),
                        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: dirs::MkdirRequest, responder: Responder<dirs::MkdirResponse>, _cx| {
                match dirs::mkdir(&req.path, &dirs::roots()) {
                    Ok(dir) => responder.respond(dirs::MkdirResponse { dir }),
                    Err(e) => {
                        responder.respond_with_error(Error::invalid_params().data(format!("{e:#}")))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: queue::QueueRequest,
                        responder: Responder<queue::QueueResponse>,
                        _cx| {
                let (hub, sid) = (&queue_state.hub, &req.session_id);
                if !hub.enqueue(sid, queue_state.conn, req.text) {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                }
                // Starts it if the session is idle.
                next_queued(&queue_state.agent, hub, sid, None, None);
                responder.respond(queue::QueueResponse {})
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: queue::UnqueueRequest,
                        responder: Responder<queue::QueueResponse>,
                        _cx| {
                if !unqueue_state
                    .hub
                    .unqueue(&req.session_id, unqueue_state.conn, req.id)
                {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                }
                responder.respond(queue::QueueResponse {})
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
fn commands(agent: &Agent, session: &Session) -> Vec<AvailableCommand> {
    let commands = agent.commands(session).into_iter();
    commands
        .map(|c| {
            let command = AvailableCommand::new(c.name.to_string(), c.description);
            match c.hint {
                Some(hint) => command.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(hint),
                )),
                None => command,
            }
        })
        .collect()
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
    // Stored sessions open under `Hub::opening`, so this is the only one.
    hub.sessions.lock().unwrap().insert(
        sid.clone(),
        Entry {
            session: session.clone(),
            running: vec![],
            ctx,
            frontend,
            commands,
            watch,
            queue: Default::default(),
            held: false,
        },
    );
    session
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
        if !run_unprompted(&agent, &hub, &sid, &ctx).await {
            return;
        }
    }
}

/// A run without a prompt, see `Agent::run_triggered`. `false` if the
/// session is gone.
pub(super) async fn run_unprompted(
    agent: &Arc<Agent>,
    hub: &Arc<Hub>,
    sid: &SessionId,
    ctx: &Context,
) -> bool {
    let Some((session, _, run, registration)) = hub.start_run(sid, None) else {
        return false;
    };
    let mut session = session.lock().await;
    // A prompt may have consumed the message while we waited for the lock.
    let turn = agent.run_triggered(&mut session);
    let ended = match Abortable::new(turn, registration).await {
        Ok(None) => None,
        Ok(Some(Err(e))) => {
            ctx.notify(NoteType::Info, "rho", &format!("{e:#}"), true);
            Some(false)
        }
        Ok(Some(Ok(stop))) => Some(stop == provider::StopReason::EndTurn),
        Err(_aborted) => {
            interrupted(&mut session).await;
            Some(false)
        }
    };
    drop(session);
    // Nothing ran: the queue did not wait for this.
    if ended.is_some() {
        next_queued(agent, hub, sid, ended, Some(run));
    }
    true
}

/// Saves a cancelled turn now, so a restart does not call its open tool
/// calls interrupted.
async fn interrupted(session: &mut Session) {
    crate::agent::close_open_calls(&mut session.history, crate::agent::CANCELLED);
    session.ctx.notify(NoteType::Info, INTERRUPTED, "", false);
    session.save().await;
}

/// Runs a prompt or a slash command as a turn; `None` if it was cancelled.
async fn turn(
    agent: &Agent,
    session: &mut Session,
    prompt: Vec<ContentBlock>,
    registration: AbortRegistration,
) -> Option<anyhow::Result<provider::StopReason>> {
    let turn = async {
        if let Some((name, args)) = slash_command(&prompt)
            && let Some(done) = agent.slash_command(session, name, &args).await
        {
            return done.map(|()| provider::StopReason::EndTurn);
        }
        agent.run(session, to_content(prompt.clone())?).await
    };
    match Abortable::new(turn, registration).await {
        Ok(result) => Some(result),
        Err(_aborted) => {
            interrupted(session).await;
            None
        }
    }
}

/// The turn ended normally; a stopped or failed one holds the queue.
fn ended_normally(result: &Option<anyhow::Result<provider::StopReason>>) -> bool {
    matches!(result, Some(Ok(provider::StopReason::EndTurn)))
}

/// The other connections on the session see a prompt from `conn` (all of
/// them one from the queue), later ones too; a command only by the note it
/// replies with.
fn echo_prompt(
    hub: &Hub,
    sid: &SessionId,
    frontend: &AcpFrontend,
    prompt: &[ContentBlock],
    conn: Option<ConnId>,
) {
    if slash_command(prompt).is_some_and(|(name, _)| hub.is_command(sid, name)) {
        return;
    }
    for block in prompt {
        let update = SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone()));
        frontend.echo(update, conn);
    }
}

/// Starts the next queued prompt, unless the queue is held: after a turn
/// that `ended` (normally or not), or with `None` when something was
/// queued, if nothing runs. The ended turn's `run` is dropped once the
/// next one is registered, so the session stays busy in between.
fn next_queued(
    agent: &Arc<Agent>,
    hub: &Arc<Hub>,
    sid: &SessionId,
    ended: Option<bool>,
    run: Option<hub::Run>,
) {
    let next = hub
        .take_queued(sid, ended)
        .and_then(|text| Some((text, hub.start_run(sid, None)?)));
    drop(run);
    let Some((text, (session, frontend, run, registration))) = next else {
        return;
    };
    let (agent, hub, sid) = (agent.clone(), hub.clone(), sid.clone());
    tokio::spawn(async move {
        let prompt = vec![ContentBlock::Text(TextContent::new(text))];
        echo_prompt(&hub, &sid, &frontend, &prompt, None);
        let mut session = session.lock().await;
        let result = turn(&agent, &mut session, prompt, registration).await;
        // Nobody waits for a response.
        if let Some(Err(e)) = &result {
            session
                .ctx
                .notify(NoteType::Info, "rho", &format!("{e:#}"), true);
        }
        drop(session);
        next_queued(&agent, &hub, &sid, Some(ended_normally(&result)), Some(run));
    });
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
