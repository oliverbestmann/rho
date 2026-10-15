# 2. A library crate plus a binary that picks the extensions

- Status: Accepted
- Date: 2026-10-09

## Context

rho needs a stable core (agent loop, providers, tools, ACP, persistence) and
a set of opinionated, personal behaviours (ripgrep/git gates, Go checks,
plan mode, memory, ...). pi loads extensions at runtime from
TypeScript; Rust has no comparable plugin story that is safe and simple.

## Decision

A Cargo workspace with two members:

- `rho/`: the library. Everything generic, and **no extensions** enabled by
  default. Entry point `rho::run(rho::Options)`.
- `agent/`: the `rho` binary. It only chooses extensions (and could change
  providers, tools, default and compaction model) through `rho::Options`;
  `main.rs` is ~40 lines.

Extensions are compiled in. Their order in `Options::extensions` is the
hook order (e.g. `Secrets` goes last so it sees the prompts and tool
output after every other extension changed them, ADR 0034; calls the
profile blocks never reach any `tool_call` hook, ADR 0016).

`Options::default()` is a working agent: the `anthropic` and
`github-copilot` providers (ADR 0048), the built-in tools (`Read`,
`ReadImage`, `Write`, `Edit`, `Bash`, `RequestAccess`), no extensions,
model `anthropic/claude-sonnet-5-5`, no compaction model (the session
model compacts, ADR 0040), sessions in `RHO_SESSIONS_DB` or
`~/.config/rho/sessions.db`.

## Consequences

- Adding or removing an extension is a rebuild, not a config change.
- Extensions use only the public API (`rho::extension`, `rho::tools`,
  `rho::provider`, `rho::store` read access, `rho::prompt::config_dir`),
  which keeps that API honest; anything an extension needs has to be
  exposed deliberately (e.g. `tools::truncate`).
- A different personal setup is a different small binary crate.
- `GrantCommands` is the exception that lives in the library
  (`rho::sandbox`), as it is tied to the sandbox, but still has to be
  enabled by the binary.

Code: `Cargo.toml`, `agent/src/main.rs`, `rho/src/cli.rs` (`Options`).
