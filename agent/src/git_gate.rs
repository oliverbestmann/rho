//! Hard gate on `git commit`, `git push` and `git tag` (pi's git-gate): the
//! user has to approve them, on top of what AGENTS.md asks. Read-only git
//! commands pass. Without a UI (CLI) they are blocked. The question is
//! titled with the call's `description`, else the command. One approval,
//! or `/git:allow`, allows all git mutations for the rest of the session.

use std::sync::atomic::{AtomicBool, Ordering};

use rho::extension::{
    CommandName, Context, Decision, Extension, SessionSetup, SlashCommand, ToolCall,
};
use rho::provider::ToolName;

use crate::background_task::BACKGROUND_TASK;

pub struct GitGate;

const ALLOW: CommandName = CommandName::from_static("git:allow");

/// Set once the user allowed git mutations in this session.
#[derive(Default)]
struct Allowed(AtomicBool);

/// Tools whose `command` input runs in a shell.
const SHELL_TOOLS: [ToolName; 2] = [rho::tools::BASH, BACKGROUND_TASK];
const MUTATIONS: [&str; 3] = ["commit", "push", "tag"];
/// Options of `git` itself that take the next word as value (lowercased).
const VALUE_OPTIONS: [&str; 5] = [
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Like the regex `\bgit\s+(commit|push|tag)\b`, case-insensitive, but
/// skipping git's own options (`git -C dir push`, `git -c k=v commit`).
fn mutates_git(command: &str) -> bool {
    let lower = command.to_lowercase();
    lower.match_indices("git").any(|(at, _)| {
        if lower[..at].chars().next_back().is_some_and(is_word) {
            return false;
        }
        let rest = &lower[at + 3..];
        if !rest.starts_with(char::is_whitespace) {
            return false;
        }
        // shortcut: a quoted option value with spaces is taken for the
        // subcommand; split shell words properly if that gets used to slip by.
        let mut words = rest.split_whitespace();
        while let Some(word) = words.next() {
            if VALUE_OPTIONS.contains(&word) {
                words.next();
            } else if !word.starts_with('-') {
                let sub: String = word.chars().take_while(|c| is_word(*c)).collect();
                return MUTATIONS.contains(&sub.as_str());
            }
        }
        false
    })
}

impl Extension for GitGate {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.commands.push(SlashCommand {
            name: ALLOW,
            description: "Allow git commit, push and tag for the rest of the session".into(),
            hint: None,
        });
    }

    async fn command(&self, ctx: &Context, name: &CommandName, _args: &str) -> Option<String> {
        if *name != ALLOW {
            return None;
        }
        ctx.state::<Allowed>().0.store(true, Ordering::Relaxed);
        Some("Git commit, push and tag are allowed for the rest of the session.".into())
    }

    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        if !SHELL_TOOLS.contains(&call.name) {
            return Decision::Allow;
        }
        let command = call.input["command"].as_str().unwrap_or_default();
        if !mutates_git(command) {
            return Decision::Allow;
        }
        let allowed = ctx.state::<Allowed>();
        if allowed.0.load(Ordering::Relaxed) {
            return Decision::Allow;
        }
        let title = call.input["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(command);
        if ctx.confirm(Some(&call.id), title).await {
            allowed.0.store(true, Ordering::Relaxed);
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
            "git -c commit.gpgsign=false commit -m x",
            "git -C /repo push",
            "git --no-pager --git-dir /x/.git tag v1",
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
            "git -C /repo status",
            "git -c core.pager=cat log",
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

    fn ctx(frontend: Arc<dyn Frontend>) -> Context {
        Context::new(SessionId::new("s"), "/tmp", frontend, Default::default())
    }

    #[tokio::test]
    async fn asks_once_before_mutations() {
        let approve = Arc::new(Approve(Mutex::default()));
        let ctx = ctx(approve.clone());
        for (name, command) in [
            ("Bash", "git push"),
            ("Bash", "git status"),
            ("Write", "git push"),
            // Allowed by the first approval.
            ("BackgroundTask", "git tag v1"),
        ] {
            let mut c = call(name, json!({"command": command}));
            assert_eq!(GitGate.tool_call(&ctx, &mut c).await, Decision::Allow);
        }
        assert_eq!(*approve.0.lock().unwrap(), ["git push"]);
    }

    #[tokio::test]
    async fn titles_with_description_else_command() {
        let approve = Arc::new(Approve(Mutex::default()));
        for input in [
            json!({"command": "git tag v1", "description": " Tag v1 "}),
            json!({"command": "git tag v2", "description": ""}),
        ] {
            let mut c = call("Bash", input);
            GitGate.tool_call(&ctx(approve.clone()), &mut c).await;
        }
        assert_eq!(*approve.0.lock().unwrap(), ["Tag v1", "git tag v2"]);
    }

    #[tokio::test]
    async fn blocks_without_approval_until_allowed() {
        // No UI: blocked, every time.
        let ctx = ctx(Arc::new(NoFrontend));
        let commit = || call("BackgroundTask", json!({"command": "git commit -m x"}));
        for _ in 0..2 {
            assert!(matches!(
                GitGate.tool_call(&ctx, &mut commit()).await,
                Decision::Block(_)
            ));
        }
        assert!(GitGate.command(&ctx, &ALLOW, "").await.is_some());
        assert_eq!(
            GitGate.tool_call(&ctx, &mut commit()).await,
            Decision::Allow
        );
        let other = CommandName::new("other");
        assert!(GitGate.command(&ctx, &other, "").await.is_none());
    }
}
