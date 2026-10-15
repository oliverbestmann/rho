//! Hard gate on `git commit`, `git push` and `git tag` (pi's git-gate): the
//! user has to approve them, on top of what AGENTS.md asks. Read-only git
//! commands pass. Without a UI (CLI) they are blocked. The question is
//! titled with the call's `description`, else the command.

use rho::extension::{Context, Decision, Extension, ToolCall};
use rho::provider::ToolName;

use crate::background_task::BACKGROUND_TASK;

pub struct GitGate;

/// Tools whose `command` input runs in a shell.
const SHELL_TOOLS: [ToolName; 2] = [rho::tools::BASH, BACKGROUND_TASK];
const MUTATIONS: [&str; 3] = ["commit", "push", "tag"];
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
        let title = call.input["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(command);
        if ctx.confirm(Some(&call.id), title).await {
            Decision::Allow
        } else {
            Decision::Block("Git operation not approved by the user".into())
        }
    }
}

#[cfg(test)]
mod tests {
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
        fn event(&self, _: rho::agent::AgentEvent<'_>) {}

        fn confirm<'a>(
            &'a self,
            _id: Option<&'a ToolCallId>,
            title: &'a str,
        ) -> BoxFuture<'a, bool> {
            self.0.lock().unwrap().push(title.into());
            Box::pin(async { true })
        }
    }

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("t1"),
            name: ToolName::new(name),
            input,
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
        for (name, input, decision) in [
            ("Bash", json!({"command": "git push"}), Decision::Allow),
            ("Bash", json!({"command": "git status"}), Decision::Allow),
            ("Write", json!({"command": "git push"}), Decision::Allow),
            (
                "Bash",
                json!({"command": "git tag v1", "description": " Tag v1 "}),
                Decision::Allow,
            ),
            (
                "Bash",
                json!({"command": "git tag v2", "description": ""}),
                Decision::Allow,
            ),
        ] {
            let mut c = call(name, input);
            assert_eq!(GitGate.tool_call(&ctx, &mut c).await, decision);
        }
        // The description titles the question, else the command.
        assert_eq!(
            *approve.0.lock().unwrap(),
            ["git push", "Tag v1", "git tag v2"]
        );

        // No UI: blocked.
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            Arc::new(NoFrontend),
            Default::default(),
        );
        let mut c = call("BackgroundTask", json!({"command": "git commit -m x"}));
        assert!(matches!(
            GitGate.tool_call(&ctx, &mut c).await,
            Decision::Block(_)
        ));
    }
}
