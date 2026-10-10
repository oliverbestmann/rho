# 21. Background tasks in process groups, reported with `trigger_turn`

- Status: Accepted
- Date: 2026-10-09

## Context

Some commands take longer than a `Bash` call should block (builds, waiting
for a server). The model should be able to start them, keep working, and
hear back when they end, even when the session is idle by then.

## Decision

`BackgroundTasks` (pi's background-task) adds three tools:

- `BackgroundTask { command, timeout }` (1-3600 s) runs `bash -c` in the
  session's sandbox, in its **own process group**, and returns an id at
  once. Output is read continuously; only the last 10 lines are kept.
- When it exits or times out, the user gets a notice and the model a
  message with the id, exit status, command and tail, sent with
  `trigger_turn`, so an idle session starts a run (ADR 0010).
- `BackgroundTaskList` shows running tasks; `BackgroundTaskCancel` kills
  one without a completion message.
- Timeouts, cancels and `session_end` kill the whole process group
  (`kill(-pid, SIGKILL)`), so children of the shell die too.

## Consequences

- Tasks live only as long as the session; nothing survives a restart.
- `GitGate` also checks `BackgroundTask` commands.
- The one-shot CLI does not wait for tasks that finish after the run.
- Unix only (process groups, signals).

Code: `agent/src/background_task.rs`.
