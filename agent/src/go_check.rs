//! After the agent settles in a Go module, runs `go test` and `go vet`. On
//! failure the output goes back to the model and the run continues, at most
//! `MAX_FAILURES` times in a row per prompt (pi's go-check has no limit).
//! The model gets the end of the output, as an automated message.
//!
//! The checks run the model's code, so they run in the session's sandbox,
//! like Bash: the module cache and `GOCACHE` must be writable (`~/go`,
//! `~/.cache` in `sandbox.json`). Not in a read-only profile: nothing could
//! have changed there, and `go` could not write its cache.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rho::extension::{
    CommandName, Context, Extension, PromptSetup, SessionSetup, Settle, SlashCommand,
};

/// Failed checks in a row (per session) before giving back to the user.
pub const MAX_FAILURES: u32 = 1;
/// Each check is killed after this.
const TIMEOUT: Duration = Duration::from_secs(300);
/// How much of a failed check's output the model gets, from the end, where
/// `go test` puts the failures and the summary.
const TAIL_LINES: usize = 50;
const TAIL_BYTES: usize = 50 * 1024;

/// The last `TAIL_LINES` lines of `text`, at most `TAIL_BYTES`, marked if cut.
fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text
        .rmatch_indices('\n')
        .nth(TAIL_LINES - 1)
        .map_or(0, |(i, _)| i + 1)
        .max(text.ceil_char_boundary(text.len().saturating_sub(TAIL_BYTES)));
    if start == 0 {
        return text.into();
    }
    format!("[earlier output cut]\n{}", &text[start..])
}

pub struct GoCheck {
    /// The `go` binary; tests use a mock.
    go: PathBuf,
}

/// Failed checks in a row, in the session's `Context::state`.
#[derive(Default)]
struct Failures(AtomicU32);

/// `Context::stored` key, `true` while `/go:check` turned the check off.
const OFF: &str = "go-check.off";

const TOGGLE: CommandName = CommandName::from_static("go:check");

impl Default for GoCheck {
    fn default() -> Self {
        Self { go: "go".into() }
    }
}

const CHECKS: [[&str; 2]; 2] = [["test", "./..."], ["vet", "./..."]];

impl Extension for GoCheck {
    async fn session_start(&self, session: &mut SessionSetup) {
        if session.ctx.cwd().join("go.mod").exists() {
            session.commands.push(SlashCommand {
                name: TOGGLE,
                description: "Turn the go test / go vet check at the end of a turn off or on"
                    .into(),
                hint: None,
            });
        }
    }

    async fn command(&self, ctx: &Context, name: &CommandName, _args: &str) -> Option<String> {
        if *name != TOGGLE {
            return None;
        }
        let was_off = ctx.stored::<bool>(OFF).unwrap_or(false);
        ctx.set_stored(OFF, !was_off);
        Some(format!(
            "The go check at the end of a turn is {} for this session.",
            if was_off { "on" } else { "off" }
        ))
    }

    /// A new prompt means the user is back in the loop: allow retries again.
    async fn before_prompt(&self, ctx: &Context, _prompt: &mut PromptSetup) {
        ctx.state::<Failures>().0.store(0, Ordering::Relaxed);
    }

    async fn before_settle(&self, ctx: &Context, settle: &Settle<'_>) -> Option<String> {
        // Skip pure Q&A runs: only check after tools that may have changed files.
        let changed = settle.tools_called.iter().any(|t| *t != rho::tools::READ);
        if !changed
            || ctx.stored::<bool>(OFF).unwrap_or(false)
            || ctx.profile().read_only
            || !ctx.cwd().join("go.mod").exists()
        {
            return None;
        }

        let mut results = Vec::new();
        let mut failed = false;
        for args in CHECKS {
            let label = format!("go {}", args.join(" "));
            let mut cmd = tokio::process::Command::new(&self.go);
            cmd.args(args)
                .current_dir(ctx.cwd())
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            // shortcut: kill_on_drop kills `go`, not test binaries it started;
            // kill the process group like Bash if they linger.
            let out = match ctx.sandbox_command(&mut cmd) {
                Ok(()) => tokio::time::timeout(TIMEOUT, cmd.output())
                    .await
                    .unwrap_or_else(|_| {
                        Err(std::io::Error::other(format!(
                            "timed out after {}s",
                            TIMEOUT.as_secs()
                        )))
                    }),
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
                    results.push(format!("❌ {label}\n{}", tail(&text)));
                }
                Err(e) => {
                    failed = true;
                    results.push(format!("❌ {label}\n{e}"));
                }
            }
        }

        let summary = results.join("\n\n");
        ctx.notify("go test ./... && go vet ./...", &summary, failed);

