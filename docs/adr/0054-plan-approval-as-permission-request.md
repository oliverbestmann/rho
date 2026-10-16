# 54. Plan approval is a permission request on the tool call again

- Status: Accepted
- Date: 2026-10-16

## Context

ADR 0047 made `PresentPlan` ask with a form (Approve / Revise plus a
feedback field) and put the plan into the form's message. Zed shows form
messages as plain text, so the plan's markdown was lost, and the form sat
apart from the tool call.

## Decision

`PresentPlan` stays (every profile, as in ADR 0047), but approval goes back
to the `tool_call` hook of ADR 0017: a permission request on the tool call
that shows the plan, with the answers "Approve" and "Skip, let me add
something".

- Approve: the tool runs; from `plan` it switches to `write`.
- Skip (or the request is dismissed): the call is blocked, the profile stays
  and the run stops, so the user writes their additions as the next message.

`Frontend::confirm_with` / `Context::confirm_with` take the two answer
labels; by default they fall back to `confirm`.

## Consequences

- The feedback field is gone; feedback is the user's next message.
- Frontends without `confirm` deny the plan (ADR 0013), as before ADR 0047.

Amends [0047](0047-present-plan-with-feedback.md). Code:
`agent/src/plan_mode.rs`, `rho/src/acp/frontend.rs`.
