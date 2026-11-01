# 69. Images shown only to the user

- Status: Accepted
- Date: 2026-11-01

## Context

The model makes charts and screenshots the user wants to see. `ReadImage`
shows them, but also sends the pixels to the model, which costs tokens on
every later request and is not needed when the model only made the image.

## Decision

- `ShowImage` reads an image like `ReadImage` (same sandbox, types and
  limits) and returns it in `ToolOutput::shown`, not `images`. The model
  gets only "shown to the user: path (type, size)".
- `Content::ToolResult` has a `shown` field, saved with the history, so
  `session/load` replays the image. `provider::for_model`, which builds
  every request, empties it.
- ACP sends `shown` images after `images` in the tool call content. Tool calls
  carry the tool name in `_meta.rho.tool`; the web client opens
  `ShowImage` cards once they completed.
- `shown` images are scaled down with `ImageSource::fit` like the others.

## Consequences

- The model cannot check what it showed; it has to call `ReadImage` for
  that.
- A request built from the history without `for_model` would send the
  field; Anthropic serializes `Content` directly.

Code: `ShowImage` in `rho/src/tools/read_image.rs`, `for_model` in
`rho/src/provider.rs`.
