# 57. New sessions from the web client, under ~ or /tmp

- Status: Accepted
- Date: 2026-10-18

## Context

The web client could only follow existing sessions. Starting one needs a
working directory, and a phone has no file picker for the daemon's
machine. ACP has no method to list or create directories.

The web client reaches the daemon over the network, so it should not start
sessions anywhere. Zed connects through the unix socket and must keep
working as before, also for projects outside home.

## Decision

- Two custom requests (`rho/src/acp/dirs.rs`):
  - `_rho/dirs { path }` lists the subdirectories of the typed level that
    start with its last part (hidden ones only for a `.` prefix, at most
    50). It answers `{ entries, dir }`, where `dir` is the canonical path
    if the typed path is an allowed directory.
  - `_rho/mkdir { path }` creates one level; its parent must be allowed.
- Allowed means: the canonical path is under the canonical `$HOME` or
  `/tmp`. Canonicalizing first resolves `..` and symlinks. Listings also
  show parents of the roots, so `/` leads to `/home/me`.
- Connections on the TCP listener (the one with a token, where the web
  client connects) get `web_limits`: `session/new` fails with
  `invalid_params` for a cwd or additional directory that is not allowed.
  Socket connections (Zed) are not limited. `_rho/dirs` and `_rho/mkdir`
  always apply the limit.
- The web client has a `/new` page: a path field with autocomplete per
  level, "Create folder" when the typed name does not exist, and "Start",
  which sends `session/new` with the canonical path (attaching compares
  cwds exactly) and opens the session. The last used directory is kept in
  localStorage.

## Consequences

- Other clients on the TCP listener get the same limit.
- `session/load` and `session/resume` from the web still open stored
  sessions outside ~ or /tmp; those were started elsewhere.
- `$HOME` comes from the daemon's environment.
