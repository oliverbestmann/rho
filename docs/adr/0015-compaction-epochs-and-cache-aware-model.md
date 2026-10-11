# 15. Compaction keeps old messages and picks the model by cache state

- Status: Accepted
- Date: 2026-10-09

## Context

Long sessions fill the context window. `/compact [focus]` replaces the
history with a summary. Summarizing costs a full read of the history. If
the prompt cache is still warm, the session model can read it at cache
prices; if it is cold, the whole history is paid in full anyway, and a
cheaper model is the better deal.

## Decision

- Summary prompt and format are pi's (Goal, Constraints, Progress, Key
  Decisions, Next Steps, Critical Context), with an optional user focus.
- If the last request started less than `CACHE_TTL - 30 s` (5 min - 30 s)
  ago, the cache is assumed warm: the session's model gets the exact same
  prefix (system, tools, thinking, history) plus the instructions, so the
  history is billed as cache reads.
- Otherwise `RHO_COMPACT_MODEL` / `Options::compact_model` (default
  `anthropic/claude-haiku-5-5`) gets the conversation as plain text:
  user and assistant text, thinking text, tool calls with arguments cut
  to 2000 characters, images as `[image]`, no tool outputs; the request
  runs with thinking off. Without a compaction model, the session model
  is always used.
- The result replaces the history with one user message holding the
  summary. The notice says which path ran and its token usage. If the
  summary is empty or hits the token limit, the history is left as it is.
- In the store, compaction starts a new **epoch**: old messages are kept
  under their epoch, the session's history is the latest epoch's messages,
  and `seq` keeps growing across epochs.

## Consequences

- Nothing is deleted; old messages stay available for search (ADR 0032).
- `session/load` replays all epochs. Each compaction summary is shown as
  a "Compacted" notice (a finished tool call), like when it was made,
  not as a user message.
- After compaction the cache is cold (`last_request` reset).
- Compaction is only manual (`/compact`, ACP only); there is no automatic
  trigger near the context limit.

Code: `rho/src/compact.rs`, `Store::next_epoch`.
