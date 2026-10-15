# 49. Extension helpers use the session's compaction model

- Status: Accepted
- Date: 2026-10-15
- Amends: ADR 0032, ADR 0040

## Context

Two extensions make one-off model calls with `Context::complete`: `GitGate`
summarizes git commands and `History` answers `summarizeOver` searches.
`History` always asked `anthropic/claude-haiku-5-5`, which fails in paths
that only allow Copilot. `GitGate` had its own list of model ids to try.
`models.json` already names a cheap model per path: `compact` (ADR 0040).

## Decision

- `SessionInfo` carries the session's `compact_model`, and
  `SessionInfo::cheap_model` returns it, else the session model.
- `GitGate` and `History` use `cheap_model`. `GitGate`'s model list and
  `History::with_model` are gone.
- Without any model (a `Context` outside an agent session) `GitGate` shows
  no summary and `History` shows the plain results.
- (Since 2026-10-15 `GitGate` makes no model call: it titles the question
  with the `Bash` call's `description`, else the command.)

## Consequences

- One place decides which cheap model a path uses; it is allowed there by
  the startup check (ADR 0040).
- Without a `compact` rule or `RHO_COMPACT_MODEL`, the helpers use the
  session model, which costs more per call.

Code: `SessionInfo::cheap_model` in `rho/src/extension.rs`,
`agent/src/git_gate.rs`, `agent/src/history.rs`.
