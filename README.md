# rho

A small ACP-native coding agent in Rust. Prototype.

## Features

- ACP agent for Zed and other clients, plus a one-shot CLI. No TUI.
- Claude through a Pro/Max subscription (OAuth), with adaptive thinking and
  prompt caching.
- Built-in tools: `Read`, `ReadImage`, `Write`, `Edit` (exact replacements),
  `Bash`, `RequestAccess`.
- Deny-by-default Landlock sandbox per session. The user approves more
  access.
- Sessions in SQLite: list, load, resume, close. Compaction keeps the old
  messages.
- Profiles and a plan mode with a read-only sandbox.
- Features: bundles of sandbox access and tools, turned on with `/enable`.
- Extensions with pi-style hooks, compiled into the binary. The `rho`
  binary brings a git commit/push gate, Go checks and docs, `Grep`, URL
  fetching, local memories, search over a project's past sessions, MCP
  tools over HTTP, background tasks and image input.

## Limitations

- Only Anthropic, and only models with adaptive thinking (Claude 4.6+).
  Subscription billing depends on undocumented request shaping that follows
  pi-ai.
- The sandbox is Linux only; elsewhere it must be disabled. Background tasks
  are Unix only. The network is not sandboxed, and extension code (`Memory`,
  `History`, `Mcp`) runs outside the sandbox. `GitGate` matches command
  text, so it is a guardrail, not a boundary.
- Without a UI (the CLI) git mutations, access requests and plans are always
  denied. One-shot runs are not saved and don't wait for background tasks.
- Changing the set of extensions needs a rebuild. Hooks see whole turns, not
  streamed text.
- Compaction is manual (`/compact`). A run stops after 50 model requests.
- Prompt files apply to new sessions only. A session is bound to its cwd;
  sandbox grants and edit diffs are not saved. The session database only
  grows.
- Builds need `nix develop` (onnxruntime). The first `Memory` or `History`
  use downloads the embedding model.
- `Mcp`: Streamable HTTP only. No stdio servers, OAuth, SSE transport,
  resources or prompts. A changed tool list needs a restart.
- `History`: needs a `.rho-project-root` file. The first search in a large
  project waits for the index. Vector search is a full scan, and the model
  is weak on code. Only one session database is searched.
- ACP over WebSocket: no TLS (put a `wss://` proxy or tunnel in front), one
  shared token. A connection that loads a session while a turn runs gets
  the saved history without tool descriptions, and misses the chunks
  already streamed of the message in progress. A daemon restart ends all
  running turns; clients continue with `session/load`.
- Images: JPEG, PNG, GIF and WebP up to 5 MB.

The reasons behind these are in [docs/adr](docs/adr/README.md).

## Layout

Cargo workspace:

- `rho/` - the library: agent loop, providers, tools, ACP, CLI (`rho::run(rho::Options)`). No extensions.
- `agent/` - the `rho` binary: its extensions (`GitGate`,
  `GoCheck`, `RipgrepFind`, `GoDoc`, `FetchUrl`, `Mcp`, `Memory`, `History`, `SessionTitle`,
  `BackgroundTasks`, `ContextNotice`, `AskUser`, `PlanMode`, `Ponytail`,
  `ProjectContext`, `Secrets`) and a `main` that calls
  `rho::run` with its `rho::Options`.

