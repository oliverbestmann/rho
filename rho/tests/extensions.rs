use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, InitializeRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, ToolCallContent,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{Client, Responder, SessionMessage};
use anyhow::Result;
use rho::agent::Agent;
use rho::extension::{
    Context, Decision, Extension, NoteType, Profile, PromptSetup, RunEnd, SessionSetup, Settle,
    ToolCall, ToolOutput, TurnEnd,
};
use rho::provider::{
    Content, Event, Message, ModelSpec, ProviderName, Registry, Request, Role, StopReason,
    ToolCallId, ToolName, ToolSpec, Usage,
};
use rho::store::SessionId;
use rho::tools::Tool;
use serde_json::{Value, json};

mod common;
use common::{Scripted, text};

fn tool_use(id: &str, name: &str, input: Value) -> Event {
    Event::Block(Content::ToolUse {
        id: ToolCallId::new(id),
        name: ToolName::new(name),
        input,
    })
}

fn last_user(request: &Request) -> &Message {
    let m = request.messages.last().unwrap();
    assert_eq!(m.role, Role::User);
    m
}

/// Echoes its `text` input.
struct Echo;

impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::new("Echo"),
            description: "echo".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Echo instead of echo in Bash."]
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        Ok(input["text"].as_str().unwrap_or_default().into())
    }
}

/// Exercises every hook.
#[derive(Default)]
struct Recorder {
    turns: AtomicUsize,
    settles: AtomicUsize,
    ended: Arc<AtomicBool>,
    tools_called: Mutex<Vec<String>>,
}

impl Extension for Recorder {
    async fn session_start(&self, s: &mut SessionSetup) {
        s.tools.push(Arc::new(Echo));
        s.system.push_str(" +ext");
    }

    fn session_end(&self, _ctx: &Context) {
        self.ended.store(true, Ordering::SeqCst);
    }

    async fn before_prompt(&self, _ctx: &Context, p: &mut PromptSetup) {
        p.content.push(Content::text("[context]"));
    }

    async fn before_request(&self, _ctx: &Context, r: &mut Request) {
        r.max_tokens = 123;
    }

    async fn tool_call(&self, _ctx: &Context, call: &mut ToolCall) -> Decision {
        match call.name.as_str() {
            "Bash" => Decision::Block("no bash".into()),
            _ => {
                call.input = json!({"text": "changed"});
                Decision::Allow
            }
        }
    }

    async fn tool_result(&self, _ctx: &Context, call: &ToolCall, out: &mut ToolOutput) {
        if call.name.as_str() == "Echo" {
            out.output.push_str(" (checked)");
        }
    }

    async fn turn_end(&self, _ctx: &Context, _turn: &TurnEnd<'_>) {
        self.turns.fetch_add(1, Ordering::SeqCst);
    }

    async fn before_settle(&self, _ctx: &Context, s: &Settle<'_>) -> Option<String> {
        *self.tools_called.lock().unwrap() =
            s.tools_called.iter().map(ToolName::to_string).collect();
        (self.settles.fetch_add(1, Ordering::SeqCst) == 0).then(|| "please continue".into())
    }
}

