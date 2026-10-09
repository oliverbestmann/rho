# rho

A small ACP-native coding agent in Rust. Prototype.

Cargo workspace:

- `rho/` - the library: agent loop, providers, tools, ACP, CLI (`rho::run(rho::Options)`). No extensions.
- `agent/` - the `rho` binary: its extensions (`RipgrepGate`, `GoCheck`,
  `SessionTitle`, `BackgroundTasks`, `AskUser`, `PlanMode`) and a
  `main` that calls `rho::run` with its `rho::Options`.

- `rho/src/provider.rs` - `Provider` trait (streamed `Event`s) and a `provider/model` registry
- `rho/src/anthropic.rs` - Anthropic Messages API with Claude Pro/Max subscription shaping
- `rho/src/auth.rs` - OAuth PKCE login (copy-code flow), refresh, credential file
- `rho/src/tools.rs` - `Tool` trait and built-ins: `Read`, `ReadImage`, `Write`, `Bash`, `RequestAccess`
- `rho/src/edit.rs` - `Edit`: exact text replacements (pi's edit tool without fuzzy matching); the change goes to ACP clients as a diff
- `rho/src/agent.rs` - sessions and the agent loop (stream, run tools, repeat)
- `rho/src/extension.rs` - `Extension` hooks, `Context` session handle, `Frontend` trait
- `rho/src/store.rs` - session persistence in SQLite
- `rho/src/acp.rs` - Agent Client Protocol server (initialize, session/new, load, resume, list, close, prompt, cancel)

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

## Sessions

ACP sessions are saved to SQLite, `~/.config/rho/sessions.db`
(`RHO_SESSIONS_DB` overrides), from `session/new` on: history, title,
model, thinking level, profile, usage and cost. One-shot CLI runs are not saved.
Clients can list them (`session/list`, filtered by cwd), continue them
(`session/load` replays the conversation, `session/resume` does not) and
`session/close` them. Sandbox grants are not saved: a continued session
starts from `sandbox.json` again. Edit diffs are not stored either, so
replayed edits show only their output.

Every session opens its own connection; `Options::sessions_db` maps a cwd
to a database file (default: the same file for all). Tables are `STRICT`
with foreign keys and checks, in WAL mode so processes can share the file.
`/compact` keeps the old messages: it starts a new epoch, and a session's
history is the messages of its latest epoch. Each message also stores its
text and tool calls as plain text, ready for full-text search.

Extensions read the database through `ctx.store()` (`session`, `list`,
`history`); writing is up to rho. `SessionSetup::resumed` tells
`session_start` that a stored session continues (`SessionTitle` keeps its
title then).

## Sandbox

`Bash`, `Read`, `ReadImage`, `Write` and `Edit` run under Landlock (via the `nono` crate, Linux
only), deny by default. A session starts with system directories (`/nix/store`,
`/etc`, `/usr`, ...) read-only, a few `/dev` files read-write, and its
workspace directories read-write: the cwd plus ACP `additionalDirectories`.

More access is configured in `~/.config/rho/sandbox.json` (next to
`APPEND_SYSTEM.md`); paths are absolute or start with `~/`, an invalid file
stops rho at startup:

```json
{
  "read": ["~/.config/git/config", "~/.ssh/known_hosts", "~/.ssh/id_ed25519.pub"],
  "read_write": ["~/go", "~/.cache", "/tmp"]
}
```

`"disabled": true` turns the sandbox off. During a session the model can
ask for more with the `RequestAccess` tool; the user approves via ACP
`request_permission` (the CLI always denies) and the grant lasts for the
session. The `GrantCommands` extension (enabled in the `rho` binary) adds
`/grant-read <path>` and `/grant-write <path>` for the user; the model is
told about the grant with the next prompt. Extensions can use `ctx.grant` / `ctx.set_sandbox`, and run their own code
under the session's rules:

```rust
// file access on a throwaway thread restricted by Landlock
let text = ctx.run_sandboxed(move || std::fs::read_to_string(path)).await??;
// a child process, restricted before exec
let mut cmd = tokio::process::Command::new("go");
ctx.sandbox_command(&mut cmd)?;
```

Both are no-ops without a sandbox.

## Extensions

Implement `rho::extension::Extension` (all hooks optional) in a binary crate
and pass it to `rho::run`, like `agent/src/main.rs`. `rho::Options` also
sets providers, tools, the default model and the compaction model; its
`Default` is anthropic + the built-in tools + no extensions. A tool returns
`ToolOutput` (`.into()` from a `String`); set its `diff` to show a file
change to ACP clients.

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
| `command(name, args) -> Option<String>` | `registerCommand` | handle a `/name` the extension pushed to `SessionSetup::commands` (advertised over ACP); the reply goes to the user only |

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

`SessionTitle` names the session from the first prompt with haiku, in the
background. `BackgroundTasks` adds `BackgroundTask`, `BackgroundTaskList` and
`BackgroundTaskCancel`: commands run sandboxed in their own process group, the
result (exit status + last 10 lines) is sent with `trigger_turn`, and running
tasks are killed when the session ends.

## Profiles

A profile decides what the model may do. rho itself has one, `write`:
every tool, the full sandbox. Extensions add more in `session_start`
(`SessionSetup::profiles`):

```rust
session.profiles.push(Profile {
    id: "plan".into(),
    name: "Plan".into(),
    description: "Read-only: explore and plan, no changes".into(),
    hidden_tools: vec!["Write".into(), "Edit".into()],
    read_only: true,                  // sandboxed tools can only read
    instructions: Some("...".into()), // told to the model on switching here
});
```

With more than one profile, ACP clients get a `mode` config option (category
`mode`) to switch. Extensions and tools switch with `ctx.set_profile(id)`;
the client then gets a `config_option_update`. A switch applies from the
next model request, also in the middle of a run, and does not wait for a
running turn. Every request carries all tools, whatever the profile, so
a switch keeps the prompt cache (tools are its first part). Instead, the
history gets a note, appended to the next user message (prompt or tool
results): the new profile, which tools not to call now and which can be
called again, whether the sandbox is read-only, and the profile's
instructions. A new session gets one on its first request if its profile
restricts anything (with `PlanMode`, `write` hides `ExitPlanMode`). Calls to
hidden tools fail. `read_only` turns every read-write sandbox rule into a
read-only one (except `/dev/null` and friends), for `Bash`, `Write`,
background tasks and anything else using `ctx.run_sandboxed` /
`ctx.sandbox_command`; grants made meanwhile are kept for later. Without a
sandbox (`"disabled": true`) only the hidden tools apply. The profile is
saved with the session.

`PlanMode` (in the `rho` binary) adds `plan`, like Claude Code's plan mode:
`Write` and `Edit` are hidden and the sandbox is read-only. The model can
enter it with `EnterPlanMode`. It leaves with `ExitPlanMode { plan }`: the
plan is shown on the tool call and the user is asked to approve it
(`session/request_permission`). Approved: back to `write`, the model starts
implementing. Rejected: the session stays in plan mode and the run stops, so
the user can say what to change. Picking `write` in the selector needs no
approval.

## Images

rho advertises `promptCapabilities.image`, so ACP clients can attach images
(`image` blocks, or embedded `image/*` blob resources) to prompts. They go to
the model as Anthropic base64 image blocks. JPEG, PNG, GIF and WebP up to 5 MB
are accepted. Anything else fails the prompt before it reaches the history,
because a rejected image in the history would fail every later request.
Images are saved with the session and replayed by `session/load`; compaction
and search see them as `[image]`.

The `ReadImage` tool lets the model look at image files. It reads them
sandboxed like `Read`, detects the type from the file's bytes, and applies the
same type and 5 MB limits. The image goes back in the `tool_result` (as
`[text, image]` content blocks) and is shown in the client's tool call card.
Tools can return images too: set `ToolOutput::images`.

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
