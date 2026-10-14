# Architecture Decision Records

Decisions behind rho, derived from the code and its history. Format: Michael
Nygard's (Context, Decision, Consequences), plus pointers into the code.

New decisions get the next number. Rather than rewriting an accepted ADR,
add a new one and mark the old one `Superseded by NNNN`.

| # | Decision | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-library-and-binary-split.md) | A library crate plus a binary that picks the extensions | Accepted |
| [0003](0003-acp-native-no-tui.md) | ACP-native, no TUI; frontends behind a `Frontend` trait | Accepted |
| [0004](0004-pi-as-reference-design.md) | pi is the reference design | Accepted |
| [0005](0005-anthropic-wire-format-as-core-model.md) | Provider-neutral types in the Anthropic wire format | Accepted |
| [0006](0006-subscription-oauth-and-request-shaping.md) | Claude subscription OAuth with Claude Code request shaping | Accepted |
| [0007](0007-keep-the-prompt-cache-prefix-stable.md) | Keep the request prefix stable for the prompt cache | Accepted |
| [0008](0008-adaptive-thinking-only.md) | Adaptive thinking only | Accepted |
| [0009](0009-extension-hooks-with-native-async-traits.md) | Extension hooks as native `async fn` traits with object-safe twins | Accepted |
| [0010](0010-context-inbox-steering-and-cooperative-stop.md) | `Context`: message inbox, steering and cooperative stop | Accepted |
| [0011](0011-history-stays-valid-under-cancellation.md) | History stays valid when a run is cancelled or fails | Accepted |
| [0012](0012-landlock-sandbox.md) | Deny-by-default Landlock sandbox per session | Accepted |
| [0013](0013-user-approval-fails-closed.md) | Escalations need user approval and fail closed without a UI | Accepted |
| [0014](0014-sqlite-session-store.md) | Sessions in SQLite | Accepted |
| [0015](0015-compaction-epochs-and-cache-aware-model.md) | Compaction keeps old messages and picks the model by cache state | Accepted |
| [0016](0016-profiles-via-history-notes.md) | Profiles restrict tools and sandbox, told to the model in the history | Accepted |
| [0017](0017-plan-mode-as-extension.md) | Plan mode is an extension with user approval | Accepted |
| [0018](0018-exact-edit-tool.md) | `Edit` does exact replacements, no fuzzy matching | Accepted |
| [0019](0019-validate-images-before-history.md) | Validate images before they enter the history | Accepted |
| [0020](0020-web-view-as-extension.md) | Web view as a hook-only extension; loopback or password | Superseded by 0037 |
| [0021](0021-background-tasks.md) | Background tasks in process groups, reported with `trigger_turn` | Accepted |
| [0022](0022-bounded-autonomous-loops.md) | Bound the agent's autonomous loops | Accepted |
| [0023](0023-layered-system-prompt.md) | System prompt assembled from layered files, fixed per session | Accepted, amended by 0025, 0026, 0027, 0029 |
| [0024](0024-go-check-in-the-sandbox.md) | GoCheck runs in the sandbox | Accepted |
| [0025](0025-prompt-after-session-start-tool-rules.md) | System prompt built after `session_start`; tools bring their rules | Accepted |
| [0026](0026-prompt-files-created-empty.md) | `SYSTEM.md` and `APPEND_SYSTEM.md` created empty on startup | Accepted, amended by 0027 |
| [0027](0027-preamble-system-md.md) | `PREAMBLE_SYSTEM.md` replaces only the preamble | Accepted, amended by 0028 |
| [0028](0028-preamble-written-to-disk.md) | `PREAMBLE_SYSTEM.md` created with the built-in preamble | Accepted |
| [0029](0029-project-context-needs-consent.md) | Project context files are read only with the user's consent | Accepted |
| [0030](0030-local-memory-store.md) | Memories stored locally with embeddings in SQLite | Accepted |
| [0031](0031-mcp-over-http.md) | MCP servers over Streamable HTTP, from mcp.json | Accepted |
| [0032](0032-project-history-search.md) | Project history search: hybrid search over past sessions under `.rho-project-root` | Accepted |
| [0033](0033-features.md) | Features: switchable bundles of sandbox access and tools | Accepted |
| [0034](0034-secret-redaction.md) | Secrets are masked in two stages with the gitleaks rules | Accepted |
| [0035](0035-redact-command.md) | `/redact` masks a phrase in every store | Accepted |
| [0036](0036-skills.md) | Skills as an extension: `ReadSkill` tool and `/skill:<name>` | Accepted |
| [0037](0037-acp-over-websocket.md) | ACP over WebSocket with a bearer token | Accepted |
| [0038](0038-daemon.md) | One rho daemon, editors connect through `rho acp --daemon` | Accepted |
| [0039](0039-shared-sessions.md) | Open sessions are shared by the connections of one process | Accepted |