#[tokio::test]
async fn hooks_register_tools_block_rewrite_and_continue() {
    let provider = Scripted::new(vec![
        vec![
            tool_use("b", "Bash", json!({"command": "rm -rf /"})),
            tool_use("e", "Echo", json!({"text": "original"})),
        ],
        vec![text("done")],
        vec![text("continued")],
    ]);
    let recorder = Arc::new(Recorder::default());
    let ended = recorder.ended.clone();
    let mut agent = Agent {
        extensions: rho::builtin(),
        ..Agent::new(provider.clone(), ModelSpec::new("test", "m"))
    };
    agent.extensions.push(recorder.clone());

    let (mut session, _log) = common::session(&agent, std::path::Path::new(".")).await;
    let stop = agent
        .run(&mut session, vec![Content::text("go")])
        .await
        .unwrap();
    assert_eq!(stop, StopReason::EndTurn);

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    // session_start: extra tool and system prompt; before_request: max_tokens.
    assert!(requests[0].tools.iter().any(|t| t.name.as_str() == "Echo"));
    assert!(requests[0].system.ends_with(" +ext"));
    // The prompt is built after session_start: it lists the tool and its rule.
    assert!(requests[0].system.contains("- Echo: echo\n"));
    assert!(
        requests[0]
            .system
            .contains("- Use Echo instead of echo in Bash.\n")
    );
    assert_eq!(requests[0].max_tokens, 123);
    // before_prompt added content after the prompt.
    assert_eq!(
        last_user(&requests[0]).content,
        vec![Content::text("go"), Content::text("[context]")]
    );
    // tool_call blocked Bash and rewrote Echo's input; tool_result appended.
    assert_eq!(
        last_user(&requests[1]).content,
        vec![
            Content::ToolResult {
                tool_use_id: ToolCallId::new("b"),
                content: "no bash".into(),
                is_error: true,
                images: vec![],
            },
            Content::ToolResult {
                tool_use_id: ToolCallId::new("e"),
                content: "changed (checked)".into(),
                is_error: false,
                images: vec![],
            },
        ]
    );
    // before_settle continued the run once.
    assert_eq!(
        last_user(&requests[2]).content,
        vec![Content::text("please continue")]
    );
    assert_eq!(recorder.turns.load(Ordering::SeqCst), 3);
    assert_eq!(recorder.settles.load(Ordering::SeqCst), 2);
    assert_eq!(*recorder.tools_called.lock().unwrap(), vec!["Bash", "Echo"]);

    assert!(!ended.load(Ordering::SeqCst));
    drop(session);
    assert!(ended.load(Ordering::SeqCst), "session_end runs on drop");
}

/// Records `run_end`: the stop and the context before and after.
#[derive(Default)]
struct Runs(Mutex<Vec<String>>);

impl Extension for Runs {
    async fn run_end(&self, ctx: &Context, run: &RunEnd) {
        let now = ctx.session_info();
        self.0.lock().unwrap().push(format!(
            "{:?} {} -> {} after {} requests",
            run.stop,
            run.before.context_used(),
            now.context_used(),
            now.requests
        ));
    }
}

#[tokio::test]
async fn run_end_sees_the_session_info_before_and_after() {
    let usage = |input| {
        Event::Usage(Usage {
            input,
            ..Default::default()
        })
    };
    let provider = Scripted::new(vec![
        vec![tool_use("e", "Echo", json!({})), usage(10)],
        vec![text("done"), usage(20)],
        vec![text("again"), usage(30)],
    ]);
    let runs = Arc::new(Runs::default());
    let mut agent = Agent {
        extensions: vec![Arc::new(rho::tools::ToolExtension::from(Echo))],
        ..Agent::new(provider, ModelSpec::new("test", "m"))
    };
    agent.extensions.push(runs.clone());
    let (mut session, _log) = common::session(&agent, std::path::Path::new(".")).await;
    let info = session.ctx.session_info();
    assert_eq!(
        (info.model, info.context_window),
        (Some(ModelSpec::new("test", "m")), 200_000)
    );
    assert_eq!(info.profile, Profile::WRITE);

    for prompt in ["a", "b"] {
        agent
            .run(&mut session, vec![Content::text(prompt)])
            .await
            .unwrap();
    }
    // Once per run, not per request.
    assert_eq!(
        *runs.0.lock().unwrap(),
        [
            "Some(EndTurn) 0 -> 20 after 2 requests",
            "Some(EndTurn) 20 -> 30 after 3 requests"
        ]
    );
}

/// Sends one steering message right away and a triggering one later, like
/// background-task.ts does when its command finishes.
struct Later;

impl Tool for Later {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::new("Later"),
            description: "finishes in the background".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn run(&self, _input: Value, ctx: &Context) -> Result<ToolOutput> {
        ctx.send("steer msg", false);
        let ctx = ctx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            ctx.notify(NoteType::Info, "background", "finished", false);
            ctx.send("bg done", true);
        });
        Ok("started".into())
    }
}

