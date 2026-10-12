//! Profiles: the tools the model gets, the read-only sandbox, telling the
//! model about a switch, saving the profile, and the ACP selector.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    InitializeRequest, NewSessionRequest, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionValue, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
};
use agent_client_protocol::{Client, ConnectionTo};
use anyhow::Result;
use rho::agent::Agent;
use rho::extension::{
    Context, Extension, NoFrontend, Profile, ProfileId, SessionSetup, ToolOutput,
};
use rho::provider::{
    Content, Event, ModelSpec, Request, StopReason, ToolCallId, ToolName, ToolSpec,
};
use rho::store::SessionId;
use rho::tools::Tool;
use serde_json::{Value, json};

mod common;
use common::{Scripted, text};

/// Adds a read-only "look" profile without `Write`.
struct Look;

impl Extension for Look {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.profiles.push(Profile {
            id: ProfileId::new("look"),
            name: "Look".into(),
            description: "read only".into(),
            hidden_tools: vec![ToolName::new("Write")],
            read_only: true,
            instructions: Some("Only look.".into()),
        });
        session.tools.push(Arc::new(Switch));
    }
}

/// Switches to the profile in its `to` input.
struct Switch;

impl Tool for Switch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::new("Switch"),
            description: "switch".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        ctx.set_profile(&ProfileId::new(input["to"].as_str().unwrap_or_default()))?;
        Ok("switched".into())
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> Event {
    Event::Block(Content::ToolUse {
        id: ToolCallId::new(id),
        name: ToolName::new(name),
        input,
    })
}

fn tool_names(r: &Request) -> Vec<String> {
    r.tools.iter().map(|t| t.name.to_string()).collect()
}

