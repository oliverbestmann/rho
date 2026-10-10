# 7. Keep the request prefix stable for the prompt cache

- Status: Accepted
- Date: 2026-10-09

## Context

Agent runs re-send the whole conversation on every model request. With
Anthropic's prompt cache, a prefix that matches an earlier request is read
at a fraction of the price (and counts less against subscription limits).
The cache key is the exact prefix: tools, then system, then messages. Any
change early in the prefix makes everything after it a cache miss.

## Decision

Treat the prefix as append-only within a session:

- Three cache breakpoints (the API allows four): the last tool, the last
  system block, and the last block of the final user message, with the
  default 5 minute TTL. Every request then reads the previous one's prefix.
- Tools and system prompt are fixed when the session starts
  (`SessionSetup`). `PromptSetup::system` can change it per run, documented
  as cache-breaking.
- **Every request carries every tool**, whatever the profile. Profile
  switches are told to the model by a note appended to the next user
  message (ADR 0016), not by changing the tool list.
- Messages for the model during a run (steering, grants, profile notes) are
  appended to the next user message rather than inserted earlier.
- Thinking blocks stay in the history with their signatures.
- The billing system block (ADR 0006) is derived from the first user
  message, so it stays the same for the whole session (until compaction
  replaces that message).
- `Session::last_request` records when the cache was last refreshed; this
  drives `/usage` ("warm, expires in ...") and compaction (ADR 0015).
- Changes that do invalidate the cache are explicit: switching the model,
  changing the additional directories (system prompt rewritten). Both
  reset `last_request`.

## Consequences

- Hidden tools are still offered to the model; calls to them are rejected
  with an error (ADR 0016).
- Usage is tracked per request as input / cache read / cache write / output
  and reported per request, per prompt and per session, with the hit rate.
- Small prompts below the model's minimum cacheable size are not cached;
  nothing is done about that.

Code: `build_body` in `rho/src/anthropic.rs`, `Session::tool_specs` and
`profile_note` in `rho/src/agent.rs`.
