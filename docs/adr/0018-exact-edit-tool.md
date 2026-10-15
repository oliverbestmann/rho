# 18. `Edit` does exact replacements, no fuzzy matching

- Status: Accepted, amended by 0041
- Date: 2026-10-09

## Context

Rewriting whole files with `Write` is slow, expensive and hides what
changed. pi's `edit` tool does targeted text replacements and also accepts
fuzzy matches, which can silently change the wrong place.

## Decision

- `Edit { path, edits: [{oldText, newText}] }` with pi's semantics, but
  matching is exact:
  - `edits` must not be empty; every `oldText` must occur exactly once and
    must not be empty;
  - all edits match against the **original** file, not incrementally;
    overlapping edits are rejected;
  - a call whose edits leave the file unchanged is an error.
  Error messages tell the model how to fix the call.
- Line endings and BOM are kept: matching happens on LF-normalized text;
  a leading BOM is restored on write, and if the file contained any CRLF,
  every line is written with CRLF (mixed endings become CRLF).
- Odd input shapes the models send are accepted, as pi does: `edits` as a
  JSON string or a single object, or `oldText`/`newText` at the top level.
- Read, replace and write happen together on one sandboxed thread
  (ADR 0012).
- The tool returns a `Diff` (path, old, new) that ACP clients show as a
  diff. It is not sent to the model. (It was not stored either; since ADR
  0041 it is kept for 7 days and replayed.)
- The system prompt adds rules for using `Edit` when the tool is present.

## Consequences

- More failed calls when the model's `oldText` is slightly off, but no
  wrong edits; the model retries with the exact text.
- Replayed sessions show edits without their diffs (until ADR 0041).

Code: `rho/src/edit.rs`.
