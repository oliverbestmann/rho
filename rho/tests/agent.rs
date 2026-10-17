use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, SessionNotification,
    SessionUpdate, StopReason as StopReasonAcp, ToolCallStatus,
};
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{Client, SessionMessage};
use anyhow::Result;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use rho::agent::Agent;

mod common;
use common::{Scripted, text};
use rho::provider::{
    Content, Event, ModelSpec, Provider, Request, Role, StopReason, ToolCallId, ToolName, Usage,
};
use serde_json::json;

fn tool_then_answer() -> Vec<Vec<Event>> {
    let thinking = Content::Thinking {
        thinking: "need a tool".into(),
        signature: "sig".into(),
    };
    vec![
        vec![
            // Leading blank thinking is not shown, later whitespace is.
            Event::ThinkingDelta("".into()),
            Event::ThinkingDelta("\n\n".into()),
            Event::ThinkingDelta("need a tool".into()),
            Event::ThinkingDelta(" ".into()),
            Event::Block(thinking),
            text("Let me look."),
            Event::Block(Content::ToolUse {
                id: ToolCallId::new("t1"),
                name: ToolName::new("Bash"),
                input: json!({"command": "echo from-tool"}),
            }),
            Event::Usage(Usage {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 1000,
            }),
            Event::Done(StopReason::ToolUse),
        ],
        vec![
            text("The tool said "),
            text("from-tool."),
            Event::Usage(Usage {
                input: 20,
                output: 7,
                cache_read: 1010,
                cache_write: 0,
            }),
            Event::Done(StopReason::EndTurn),
        ],
    ]
}

#[tokio::test]
async fn agent_runs_tools_and_feeds_results_back() {
    let provider = Scripted::new(tool_then_answer());
    let agent = Agent::new(
        provider.clone(),
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    );
    let dir = tempfile::tempdir().unwrap();

    let (mut session, log) = common::session(&agent, dir.path()).await;
    let stop = agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    let log = log.lines();
    let history = &session.history;
    assert_eq!(stop, StopReason::EndTurn);
    assert!(log.iter().any(|l| l.contains("ToolStart")), "{log:?}");
    let thoughts: Vec<_> = log.iter().filter(|l| l.starts_with("Thinking")).collect();
    assert_eq!(
        thoughts,
        [r#"Thinking("need a tool")"#, r#"Thinking(" ")"#],
        "{log:?}"
    );
    assert_eq!(log.iter().filter(|l| l.starts_with("Usage")).count(), 2);

    // Second request carries the assistant tool call and its result.
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 6);
    let msgs = &requests[1].messages;
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1].role, Role::Assistant);
    // Thinking (with signature) comes first, as the API requires.
    assert_eq!(
        msgs[1].content[0],
        Content::Thinking {
            thinking: "need a tool".into(),
            signature: "sig".into()
        }
    );
    assert_eq!(requests[0].thinking, rho::provider::Thinking::High);
    assert_eq!(
        msgs[2].content,
        vec![Content::ToolResult {
            tool_use_id: ToolCallId::new("t1"),
            // The call has no `description`: it runs, with a reminder.
            content: "from-tool\n\n\n<system-reminder>\nThis call left out the required argument(s) `description`. \
                Pass every required argument in every call.\n</system-reminder>"
                .into(),
            is_error: false,
            images: vec![],
        }]
    );

    // Final history: user, assistant(tool), user(result), assistant(text).
    assert_eq!(history.len(), 4);
    assert_eq!(
        history[3].content,
        vec![Content::text("The tool said from-tool.")]
    );
}

#[tokio::test]
async fn usage_report_shows_cache_details() {
    let agent = Agent::new(
        Scripted::new(tool_then_answer()),
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    );
    let dir = tempfile::tempdir().unwrap();
    let (mut session, _log) = common::session(&agent, dir.path()).await;
    assert!(session.usage_report().contains("No requests yet."));

    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    let report = session.usage_report();
    for line in [
        "| Input (uncached) | 20 | 30 |",
        "| Cache read | 1,010 | 1,010 |",
        "| Cache write | 0 | 1,000 |",
        "| **Input total** | 1,030 | 2,040 |",
        "| Output | 7 | 12 |",
        "| Cache hit rate | 98% | 50% |",
        "- Context: 1,037 / 200,000 (0.5%)",
        "- Cache: warm, expires in ~4m",
    ] {
        assert!(report.contains(line), "missing {line:?} in:\n{report}");
    }
    // The scripted model has no price.
    assert!(!report.contains("Cost"), "{report}");
}