        let count = &ctx.state::<Failures>().0;
        if !failed {
            count.store(0, Ordering::Relaxed);
            return None;
        }
        let failures = count.fetch_add(1, Ordering::Relaxed) + 1;
        if failures > MAX_FAILURES {
            ctx.notify(
                "go check",
                "Still failing after the model's fix; leaving it to you.",
                true,
            );
            return None;
        }
        Some(format!(
            "<system-reminder>\nThis is not a message from the user. When your turn ended, rho ran \
             `go test ./...` and `go vet ./...` automatically, and they failed:\n\n{summary}\n\n\
             Please fix the failures. The checks run once more when you end your turn.\n</system-reminder>"
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rho::provider::ToolName;
    use std::sync::Arc;

    use super::*;
    use rho::extension::{NoFrontend, Profile};
    use rho::provider::StopReason;
    use rho::store::SessionId;

    /// Sandboxed with the base policy (system dirs and `dir`).
    fn sandboxed(dir: &std::path::Path) -> Context {
        Context::new(
            SessionId::new("t"),
            dir,
            Arc::new(NoFrontend),
            Default::default(),
        )
    }

    /// Unsandboxed: the test machine's Go cache may be anywhere.
    fn ctx(dir: &std::path::Path) -> Context {
        let ctx = sandboxed(dir);
        ctx.set_sandbox(None);
        ctx
    }

    async fn check_in(go: &GoCheck, ctx: &Context, tools: &[&str]) -> Option<String> {
        let tools: Vec<ToolName> = tools.iter().map(|t| ToolName::new(*t)).collect();
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

    /// An executable `go` in `dir` running the shell script `body`.
    pub(crate) fn mock_go(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("go");
        std::fs::write(
            &path,
            format!("#!/bin/sh\n[ \"$1\" = __probe ] && exit 0\n{body}"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A process forked by another test thread may still hold our write fd
        // until it execs, so running the script fails with ETXTBSY. Our fd is
        // closed now, so once one exec works it keeps working.
        loop {
            match std::process::Command::new(&path).arg("__probe").status() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(1))
                }
                status => break assert!(status.unwrap().success()),
            }
        }
        path
    }

    fn passing_module(dir: &std::path::Path) {
        std::fs::write(dir.join("go.mod"), "module example.com/m\n\ngo 1.21\n").unwrap();
        std::fs::write(
            dir.join("m.go"),
            "package m\n\nfunc One() int { return 1 }\n",
        )
        .unwrap();
    }

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail("\na\nb\n"), "a\nb");
        let lines: Vec<String> = (1..=60).map(|i| i.to_string()).collect();
        let out = tail(&lines.join("\n"));
        assert!(out.starts_with("[earlier output cut]\n11\n") && out.ends_with("\n60"));
        let out = tail(&"é".repeat(TAIL_BYTES));
        assert_eq!(out.len(), "[earlier output cut]\n".len() + TAIL_BYTES);
    }

    #[tokio::test]
    async fn command_toggles_the_check() {
        let bin = tempfile::tempdir().unwrap();
        let go = GoCheck {
            go: mock_go(bin.path(), "exit 1\n"),
        };
        let dir = tempfile::tempdir().unwrap();
        passing_module(dir.path());
        let ctx = ctx(dir.path());
        let toggle = async || go.command(&ctx, &TOGGLE, "").await.unwrap();
        assert!(toggle().await.contains("off"));
        assert_eq!(check_in(&go, &ctx, &["Write"]).await, None);
        assert!(toggle().await.contains("on"));
        assert!(check_in(&go, &ctx, &["Write"]).await.is_some());
        let other = CommandName::new("other");
        assert!(go.command(&ctx, &other, "").await.is_none());
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
        let outside = tempfile::tempdir().unwrap();
        passing_module(dir.path());
        let probe = outside.path().join("probe");
        let go = GoCheck {
            go: mock_go(dir.path(), &format!("touch '{}'\n", probe.display())),
        };
        // The base policy grants only the cwd, so writing elsewhere fails.
        let msg = check_in(&go, &sandboxed(dir.path()), &["Write"]).await;
        assert!(msg.is_some_and(|m| m.contains("❌")));
        assert!(!probe.exists());
        // Unsandboxed it passes.
        assert_eq!(check(&go, dir.path(), &["Write"]).await, None);
        assert!(probe.exists());
    }

    #[tokio::test]
    async fn continues_on_failure_with_a_retry_limit() {
        let bin = tempfile::tempdir().unwrap();
        // Fails with the content of `fail` in the cwd, if there is one.
        let go = GoCheck {
            go: mock_go(
                bin.path(),
                "echo \"$@\" >> calls\n[ -f fail ] && cat fail && exit 1\nexit 0\n",
            ),
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let ctx = ctx(dir);
        // Not a Go module: nothing to do.
        assert_eq!(check_in(&go, &ctx, &["Write"]).await, None);

        let w = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();
        w("go.mod", "module example.com/m\n\ngo 1.21\n");
        w("fail", "--- FAIL: TestAdd\n    bad add\n");
        // Read-only runs are not checked.
        assert_eq!(check_in(&go, &ctx, &["Read"]).await, None);
        assert!(!dir.join("calls").exists());

        let msg = check_in(&go, &ctx, &["Write"])
            .await
            .expect("should continue");
        assert!(
            msg.contains("❌ go test ./...")
                && msg.contains("bad add")
                && msg.contains("Please fix"),
            "{msg}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("calls")).unwrap(),
            "test ./...\nvet ./...\n"
        );

        // Gives up when it fails again ...
        assert_eq!(check_in(&go, &ctx, &["Write"]).await, None);
        // ... until the user prompts again.
        let mut prompt = PromptSetup {
            content: vec![],
            system: String::new(),
        };
        go.before_prompt(&ctx, &mut prompt).await;
        assert!(check_in(&go, &ctx, &["Write"]).await.is_some());

        // Passing resets the count too.
        std::fs::remove_file(dir.join("fail")).unwrap();
        assert_eq!(check_in(&go, &ctx, &["Write"]).await, None);
        assert_eq!(ctx.state::<Failures>().0.load(Ordering::Relaxed), 0);
    }
}
