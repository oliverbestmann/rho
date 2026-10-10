# 26. `SYSTEM.md` and `APPEND_SYSTEM.md` created empty on startup

- Status: Accepted, amended by ADR 0027 (`SYSTEM.md` is now `PREAMBLE_SYSTEM.md`)
- Date: 2026-10-10
- Amends: ADR 0023

## Context

The prompt files in the config dir (ADR 0023) are only read if they exist,
so users have to know their names and location to customise the prompt.

## Decision

- `rho` creates `SYSTEM.md` and `APPEND_SYSTEM.md` in the config dir on
  startup if they are missing, empty. Failures are ignored.
- Prompt and context files that are empty or whitespace only count as
  absent. Otherwise an empty `SYSTEM.md` would replace the base prompt,
  tools and rules with nothing.

## Consequences

- To replace the base prompt, `SYSTEM.md` needs content; deleting it or
  emptying it has the same effect.
- An empty `AGENTS.md` no longer shadows a `CLAUDE.md` in the same
  directory.

Code: `prompt::ensure_files`, `prompt::build`, `make_agent` in
`rho/src/cli.rs`.
