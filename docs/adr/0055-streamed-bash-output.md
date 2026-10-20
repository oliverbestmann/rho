# 55. Bash output streams into Zed's terminal card

- Status: Accepted, amended by 0060
- Date: 2026-10-16

## Context

`Bash` read stdout and stderr to the end and returned them in one piece,
so a terminal card in Zed stayed empty until the command exited. Since the
card's header shows the call's `description` (ADR 0044), the command
itself also needs a place to be seen.

Zed's legacy terminal protocol (`_meta.terminal_output` on tool call
updates) appends each update's data to the card, turns LF into CRLF and
parses ANSI escapes.

## Decision

- `Context::stream_output(chunk, stderr)` shows output of the running tool
  call to the user, as `AgentEvent::ToolStream`. The context knows the
  call because tool calls run one at a time. The tool's result for the
  model does not change.
- `Bash` reads both pipes in chunks and streams them, holding back a UTF-8
  character cut off at a chunk's end. The model still gets stdout, then
  stderr, cut to `MAX_OUTPUT`.
- `AcpFrontend` sends terminal viewers `$ <command>` right after the
  `tool_call`, then each chunk, stderr in red. It keeps what it sent per
  running call, up to 1 MiB, for viewers that attach while the call runs.
  `ToolEnd` then sends only the exit code, plus the last line of an error
  (exit status, timeout). Without streamed output (a blocked call, a
  replay) it sends the whole output as before.
- The shown command (the `$` line and the `sh` block) is formatted by a
  best-effort pass in `rho/src/acp/shell.rs`: one command per line, a
  break before `&&`, `||` and `|`, indented `if`, loops and `{ }`. No Rust
  crate formats shell, and `shfmt` would be a Go binary to run per call.
  Heredocs, `case` and unbalanced quotes stay as written.
- Clients without terminals, like the web client, get nothing streamed;
  their tool call content is replaced on each update, so streaming would
  resend everything each time.
- Note (2026-10-16): the web client now says it supports
  `_meta.terminal_output` too and renders the output with xterm.js and its
  fit addon. Both load only when a terminal card is opened. The `rho` CLI
  prints the chunks to stderr.

## Consequences

- Chunks arrive in the order the pipes deliver them, so the card can
  interleave stdout and stderr differently from the model's copy.
- Other tools can stream with `stream_output`; only terminal cards show it.
- The web client shows Bash output only at the end, until it gets its own
  streaming. (No longer true, see the note above.)
