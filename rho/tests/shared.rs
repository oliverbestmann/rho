//! Shared sessions: connections on one `Hub` (like `rho daemon`) see each
//! other's sessions live, and a turn outlives the connection that started it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, ListSessionsRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigKind, SessionConfigOption, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Responder};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use rho::acp::Hub;
use rho::agent::Agent;
use rho::extension::{Context, Decision, Extension, ToolCall as ExtToolCall};
use rho::provider::{
    Content, Event, ModelSpec, Provider, Request, StopReason, ToolCallId, ToolName,
};
use tokio::sync::{Notify, oneshot};

mod common;
use common::{Scripted, text};

type Updates = Arc<Mutex<Vec<SessionUpdate>>>;

fn agent(provider: Arc<dyn Provider>, db: &std::path::Path) -> Arc<Agent> {
    let mut agent = Agent::new(provider, ModelSpec::new("test", "m"), vec![]);
    let db = db.to_path_buf();
    agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    Arc::new(agent)
}

fn answer(t: &str) -> Vec<Event> {
    vec![text(t), Event::Done(StopReason::EndTurn)]
}

fn prompt(sid: &SessionId, text: &str) -> PromptRequest {
    PromptRequest::new(
        sid.clone(),
        vec![ContentBlock::Text(TextContent::new(text))],
    )
}

/// "user: .." and "agent: .." for the text chunks.
fn chunks(updates: &Updates) -> Vec<String> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::UserMessageChunk(ContentChunk {
                content: ContentBlock::Text(t),
                ..
            }) => Some(format!("user: {}", t.text)),
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(t),
                ..
            }) => Some(format!("agent: {}", t.text)),
            _ => None,
        })
        .collect()
}

/// The `_meta.rho.running` values of the `session_info_update`s.
fn running(updates: &Updates) -> Vec<bool> {
    updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::SessionInfoUpdate(i) => i.meta.as_ref()?["rho"]["running"].as_bool(),
            _ => None,
        })
        .collect()
}

macro_rules! client {
    ($updates:expr) => {{
        let record = $updates.clone();
        Client.builder().on_receive_notification(
            async move |n: SessionNotification, _cx| {
                record.lock().unwrap().push(n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
    }};
}

#[tokio::test]
async fn a_second_connection_follows_and_continues_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![answer("One."), answer("Two."), answer("Three.")]);
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    let (sid_tx, sid_rx) = oneshot::channel();
    let (go_tx, go_rx) = oneshot::channel::<()>();
    let a_updates = Updates::default();
    let a_cwd = cwd.clone();
    let a = tokio::spawn(client!(a_updates).connect_with(
        rho::acp::agent_in(agent.clone(), hub.clone(), false),
        async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let sid = cx
                .send_request(NewSessionRequest::new(a_cwd))
                .block_task()
                .await?
                .session_id;
            cx.send_request(prompt(&sid, "hi")).block_task().await?;
            sid_tx.send(sid.clone()).unwrap();
            go_rx.await.unwrap();
            cx.send_request(prompt(&sid, "again")).block_task().await?;
            // Then A disconnects.
            Ok(())
        },
    ));

    let b_updates = Updates::default();
    tokio::time::timeout(
        Duration::from_secs(10),
        client!(b_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = sid_rx.await.unwrap();
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                go_tx.send(()).unwrap();
                a.await.unwrap().unwrap();
                // A is gone; the session is still open for B.
                cx.send_request(prompt(&sid, "third")).block_task().await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // B: the replay, A's prompt live, then its own.
    assert_eq!(
        chunks(&b_updates),
        [
            "user: hi",
            "agent: One.",
            "user: again",
            "agent: Two.",
            "agent: Three."
        ]
    );
    // A: no replay and no echo of its own prompts, nothing after it left.
    assert_eq!(chunks(&a_updates), ["agent: One.", "agent: Two."]);
}

#[tokio::test]
async fn other_connections_see_prompts_but_not_commands() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![answer("One."), answer("Two.")]);
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    let (sid_tx, sid_rx) = oneshot::channel();
    let (go_tx, go_rx) = oneshot::channel::<()>();
    let a_cwd = cwd.clone();
    let a = tokio::spawn(Client.builder().connect_with(
        rho::acp::agent_in(agent.clone(), hub.clone(), false),
        async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let sid = cx
                .send_request(NewSessionRequest::new(a_cwd))
                .block_task()
                .await?
                .session_id;
            sid_tx.send(sid.clone()).unwrap();
            go_rx.await.unwrap();
            for text in ["/usage", "/nope x", "hi"] {
                cx.send_request(prompt(&sid, text)).block_task().await?;
            }
            Ok(())
        },
    ));

    let b_updates = Updates::default();
    tokio::time::timeout(
        Duration::from_secs(10),
        client!(b_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = sid_rx.await.unwrap();
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                go_tx.send(()).unwrap();
                a.await.unwrap().unwrap();
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // The unknown command is a prompt for the model.
    let chunks = chunks(&b_updates);
    let users: Vec<_> = chunks.iter().filter(|c| c.starts_with("user: ")).collect();
    assert_eq!(users, ["user: /nope x", "user: hi"], "{chunks:?}");
}

/// Waits for `go` before it answers, and tells `started` when asked.
struct Gated {
    inner: Arc<Scripted>,
    started: Arc<Notify>,
    go: Arc<Notify>,
}

impl Provider for Gated {
    fn stream(
        &self,
        request: Request,
        before_send: &dyn Fn(&mut serde_json::Value),
    ) -> BoxStream<'static, anyhow::Result<Event>> {
        self.started.notify_one();
        let events = self.inner.stream(request, before_send);
        let go = self.go.clone();
        stream::once(async move {
            go.notified().await;
            events
        })
        .flatten()
        .boxed()
    }
}

