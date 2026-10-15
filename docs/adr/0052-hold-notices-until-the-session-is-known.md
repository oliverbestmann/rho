# 52. Hold a starting session's notices until the client knows it

- Status: Accepted
- Date: 2026-10-15
- Amends: ADR 0031, ADR 0036, ADR 0039

## Context

A client learns a new session's id from the `session/new` response.
Updates sent before it (e.g. a notice from a `session_start` hook) carry an
id the client does not know yet, and clients drop them. So `Mcp` kept its
config and connect problems in a map by session id and showed them with
the first prompt, `Skills` had a `problems_shown` flag for the same reason,
and the "Additional directories" notice was sent after the response by
hand.

## Decision

- An ACP viewer attached while its session starts (`session/new`, or a
  stored session that `session/load` or `session/resume` opens) holds its
  updates. `AcpFrontend::ready`, called after the response, sends them.
  Replayed history (`To::Only`) is not held: `session/load` must replay
  before it answers.
- Such a viewer is also not ready for questions until then. Permission
  requests and forms wait for the response, so `session_start` hooks must
  not ask: the response waits for them, and that would deadlock.
  `ProjectContext` keeps asking with the first prompt.
- `Mcp` and `Skills` notify their problems in `session_start`. The
  additional directories notice is sent before the response like any other.

## Consequences

- Hooks can notify at any time; the frontend handles the order.
- On `session/load` of a stored session, `session_start` notices come
  after the replayed history, not before it.
- A hook that asks in `session_start` hangs the session start.

Code: `Viewer::held`, `AcpFrontend::ready` and `deliver` in
`rho/src/acp/frontend.rs`, the `session/new` handler and `open_stored` in
`rho/src/acp/mod.rs`, `agent/src/mcp.rs`, `agent/src/skills.rs`.
