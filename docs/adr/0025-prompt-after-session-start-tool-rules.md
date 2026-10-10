# 25. System prompt built after `session_start`; tools bring their rules

- Status: Accepted
- Date: 2026-10-10
- Amends: ADR 0023

## Context

The system prompt was built before the extensions' `session_start` hooks
(ADR 0023), so its `<tools>` list had only the agent's own tools, and
`<rules>` was a fixed list in `prompt.rs` for the built-in tools. The model
learned about `Grep`, `FetchUrl` or `GoDoc` only from the tool specs, and a
built-in rule ("Use Bash for file operations like ls, rg, find") pointed it
at the shell instead of them; it reached for `curl` with `FetchUrl` present.

## Decision

- `Agent::setup` runs the `session_start` hooks first and then builds the
  prompt from `SessionSetup::tools`, so every tool is listed. Tools that
  depend on the session (e.g. `GoDoc` only in Go modules) are still
  decided per session in `session_start`.
- `SessionSetup::system` starts empty and is appended to the built prompt
  (after `<cwd>`). Extensions add to the prompt; they no longer see or
  edit the base prompt.
- `Tool::rules()` (default: none) returns lines for `<rules>`, which lists
  each present tool's rules in tool order, then the general ones. The
  built-in rules moved to their tools. `Bash`'s rule no longer names `rg`
  and `find`; `Grep` and `FetchUrl` ask to be used instead of `rg`/`grep`/
  `find` and `curl`/`wget` in `Bash`, `GoDoc` instead of searching the
  module cache.
- The closing line "you may have access to other custom tools" is gone:
  the list is complete.

## Consequences

- An extension cannot rewrite the base prompt anymore (none did);
  `SYSTEM.md` remains the way to replace it.
- Every rule costs tokens in every request; rules are for steering
  between tools, not for repeating the tool description.

Code: `Agent::setup` in `rho/src/agent.rs`, `prompt::build`,
`Tool::rules` in `rho/src/tools.rs`.
