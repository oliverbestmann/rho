# 41. Edit diffs are stored for 7 days and replayed

- Status: Accepted
- Date: 2026-10-15

## Context

`Edit` sends its change to the client as an ACP `diff` (`path`, `oldText`,
`newText`, the whole file before and after). The history only holds the
tool call and its one-line result, so `session/load` and `session/resume`
showed edits without their diff. On a phone, reviewing what changed is
the main reason to open an old session.

ACP has no content type for a patch. A `content` block with a markdown
```` ```diff ```` would lose Zed's diff view. A `diff` with only the hunks as
`oldText`/`newText` would lose the line numbers.

## Decision

Store the diff as sent and replay it unchanged (`rho/src/store.rs`, schema
migration 5).

- A `diffs` table holds `path`, `old`, `new` per `(session_id,
  tool_use_id)`, apart from `messages`: `save_messages` deletes and
  rewrites message rows, and the diff is not part of the provider wire
  format.
- The agent collects `ToolOutput::diff` during a turn
  (`Persist::diffs`) and `try_save` writes it after the messages.
- Each `save_diffs` deletes diffs older than `DIFF_DAYS` (7). There is no
  timer.
- `replay_history` passes the stored diff to `ToolEnd`, so a replay sends
  what the live edit sent.
- `/redact` (ADR 0035) also replaces the phrase in path, old and new text.

## Consequences

- The text is not compressed. A big file costs two copies per edit, for 7
  days.
- Edits older than 7 days, or made before migration 5, show their output
  only.
- Old diffs are only deleted when a new diff is saved. A database that no
  longer gets edits keeps them.
- Deleting a session deletes its diffs (`ON DELETE CASCADE`).
