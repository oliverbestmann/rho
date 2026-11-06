# 73. ShareFile links

- Status: Accepted
- Date: 2026-11-06

## Context

The model makes files the user wants to open: reports, PDFs, build
artifacts. In Zed a link to the local path opens them. The web client runs
in a browser, maybe on another machine, so it needs a link the daemon
serves. A link cannot carry the `Authorization` header, and the token must
not go into URLs.

## Decision

- `ShareFile {path}` opens the file under the session's sandbox and tells
  the model "shared with the user: path (size)". Nothing new is stored: the
  ACP frontend builds the link from the call's `path` (`Shown::Link`), also
  on replay, and adds it as a `resource_link` with a `file://` uri once the
  call succeeded.
- Viewers on the TCP listener (the web client) get the uri rewritten per
  viewer to `/share/<session>/<mac>/<path>`. The mac is HMAC-SHA256 of the
  session id and path, keyed with a hash of the listener's token, cut to
  128 bits. Links stay valid across restarts while the token does.
- `GET /share/...` checks the mac, then opens the file with the open
  session's sandbox (`Context::run_sandboxed`), so grants, features and
  read-only profiles apply at download time. The session must be open in
  the daemon; the web client has it open while it shows the link.
- Responses carry `Content-Security-Policy: sandbox` and `nosniff`: a
  shared HTML file must not run scripts on the web client's origin, where
  the token is stored.

## Consequences

- A link works for anyone who has it, until the token changes, as long as
  the session is open and its sandbox allows the file.
- Following a link after the session was unloaded gives a 404 until it is
  opened again.
- The CSP sandbox can keep some browsers' PDF viewers from rendering.

Code: `rho/src/tools/share_file.rs`, `web_links` in
`rho/src/acp/frontend.rs`, `shared_file` in `rho/src/acp_ws.rs`.
