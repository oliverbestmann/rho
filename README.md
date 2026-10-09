# rho

A small ACP-native coding agent in Rust. Prototype.

Cargo workspace:

- `rho/` - the library: agent loop, providers, tools, ACP, CLI (`rho::run(rho::Options)`). No extensions.
- `agent/` - the `rho` binary: its extensions (`RipgrepGate`, `GoCheck`) and a
  `main` that calls `rho::run` with its `rho::Options`.

- `rho/src/provider.rs` - `Provider` trait (streamed `Event`s) and a `provider/model` registry
- `rho/src/anthropic.rs` - Anthropic Messages API with Claude Pro/Max subscription shaping
- `rho/src/auth.rs` - OAuth PKCE login (copy-code flow), refresh, credential file
- `rho/src/tools.rs` - `Tool` trait and built-ins: `Read`, `Write`, `Bash`
- `rho/src/agent.rs` - sessions and the agent loop (stream, run tools, repeat)
- `rho/src/extension.rs` - `Extension` hooks, `Context` session handle, `Frontend` trait
- `rho/src/acp.rs` - Agent Client Protocol server (initialize, session/new, session/prompt, session/cancel)

## Use

```sh
nix develop path:.        # or `nix develop` once tracked by git
cargo build

rho login                                  # sign in with your Claude subscription
rho "what does src/main.rs do?"            # one-shot, prints the answer
rho -m anthropic/claude-opus-4-8 "..."     # pick a model
rho -t low "..."                           # thinking: off|low|medium|high|xhigh|max (default high)
rho acp                                    # ACP on stdio
```

The one-shot mode prints the answer to stdout, and thinking (dimmed), tool calls
and a token usage line to stderr. In ACP mode, thinking goes out as
`agent_thought_chunk` and usage as `usage_update` after every model request.

## Thinking and caching

- Adaptive thinking with `output_config.effort` (Claude 4.6+ only; no
  `budget_tokens` models). Thinking blocks are kept in history with their
  signatures.
- Cache breakpoints: last tool, last system block, last block of the final
  user message (5 minute TTL). Prompts below the model's minimum cacheable
  size (1-4k tokens) are not cached. Every request reads the full previous
  prefix.

To check caching and thinking live (uses subscription quota):

```sh
RHO_AUTH_FILE=$PWD/_rho-auth.json cargo test -p rho --test live -- --ignored --nocapture
```

Credentials live in `~/.config/rho/auth.json` (`RHO_AUTH_FILE` overrides). The
layout is the same as pi's `~/.pi/agent/auth.json`.

## System prompt

Built per session like pi's (`rho/src/prompt.rs`): preamble, `<tools>`, `<rules>`,
then `<addendum>` from `~/.config/rho/APPEND_SYSTEM.md`, `<project_context>`
from `AGENTS.md`/`CLAUDE.md` (config dir, then cwd ancestors outermost first)
and `<cwd>`. `~/.config/rho/SYSTEM.md` replaces preamble, tools and rules.
To reuse pi's addendum:

    ln -s ~/.dotfiles/pia/config/APPEND_SYSTEM.md ~/.config/rho/APPEND_SYSTEM.md

## Compaction

`/compact [focus]` in Zed (advertised via ACP `available_commands_update`)
replaces the history with a pi-style structured summary (`rho/src/compact.rs`).
If the last request was less than 4.5 minutes ago, the prompt cache is
assumed warm: the session model summarizes with the exact same prefix, so the
history is billed as cache reads. Otherwise `RHO_COMPACT_MODEL` (default
`anthropic/claude-haiku-5-5`) gets the conversation as text, without tool
outputs and with tool arguments cut to 2000 chars. The notice shows which path
ran and its token usage.

## Extensions

Implement `rho::extension::Extension` (all hooks optional) in a binary crate
and pass it to `rho::run`, like `agent/src/main.rs`. `rho::Options` also
sets providers, tools, the default model and the compaction model; its
`Default` is anthropic + `Read`/`Write`/`Bash` + no extensions.

Hooks and `Tool::run` are plain `async fn` (no `async-trait`):

```rust
impl Extension for RipgrepGate {
    async fn tool_call(&self, _ctx: &Context, call: &mut ToolCall) -> Decision { ... }
}
```

Since `async fn` traits cannot be `dyn` yet, rho stores them through the
object-safe twins `DynExtension` / `DynTool`, implemented automatically;
`Arc::new(MyExtension)` converts on its own.

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rho::run(rho::Options {
        extensions: vec![Arc::new(RipgrepGate), Arc::new(GoCheck::default())],
        ..Default::default()
    })
    .await
}
```

| rho hook | pi event | use |
|---|---|---|
| `session_start(&mut SessionSetup)` | `session_start` + `registerTool` | add tools, edit the system prompt |
| `session_end(&Context)` | `session_shutdown` | cleanup (runs when the session is dropped) |
| `before_prompt(&mut PromptSetup)` | `before_agent_start` | add context, per-run system prompt |
| `before_request(&mut Request)` | `context` / `before_provider_request` | prune or edit the request |
| `tool_call(&mut ToolCall) -> Decision` | `tool_call` | block (reason goes to the model) or edit input |
| `tool_result(&ToolCall, &mut ToolOutput)` | `tool_result` | rewrite output |
| `turn_end(&TurnEnd)` | `turn_end` | observe content, results, usage |
| `before_settle(&Settle) -> Option<String>` | `agent_before_settle` / `agent_settled` | continue the run with a message |

`Context` is passed to hooks and tools and can be cloned into background tasks:

- `send(text, trigger_turn)` - message for the model (pi `sendMessage`). During a
  run it is added at the next tool-result boundary. When idle it waits for the
  next prompt, or with `trigger_turn` starts a run right away; in ACP that run
  streams `session/update`s without an open prompt, as pi-acp does.
- `notify(title, text, is_error)` - shown to the user only (ACP: a finished tool call).
- `confirm(tool_call_id, title)` - ACP `session/request_permission`; `false` without a UI (CLI).
- `complete("provider/model", system, prompt)` - one-off LLM call, e.g. session titles.
- `set_title(title)` - ACP `session_info_update`.
- `stop(reason)` - end the current run cleanly (pi `ctx.abort()`), from any hook,
  tool or background task. Checked after `before_prompt` (prompt dropped, like
  pi `input` -> `handled`), after `before_request` (request not sent), between
  tool calls (rest skipped; with `Decision::Block` this is pi's `terminate`)
  and before continuing. The run returns `StopReason::Stopped` (ACP: `cancelled`).

`GoCheck` continues the run at most 3 times in a row per prompt; pi's
go-check has no limit (pi itself has no turn limit, rho stops at 50).

## Zed

```json
{
  "agent_servers": {
    "rho": { "command": "/path/to/rho", "args": ["acp"] }
  }
}
```

## Notes on subscription auth

Based on pi-ai's Anthropic OAuth and `@gotgenes/pi-anthropic-auth`, not
`ai-connect`: ai-connect only does the OAuth handshake, uses the old
`console.anthropic.com` token endpoint and fewer scopes, and does not shape
requests. Subscription requests need the Claude Code headers, the identity
system block and the `x-anthropic-billing-header` block, or Anthropic bills
them as extra usage.
