# 64. Reconnects replay from a cursor

- Status: Accepted
- Date: 2026-10-26
- Amends: ADR 0060

## Context

The web client loses its socket often, e.g. when a phone sleeps. On every
reconnect it loaded each open session again, and rho replayed the whole
transcript (two epochs, with images and tool output). The client already
had nearly all of it.

## Decision

- Every update the transcript keeps is sent with `_meta.rho.cursor` on its
  `session/update` notification: `{transcript, entry, len}`. `transcript`
  is a random id per `AcpFrontend`, so it changes after a restart or an
  idle unload (ADR 0063). `entry` counts entries from the start, including
  those that a compaction dropped. `len` is how many bytes of a merged text,
  thinking or terminal output entry were sent. It is 0 for other updates.
- The sender of a prompt does not get its echo. It gets an empty
  `session_info_update` with the cursor, so a reload does not send it the
  prompt again.
- `session/load` takes the last cursor as `_meta.rho.since`. If rho still
  has that position, it sends the rest of that entry and the entries after
  it. If not, or if `since` cannot be read, it sends a `session_info_update`
  with `_meta.rho.reset` and then the whole transcript.
- When a viewer gets ready, it always gets the busy state and the queue,
  also when the session is idle and the queue is empty. A client that loads
  again may still show the old values.
- The web client keeps the state of each session it had open when the
  connection drops, as long as the server stays the same. On the next visit
  it loads with `since` and clears its entries on `reset`.

## Consequences

- A reconnect sends only what the client missed. After a restart, an
  unload or a compaction that dropped the cursor's entry, it sends
  everything, as before.
- Each recorded notification carries about 60 bytes more.
- A page reload still loads everything: the cursor lives only in memory.
- `session/resume` and other clients (Zed) are unchanged; they do not send
  `since`.

Code: `rho/src/acp/frontend.rs` (`Cursor`, `Transcript::updates`,
`attach_replayed`, `echo`), `rho/src/acp/load.rs` (`Replay`),
`web/src/app/acp.ts`, `web/src/app/session/session-state.ts`.
