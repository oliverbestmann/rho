# 56. Semantic code search via the zg CLI

- Status: Accepted
- Date: 2026-10-18

## Context

Grep finds exact strings. Questions like "where are retries configured"
need search by meaning. zvec-grep (`zg`) indexes a directory with local
embeddings and searches it, as a CLI, a daemon or an MCP server.

The daemon and the MCP server run outside rho's sandbox with full read
access, so searching through them would bypass it. MCP search also never
creates an index; only the CLI does.

zg finds the index for a query by walking up from its cwd. Under Landlock
`stat` still works on paths outside the policy, but `open` does not, so zg
would pick a parent's index it cannot read and fail.

## Decision

- `SemanticSearch` (in `agent/src/semantic_search.rs`) is offered when
  `zg` is on `PATH`. It runs the CLI with `--mode direct`, so everything
  happens in a process rho sandboxes.
- rho walks up from the start directory (`path`, default the cwd) itself,
  under the session's rules, and uses the first directory whose
  `.zvec-grep/manifest.json` it can read. zg then runs with that directory
  as its cwd.
- Without a readable index, it creates one at the start directory with a
  fixed model (`local/potion-code-16m-v2`, CPU) and a unique workspace name
  (`<dir>-<8 hex of sha256(path)>`). This needs write access to the start
  directory; read-only profiles get an error pointing to Grep.
- Searches use `--refresh wait`, so results match the files on disk.
- Each zg command runs under the effective sandbox plus read access to
  zg's directory (it loads its libraries from there) and read-write access
  to `ZVEC_GREP_HOME` (`~/.zvec-grep`) and `<root>/.zvec-grep`. The grant
  is per command, not added to the session.

## Consequences

- Read-only profiles can search existing indexes, and zg writes to its own
  directories there.
- Every search scans the workspace for changes first.
- Indexes of deleted directories stay in zg's `workspaces.json`.
- No daemon: each call loads the model again (fast for potion).
