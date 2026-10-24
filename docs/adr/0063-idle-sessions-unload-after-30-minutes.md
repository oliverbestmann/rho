# 63. Idle sessions unload after 30 minutes

- Status: Accepted
- Date: 2026-10-22

## Context

The hub dropped a session from memory as soon as no connection had it open
and nothing ran. That killed its background tasks (`session_end`), so a
build started in the background died when the phone's screen turned off.
A stopped turn's held queue (ADR 0062) kept a session in memory with nobody
left to release it. And every reconnect after a short drop loaded the
session from the store again.

## Decision

- `Context::keep_alive` returns a guard; while one lives, the session counts
  as used. The background task extension holds one per running task.
- A session that is unused (no viewer, no run, no queue, no guard) stays
  loaded for 30 minutes (`Hub::with_idle_timeout`). Then it is saved and
  dropped. If it got user input or model text since it was created or
  loaded, it gets a "Session idle" note first. Notices, tool calls and
  thinking do not count, so a session that was only looked at stays as it
  was. A task per session polls for
  this (`Hub::unload_when_idle`), every 30th of the timeout.
- It is still saved as soon as it becomes unused, so notices survive a
  restart.
- When the last connection drops, a held queue is let go and runs.
  `session/close` by the last viewer still clears it and cancels the runs.

## Consequences

- An unused session takes memory for 30 minutes.
- A background task running for up to an hour keeps its session loaded,
  even with nobody connected.
- The timeout can be up to a 30th longer than set.
- A client that attaches in the moment between the idle check and the
  removal sees the "Session idle" note, and the session stays.
