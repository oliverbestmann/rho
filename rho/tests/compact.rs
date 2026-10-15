use std::sync::Arc;
use std::time::{Duration, Instant};

use rho::agent::{Agent, Session};
use rho::provider::{Content, Event, ModelSpec, ProviderName, Role, ToolCallId, ToolName};
use serde_json::json;

mod common;
use common::{Scripted, text};

struct Run {
    main: Arc<Scripted>,
    cheap: Arc<Scripted>,
    session: Session,
}

/// Runs one prompt with a tool call, then compacts as if the last request
/// was `cache_age` ago.
async fn compact_after(cache_age: Option<Duration>) -> Run {
    let tool_use = Event::Block(Content::ToolUse {
        id: ToolCallId::new("t1"),
        name: ToolName::new("Bash"),
        input: json!({"command": "echo $((40+2))"}),
    });
    let main = Scripted::new(vec![
        vec![tool_use],
        vec![text("done")],
        // Only used on the warm path.
        vec![text("## Goal\nwarm summary")],
    ]);
    let cheap = Scripted::new(vec![vec![text("## Goal\ncold summary")]]);
    let mut agent = Agent::new(
        main.clone(),
        ModelSpec::new("test", "big"),
        rho::tools::builtin(),
    );
    agent
        .registry
        .register(ProviderName::new("cheap"), cheap.clone());
    // The `models.json` compaction model wins over the global one.
    agent.compact_model = Some(ModelSpec::new("cheap", "global"));
    agent.models = rho::models::Config::parse(
        r#"{"rules": [{"path": "/", "compact": "cheap/small"}]}"#,
        "models.json".as_ref(),
    )
    .unwrap();

    let cwd = std::env::current_dir().unwrap();
    let (mut session, _log) = common::session(&agent, &cwd).await;
    agent
        .run(&mut session, vec![Content::text("do it")])
        .await
        .unwrap();
    session.last_request = cache_age.map(|age| Instant::now() - age);
    agent.compact(&mut session, "focus on X").await.unwrap();
    Run {
        main,
        cheap,
        session,
    }
}

fn only_text(content: &[Content]) -> &str {
    let Some(Content::Text { text }) = content.last() else {
        panic!("{content:?}")
    };
    text
}

#[tokio::test]
async fn warm_cache_compacts_with_the_session_model_and_same_prefix() {
    let Run {
        main,
        cheap,
        session,
    } = compact_after(Some(Duration::from_secs(10))).await;
    assert_eq!(session.history.len(), 1);
    assert_eq!(session.history[0].role, Role::User);
    assert!(only_text(&session.history[0].content).contains("<summary>\n## Goal\nwarm summary"));
    assert_eq!(session.last_request, None);
    assert!(cheap.requests.lock().unwrap().is_empty());

    // The compaction request extends the last run's request, so it reads the cache.
    let reqs = main.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    let (last_run, compact) = (&reqs[1], &reqs[2]);
    assert_eq!(compact.model.as_str(), "big");
    assert_eq!(compact.system, last_run.system);
    assert_eq!(compact.tools, last_run.tools);
    assert_eq!(compact.thinking, last_run.thinking);
    let n = last_run.messages.len();
    assert_eq!(compact.messages[..n], last_run.messages[..]);
    assert_eq!(
        compact.messages.len(),
        n + 2,
        "assistant answer + instructions"
    );
    assert!(only_text(&compact.messages[n + 1].content).contains("Additional focus: focus on X"));
}

#[tokio::test]
async fn cold_cache_uses_the_cheap_model_without_tool_outputs() {
    for age in [None, Some(Duration::from_secs(600))] {
        let Run {
            main,
            cheap,
            session,
        } = compact_after(age).await;
        assert!(only_text(&session.history[0].content).contains("cold summary"));
        assert_eq!(
            main.requests.lock().unwrap().len(),
            2,
            "no request to the big model"
        );

        let reqs = cheap.requests.lock().unwrap();
        let req = &reqs[0];
        assert_eq!(req.model.as_str(), "small");
        assert!(req.tools.is_empty());
        let text = only_text(&req.messages[0].content);
        assert!(text.contains("[User]: do it"), "{text}");
        assert!(
            text.contains("Bash({\"command\":\"echo $((40+2))\"})"),
            "{text}"
        );
        assert!(text.contains("[Assistant]: done"), "{text}");
        assert!(!text.contains("42"), "tool output leaked: {text}");
        assert!(text.ends_with("Additional focus: focus on X"), "{text}");
    }
}
