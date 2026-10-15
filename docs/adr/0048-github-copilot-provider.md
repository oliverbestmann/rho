# 48. GitHub Copilot as a second provider

- Status: Accepted
- Date: 2026-10-15
- Amends: ADR 0005

## Context

Work repositories may only send code to GitHub Copilot (ADR 0040), and a
Copilot subscription also serves Claude and GPT models. ADR 0005 kept the
core types in the Anthropic wire format with one provider. pi-ai has a
`github-copilot` provider to follow (ADR 0004).

## Decision

`Copilot` (`rho/src/copilot.rs`) is registered next to `anthropic` in
`Options::default()`, under the name `github-copilot`.

- Login: `rho login github-copilot` runs GitHub's device flow with VS Code
  Copilot Chat's client id. The GitHub token is exchanged for a short-lived
  Copilot token, stored in pi's `auth.json` layout (`access` = Copilot
  token, `refresh` = GitHub token) and refreshed like the Anthropic one.
- The API host comes from the token's `proxy-ep` field, so individual,
  business and enterprise plans work.
- The model id picks the API: `claude*` goes to the Anthropic Messages API
  with the body from `anthropic::build_body` (no billing header), every
  other model to the OpenAI Responses API (`rho/src/openai.rs`).
- The Responses API is used statelessly (`store: false`). A reasoning item
  comes back encrypted and is kept as a `Content::Thinking` whose signature
  is the item's JSON (it starts with `{`, Anthropic signatures never do).
  Each API leaves out the other's thinking blocks.
- Requests carry VS Code Copilot Chat's headers and `X-Initiator`: `agent`
  when the last message holds tool results, else `user`.
  `copilot-vision-request` is set when the request has images.
- The model list (ids, context windows, prices) is hardcoded from GitHub's
  "Models and pricing" page. Copilot ids use dots
  (`claude-haiku-5.5`), Anthropic's use dashes (`claude-haiku-5-5`).
- A `data: [DONE]` line ends Copilot streams on both APIs and is ignored.

## Consequences

- Other providers no longer need to be hypothetical: anything that is not
  the Anthropic format is translated in its provider (here `openai.rs`).
- The model list has to be synced by hand when Copilot changes it.
- Depends on undocumented Copilot behaviour, like ADR 0006 for Anthropic.
- Not supported: Gemini and other Chat Completions models, GitHub
  Enterprise Server domains, enabling model policies at login.
- Effort names are sent as they are; a model that lacks `xhigh` or `max`
  rejects them.

Code: `rho/src/copilot.rs`, `rho/src/openai.rs`, `copilot::start_login` /
`finish_login` in `rho/src/cli.rs`.
