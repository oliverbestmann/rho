# 50. Tools say how their calls are shown

- Status: Accepted
- Date: 2026-10-15
- Amends: ADR 0044

## Context

`rho/src/acp.rs` picked a tool call's icon (`ToolKind`) and whether its
`description` input titles the card (`DESCRIBED`) from lists of tool names,
most of them tools of the `rho` binary. The library had to know its
extensions (against ADR 0002), and a new tool needed an entry in both lists.

## Decision

- `Tool` gets two methods with defaults: `kind() -> ToolKind` (`Other`) and
  `titled() -> bool` (`false`). `ToolKind` is rho's own enum (`Read`,
  `Edit`, `Execute`, `Search`, `Fetch`, `SwitchMode`, `Other`), mapped to
  ACP's in `acp.rs`.
- `AgentEvent::ToolStart` carries both, so frontends need no lookup.
- For replays, the ACP frontend keeps the session's tools by name. Replays
  while a turn holds the session now show tool descriptions too.
- Wrappers such as `Secrets`' `AllowMasked` pass both on.

## Consequences

- A tool that should get a description title sets `titled` next to its
  `description` field.
- Tools that are gone from a session replay with `Other` and the input-key
  title.

Code: `Tool` in `rho/src/tools.rs`, `to_update` and `replay_history` in
`rho/src/acp.rs`.
