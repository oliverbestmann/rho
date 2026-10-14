//! Shared sessions: connections on one `Hub` (like `rho daemon`) see each
//! other's sessions live, and a turn outlives the connection that started it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, InitializeRequest, LoadSessionRequest, NewSessionRequest,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, TextContent,
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
        rho::acp::agent_in(agent.clone(), hub.clone()),
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
            rho::acp::agent_in(agent.clone(), hub.clone()),
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
            rho::acp::agent_in(agent.clone(), hub.clone()),
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
            rho::acp::agent_in(agent.clone(), hub.clone()),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                    .block_task()
                    .await?;
                go.notify_one();
                while !chunks(&seen).contains(&"agent: Late.".to_string()) {
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
}

/// Asks before every tool call.
struct Confirmer;

impl Extension for Confirmer {
    async fn tool_call(&self, ctx: &Context, call: &mut ExtToolCall) -> Decision {
        if ctx.confirm(Some(&call.id), "Run it?").await {
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
        vec![Event::Block(Content::ToolUse {
            id: ToolCallId::new("t1"),
            name: ToolName::new("Bash"),
            input: serde_json::json!({"command": "echo hi"}),
        })],
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
                rho::acp::agent_in(agent.clone(), hub.clone()),
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

    // B loads the session: it gets the tool call, then the question.
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
                rho::acp::agent_in(agent.clone(), hub.clone()),
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
    assert_eq!(seen.lock().unwrap()[..2], ["tool t1", "ask t1"]);
    let requests = provider.requests.lock().unwrap();
    let result = &requests[1].messages.last().unwrap().content[0];
    assert!(
        matches!(result, Content::ToolResult { is_error: false, content, .. } if content == "hi\n"),
        "{result:?}"
    );
}
