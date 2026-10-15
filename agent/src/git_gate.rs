//! Hard gate on `git commit`, `git push` and `git tag` (pi's git-gate): the
//! user has to approve them, on top of what AGENTS.md asks. Read-only git
//! commands pass. Without a UI (CLI) they are blocked. Before the question, a
//! summary of the command by a cheap model (the first allowed one that
//! answers, from any provider; else the session's model) is shown as agent
//! text.

use rho::agent::AgentEvent;
use rho::extension::{Context, Decision, Extension, ToolCall};
use rho::provider::ToolName;

use crate::background_task::BACKGROUND_TASK;

pub struct GitGate;

/// Tools whose `command` input runs in a shell.
const SHELL_TOOLS: [ToolName; 2] = [rho::tools::BASH, BACKGROUND_TASK];
const MUTATIONS: [&str; 3] = ["commit", "push", "tag"];
/// Model ids, tried in order at every provider that lists them (and the
/// session's `models.json` rules allow); a failed request tries the next.
const SUMMARY_MODELS: [&str; 2] = ["claude-haiku-5-5", "gpt-6-luna"];
const SUMMARY_PROMPT: &str = "Say in about 10 to 15 words what this shell command does to the git repository. Name only what matters: what is committed, pushed or tagged, and where. No details on staging or output, no quotes, no markdown. Examples: \"Commits all changes locally.\" \"Pushes main to origin.\" \"Tags HEAD as v1.2 and pushes the tag.\"";

