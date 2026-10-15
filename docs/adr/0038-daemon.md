# 38. One rho daemon, editors connect through `rho acp --daemon`

- Status: Accepted, amended by 0039
- Date: 2026-10-14

## Context

Every editor window starts its own `rho acp`, so every window loads the
extensions (embedding model, MCP clients) again, and a session open in Zed
cannot be followed from acp-ui. ADR 0037 added ACP over WebSocket, which is
the way in for a shared process. Editors still need an agent on stdio.

## Decision

`rho daemon` serves ACP over WebSocket on a Unix socket,
`<config_dir>/daemon/acp.sock`. With `--listen ADDR` it also serves TCP with
the token, as in ADR 0037 (`rho acp --listen` is gone).

- The socket needs no token: its directory is created 0700, so only the
  user can reach it. TCP keeps the token.
- A `try_lock` on `daemon/lock`, held for the daemon's lifetime, keeps it to
  one daemon. The holder removes a stale socket before binding. A second
  daemon prints "already running" and exits.
- `rho acp --daemon` is what the editor runs. It connects to the socket and
  pumps bytes: stdin lines become text frames, frames become stdout lines.
  It does not parse ACP. After stdin ends it sends a close and waits for the
  daemon to drop the connection.
- If nothing answers, the client starts `rho daemon` itself, in its own
  process group (not killed with the editor), stdout and stderr appended to
  `daemon/log`, and retries for 5 s. It passes its `-m`/`-t`. Clients that
  find a running daemon print that their flags are ignored.
- `rho acp` without flags stays in-process.

## Consequences

- Tools run with the environment of the daemon, that is of the editor that
  started it, not of the editor that sent the prompt. Accepted for now; a
  client could later send its environment in `initialize`.
- Sessions still belong to their connection, so this is one process, not
  shared sessions yet. ADR 0039 shares them.
- After a rebuild the old daemon keeps running until it is stopped; there is
  no version check. The log grows without rotation.
