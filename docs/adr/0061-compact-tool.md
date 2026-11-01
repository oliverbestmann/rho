# 61. The model can compact with a Compact tool

- Status: Accepted
- Date: 2026-10-21

## Context

Compaction was only manual (`/compact`). The model often knows best when a
task is done and the earlier conversation is no longer needed, and the user
may ask it to compact in a prompt.

## Decision

- A built-in `Compact` tool (`rho/src/compact.rs`) with an optional `focus`.
  Its description tells the model to use it only if it is certain it does not
  need the earlier content anymore, or if the user asked for it.
- An optional `verbatim` text is shown to the summarizer (which is told to
  leave its content out) and appended unchanged to the summary under
  `## Notes`. The model uses it to keep important information exact.
- A hidden system note (`compact::size_note`) tells the model the context
  size from 200k tokens, and suggests the Compact tool from 300k. It also
  says to compact soon once the history is over 10 MB. Absolute numbers, not
  a share of the window, so 1M-token models get it too. It comes at the start
  of each run, and before the next request whenever the context crosses a
  100k-token step or the history (from 10 MB) another MB. It is appended to the
  history, so the prompt cache stays valid.
- Below 10% of the context window (the last request's prompt plus output)
  the tool does nothing and says so. With an unknown window it compacts.
- The tool only sets a flag on the `Context`. The agent loop compacts after
  all tool calls of the response have results, before the steering messages
  are added, so these are not summarized away. A stop skips the compaction;
  a new run clears a leftover flag.
- Compaction is the same as `/compact` (warm or cold path). Then the loop
  adds a system note ("You compacted the conversation ...", or the error if
  it failed) and the run goes on from the summary.

## Consequences

- A run can contain a compaction; the web and ACP clients see a new epoch
  in the middle of it.
- The tool list changed, so the first request after the update has a cold
  prompt cache.
