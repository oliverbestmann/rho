# 66. Config from `config.d`, merged

- Status: Accepted
- Date: 2026-10-28
- Amends: ADR 0012, 0026, 0027, 0028, 0031, 0033, 0040

## Context

Each subsystem had its own file in `~/.config/rho`: `sandbox.json`,
`features.json`, `models.json`, `mcp.json`, `PREAMBLE_SYSTEM.md` and
`APPEND_SYSTEM.md`. Settings could not be split across files, for example
shared settings from dotfiles plus work-only features on one machine.

## Decision

- Config lives in `~/.config/rho/config.d` (`rho/src/config.rs`).
- Its `*.json` files are applied in file name order as JSON merge patches
  (RFC 7396). Objects merge recursively, other values (arrays too) replace,
  and `null` deletes a key. Hidden files are skipped. Every file must hold
  a JSON object.
- The merged config has one key per subsystem: `sandbox`, `features`,
  `models` and `mcps`. `config::section` reads a key with serde, and
  extensions read their own key the same way (`Mcp` reads `mcps`).
- Lists that several files should extend are maps now:
  - `sandbox.paths` and a feature's `paths` map a path to `"read"` or
    `"readwrite"`, replacing `read` / `read_write`.
  - `features` maps id to feature.
  - `models` maps rule path to rule, and the `rules` wrapper is gone.
  - `mcps` maps name to server, with servers as in Claude's `mcpServers`.
- `*PREAMBLE_SYSTEM.md` and `*APPEND_SYSTEM.md` in `config.d` are joined in
  file name order with a blank line between them, and empty files are
  skipped. Preamble files replace the built-in preamble. They are read for
  every session, as before.
- On startup, `00-rho.PREAMBLE_SYSTEM.md` is written with the built-in
  preamble when `config.d` has no preamble file (ADR 0028 moved into
  `config.d`). An empty `APPEND_SYSTEM.md` is no longer created.
- If one of the old files is still in `~/.config/rho`, rho stops at startup
  and names where its content goes. There is no compatibility code.
- State that rho writes itself stays in `~/.config/rho`: `auth.json`,
  `project_context.json`, `acp-token`, `sessions.db`, `memory.db`,
  `daemon/` and `skills/`. So does the global `AGENTS.md`.

## Consequences

- A typo in a top-level key (such as `"sandbx"`) is ignored, because
  extensions own their keys and rho does not know them all. Within a
  section, unknown fields are still errors.
- Errors in a section name the section (`config.d: models: ...`), not the
  file or line it came from.
- Two rules for the same path can no longer exist, so the old "later rule
  wins on equal paths" ordering is gone. `~/x` and its expanded path are
  still two keys; the later one in key order wins.
- Features are listed in id order, not file order.
- The JSON config is still read only at startup.
