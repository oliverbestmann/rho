# 31. MCP servers over Streamable HTTP, from mcp.json

- Status: Accepted, amended by 0052
- Date: 2026-10-11

## Context

Remote MCP servers (GitHub, Jira, internal services) offer tools the model
should be able to use. They speak MCP's Streamable HTTP transport and mostly
authenticate with a static token.

## Decision

The `Mcp` extension (agent/src/mcp.rs) reads `mcp.json` in rho's config
dir, in Claude's format (`mcpServers`, `type: "http"`, `url`, `headers`).
`${VAR}` in header values comes from the environment.

- The client is hand-written JSON-RPC over `reqwest`: `initialize`,
  `notifications/initialized`, `tools/list` (paged), `tools/call`. It sends
  `Mcp-Session-Id` and `MCP-Protocol-Version`, and reads JSON or a complete
  SSE body. A 404 with a session reconnects once and retries the call.
- Servers connect on the first session (in parallel, 10 s each) and are
  shared by later ones, so the tool list and the prompt cache stay stable.
- Each tool is `mcp__<server>__<tool>`. Calls run without approval, like
  the built-in tools.
- Config and connect problems are shown with the session's first prompt,
  because notices during `session/new` do not reach the client. (Since
  ADR 0052 they are shown when the session starts.)

## Consequences

- Not supported: stdio servers, OAuth, the old HTTP+SSE transport,
  resources, prompts, server requests and progress, `list_changed`.
- A server that was down at the first session is tried again with the next
  one; a changed tool list needs a restart of rho.
- Servers from ACP `session/new` are ignored.
