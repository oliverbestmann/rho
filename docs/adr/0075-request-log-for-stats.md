# 75. Request log for stats

- Status: Accepted
- Date: 2026-11-15

## Context

The web client should show a monthly usage report: cost, requests,
sessions, tokens and cache hit per day, provider and model, plus the
subscription limits (5 hour and 7 day windows). The store only had totals
per session, without dates, and rho threw the limit headers away.

## Decision

- Migration 12 adds `requests`, one row per model request: time, session
  (NULL for API proxy requests), provider, model, tokens and cost per token
  type, and the limits the response reported (Anthropic's
  `anthropic-ratelimit-unified-{5h,7d}-{utilization,reset}` headers).
- Every request that goes through `Context::stream` or the API proxies is
  recorded when its stream ends, if it reported usage.
- The migration fills the table once from the `Turn: ...` notes saved in
  `items` (from 2026-10-20 on), one row per turn with `estimated = 1` and
  `requests` the turn's count. These rows have the session's current model,
  the cached tokens rounded from a percentage, and no per-type costs or
  limits. Requests outside turns (compaction, titles) are missing from them.
- `_rho/stats` returns the report for one month. The client sends the
  local day bounds, so days follow its time zone, DST included.
- An account is a provider name.

## Consequences

- Stats before the upgrade are estimates; limit charts start with it.
- The table grows by one row per request and is not pruned.
