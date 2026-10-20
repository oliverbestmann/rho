# 59. System and user notes in the history

- Status: Accepted
- Date: 2026-10-19
- Amends: ADR 0016, ADR 0046

## Context

The history only had `Content::Text` for user messages. Text that rho or an
extension adds for the model (the go check's failed output, profile and
feature notes, the session title reminder, `Context::send` messages) was
stored as plain user text. The replay on resume showed it as if the user had
typed it, though it was hidden while the session ran. The other way round,
notices (`Context::notify`) and slash command replies were only sent live
and were gone after a resume.

Tool results are already told apart by their block type, not their message:
a user message can hold the prompt, reminders and notices at once
(`push_user` merges them). So the flag belongs on the block.

## Decision

- `Content::SystemNote { text }`: for the model, hidden in the UI. Used for
  `before_settle` messages, profile/feature notes, the session title
  reminder and `Context::send` messages.
- `Content::UserNote { kind, title, text, is_error }`: for the user, never
  sent to the model. `NoteType::Info` is rendered like notices so far (a
  finished tool call in ACP, e.g. turn reports); `NoteType::Text` is for
  longer messages (slash command replies) and goes out as an
  `agent_message_chunk` with `_meta.rho.note`, in italics on the web.
- `provider::for_model` is the only way history reaches a provider: system
  notes become text, user notes and messages left empty are dropped. The
  run and compaction's warm path both use it, so the cache prefix matches.
- `Context::notify` takes the `NoteType` and buffers the note; the session
  adds it to the history when it saves (and before a prompt, to keep the
  order). Notices while a session is set up are not kept: they would repeat
  on every resume. A run saves at its end, for the turn report.
- Slash command replies are `Text` notes titled with the command; `/redact`
  leaves the phrase out of the title.
- The replay skips system notes and shows user notes in place. The
  synthesized "Compacted" notice is only shown for old sessions, whose
  summary has no saved compaction notice.

## Consequences

- No migration: the blocks are part of the stored JSON. Sessions saved
  before keep their plain-text reminders, and the replay shows them.
- The history search (`plain_text`, the History tool) leaves notes out;
  `/redact` covers them.
- A notice raised during tool calls is saved after the tool results.
- The ACP hub saves a session before it drops it (no viewer, no run), so
  notices raised while idle are kept. The session stays open until the save
  is done: a client that opens it meanwhile attaches to it instead of
  loading the store, which would race the save.
- Live, a slash command shows the typed command and the reply note; the
  replay only the note, whose title names the command.