#[tokio::test]
async fn a_turn_outlives_its_connection() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let (started, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let provider = Arc::new(Gated {
        inner: Scripted::new(vec![answer("Late.")]),
        started: started.clone(),
        go: go.clone(),
    });
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    // A prompts and leaves while the model is still answering.
    let a_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(a_cwd))
                    .block_task()
                    .await?
                    .session_id;
                let _pending = cx.send_request(prompt(&sid, "hi"));
                started.notified().await;
                Ok(sid)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // B attaches while the turn holds the session, then sees it finish.
    let b_updates = Updates::default();
    let seen = b_updates.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        client!(b_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                let list = cx
                    .send_request(ListSessionsRequest::new().cwd(cwd.clone()))
                    .block_task()
                    .await?;
                let meta = list.sessions[0].meta.as_ref().expect("running meta");
                assert_eq!(meta["rho"]["running"], true);
                go.notify_one();
                while running(&seen) != [true, false] {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // The replay came from the store: the prompt was saved before the request.
    assert_eq!(chunks(&b_updates), ["user: hi", "agent: Late."]);
    // Busy when it attached, idle when the turn ended.
    assert_eq!(running(&b_updates), [true, false]);
}

#[tokio::test]
async fn a_connection_joining_a_running_turn_gets_the_usage() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let usage = rho::provider::Usage {
        input: 1234,
        output: 10,
        ..Default::default()
    };
    let mut first = answer("One.");
    first.insert(0, Event::Usage(usage));
    let (started, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let provider = Arc::new(Gated {
        inner: Scripted::new(vec![first, answer("Two.")]),
        started: started.clone(),
        go: go.clone(),
    });
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    // A runs one turn, then leaves during the second.
    let a_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(a_cwd))
                    .block_task()
                    .await?
                    .session_id;
                go.notify_one();
                cx.send_request(prompt(&sid, "one")).block_task().await?;
                // The first request's permit.
                started.notified().await;
                let _pending = cx.send_request(prompt(&sid, "two"));
                started.notified().await;
                Ok(sid)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // B attaches while the turn holds the session.
    let b_updates = Updates::default();
    let seen = b_updates.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        client!(b_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                // Before the running request reports anything.
                let used = loop {
                    let used = seen.lock().unwrap().iter().find_map(|u| match u {
                        SessionUpdate::UsageUpdate(u) => Some(u.used),
                        _ => None,
                    });
                    if let Some(used) = used {
                        break used;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                assert_eq!(used, 1244);
                go.notify_one();
                while running(&seen) != [true, false] {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
}

/// Asks before every tool call but `t0`.
struct Confirmer;

impl Extension for Confirmer {
    async fn tool_call(&self, ctx: &Context, call: &mut ExtToolCall) -> Decision {
        if call.id.as_str() == "t0" || ctx.confirm(Some(&call.id), "Run it?").await {
            Decision::Allow
        } else {
            Decision::Block("denied by user".into())
        }
    }
}

#[tokio::test]
async fn a_question_waits_for_the_next_connection() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![
        vec![
            text("Let me check."),
            Event::Block(Content::ToolUse {
                id: ToolCallId::new("t0"),
                name: ToolName::new("Bash"),
                input: serde_json::json!({"command": "echo zero", "description": "Say zero"}),
            }),
            Event::Block(Content::ToolUse {
                id: ToolCallId::new("t1"),
                name: ToolName::new("Bash"),
                input: serde_json::json!({"command": "echo hi", "description": "Say hi"}),
            }),
        ],
        answer("ok"),
    ]);
    let mut agent = Agent::new(
        provider.clone(),
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    );
    let db = dir.path().join("sessions.db");
    agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    agent.extensions = vec![Arc::new(Confirmer)];
    let agent = Arc::new(agent);
    let hub = Arc::new(Hub::default());

    // A is asked and leaves without an answer.
    let (asked_tx, asked_rx) = oneshot::channel();
    let asked_tx = Mutex::new(Some(asked_tx));
    let a_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_request(
                async move |_: RequestPermissionRequest,
                            _: Responder<RequestPermissionResponse>,
                            _cx| {
                    asked_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                rho::acp::agent_in(agent.clone(), hub.clone(), false),
                async |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let sid = cx
                        .send_request(NewSessionRequest::new(a_cwd))
                        .block_task()
                        .await?
                        .session_id;
                    let _pending = cx.send_request(prompt(&sid, "hi"));
                    asked_rx.await.unwrap();
                    Ok(sid)
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();

    // B loads the session: it gets the finished call, the open one, then the
    // question.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let (record, answer_record) = (seen.clone(), seen.clone());
    let b_updates = Updates::default();
    let updates = b_updates.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    if let SessionUpdate::ToolCall(call) = &n.update {
                        record
                            .lock()
                            .unwrap()
                            .push(format!("tool {}", call.tool_call_id.0));
                    }
                    updates.lock().unwrap().push(n.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            _cx| {
                    let id = &req.tool_call.tool_call_id.0;
                    answer_record.lock().unwrap().push(format!("ask {id}"));
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                rho::acp::agent_in(agent.clone(), hub.clone(), false),
                async |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                        .block_task()
                        .await?;
                    while !chunks(&b_updates).contains(&"agent: ok".to_string()) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();

    // Notices after the turn are tool calls too.
    assert_eq!(seen.lock().unwrap()[..3], ["tool t0", "tool t1", "ask t1"]);
    let t0_done = b_updates.lock().unwrap().iter().any(|u| {
        let v = serde_json::to_value(u).unwrap();
        v["toolCallId"] == "t0" && v["status"] == "completed"
    });
    assert!(t0_done, "t0 shows as finished");
    // The text before the calls is shown once.
    assert_eq!(
        chunks(&b_updates),
        ["user: hi", "agent: Let me check.", "agent: ok"]
    );
    let requests = provider.requests.lock().unwrap();
    let result = &requests[1].messages.last().unwrap().content[1];
    assert!(
        matches!(result, Content::ToolResult { is_error: false, content, .. } if content == "hi\n"),
        "{result:?}"
    );
}

/// The current value of a select config option.
fn current(options: &[SessionConfigOption], id: &str) -> Option<String> {
    options
        .iter()
        .find(|o| *o.id.0 == *id)
        .and_then(|o| match &o.kind {
            SessionConfigKind::Select(s) => Some(s.current_value.0.to_string()),
            _ => None,
        })
}

#[tokio::test]
async fn a_config_change_reaches_the_other_viewers() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let agent = agent(Scripted::new(vec![]), &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    let (sid_tx, sid_rx) = oneshot::channel();
    let (loaded_tx, loaded_rx) = oneshot::channel::<()>();
    let a_updates = Updates::default();
    let a_cwd = cwd.clone();
    let a = tokio::spawn(client!(a_updates).connect_with(
        rho::acp::agent_in(agent.clone(), hub.clone(), false),
        async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let sid = cx
                .send_request(NewSessionRequest::new(a_cwd))
                .block_task()
                .await?
                .session_id;
            sid_tx.send(sid.clone()).unwrap();
            loaded_rx.await.unwrap();
            let res = cx
                .send_request(SetSessionConfigOptionRequest::new(
                    sid,
                    "thought_level",
                    "low",
                ))
                .block_task()
                .await?;
            Ok(res.config_options)
        },
    ));

    let b_updates = Updates::default();
    let seen = b_updates.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        client!(b_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = sid_rx.await.unwrap();
                cx.send_request(LoadSessionRequest::new(sid, cwd.clone()))
                    .block_task()
                    .await?;
                loaded_tx.send(()).unwrap();
                let options = a.await.unwrap().unwrap();
                assert_eq!(current(&options, "thought_level").as_deref(), Some("low"));
                // B, which did not ask, is told too.
                loop {
                    let got = seen.lock().unwrap().iter().any(|u| {
                        matches!(u, SessionUpdate::ConfigOptionUpdate(c)
                            if current(&c.config_options, "thought_level").as_deref() == Some("low"))
                    });
                    if got {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // A got the options in its response, not again as an update.
    assert!(
        !a_updates
            .lock()
            .unwrap()
            .iter()
            .any(|u| matches!(u, SessionUpdate::ConfigOptionUpdate(_)))
    );
}

#[tokio::test]
async fn web_connections_start_sessions_only_under_home_or_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(Scripted::new(vec![]), &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());
    for web_limits in [true, false] {
        let started = Client
            .builder()
            .connect_with(
                rho::acp::agent_in(agent.clone(), hub.clone(), web_limits),
                async |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    cx.send_request(NewSessionRequest::new("/"))
                        .block_task()
                        .await
                },
            )
            .await;
        assert_eq!(started.is_ok(), !web_limits, "web_limits {web_limits}");
    }
}

/// Never answers a question, and tells `asked` when one comes.
struct Silent(Arc<Notify>);

impl rho::extension::Frontend for Silent {
    fn event(&self, _ev: rho::agent::AgentEvent<'_>) {}
    fn confirm<'a>(
        &'a self,
        _id: Option<&'a ToolCallId>,
        _title: &'a str,
    ) -> rho::extension::BoxFuture<'a, bool> {
        self.0.notify_one();
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_question_open_at_a_restart_is_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![
        vec![
            text("Let me check."),
            Event::Block(Content::ToolUse {
                id: ToolCallId::new("t1"),
                name: ToolName::new("Bash"),
                input: serde_json::json!({"command": "echo hi", "description": "Say hi"}),
            }),
        ],
        answer("ok"),
    ]);
    let mut agent = Agent::new(
        provider.clone(),
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    );
    let db = dir.path().join("sessions.db");
    agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    agent.extensions = vec![Arc::new(Confirmer)];
    let agent = Arc::new(agent);

    // The question is open when rho stops.
    let asked = Arc::new(Notify::new());
    let sid = rho::store::SessionId::new("s");
    let mut session = agent
        .start_session(sid.clone(), &cwd, vec![], Arc::new(Silent(asked.clone())))
        .await
        .unwrap();
    tokio::select! {
        _ = agent.run(&mut session, vec![Content::text("hi")]) => panic!("the run ended"),
        _ = asked.notified() => {}
    }
    std::mem::forget(session);

    // After the restart, loading the session asks again; the answer lets
    // the call run and the turn go on.
    let b_updates = Updates::default();
    let updates = b_updates.clone();
    let asks = Arc::new(Mutex::new(vec![]));
    let record = asks.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    updates.lock().unwrap().push(n.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            _cx| {
                    record
                        .lock()
                        .unwrap()
                        .push(req.tool_call.tool_call_id.0.to_string());
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow")),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                rho::acp::agent_in(agent.clone(), Arc::new(Hub::default()), false),
                async |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    cx.send_request(LoadSessionRequest::new(SessionId::new("s"), cwd.clone()))
                        .block_task()
                        .await?;
                    while !chunks(&b_updates).contains(&"agent: ok".to_string()) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(*asks.lock().unwrap(), ["t1"]);
    assert_eq!(
        chunks(&b_updates),
        ["user: hi", "agent: Let me check.", "agent: ok"]
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one more request, after the answer");
    let result = &requests[1].messages.last().unwrap().content[0];
    assert!(
        matches!(result, Content::ToolResult { is_error: false, content, .. } if content == "hi\n"),
        "{result:?}"
    );
}

#[tokio::test]
async fn two_connections_opening_a_stored_session_at_once_share_it() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![answer("One."), answer("Two.")]);
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let sid = rho::store::SessionId::new("s");
    let mut session = agent
        .start_session(sid, &cwd, vec![], Arc::new(rho::extension::NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    drop(session);

    let hub = Arc::new(Hub::default());
    let (a_loaded, b_loaded) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let b_updates = Updates::default();
    let seen = b_updates.clone();
    let a_cwd = cwd.clone();
    let (a_hub, a_agent, a_wait, a_done) = (
        hub.clone(),
        agent.clone(),
        b_loaded.clone(),
        a_loaded.clone(),
    );
    let a = Client.builder().connect_with(
        rho::acp::agent_in(a_agent, a_hub, false),
        async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let sid = SessionId::new("s");
            cx.send_request(LoadSessionRequest::new(sid.clone(), a_cwd))
                .block_task()
                .await?;
            a_done.notify_one();
            a_wait.notified().await;
            cx.send_request(prompt(&sid, "again")).block_task().await?;
            Ok(())
        },
    );
    let b = client!(b_updates).connect_with(
        rho::acp::agent_in(agent.clone(), hub.clone(), false),
        async |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.send_request(LoadSessionRequest::new(SessionId::new("s"), cwd.clone()))
                .block_task()
                .await?;
            b_loaded.notify_one();
            a_loaded.notified().await;
            while !chunks(&seen).contains(&"agent: Two.".to_string()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(())
        },
    );
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(a, b) })
        .await
        .unwrap();
    a.unwrap();
    b.unwrap();
    // B sees A's prompt and its answer live: one session.
    assert_eq!(
        chunks(&b_updates),
        ["user: hi", "agent: One.", "user: again", "agent: Two."]
    );
}

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/queue", response = Empty)]
struct Queue {
    session_id: SessionId,
    text: String,
}

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[serde(rename_all = "camelCase")]
#[request(method = "_rho/unqueue", response = Empty)]
struct Unqueue {
    session_id: SessionId,
    id: u64,
}

#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, agent_client_protocol::JsonRpcResponse,
)]
struct Empty {}

fn queue(sid: &SessionId, text: &str) -> Queue {
    Queue {
        session_id: sid.clone(),
        text: text.into(),
    }
}

/// The last `_meta.rho.queue` sent, as (id, text).
fn queued(updates: &Updates) -> Option<Vec<(u64, String)>> {
    updates.lock().unwrap().iter().rev().find_map(|u| match u {
        SessionUpdate::SessionInfoUpdate(i) => {
            let items = i.meta.as_ref()?["rho"]["queue"].as_array()?.clone();
            Some(
                items
                    .iter()
                    .map(|q| {
                        (
                            q["id"].as_u64().unwrap(),
                            q["text"].as_str().unwrap().into(),
                        )
                    })
                    .collect(),
            )
        }
        _ => None,
    })
}

/// The last user text of each request.
fn prompts(provider: &Scripted) -> Vec<String> {
    provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            r.messages
                .iter()
                .flat_map(|m| &m.content)
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .next_back()
                .unwrap_or_default()
        })
        .collect()
}

async fn wait_for(what: impl Fn() -> bool) {
    while !what() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn queued_prompts_run_after_the_turn_without_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let (started, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let inner = Scripted::new(vec![answer("One."), answer("Two.")]);
    let provider = Arc::new(Gated {
        inner: inner.clone(),
        started: started.clone(),
        go: go.clone(),
    });
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    // A queues two prompts during a turn, takes one back and leaves.
    let a_updates = Updates::default();
    let seen = a_updates.clone();
    let a_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        client!(a_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(a_cwd))
                    .block_task()
                    .await?
                    .session_id;
                let _pending = cx.send_request(prompt(&sid, "one"));
                started.notified().await;
                cx.send_request(queue(&sid, "two")).block_task().await?;
                cx.send_request(queue(&sid, "three")).block_task().await?;
                wait_for(|| queued(&seen).is_some_and(|q| q.len() == 2)).await;
                let id = queued(&seen).unwrap()[1].0;
                cx.send_request(Unqueue {
                    session_id: sid.clone(),
                    id,
                })
                .block_task()
                .await?;
                wait_for(|| queued(&seen).is_some_and(|q| q.len() == 1)).await;
                Ok(sid)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // Nobody is connected: the queued prompt runs after the turn.
    go.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        started.notified().await;
        go.notify_one();
        wait_for(|| inner.requests.lock().unwrap().len() == 2).await;
    })
    .await
    .unwrap();
    assert_eq!(prompts(&inner), ["one", "two"]);

    // B sees it as a user message, and the queue is empty.
    let b_updates = Updates::default();
    let seen = b_updates.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        client!(b_updates).connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                wait_for(|| chunks(&seen).contains(&"agent: Two.".to_string())).await;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        chunks(&b_updates),
        ["user: one", "agent: One.", "user: two", "agent: Two."]
    );
    assert!(queued(&b_updates).is_none_or(|q| q.is_empty()));
}

