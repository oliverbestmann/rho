# 33. Features: switchable bundles of sandbox access and tools

- Status: Accepted
- Date: 2026-10-12

## Context

Some work needs access the session should not have all the time, e.g.
`~/.ssh` for git over SSH, or tools that reach external systems. Granting
paths one at a time (ADR 0013) is tedious and cannot be taken back. Profiles
(ADR 0016) are exclusive, so they cannot be combined like this.

## Decision

- A `Feature` has an id, a description, `enabled` (on at session start),
  `read` / `read_write` paths, `tools` and optional `instructions`. They come
  from `~/.config/rho/features.json` (a list; paths absolute or `~/`; an
  invalid file stops rho at startup) and from extensions through
  `SessionSetup::features`.
- Any number of features can be on at once. The user switches them with
  `/enable a b` and `/disable a` and lists them with `/features`
  (`FeatureCommands`); code uses `ctx.set_feature(id, on)`.
- The system prompt lists the features (`<features>`: id, description,
  paths, tools), but not whether they are on, so it stays the same and the
  prompt cache holds. The model can ask to turn one on with
  `RequestFeature { feature, reason }`. The user approves it like
  `RequestAccess`, and it fails closed (ADR 0013). Without features there is
  no list and no tool. `FeatureCommands` must come after extensions that
  add features.
- Tools: a tool listed by any feature can only be called while one of
  those features is on. It stays in every request (ADR 0007); the agent
  blocks the call. The profile check comes first: a feature cannot
  unlock a tool the profile hides.
- Sandbox: feature paths are a separate layer, added in
  `effective_sandbox` on top of the starting policy and grants, before a
  read-only profile is applied. Turning a feature off removes its layer;
  grants for the same paths stay.
- The model is told in the same `<system-reminder>` as profile changes:
  features turned on (with their paths and instructions) or off, and the
  tools it can or cannot call now.
- The ids of the features that are on are stored with the session. On
  resume they replace the defaults; unknown ids get a notice.

## Consequences

- Turning a feature off does not affect processes that are already
  running: Landlock rules are fixed at spawn (e.g. background tasks keep
  their access).
- Unlike grants (ADR 0012), feature state is stored with the session.
  Sessions saved before this change resume with no features on.
- Gated tool schemas are sent even while their feature is off.
- Tool names in `features.json` are not checked against the session's
  tools.

Code: `rho/src/feature.rs`, `Context::set_feature` / `tool_available` /
`effective_sandbox`, `profile_note` and `decide_and_run` in `rho/src/agent.rs`.
