# 45. `Glob` and `Grep` split like Claude Code's

- Status: Accepted
- Date: 2026-10-15

## Context

`Grep` came from pi's ripgrep-find: with a pattern it searched contents,
and without one it listed files. Models trained on Claude Code expect
`Glob` for file names and `Grep` with `outputMode`, and called the one
tool both ways. Built-in tools already use Claude Code names (ADR 0006).

## Decision

`RipgrepFind` (`agent/src/ripgrep_find.rs`) adds two tools. Both run `rg`
in the session's sandbox, like `Bash`, with 50 lines by default.

- `Glob`: file names matching a glob, newest first (`rg --files`).
- `Grep`: `pattern` is required. `outputMode` is `files_with_matches` (the
  default), `content` or `count`. `context` applies to `content` only.
- A `Grep` call without a pattern fails. It no longer falls back to a file
  listing.

## Consequences

- `Grep` returns file names by default, so the model has to ask for
  `content` to see lines.
- Sessions from before still replay. A stored `Grep` call without a
  pattern would now fail if the model repeated it.