#[tokio::test]
async fn async_messages_steer_queue_and_trigger_turns() {
    let provider = Scripted::new(vec![
        vec![tool_use("l", "Later", json!({}))],
        vec![text("waiting")],
        vec![text("got it")],
        vec![text("second prompt")],
    ]);
    let agent = Agent {
        extensions: vec![Arc::new(rho::tools::ToolExtension::from(Later))],
        ..Agent::new(provider.clone(), ModelSpec::new("test", "m"))
    };
    let (mut session, log) = common::session(&agent, std::path::Path::new(".")).await;

    agent
        .run(&mut session, vec![Content::text("start")])
        .await
        .unwrap();
    // Sent during the run: delivered with the tool results.
    assert_eq!(
        last_user(&provider.requests.lock().unwrap()[1]).content[1],
        Content::text("steer msg")
    );

    // While idle, the triggered message starts a run (what the ACP watcher does).
    let ctx = session.ctx.clone();
    let stop = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            ctx.wait_inbox().await;
            if let Some(result) = agent.run_triggered(&mut session).await {
                return result;
            }
        }
    })
    .await
    .expect("no triggered run")
    .unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(
        last_user(&provider.requests.lock().unwrap()[2]).content,
        vec![Content::text("bg done")]
    );
    assert!(
        log.lines()
            .iter()
            .any(|l| l.contains("Notice") && l.contains("finished")),
        "{:?}",
        log.lines()
    );

    // Without trigger it waits for the next prompt and goes first.
    session.ctx.send("note", false);
    assert!(agent.run_triggered(&mut session).await.is_none());
    agent
        .run(&mut session, vec![Content::text("next")])
        .await
        .unwrap();
    assert_eq!(
        last_user(&provider.requests.lock().unwrap()[3]).content,
        vec![Content::text("note"), Content::text("next")]
    );
}

#[tokio::test]
async fn ctx_complete_calls_any_registered_model() {
    let provider = Scripted::new(vec![vec![text("Fix "), text("the build")]]);
    let mut registry = Registry::default();
    registry.register(ProviderName::new("fake"), provider.clone());
    let ctx = Context::new(
        SessionId::new("s"),
        ".",
        Arc::new(rho::extension::NoFrontend),
        registry,
    );

    let title = ctx
        .complete(
            &ModelSpec::new("fake", "small"),
            "Write a title.",
            "the build is broken",
        )
        .await
        .unwrap();
    assert_eq!(title, "Fix the build");
    let req = &provider.requests.lock().unwrap()[0];
    assert_eq!(req.model.as_str(), "small");
    assert_eq!(req.system, "Write a title.");
    assert!(req.tools.is_empty());
}

/// Asks before every tool call, like git-gate.ts; with `own_card` on a card
/// of its own instead of the tool call, like RequestFeature.
struct Confirmer {
    own_card: bool,
}

impl Extension for Confirmer {
    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        let id = (!self.own_card).then_some(&call.id);
        if ctx.confirm(id, "Run it?\nIt echoes hi.").await {
            Decision::Allow
        } else {
            Decision::Block("denied by user".into())
        }
    }
}

fn confirm_agent(own_card: bool) -> (Arc<Scripted>, Agent) {
    let provider = Scripted::new(vec![
        vec![tool_use(
            "t1",
            "Bash",
            json!({"command": "echo hi", "description": "Say hi"}),
        )],
        vec![text("ok")],
    ]);
    let mut agent = Agent {
        extensions: rho::builtin(),
        ..Agent::new(provider.clone(), ModelSpec::new("test", "m"))
    };
    agent.extensions.push(Arc::new(Confirmer { own_card }));
    (provider, agent)
}

fn first_result(provider: &Scripted) -> Content {
    last_user(&provider.requests.lock().unwrap()[1]).content[0].clone()
}

#[tokio::test]
async fn confirm_without_ui_denies() {
    let (provider, agent) = confirm_agent(false);
    let (mut session, _log) = common::session(&agent, std::path::Path::new(".")).await;
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    assert!(
        matches!(first_result(&provider), Content::ToolResult { is_error: true, content, .. } if content == "denied by user")
    );
}

/// Collects agent text until it contains `until`.
async fn read_text<'a>(
    session: &mut agent_client_protocol::ActiveSession<'a, agent_client_protocol::Agent>,
    until: &str,
) -> Result<String, agent_client_protocol::Error> {
    let mut text = String::new();
    while !text.contains(until) {
        if let SessionMessage::SessionMessage(dispatch) = session.read_update().await? {
            MatchDispatch::new(dispatch)
                .if_notification(async |n: SessionNotification| {
                    if let SessionUpdate::AgentMessageChunk(ContentChunk {
                        content: ContentBlock::Text(t),
                        ..
                    }) = n.update
                    {
                        text.push_str(&t.text);
                    }
                    Ok(())
                })
                .await
                .otherwise_ignore()?;
        }
    }
    Ok(text)
}