/// Texts of the last message, which is the user's.
fn last_texts(r: &Request) -> Vec<String> {
    r.messages
        .last()
        .unwrap()
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_results(r: &Request) -> Vec<(String, bool)> {
    r.messages
        .last()
        .unwrap()
        .content
        .iter()
        .filter_map(|c| match c {
            Content::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn agent(scripted: Arc<Scripted>, db: Option<std::path::PathBuf>) -> Agent {
    let mut agent = Agent::new(scripted, ModelSpec::new("test", "m"), rho::tools::builtin());
    agent.extensions.push(Arc::new(Look));
    if let Some(db) = db {
        agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    }
    agent
}

#[tokio::test]
async fn switching_changes_tools_and_sandbox_and_tells_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("w");
    std::fs::create_dir(&cwd).unwrap();
    let scripted = Scripted::new(vec![
        vec![
            tool_use("s1", "Switch", json!({"to": "look"})),
            Event::Done(StopReason::ToolUse),
        ],
        vec![
            tool_use("w1", "Write", json!({"path": "a", "content": "x"})),
            tool_use("b1", "Bash", json!({"command": "touch b"})),
            tool_use("b2", "Bash", json!({"command": "ls"})),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("looked"), Event::Done(StopReason::EndTurn)],
        vec![text("writing"), Event::Done(StopReason::EndTurn)],
    ]);
    let agent = agent(scripted.clone(), Some(dir.path().join("sessions.db")));
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    agent
        .run(&mut session, vec![Content::text("look around")])
        .await
        .unwrap();

    {
        let requests = scripted.requests.lock().unwrap();
        // Write hides nothing: no note on the first request.
        assert_eq!(last_texts(&requests[0]), ["look around"]);
        // The tools stay the same, so the prompt cache does too.
        assert!(tool_names(&requests[0]).contains(&"Write".to_string()));
        assert_eq!(tool_names(&requests[0]), tool_names(&requests[1]));
        // The switch applies to the next request of the same run.
        let note = last_texts(&requests[1]).join("\n");
        assert!(note.contains("switched to the Look profile"), "{note}");
        assert!(note.contains("calls fail: Write."), "{note}");
        assert!(note.contains("read-only"), "{note}");
        assert!(note.contains("Only look."), "{note}");

        let results = tool_results(&requests[2]);
        assert_eq!(
            results[0],
            ("Write is not available in the Look profile".into(), true)
        );
        assert!(results[1].1 && results[1].0.contains("Permission denied"));
        assert!(!results[2].1, "reading still works: {:?}", results[2]);
        assert!(!cwd.join("a").exists() && !cwd.join("b").exists());
        // No second note.
        assert!(last_texts(&requests[2]).is_empty());
    }

    session.save().await;
    drop(session);
    let mut session = agent
        .resume_session(&SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert_eq!(session.ctx.profile().id, ProfileId::new("look"));

    // The user switches back while idle; the next prompt carries the note.
    session.ctx.set_profile(&Profile::WRITE).unwrap();
    assert!(session.ctx.set_profile(&ProfileId::new("nope")).is_err());
    agent
        .run(&mut session, vec![Content::text("now write")])
        .await
        .unwrap();
    let requests = scripted.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert!(tool_names(last).contains(&"Write".to_string()));
    let texts = last_texts(last);
    assert_eq!(texts[0], "now write");
    assert!(
        texts[1].contains("call these tools now: Write.") && texts[1].contains("allows writing"),
        "{texts:?}"
    );
}

#[tokio::test]
async fn a_restricting_first_profile_is_announced_on_the_first_request() {
    struct Narrow;
    impl Extension for Narrow {
        async fn session_start(&self, session: &mut SessionSetup) {
            session.profiles[0].hidden_tools.push(ToolName::new("Bash"));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let scripted = Scripted::new(vec![
        vec![text("a"), Event::Done(StopReason::EndTurn)],
        vec![text("b"), Event::Done(StopReason::EndTurn)],
    ]);
    let mut agent = Agent::new(
        scripted.clone(),
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    );
    agent.extensions.push(Arc::new(Narrow));
    let mut session = agent
        .start_session(
            SessionId::new("s"),
            dir.path(),
            vec![],
            Arc::new(NoFrontend),
        )
        .await
        .unwrap();
    for prompt in ["one", "two"] {
        agent
            .run(&mut session, vec![Content::text(prompt)])
            .await
            .unwrap();
    }
    let requests = scripted.requests.lock().unwrap();
    let first = last_texts(&requests[0]);
    assert!(
        first[1].contains("in the Write profile") && first[1].contains("calls fail: Bash."),
        "{first:?}"
    );
    assert!(tool_names(&requests[0]).contains(&"Bash".to_string()));
    assert_eq!(last_texts(&requests[1]), ["two"]);
}

fn profile_option(options: &[SessionConfigOption]) -> (String, Vec<String>) {
    let option = options.iter().find(|o| &*o.id.0 == "mode").expect("mode");
    let SessionConfigKind::Select(select) = &option.kind else {
        panic!("not a select");
    };
    let agent_client_protocol::schema::v1::SessionConfigSelectOptions::Ungrouped(values) =
        &select.options
    else {
        panic!("grouped");
    };
    (
        select.current_value.0.to_string(),
        values.iter().map(|v| v.value.0.to_string()).collect(),
    )
}

#[tokio::test]
async fn acp_offers_a_profile_selector_and_follows_switches() {
    let scripted = Scripted::new(vec![
        vec![
            tool_use("s1", "Switch", json!({"to": "look"})),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("ok"), Event::Done(StopReason::EndTurn)],
    ]);
    let agent = Arc::new(agent(scripted, None));
    let updates = Arc::new(Mutex::new(vec![]));
    let record = updates.clone();

    let client = Client
        .builder()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if let SessionUpdate::ConfigOptionUpdate(u) = n.update {
                    record.lock().unwrap().push(u.config_options);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                .block_task()
                .await?;
            let sid = session.session_id;
            let options = session.config_options.unwrap();
            assert_eq!(
                profile_option(&options),
                ("write".into(), vec!["write".into(), "look".into()])
            );

            // The model switches: the client gets all options again.
            cx.send_request(rho_prompt(&sid, "go")).block_task().await?;
            // The user switches back.
            let set = SetSessionConfigOptionRequest::new(
                sid.clone(),
                "mode",
                SessionConfigOptionValue::from("write"),
            );
            let options = cx.send_request(set).block_task().await?.config_options;
            assert_eq!(profile_option(&options).0, "write");
            assert!(options.iter().any(|o| &*o.id.0 == "thought_level"));
            let bad = SetSessionConfigOptionRequest::new(
                sid,
                "mode",
                SessionConfigOptionValue::from("x"),
            );
            assert!(cx.send_request(bad).block_task().await.is_err());
            Ok(())
        });
    tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .unwrap()
        .unwrap();

    let updates = updates.lock().unwrap();
    let current: Vec<String> = updates.iter().map(|o| profile_option(o).0).collect();
    assert_eq!(current, ["look", "write"]);
    assert!(updates[0].iter().any(|o| &*o.id.0 == "thought_level"));
}

fn rho_prompt(
    sid: &agent_client_protocol::schema::v1::SessionId,
    text: &str,
) -> agent_client_protocol::schema::v1::PromptRequest {
    agent_client_protocol::schema::v1::PromptRequest::new(
        sid.clone(),
        vec![agent_client_protocol::schema::v1::ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new(text),
        )],
    )
}

#[tokio::test]
async fn sessions_without_profiles_have_no_selector() {
    let agent = Arc::new(Agent::new(
        Scripted::new(vec![]),
        ModelSpec::new("test", "m"),
        vec![],
    ));
    let client =
        Client
            .builder()
            .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                    .block_task()
                    .await?;
                let options = session.config_options.unwrap();
                assert!(options.iter().all(|o| &*o.id.0 != "mode"));
                Ok(())
            });
    tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .unwrap()
        .unwrap();
}
