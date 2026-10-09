//! Session persistence: sessions are saved as they run and can be continued,
//! directly and over ACP (`session/list`, `session/load`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{Client, ConnectionTo};
use rho::agent::Agent;
use rho::extension::NoFrontend;
use rho::provider::{Content, Event, Message, Role, StopReason, Usage};
use serde_json::json;

mod common;
use common::{Scripted, text};

fn agent(provider: Arc<Scripted>, db: &Path) -> Agent {
    let mut agent = Agent::new(provider, "m", rho::tools::builtin());
    let db = db.to_path_buf();
    agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    agent
}

fn tool_then_answer() -> Vec<Vec<Event>> {
    vec![
        vec![
            text("Let me look."),
            Event::Block(Content::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: json!({"command": "echo from-tool"}),
            }),
            Event::Usage(Usage {
                input: 10,
                output: 5,
                ..Default::default()
            }),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("Done."), Event::Done(StopReason::EndTurn)],
    ]
}

#[tokio::test]
async fn sessions_are_saved_and_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd) = (dir.path().join("sessions.db"), dir.path().to_path_buf());
    let mut responses = tool_then_answer();
    responses.push(vec![text("Again."), Event::Done(StopReason::EndTurn)]);
    let provider = Scripted::new(responses);
    let agent = agent(provider.clone(), &db);

    // Not stored until the first prompt.
    let empty = agent
        .start_session("empty", &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    let store = empty.ctx.store().unwrap().clone();
    assert!(store.session("empty").await.unwrap().is_none());
    drop(empty);

    let mut session = agent
        .start_session("s1", &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    session.ctx.set_title("A title");
    session.thinking = rho::provider::Thinking::Low;
    session.save().await;
    let history = session.history.clone();
    assert_eq!(history.len(), 4);
    drop(session);

    let mut resumed = agent
        .resume_session("s1", &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert_eq!(resumed.history, history);
    assert_eq!(resumed.usage.input, 10);
    assert_eq!(resumed.thinking, rho::provider::Thinking::Low);
    assert_eq!(resumed.ctx.title().as_deref(), Some("A title"));

    agent
        .run(&mut resumed, vec![Content::text("more")])
        .await
        .unwrap();
    // The continued run sent the stored history.
    let sent = provider
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages
        .clone();
    assert_eq!(sent[..4], history[..]);

    let stored = store.history("s1").await.unwrap();
    assert_eq!(stored, resumed.history);
    assert_eq!(stored.len(), 6);
    let page = store.list(Some(&cwd), None, 10).await.unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].title.as_deref(), Some("A title"));

    // Wrong cwd or id.
    let other = dir.path().join("other");
    assert!(
        agent
            .resume_session("s1", &other, vec![], Arc::new(NoFrontend))
            .await
            .is_err()
    );
    assert!(
        agent
            .resume_session("nope", &cwd, vec![], Arc::new(NoFrontend))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn compaction_starts_a_new_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sessions.db");
    let mut responses = tool_then_answer();
    responses.push(vec![text("## Goal\nsummary")]);
    let agent = agent(Scripted::new(responses), &db);

    let mut session = agent
        .start_session("s", dir.path(), vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    agent.compact(&mut session, "").await.unwrap();
    let summary = session.history.clone();
    drop(session);

    let resumed = agent
        .resume_session("s", dir.path(), vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert_eq!(resumed.history, summary);
    assert_eq!(resumed.history.len(), 1);

    // The messages before the compaction are kept.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let rows: Vec<(i64, i64)> = conn
        .prepare("SELECT seq, epoch FROM messages WHERE session_id = 's' ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, [(0, 0), (1, 0), (2, 0), (3, 0), (4, 1)]);
}

type Updates = Arc<Mutex<Vec<SessionUpdate>>>;

fn prompt(sid: &agent_client_protocol::schema::v1::SessionId, text: &str) -> PromptRequest {
    PromptRequest::new(
        sid.clone(),
        vec![ContentBlock::Text(TextContent::new(text))],
    )
}

#[tokio::test]
async fn acp_lists_and_loads_saved_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let mut responses = tool_then_answer();
    responses.push(vec![text("Again."), Event::Done(StopReason::EndTurn)]);
    let provider = Scripted::new(responses);
    let agent = Arc::new(agent(provider.clone(), &db));

    // First connection: one prompt.
    let first_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent(agent.clone()),
            async |cx: ConnectionTo<_>| {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(init.agent_capabilities.load_session);
                let sid = cx
                    .send_request(NewSessionRequest::new(first_cwd))
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

    // Second connection (a restarted rho): list, load, continue.
    let updates = Updates::default();
    let record = updates.clone();
    let load_sid = sid.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client
            .builder()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    record.lock().unwrap().push(n.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                rho::acp::agent(agent.clone()),
                async |cx: ConnectionTo<_>| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let list = cx
                        .send_request(ListSessionsRequest::new().cwd(cwd.clone()))
                        .block_task()
                        .await?;
                    assert_eq!(list.sessions.len(), 1);
                    assert_eq!(list.sessions[0].session_id, load_sid);
                    assert!(list.sessions[0].updated_at.is_some());

                    cx.send_request(LoadSessionRequest::new(load_sid.clone(), cwd.clone()))
                        .block_task()
                        .await?;
                    cx.send_request(prompt(&load_sid, "more"))
                        .block_task()
                        .await?;
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();

    // The replay: the user prompt, the answer around the tool call.
    let updates = updates.lock().unwrap();
    let chunks: Vec<String> = updates
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
            SessionUpdate::ToolCall(t) => Some(format!("tool: {}", t.tool_call_id.0)),
            _ => None,
        })
        .collect();
    assert_eq!(
        chunks,
        [
            "user: hi",
            "agent: Let me look.",
            "tool: t1",
            "agent: Done.",
            "agent: Again."
        ]
    );

    // The continued prompt had the earlier conversation.
    let requests = provider.requests.lock().unwrap();
    let last = &requests.last().unwrap().messages;
    assert_eq!(last.len(), 5);
    assert_eq!(
        last[4],
        Message {
            role: Role::User,
            content: vec![Content::text("more")]
        }
    );
}
