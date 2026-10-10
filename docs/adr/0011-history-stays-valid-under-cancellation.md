# 11. History stays valid when a run is cancelled or fails

- Status: Accepted
- Date: 2026-10-09

## Context

The Messages API rejects histories where roles do not alternate, an
assistant `tool_use` has no matching `tool_result`, or an assistant message
is empty. One invalid history fails every later request of the session,
and sessions are stored and resumed (ADR 0014). ACP cancellation aborts the
run's future at any `await`.

## Decision

Cancellation is done by dropping the future (`Abortable` in ACP,
`kill_on_drop` for child processes), and `Agent::run` is written so that
the history is valid at every point where it can be dropped or fail:

- An assistant turn is appended only after all its tool calls have run,
  together with the user message holding their results.
- Every `tool_use` gets a result, also when a stop was requested (then it
  is "skipped: run stopped").
- Empty assistant content is not appended.
- `push_user` merges into a trailing user message instead of adding a
  second one; a prompt after a cancelled run is merged into the dangling
  user message.
- Prompts are validated before they are added (images, ADR 0019).

## Consequences

- A cancelled turn's partial output (streamed text, tool calls already
  run) is not in the history; side effects of tools that ran are.
- Saving after each step (`Session::save`) persists only valid histories.

Code: `Agent::run`, `push_user` in `rho/src/agent.rs`; prompt handling in
`rho/src/acp.rs`.
