# 65. Sessions join others through aliases

- Status: Accepted
- Date: 2026-10-27
- Amends: ADR 0039

## Context

A session started in the web client could not be continued in Zed. A Zed
thread can always run slash commands in a session it started itself.

## Decision

- `/sessions:list` (an agent command) shows the 10 sessions updated last,
  without aliases and the current session: id, title and cwd.
- `/session:join <id>` (handled by the ACP frontend, before a turn) makes
  the current session N an alias of session O. `<id>` may be a start of
  O's id that no other session has (aliases of O do not count):
  - The store sets `sessions.alias_of` of N (and of N's own aliases) to O,
    or to what O stands for, so an alias never points at an alias.
  - The hub drops N's open session and maps N to O. Every hub method
    resolves the client's id first.
  - The connection becomes a viewer of O that knows it as N: viewers carry
    their own session id, and updates, permission requests and forms go
    out under it. It gets `_meta.rho.reset` and O's whole transcript, then
    O's config options and commands.
  - It fails while N runs or has queued prompts, and from a session that
    is an alias already.
- `session/load` and `session/resume` of an alias open its session, in that
  session's cwd and directories, whatever the client sends.
- `session/list` leaves aliases out. `Store::list` keeps them: History
  still indexes and finds N's own messages.

## Consequences

- One `/session:join` per Zed thread continues any session there, also
  after a restart, also from another cwd.
- Other connections that viewed N lose it until they load it again.
- A connection views a session under one id: viewing O and an alias of O
  on one connection keeps only the later.
- The alias is looked up in the database of the cwd the client sends; with
  more than one database (`sessions_db` by cwd) the target must be in it.

Code: `Store::join`, `rho/src/acp/join.rs`, `Hub::alias`, `joined` in
`rho/src/acp/load.rs`, `list` in `rho/src/agent/commands.rs`.
