# 51. Per-session extension state in the `Context`

- Status: Accepted
- Date: 2026-10-15
- Amends: ADR 0009

## Context

One extension instance serves every session of the process (ADR 0009), so
extensions that kept something per session held a `HashMap<SessionId, _>`
and removed the entry in `session_end`: `GoCheck` (failures in a row),
`Skills` (skills in context, problems shown), `ProjectContext` (the system
prompt section). Each repeated the same map, lock and cleanup, and a missed
`session_end` leaked the entry.

## Decision

- `Context::state::<T>() -> Arc<T>` returns the session's `T`, created with
  `T::default()` on first use. The slots are keyed by type and live in the
  `Context`, so they go when the session's last `Context` does.
- Extensions use a type private to them (e.g. `struct Failures(AtomicU32)`)
  so no other extension gets the same slot. `T` handles its own mutation
  (atomics, `Mutex`, `OnceLock`).
- `GoCheck`, `Skills` and `ProjectContext` keep their state there and no
  longer have a `session_end` hook.
- `session_end` stays for work that must happen at the end, e.g.
  `BackgroundTasks` kills its processes.

## Consequences

- Per-session state is one line to get and needs no cleanup.
- The state is freed when the last `Context` clone is dropped, which can be
  a little after `session_end` (e.g. an ACP hub entry still holds one).
- Two extensions using the same public type (say `Mutex<u32>`) would share
  a slot; the convention of private types avoids it.

Code: `Context::state` in `rho/src/extension.rs`, `agent/src/go_check.rs`,
`agent/src/skills.rs`, `agent/src/project_context.rs`.
