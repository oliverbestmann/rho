# 78. Projects with a shared text and a work log

- Status: Accepted
- Date: 2026-11-27

## Context

Separate sessions share no context, and one long session is hard to return
to: compaction summaries lose detail, and earlier work is far back in the
history. `Memory` holds facts across all work and is only found by search;
`AGENTS.md` is read once per session and lives in a repo. The user wants a
current picture of a piece of work that every session working on it sees,
and a way back to earlier work. A project is named and chosen per session, so
it can span several repos.

## Decision

The `Projects` extension (agent crate, `project.rs`) adds named projects.

- **Storage** in `sessions.db` (`rho/src/store/projects.rs`): `projects`
  holds one text per project with a `version` that goes up with each change;
  `project_log` holds entries (`entry` from the model, `compaction` written
  automatically) with the session id and its next message number (`seq`). A
  session's project is the extension state value `project.name`, so the
  hint below is one query with `->>` and no new session column.
- **Joining:** `/project <name>` joins (and creates) a project, `/project`
  lists them. A new session whose directory last had a project gets a hint
  note: "Last project here: rho. Run `/project rho` to join it." It does not
  join on its own. Hint notes are a new `NoteType::Hint`, sent like text notes
  with `_meta.rho.class = "hint"`; the web client colours them.
- **What the model sees:** a fixed `<projects>` rule in the system prompt
  for every session. The full project text and the last 50 log titles come
  as a system reminder with the first prompt after joining or a resume, and
  after each compaction or `/clear` (a new `compacted` extension hook sends
  it through the inbox, so it also arrives mid-run after the `Compact` tool).
  If another session changes the text, the next prompt gets the new text.
  The system prompt itself never changes, so the prompt cache stays valid.
- **Writing:** the `Project` tool reads and edits the text (`edit` is an
  exact replace that fails if the text changed since the session last saw
  it), adds log entries, and reads or searches them. Approved `PresentPlan`
  and `PresentDiff` calls and `git commit` results get a reminder to log and
  update the text; so does the reminder after a compaction. `/project:log`
  asks for an entry.
- **Compaction log line:** after a compaction the cheap model turns the
  summary into one line, logged as a `compaction` entry, so the log shows
  the course of a session even when the model logs nothing.
- **Going back:** a log entry names its session and message number. The
  `History` tool searches and reads the sessions of the joined project in
  any directory, besides those under its root. It is now always offered:
  without a `.rho-project-root` marker, the root is the session's cwd
  (amends ADR 0032).

## Consequences

- The project text is limited only by a reminder to condense it above 8 KB;
  it is sent in full with each full reminder and after each change by another
  session.
- Without a marker, `History` in a parent directory also searches the
  sessions of all directories below it.
- `/redact` also masks project texts and log entries.
- Several sessions editing at once are safe only through the version check;
  there is no lock or merge.
