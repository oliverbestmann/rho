//! `Bash`: a shell command in the session's sandbox, its output streamed.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::truncate_to;
use super::{Tool, ToolKind, input_schema, max_output, parse, resolve};

pub const BASH: ToolName = ToolName::from_static("Bash");

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct BashInput {
    command: String,
    /// environment variables to set for the command
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    /// start from an empty environment that keeps only HOME, USER, PATH, LANG and TERM (plus env)
    #[serde(default)]
    clean_env: bool,
    /// working directory, absolute or relative to the session's; default the session's
    cwd: Option<String>,
    /// seconds, default 120
    timeout: Option<f64>,
    /// output cap in bytes; default 50 KB, at most 64 KB
    #[schemars(range(min = 1, max = 65536))]
    max_output: Option<usize>,
    /// what the command does, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

pub struct Bash;

/// Variables a `cleanEnv` command keeps.
const CLEAN_ENV: [&str; 5] = ["HOME", "USER", "PATH", "LANG", "TERM"];

impl Tool for Bash {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Bash for file operations like ls, mv or mkdir."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: BASH,
            description: "Run a shell command in the working directory. Returns stdout, stderr and the exit code. \
                Commands run in a sandbox: they can read system directories and read/write the workspace directories, \
                nothing else unless granted. If something fails with 'Permission denied' on a path outside these, \
                ask for it with RequestAccess.".into(),
            input_schema: input_schema::<BashInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: BashInput = parse(input)?;
        let timeout = Duration::try_from_secs_f64(i.timeout.unwrap_or(120.0))
            .ok()
            .filter(|t| !t.is_zero())
            .context("timeout must be a positive number of seconds")?;
        let max = max_output(i.max_output);
        let cwd = i.cwd.map_or(ctx.cwd().into(), |d| resolve(ctx.cwd(), &d));
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&i.command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so a timeout or cancel can kill what it started.
            .process_group(0)
            .kill_on_drop(true);
        if i.clean_env {
            cmd.env_clear();
            for key in CLEAN_ENV {
                if let Some(value) = std::env::var_os(key) {
                    cmd.env(key, value);
                }
            }
        }
        cmd.envs(&i.env);
        ctx.sandbox_command(&mut cmd)
            .context("setting up the sandbox")?;
        let mut child = cmd.spawn()?;
        // Cancelling the prompt drops this future; take the processes with it.
        let mut group = KillGroup(child.id());
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let done = async {
            let (out, err) = tokio::try_join!(
                read_streamed(stdout, ctx, false, max),
                read_streamed(stderr, ctx, true, max)
            )?;
            std::io::Result::Ok((child.wait().await?, out, err))
        };
        let Ok(done) = tokio::time::timeout(timeout, done).await else {
            bail!("command timed out after {timeout:?}");
        };
        let (status, stdout, stderr) = done?;
        // Finished: processes left in the group closed their output and run
        // on (e.g. `nohup cmd >log &`).
        group.0 = None;
        let mut text = String::from_utf8_lossy(&stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&stderr));
        let text = truncate_to(text, max);
        if !status.success() {
            bail!("{text}\n[{status}]");
        }
        Ok(text.into())
    }
}

/// Reads `stream` to its end and streams it to the user, but keeps only what
/// `truncate_to(_, max)` can use, so a huge output does not pile up in memory.
async fn read_streamed(
    stream: Option<impl AsyncRead + Unpin>,
    ctx: &Context,
    stderr: bool,
    max: usize,
) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let Some(mut stream) = stream else {
        return Ok(kept);
    };
    let (mut chunk, mut pending) = (vec![0; 8192], Vec::new());
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let room = (max + 1).saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..n.min(room)]);
        pending.extend_from_slice(&chunk[..n]);
        let whole = utf8_prefix(&pending);
        if whole > 0 {
            ctx.stream_output(&String::from_utf8_lossy(&pending[..whole]), stderr);
            pending.drain(..whole);
        }
    }
    if !pending.is_empty() {
        ctx.stream_output(&String::from_utf8_lossy(&pending), stderr);
    }
    Ok(kept)
}

