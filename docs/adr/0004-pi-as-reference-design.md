# 4. pi is the reference design

- Status: Accepted
- Date: 2026-10-09

## Context

rho replaces a working pi setup (pi, pi-ai, pi-acp and a set of pi
extensions in `~/.dotfiles/pia`). Re-deciding every detail would waste the
experience built into that setup, and switching back and forth should be
cheap.

## Decision

Where there is no reason to differ, do what pi does and use its formats:

- Credentials: pi's `auth.json` layout; other providers' keys in the file
  are preserved, so the file can be shared (`rho/src/auth.rs`).
- OAuth endpoints, client id and request shaping: pi-ai and
  `@gotgenes/pi-anthropic-auth` (ADR 0006).
- System prompt structure, `SYSTEM.md` / `APPEND_SYSTEM.md` /
  `AGENTS.md` (ADR 0023).
- Compaction prompts (ADR 0015) and the `edit` tool semantics (ADR 0018).
- Extension hooks map to pi events (table in the README; some hooks cover
  two pi events, e.g. `before_settle` for `agent_before_settle` and
  `agent_settled`), and
  pi's extensions are ported one by one, named "(pi's X)" in their doc
  comments.
- ACP presentation follows pi-acp (notices as tool calls, terminal meta,
  runs without a prompt).

Deviations are deliberate and documented where they happen, for example:
no fuzzy matching in `Edit`, `GoCheck` stops after 3 failures, tool names in
Claude Code casing.

## Consequences

- When pi changes (e.g. the Claude Code version it claims), rho may need the
  same change (`CLAUDE_CODE_VERSION` says "keep in sync with pi-ai").
- Reviewers can compare behaviour against pi instead of a spec.
