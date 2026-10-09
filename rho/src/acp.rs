//! Agent Client Protocol frontend: exposes the agent loop to editors (Zed).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, ContentBlock, ContentChunk, Diff, EmbeddedResourceResource, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionAdditionalDirectoriesCapabilities, SessionCapabilities,
    SessionId, SessionInfoUpdate, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UnstructuredCommandInput,
    UsageUpdate,
};
use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Error, Responder};
use futures::future::{AbortHandle, Abortable};

use crate::agent::{Agent, AgentEvent, Session};
use crate::extension::{BoxFuture, Context, Frontend};
use crate::provider::{self, Content};

type SharedSession = Arc<tokio::sync::Mutex<Session>>;

struct Entry {
    session: SharedSession,
    running: Option<AbortHandle>,
}

#[derive(Default)]
struct State {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    next_id: AtomicU64,
}

impl State {
    /// Registers the run that a cancel notification should abort.
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
    let (new_state, prompt_state, cancel_state) = (state.clone(), state.clone(), state);
    let prompt_agent = agent.clone();

    agent_client_protocol::Agent
        .builder()
        .name("rho")
        .on_receive_request(
            async move |_req: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
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
                        commands: Mutex::default(),
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
                    let session = Arc::new(tokio::sync::Mutex::new(session));
                    state.sessions.lock().unwrap().insert(
                        sid.clone(),
                        Entry {
                            session,
                            running: None,
                        },
                    );
                    cx.spawn(watch_inbox(agent, state, sid.clone(), ctx))?;
                    responder.respond(NewSessionResponse::new(sid.clone()))?;
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
                    let turn = async {
                        match slash_command(&req.prompt) {
                            Some(("compact", focus)) => agent
                                .compact(&mut session, &focus)
                                .await
                                .map(|()| provider::StopReason::EndTurn),
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
                    match Abortable::new(turn, registration).await {
                        Ok(Ok(stop)) => responder.respond(PromptResponse::new(to_stop(stop))),
                        Ok(Err(e)) => responder.respond_with_internal_error(format!("{e:#}")),
                        Err(_aborted) => {
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
                        }
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
    /// Shell commands of running tool calls, by id; ToolEnd replaces the
    /// call's content, so it has to repeat the command.
    commands: Mutex<HashMap<String, String>>,
}

impl Frontend for AcpFrontend {
    fn event(&self, ev: AgentEvent<'_>) {
        let n = self.notices.fetch_add(1, Ordering::Relaxed);
        let command = match &ev {
            AgentEvent::ToolStart { id, input, .. } => {
                if let Some(c) = input["command"].as_str() {
                    self.commands
                        .lock()
                        .unwrap()
                        .insert(id.to_string(), c.to_string());
                }
                None
            }
            AgentEvent::ToolEnd { id, .. } => self.commands.lock().unwrap().remove(*id),
            _ => None,
        };
        // Dropped updates only affect display; the turn continues.
        let _ = self
            .cx
            .send_notification(SessionNotification::new(self.sid.clone(), to_update(ev, n, command.as_deref())));
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

/// `command` is the shell command of a finished tool call, see `AcpFrontend::commands`.
fn to_update(ev: AgentEvent, n: u64, command: Option<&str>) -> SessionUpdate {
    match ev {
        // Rendered as a finished tool call, like pi-acp does for extension messages.
        AgentEvent::Notice {
            title,
            text,
            is_error,
        } => SessionUpdate::ToolCall(
            ToolCall::new(format!("notice-{n}"), title)
                .kind(ToolKind::Other)
                .status(if is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .content(vec![text.into()]),
        ),
        AgentEvent::Title(t) => {
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(t.to_string()))
        }
        AgentEvent::Text(t) => SessionUpdate::AgentMessageChunk(ContentChunk::new(t.into())),
        AgentEvent::Thinking(t) => SessionUpdate::AgentThoughtChunk(ContentChunk::new(t.into())),
        AgentEvent::Usage {
            usage,
            context_window,
        } => SessionUpdate::UsageUpdate(UsageUpdate::new(
            usage.prompt() + usage.output,
            context_window,
        )),
        AgentEvent::ToolStart { id, name, input } => {
            let kind = match name {
                "Read" => ToolKind::Read,
                "Write" | "Edit" => ToolKind::Edit,
                "Bash" => ToolKind::Execute,
                _ => ToolKind::Other,
            };
            let mut call = ToolCall::new(id.to_string(), title(name, input))
                .kind(kind)
                .status(ToolCallStatus::InProgress)
                .raw_input(input.clone());
            if let Some(c) = input["command"].as_str() {
                call = call.content(vec![command_block(c).as_str().into()]);
            }
            SessionUpdate::ToolCall(call)
        }
        AgentEvent::ToolEnd {
            id,
            output,
            is_error,
            diff,
        } => {
            let mut content = vec![];
            if let Some(c) = command {
                content.push(command_block(c).as_str().into());
            }
            if let Some(d) = diff {
                content.push(Diff::new(&d.path, &d.new).old_text(d.old.clone()).into());
            }
            content.push(output.into());
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_string(),
                ToolCallUpdateFields::new()
                    .status(if is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .content(content),
            ))
        }
    }
}

fn title(name: &str, input: &serde_json::Value) -> String {
    let arg = ["command", "path"].iter().find_map(|k| input[k].as_str());
    match arg {
        Some(a) => format!("{name} {}", a.lines().next().unwrap_or_default()),
        None => name.to_string(),
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

    use super::*;

    #[test]
    fn tool_end_sends_the_diff() {
        let diff = crate::extension::Diff {
            path: "/w/f.txt".into(),
            old: "a\n".into(),
            new: "b\n".into(),
        };
        let update = to_update(
            AgentEvent::ToolEnd {
                id: "t1",
                output: "ok",
                is_error: false,
                diff: Some(&diff),
            },
            0,
            None,
        );
        let wire = serde_json::to_value(update).unwrap();
        assert_eq!(
            wire["content"][0],
            serde_json::json!({"type": "diff", "path": "/w/f.txt", "oldText": "a\n", "newText": "b\n"})
        );
        assert_eq!(wire["content"][1]["content"]["text"], "ok");
    }

    #[test]
    fn commands_are_fenced_as_sh() {
        assert_eq!(command_block("ls -la"), "```sh\nls -la\n```");
        assert_eq!(command_block("echo ```"), "````sh\necho ```\n````");

        let input = serde_json::json!({"command": "ls"});
        let start = AgentEvent::ToolStart {
            id: "t1",
            name: "Bash",
            input: &input,
        };
        let wire = serde_json::to_value(to_update(start, 0, None)).unwrap();
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");

        let end = AgentEvent::ToolEnd {
            id: "t1",
            output: "out",
            is_error: false,
            diff: None,
        };
        let wire = serde_json::to_value(to_update(end, 1, Some("ls"))).unwrap();
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");
        assert_eq!(wire["content"][1]["content"]["text"], "out");
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