#[tokio::test]
async fn acp_confirm_uses_request_permission() {
    // A tool call keeps its content; our own card gets the rest of the title.
    acp_confirm(false, "t1", "Run it?\nIt echoes hi.", None).await;
    acp_confirm(true, "confirm-", "Run it?", Some("It echoes hi.")).await;
}

async fn acp_confirm(
    own_card: bool,
    call_id: &'static str,
    title: &'static str,
    content: Option<&'static str>,
) {
    let (provider, agent) = confirm_agent(own_card);
    let asked = Arc::new(AtomicBool::new(false));
    let asked_in_client = asked.clone();

    let client = Client
        .builder()
        .on_receive_request(
            async move |req: RequestPermissionRequest,
                        responder: Responder<RequestPermissionResponse>,
                        _cx| {
                // Own cards get a fresh `confirm-<hex>` id each.
                assert!(req.tool_call.tool_call_id.0.starts_with(call_id));
                let fields = &req.tool_call.fields;
                assert_eq!(fields.title.as_deref(), Some(title));
                let text = fields.content.as_ref().map(|c| match &c[..] {
                    [ToolCallContent::Content(c)] => match &c.content {
                        ContentBlock::Text(t) => t.text.clone(),
                        other => panic!("{other:?}"),
                    },
                    other => panic!("{other:?}"),
                });
                assert_eq!(text.as_deref(), content);
                asked_in_client.store(true, Ordering::SeqCst);
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(rho::acp::agent(Arc::new(agent)), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.build_session_cwd()?
                .block_task()
                .run_until(async |mut session| {
                    session.send_prompt("hi")?;
                    read_text(&mut session, "ok").await?;
                    Ok(())
                })
                .await
        });
    tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .expect("timed out")
        .expect("acp failed");

    assert!(asked.load(Ordering::SeqCst));
    assert!(
        matches!(first_result(&provider), Content::ToolResult { is_error: false, content, .. } if content == "hi\n")
    );
}

#[tokio::test]
async fn acp_background_message_starts_a_turn_without_prompt() {
    let provider = Scripted::new(vec![
        vec![tool_use("l", "Later", json!({}))],
        vec![text("waiting")],
        vec![text("got it")],
    ]);
    let agent = Agent {
        extensions: vec![Arc::new(rho::tools::ToolExtension::from(Later))],
        ..Agent::new(provider, ModelSpec::new("test", "m"))
    };

    let client = Client
        .builder()
        .connect_with(rho::acp::agent(Arc::new(agent)), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.build_session_cwd()?
                .block_task()
                .run_until(async |mut session| {
                    session.send_prompt("start")?;
                    assert_eq!(session.read_to_string().await?, "waiting");
                    // No prompt is open now; the background message still runs a turn.
                    read_text(&mut session, "got it").await?;
                    Ok(())
                })
                .await
        });
    tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .expect("timed out")
        .expect("acp failed");
}