#[tokio::test]
async fn unknown_tool_is_reported_as_error() {
    let provider = Scripted::new(vec![
        vec![Event::Block(Content::ToolUse {
            id: ToolCallId::new("x"),
            name: ToolName::new("Nope"),
            input: json!({}),
        })],
        vec![text("ok")],
    ]);
    let agent = Agent::new(provider, ModelSpec::new("test", "m"), vec![]);
    let (mut session, log) = common::session(&agent, std::path::Path::new(".")).await;
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    let errors: Vec<String> = log
        .lines()
        .into_iter()
        .filter(|l| l.starts_with("ToolEnd") && l.contains("is_error: true"))
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("unknown tool Nope"), "{errors:?}");
}

#[tokio::test]
async fn failed_turn_keeps_history_valid() {
    struct Failing;
    impl Provider for Failing {
        fn stream(
            &self,
            _: Request,
            _: &dyn Fn(&mut serde_json::Value),
        ) -> BoxStream<'static, Result<Event>> {
            stream::iter([Err(anyhow::anyhow!("boom"))]).boxed()
        }
    }
    let agent = Agent::new(Arc::new(Failing), ModelSpec::new("test", "m"), vec![]);
    let (mut session, _log) = common::session(&agent, std::path::Path::new(".")).await;
    for prompt in ["a", "b"] {
        let result = agent.run(&mut session, vec![Content::text(prompt)]).await;
        assert!(result.is_err());
    }
    let history = &session.history;
    // Consecutive prompts merge into one user message instead of two.
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].content,
        vec![Content::text("a"), Content::text("b")]
    );
}

#[tokio::test]
async fn acp_session_streams_text_and_tool_calls() {
    let provider = Scripted::new(tool_then_answer());
    let agent = Arc::new(Agent::new(
        provider,
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    ));

    let client = Client
        .builder()
        .connect_with(rho::acp::agent(agent), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.build_session_cwd()?
                .block_task()
                .run_until(async |mut session| {
                    session.send_prompt("hi")?;
                    let mut text = String::new();
                    let mut tool_statuses = Vec::new();
                    let mut thoughts = String::new();
                    let mut used = Vec::new();
                    loop {
                        match session.read_update().await? {
                            SessionMessage::SessionMessage(dispatch) => {
                                MatchDispatch::new(dispatch)
                                    .if_notification(async |n: SessionNotification| {
                                        match n.update {
                                            SessionUpdate::AgentMessageChunk(ContentChunk {
                                                content: ContentBlock::Text(t),
                                                ..
                                            }) => text.push_str(&t.text),
                                            SessionUpdate::AgentThoughtChunk(ContentChunk {
                                                content: ContentBlock::Text(t),
                                                ..
                                            }) => thoughts.push_str(&t.text),
                                            SessionUpdate::UsageUpdate(u) => {
                                                used.push((u.used, u.size))
                                            }
                                            SessionUpdate::ToolCall(t) => {
                                                tool_statuses.push(t.status)
                                            }
                                            SessionUpdate::ToolCallUpdate(u) => {
                                                tool_statuses.extend(u.fields.status)
                                            }
                                            _ => {}
                                        }
                                        Ok(())
                                    })
                                    .await
                                    .otherwise_ignore()?
                            }
                            SessionMessage::StopReason(_) => break,
                            _ => {}
                        }
                    }
                    assert_eq!(text, "Let me look.The tool said from-tool.");
                    assert_eq!(thoughts, "need a tool ");
                    // used = whole prompt + output; size = provider's default window.
                    assert_eq!(used, vec![(1015, 200_000), (1037, 200_000)]);
                    // The tool, then the turn's usage notice.
                    assert_eq!(
                        tool_statuses,
                        vec![
                            ToolCallStatus::InProgress,
                            ToolCallStatus::Completed,
                            ToolCallStatus::Completed
                        ]
                    );
                    Ok(())
                })
                .await
        });

    tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .expect("timed out")
        .expect("acp failed");
}

