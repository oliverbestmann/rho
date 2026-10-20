# 60. History saved per block; viewers replay the transcript; runs go on after a restart

- Status: Accepted
- Date: 2026-10-20
- Amends: ADR 0011, 0014, 0035, 0038, 0039, 0041, 0052, 0055

## Context

The store held only histories that are valid for the API, and only after
a step had ended: a model response was saved together with the results of
all its tool calls (ADR 0011). That had three consequences:

- A restart lost everything since the last save. In one session the model
  wrote a long summary, then asked a question with `AskUserQuestion`; the
  question waited 14 minutes, the client reconnected, and the summary was
  gone from the view. After a restart it would have been gone from the
  history too, and the model would have redone the work.
- A client that attached during a turn got the store's replay plus what
  the frontend patched in from memory (`running`, `streamed`, a buffer of
  unsaved text). Finished but unsaved tool calls were missing, and a read
  of the store racing a save lost or doubled updates.
- Display data was spread out: diffs in their own table, tool descriptions
  computed again at replay, terminal output only in memory.

## Decision

**Blocks.** Table `items` replaces `messages` (migration 7): one row per
content block, with `seq` per block, `epoch`, `role`, the block as wire
JSON, its plain text, `tool_call_id`, `started_at` and `display`.
Consecutive blocks of one role are one message. The agent saves a response
right after it streamed, before its tool calls run, and each result as the
call ends. `Persist` appends the new blocks; a history that shrank is
rewritten from the epoch's start. Existing `messages` rows were not moved;
older sessions are no longer listed.

**Repair.** A history with open tool calls (no result yet) is valid in the
store, not for the API. `close_open_calls` gives them an error result at
the end of the next message. It runs when a run starts and before a
compaction ("cancelled"), when an ACP prompt is cancelled, and on resume
("interrupted"). `Store::mark_started` sets `started_at` when a call's
hooks let it run.

**Going on after a restart.** On resume, the first open call is where rho
stopped. If it never started (its hooks were asking) or its tool is
`repeatable` (it only reads or asks: `Read`/`Search` kinds,
`AskUserQuestion`, `PresentPlan`, `RequestAccess`, `RequestFeature`), the
session is `resuming`: the next run first runs the open calls, then goes
on. ACP starts that run right after the `session/load` response, so the
question shows again; no model request is made before it is answered.
Otherwise the open calls are closed as interrupted and the session waits.

**Transcript.** `AcpFrontend` keeps what the session showed: text and
thinking merged, tool call updates (both for terminal and plain viewers),
terminal output by call, prompts. It keeps the current and the previous
epoch (`Frontend::compacted`). A stored session that opens records its
history into it (the previous epoch from the store, then the current one,
before the notices of its start); later the live events add to it. A
viewer that attaches gets the transcript and then the live updates, under
the `viewers` lock, so each update reaches it once. The store is not read
for an open session.

**Terminal output.** `Context` keeps what the running call streamed
(stderr in red, up to 1 MiB) and the agent saves it in the result's
`display` (`{"terminal": ...}`), dropped after 7 days like diffs. The
replay streams it into the card as it came.

**One process, one open.** `Store::open` takes `<file>.lock` for the
process; another process cannot open the database. `Hub::opening`
serializes opening stored sessions, so two connections share one session.
`rho acp --daemon` no longer starts a daemon; it fails if none runs.

## Consequences

- A restart loses only the response that was streaming and the output of
  the call that ran. The model gets its earlier work back, with an
  "interrupted" or "cancelled" result where a call did not finish.
- A cancelled response stays in the history with its finished results.
  Before, the model did not see it at all.
- `seq` counts blocks, so a message's `seq` has gaps; the `History` read
  action takes the next ten messages from a `seq`.
- `/redact` turns a thinking block with the phrase into marker text
  instead of dropping it, so the block count stays in line with the
  stored items.
- Viewers of a session that a client opens with `session/load` are no
  longer held (ADR 0052): they know the session id.
- Loading during a turn still does not apply new additional directories.
- A partly streamed response is lost on a restart; saving during the
  stream would need checkpoints.
- Opens of stored sessions wait for each other.
- A second rho on the same database (e.g. `rho acp` without `--daemon`
  while the daemon runs) cannot start sessions.

Code: `rho/src/store.rs` (`items`, `save_items`, `mark_started`,
`save_outputs`, `lock`), `rho/src/agent.rs` (`run_prompt`, `run_calls`,
`close_open_calls`, `resume_session`), `rho/src/agent/persist.rs`,
`rho/src/acp/frontend.rs` (`Transcript`), `rho/src/acp/load.rs`,
`rho/src/acp/hub.rs`, `rho/src/acp_ws.rs`.
