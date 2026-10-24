//! Session persistence: sessions are saved as they run and can be continued,
//! directly and over ACP (`session/list`, `session/load`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, ImageContent, InitializeRequest, ListSessionsRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, SessionNotification, SessionUpdate,
    TextContent, ToolCallContent,
};
use agent_client_protocol::{Client, ConnectionTo};
use rho::agent::Agent;
use rho::extension::NoFrontend;
use rho::provider::{
    Content, Event, Message, ModelSpec, NoteType, Role, StopReason, ToolCallId, ToolName, Usage,
};
use rho::store::SessionId;
use serde_json::json;

mod common;
use common::{Scripted, text};

fn agent(provider: Arc<Scripted>, db: &Path) -> Agent {
    let mut agent = Agent::new(provider, ModelSpec::new("test", "m"), rho::tools::builtin());
    let db = db.to_path_buf();
    agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    agent
}

fn tool_then_answer() -> Vec<Vec<Event>> {
    vec![
        vec![
            text("Let me look."),
            Event::Block(Content::ToolUse {
                id: ToolCallId::new("t1"),
                name: ToolName::new("Bash"),
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

    // Stored right away, so it can be resumed before the first prompt.
    let empty = agent
        .start_session(SessionId::new("empty"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    let store = empty.ctx.store().unwrap().clone();
    assert!(
        store
            .session(&SessionId::new("empty"))
            .await
            .unwrap()
            .is_some()
    );
    drop(empty);
    let resumed = agent
        .resume_session(&SessionId::new("empty"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert!(resumed.history.is_empty());
    drop(resumed);

    let mut session = agent
        .start_session(SessionId::new("s1"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    session.ctx.set_title("A title");
    session.thinking = rho::provider::Thinking::Low;
    session.ctx.set_stored("test.flag", true);
    session.save().await;
    let history = session.history.clone();
    // Prompt, tool call, results, answer, then the turn report note.
    assert_eq!(history.len(), 5);
    drop(session);

    let mut resumed = agent
        .resume_session(&SessionId::new("s1"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert_eq!(resumed.history, history);
    assert_eq!(resumed.usage.input, 10);
    assert_eq!(resumed.thinking, rho::provider::Thinking::Low);
    assert_eq!(resumed.ctx.title().as_deref(), Some("A title"));
    assert_eq!(resumed.ctx.stored::<bool>("test.flag"), Some(true));

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
    // The turn report note is not sent.
    assert_eq!(sent[..4], history[..4]);

    let stored = store.history(&SessionId::new("s1")).await.unwrap();
    assert_eq!(stored, resumed.history);
    // The new prompt merged into the note's message, then the answer and
    // the next turn report.
    assert_eq!(stored.len(), 7);
    let page = store.list(Some(&cwd), None, 10).await.unwrap();
    // "empty" has no messages, so it is not listed.
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].title.as_deref(), Some("A title"));

    // Wrong cwd or id.
    let other = dir.path().join("other");
    assert!(
        agent
            .resume_session(&SessionId::new("s1"), &other, vec![], Arc::new(NoFrontend))
            .await
            .is_err()
    );
    assert!(
        agent
            .resume_session(&SessionId::new("nope"), &cwd, vec![], Arc::new(NoFrontend))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn resume_falls_back_when_the_stored_model_is_forbidden() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd) = (dir.path().join("sessions.db"), dir.path().to_path_buf());
    let provider = Scripted::new(vec![]);
    let mut agent = agent(provider.clone(), &db);
    agent
        .registry
        .register(rho::provider::ProviderName::new("test"), provider);
    drop(
        agent
            .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
            .await
            .unwrap(),
    );

    let rules = format!(
        r#"{{"rules": [{{"path": "{}", "allow": ["test/other"], "default": "test/other"}}]}}"#,
        cwd.display()
    );
    agent.models = rho::models::Config::parse(&rules, "models.json".as_ref()).unwrap();
    let log = Arc::new(common::Log::default());
    let resumed = agent
        .resume_session(&SessionId::new("s"), &cwd, vec![], log.clone())
        .await
        .unwrap();
    assert_eq!(resumed.model, ModelSpec::new("test", "other"));
    assert!(
        log.lines().iter().any(|l| l.contains("Model unavailable")),
        "{:?}",
        log.lines()
    );
}

#[tokio::test]
async fn changed_additional_directories_are_announced() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd) = (dir.path().join("sessions.db"), dir.path().to_path_buf());
    let provider = Scripted::new(vec![vec![text("Ok."), Event::Done(StopReason::EndTurn)]]);
    let agent = agent(provider, &db);
    let (a, b) = (PathBuf::from("/extra/a"), PathBuf::from("/extra/b"));

    let mut session = agent
        .start_session(
            SessionId::new("s1"),
            &cwd,
            vec![a.clone()],
            Arc::new(NoFrontend),
        )
        .await
        .unwrap();
    assert_eq!(session.dirs_notice().as_deref(), Some("- /extra/a"));
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    session.save().await;
    drop(session);

    let notices = async |dirs: Vec<PathBuf>| {
        let log = Arc::new(common::Log::default());
        agent
            .resume_session(&SessionId::new("s1"), &cwd, dirs, log.clone())
            .await
            .unwrap()
            .save()
            .await;
        log.lines()
            .into_iter()
            .filter(|l| l.contains("Additional directories"))
            .collect::<Vec<_>>()
    };
    // Same directories: nothing to say.
    assert_eq!(notices(vec![a.clone()]).await, Vec::<String>::new());
    let changed = notices(vec![a.clone(), b.clone()]).await;
    assert_eq!(changed.len(), 1);
    assert!(
        changed[0].contains("- /extra/a\\n- /extra/b"),
        "{changed:?}"
    );
    let removed = notices(vec![]).await;
    assert!(removed[0].contains("(none)"), "{removed:?}");

    // An open session: sandbox, prompt and store follow; grants stay.
    let log = Arc::new(common::Log::default());
    let mut open = agent
        .resume_session(&SessionId::new("s1"), &cwd, vec![a.clone()], log.clone())
        .await
        .unwrap();
    let granted = PathBuf::from("/granted");
    let mut policy = open.ctx.sandbox().unwrap();
    policy.allow(&granted, rho::sandbox::Access::Read);
    open.ctx.set_sandbox(Some(policy));
    open.system += "\n\nfrom an extension";

    assert!(!agent.set_additional_dirs(&mut open, vec![a.clone()]));
    assert!(agent.set_additional_dirs(&mut open, vec![b.clone()]));
    let paths: Vec<PathBuf> = open
        .ctx
        .sandbox()
        .unwrap()
        .rules
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert!(paths.contains(&b) && paths.contains(&granted) && paths.contains(&cwd));
    assert!(!paths.contains(&a), "{paths:?}");
    assert!(
        open.system
            .contains("- /extra/b\n</additional_directories>")
    );
    assert!(!open.system.contains("/extra/a"));
    assert!(open.system.ends_with("from an extension"));
    assert!(log.lines().iter().any(|l| l.contains("- /extra/b")));
    open.save().await;
    let store = open.ctx.store().unwrap().clone();
    let info = store.session(&SessionId::new("s1")).await.unwrap().unwrap();
    assert_eq!(info.additional_dirs, [b]);
}

#[tokio::test]
async fn compaction_starts_a_new_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sessions.db");
    let mut responses = tool_then_answer();
    responses.push(vec![text("## Goal\nsummary")]);
    let agent = agent(Scripted::new(responses), &db);

    let mut session = agent
        .start_session(
            SessionId::new("s"),
            dir.path(),
            vec![],
            Arc::new(NoFrontend),
        )
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
        .resume_session(
            &SessionId::new("s"),
            dir.path(),
            vec![],
            Arc::new(NoFrontend),
        )
        .await
        .unwrap();
    assert_eq!(resumed.history, summary);
    // The summary with the compaction notice.
    assert_eq!(resumed.history.len(), 1);
    assert!(matches!(summary[0].content[1], Content::UserNote { .. }));

    // The messages before the compaction are kept.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let rows: Vec<(i64, i64)> = conn
        .prepare("SELECT seq, epoch FROM items WHERE session_id = 's' ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    // One row per block: prompt, text and tool call, result, answer, then
    // the turn report as the old epoch's last; the summary and its notice.
    assert_eq!(
        rows,
        [
            (0, 0),
            (1, 0),
            (2, 0),
            (3, 0),
            (4, 0),
            (5, 0),
            (6, 1),
            (7, 1)
        ]
    );
}

#[tokio::test]
async fn acp_load_replays_the_current_and_the_previous_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let mut responses = vec![vec![text("First."), Event::Done(StopReason::EndTurn)]];
    responses.push(vec![text("the first summary")]);
    responses.extend(tool_then_answer());
    responses.push(vec![text("the summary")]);
    responses.push(vec![text("After."), Event::Done(StopReason::EndTurn)]);
    let agent = Arc::new(agent(Scripted::new(responses), &db));

    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    // Epoch 0 is not shown.
    agent
        .run(&mut session, vec![Content::text("first")])
        .await
        .unwrap();
    agent.compact(&mut session, "").await.unwrap();
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    agent.compact(&mut session, "").await.unwrap();
    agent
        .run(&mut session, vec![Content::text("next")])
        .await
        .unwrap();
    drop(session);

    let updates = load(agent, &cwd).await;
    let shown: Vec<String> = updates
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
            SessionUpdate::ToolCall(t) => {
                let text = match t.content.first() {
                    Some(ToolCallContent::Content(c)) => match &c.content {
                        ContentBlock::Text(t) => t.text.clone(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                // The timing in turn reports varies.
                let title = t
                    .title
                    .split(':')
                    .next()
                    .filter(|_| t.title.starts_with("Turn:"));
                Some(format!("tool: {} {text}", title.unwrap_or(&t.title)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        [
            "tool: Compacted (test/m, cache warm) input 0 + cache read 0 + cache write 0 | output 0\n\nthe first summary",
            "user: hi",
            "agent: Let me look.",
            "tool: Bash echo from-tool ```sh\necho from-tool\n```",
            "agent: Done.",
            "tool: Turn ",
            "tool: Compacted (test/m, cache warm) input 0 + cache read 0 + cache write 0 | output 0\n\nthe summary",
            "user: next",
            "agent: After.",
            "tool: Turn ",
        ]
    );
}

#[tokio::test]
async fn acp_load_replays_edit_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    std::fs::write(cwd.join("f.txt"), "a\n").unwrap();
    let edit = vec![
        Event::Block(Content::ToolUse {
            id: ToolCallId::new("t1"),
            name: ToolName::new("Edit"),
            input: json!({"path": "f.txt", "edits": [{"oldText": "a", "newText": "b"}]}),
        }),
        Event::Done(StopReason::ToolUse),
    ];
    let responses = vec![edit, vec![text("Done."), Event::Done(StopReason::EndTurn)]];
    let agent = Arc::new(agent(Scripted::new(responses), &db));
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("edit")])
        .await
        .unwrap();
    drop(session);

    let updates = load(agent, &cwd).await;
    let diff = updates.iter().find_map(|u| match u {
        SessionUpdate::ToolCallUpdate(t) => {
            t.fields.content.as_ref()?.iter().find_map(|c| match c {
                ToolCallContent::Diff(d) => Some(d.clone()),
                _ => None,
            })
        }
        _ => None,
    });
    let diff = diff.expect("replayed diff");
    assert_eq!(diff.path, cwd.join("f.txt"));
    assert_eq!(diff.old_text.as_deref(), Some("a\n"));
    assert_eq!(diff.new_text, "b\n");
}

#[tokio::test]
async fn acp_load_hides_system_notes_and_shows_user_notes() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let agent = Arc::new(agent(Scripted::new(vec![]), &db));
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    session.history = vec![Message {
        role: Role::User,
        content: vec![
            Content::system_note("<system-reminder>for the model</system-reminder>"),
            Content::text("hi"),
            Content::UserNote {
                kind: NoteType::Text,
                title: "/go:check".into(),
                text: "off".into(),
                is_error: false,
            },
        ],
    }];
    session.save().await;
    drop(session);

    let shown: Vec<String> = load(agent, &cwd)
        .await
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::UserMessageChunk(ContentChunk {
                content: ContentBlock::Text(t),
                ..
            }) => Some(format!("user: {}", t.text)),
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(t),
                meta,
                ..
            }) => Some(format!(
                "agent: {} {:?}",
                t.text,
                meta.as_ref()?["rho"]["note"]
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        ["user: hi", "agent: **/go:check**\n\noff Bool(true)"]
    );
}

#[tokio::test]
async fn acp_saves_notices_when_the_session_is_dropped() {
    use rho::extension::{Context, Extension, SessionSetup};
    /// Hands out the session's context, to notify while it is idle.
    struct Grab(Arc<Mutex<Option<Context>>>);
    impl Extension for Grab {
        async fn session_start(&self, s: &mut SessionSetup) {
            *self.0.lock().unwrap() = Some(s.ctx.clone());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let grabbed = Arc::default();
    let answer = vec![text("ok"), Event::Done(StopReason::EndTurn)];
    let mut agent = agent(Scripted::new(vec![answer]), &db);
    agent.extensions.push(Arc::new(Grab(Arc::clone(&grabbed))));
    let agent = Arc::new(agent);
    // One hub, like the daemon: separate ones would race for the store.
    let hub = Arc::new(rho::acp::Hub::default());

    let new_cwd = cwd.clone();
    let sid = tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let sid = cx
                    .send_request(NewSessionRequest::new(new_cwd))
                    .block_task()
                    .await?
                    .session_id;
                let ctx = grabbed.lock().unwrap().clone().unwrap();
                ctx.notify(NoteType::Info, "late", "", false);
                Ok(sid)
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    // The connection is gone, so the hub saves the session and then drops
    // it. A client that comes back right away gets the open session or the
    // saved one; either way its prompt does not overwrite the notice.
    let load_cwd = cwd.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        Client.builder().connect_with(
            rho::acp::agent_in(agent.clone(), hub.clone(), false),
            async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(sid.clone(), load_cwd))
                    .block_task()
                    .await?;
                cx.send_request(prompt(&sid, "next")).block_task().await?;
                Ok(())
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();

    let store = rho::store::Store::open(&db).unwrap();
    let history = store.history(&SessionId::new(&*sid.0)).await.unwrap();
    let late = Content::UserNote {
        kind: NoteType::Info,
        title: "late".into(),
        text: String::new(),
        is_error: false,
    };
    assert_eq!(history[0].content[..2], [late, Content::text("next")]);
}

/// Loads session "s" over ACP and returns the replayed updates.
async fn load(agent: Arc<Agent>, cwd: &Path) -> Vec<SessionUpdate> {
    load_with(agent, cwd, false).await
}

/// `load` by a client that shows terminals if `terminal`.
async fn load_with(agent: Arc<Agent>, cwd: &Path, terminal: bool) -> Vec<SessionUpdate> {
    let mut caps = agent_client_protocol::schema::v1::ClientCapabilities::new();
    caps.meta = terminal.then(|| {
        json!({"terminal_output": true})
            .as_object()
            .cloned()
            .unwrap()
    });
    let updates = Updates::default();
    let record = updates.clone();
    let cwd = cwd.to_path_buf();
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
                    cx.send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps),
                    )
                    .block_task()
                    .await?;
                    cx.send_request(LoadSessionRequest::new("s", cwd))
                        .block_task()
                        .await?;
                    Ok(())
                },
            ),
    )
    .await
    .unwrap()
    .unwrap();
    std::mem::take(&mut *updates.lock().unwrap())
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
                let mut hi = prompt(&sid, "hi");
                hi.prompt.push(ContentBlock::Image(ImageContent::new(
                    "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7",
                    "image/gif",
                )));
                cx.send_request(hi).block_task().await?;
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

    // The replay: the user prompt with its image, the answer around the tool call.
    let updates = updates.lock().unwrap();
    let chunks: Vec<String> = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::UserMessageChunk(ContentChunk {
                content: ContentBlock::Text(t),
                ..
            }) => Some(format!("user: {}", t.text)),
            SessionUpdate::UserMessageChunk(ContentChunk {
                content: ContentBlock::Image(i),
                ..
            }) => Some(format!("user image: {} {}", i.mime_type, i.data)),
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(t),
                ..
            }) => Some(format!("agent: {}", t.text)),
            SessionUpdate::ToolCall(t) => Some(format!("tool: {}", t.tool_call_id.0)),
            SessionUpdate::UsageUpdate(u) => Some(format!(
                "usage: {} of {}",
                u.used,
                u.meta.as_ref().unwrap()["rho"]["session"]["input"]
            )),
            _ => None,
        })
        .collect();
    // The stored usage follows the replay: the last request's context, the session total.
    assert_eq!(
        chunks,
        [
            "user: hi",
            "user image: image/gif R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7",
            "agent: Let me look.",
            "tool: t1",
            "agent: Done.",
            // The saved turn report.
            "tool: notice-4",
            "usage: 15 of 10",
            "agent: Again.",
            // The new prompt's usage notice.
            "tool: notice-7"
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

/// One response that runs `echo first`, then `sleep 30`.
fn two_calls() -> Vec<Event> {
    let bash = |id: &str, command: &str| {
        Event::Block(Content::ToolUse {
            id: ToolCallId::new(id),
            name: ToolName::new("Bash"),
            input: json!({"command": command, "description": "Run it"}),
        })
    };
    vec![
        text("Two calls."),
        bash("t1", "echo first"),
        bash("t2", "sleep 30"),
        Event::Done(StopReason::ToolUse),
    ]
}

/// Runs `two_calls` until `t2` has started, then drops the run, like a
/// cancel or a restart does.
async fn stop_during_t2(agent: &Agent, session: &mut rho::agent::Session, db: &Path) {
    stop_in(agent, session, db, "t2").await
}

/// Prompts "hi" until tool call `id` has started, then drops the run.
async fn stop_in(agent: &Agent, session: &mut rho::agent::Session, db: &Path, id: &str) {
    let run = agent.run(session, vec![Content::text("hi")]);
    let started = async {
        let conn = rusqlite::Connection::open(db).unwrap();
        loop {
            let at: Option<i64> = conn
                .query_row(
                    "SELECT started_at FROM items WHERE tool_call_id = ?1
                         AND json_extract(block, '$.type') = 'tool_use'",
                    [id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            if at.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _ = run => panic!("the run ended"),
        _ = tokio::time::timeout(Duration::from_secs(10), started) => {}
    }
}

fn result(id: &str, content: &str, is_error: bool) -> Content {
    Content::ToolResult {
        tool_use_id: ToolCallId::new(id),
        content: content.into(),
        is_error,
        images: vec![],
    }
}

#[tokio::test]
async fn a_restart_during_tool_calls_keeps_the_response_and_the_finished_results() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let responses = vec![
        two_calls(),
        vec![text("Ok."), Event::Done(StopReason::EndTurn)],
    ];
    let provider = Scripted::new(responses);
    let agent = agent(provider.clone(), &db);
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    stop_during_t2(&agent, &mut session, &db).await;
    // The process ends: nothing more is saved.
    std::mem::forget(session);

    let mut resumed = agent
        .resume_session(&SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert_eq!(resumed.history.len(), 3, "{:#?}", resumed.history);
    assert_eq!(resumed.history[1].content.len(), 3, "text and both calls");
    let results: Vec<_> = resumed.history[2]
        .content
        .iter()
        .filter(|c| matches!(c, Content::ToolResult { .. }))
        .cloned()
        .collect();
    assert_eq!(
        results,
        [
            result("t1", "first\n", false),
            result("t2", "interrupted: rho stopped while this call ran", true),
        ]
    );

    // The next prompt goes after the results; the request is valid.
    agent
        .run(&mut resumed, vec![Content::text("next")])
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let last = requests[1].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content[..2], results[..]);
    assert_eq!(last.content.last(), Some(&Content::text("next")));
}

#[tokio::test]
async fn a_cancelled_tool_call_gets_a_result_with_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let responses = vec![
        two_calls(),
        vec![text("Ok."), Event::Done(StopReason::EndTurn)],
    ];
    let provider = Scripted::new(responses);
    let agent = agent(provider.clone(), &db);
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    stop_during_t2(&agent, &mut session, &db).await;

    agent
        .run(&mut session, vec![Content::text("next")])
        .await
        .unwrap();
    let last = provider.requests.lock().unwrap()[1]
        .messages
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        last.content,
        [
            result("t1", "first\n", false),
            result(
                "t2",
                "cancelled: the run was stopped before this call finished",
                true
            ),
            Content::text("next"),
        ]
    );
    // And it is saved like that.
    drop(session);
    let store = rho::store::Store::open(&db).unwrap();
    let history = store.history(&SessionId::new("s")).await.unwrap();
    assert_eq!(history[2].content[..2], last.content[..2]);
}

#[tokio::test]
async fn acp_load_replays_the_streamed_terminal_output() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let call = Event::Block(Content::ToolUse {
        id: ToolCallId::new("t1"),
        name: ToolName::new("Bash"),
        input: json!({"command": "echo hi; sleep 0.1; echo oops >&2", "description": "Say hi"}),
    });
    let responses = vec![
        vec![call, Event::Done(StopReason::ToolUse)],
        vec![text("Done."), Event::Done(StopReason::EndTurn)],
    ];
    let agent = Arc::new(agent(Scripted::new(responses), &db));
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    drop(session);

    let wire: Vec<serde_json::Value> = load_with(agent, &cwd, true)
        .await
        .into_iter()
        .map(|u| serde_json::to_value(u).unwrap())
        .filter(|v| v["toolCallId"] == "t1")
        .collect();
    let shown: String = wire
        .iter()
        .filter_map(|v| v["_meta"]["terminal_output"]["data"].as_str())
        .collect();
    // As it streamed (the sleep keeps the pipes in order), stderr in red;
    // the end sends only the exit code.
    assert_eq!(
        shown,
        "$ echo hi\n  sleep 0.1\n  echo oops >&2\nhi\n\x1b[31moops\n\x1b[0m"
    );
    assert_eq!(
        wire.last().unwrap()["_meta"]["terminal_exit"]["exit_code"],
        0
    );
}

/// A read-only tool that hangs if `hang`, else answers "read".
struct Look {
    hang: bool,
}

impl rho::tools::Tool for Look {
    fn spec(&self) -> rho::provider::ToolSpec {
        rho::provider::ToolSpec {
            name: ToolName::new("Look"),
            description: "reads".into(),
            input_schema: json!({"type": "object"}),
        }
    }
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::Read
    }
    async fn run(
        &self,
        _input: serde_json::Value,
        _ctx: &rho::extension::Context,
    ) -> anyhow::Result<rho::extension::ToolOutput> {
        if self.hang {
            std::future::pending::<()>().await;
        }
        Ok("read".into())
    }
}

#[tokio::test]
async fn a_read_cut_off_by_a_restart_runs_again() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd): (PathBuf, PathBuf) = (dir.path().join("sessions.db"), dir.path().into());
    let look = Event::Block(Content::ToolUse {
        id: ToolCallId::new("t1"),
        name: ToolName::new("Look"),
        input: json!({}),
    });
    let provider = Scripted::new(vec![
        vec![look, Event::Done(StopReason::ToolUse)],
        vec![text("Seen."), Event::Done(StopReason::EndTurn)],
    ]);
    let with = |hang| {
        let mut a = agent(provider.clone(), &db);
        a.tools = vec![Arc::new(Look { hang })];
        a
    };
    let before = with(true);
    let mut session = before
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    stop_in(&before, &mut session, &db, "t1").await;
    std::mem::forget(session);

    let after = with(false);
    let mut resumed = after
        .resume_session(&SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert!(resumed.resuming());
    after.run_triggered(&mut resumed).await.unwrap().unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests[1].messages.last().unwrap().content,
        [result("t1", "read", false)]
    );
    // The answer, then the turn report.
    let n = resumed.history.len();
    assert_eq!(resumed.history[n - 2].content, [Content::text("Seen.")]);
}