#[tokio::test]
async fn a_stop_holds_the_queue_until_a_turn_ends() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let (started, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let inner = Scripted::new(vec![answer("One."), answer("Three."), answer("Two.")]);
    let provider = Arc::new(Gated {
        inner: inner.clone(),
        started: started.clone(),
        go: go.clone(),
    });
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());
    // A stops the turn with a prompt queued; a turn that ends normally lets
    // the queue go on.
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let one = cx.send_request(prompt(&sid, "one"));
                started.notified().await;
                cx.send_request(queue(&sid, "two")).block_task().await?;
                cx.send_notification(CancelNotification::new(sid.clone()))?;
                assert_eq!(
                    one.block_task().await?.stop_reason,
                    agent_client_protocol::schema::v1::StopReason::Cancelled
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(prompts(&inner), ["one"], "held after the stop");
                go.notify_one();
                cx.send_request(prompt(&sid, "three")).block_task().await?;
                started.notified().await;
                go.notify_one();
                wait_for(|| inner.requests.lock().unwrap().len() == 3).await;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(prompts(&inner), ["one", "three", "two"]);
}

#[tokio::test]
async fn a_held_queue_runs_once_everyone_left() {
    let dir = tempfile::tempdir().unwrap();
    let (started, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let inner = Scripted::new(vec![answer("One."), answer("Two.")]);
    let provider = Arc::new(Gated {
        inner: inner.clone(),
        started: started.clone(),
        go: go.clone(),
    });
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let cwd = dir.path().to_path_buf();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent, Arc::new(Hub::default()), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let one = cx.send_request(prompt(&sid, "one"));
                started.notified().await;
                cx.send_request(queue(&sid, "two")).block_task().await?;
                cx.send_notification(CancelNotification::new(sid.clone()))?;
                one.block_task().await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    go.notify_one();
    tokio::time::timeout(
        Duration::from_secs(10),
        wait_for(|| inner.requests.lock().unwrap().len() == 2),
    )
    .await
    .unwrap();
    assert_eq!(prompts(&inner), ["one", "two"]);
}

/// Hands out the session's context and tells when the session ends.
#[derive(Default)]
struct Lifecycle {
    ctx: Mutex<Option<Context>>,
    ended: std::sync::atomic::AtomicBool,
}

impl Extension for Lifecycle {
    async fn session_start(&self, s: &mut rho::extension::SessionSetup) {
        *self.ctx.lock().unwrap() = Some(s.ctx.clone());
    }

    fn session_end(&self, _ctx: &Context) {
        self.ended.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[tokio::test]
async fn an_unused_session_is_unloaded_after_the_idle_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sessions.db");
    let life = Arc::new(Lifecycle::default());
    let mut agent = Agent::new(
        Scripted::new(vec![answer("Hi.")]),
        ModelSpec::new("test", "m"),
        vec![],
    );
    let path = db.clone();
    agent.sessions_db = Some(Arc::new(move |_| path.clone()));
    agent.extensions.push(life.clone());
    let agent = Arc::new(agent);
    let hub = Arc::new(Hub::default().with_idle_timeout(Duration::from_millis(300)));
    let cwd = dir.path().to_path_buf();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                cx.send_request(prompt(&sid, "hi")).block_task().await?;
                Ok(sid)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // Kept alive, e.g. by a background task, it stays past the timeout.
    let ctx = life.ctx.lock().unwrap().clone().unwrap();
    let alive = ctx.keep_alive();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!life.ended.load(std::sync::atomic::Ordering::Relaxed));

    drop(alive);
    let ended = || life.ended.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(5), wait_for(ended))
        .await
        .unwrap();
    let store = rho::store::Store::open(&db).unwrap();
    let notes = async || {
        let history = store
            .history(&rho::store::SessionId::new(&*sid.0))
            .await
            .unwrap();
        history
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::UserNote { title, .. } if title == "Session idle" => Some(title),
                _ => None,
            })
            .count()
    };
    assert_eq!(notes().await, 1);

    // Loaded and given only a notice, it unloads without another note.
    life.ended
        .store(false, std::sync::atomic::Ordering::Relaxed);
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent, hub, false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd))
                    .block_task()
                    .await?;
                let ctx = life.ctx.lock().unwrap().clone().unwrap();
                ctx.notify(rho::provider::NoteType::Info, "Notice", "", false);
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), wait_for(ended))
        .await
        .unwrap();
    assert_eq!(notes().await, 1);
}

