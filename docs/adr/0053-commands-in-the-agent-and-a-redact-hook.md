# 53. Slash commands run in the agent; extensions redact through a hook

- Status: Accepted
- Date: 2026-10-15
- Amends: ADR 0009, ADR 0035

## Context

The ACP prompt handler matched `/compact`, `/redact` and `/usage` itself,
and built the advertised command list with their descriptions. A second
frontend would have to repeat both. `/redact` reached the extensions by
calling their `command` hook with the name `redact`, a command none of
them registered: `Memory` and `History` had to check for the magic name,
and any extension could take it by accident.

## Decision

- `Agent::commands(session)` returns the session's slash commands: rho's
  own (`compact`, `usage`, `redact`), then the extensions'.
  `Agent::slash_command(session, name, args)` runs one; `None` means there
  is no such command and the text is a prompt. The ACP frontend only parses
  the `/name args` and maps the list to `AvailableCommand`s.
- A new hook, `Extension::redact(ctx, phrase) -> Option<String>` (and its
  `DynExtension` twin), replaces `command("redact")`. `Agent::redact` runs it
  for every extension, after the session store, and shows all replies.
  `Memory` and `History` implement it.
- `Agent::command` is private; tests run extension commands through
  `slash_command`.

## Consequences

- Built-in commands and their descriptions live in one place, next to the
  code that runs them.
- `command` only sees commands the extension registered.
- One more hook on `Extension` and `DynExtension`.

Code: `Agent::commands`, `Agent::slash_command` and `Agent::redact` in
`rho/src/agent.rs`, `Extension::redact` in `rho/src/extension.rs`, the
prompt handler and `commands` in `rho/src/acp/mod.rs`, `agent/src/memory.rs`,
`agent/src/history.rs`.