/// How much of `bytes` can be shown now: all but a character cut off at
/// the end, which the next read completes.
fn utf8_prefix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Not UTF-8 at all: shown with replacement characters.
        Err(_) => bytes.len(),
    }
}

/// Kills the process group led by this pid when dropped.
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: plain syscall; the group was created by `process_group(0)`.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_ctx as ctx;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn schema_matches_input_struct() {
        assert_eq!(
            Bash.spec().input_schema,
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "env": {
                        "type": "object",
                        "additionalProperties": {"type": "string"},
                        "description": "environment variables to set for the command",
                        "default": {}
                    },
                    "cleanEnv": {
                        "type": "boolean",
                        "description": "start from an empty environment that keeps only HOME, USER, PATH, LANG and TERM (plus env)",
                        "default": false
                    },
                    "cwd": {"type": ["string", "null"], "description": "working directory, absolute or relative to the session's; default the session's"},
                    "timeout": {"type": ["number", "null"], "format": "double", "description": "seconds, default 120"},
                    "maxOutput": {"type": ["integer", "null"], "format": "uint", "minimum": 1, "maximum": 65536, "description": "output cap in bytes; default 50 KB, at most 64 KB"},
                    "description": {"type": "string", "description": "what the command does, at most 10 words; shown to the user as its title"},
                },
                "required": ["command", "description"],
            })
        );
    }

    #[tokio::test]
    async fn bash_runs_in_cwd_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "").unwrap();
        let out = Bash
            .run(json!({"command": "ls"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out.output.trim(), "marker");

        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner"), "").unwrap();
        let out = Bash
            .run(json!({"command": "ls", "cwd": "sub"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out.output.trim(), "inner");

        let err = Bash
            .run(
                json!({"command": "echo oops >&2; exit 3"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("oops") && err.contains('3'), "{err}");

        let err = Bash
            .run(
                json!({"command": "sleep 5", "timeout": 0.1}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn bash_rejects_bad_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        for timeout in [json!(-1), json!(0), json!(1e300)] {
            let input = json!({"command": "true", "timeout": timeout});
            let err = Bash.run(input, &ctx(dir.path())).await.unwrap_err();
            assert!(err.to_string().contains("positive"), "{err}");
        }
    }

    #[tokio::test]
    async fn bash_sets_and_clears_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let run = async |input| Bash.run(input, &ctx(dir.path())).await.unwrap().output;
        let cmd = "echo \"$FOO|${RHO_BASH_TEST-unset}|${PATH:+path}\"";
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var("RHO_BASH_TEST", "x") };
        let out = run(json!({"command": cmd, "env": {"FOO": "bar"}})).await;
        assert!(out.starts_with("bar|x|path"), "{out}");
        let out = run(json!({"command": cmd, "env": {"FOO": "bar"}, "cleanEnv": true})).await;
        assert!(out.starts_with("bar|unset|path"), "{out}");
    }

    /// Whether `pid` is gone (or a zombie, waiting to be reaped).
    #[cfg(target_os = "linux")]
    fn dead(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, s)| s.starts_with('Z')),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_timeout_kills_what_the_command_started() {
        let dir = tempfile::tempdir().unwrap();
        let input = json!({"command": "sleep 30 & echo $! > pid; wait", "timeout": 0.5});
        let err = Bash.run(input, &ctx(dir.path())).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        let pid = pid.trim();
        for _ in 0..50 {
            if dead(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sleep {pid} survived the timeout");
    }

    #[test]
    fn utf8_prefix_holds_back_a_cut_char() {
        let e = "é".as_bytes();
        assert_eq!(utf8_prefix(b"ab"), 2);
        assert_eq!(utf8_prefix(&[b'a', e[0]]), 1);
        assert_eq!(utf8_prefix(&[b'a', 0xff, b'b']), 3);
    }
}