#[tokio::test]
async fn a_prompt_queued_while_idle_runs_right_away() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![answer("Hi.")]);
    let agent = agent(provider.clone(), &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent, hub, false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?
                    .session_id;
                cx.send_request(queue(&sid, "hi")).block_task().await?;
                wait_for(|| provider.requests.lock().unwrap().len() == 1).await;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(prompts(&provider), ["hi"]);
}

/// Notifications with their `_meta`.
type Notes = Arc<Mutex<Vec<SessionNotification>>>;

/// Loads `sid` on a new connection, with `since` as `_meta.rho.since`, and
/// returns the replay.
async fn load_since(
    agent: &Arc<Agent>,
    hub: &Arc<Hub>,
    sid: &SessionId,
    cwd: &std::path::Path,
    since: Option<serde_json::Value>,
) -> Vec<SessionNotification> {
    let notes = Notes::default();
    let record = notes.clone();
    let mut load = LoadSessionRequest::new(sid.clone(), cwd.to_path_buf());
    if let Some(since) = since {
        let meta = serde_json::json!({ "rho": { "since": since } });
        load = load.meta(meta.as_object().unwrap().clone());
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    record.lock().unwrap().push(n);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                rho::acp::agent_in(agent.clone(), hub.clone(), false),
                async |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    cx.send_request(load).block_task().await?;
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    notes.lock().unwrap().clone()
}

/// The last `_meta.rho.cursor`.
fn last_cursor(notes: &[SessionNotification]) -> Option<serde_json::Value> {
    notes
        .iter()
        .rev()
        .find_map(|n| Some(n.meta.as_ref()?.get("rho")?.get("cursor")?.clone()))
}

fn note_updates(notes: &[SessionNotification]) -> Updates {
    Arc::new(Mutex::new(notes.iter().map(|n| n.update.clone()).collect()))
}

fn reset(notes: &[SessionNotification]) -> bool {
    notes.iter().any(|n| match &n.update {
        SessionUpdate::SessionInfoUpdate(i) => {
            i.meta.as_ref().is_some_and(|m| m["rho"]["reset"] == true)
        }
        _ => false,
    })
}

#[tokio::test]
async fn loading_again_with_a_cursor_sends_only_what_came_after() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Scripted::new(vec![answer("One."), answer("Two.")]);
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    // A starts the session and prompts; its own prompt is not echoed, but
    // the cursor still moves past it.
    let a_notes = Notes::default();
    let record = a_notes.clone();
    let a_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    record.lock().unwrap().push(n);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                rho::acp::agent_in(agent.clone(), hub.clone(), false),
                async move |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let sid = cx
                        .send_request(NewSessionRequest::new(a_cwd))
                        .block_task()
                        .await?
                        .session_id;
                    cx.send_request(prompt(&sid, "hi")).block_task().await?;
                    Ok(sid)
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    let cursor = last_cursor(&a_notes.lock().unwrap()).expect("a cursor");

    // Meanwhile another client goes on.
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                cx.send_request(prompt(&sid, "again")).block_task().await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    let after = load_since(&agent, &hub, &sid, &cwd, Some(cursor.clone())).await;
    assert!(!reset(&after));
    assert_eq!(
        chunks(&note_updates(&after)),
        ["user: again", "agent: Two."]
    );
    // Nothing new since the last cursor: nothing.
    let last = last_cursor(&after).unwrap();
    let none = load_since(&agent, &hub, &sid, &cwd, Some(last)).await;
    assert!(chunks(&note_updates(&none)).is_empty());

    // A cursor of another transcript, e.g. from before a restart: a reset
    // and everything.
    let mut stale = cursor;
    stale["transcript"] = "elsewhere".into();
    let all = load_since(&agent, &hub, &sid, &cwd, Some(stale)).await;
    assert!(reset(&all));
    assert_eq!(
        chunks(&note_updates(&all)),
        ["user: hi", "agent: One.", "user: again", "agent: Two."]
    );
    // Without a cursor: everything, no reset.
    let plain = load_since(&agent, &hub, &sid, &cwd, None).await;
    assert!(!reset(&plain));
    assert_eq!(chunks(&note_updates(&plain)).len(), 4);
}

