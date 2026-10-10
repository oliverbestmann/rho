# 28. `PREAMBLE_SYSTEM.md` created with the built-in preamble

- Status: Accepted
- Date: 2026-10-10
- Amends: ADR 0027

## Context

`PREAMBLE_SYSTEM.md` was created empty (ADR 0027), so to change the
preamble you first had to find the built-in one in the source.

## Decision

- On startup, a missing `PREAMBLE_SYSTEM.md` is written with
  `prompt::PREAMBLE`. An existing file is never touched.
- An empty file still means the built-in preamble.

## Consequences

- The preamble can be edited in place, and is fixed: later changes to the
  built-in preamble do not reach an existing file. Empty or delete it to
  follow them again.

Code: `prompt::ensure_files` in `rho/src/prompt.rs`.
