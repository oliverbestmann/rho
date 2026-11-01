# 71. The model can discard old tool outputs

- Status: Accepted
- Date: 2026-11-01

## Context

Large tool outputs (file reads, logs, images) stay in the context until a
compaction, which replaces everything. The model often knows that one
output is no longer needed. Removing it changes the request in the middle,
so the prompt cache breaks from there: the rest is written again at 1.25x
instead of read at 0.1x.

## Decision

- An extension in the `rho` binary, `Discards` (`agent/src/discard.rs`).
  The core is not involved.
- `before_request` starts every tool result with `[output N, size]`, N
  counting tool results in the request. The numbers only depend on the
  history, which only grows, so the labels do not break the cache. After a
  compaction they start again at 1.
- The `Discard` tool takes numbers. It maps them to tool call ids with the
  last request's numbering; unknown or already discarded numbers are an
  error and nothing is discarded.
- Discards wait in memory until the cache has certainly expired
  (`CACHE_TTL` + 30 s since the last request; our clock starts before the
  provider's). Then they move to the session's stored state (ids, saved with
  the session), and every later request replaces those results with
  `[output N discarded]` and drops their images.
- Only requests change. The history, the store, the replay and the user's
  transcript keep the full outputs.
- Stored ids not in the request anymore (after a compaction) are dropped.
- Warm compaction runs the extensions' `before_request` hooks too (fix in
  `rho/src/compact.rs`), so its request matches the run's prefix and the
  summarizer sees what the model saw.

## Consequences

- The first request after this change has a cold cache: every tool result
  got a label.
- In a busy run a discard only takes effect after a pause of more than five
  and a half minutes. Pending discards are lost on a daemon restart, which
  costs nothing.
- The model can misjudge what it needs later; it can run the tool again.