#[tokio::test]
async fn fork_copies_the_session_with_a_new_title() {
    let dir = tempfile::tempdir().unwrap();
    let (db, cwd) = (dir.path().join("sessions.db"), dir.path().to_path_buf());
    let mut responses = tool_then_answer();
    responses.push(vec![text("On the fork.")]);
    let provider = Scripted::new(responses);
    let agent = agent(provider.clone(), &db);
    let mut session = agent
        .start_session(SessionId::new("s1"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    session.ctx.set_title("Fix the bug");
    agent
        .run(&mut session, vec![Content::text("hi")])
        .await
        .unwrap();
    let before = session.history.clone();
    agent
        .slash_command(&mut session, "fork", "")
        .await
        .unwrap()
        .unwrap();

    let store = session.ctx.store().unwrap().clone();
    let page = store.list(None, None, 10).await.unwrap();
    let fork = page
        .sessions
        .iter()
        .find(|s| s.id != SessionId::new("s1"))
        .expect("the fork is listed");
    assert_eq!(fork.title.as_deref(), Some("fork of Fix the bug"));
    let original = store.session(&SessionId::new("s1")).await.unwrap().unwrap();
    assert_eq!(original.title.as_deref(), Some("Fix the bug"));
    assert_eq!(
        (fork.usage, fork.model.clone()),
        (original.usage, original.model)
    );

    let mut resumed = agent
        .resume_session(&fork.id, &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    // The fork has everything up to /fork and a note to retitle it; the
    // reply with the link stays in the original.
    let mut history = resumed.history.clone();
    let note = history.last_mut().unwrap().content.pop().unwrap();
    assert!(
        matches!(&note, Content::SystemNote { text } if text.contains("a fork of \"Fix the bug\"")),
        "{note:?}"
    );
    assert_eq!(history, before);
    let reply = format!("{:?}", session.history);
    assert!(
        reply.contains(&format!(
            "Forked into [fork of Fix the bug](/session/{})",
            fork.id
        )),
        "{reply}"
    );

    // The note goes to the model with the next prompt.
    agent
        .run(&mut resumed, vec![Content::text("go on")])
        .await
        .unwrap();
    let reqs = provider.requests.lock().unwrap();
    let last = format!("{:?}", reqs.last().unwrap().messages.last().unwrap());
    assert!(
        last.contains("a fork of") && last.contains("go on"),
        "{last}"
    );
}
