# 16. Profiles restrict tools and sandbox, told to the model in the history

- Status: Accepted
- Date: 2026-10-09

## Context

Sometimes the agent should only explore (plan mode), sometimes change
things. The user should be able to switch from the editor, and extensions
or tools should be able to switch too, also during a run. Removing tools
from the request would break the prompt cache (tools are the start of the
prefix, ADR 0007); the first version did that and was changed in
`f782c03`.

## Decision

- A `Profile` has an id, name, description, `hidden_tools`, `read_only`
  and optional `instructions`. rho itself has one, `write` (everything).
  Extensions add more, or hide their own tools in `write`, through
  `SessionSetup::profiles`.
- Hidden tools stay in every request. Instead, the agent checks
  `profile.shows(name)` before running a tool; a hidden tool's call fails
  with "X is not available in the Y profile".
- The model learns about the profile from a `<system-reminder>` note
  appended to the next user message (prompt or tool results): the new
  profile, which tools not to call now, which can be called again, whether
  the sandbox is read-only, and the instructions. It is sent once per
  change, and on a session's first request only if the profile restricts
  something.
- `read_only` makes every read-write sandbox rule read-only (except the
  devices) for all sandboxed code. Grants made meanwhile are kept for
  later.
- Switching (`Context::set_profile`, or the ACP `mode` config option)
  does not wait for the running turn: the tool check and the sandbox use
  the new profile right away (also for the remaining tool calls of the
  current turn), and the model is told with the next model request. The
  client gets a `config_option_update`.
- The profile is stored with the session; a resumed session whose profile
  no longer exists falls back to the first one; the user gets a notice and
  the model is told with the next request.

## Consequences

- Restrictions rely on the agent's check and the sandbox, not on the model
  not seeing the tool.
- Without a sandbox, only hidden tools apply; `read_only` does nothing.
- With only one profile, ACP clients get no mode selector.

Code: `Profile` and `Context::set_profile` in `rho/src/extension.rs`,
`profile_note` and `call_tool` in `rho/src/agent.rs`, `Policy::read_only`.
