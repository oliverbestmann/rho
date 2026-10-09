//! Long shell commands in the background (pi's background-task). The model
//! gets a message with the exit status and the last output lines when a
//! task ends, which starts a new run if the session is idle. Tasks run in
//! the session's sandbox and are killed when the session ends.

use std::collections::{HashMap, VecDeque};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::ToolSpec;
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

const TAIL_LINES: usize = 10;
const MAX_TIMEOUT: u64 = 3600;

#[derive(Default)]
pub struct BackgroundTasks {
    sessions: Mutex<HashMap<String, Arc<Tasks>>>,
}

/// One session's tasks.
#[derive(Default)]
struct Tasks {
    next_id: AtomicU32,
    running: Mutex<HashMap<String, Task>>,
}

struct Task {
    command: String,
    started: Instant,
    pid: i32,
    tail: Arc<Mutex<VecDeque<String>>>,
}

/// Kills the whole process group, so children of `bash -c` die too.
fn kill(task: &Task) {
    // SAFETY: plain syscall; the group was created by `process_group(0)`.
    unsafe { libc::kill(-task.pid, libc::SIGKILL) };
}

impl Extension for BackgroundTasks {
    async fn session_start(&self, session: &mut SessionSetup) {
        let tasks = Arc::new(Tasks::default());
        self.sessions
            .lock()
            .unwrap()
            .insert(session.ctx.session_id().into(), tasks.clone());
        session.tools.push(Arc::new(Start(tasks.clone())));
        session.tools.push(Arc::new(List(tasks.clone())));
        session.tools.push(Arc::new(Cancel(tasks)));
    }

    fn session_end(&self, ctx: &Context) {
        if let Some(tasks) = self.sessions.lock().unwrap().remove(ctx.session_id()) {
            for (_, task) in tasks.running.lock().unwrap().drain() {
                kill(&task);
            }
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct StartInput {
    /// Shell command, run with bash -c
    command: String,
    /// Timeout in seconds (max 3600), the task is killed after this
    #[schemars(range(min = 1, max = 3600))]
    timeout: u64,
}

struct Start(Arc<Tasks>);

impl Tool for Start {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "BackgroundTask".into(),
            description: format!(
                "Run a shell command in the background (e.g. a long build, or a while loop waiting for a process). Returns a task id immediately. When the command exits or times out, you get a message with the id, exit status and the last {TAIL_LINES} output lines. Sandboxed like Bash."
            ),
            input_schema: input_schema::<StartInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: StartInput = parse(input)?;
        if !(1..=MAX_TIMEOUT).contains(&i.timeout) {
            bail!("timeout must be between 1 and {MAX_TIMEOUT} seconds");
        }
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&i.command)
            .current_dir(ctx.cwd())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        ctx.sandbox_command(&mut cmd)?;
        let mut child = cmd.spawn()?;

        let id = (self.0.next_id.fetch_add(1, Ordering::Relaxed) + 1).to_string();
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let task = Task {
            command: i.command.clone(),
            started: Instant::now(),
            pid: child.id().unwrap_or_default() as i32,
            tail: tail.clone(),
        };
        let pid = task.pid;
        self.0.running.lock().unwrap().insert(id.clone(), task);

        let tasks = self.0.clone();
        let ctx = ctx.clone();
        let started = format!("Started background task {id}");
        tokio::spawn(async move {
            let (out, err) = (child.stdout.take(), child.stderr.take());
            let done = async {
                tokio::join!(collect(out, &tail), collect(err, &tail));
                child.wait().await
            };
            let status = match tokio::time::timeout(Duration::from_secs(i.timeout), done).await {
                Ok(Ok(status)) => describe(status),
                Ok(Err(e)) => format!("failed: {e}"),
                Err(_) => {
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                    let _ = child.wait().await;
                    format!("timed out after {}s", i.timeout)
                }
            };
            // Cancelled: stay silent.
            if tasks.running.lock().unwrap().remove(&id).is_none() {
                return;
            }
            let lines = Vec::from(std::mem::take(&mut *tail.lock().unwrap())).join("\n");
            let lines = if lines.is_empty() {
                "(no output)".into()
            } else {
                lines
            };
            let failed = status != "exited with code 0";
            ctx.notify(&format!("background task {id} {status}"), &lines, failed);
            ctx.send(
                format!(
                    "Background task {id} {status}.\nCommand: {}\nLast {TAIL_LINES} lines:\n{lines}",
                    i.command
                ),
                true,
            );
        });
        Ok(started.into())
    }
}

/// Keeps the last `TAIL_LINES` lines of `stream`.
async fn collect(stream: Option<impl AsyncRead + Unpin>, tail: &Mutex<VecDeque<String>>) {
    let Some(stream) = stream else { return };
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    while let Ok(n) = reader.read_until(b'\n', &mut buf).await
        && n > 0
    {
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches('\n')
            .to_string();
        let mut tail = tail.lock().unwrap();
        tail.push_back(line);
        if tail.len() > TAIL_LINES {
            tail.pop_front();
        }
        buf.clear();
    }
}

fn describe(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with code {code}"),
        (None, Some(sig)) => format!("killed by signal {sig}"),
        (None, None) => "ended".into(),
    }
}

