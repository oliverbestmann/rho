# 9. Extension hooks as native `async fn` traits with object-safe twins

- Status: Accepted
- Date: 2026-10-09

## Context

Most behaviour beyond the core loop should live in extensions (ADR 0002),
modelled on pi's events. Hooks need to be async (ask the user, run
`go test`, call a model). Stable Rust supports `async fn` in traits, but
such traits cannot be used as `dyn` objects yet. The `async-trait` crate
would box every future and makes signatures harder to read.

## Decision

- `Extension` has optional hooks, each a plain `async fn` with a default:
  `session_start`, `session_end` (sync, from `Drop`), `before_prompt`,
  `before_request`, `tool_call -> Decision`, `tool_result`, `turn_end`,
  `before_settle -> Option<String>`, `command -> Option<String>`.
  `Tool::run` is an `async fn` too.
- For storage, rho has object-safe twins `DynExtension` and `DynTool` that
  return `BoxFuture`s, with blanket impls for every `Extension` / `Tool`,
  so `Arc::new(MyExtension)` converts on its own. They are to be removed
  once `async fn` in `dyn` traits is stable.
- Hooks run in registration order. Hooks cannot fail the turn; extensions
  handle their own errors (e.g. `ctx.notify`).
- `tool_call` hooks run until the first `Block`; the reason goes to the
  model as a tool error. Hooks can also change the input. A call to a tool
  the profile hides is blocked before any hook runs (ADR 0016);
  `tool_result` hooks run for every call, blocked or not.
- `before_settle` returning text continues the run with it as a user
  message (pi's `agent_before_settle`).
- Extensions register tools, slash commands and profiles in
  `session_start` through `SessionSetup`; slash command replies go to the
  user only.
- Tools return `ToolOutput { output, is_error, images, diff }`; `diff` and
  the `describe` markdown are only for the user.

## Consequences

- No `async-trait` dependency; one boxed future per hook call.
- Extension state is per process, not per session; extensions keep their
  own maps by `session_id` and clean them up in `session_end` (e.g.
  `GoCheck`, `BackgroundTasks`, `SessionTitle`, `WebUi`).
- Hooks do not see the streamed text, only whole turns (`turn_end`), which
  limits what an extension like the web view can show (ADR 0020).

Code: `rho/src/extension.rs`, `rho/src/tools.rs`.
