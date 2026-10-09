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
use rho::provider::{Content, Event, Provider, Request, Role, StopReason, Usage};
use serde_json::json;

fn tool_then_answer() -> Vec<Vec<Event>> {
    let thinking = Content::Thinking {
        thinking: "need a tool".into(),
        signature: "sig".into(),
    };
    vec![
        vec![
            Event::ThinkingDelta("need a tool".into()),
            Event::Block(thinking),
            text("Let me look."),
            Event::Block(Content::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
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
    let agent = Agent::new(provider.clone(), "m", rho::tools::builtin());
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
    assert!(
        log.contains(&r#"Thinking("need a tool")"#.to_string()),
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
            tool_use_id: "t1".into(),
            content: "from-tool\n".into(),
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
        "m",
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
            id: "x".into(),
            name: "Nope".into(),
            input: json!({}),
        })],
        vec![text("ok")],
    ]);
    let agent = Agent::new(provider, "m", vec![]);
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
        fn stream(&self, _: Request) -> BoxStream<'static, Result<Event>> {
            stream::iter([Err(anyhow::anyhow!("boom"))]).boxed()
        }
    }
    let agent = Agent::new(Arc::new(Failing), "m", vec![]);
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
    let agent = Arc::new(Agent::new(provider, "m", rho::tools::builtin()));

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
                    assert_eq!(thoughts, "need a tool");
                    // used = whole prompt + output; size = provider's default window.
                    assert_eq!(used, vec![(1015, 200_000), (1037, 200_000)]);
                    assert_eq!(
                        tool_statuses,
                        vec![ToolCallStatus::InProgress, ToolCallStatus::Completed]
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
        fn stream(&self, _: Request) -> BoxStream<'static, Result<Event>> {
            stream::iter([Ok(text("thinking..."))])
                .chain(stream::pending())
                .boxed()
        }
    }
    let agent = Arc::new(Agent::new(Arc::new(Hangs), "m", vec![]));

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
                    loop {
                        if let SessionMessage::StopReason(r) = session.read_update().await? {
                            assert_eq!(r, StopReasonAcp::Cancelled);
                            return Ok(());
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