/// Calls `ctx.stop` from one hook.
struct Stopper(&'static str);

impl Extension for Stopper {
    async fn before_prompt(&self, ctx: &Context, _p: &mut PromptSetup) {
        if self.0 == "prompt" {
            ctx.stop("swallowed");
        }
    }

    async fn before_request(&self, ctx: &Context, _r: &mut Request) {
        if self.0 == "request" {
            ctx.stop("not sending");
        }
    }

    async fn tool_call(&self, ctx: &Context, _call: &mut ToolCall) -> Decision {
        if self.0 != "tool" {
            return Decision::Allow;
        }
        // pi's `{ block: true, terminate: true }`.
        ctx.stop("terminated");
        Decision::Block("blocked".into())
    }

    async fn before_settle(&self, ctx: &Context, _s: &Settle<'_>) -> Option<String> {
        if self.0 != "settle" {
            return None;
        }
        ctx.stop("enough");
        Some("this continuation is dropped".into())
    }
}

async fn run_with_stopper(
    at: &'static str,
    responses: Vec<Vec<Event>>,
) -> (
    StopReason,
    Arc<Scripted>,
    rho::agent::Session,
    Arc<common::Log>,
) {
    let provider = Scripted::new(responses);
    let mut agent = Agent {
        extensions: rho::builtin(),
        ..Agent::new(provider.clone(), ModelSpec::new("test", "m"))
    };
    agent.extensions.push(Arc::new(Stopper(at)));
    let (mut session, log) = common::session(&agent, std::path::Path::new(".")).await;
    let stop = agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    (stop, provider, session, log)
}

#[tokio::test]
async fn stop_in_before_prompt_drops_the_prompt() {
    let (stop, provider, session, log) = run_with_stopper("prompt", vec![]).await;
    assert_eq!(stop, StopReason::Stopped);
    assert!(provider.requests.lock().unwrap().is_empty());
    // Only the "Run stopped" notice is saved.
    assert!(rho::provider::for_model(&session.history).is_empty());
    assert!(
        log.lines()
            .iter()
            .any(|l| l.contains("Run stopped") && l.contains("swallowed"))
    );
}

#[tokio::test]
async fn stop_in_before_request_cancels_the_request() {
    // No scripted responses: a request would panic.
    let (stop, provider, session, _log) = run_with_stopper("request", vec![]).await;
    assert_eq!(stop, StopReason::Stopped);
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(session.history.len(), 1, "prompt kept for the next run");
}

#[tokio::test]
async fn stop_in_tool_call_skips_the_rest_and_keeps_history_valid() {
    let (stop, provider, session, _log) = run_with_stopper(
        "tool",
        vec![vec![
            tool_use("a", "Bash", json!({"command": "touch should-not-exist"})),
            tool_use("b", "Bash", json!({"command": "touch should-not-exist"})),
        ]],
    )
    .await;
    assert_eq!(stop, StopReason::Stopped);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(!std::path::Path::new("should-not-exist").exists());
    // Every tool_use has a result, so the next request would be valid.
    let history = rho::provider::for_model(&session.history);
    let last = history.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(
        last.content,
        vec![
            Content::ToolResult {
                tool_use_id: ToolCallId::new("a"),
                content: "blocked".into(),
                is_error: true,
                images: vec![],
            },
            Content::ToolResult {
                tool_use_id: ToolCallId::new("b"),
                content: "skipped: run stopped".into(),
                is_error: true,
                images: vec![],
            },
        ]
    );
}

#[tokio::test]
async fn stop_in_before_settle_prevents_continuation() {
    let (stop, provider, _session, _log) =
        run_with_stopper("settle", vec![vec![text("done")]]).await;
    assert_eq!(stop, StopReason::Stopped);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stop_while_idle_does_not_affect_the_next_run() {
    let provider = Scripted::new(vec![vec![text("ok")]]);
    let agent = Agent::new(provider, ModelSpec::new("test", "m"));
    let (mut session, _log) = common::session(&agent, std::path::Path::new(".")).await;
    session.ctx.stop("stale");
    let stop = agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    assert_eq!(stop, StopReason::EndTurn);
}

#[tokio::test]
async fn grant_commands_widen_the_sandbox_and_tell_the_model() {
    let cwd = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let provider = Scripted::new(vec![vec![text("ok")]]);
    let mut agent = Agent {
        extensions: rho::builtin(),
        ..Agent::new(provider.clone(), ModelSpec::new("test", "m"))
    };
    agent.extensions.push(Arc::new(rho::sandbox::GrantCommands));
    let (mut session, log) = common::session(&agent, cwd.path()).await;
    let names: Vec<_> = session.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["grant-read", "grant-write", "sandbox"]);

    let path = other.path().canonicalize().unwrap();
    let grant = agent.slash_command(&mut session, "grant-write", path.to_str().unwrap());
    assert!(grant.await.is_some_and(|r| r.is_ok()));
    assert!(
        agent
            .slash_command(&mut session, "nope", "")
            .await
            .is_none()
    );
    let rule = rho::sandbox::Rule {
        path: path.clone(),
        access: rho::sandbox::Access::ReadWrite,
    };
    assert!(session.ctx.sandbox().unwrap().rules.contains(&rule));
    assert!(log.lines().iter().any(|l| l.contains("Granted read-write")));

    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    let request = &provider.requests.lock().unwrap()[0];
    let sent = format!("{:?}", last_user(request).content);
    assert!(sent.contains("granted read-write access"), "{sent}");
}
