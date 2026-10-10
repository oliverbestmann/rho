# 29. Project context files are read only with the user's consent

- Status: Accepted
- Date: 2026-10-10
- Amends: ADR 0023

## Context

`AGENTS.md`/`CLAUDE.md` in the cwd and every parent were read into the
system prompt automatically (ADR 0023). A cloned repository can steer the
agent through them without the user noticing.

## Decision

- `prompt::build` reads only the config dir's context file.
- The `ProjectContext` extension (agent binary, registered last) asks
  before the first prompt of a session about each directory with a context
  file and no remembered answer, in one form: "Always read" / "Never read".
  Answers are stored per directory in `~/.config/rho/project_context.json`
  (`{"/dir": true}`), editable by hand.
- Dismissing the form skips the files for this session and asks again next
  time. Without forms, it falls back to a confirmation per file that is not
  remembered (one-shot runs deny it).
- Approved files are appended at the very end of the system prompt, as a
  `<project_context>` section, read once and fixed for the session.

## Consequences

- The first prompt in a new project waits for the answer.
- The question comes with the first prompt, not at session start: the ACP
  client does not know the session yet while it is being created.
- Concurrent sessions answering at the same moment can lose one answer
  (unlocked read-modify-write); it is asked again.
- A file's content is not pinned: an approved directory's file is read
  whatever it says later.

Code: `agent/src/project_context.rs`, `prompt::context_file`,
`prompt::context_text`.
