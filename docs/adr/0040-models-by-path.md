# 40. Allowed, default and compaction models by path

- Status: Accepted
- Date: 2026-10-15

## Context

Some projects may only send code to some providers (work repositories:
GitHub Copilot only), and different projects want different default models.
One daemon (ADR 0038) serves all projects, so a startup flag cannot express
this.

Zed applies its `default_config_options` after every `session/new`,
`session/load` and `session/resume` with `session/set_config_option`, and
saves every pick in the model picker back to them. It only applies a value
that the agent lists and skips others with a log warning. It finds the model
option by category, not by id.

## Decision

`~/.config/rho/models.json` has rules with a `path` and optional `allow`,
`default` and `compact` (`rho/src/models.rs`). Each key is inherited on its
own: the longest rule path containing the session cwd that sets it wins. It
is read once at startup.

- **One enforcement point:** each session gets the agent's `Registry`
  restricted to the allowed models. `models()` filters, `resolve()` fails.
  The model picker, model switches, compaction and `Context::complete` all
  use it, so helpers with a forbidden hardcoded model fail (hard fail, no
  quiet fallback).
- **Start model:** an explicit `-m` / `RHO_MODEL` wins, and a session whose
  path forbids it fails to start. Otherwise the path `default`, then the
  built-in default. The same for `RHO_COMPACT_MODEL` and `compact`.
- **Startup check:** for every rule path, the non-explicit default and
  compaction model must be allowed. The policy only changes at rule paths,
  so this covers every cwd.
- **Resume:** the stored model wins; if it is forbidden now, the existing
  "Model unavailable" notice falls back to the start model.
- **Zed:** the model option's config id is `model:<rule path as written>`
  under the longest rule with `allow` or `default`, else `model`. Zed
  remembers its default per config id, so a pick in one area does not
  override the path default of another. A remembered model that is
  forbidden is not listed, so Zed skips it.
- `GitGate` summarizes with the first listed (so allowed) `claude-haiku-5-5`
  or `gpt-6-luna` of any provider, else with the session's model. Before,
  it asked `openai/gpt-6-luna`, a provider rho does not register.

## Consequences

- Only the cwd counts; additional directories do not tighten the limit.
- Changing `models.json` needs a restart (of the daemon).
- Renaming a rule path changes the config id, so Zed forgets the pick
  remembered for it.
- With an explicit `-m` the daemon refuses sessions in paths that forbid
  that model; drop `-m` and use `default` rules instead.
- `History` answers (`summarizeOver`) and compaction fail in paths that
  forbid their models unless `compact` is set; `History` then shows the
  plain results.