#[derive(Deserialize, JsonSchema)]
struct ListInput {}

struct List(Arc<Tasks>);

impl Tool for List {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "BackgroundTaskList".into(),
            description:
                "List running background tasks with id, runtime, command and last output line."
                    .into(),
            input_schema: input_schema::<ListInput>(),
        }
    }

    async fn run(&self, _input: Value, _ctx: &Context) -> Result<ToolOutput> {
        let running = self.0.running.lock().unwrap();
        let mut ids: Vec<_> = running.keys().collect();
        ids.sort_by_key(|id| id.parse::<u32>().unwrap_or_default());
        let lines: Vec<String> = ids
            .into_iter()
            .map(|id| {
                let t = &running[id];
                let last = t.tail.lock().unwrap().back().cloned();
                format!(
                    "{id}  {}s  {}\n    last: {}",
                    t.started.elapsed().as_secs(),
                    t.command,
                    last.as_deref().unwrap_or("(no output)")
                )
            })
            .collect();
        if lines.is_empty() {
            return Ok("No running background tasks".into());
        }
        Ok(lines.join("\n").into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct CancelInput {
    /// Task id
    id: String,
}

struct Cancel(Arc<Tasks>);

impl Tool for Cancel {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "BackgroundTaskCancel".into(),
            description: "Stop a background task by id. No completion message is sent for it."
                .into(),
            input_schema: input_schema::<CancelInput>(),
        }
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        let i: CancelInput = parse(input)?;
        let Some(task) = self.0.running.lock().unwrap().remove(&i.id) else {
            bail!("no running background task {}", i.id);
        };
        kill(&task);
        Ok(format!("Cancelled background task {}", i.id).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::agent::AgentEvent;
    use rho::extension::Frontend;
    use serde_json::json;

    /// Records notices.
    #[derive(Default)]
    struct Notices(Mutex<Vec<(String, String, bool)>>);

    impl Frontend for Notices {
        fn event(&self, event: AgentEvent<'_>) {
            if let AgentEvent::Notice {
                title,
                text,
                is_error,
            } = event
            {
                self.0
                    .lock()
                    .unwrap()
                    .push((title.into(), text.into(), is_error));
            }
        }
    }

    fn setup() -> (tempfile::TempDir, Arc<Notices>, Context, Arc<Tasks>) {
        let dir = tempfile::tempdir().unwrap();
        let notices = Arc::new(Notices::default());
        let ctx = Context::new("s", dir.path(), notices.clone(), Default::default());
        (dir, notices, ctx, Arc::new(Tasks::default()))
    }

    async fn wait_notice(notices: &Notices) -> (String, String, bool) {
        for _ in 0..100 {
            if let Some(n) = notices.0.lock().unwrap().first() {
                return n.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no notice");
    }

    #[tokio::test]
    async fn reports_exit_status_and_tail() {
        let (_dir, notices, ctx, tasks) = setup();
        let cmd = "for i in $(seq 1 12); do echo $i; done; sleep 0.2; echo err >&2; exit 3";
        let out = Start(tasks.clone())
            .run(json!({"command": cmd, "timeout": 10}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.output, "Started background task 1");
        let (title, text, failed) = wait_notice(&notices).await;
        assert_eq!(title, "background task 1 exited with code 3");
        assert!(failed);
        assert!(
            !text.contains("\n2\n") && text.contains("12") && text.contains("err"),
            "{text}"
        );
        assert!(ctx.has_triggered());
        assert!(tasks.running.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn times_out() {
        let (_dir, notices, ctx, tasks) = setup();
        Start(tasks)
            .run(json!({"command": "sleep 30", "timeout": 1}), &ctx)
            .await
            .unwrap();
        let (title, _, failed) = wait_notice(&notices).await;
        assert_eq!(title, "background task 1 timed out after 1s");
        assert!(failed);
    }

    #[tokio::test]
    async fn lists_and_cancels_silently() {
        let (_dir, notices, ctx, tasks) = setup();
        Start(tasks.clone())
            .run(json!({"command": "echo up; sleep 30", "timeout": 60}), &ctx)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let list = List(tasks.clone()).run(json!({}), &ctx).await.unwrap();
        assert!(
            list.output.starts_with("1  ") && list.output.contains("last: up"),
            "{}",
            list.output
        );

        Cancel(tasks.clone())
            .run(json!({"id": "1"}), &ctx)
            .await
            .unwrap();
        let list = List(tasks.clone()).run(json!({}), &ctx).await.unwrap();
        assert_eq!(list.output, "No running background tasks");
        assert!(Cancel(tasks).run(json!({"id": "1"}), &ctx).await.is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(notices.0.lock().unwrap().is_empty());
        assert!(!ctx.has_triggered());
    }
}
