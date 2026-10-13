# 34. Secrets are masked in two stages with the gitleaks rules

- Status: Accepted
- Date: 2026-10-14

## Context

Prompts, files and command output can contain API keys, tokens and private
keys. They should not reach the model provider, nor the session database.

## Decision

The `Secrets` extension (agent/src/secrets.rs) uses `gitleaks-rs` (a git
dependency, not on crates.io) with gitleaks' `config/gitleaks.toml`, copied
to `agent/src/gitleaks.toml`. Secrets become `REDACTED`.

- Text is scanned as a whole, not line by line, like gitleaks: some rules
  span lines (private keys, curl headers).
- Stage 1 redacts at the source, so history and database only hold the
  marker: prompt text and the system prompt (`before_prompt`) and tool output
  (`tool_result`). It is registered last, after every extension that changes
  them.
- Stage 2 is the new core hook `Extension::before_send(&Context, &mut Value)`:
  the JSON body of every request, right before it is posted.
  `Provider::stream` takes the callback and must call it; `Context::stream`
  runs all extensions' hooks and is used by the agent loop, compaction and
  `Context::complete`. `Secrets` redacts every string there except thinking,
  reasoning and image blocks (signed, encrypted or base64), and notifies the
  user once per new hit: it means stage 1 missed something (steering
  messages, `Context::complete` input, sessions from before this extension).
- The Anthropic billing header is derived from the body after `before_send`.
- Write calls that add `REDACTED` to a file, and Edit calls whose `newText`
  has more of it than `oldText`, are blocked: the model never sees the real
  secret and would overwrite it. The block message tells the model it can
  repeat the call with `allowMasked: true` when it means to write the
  marker itself, e.g. in tests or docs. `Secrets` adds that field in
  `session_start` by wrapping Write and Edit (schema plus the field
  stripped before the tool runs), so the core tools don't know about it.
  This trusts the model to tell the two cases apart.

## Consequences

- Compiling the 220+ rules takes ~3 s (release); it starts in a background
  thread at startup and the first scan waits for it.
- Stage 2 only scans strings it has not seen clean before (hashes), so a
  request costs the scan of its new content.
- The user also sees `REDACTED` in tool output. The model cannot use a
  masked secret, e.g. to pass it to a command; the user has to do that.
- Not covered: Bash commands that write the marker into files, rules from a
  user config.
