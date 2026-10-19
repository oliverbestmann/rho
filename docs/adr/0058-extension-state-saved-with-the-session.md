# 58. Extension state saved with the session

- Status: Accepted
- Date: 2026-10-19
- Amends: ADR 0051

## Context

`Context::state` (ADR 0051) lives in memory. A session is set up again,
with a new `Context`, whenever it is resumed: after a restart, or when the
ACP hub dropped it because no client looked at it (switching sessions,
reloading the web tab, a reconnect). Switches like `/go:check` and
`/git:allow` were lost then, often without the user noticing.

## Decision

- `Context::set_stored(key, value)` keeps a JSON value under a string key;
  `Context::stored::<T>(key)` reads it (`None` if missing or not a `T`).
  Keys are strings, not types: type names are not stable in a database.
  Extensions prefix them with their name (`go-check.off`).
- The values are saved with the session state, in the `extension_state`
  column of `sessions` (a JSON object), and restored in `resume_session`.
- The session is saved after an extension's slash command too, so a switch
  is kept even if no run follows.
- `GoCheck` keeps `/go:check` there and `GitGate` its approval (one
  approval or `/git:allow`). `state` stays for what may be lost (failure
  counters, running tasks, caches).

## Consequences

- An approval in `GitGate` now lasts for the stored session, across
  restarts. Sandbox grants are still not stored (ADR 0012).
- Sessions saved before this change resume with no values: the go check
  is on and git mutations ask again.
- `/redact` does not look at `extension_state`.

Code: `Context::stored` / `set_stored` in `rho/src/extension.rs`,
`rho/src/store.rs`, `rho/src/agent/persist.rs`, `agent/src/go_check.rs`,
`agent/src/git_gate.rs`.
