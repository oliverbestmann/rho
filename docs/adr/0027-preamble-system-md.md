# 27. `PREAMBLE_SYSTEM.md` replaces only the preamble

- Status: Accepted
- Date: 2026-10-10
- Amends: ADR 0023, ADR 0026

## Context

`SYSTEM.md` replaced the preamble, `<tools>` and `<rules>` (ADR 0023, after
pi). Since ADR 0025 the tool list and rules are built per session from the
tools present, so a `SYSTEM.md` swapped them for fixed text that drifts as
tools change. With the file created empty on startup (ADR 0026), the first
line written into it silently dropped them.

## Decision

- `SYSTEM.md` is no longer read. `PREAMBLE_SYSTEM.md` in the config dir
  replaces only the preamble; `<tools>` and `<rules>` are always built.
- `PREAMBLE_SYSTEM.md` is created empty on startup instead of `SYSTEM.md`;
  empty means the built-in preamble.

## Consequences

- The whole prompt can no longer be replaced; diverges from pi.
- A leftover `SYSTEM.md` is ignored and can be deleted.

Code: `prompt::build`, `prompt::ensure_files` in `rho/src/prompt.rs`.
