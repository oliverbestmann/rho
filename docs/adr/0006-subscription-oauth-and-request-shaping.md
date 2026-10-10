# 6. Claude subscription OAuth with Claude Code request shaping

- Status: Accepted
- Date: 2026-10-09

## Context

rho is meant to run on a Claude Pro/Max subscription, not an API key.
Subscription requests that do not look like Claude Code requests are billed
as extra usage ("You're out of extra usage"). `ai-connect` only does the
OAuth handshake, with an old token endpoint and fewer scopes, and does not
shape requests.

## Decision

- `rho login`: OAuth PKCE with the copy-code flow (no local callback
  server). The user opens the URL and pastes `code#state`, a redirect URL or
  a bare code. The verifier doubles as `state`, as in pi.
- Endpoints, client id and scopes as in pi-ai. Tokens are refreshed lazily
  behind a mutex, with a 5 minute margin, and saved atomically (temp file
  with mode 0600, then rename).
- Every request is shaped like Claude Code, following pi-ai and
  `@gotgenes/pi-anthropic-auth`:
  - headers `anthropic-beta: claude-code-20250219,oauth-2025-04-20`,
    `user-agent: claude-cli/<CLAUDE_CODE_VERSION>`, `x-app: cli`;
  - system blocks: the `x-anthropic-billing-header` text (derived from the
    first user message, including the reference's UTF-16 indexing), the
    Claude Code identity line, then rho's own prompt.
- Built-in tools use Claude Code names and casing (`Read`, `Write`, `Edit`,
  `Bash`).

## Consequences

- Depends on undocumented behaviour; `CLAUDE_CODE_VERSION` and the billing
  algorithm have to follow pi-ai. Unit tests pin the billing header against
  reference values.
- The model's system prompt always starts with "You are Claude Code".
- Live tests use real quota and are `#[ignore]`d.

Code: `rho/src/auth.rs`, `rho/src/anthropic.rs`.
