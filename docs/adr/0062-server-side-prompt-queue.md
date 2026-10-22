# 62. Prompts queue in the daemon

- Status: Accepted
- Date: 2026-10-21

## Context

The web client queued messages typed during a turn in the browser and sent
the next one when a turn ended. A phone drops its WebSocket when the screen
turns off, so the queue stopped there and was lost when the page reloaded.
Zed queues on its own and can remove queued messages; it does not need help
from rho.

A slash command cannot add to a queue: while a turn runs, every
`session/prompt` waits for the session lock.

## Decision

- ACP extension methods: `_rho/queue {sessionId, text}` adds a prompt,
  `_rho/unqueue {sessionId, id}` removes one (an id that is gone is fine,
  it may just have started). Only connections that opened the session may
  use them, like `session/cancel`.
- The queue lives in the hub entry of the session (`rho/src/acp/hub.rs`).
  When a turn ends, the next queued prompt starts as a run without a client:
  it is echoed to every viewer as a user message, slash commands work, and
  errors show as a note. Its run is registered before the ended one is
  dropped, so the session stays busy in between. Queuing while idle starts
  the prompt at once.
- A stopped (`session/cancel`) or failed turn holds the queue until a turn
  ends normally, like the web client did.
- Every change goes to all viewers as `_meta.rho.queue: [{id, text}]` in a
  `session_info_update`; a viewer that attaches gets it with the busy state.
- A session with a queue stays in memory without viewers. `session/close` by
  the last viewer clears the queue along with cancelling its runs.
- The web client sends to rho's queue while a turn runs and shows rho's
  queue; its own queue is gone.

## Consequences

- Queued messages survive disconnects, but not a daemon restart (in memory
  only). Saving them with the session would also need a way to restart the
  queue when nobody opens the session.
- Zed and other clients keep their own queues, which rho does not see.
- Queued prompts are text only.