- `rho/src/provider.rs` - `Provider` trait (streamed `Event`s) and a `provider/model` registry
- `rho/src/anthropic.rs` - Anthropic Messages API with Claude Pro/Max subscription shaping
- `rho/src/auth.rs` - OAuth PKCE login (copy-code flow), refresh, credential file
- `rho/src/tools.rs` - `Tool` trait and built-ins: `Read`, `ReadImage`, `Write`, `Bash`, `RequestAccess`
- `rho/src/edit.rs` - `Edit`: exact text replacements (pi's edit tool without fuzzy matching); the change goes to ACP clients as a diff
- `rho/src/agent.rs` - sessions and the agent loop (stream, run tools, repeat)
- `rho/src/extension.rs` - `Extension` hooks, `Context` session handle, `Frontend` trait
- `rho/src/store.rs` - session persistence in SQLite
- `rho/src/acp.rs` - Agent Client Protocol server (initialize, session/new, load, resume, list, close, prompt, cancel)
- `rho/src/acp_ws.rs` - ACP over WebSocket: `rho daemon` (Unix socket, `--listen` with a bearer token) and `rho acp --daemon`

## Use

```sh
nix develop path:.        # or `nix develop` once tracked by git
cargo build

rho login                                  # sign in with your Claude subscription
rho "what does src/main.rs do?"            # one-shot, prints the answer
rho -m anthropic/claude-opus-4-8 "..."     # pick a model
rho -t low "..."                           # thinking: off|low|medium|high|xhigh|max (default high)
rho acp                                    # ACP on stdio
rho acp --daemon                           # ACP on stdio, through the shared daemon
rho daemon --listen 127.0.0.1:8788         # the daemon, also on ws://127.0.0.1:8788/
```

`RHO_MODEL` and `RHO_THINKING` set the defaults for `-m` and `-t`.

The one-shot mode prints the answer to stdout, and thinking (dimmed), tool calls
and a token usage line to stderr. In ACP mode, thinking goes out as
`agent_thought_chunk` and usage as `usage_update` after every model request.
After each run a "Turn" notice sums up its requests, tokens, time and cost.

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

Built per session like pi's (`rho/src/prompt.rs`), after the extensions'
`session_start`: preamble, `<tools>` (every tool, also the extensions'),
`<rules>` (each tool's `Tool::rules`, e.g. `Grep` over `rg` in `Bash`),
then `<addendum>` from `~/.config/rho/APPEND_SYSTEM.md`, `<project_context>`
from the config dir's `AGENTS.md`/`CLAUDE.md` and `<cwd>`, then what
extensions added to `SessionSetup::system`. Context files in the cwd and its
parents are added last, by the `ProjectContext` extension, only if the user
approves them; the answer is remembered per directory in
`~/.config/rho/project_context.json`.
`~/.config/rho/PREAMBLE_SYSTEM.md` replaces the preamble. It and
`APPEND_SYSTEM.md` are created on startup if missing, the first with the
built-in preamble, the second empty; empty files are ignored.
To reuse pi's addendum:

    ln -sf ~/.dotfiles/pia/config/APPEND_SYSTEM.md ~/.config/rho/APPEND_SYSTEM.md

## Compaction

`/compact [focus]` in Zed (advertised via ACP `available_commands_update`)
replaces the history with a pi-style structured summary (`rho/src/compact.rs`).
If the last request was less than 4.5 minutes ago, the prompt cache is
assumed warm: the session model summarizes with the exact same prefix, so the
history is billed as cache reads. Otherwise `RHO_COMPACT_MODEL` or the path's
`compact` model (`models.json`) gets the conversation as text, without tool
outputs and with tool arguments cut to 2000 chars; without one, the session
model does. The notice shows which path
ran and its token usage.

## Models by path

`~/.config/rho/models.json` limits the models a session may use, and sets the
model it starts with and the compaction model, by the session's cwd
(`rho/src/models.rs`). It is read at startup; an invalid file stops rho.

```json
{
  "rules": [
    { "path": "~/github", "allow": ["github-copilot/*"],
      "default": "github-copilot/claude-sonnet-5.5",
      "compact": "github-copilot/claude-haiku-5.5" },
    { "path": "~/github/sandbox-repo", "allow": ["*"] }
  ]
}
```

- `allow`: `*`, `provider/*` or `provider/model`. Without a matching rule
  every model is allowed.
- `allow`, `default` and `compact` are inherited separately: for each, the
  longest rule path containing the cwd that sets it wins.
- `-m` / `RHO_MODEL` and `RHO_COMPACT_MODEL` win over `default` and
  `compact`; a session whose path forbids them fails to start. At startup rho
  checks that every rule path allows its non-explicit default and compaction
  model.
- Everything goes through the session's restricted `Registry`: the model
  picker lists only allowed models, switching to others fails, and so do
  compaction and extension calls (`Context::complete`) with a forbidden model.
- Resuming a session whose model is forbidden now falls back to the start
  model, with a notice.
- The model option's ACP config id is `model:<rule path>` under a rule with
  `allow` or `default` (else `model`), so Zed's `default_config_options`
  remember a pick per rule (ADR 0040).

## Sessions

ACP sessions are saved to SQLite, `~/.config/rho/sessions.db`
(`RHO_SESSIONS_DB` overrides), from `session/new` on: history, title,
model, thinking level, profile, usage and cost. One-shot CLI runs are not saved.
Clients can list them (`session/list`, filtered by cwd), continue them
(`session/load` replays the conversation with all epochs, each compaction
summary as a "Compacted" notice; `session/resume` replays nothing) and
`session/close` them. Sandbox grants are not saved: a continued session
starts from `sandbox.json` again. Edit diffs are not stored either, so
replayed edits show only their output.

Every session opens its own connection; `Options::sessions_db` maps a cwd
to a database file (default: the same file for all). Tables are `STRICT`
with foreign keys and checks, in WAL mode so processes can share the file.
`/compact` keeps the old messages: it starts a new epoch, and a session's
history is the messages of its latest epoch. Each message also stores its
text and tool calls as plain text, ready for full-text search.

`/redact <phrase>` replaces an exact phrase with the secrets marker
(`store::MASK`) in every stored session (all epochs, titles), in the open
session, and through each extension's `redact` command in the `History`
index (dropped, rebuilt on the next search) and `Memory` (matching memories
are deleted). Thinking blocks with the phrase are dropped, since their
signature covers the text. The databases zero deleted bytes and truncate
their WAL. Other open sessions keep the phrase in memory until reopened
(ADR 0035).

Extensions read the database through `ctx.store()` (`session`, `list`,
`history`, `messages` for all epochs); writing is up to rho. `SessionSetup::resumed` tells
`session_start` that a stored session continues.

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
told about the grant with the next prompt, and `/sandbox` to list the
paths the sandbox allows now. Extensions can use `ctx.grant` / `ctx.set_sandbox`, and run their own code
under the session's rules:

```rust
// file access on a throwaway thread restricted by Landlock
let text = ctx.run_sandboxed(move || std::fs::read_to_string(path)).await??;
// a child process, restricted before exec
let mut cmd = tokio::process::Command::new("go");
ctx.sandbox_command(&mut cmd)?;
```

Both are no-ops without a sandbox.

### Features

A feature bundles sandbox access and tools that the user turns on and off
during a session: `/enable ssh git`, `/disable ssh`, and `/features` to
list them with their state and paths (`FeatureCommands`, enabled in the `rho` binary).
The system prompt lists the features (not their state, so the prompt cache
holds), and the model can ask for one with `RequestFeature`; the user
approves like `RequestAccess`. They are defined
in `~/.config/rho/features.json`, with paths like in `sandbox.json`:

```json
[
  {
    "id": "ssh",
    "description": "SSH keys for git",
    "enabled": false,
    "read": ["~/.ssh"],
    "read_write": [],
    "tools": ["mcp__github__create_pull_request"],
    "instructions": "Use SSH remotes."
  }
]
```

`enabled` turns a feature on when a session starts. A tool listed by any
feature can only be called while one of those features is on; the profile
still decides first. Paths are added on top of the session's sandbox (still
read-only in a read-only profile) and removed when the feature is turned off;
commands that are already running keep their access. The model is told with
the next request, like a profile switch. Which features are on is saved with
the session. Extensions add features through `SessionSetup::features` and
switch them with `ctx.set_feature(&id, on)`; register them before
`FeatureCommands`, or their features are missing from the prompt and the
tool.

## Extensions

Implement `rho::extension::Extension` (all hooks optional) in a binary crate
and pass it to `rho::run`, like `agent/src/main.rs`. `rho::Options` also
sets providers, tools, the default model and the compaction model; its
`Default` is anthropic + the built-in tools + no extensions. A tool returns
`ToolOutput` (`.into()` from a `String`); set its `diff` to show a file
change to ACP clients.

Hooks and `Tool::run` are plain `async fn` (no `async-trait`):

```rust
impl Extension for GitGate {
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
        extensions: vec![Arc::new(GitGate), Arc::new(GoCheck::default())],
        ..Default::default()
    })
    .await
}
```

| rho hook | pi event | use |
|---|---|---|
| `session_start(&mut SessionSetup)` | `session_start` + `registerTool` | add tools, add to the system prompt |
| `session_end(&Context)` | `session_shutdown` | cleanup (runs when the session is dropped) |
| `before_prompt(&mut PromptSetup)` | `before_agent_start` | add context, per-run system prompt |
| `before_request(&mut Request)` | `context` / `before_provider_request` | prune or edit the request |
| `before_send(&Context, &mut Value)` | `before_provider_request` | the JSON body right before it is posted; every request, also compaction and `Context::complete` (sync) |
| `tool_call(&mut ToolCall) -> Decision` | `tool_call` | block (reason goes to the model) or edit input |
| `tool_result(&ToolCall, &mut ToolOutput)` | `tool_result` | rewrite output |
| `turn_end(&TurnEnd)` | `turn_end` | observe content, results, usage (once per model request) |
| `run_end(&RunEnd)` | `agent_end` | the run (prompt until the model stops) ended; `before` has the session info at its start |
| `before_settle(&Settle) -> Option<String>` | `agent_before_settle` / `agent_settled` | continue the run with a message |
| `command(name, args) -> Option<String>` | `registerCommand` | handle a `/name` the extension pushed to `SessionSetup::commands` (advertised over ACP); the reply goes to the user only |

`Context` is passed to hooks and tools and can be cloned into background tasks:

- `send(text, trigger_turn)` - message for the model (pi `sendMessage`). During a
  run it is added at the next tool-result boundary. When idle it waits for the
  next prompt, or with `trigger_turn` starts a run right away; in ACP that run
  streams `session/update`s without an open prompt, as pi-acp does.
- `notify(title, text, is_error)` - shown to the user only (ACP: a finished tool call).
- `confirm(tool_call_id, title)` - ACP `session/request_permission`; `false` without a UI (CLI).
- `ask(form)` - a form for the user (ACP elicitation), if the client supports it.
- `complete(&ModelSpec, system, prompt)` - one-off LLM call, e.g. session titles.
- `set_title(title)` - ACP `session_info_update`.
- `session_info()` - model, context window, thinking, profile, usage, cost and
  request count right now.
- `stop(reason)` - end the current run cleanly (pi `ctx.abort()`), from any hook,
  tool or background task. Checked after `before_prompt` (prompt dropped, like
  pi `input` -> `handled`), after `before_request` (request not sent), between
  tool calls (rest skipped; with `Decision::Block` this is pi's `terminate`)
  and before continuing. The run returns `StopReason::Stopped` (ACP: `cancelled`).

`GoCheck` continues the run at most 3 times in a row per prompt; pi's
go-check has no limit (neither pi nor rho limits the turns of a run).
`go test` / `go vet` run sandboxed like `Bash`, so the module cache,
`GOCACHE` and the temp dir must be writable; they are skipped in read-only
profiles.

`ContextNotice` tells the user once when a run takes the context past 25% of
the window.

`AskUser` adds `AskUserQuestion` (pi's ask-user-question): up to four
questions in one form, each a pick list with a text field for an answer of
the user's own.

`SessionTitle` adds `SetSessionTitle`: the system prompt tells the model to
set a title right away and update it when the conversation drifts. After the
first turn, every input reminds the model while there is no title. A
compaction summary ends with the current title. `BackgroundTasks` adds `BackgroundTask`, `BackgroundTaskList` and
`BackgroundTaskCancel`: commands run sandboxed in their own process group, the
result (exit status + last 10 lines) is sent with `trigger_turn`, and running
tasks are killed when the session ends.

Ports of the other pi extensions:

- `GitGate` asks the user before `Bash` or `BackgroundTask` runs `git commit`,
  `git push` or `git tag`, with a summary of the command by the first allowed
  Claude Haiku 5.5 or `gpt-6-luna` from any provider (else the session's
  model); without a UI (CLI) they are blocked.
- `Secrets` masks secrets with the gitleaks rules (`agent/src/gitleaks.toml`,
  copied from gitleaks' `config/gitleaks.toml`) as `REDACTED`: prompts, the
  system prompt and tool output, then every request body again
  (`before_send`, with a notice on a hit). Write/Edit calls that would put
  `REDACTED` into a file are blocked unless the call sets `allowMasked: true`
  (ADR 0034).
- `RipgrepFind` adds `Glob` (file names, newest first) and `Grep` (contents;
  only file names unless `outputMode` is `content` or `count`), like Claude
  Code's. Both run `rg`, sandboxed like `Bash`, 50 lines by default.
- `GoDoc` adds `GoDoc` (pi's `go_doc`) in Go modules: `go doc [-src] [-all]`,
  sandboxed, so the module cache and `GOCACHE` must be accessible.
- `FetchUrl` (pi's `fetch_url`) fetches a URL; HTML becomes markdown via
  `htmd`. The network is not sandboxed.
- `Memory` (pi's `memory`) stores memories in `memory.db` in the config dir,
  embedded locally with fastembed (all-MiniLM-L6-v2, downloaded on first use
  to `~/.cache/rho/fastembed`) and ranked with sqlite-vec (ADR 0030).
  onnxruntime comes from nixpkgs, so build inside `nix develop`.
- `History` searches earlier conversations of a project: the nearest
  directory above the cwd with a `.rho-project-root` file, and every session
  whose cwd is in it (older epochs included). Without the file there is no
  tool. Chunks of ~800 bytes with ~150 bytes overlap are embedded like
  `Memory` and put into FTS5 in `~/.cache/rho/history.db`; search fuses both
  rankings and merges hits on neighbouring chunks. `summarizeOver: n` lets
  a cheap model answer from more than n results instead. `read` shows the
  messages around a result (ADR 0032). The user can search too with
  `/history <query>`; the results are not shown to the model.
- `Mcp` adds the tools of MCP servers over Streamable HTTP from `mcp.json`
  in the config dir (Claude's `mcpServers` format, `type: "http"`, headers
  with `${VAR}` from the environment) as `mcp__<server>__<tool>` (ADR 0031).
- `Skills` loads [Agent Skills](https://agentskills.io) from `skills` in the
  config dir and `~/.agents/skills`. The system prompt lists them, the
  `ReadSkill` tool reads their files without sandbox access, and
  `/skill:<name> [request]` grants read access to the skill dir and sends the
  skill to the model (ADR 0036).

`Ponytail` adds the [ponytail](https://github.com/DietrichGebert/ponytail)
skill ("full" level, vendored in `agent/src/ponytail.md`) to the system prompt.

pi's `exit` extension is not ported: rho has no interactive TUI.

## Profiles

A profile decides what the model may do. rho itself has one, `write`:
every tool, the full sandbox. Extensions add more in `session_start`
(`SessionSetup::profiles`):

Ids are newtypes (`SessionId`, `ToolName`, `ProfileId`, `ModelSpec`, ...); extensions declare
their own with `rho::id_type!`.

```rust
session.profiles.push(Profile {
    id: ProfileId::from_static("plan"),
    name: "Plan".into(),
    description: "Read-only: explore and plan, no changes".into(),
    hidden_tools: vec![rho::tools::WRITE, rho::edit::EDIT],
    read_only: true,                  // sandboxed tools can only read
    instructions: Some("...".into()), // told to the model on switching here
});
```

With more than one profile, ACP clients get a `mode` config option (category
`mode`) to switch. Extensions and tools switch with `ctx.set_profile(&id)`;
the client then gets a `config_option_update`. A switch applies from the
next model request, also in the middle of a run, and does not wait for a
running turn. Every request carries all tools, whatever the profile, so
a switch keeps the prompt cache (tools are its first part). Instead, the
history gets a note, appended to the next user message (prompt or tool
results): the new profile, which tools not to call now and which can be
called again, whether the sandbox is read-only, and the profile's
instructions. A new session gets one on its first request if its profile
restricts anything (e.g. `plan`). Calls to
hidden tools fail. `read_only` turns every read-write sandbox rule into a
read-only one (except `/dev/null` and friends), for `Bash`, `Write`,
background tasks and anything else using `ctx.run_sandboxed` /
`ctx.sandbox_command`; grants made meanwhile are kept for later. Without a
sandbox (`"disabled": true`) only the hidden tools apply. The profile is
saved with the session.

`PlanMode` (in the `rho` binary) adds `plan`, like Claude Code's plan mode:
`Write` and `Edit` are hidden and the sandbox is read-only. The model can
enter it with `EnterPlanMode`. `PresentPlan { plan }` works in every
profile: the plan is shown on the tool call and the user gets a form with
Approve / Revise and a feedback text field. Approved: from `plan` back to
`write`, the model starts implementing (with the feedback as notes).
Feedback without approval: the model gets it and revises the plan. Neither:
the profile stays and the run stops. Picking `write` in the selector needs
no approval (ADR 0047).

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

## Daemon and ACP over WebSocket

`rho daemon` is one rho for all editors. It serves ACP over WebSocket on
`~/.config/rho/daemon/acp.sock` (directory 0700, no token). `rho acp --daemon`
is what the editor runs: an ACP agent on stdio that only forwards lines to
the daemon and back. If no daemon answers, it starts one in the background
(log in `~/.config/rho/daemon/log`), with its own `-m`/`-t` and environment.
Later clients' flags are ignored, and tools run with the daemon's
environment, not the editor's. A lock file keeps it to one daemon. After a
rebuild, stop the old one (`pkill -f 'rho daemon'`); the next editor starts
the new one.

Sessions are shared within the daemon: any connection can `session/load` a
session that another one has open, gets its history and then every update
live, including the other clients' prompts. Permission requests and forms go
to every connection on the session (forms only to clients that support
them); the first answer counts and the others are withdrawn. A question
nobody answered waits, also when everyone left: the next client that loads
the session gets it. A turn keeps running when its connection drops. A session is dropped from memory once no connection has
it open and nothing runs; `session/close` by the last one cancels its turn.

`rho daemon --listen ADDR` also serves ACP on `ws://ADDR/`, for
clients like [acp-ui](https://github.com/formulahendry/acp-ui) (web, phone).
Each text frame carries one JSON-RPC message (several, split by newlines,
are accepted too). Every connection gets its own ACP connection to one
shared agent, so the extensions (memory, MCP, ...) and open sessions are
shared. It replaces the earlier web view (ADR 0020): to follow a Zed
session from a phone, load it in acp-ui.

Every client needs the token, also from this machine, since any website can
open a WebSocket to localhost. It is `RHO_ACP_TOKEN`, else
`~/.config/rho/acp-token`, created with a random token on first start. Send
it as `Authorization: Bearer <token>`, or, from browsers, as a
`bearer.<token>` subprotocol next to `acp.v1` (acp-ui does this for an
`Authorization` header in its config). Only `acp.v1` is echoed back.

## Notes on subscription auth

Based on pi-ai's Anthropic OAuth and `@gotgenes/pi-anthropic-auth`, not
`ai-connect`: ai-connect only does the OAuth handshake, uses the old
`console.anthropic.com` token endpoint and fewer scopes, and does not shape
requests. Subscription requests need the Claude Code headers, the identity
system block and the `x-anthropic-billing-header` block, or Anthropic bills
them as extra usage.
