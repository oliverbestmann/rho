# 3. ACP-native, no TUI; frontends behind a `Frontend` trait

- Status: Accepted
- Date: 2026-10-09

## Context

The agent is used from an editor (Zed). The Agent Client Protocol (ACP)
already gives sessions, streaming updates, tool call cards, diffs,
permission prompts, elicitation forms, config options (model, thinking,
mode) and slash commands. Writing a terminal UI would duplicate all of that.

## Decision

- rho's main interface is ACP over stdio (`rho acp`), implemented in
  `rho/src/acp/` with the `agent-client-protocol` crate.
- There is no interactive TUI. A one-shot CLI (`rho "<prompt>"`) exists for
  scripting: answer on stdout, thinking/tools/usage on stderr, sessions not
  saved.
- The agent loop knows nothing about either. It emits `AgentEvent`s to a
  `Frontend` trait object (`event`, `confirm`, `ask`); `AcpFrontend`, the
  CLI printer and test frontends implement it. Without a UI, `confirm` is
  `false` and `ask` is `Unsupported` (see ADR 0013).
- Long-running work (`session/new` hooks, prompts, config changes) is
  spawned off the ACP dispatch loop so `session/cancel` still gets through.
- ACP features rho has no standard for go into `_meta` (usage breakdown
  under `_meta.rho`, Zed's terminal output for `Bash`), the way pi-acp does.
- Messages an extension sends while idle with `trigger_turn` start runs
  that only stream `session/update`s, since ACP has no agent-initiated
  prompts (`watch_inbox`).

## Consequences

- Capabilities are negotiated: forms only if the client supports
  elicitation, terminal cards only with `terminal_output` meta, list/resume
  only if sessions are stored.
- pi's `exit` extension has no counterpart.
- Notices (`Context::notify`) are rendered as finished tool calls, as there
  is no other ACP element for agent-side messages to the user.

Code: `rho/src/acp/`, `rho/src/cli.rs`, `Frontend` in
`rho/src/extension.rs`.
