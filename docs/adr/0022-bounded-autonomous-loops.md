# 22. Bound the agent's autonomous loops

- Status: Accepted
- Date: 2026-10-09

## Context

Several mechanisms keep a run going without the user: tool calls, messages
from `before_settle` hooks, triggered messages. pi has no turn limit, and
its go-check retries forever. A model that cannot fix a failing test can
then burn quota indefinitely.

## Decision

- `Agent::max_turns` (default 50) caps the model requests per run; the run
  then ends with `StopReason::MaxTurns` (ACP: `max_turn_requests`).
- `GoCheck` (runs `go test ./...` and `go vet ./...` after a run in a Go
  module that used a tool other than `Read`) continues the run with the
  failures at most `MAX_FAILURES = 3` times in a row per session. Then it
  tells the user it gives up. The count resets on a passing check and in
  `before_prompt`, i.e. on a new prompt but also on a run started by a
  triggered message (e.g. a finished background task).
- Tool output is bounded: `Bash`, `Read`, `Grep` and `GoDoc` cut to 50 KB
  (`tools::truncate`, `tools::MAX_OUTPUT`); `Memory` cuts to 50 KB too
  (ADR 0030; it used to cut to 2000 lines and save the full JSON to a
  temp file); `FetchUrl` cuts to
  64,000 characters by default (`maxLength`, set by the model);
  background tasks keep 10 lines.
- `Bash` has a default timeout of 120 s (the model can set another);
  background tasks at most 3600 s. Each model request has
  `max_tokens = 32000`.

## Consequences

- Long legitimate runs can hit 50 turns; the user continues with a new
  prompt.
- Extensions that continue runs should bring their own limits, like
  `GoCheck`.

Code: `Agent::run` in `rho/src/agent.rs`, `agent/src/go_check.rs`,
`rho/src/tools.rs`.