#[tokio::test]
async fn acp_cancel_stops_a_running_prompt() {
    struct Hangs;
    impl Provider for Hangs {
        fn stream(
            &self,
            _: Request,
            _: &dyn Fn(&mut serde_json::Value),
        ) -> BoxStream<'static, Result<Event>> {
            stream::iter([Ok(text("thinking..."))])
                .chain(stream::pending())
                .boxed()
        }
    }
    let agent = Arc::new(Agent::new(
        Arc::new(Hangs),
        ModelSpec::new("test", "m"),
        vec![],
    ));

    let client = Client
        .builder()
        .connect_with(rho::acp::agent(agent), async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.build_session_cwd()?
                .block_task()
                .run_until(async |mut session| {
                    session.send_prompt("hi")?;
                    // First update proves the turn is running; then cancel it.
                    let _ = session.read_update().await?;
                    cx.send_notification(CancelNotification::new(session.session_id().clone()))?;
                    let mut titles = vec![];
                    loop {
                        match session.read_update().await? {
                            SessionMessage::SessionMessage(dispatch) => {
                                MatchDispatch::new(dispatch)
                                    .if_notification(async |n: SessionNotification| {
                                        if let SessionUpdate::ToolCall(t) = n.update {
                                            titles.push(t.title);
                                        }
                                        Ok(())
                                    })
                                    .await
                                    .otherwise_ignore()?
                            }
                            SessionMessage::StopReason(r) => {
                                assert_eq!(r, StopReasonAcp::Cancelled);
                                assert_eq!(titles, vec!["Interrupted by the user"]);
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                })
                .await
        });

    tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .expect("timed out")
        .expect("acp failed");
}

#[tokio::test]
async fn cancelled_tool_call_is_ended() {
    let provider = Scripted::new(vec![vec![
        Event::Block(Content::ToolUse {
            id: ToolCallId::new("t1"),
            name: ToolName::new("Bash"),
            input: json!({"command": "sleep 30"}),
        }),
        Event::Done(StopReason::ToolUse),
    ]]);
    let agent = Agent::new(provider, ModelSpec::new("test", "m"), rho::tools::builtin());
    let dir = tempfile::tempdir().unwrap();
    let (mut session, log) = common::session(&agent, dir.path()).await;
    // Dropping the run future is how ACP cancels it.
    let run = agent.run(&mut session, vec![Content::text("hi")]);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), run)
            .await
            .is_err()
    );
    let log = log.lines();
    let end = log.iter().find(|l| l.starts_with("ToolEnd"));
    assert!(
        end.is_some_and(
            |l| l.contains(r#"id: ToolCallId("t1"), output: "cancelled", is_error: true"#)
        ),
        "{log:?}"
    );
}

/// Returns an image the API would reject.
struct BadImage;

impl rho::tools::Tool for BadImage {
    fn spec(&self) -> rho::provider::ToolSpec {
        rho::provider::ToolSpec {
            name: ToolName::new("BadImage"),
            description: "returns a TIFF".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn run(
        &self,
        _input: serde_json::Value,
        _ctx: &rho::extension::Context,
    ) -> Result<rho::extension::ToolOutput> {
        Ok(rho::extension::ToolOutput {
            output: "here".into(),
            images: vec![rho::provider::ImageSource::Base64 {
                media_type: "image/tiff".into(),
                data: "AAAA".into(),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn tool_images_the_api_rejects_become_errors() {
    let call = Event::Block(Content::ToolUse {
        id: ToolCallId::new("t1"),
        name: ToolName::new("BadImage"),
        input: json!({}),
    });
    let provider = Scripted::new(vec![
        vec![call, Event::Done(StopReason::ToolUse)],
        vec![text("ok"), Event::Done(StopReason::EndTurn)],
    ]);
    let agent = Agent::new(
        provider,
        ModelSpec::new("test", "m"),
        vec![Arc::new(BadImage)],
    );
    let dir = tempfile::tempdir().unwrap();
    let (mut session, _) = common::session(&agent, dir.path()).await;
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    let Content::ToolResult {
        content,
        is_error,
        images,
        ..
    } = &session.history[2].content[0]
    else {
        panic!("no tool result: {:?}", session.history);
    };
    assert!(*is_error && images.is_empty(), "{content}");
    assert!(content.contains("unsupported image type"), "{content}");
}

#[tokio::test]
async fn before_send_changes_the_body_but_not_the_history() {
    struct Mask;
    impl rho::extension::Extension for Mask {
        fn before_send(&self, _: &rho::extension::Context, body: &mut serde_json::Value) {
            body["messages"][0]["content"][0]["text"] = json!("masked");
        }
    }
    let provider = Scripted::new(vec![vec![text("ok"), Event::Done(StopReason::EndTurn)]]);
    let mut agent = Agent::new(provider.clone(), ModelSpec::new("test", "m"), vec![]);
    agent.extensions = vec![Arc::new(Mask)];
    let dir = tempfile::tempdir().unwrap();
    let (mut session, _) = common::session(&agent, dir.path()).await;
    agent
        .run(&mut session, vec![Content::text("secret")])
        .await
        .unwrap();
    let bodies = provider.bodies.lock().unwrap();
    assert_eq!(bodies[0]["messages"][0]["content"][0]["text"], "masked");
    assert_eq!(session.history[0].content[0], Content::text("secret"));
}
