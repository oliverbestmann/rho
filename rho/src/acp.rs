//! Agent Client Protocol frontend: exposes the agent loop to editors (Zed).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, ContentBlock, ContentChunk, Cost, CreateElicitationRequest, Diff,
    ElicitationAction, ElicitationContentValue, ElicitationFormMode, ElicitationSchema,
    ElicitationSessionScope, EmbeddedResourceResource, EnumOption, Implementation,
    InitializeRequest, InitializeResponse, Meta, MultiSelectPropertySchema, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionAdditionalDirectoriesCapabilities,
    SessionCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionId, SessionInfoUpdate, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
    StringPropertySchema, Terminal, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UnstructuredCommandInput, Usage as AcpUsage, UsageUpdate,
};
use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Error, Responder};
use futures::future::{AbortHandle, Abortable};

use crate::agent::{Agent, AgentEvent, Session};
use crate::extension::{Answers, BoxFuture, Context, Field, FieldKind, Form, FormReply, Frontend};
use crate::provider::{self, Content, Thinking, Usage};

type SharedSession = Arc<tokio::sync::Mutex<Session>>;

struct Entry {
    session: SharedSession,
    running: Option<AbortHandle>,
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
    next_id: AtomicU64,
    caps: ClientCaps,
}

impl State {
    /// Registers the run that a cancel notification should abort.
    fn session(&self, sid: &SessionId) -> Option<SharedSession> {
        Some(self.sessions.lock().unwrap().get(sid)?.session.clone())
    }

    fn start_run(
        &self,
        sid: &SessionId,
    ) -> Option<(SharedSession, futures::future::AbortRegistration)> {
        let (abort, registration) = AbortHandle::new_pair();
        let mut sessions = self.sessions.lock().unwrap();
        let entry = sessions.get_mut(sid)?;
        entry.running = Some(abort);
        Some((entry.session.clone(), registration))
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
        state,
    );
    let (prompt_agent, config_agent) = (agent.clone(), agent.clone());

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
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new().session_capabilities(
                            SessionCapabilities::new().additional_directories(
                                SessionAdditionalDirectoriesCapabilities::new(),
                            ),
                        ))
                        .agent_info(Implementation::new("rho", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let n = new_state.next_id.fetch_add(1, Ordering::Relaxed);
                let sid = SessionId::new(format!("rho-{}-{n}", std::process::id()));
                let (agent, state) = (agent.clone(), new_state.clone());
                // Extension session_start hooks are async; keep them off the dispatch loop.
                cx.clone().spawn(async move {
                    let frontend = Arc::new(AcpFrontend {
                        cx: cx.clone(),
                        sid: sid.clone(),
                        notices: AtomicU64::new(0),
                        forms: state.caps.forms.load(Ordering::Relaxed),
                        terminal_cwd: state
                            .caps
                            .terminal_output
                            .load(Ordering::Relaxed)
                            .then(|| req.cwd.clone()),
                        shown: Mutex::default(),
                        last_usage: Mutex::default(),
                    });
                    let session = agent
                        .start_session(
                            sid.0.to_string(),
                            req.cwd,
                            req.additional_directories,
                            frontend,
                        )
                        .await;
                    let ctx = session.ctx.clone();
                    let mut commands = vec![
                        AvailableCommand::new(
                            "compact",
                            "Summarize the conversation to free up context",
                        )
                        .input(AvailableCommandInput::Unstructured(
                            UnstructuredCommandInput::new("optional focus for the summary"),
                        )),
                        AvailableCommand::new(
                            "usage",
                            "Show token, cache and cost usage of this session",
                        ),
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
                    let config_options = config_options(&agent, &session);
                    let session = Arc::new(tokio::sync::Mutex::new(session));
                    state.sessions.lock().unwrap().insert(
                        sid.clone(),
                        Entry {
                            session,
                            running: None,
                        },
                    );
                    cx.spawn(watch_inbox(agent, state, sid.clone(), ctx))?;
                    responder.respond(
                        NewSessionResponse::new(sid.clone()).config_options(config_options),
                    )?;
                    cx.send_notification(SessionNotification::new(
                        sid,
                        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                            commands,
                        )),
                    ))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let Some((session, registration)) = prompt_state.start_run(&req.session_id) else {
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
                                session
                                    .ctx
                                    .emit(AgentEvent::Text(&session.usage_report()));
                                Ok(provider::StopReason::EndTurn)
                            }
                            Some((name, args))
                                if session.commands.iter().any(|c| c.name == name) =>
                            {
                                agent.command(&session, name, &args).await;
                                Ok(provider::StopReason::EndTurn)
                            }
                            _ => {
                                agent
                                    .run(&mut session, to_content(req.prompt.clone()))
                                    .await
                            }
                        }
                    };
                    let result = Abortable::new(turn, registration).await;
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
                let Some(session) = config_state.session(&req.session_id) else {
                    return responder
                        .respond_with_error(Error::invalid_params().data("unknown session"));
                };
                let agent = config_agent.clone();
                // Waits for a running turn: the change applies from the next one.
                cx.spawn(async move {
                    let mut session = session.lock().await;
                    match set_config(&agent, &mut session, &req) {
                        Ok(()) => responder.respond(SetSessionConfigOptionResponse::new(
                            config_options(&agent, &session),
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
                if let Some(s) = cancel_state.sessions.lock().unwrap().get_mut(&n.session_id)
                    && let Some(abort) = s.running.take()
                {
                    abort.abort();
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
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
        let Some((session, registration)) = state.start_run(&sid) else {
            return Ok(());
        };
        let mut session = session.lock().await;
        // A prompt may have consumed the message while we waited for the lock.
        let run = agent.run_triggered(&mut session);
        if let Ok(Some(Err(e))) = Abortable::new(run, registration).await {
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
    last_usage: Mutex<LastUsage>,
}

impl Frontend for AcpFrontend {
    fn event(&self, ev: AgentEvent<'_>) {
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
        let update = to_update(ev, n, shown.as_ref(), &mut self.last_usage.lock().unwrap());
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

/// The session's model and thinking level as ACP config options.
fn config_options(agent: &Agent, session: &Session) -> Vec<SessionConfigOption> {
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

fn to_content(blocks: Vec<ContentBlock>) -> Vec<Content> {
    blocks
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(Content::text(t.text)),
            ContentBlock::ResourceLink(l) => Some(Content::text(format!("@{}", l.uri))),
            ContentBlock::Resource(r) => match r.resource {
                EmbeddedResourceResource::TextResourceContents(t) => Some(Content::text(format!(
                    "<file uri=\"{}\">\n{}\n</file>",
                    t.uri, t.text
                ))),
                _ => None,
            },
            // We advertise no image/audio capability.
            _ => None,
        })
        .collect()
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
    last_usage: &mut LastUsage,
) -> SessionUpdate {
    match ev {
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
        let mut last = LastUsage::default();
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
        let mut last = LastUsage::default();
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
