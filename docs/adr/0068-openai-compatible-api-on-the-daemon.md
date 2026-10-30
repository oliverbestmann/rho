# 68. OpenAI-compatible API on the daemon

- Status: Accepted
- Date: 2026-10-28

## Context

Other tools (chat UIs, editor plugins, scripts) speak the OpenAI chat
completions format. rho already holds working credentials for Anthropic and
Copilot, so the daemon can act as a proxy for those tools.

## Decision

- The daemon serves `GET /v1/models` and `POST /v1/chat/completions`
  (`rho/src/openai_api.rs`) on both of its listeners. It speaks only
  OpenAI's format, not Ollama's native `/api/*`. Ollama offers the OpenAI
  format too.
- The endpoints are off by default. The `"api"` config key turns them on and
  lists the offered models with the `models::Pattern` syntax:
  `{"enabled": true, "models": ["anthropic/*"], "token": "..."}`. An empty list offers
  nothing.
- On the TCP listener, the API key is `token`, else the ACP token. Its own
  token keeps API clients out of ACP, which runs tools. The private Unix socket
  needs no token, the same as for ACP.
- Requests go straight to `Provider::stream`. There is no session, no rho
  system prompt, no tools and no extension hooks, and nothing is saved.
- Input is text plus images as base64 `data:` URLs. Images go through
  `ImageSource::fit`. The endpoints reject `tools`, `tool` messages, `n > 1`
  and remote image URLs. `reasoning_effort` maps to `Thinking`, default off.
- System and developer messages become the system prompt. Consecutive
  messages with the same role are merged, because Anthropic needs
  alternating roles.

## Consequences

- Anyone with the API token can use the offered models through the
  subscription. Anthropic's terms for subscription use may not cover this.
- Thinking text is not returned.
- Tool calling, remote images, `/v1/completions` and embeddings would need
  more mapping code.
