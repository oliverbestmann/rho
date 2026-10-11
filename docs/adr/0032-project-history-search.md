# 32. Project history search

- Status: Accepted
- Date: 2026-10-11

## Context

Earlier sessions hold decisions, failed attempts and fixes that the agent
can't see. The session store keeps every message, also the ones that
compaction removed (ADR 0015). The agent should be able to search a
project's sessions by meaning and by keywords, but no sessions from
outside the project.

## Decision

- An extension, `History` (`agent/src/history.rs`). It offers a `History`
  tool only if a directory at or above the cwd has a `.rho-project-root`
  file. The nearest such directory is the project root. Sessions whose cwd
  is the root or below it (compared by path components) are searched.
  The tool description names the root. Without a marker there is no
  fallback to the cwd: a cwd alone doesn't say where a project starts,
  and a guess could mix in other projects' sessions.
- The store gets one read method, `Store::messages`, which returns all
  messages with their epoch.
- The index is `history.db` in `$XDG_CACHE_HOME/rho`. It's derived data:
  deleting it means a rebuild. `sessions.db` is not changed.
- Each epoch is rendered as `User: …` / `Assistant: …` text: text and tool
  calls (cut to 300 bytes), no thinking, no tool output. It is cut into
  windows of ~800 bytes at whitespace, each starting ~150 bytes before the
  previous one ends. A window depends only on the text before its end, so
  new messages change only the last window.
- Chunks are embedded with the session title in front, using the same
  model as `Memory` (one instance per process). Embeddings are reused by
  the SHA-256 of what was embedded. FTS5 (external content, kept in sync by
  triggers) indexes the same chunks.
- Search ranks with sqlite-vec's `vec_distance_cosine` and FTS5's rank,
  50 candidates each, fused by reciprocal rank (k = 60). The mode
  `semantic` or `keyword` uses one ranking only. The current session's
  current epoch is left out, because it is already in context.
- Hits on consecutive chunks of the same epoch are merged into one result,
  ranked by its best chunk, with the overlap removed. `limit` (default 10,
  at most 50) applies to the merged results.
- With `summarizeOver: n` and more than n results, a cheap model
  (`History::with_model`, default `SessionTitle`'s Haiku) answers the query from the results and cites
  session and seq; the agent gets only the answer. If the call fails, the
  results are shown.
- `read` shows ten messages of a session from a given `seq` on, with tool
  outputs cut, only for sessions in the project.
- Sessions whose `updated_at` or title changed are indexed after every run
  (`run_end`, in the background) and before every search. An indexing
  error after a run is shown as a notice.

## Consequences

- The first search in a project with many sessions waits for the index to
  be built.
- Offline, before the embedding model has been downloaded, every run ends
  with a "History not indexed" notice.
- The vector search is a full scan of the project's chunks. That is fine
  for tens of thousands of chunks. Beyond that, use a `vec0` table.
- MiniLM is weak on code and identifiers; the keyword ranking makes up for
  part of that.
- Only sessions in the current session's database are found
  (`Options::sessions_db`).
- No cheap model annotates the chunks; the title in front is the only
  added context.
