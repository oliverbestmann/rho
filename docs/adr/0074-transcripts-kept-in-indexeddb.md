# 74. Transcripts kept in IndexedDB, saved transcripts in rho

- Status: Accepted
- Date: 2026-11-08
- Amends: ADR 0064, ADR 0070

## Context

A page reload in the web client loaded the whole transcript again, since
the cursor of ADR 0064 lived only in memory. After an idle unload (ADR
0063) or a restart, rho rebuilt the transcript from the store under a new
id, with other entries than it had live (a tool call there is fewer
updates), so every cursor was stale and rho sent everything. ADR 0070
rejected keeping the transcript in IndexedDB for these reasons.

## Decision

- rho saves the frontend's transcript in the store (`transcripts`,
  `transcript_entries`) when a turn ends and when a session unloads, after
  the session itself. Only entries that changed since the last save are
  written. The saved one is valid while the session's `epoch` and
  `updated_at` are unchanged; opening the session then takes it over with
  its id, so cursors stay valid. Otherwise the transcript is rebuilt as
  before. A restart during a turn still resets.
- New notice ids continue after the restored ones.
- `redact` deletes all saved transcripts. An open one is not saved again,
  since its memory may still hold the phrase.
- The cursor gets `start`, where the previous epoch starts. The client
  drops kept entries before it, as rho does.
- The web client keeps, per server and session, the transcript's entries
  by their index and the last cursor in IndexedDB. A merged text,
  thinking or terminal entry grows only by the bytes after what it has, so
  several tabs can write the same entries without messages between them.
  The echo of its own prompt (an empty update with a cursor) is kept as the
  prompt's block.
- On a reload it shows the last 50 entries and loads with the kept cursor.
  "Show earlier" reads 50 more. A tool call update whose call is older
  than the entries read is skipped.
- A `reset` clears what is kept for the session.

## Consequences

- A reload, also after an unload or a restart between turns, shows the
  session at once and fetches only what is new.
- Each turn end writes the changed entries to SQLite, images included.
- IndexedDB grows with every session shown; nothing deletes old ones.
  shortcut: prune sessions not opened for a while if the browser's quota
  becomes a problem.
- After a `reset` the replay shows as it arrives, as the kept entries are
  gone.

Code: `rho/src/acp/frontend.rs` (`save_transcript`, `restore`),
`rho/src/acp/hub.rs`, `rho/src/acp/load.rs`, `rho/src/store.rs`,
`web/src/app/session/history-db.ts`, `web/src/app/acp.ts`.
