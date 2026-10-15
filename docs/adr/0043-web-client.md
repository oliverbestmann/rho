# 43. A web client in `web/`, for the phone

- Status: Accepted
- Date: 2026-10-15

## Context

ADR 0037 removed the web view in favour of any ACP client over WebSocket.
The generic clients (acp-ui) work, but are not made for following and
answering rho sessions on a phone: rho's tool titles, Edit diffs, forms,
usage and config options (model, thinking, profile) need more care than
a generic client gives them.

## Decision

`web/` is an Angular app that speaks only ACP to the daemon (ADR 0038). It
gets no special API.

- Angular 22 (zoneless, signals, `resource()`), Material 3, and
  `@agentclientprotocol/sdk` with its WebSocket client. The token goes as a
  `bearer.<token>` subprotocol (ADR 0037). Server URL and token are kept in
  localStorage.
- One connection (`Acp`, `web/src/app/acp.ts`). It reconnects with
  backoff, and right away when the page becomes visible again, since
  phones drop sockets while asleep. Pages reload when `agent()` changes.
- A session page builds its transcript from the `session/update`s of
  `session/load` (`SessionState`). Permission requests and forms queue
  over all sessions until answered here or withdrawn
  (`$/cancel_request`).
- Tool cards are markdown (`marked`). Edit diffs become unified hunks with
  3 lines of context (`diff`), computed only when a card is expanded. Diff
  blocks are coloured per line.
- Busy state per session: this client's pending prompts, running tool
  calls and `_meta.rho.running` (ADR 0042). Drafts are kept per session in
  localStorage.
- `ng serve` proxies `/dev/acp` to the daemon (`proxy.conf.json`). The
  Node toolchain comes from the flake's dev shell.

## Consequences

- rho has a second codebase in another language. Its tests run with
  `npx ng test`, not with `cargo test`.
- No build is served by rho. It runs with `ng serve`, or as static files
  behind any server, and needs TLS (a proxy or Tailscale) away from
  localhost.
- Features that ACP lacks (busy state, usage details) need rho metadata
  in `_meta.rho`. Other clients don't show them.
