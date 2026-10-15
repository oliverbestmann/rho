# 46. The model sets the session title with `SetSessionTitle`

- Status: Accepted
- Date: 2026-10-15

## Context

`SessionTitle` named the session from its first prompt with a background
request to `anthropic/claude-haiku-5-5` (pi's approach). The title never
changed when the conversation moved on, and it needed a second model next
to the session's.

## Decision

`SessionTitle` (`agent/src/session_title.rs`) adds a `SetSessionTitle` tool
("concise 3-7 word title") instead.

- The system prompt asks for a title as soon as possible, and for a new one
  when the conversation drifts too far from it.
- After the first request, every input gets a `<system-reminder>` while the
  session has no title.
- Compaction appends "The current title of the session is: …" to the
  summary (`rho/src/compact.rs`), since the call that set it is gone with
  the history.

## Consequences

- No extra model request, and no dependency on a second model.
- The title costs a tool call in the session's own context.
- A model that ignores the instruction leaves the session untitled. The
  reminder repeats on every input until it sets one.
