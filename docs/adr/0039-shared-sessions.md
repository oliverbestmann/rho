# 39. Open sessions are shared by the connections of one process

- Status: Accepted, amended by 0042, 0052
- Date: 2026-10-14

## Context

With the daemon (ADR 0038) Zed and acp-ui talk to one process, but every
connection still had its own session map, and turns ran as tasks of their
connection. A session open in Zed could not be followed from a phone, and a
dropped connection killed its running turn.

## Decision

The open sessions live in a `Hub` (`rho/src/acp/hub.rs`). `acp::agent_in(agent,
hub)` builds one connection on it; the daemon passes one hub to the Unix and
the TCP server. `acp::agent` (stdio) makes its own hub.

- A session's `AcpFrontend` belongs to the session, not to a connection. It
  holds the viewers (connections that opened it) and sends every update to
  all of them. Terminal cards go only to clients that announced
  `terminal_output`; the others get the command as a `sh` block.
- `session/load` and `session/resume` of an open session add the
  connection as a viewer. The replay goes only to it. If a turn holds the
  session, the replay comes from the store instead, without waiting.
- A prompt is echoed as `user_message_chunk` to the other viewers.
  Permission requests and forms go to all viewers that know the session
  (forms only to those that support them). The first answer wins; the
  other requests are dropped, which sends `$/cancel_request`. Errors do
  not count, so a client that drops off does not reject for the others.
  With no viewers the question waits; with viewers but none that supports
  forms, `ask` is unsupported.
- A viewer that attaches to an open session is ready after the response
  to its `session/load` or `session/resume`. Then it gets the tool calls
  that are running (the store has them only after they ran) and every
  question that waits. A viewer of a newly opened session is ready at
  once, since `session_start` hooks may ask before the response.
  (Since ADR 0052 it is ready after the response too, and gets the
  updates held until then.)
- Turns run on `tokio::spawn`, not as tasks of the connection, so they
  finish and are saved after the connection drops.
- A connection only prompts, configures or cancels sessions it opened.
- When a connection ends (its state is dropped), it leaves all sessions.
  `session/close` leaves one; the last viewer to close it cancels its runs.
  A session without viewers and runs is dropped, and its inbox watcher
  aborted.

## Consequences

- To follow a Zed session from acp-ui, load it there: both see the turn,
  and a permission request left open in Zed can be answered from the
  phone.
- A client that answers with an error (instead of dropping off) gets no
  second chance until it loads the session again; the question waits for
  another one.
- Loading during a turn: no tool descriptions in the replay, new additional
  directories are not applied, and the chunks already streamed of the
  message in progress are missing.
- Two connections resuming the same closed session at the same moment get
  one session each; the later one wins.
- A daemon restart still ends all turns. Reconnecting the stdio client
  would need it to replay `initialize` and resume its sessions.
