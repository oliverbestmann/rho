# 13. Escalations need user approval and fail closed without a UI

- Status: Accepted
- Date: 2026-10-09

## Context

Some actions should not be decided by the model alone: widening the
sandbox, mutating git history or remotes, leaving plan mode. rho runs both
with a UI (ACP) and without one (one-shot CLI, tests).

## Decision

- Approvals go through `Context::confirm(tool_call_id, title)`, which ACP
  shows as `session/request_permission` (Allow / Reject once), attached to
  the tool call when the id is known so the question appears next to what
  is being approved.
- **Without a UI, `confirm` returns `false`**, and a failed or dismissed
  request counts as rejected. So does a form without elicitation support
  (`FormReply::Unsupported`).
- Users of this:
  - `RequestAccess` tool: the model asks for read or read-write access to a
    path (canonicalized, so the user sees the real target, not a symlink).
    It asks without the call id, so the request is not attached to the
    tool call.
  - `GitGate`: `git commit|push|tag` in `Bash` or `BackgroundTask` need
    approval, on top of what `AGENTS.md` says.
  - `PlanMode`: `ExitPlanMode` (ADR 0017).
- The user can grant without being asked: `/grant-read <path>` and
  `/grant-write <path>` (`GrantCommands`). The model is told through the
  inbox (ADR 0010): with the next tool results if a run is active,
  otherwise with the next prompt.
- Advice that needs no approval is a plain block with a reason the model
  can act on, e.g. `RipgrepGate` blocks `grep` and asks for `rg`.

## Consequences

- In the CLI, git mutations and sandbox requests are always denied.
- Detection is textual (word-boundary matching on the command), so it can
  be bypassed by a determined model (e.g. via a script); the gates are
  guardrails, the sandbox is the boundary.

Code: `RequestAccess` in `rho/src/tools.rs`, `GrantCommands` in
`rho/src/sandbox.rs`, `agent/src/git_gate.rs`,
`agent/src/ripgrep_gate.rs`, `AcpFrontend::confirm`.
