# 20. Web view as a hook-only extension; loopback or password

- Status: Accepted
- Date: 2026-10-09

## Context

It is useful to follow sessions and queue messages from another device
(e.g. a phone) while the agent works in the editor. This should not need
changes to the core, and the page must not become a way for anyone on the
network, or any website, to send prompts to an agent that runs commands.

## Decision

- `WebUi` (in the `rho` binary) is built **only on extension hooks**:
  user-side messages from `before_request`, tool calls from
  `tool_call`/`tool_result`, the model's text and thinking from
  `turn_end`, stored history for loaded sessions, other stored sessions via
  `ctx.store()` (read-only). It is registered first so it sees tool calls
  before others block them (calls the profile blocks only show up through
  `tool_result`).
- A message from the page goes to `ctx.send(text, trigger_turn = true)`
  (ADR 0010) and is shown in the editor as a notice.
- The page is a static HTML file served with axum; the API is polled
  (`?since=n`), no streaming.
- The server starts with the first session. It listens on
  `0.0.0.0:8787` by default (`web.json` `listen`, `RHO_WEB_UI`, `off`
  disables it), trying up to ten ports from the configured one if it is
  taken, so a second rho gets its own. An invalid `web.json` does not stop
  rho; the page is off and `/web` says why.
- Access:
  - requests from loopback need no password;
  - other clients need the `password` from `~/.config/rho/web.json`
    (header `x-rho-password`; the page asks once and keeps it); without a
    configured password they are refused;
  - the `Host` header must be `localhost` or an IP address (against DNS
    rebinding); posts must be JSON (no cross-site form posts).
  The first version used a random per-process token in the `/web` link
  (`604a8f8`); it was replaced by the configured password (`88c5f6a`),
  which stays the same across restarts and processes.
- `/web` shows the link.

## Consequences

- No streamed text on the page; "working" is approximate (from request
  start to settle).
- Anyone with the password can prompt the agent; the password should be
  long. There is no TLS.
- Sessions open in another rho process are shown read-only from the
  database (only databases of sessions this process has opened);
  messages can only go to sessions open in this process.
- Tool output on the page is cut to 4000 characters.

Code: `agent/src/web_ui.rs`, `agent/src/web_ui.html`.
