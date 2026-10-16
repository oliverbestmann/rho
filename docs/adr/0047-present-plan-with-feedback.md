# 47. `PresentPlan` replaces `ExitPlanMode` and takes feedback

- Status: Accepted, amended by [0054](0054-plan-approval-as-permission-request.md)
- Date: 2026-10-15

## Context

`ExitPlanMode` (ADR 0017) only existed in plan mode and the user could
only answer yes or no. A rejected plan stopped the run, so the user had to
write the changes as a new prompt. A plan is also useful outside plan mode,
e.g. when the model wants sign-off before a large change in `write`.

## Decision

`EnterPlanMode` and the `plan` profile stay as in ADR 0017. `ExitPlanMode`
becomes `PresentPlan { plan }`, available in every profile:

- (Since ADR 0054 the approval is a permission request on the tool call
  with Approve / "Skip, let me add something"; the form below is gone.)
- The plan is the form's message, so it shows inside the approval box;
  Zed and the web client collapse the tool call card. It also stays on the
  tool call (`Tool::describe`) for the transcript.
- The user gets a form (`ctx.ask`): an Approve / Revise pick and an
  optional free-text feedback field.
- Approved: in `plan` the session switches to `write`; in other profiles it
  stays. The model is told to start, with the feedback as notes.
- Not approved, with feedback: the profile stays and the run continues; the
  model gets the feedback and is told to revise the plan.
- Not approved, no feedback (or the form is dismissed): the profile stays
  and the run stops (`ctx.stop`), like a rejection in ADR 0017.
- Approval moved from the `tool_call` hook (`ctx.confirm`) into the tool,
  because forms are not attached to a tool call anyway.

## Consequences

- Frontends without forms cannot approve plans: the tool fails (ADR 0013).
  Before, it needed `confirm` support instead.
- In ACP clients the form is a session-scoped elicitation, not a permission
  request on the tool call.

Supersedes [0017](0017-plan-mode-as-extension.md). Code:
`agent/src/plan_mode.rs`.
