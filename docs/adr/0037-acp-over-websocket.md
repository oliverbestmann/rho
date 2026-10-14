# 37. ACP over WebSocket with a bearer token

- Status: Accepted
- Date: 2026-10-14

## Context

Clients on other devices (acp-ui on the web or a phone) cannot start rho as
a subprocess. They speak ACP over WebSocket: one JSON-RPC message per text
frame, the subprotocol `acp.v1`, and, since browsers cannot set headers, the
token as a `bearer.<token>` subprotocol. Later, one rho process could also
serve several editors, with `rho acp` on stdio only forwarding to it.

## Decision

`rho acp --listen ADDR` serves ACP on `ws://ADDR/` (axum, `rho/src/acp_ws.rs`)
instead of stdio. It lives in the rho crate, because extensions cannot add
subcommands.

- The socket becomes the SDK's `Lines` transport. Text frames are split on
  newlines, because stdio bridges forward several messages per frame.
  Binary frames end the connection.
- Every connection gets its own `acp::agent(..)` on one shared `Arc<Agent>`,
  so connections are as separate as two stdio processes today.
- Every request needs the token, also from localhost: WebSockets have no
  CORS, so any website could otherwise drive the agent. It comes from
  `RHO_ACP_TOKEN`, else `acp-token` in the config dir, created (0600) with
  32 random bytes on first start. It is accepted as `Authorization: Bearer`
  or as a `bearer.` subprotocol. Only `acp.v1` is echoed back.

It replaces the web view (ADR 0020): any ACP client is a better frontend
than a page fed only by hooks, and one way in is easier to secure.

## Consequences

- Following a session that is open in an editor, from another device, is
  gone until one rho serves both (see below).

- No TLS: a `wss://` proxy or tunnel goes in front for remote use.
- Sessions belong to their connection. A dropped connection ends its open
  sessions and running turns; the client continues with `session/load`.
- For one rho serving several editors, more is needed: `Bash` gets the
  server's environment, not the editor's, and reattaching to a running
  session needs the session map moved from the connection to the agent.
  The sandbox is already per session.
