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
| [0005](0005-anthropic-wire-format-as-core-model.md) | Provider-neutral types in the Anthropic wire format | Accepted, amended by 0048 |
| [0006](0006-subscription-oauth-and-request-shaping.md) | Claude subscription OAuth with Claude Code request shaping | Accepted |
| [0007](0007-keep-the-prompt-cache-prefix-stable.md) | Keep the request prefix stable for the prompt cache | Accepted |
| [0008](0008-adaptive-thinking-only.md) | Adaptive thinking only | Accepted |
| [0009](0009-extension-hooks-with-native-async-traits.md) | Extension hooks as native `async fn` traits with object-safe twins | Accepted, amended by 0051, 0053 |
| [0010](0010-context-inbox-steering-and-cooperative-stop.md) | `Context`: message inbox, steering and cooperative stop | Accepted |
| [0011](0011-history-stays-valid-under-cancellation.md) | History stays valid when a run is cancelled or fails | Accepted, amended by 0060 |
| [0012](0012-landlock-sandbox.md) | Deny-by-default Landlock sandbox per session | Accepted |
| [0013](0013-user-approval-fails-closed.md) | Escalations need user approval and fail closed without a UI | Accepted, amended by 0047 |
| [0014](0014-sqlite-session-store.md) | Sessions in SQLite | Accepted, amended by 0041, 0060 |
| [0015](0015-compaction-epochs-and-cache-aware-model.md) | Compaction keeps old messages and picks the model by cache state | Accepted, amended by 0040 |
| [0016](0016-profiles-via-history-notes.md) | Profiles restrict tools and sandbox, told to the model in the history | Accepted |
| [0017](0017-plan-mode-as-extension.md) | Plan mode is an extension with user approval | Superseded by 0047 |
| [0018](0018-exact-edit-tool.md) | `Edit` does exact replacements, no fuzzy matching | Accepted, amended by 0041 |
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
| [0031](0031-mcp-over-http.md) | MCP servers over Streamable HTTP, from mcp.json | Accepted, amended by 0052 |
| [0032](0032-project-history-search.md) | Project history search | Accepted, amended by 0049 |
| [0033](0033-features.md) | Features: switchable bundles of sandbox access and tools | Accepted |
| [0034](0034-secret-redaction.md) | Secrets are masked in two stages with the gitleaks rules | Accepted |
| [0035](0035-redact-command.md) | `/redact` masks a phrase in every store | Accepted, amended by 0053, 0060 |
| [0036](0036-skills.md) | Skills as an extension | Accepted, amended by 0052 |
| [0037](0037-acp-over-websocket.md) | ACP over WebSocket with a bearer token | Accepted, amended by 0038, 0039 |
| [0038](0038-daemon.md) | One rho daemon, editors connect through `rho acp --daemon` | Accepted, amended by 0039, 0060 |
| [0039](0039-shared-sessions.md) | Open sessions are shared by the connections of one process | Accepted, amended by 0042, 0052, 0060 |
| [0040](0040-models-by-path.md) | Allowed, default and compaction models by path | Accepted, amended by 0049 |
| [0041](0041-edit-diffs-kept-for-replay.md) | Edit diffs are stored for 7 days and replayed | Accepted, amended by 0060 |
| [0042](0042-busy-state-in-rho-meta.md) | Clients learn that a turn runs from `_meta.rho.running` | Accepted |
| [0043](0043-web-client.md) | A web client in `web/`, for the phone | Accepted |
| [0044](0044-tool-call-descriptions.md) | The model titles its tool calls with a `description` argument | Accepted, amended by 0050 |
| [0045](0045-glob-and-grep.md) | `Glob` and `Grep` split like Claude Code's | Accepted |
| [0046](0046-model-sets-session-title.md) | The model sets the session title with `SetSessionTitle` | Accepted |
| [0047](0047-present-plan-with-feedback.md) | `PresentPlan` replaces `ExitPlanMode` and takes feedback | Accepted, amended by 0054 |
| [0048](0048-github-copilot-provider.md) | GitHub Copilot as a second provider | Accepted |
| [0049](0049-helpers-use-the-compaction-model.md) | Extension helpers use the session's compaction model | Accepted |
| [0050](0050-tools-describe-their-cards.md) | Tools say how their calls are shown | Accepted |
| [0051](0051-per-session-extension-state.md) | Per-session extension state in the `Context` | Accepted |
| [0052](0052-hold-notices-until-the-session-is-known.md) | Hold a starting session's notices until the client knows it | Accepted, amended by 0060 |
| [0053](0053-commands-in-the-agent-and-a-redact-hook.md) | Slash commands run in the agent; extensions redact through a hook | Accepted |
| [0054](0054-plan-approval-as-permission-request.md) | Plan approval is a permission request on the tool call again | Accepted |
| [0055](0055-streamed-bash-output.md) | Bash output streams into Zed's terminal card | Accepted, amended by 0060 |
| [0056](0056-semantic-search-via-zg.md) | Semantic code search via the zg CLI | Accepted |
| [0057](0057-new-sessions-from-the-web.md) | New sessions from the web client, under ~ or /tmp | Accepted |
| [0058](0058-extension-state-saved-with-the-session.md) | Extension state saved with the session | Accepted |
| [0059](0059-notes-in-the-history.md) | System and user notes in the history | Accepted |
| [0060](0060-history-saved-per-block.md) | History saved per block; viewers replay the transcript; runs go on after a restart | Accepted |
| [0061](0061-compact-tool.md) | The model can compact with a Compact tool | Accepted |