#[tokio::test]
async fn a_session_joins_another_and_stands_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let (web_cwd, zed_cwd) = (dir.path().join("web"), dir.path().join("zed"));
    std::fs::create_dir_all(&web_cwd).unwrap();
    std::fs::create_dir_all(&zed_cwd).unwrap();
    let provider = Scripted::new(vec![answer("One."), answer("Two.")]);
    let agent = agent(provider, &dir.path().join("sessions.db"));
    let hub = Arc::new(Hub::default());

    // The web client starts the session that is joined later.
    let cwd = web_cwd.clone();
    let joined = tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async move |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                cx.send_request(prompt(&sid, "hi")).block_task().await?;
                Ok(sid)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // Zed starts its own, lists, joins and goes on.
    let notes = Notes::default();
    let record = notes.clone();
    let (cwd, target) = (zed_cwd.clone(), joined.clone());
    let (sid, listed) = tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    record.lock().unwrap().push(n);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                rho::acp::agent_in(agent.clone(), hub.clone(), false),
                async move |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let sid = cx
                        .send_request(NewSessionRequest::new(cwd))
                        .block_task()
                        .await?
                        .session_id;
                    let send = |text: String| cx.send_request(prompt(&sid, &text)).block_task();
                    send("/sessions:list".into()).await?;
                    for bad in ["nope", &sid.0] {
                        let joining = send(format!("/session:join {bad}")).await;
                        assert!(joining.is_err(), "joined {bad}");
                    }
                    // A start of the id is enough.
                    send(format!("/session:join {}", &target.0[..30])).await?;
                    let again = send(format!("/session:join {}", target.0)).await;
                    assert!(again.is_err(), "an alias joined again");
                    send("more".into()).await?;
                    let listed = cx
                        .send_request(ListSessionsRequest::new())
                        .block_task()
                        .await?;
                    Ok((sid, listed))
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();

    let notes = notes.lock().unwrap().clone();
    assert!(notes.iter().all(|n| n.session_id == sid), "{notes:?}");
    // The list with the session to join, its replay, then the prompt that
    // ran in it.
    let shown = chunks(&note_updates(&notes));
    assert!(shown[0].contains(&*joined.0), "{shown:?}");
    assert_eq!(shown[1..], ["user: hi", "agent: One.", "agent: Two."]);
    let ids: Vec<_> = listed.sessions.iter().map(|s| &s.session_id).collect();
    assert_eq!(ids, [&joined]);

    // After a restart the alias still loads the joined session, wherever
    // the client says it is.
    let restarted = Arc::new(Hub::default());
    let replay = load_since(&agent, &restarted, &sid, &zed_cwd, None).await;
    assert_eq!(
        chunks(&note_updates(&replay)),
        ["user: hi", "agent: One.", "user: more", "agent: Two."]
    );
}
