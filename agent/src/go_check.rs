//! After the agent settles in a Go module, runs `go test` and `go vet`. On
//! failure the output goes back to the model and the run continues, at most
//! `MAX_FAILURES` times in a row per prompt (pi's go-check has no limit).
//!
//! The checks run the model's code, so they run in the session's sandbox,
//! like Bash: the module cache and `GOCACHE` must be writable (`~/go`,
//! `~/.cache` in `sandbox.json`). Not in a read-only profile: nothing could
//! have changed there, and `go` could not write its cache.

use std::collections::HashMap;
use std::sync::Mutex;

use rho::extension::{Context, Extension, PromptSetup, Settle};

/// Failed checks in a row (per session) before giving back to the user.
pub const MAX_FAILURES: u32 = 3;

#[derive(Default)]
pub struct GoCheck {
    /// Failures in a row, by session id.
    failures: Mutex<HashMap<String, u32>>,
}

const CHECKS: [[&str; 2]; 2] = [["test", "./..."], ["vet", "./..."]];

impl Extension for GoCheck {
    /// A new prompt means the user is back in the loop: allow retries again.
    async fn before_prompt(&self, ctx: &Context, _prompt: &mut PromptSetup) {
        self.failures.lock().unwrap().remove(ctx.session_id());
    }

    fn session_end(&self, ctx: &Context) {
        self.failures.lock().unwrap().remove(ctx.session_id());
    }

    async fn before_settle(&self, ctx: &Context, settle: &Settle<'_>) -> Option<String> {
        // Skip pure Q&A runs: only check after tools that may have changed files.
        let changed = settle.tools_called.iter().any(|t| t != "Read");
        if !changed || ctx.profile().read_only || !ctx.cwd().join("go.mod").exists() {
            return None;
        }

        let mut results = Vec::new();
        let mut failed = false;
        for args in CHECKS {
            let label = format!("go {}", args.join(" "));
            let mut cmd = tokio::process::Command::new("go");
            cmd.args(args)
                .current_dir(ctx.cwd())
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            let out = match ctx.sandbox_command(&mut cmd) {
                Ok(()) => cmd.output().await,
                Err(e) => Err(std::io::Error::other(format!(
                    "setting up the sandbox: {e:#}"
                ))),
            };
            match out {
                Ok(o) if o.status.success() => results.push(format!("✅ {label}")),
                Ok(o) => {
                    failed = true;
                    let text =
                        String::from_utf8_lossy(&o.stdout) + String::from_utf8_lossy(&o.stderr);
                    results.push(format!("❌ {label}\n{}", text.trim()));
                }
                Err(e) => {
                    failed = true;
                    results.push(format!("❌ {label}\n{e}"));
                }
            }
        }

        let summary = results.join("\n\n");
        ctx.notify("go test ./... && go vet ./...", &summary, failed);

        let failures = {
            let mut map = self.failures.lock().unwrap();
            let n = map.entry(ctx.session_id().to_string()).or_default();
            *n = if failed { *n + 1 } else { 0 };
            *n
        };
        if !failed {
            return None;
        }
        if failures > MAX_FAILURES {
            ctx.notify(
                "go check",
                &format!("Still failing after {MAX_FAILURES} attempts; leaving it to you."),
                true,
            );
            return None;
        }
        Some(format!(
            "Automatic `go test ./...` / `go vet ./...` check after your last change failed:\n\n{summary}\n\nPlease fix."
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use rho::extension::{NoFrontend, Profile};
    use rho::provider::StopReason;

    /// Sandboxed with the base policy (system dirs and `dir`).
    fn sandboxed(dir: &std::path::Path) -> Context {
        Context::new("t", dir, Arc::new(NoFrontend), Default::default())
    }

    /// Unsandboxed: the test machine's Go cache may be anywhere.
    fn ctx(dir: &std::path::Path) -> Context {
        let ctx = sandboxed(dir);
        ctx.set_sandbox(None);
        ctx
    }

    async fn check_in(go: &GoCheck, ctx: &Context, tools: &[&str]) -> Option<String> {
        let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
        let settle = Settle {
            stop: StopReason::EndTurn,
            history: &[],
            tools_called: &tools,
        };
        go.before_settle(ctx, &settle).await
    }

    async fn check(go: &GoCheck, dir: &std::path::Path, tools: &[&str]) -> Option<String> {
        check_in(go, &ctx(dir), tools).await
    }

    fn go_installed() -> bool {
        let ok = std::process::Command::new("go")
            .arg("version")
            .output()
            .is_ok();
        if !ok {
            eprintln!("go not installed, skipping");
        }
        ok
    }

    fn passing_module(dir: &std::path::Path) {
        std::fs::write(dir.join("go.mod"), "module example.com/m\n\ngo 1.21\n").unwrap();
        std::fs::write(
            dir.join("m.go"),
            "package m\n\nfunc One() int { return 1 }\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn skipped_in_read_only_profiles() {
        let dir = tempfile::tempdir().unwrap();
        passing_module(dir.path());
        let ctx = ctx(dir.path());
        ctx.set_profiles(vec![Profile {
            read_only: true,
            ..Profile::write()
        }]);
        // Would fail without go, or run it; neither happens.
        assert_eq!(check_in(&GoCheck::default(), &ctx, &["Bash"]).await, None);
    }

    #[tokio::test]
    async fn runs_in_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        passing_module(dir.path());
        if !go_installed() {
            return;
        }
        // The base policy does not grant the Go caches, so a module that
        // passes unsandboxed fails here.
        let msg = check_in(&GoCheck::default(), &sandboxed(dir.path()), &["Write"]).await;
        assert!(msg.is_some_and(|m| m.contains("❌")));
    }

    #[tokio::test]
    async fn continues_on_failure_with_a_retry_limit() {
        let go = GoCheck::default();
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // Not a Go module: nothing to do.
        assert_eq!(check(&go, dir, &["Write"]).await, None);

        if !go_installed() {
            return;
        }
        let w = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();
        w("go.mod", "module example.com/m\n\ngo 1.21\n");
        w(
            "m.go",
            "package m\n\nfunc Add(a, b int) int { return a - b }\n",
        );
        w(
            "m_test.go",
            "package m\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(1, 2) != 3 {\n\t\tt.Fatal(\"bad add\")\n\t}\n}\n",
        );
        // Read-only runs are not checked.
        assert_eq!(check(&go, dir, &["Read"]).await, None);

        let msg = check(&go, dir, &["Write"]).await.expect("should continue");
        assert!(
            msg.contains("bad add") && msg.contains("Please fix"),
            "{msg}"
        );

        // Gives up after MAX_FAILURES in a row ...
        for _ in 1..MAX_FAILURES {
            assert!(check(&go, dir, &["Write"]).await.is_some());
        }
        assert_eq!(check(&go, dir, &["Write"]).await, None);
        // ... until the user prompts again.
        let mut prompt = PromptSetup {
            content: vec![],
            system: String::new(),
        };
        go.before_prompt(&ctx(dir), &mut prompt).await;
        assert!(check(&go, dir, &["Write"]).await.is_some());

        // Passing resets the count too.
        w(
            "m.go",
            "package m\n\nfunc Add(a, b int) int { return a + b }\n",
        );
        assert_eq!(check(&go, dir, &["Write"]).await, None);
        assert_eq!(go.failures.lock().unwrap()["t"], 0);
    }
}
