# 76. Review comments on plans and diffs

- Status: Accepted
- Date: 2026-11-08

## Context

The user wants to comment on parts of a presented plan, and to review a
diff (for example a pull request before it is pushed) with line comments,
like on GitHub. Both have to work on a phone. ACP has no way to send
comments with a permission answer.

## Decision

- Comments go back as the user's next prompt. The web client answers the
  pending permission request with its reject option, waits for the turn to
  end, and sends one markdown message that quotes each commented block or
  line with its comment. The daemon gets no new method for it.
- Plans: the web client renders the plan of a `PresentPlan` call block by
  block (paragraphs, headings, list items). Tapping a block opens a comment
  box under it.
- Diffs: the new tool `PresentDiff {path}` (agent crate, `review.rs`) takes
  a file with a unified diff, so a large diff does not pass through the
  model. Its `tool_call` hook checks that the sandbox can read the file,
  then asks "Approve" / "Request changes" on the call, as `PresentPlan`
  does (ADR 0054). Requesting changes blocks the call and stops the run.
- The ACP frontend links the diff file on the call from its start
  (`Shown::Review`), also when the call fails, the same way as ShareFile
  links (ADR 0073). The web client fetches it through the signed `/share/`
  link and shows it on its own page, `session/:id/review/:callId`, with
  syntax highlighting and line comments.

## Consequences

- Zed shows the link to the diff file and the two answers, without
  comments.
- The review page needs the session open in the daemon and the diff file
  to still exist. Old reviews may no longer load.
- The web client keeps comment drafts in localStorage per tool call.

Code: `agent/src/review.rs`, `rho/src/acp/update.rs`,
`web/src/app/review/`, `web/src/app/session/plan-comments.ts`.
