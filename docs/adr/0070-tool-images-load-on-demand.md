# 70. Tool images load on demand

- Status: Accepted
- Date: 2026-11-01
- Amends: ADR 0064

## Context

A page reload, a daemon restart or an idle unload (ADR 0063) makes rho send
the web client the whole transcript of two epochs. Images from `ReadImage`,
`ShowImage` and tool results are the largest part of it, and most sit in
tool cards that stay closed.

Keeping the transcript in IndexedDB was considered. It does not help after a
restart or an unload (the transcript id changes, so rho sends everything
again), the client does not know where epochs start, and several tabs on one
session would share one store. It is not done.

## Decision

- A client that sets `clientCapabilities._meta.rho.imageRefs` in
  `initialize` gets images in `tool_call` and `tool_call_update` content
  with empty `data` and `uri: "rho-image:<first 16 bytes of the sha256 of
  the data, hex>"`, and with `_meta.rho.width`/`height` read from the image
  header if rho can read it. This is done per viewer in `AcpFrontend::deliver`; the
  transcript keeps the data.
- `_rho/image {sessionId, uri}` returns `{mimeType, data}`. rho looks for
  the image in the session's transcript, so only a connection that opened
  the session gets it, and only while the transcript has it.
- The web client fetches an image when its tool card opens. Until it
  arrives it shows a box of the image's size (4:3 if unknown) with a
  spinner, so the layout does not jump. It caches images by uri.
- Images in user messages stay inline.

## Consequences

- Replays and live updates to the web client no longer carry tool images.
- Opening a card with an image needs a round trip. ShowImage cards open by
  themselves, so their image is fetched right away.
- The hash is the same after a restart, so the client's cache stays valid.
- An image whose epoch was dropped by compaction cannot be fetched; the card
  shows "Image not available".
- Zed and other clients are unchanged.

Code: `rho/src/acp/frontend.rs` (`image_refs`, `AcpFrontend::image`),
`rho/src/acp/images.rs`, `web/src/app/acp.ts` (`image`),
`web/src/app/session/lazy-image.ts`.
