# 35. `/redact` masks a phrase in every store

- Status: Accepted
- Date: 2026-10-15

## Context

A secret that `Secrets` (ADR 0034) did not catch, or that got in before it
existed, stays in the session database, the `History` index and maybe in
memories. The user needs a way to remove it everywhere.

## Decision

`/redact <phrase>` is a built-in command like `/compact`: it needs the open
session mutably and writes rows only the core may write.

- The phrase (exact, case-sensitive) is replaced with the secrets marker
  (`store::MASK`), not deleted with its message. Deleting a message would
  leave a `tool_use` without its result or two user messages in a row, which
  the API rejects on every later request, and `Store::load` needs
  contiguous `seq`.
- Text, tool results and every string in tool inputs are masked. A thinking
  block containing the phrase is dropped: its signature covers the text. A
  message left empty gets the marker as text.
- `Store::redact` updates all sessions in the database, all epochs and
  titles, and sets `updated_at` so `History` re-indexes them.
- Then the core runs the `redact` command of every extension (not only the
  first that answers) and shows all replies. `History` drops the index of
  matching sessions, `Memory` deletes matching memories.
- Every write runs with `secure_delete` and ends with
  `wal_checkpoint(TRUNCATE)`; the FTS index is optimized. Old text does not
  linger in free pages or the WAL.

## Consequences

- The open session's prompt cache is cold after a hit.
- Other open sessions (in this process or another rho) still hold the phrase
  in memory and send it until they are reopened. Their saves only rewrite
  the last message, so stored rows stay masked.
- Files outside rho's databases (client logs, scrollback) and other session
  databases are not touched. A checkpoint blocked by a reader leaves the
  WAL as is.
- The arguments are trimmed, so leading and trailing spaces are not part of
  the phrase.
