//! Hard gate on `git commit`, `git push` and `git tag` (pi's git-gate): the
//! user has to approve them, on top of what AGENTS.md asks. Read-only git
//! commands pass. Without a UI (CLI) they are blocked. The question comes
//! with a summary of the command by a cheap model, the first that answers.

use rho::extension::{Context, Decision, Extension, ToolCall};

pub struct GitGate;

/// Tools whose `command` input runs in a shell.
const SHELL_TOOLS: [&str; 2] = ["Bash", "BackgroundTask"];
const MUTATIONS: [&str; 3] = ["commit", "push", "tag"];
/// Tried in order; a missing provider or a failed request tries the next.
const SUMMARY_MODELS: [&str; 2] = ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna"];
const SUMMARY_PROMPT: &str = "Say in about 10 to 15 words what this shell command does to the git repository. Name only what matters: what is committed, pushed or tagged, and where. No details on staging or output, no quotes, no markdown. Examples: \"Commits all changes locally.\" \"Pushes main to origin.\" \"Tags HEAD as v1.2 and pushes the tag.\"";

/// The first summary a model in `SUMMARY_MODELS` gives, if any.
async fn summarize(ctx: &Context, command: &str) -> Option<String> {
    for model in SUMMARY_MODELS {
        if let Ok(text) = ctx.complete(model, SUMMARY_PROMPT, command).await
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
        if !SHELL_TOOLS.contains(&call.name.as_str()) {
            return Decision::Allow;
        }
        let command = call.input["command"].as_str().unwrap_or_default();
        if !mutates_git(command) {
            return Decision::Allow;
        }
        let title = match summarize(ctx, command).await {
            // Zed renders the title as inline markdown and shows the
            // command with the tool call already.
            Some(summary) => format!("Allow this git operation? **{summary}**"),
            None => format!("Allow this git operation? `{command}`"),
        };
        if ctx.confirm(Some(&call.id), &title).await {
            Decision::Allow
        } else {
            Decision::Block("Git operation not approved by the user".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use rho::agent::AgentEvent;
    use rho::extension::{BoxFuture, Frontend, NoFrontend};
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
        fn event(&self, _event: AgentEvent<'_>) {}

        fn confirm<'a>(&'a self, _id: Option<&'a str>, title: &'a str) -> BoxFuture<'a, bool> {
            self.0.lock().unwrap().push(title.into());
            Box::pin(async { true })
        }
    }

    /// A model that always answers `.0`.
    struct Says(&'static str);

    impl rho::provider::Provider for Says {
        fn stream(
            &self,
            _: rho::provider::Request,
        ) -> futures::stream::BoxStream<'static, anyhow::Result<rho::provider::Event>> {
            let text = rho::provider::Event::TextDelta(self.0.into());
            Box::pin(futures::stream::iter([Ok(text)]))
        }
    }

    fn call(name: &str, command: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: json!({ "command": command }),
        }
    }

    #[tokio::test]
    async fn asks_before_mutations() {
        let approve = Arc::new(Approve(Mutex::default()));
        let ctx = Context::new("s", "/tmp", approve.clone(), Default::default());
        let mut c = call("Bash", "git push");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        let mut c = call("Bash", "git status");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        let mut c = call("Write", "git push");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        assert_eq!(
            *approve.0.lock().unwrap(),
            ["Allow this git operation? `git push`"]
        );

        // With a summary from the first model that is available.
        let mut registry = rho::provider::Registry::default();
        registry.register("openai", Arc::new(Says("Pushes main to origin.")));
        let ctx = Context::new("s", "/tmp", approve.clone(), registry);
        let mut c = call("Bash", "git push origin main");
        assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        assert_eq!(
            approve.0.lock().unwrap().last().unwrap(),
            "Allow this git operation? **Pushes main to origin.**"
        );

        // No UI: blocked.
        let ctx = Context::new("s", "/tmp", Arc::new(NoFrontend), Default::default());
        let mut c = call("BackgroundTask", "git commit -m x");
        assert!(matches!(
            GitGate.tool_call(&ctx, &mut c).await,
            Decision::Block(_)
        ));
    }
}