/// The first summary a model in `SUMMARY_MODELS`, or else the session's
/// model, gives, if any.
async fn summarize(ctx: &Context, command: &str) -> Option<String> {
    let listed = ctx.registry().models();
    let mut models: Vec<_> = SUMMARY_MODELS
        .iter()
        .flat_map(|id| listed.iter().filter(move |(m, _)| m.id().as_str() == *id))
        .map(|(m, _)| m.clone())
        .collect();
    models.extend(ctx.session_info().model);
    for model in models {
        if let Ok(text) = ctx.complete(&model, SUMMARY_PROMPT, command).await
            && !text.trim().is_empty()
        {
            return Some(text.trim().to_string());
        }
    }
    None
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Same as the regexes `\bgit\s+(commit|push|tag)\b`, case-insensitive.
fn mutates_git(command: &str) -> bool {
    let lower = command.to_lowercase();
    lower.match_indices("git").any(|(at, _)| {
        if lower[..at].chars().next_back().is_some_and(is_word) {
            return false;
        }
        let rest = &lower[at + 3..];
        let args = rest.trim_start();
        if args.len() == rest.len() {
            return false; // no whitespace after `git`
        }
        let word: String = args.chars().take_while(|c| is_word(*c)).collect();
        MUTATIONS.contains(&word.as_str())
    })
}

impl Extension for GitGate {
    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        if !SHELL_TOOLS.contains(&call.name) {
            return Decision::Allow;
        }
        let command = call.input["command"].as_str().unwrap_or_default();
        if !mutates_git(command) {
            return Decision::Allow;
        }
        // Shown like an agent message, not sent to the model. The blank line
        // keeps the model's next text from running into it.
        if let Some(summary) = summarize(ctx, command).await {
            ctx.emit(AgentEvent::Text(&format!(
                "{}\n\n",
                summary.replace('\n', " ")
            )));
        }
        if ctx.confirm(Some(&call.id), command).await {
            Decision::Allow
        } else {
            Decision::Block("Git operation not approved by the user".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use rho::provider::ProviderName;
    use rho::provider::ToolCallId;
    use std::sync::{Arc, Mutex};

    use super::*;
    use rho::extension::{BoxFuture, Frontend, NoFrontend};
    use rho::store::SessionId;
    use serde_json::json;

    #[test]
    fn detects_git_mutations() {
        for cmd in [
            "git commit -m x",
            "git  push origin main",
            "cd x && GIT TAG v1",
            "git\tcommit",
            "(git push)",
            "git commit-tree",
        ] {
            assert!(mutates_git(cmd), "{cmd}");
        }
        for cmd in [
            "git status",
            "git log --grep commit",
            "gitcommit",
            "legit commit",
            "git committed",
            "git-push",
            "git",
            "echo 'git tagged'",
        ] {
            assert!(!mutates_git(cmd), "{cmd}");
        }
    }

    struct Approve(Mutex<Vec<String>>);

    impl Frontend for Approve {
        fn event(&self, event: AgentEvent<'_>) {
            if let AgentEvent::Text(t) = event {
                self.0.lock().unwrap().push(format!("text: {t}"));
            }
        }

        fn confirm<'a>(
            &'a self,
            _id: Option<&'a ToolCallId>,
            title: &'a str,
        ) -> BoxFuture<'a, bool> {
            self.0.lock().unwrap().push(title.into());
            Box::pin(async { true })
        }
    }

    /// Lists `gpt-6-luna`, which always answers `.0`.
    struct Says(&'static str);

    impl rho::provider::Provider for Says {
        fn stream(
            &self,
            _: rho::provider::Request,
            _: &dyn Fn(&mut serde_json::Value),
        ) -> futures::stream::BoxStream<'static, anyhow::Result<rho::provider::Event>> {
            let text = rho::provider::Event::TextDelta(self.0.into());
            Box::pin(futures::stream::iter([Ok(text)]))
        }

        fn models(&self) -> Vec<rho::provider::ModelInfo> {
            vec![rho::provider::ModelInfo {
                id: rho::provider::ModelId::new("gpt-6-luna"),
                name: "GPT-6 Luna".into(),
                context_window: 1_000_000,
                price: None,
                price_above: None,
            }]
        }
    }

    fn call(name: &str, command: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("t1"),
            name: ToolName::new(name),
            input: json!({ "command": command }),
        }
    }

    #[tokio::test]
    async fn asks_before_mutations() {
        let approve = Arc::new(Approve(Mutex::default()));
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            approve.clone(),
            Default::default(),
        );
        let mut c = call("Bash", "git push");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        let mut c = call("Bash", "git status");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        let mut c = call("Write", "git push");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        assert_eq!(*approve.0.lock().unwrap(), ["git push"]);

        // With a summary from the first model that is available.
        let mut registry = rho::provider::Registry::default();
        registry.register(
            ProviderName::new("github-copilot"),
            Arc::new(Says("Pushes main to origin.")),
        );
        // A path that forbids it: no summary.
        let only = rho::models::Pattern::try_from("anthropic/*".to_string()).unwrap();
        let restricted = registry.restricted(Some(Arc::new([only])));
        let ctx = Context::new(SessionId::new("s"), "/tmp", approve.clone(), restricted);
        let mut c = call("Bash", "git tag v1");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        assert_eq!(
            approve.0.lock().unwrap().pop().as_deref(),
            Some("git tag v1")
        );

        let ctx = Context::new(SessionId::new("s"), "/tmp", approve.clone(), registry);
        let mut c = call("Bash", "git push origin main");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        // The summary as text, then the plain command to approve.
        assert_eq!(
            approve.0.lock().unwrap()[1..],
            ["text: Pushes main to origin.\n\n", "git push origin main"]
        );

        // Neither is allowed here: the session's model summarizes.
        let mut agent = rho::agent::Agent::new(
            Arc::new(Says("Tags v1.")),
            rho::provider::ModelSpec::new("github-copilot", "main"),
            vec![],
        );
        agent.registry.register(
            ProviderName::new("github-copilot"),
            Arc::new(Says("Tags v1.")),
        );
        agent.models = rho::models::Config::parse(
            r#"{"rules": [{"path": "/", "allow": ["github-copilot/main"]}]}"#,
            "models.json".as_ref(),
        )
        .unwrap();
        let session = agent
            .start_session(SessionId::new("s"), "/tmp", vec![], approve.clone())
            .await
            .unwrap();
        let mut c = call("Bash", "git tag v1");
        assert_eq!(
            GitGate.tool_call(&session.ctx, &mut c).await,
            Decision::Allow
        );
        assert_eq!(
            approve.0.lock().unwrap()[3..],
            ["text: Tags v1.\n\n", "git tag v1"]
        );

        // No UI: blocked.
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            Arc::new(NoFrontend),
            Default::default(),
        );
        let mut c = call("BackgroundTask", "git commit -m x");
        assert!(matches!(
            GitGate.tool_call(&ctx, &mut c).await,
            Decision::Block(_)
        ));
    }
}
