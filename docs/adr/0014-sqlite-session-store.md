# 14. Sessions in SQLite

- Status: Accepted, amended by 0041
- Date: 2026-10-09

## Context

ACP clients can list, load (with replay), resume and close sessions.
Sessions must survive restarts, several rho processes may run at once (one
per editor window), and extensions such as the web view want to read
other sessions. Later full-text search over past sessions is wanted.

## Decision

- One SQLite database, `~/.config/rho/sessions.db` by default
  (`RHO_SESSIONS_DB`). `Options::sessions_db` maps a cwd to a database file,
  so a project could get its own.
- Tables `sessions` and `messages`, `STRICT`, with foreign keys
  (`ON DELETE CASCADE`) and `CHECK` constraints. Schema migrations are a
  list of SQL scripts; `PRAGMA user_version` is the number applied, and a
  newer database than the binary knows is refused.
- WAL mode, `synchronous = NORMAL` and a 5 s busy timeout, so processes
  can share the file. Each session opens its own connection; blocking
  calls run on `spawn_blocking`.
- Messages are stored as their wire JSON (ADR 0005), plus a plain-text
  column (text and tool calls; no thinking, no tool output) for search.
- Session ids are UUIDv7: unique across processes and sorted by creation
  time.
- A session is stored right away on `session/new`, so it can be loaded
  before its first prompt. (Initially it was stored from the first prompt;
  changed in `eeb34b0`.) If this first save fails, `session/new` fails.
- Saving is incremental: only new messages, plus a rewrite of the last
  one if it grew (`push_user` merges). A later failed save is shown once
  and does not stop the session.
- Stored per session: history, title, model, thinking, profile, cwd,
  additional directories, usage (total and last request) and cost. Not
  stored: sandbox grants (ADR 0012), edit diffs (replayed edits show only
  their output), and when the last request was sent, so a resumed session
  counts as cache-cold (ADR 0015).
- Extensions get read access (`ctx.store()`: `session`, `list`,
  `history`); writing is up to rho.
- The one-shot CLI does not store sessions.

## Consequences

- A session is bound to its cwd; resuming from another cwd is an error.
- `rusqlite` with the `bundled` feature, so no system SQLite is needed.
- Compaction keeps old messages (ADR 0015), so the database grows.

Code: `rho/src/store.rs`, `Session::save` in `rho/src/agent.rs`.
