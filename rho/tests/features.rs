//! Features: gated tools, extra sandbox access, telling the model, the
//! `/enable` and `/disable` commands, and saving which are on.

use std::sync::Arc;

use rho::agent::Agent;
use rho::extension::NoFrontend;
use rho::feature::{Feature, FeatureCommands, FeatureId};
use rho::provider::{Content, Event, ModelSpec, Request, StopReason, ToolCallId, ToolName};
use rho::store::SessionId;
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

/// Texts and tool results of the last (user) message.
fn last_user(r: &Request) -> Vec<String> {
    let content = &r.messages.last().unwrap().content;
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            Content::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn features_unlock_tools_and_sandbox_until_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let (cwd, other) = (dir.path().join("w"), dir.path().join("other"));
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&other).unwrap();
    let touch = json!({"command": format!("touch {}/x", other.display())});
    let scripted = Scripted::new(vec![
        vec![
            tool_use("w1", "Write", json!({"path": "a", "content": "x"})),
            tool_use("b1", "Bash", touch.clone()),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("blocked"), Event::Done(StopReason::EndTurn)],
        vec![
            tool_use("b2", "Bash", touch),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("done"), Event::Done(StopReason::EndTurn)],
        vec![text("off"), Event::Done(StopReason::EndTurn)],
    ]);
    let mut agent = Agent::new(
        scripted.clone(),
        ModelSpec::new("test", "m"),
        rho::tools::builtin(),
    );
    agent.extensions.push(Arc::new(FeatureCommands));
    agent.features = vec![Feature {
        id: FeatureId::new("files"),
        description: "write files".into(),
        enabled: false,
        read: vec![],
        read_write: vec![other.clone()],
        tools: vec![ToolName::new("Write")],
        instructions: Some("Be careful.".into()),
    }];
    let db = dir.path().join("sessions.db");
    agent.sessions_db = Some(Arc::new(move |_| db.clone()));
    let mut session = agent
        .start_session(SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();

    agent
        .run(&mut session, vec![Content::text("go")])
        .await
        .unwrap();
    let enable = agent.slash_command(&mut session, "enable", "files nope");
    assert!(enable.await.is_some_and(|r| r.is_ok()));
    assert!(
        !session
            .ctx
            .set_feature(&FeatureId::new("files"), true)
            .unwrap()
    );
    agent
        .run(&mut session, vec![Content::text("again")])
        .await
        .unwrap();

    {
        let requests = scripted.requests.lock().unwrap();
        let first = last_user(&requests[0]).join("\n");
        assert!(first.contains("calls fail: Write."), "{first}");
        let results = last_user(&requests[1]);
        assert!(results[0].contains("needs one of these features, which are off: files"));
        assert!(results[1].contains("Permission denied"), "{results:?}");
        let note = last_user(&requests[2]).join("\n");
        assert!(
            note.contains("The files feature is on (write files)"),
            "{note}"
        );
        assert!(
            note.contains(&format!("allows writing {}", other.display())),
            "{note}"
        );
        assert!(note.contains("call these tools now: Write.") && note.contains("Be careful."));
        assert!(other.join("x").exists());
    }

    // Saved with the session, and the model is not told again.
    session.save().await;
    drop(session);
    let mut session = agent
        .resume_session(&SessionId::new("s"), &cwd, vec![], Arc::new(NoFrontend))
        .await
        .unwrap();
    assert_eq!(session.ctx.active_features(), [FeatureId::new("files")]);
    let disable = agent.slash_command(&mut session, "disable", "files");
    assert!(disable.await.is_some_and(|r| r.is_ok()));
    agent
        .run(&mut session, vec![Content::text("stop")])
        .await
        .unwrap();
    let requests = scripted.requests.lock().unwrap();
    let note = last_user(requests.last().unwrap()).join("\n");
    assert!(
        note.contains("The files feature is off now.") && note.contains("calls fail: Write."),
        "{note}"
    );
    assert!(
        session
            .ctx
            .effective_sandbox()
            .unwrap()
            .rules
            .iter()
            .all(|r| r.path != other)
    );
}
