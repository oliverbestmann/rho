# 5. Provider-neutral types in the Anthropic wire format

- Status: Accepted
- Date: 2026-10-09

## Context

The loop, the store, compaction and the extensions all handle messages.
Only one provider exists today (Anthropic), but the model is picked per
session and extensions call other models (`Context::complete`).

## Decision

- `rho/src/provider.rs` defines `Message`, `Content`, `ToolSpec`,
  `Request`, `Usage`, `StopReason`, `Thinking`. The content types serialize
  **exactly** as the Anthropic Messages API expects (`#[serde(tag = "type")]`),
  so the Anthropic provider sends them as they are and the store saves them
  as they are.
- The one exception is the `images` field of `Content::ToolResult`; the
  Anthropic provider turns results with images into a list of blocks
  (`WireBlock`).
- A `Provider` returns a stream of `Event`s (`TextDelta`, `ThinkingDelta`,
  complete `Block`s, `Usage`, `Done`). Dropping the stream cancels the
  request.
- Providers are registered by name in a `Registry`; models are addressed
  as `provider/model` everywhere (CLI `-m`, `RHO_MODEL`, the store, ACP model
  selector, `complete`).
- Providers describe their models (`ModelInfo`: context window and price,
  including a higher price above a prompt size) for the model picker,
  `/usage` and cost tracking.

## Consequences

- Another provider has to translate from Anthropic's shape, including
  thinking blocks with signatures.
- Stored sessions are in the Anthropic format; a format change needs a
  store migration.
- A provider may report usage more than once per request, each report
  replacing the last; the loop adds only the difference. (The Anthropic
  provider currently reports once, at `message_stop`.)
