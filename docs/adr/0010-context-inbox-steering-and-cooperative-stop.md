# 10. `Context`: message inbox, steering and cooperative stop

- Status: Accepted
- Date: 2026-10-09

## Context

Hooks, tools and background work (finished background tasks, messages from
the web view, sandbox grants) need to talk to the model and to end runs, at
any time and from any task, including while no run is active.

## Decision

`Context` is a cheap-to-clone `Arc` handle per session, passed to hooks and
tools, which can keep it.

- `send(text, trigger_turn)` puts a message in the session's inbox:
  - during a run it is added at the next tool-result boundary ("steering":
    it rides along with the tool results in the same user message);
  - when idle it waits for the next prompt, or, with `trigger_turn`, starts
    a run right away (`Agent::run_triggered`; in ACP via `watch_inbox`).
  - at settle time, triggered messages continue the run; untriggered ones
    wait for the next prompt.
- `stop(reason)` requests a clean end of the run (pi's `ctx.abort()`). The
  loop checks after `before_prompt` (prompt and queued messages dropped), after
  `before_request` (request not sent), between tool calls (remaining calls
  get a "skipped" result) and before continuing. The run returns
  `StopReason::Stopped` (ACP: `cancelled`). A stop requested while idle
  does not end the next run.
- Also on `Context`: `notify` (user only), `confirm` / `ask` (user dialogs),
  `complete` (one-off model call), `set_title`, `set_profile`, sandbox
  access (ADR 0012) and read access to the store.

## Consequences

- Messages are never injected mid-stream; the model sees them at the next
  natural boundary, which keeps the history valid (ADR 0011) and the cache
  prefix stable (ADR 0007).
- The one-shot CLI only handles messages already triggered when the run
  ends; it does not wait for background work that reports later.
- Hard cancellation (ACP `session/cancel`) is separate: it drops the run's
  future (ADR 0011).

Code: `Context` in `rho/src/extension.rs`, `Agent::run` in
`rho/src/agent.rs`, `watch_inbox` in `rho/src/acp.rs`.
