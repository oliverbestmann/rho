# 17. Plan mode is an extension with user approval

- Status: Superseded by [0047](0047-present-plan-with-feedback.md)
- Date: 2026-10-09

## Context

Claude Code's plan mode works well for non-trivial changes: explore
read-only, write a plan, have the user approve it, then implement.
Profiles (ADR 0016) provide the mechanism, so plan mode does not need to be
in the core.

## Decision

`PlanMode` in the `rho` binary:

- Adds a `plan` profile: `Write`, `Edit` and `EnterPlanMode` hidden, sandbox
  read-only, instructions telling the model to explore, ask questions and
  finish with `ExitPlanMode`.
- Hides `ExitPlanMode` in `write`.
- `EnterPlanMode` lets the model enter plan mode on its own (for changes
  with several options, many files or unclear requirements).
- `ExitPlanMode { plan }` shows the plan as the tool call's content
  (`Tool::describe`). Approval is asked in the `tool_call` hook, where the
  call id is known, so the question is attached to the plan.
  - approved: the tool switches to `write` and tells the model to start;
  - rejected: the call is blocked, the session stays in `plan` and the run
    is stopped (`ctx.stop`), so the user can say what to change.
- Picking `write` in the editor's selector needs no approval.

## Consequences

- Without a UI, plans are never approved (ADR 0013).
- The read-only sandbox, not the instructions, is what stops `Bash` from
  changing files in plan mode. Side effects outside the filesystem (e.g.
  network calls) are only discouraged by the instructions; git mutations
  are still gated by `GitGate`.

Code: `agent/src/plan_mode.rs`.
