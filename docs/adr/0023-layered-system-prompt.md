# 23. System prompt assembled from layered files, fixed per session

- Status: Accepted, amended by ADR 0025 (built after `session_start`)
- Date: 2026-10-09

## Context

The system prompt needs general instructions, the tool list and usage
rules, personal preferences, project instructions and the workspace. pi
builds it from files in a known order, and the same files
(`APPEND_SYSTEM.md`, `AGENTS.md`/`CLAUDE.md`) are already maintained for
pi and other agents.

## Decision

`prompt::build` assembles the prompt once per session, like pi:

1. Preamble (`PREAMBLE`), then `<tools>` (name and first description line
   of each of the agent's tools) and `<rules>` (only for tools that exist,
   e.g. the `Edit` rules). `~/.config/rho/SYSTEM.md` replaces these three.
   The prompt is built before `session_start`, so tools added by
   extensions are not listed; a closing line says other tools may exist,
   and the model learns about them from the tool specs.
2. `<addendum>` from `~/.config/rho/APPEND_SYSTEM.md`.
3. `<project_context>`: the first of `AGENTS.override.md`, `AGENTS.md`,
   `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD` from the config dir, then from each
   directory from `/` down to the cwd (outermost first), each in a
   `<project_instructions path=...>` block.
4. `<cwd>`, then `<additional_directories>` with a hint to use absolute
   paths.

The config dir is `$XDG_CONFIG_HOME/rho` or `~/.config/rho`.
Extensions may change the prompt in `session_start`. Changing the
additional directories of an open session replaces only that section and
keeps what extensions appended.

## Consequences

- The prompt is stable for the session, which keeps the cache warm
  (ADR 0007); edits to these files apply to new sessions only.
- The Anthropic provider puts its own billing and identity blocks before
  it (ADR 0006).
- pi's `APPEND_SYSTEM.md` can be reused by symlink.

Code: `rho/src/prompt.rs`.
