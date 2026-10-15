# 42. Clients learn that a turn runs from `_meta.rho.running`

- Status: Accepted
- Date: 2026-10-15

## Context

With shared sessions (ADR 0039) a turn can be started by another client,
by an extension (`trigger_turn`), or still run when a client opens the
session. A client only knows a turn runs while its own `session/prompt`
waits for its answer. Between tool calls, while the model streams, other
clients saw an idle session. ACP has no update that says a turn runs.

## Decision

rho sends it as its own metadata, like the usage details (`_meta.rho` of
`usage_update`). See `busy_update` in `rho/src/acp/update.rs`.

- `Hub::start_run` and `Run::drop` call `AcpFrontend::set_busy` when the
  session's run list becomes non-empty or empty. It sends a
  `session_info_update` with only `_meta: {"rho": {"running": true|false}}`
  to all viewers. This covers prompts from any connection and
  `watch_inbox` runs. A prompt that waits for the session lock already
  counts.
- A viewer that gets ready (ADR 0039) during a turn gets `running: true`.
- `session/list` sets `_meta.rho.running: true` on sessions with a run.
  Idle sessions get no `_meta`.

## Consequences

- Other ACP clients ignore it. A `session_info_update` without `title` or
  `updatedAt` changes nothing for them.
- `session/list` is a snapshot. A list that stays open does not update.
- The web client (ADR 0043) still treats its own pending prompt as busy,
  so the bar shows before the first update arrives.
