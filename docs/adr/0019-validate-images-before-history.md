# 19. Validate images before they enter the history

- Status: Accepted
- Date: 2026-10-09

## Context

Users paste images into the editor, and the model should be able to look
at image files. The API rejects images of other types or over 5 MB. Since
the whole history is sent with every request, one rejected image would fail
every later request of the session (and it would be stored, ADR 0014).

## Decision

- `Content::Image` uses the Anthropic base64 format. ACP advertises
  `promptCapabilities.image`; `image` blocks and embedded `image/*` blob
  resources become image content.
- `ImageSource::check` (JPEG, PNG, GIF, WebP; valid base64; at most 5 MB
  decoded) runs **before** a prompt is added to the history. A bad image
  fails the prompt instead.
- `ReadImage` reads files sandboxed like `Read`, checks the size before
  reading, and detects the type from the magic bytes, not the extension.
- Tools can return images (`ToolOutput::images`); the provider sends such
  results as `[text, image]` blocks, and ACP shows them in the tool call.
  The agent checks them after the `tool_result` hooks; a result with a bad
  image becomes a tool error without the images.
- Images are stored and replayed with the session; compaction and search
  see user images as `[image]` (tool results, and so their images, are
  left out of both).

## Consequences

- Other types (e.g. TIFF, SVG) have to be converted before use.
- Images make the stored sessions larger.

Code: `ImageSource` in `rho/src/provider.rs`, `to_content` in
`rho/src/acp/mod.rs`, `ReadImage` in `rho/src/tools.rs`.
