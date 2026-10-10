# 8. Adaptive thinking only

- Status: Accepted
- Date: 2026-10-09

## Context

Claude models expose extended thinking either with a fixed
`budget_tokens` (older models) or as adaptive thinking with an effort level
(Claude 4.6 and later). Supporting both means two code paths and per-model
knowledge.

## Decision

- rho uses only adaptive thinking:
  `thinking: {type: adaptive, display: summarized}` plus
  `output_config.effort`.
- `Thinking` has six levels: `off|low|medium|high|xhigh|max`, default
  `high`. `off` sends no thinking config. Providers map the levels to their
  own settings.
- The model list contains only models that support it.
- Thinking text is streamed to the user (`agent_thought_chunk` in ACP,
  dimmed on stderr in the CLI). The complete blocks, with signatures, are
  kept in the history and sent back unchanged.
- The level is per session, can be switched over ACP (`thought_level`
  config option) and is stored with the session.

## Consequences

- Older models (with `budget_tokens`) cannot be used with thinking.
- One-off completions (`Context::complete`, the cheap compaction path) run
  with thinking off.
