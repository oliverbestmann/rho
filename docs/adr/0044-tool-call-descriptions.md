# 44. The model titles its tool calls with a `description` argument

- Status: Accepted, amended by 0050
- Date: 2026-10-15

## Context

Tool cards were titled from an input key (`Bash <first line>`, `Read
<path>`). On a phone, a title that says what a call is for reads better than
a long command or path. Claude Code asks for a `description` on `Bash`.

## Decision

Built-in and extension tools whose calls show as cards take a
`description`: "what this call is for, at most 10 words; shown to the user
as its title". That is `Read`, `Write`, `Edit`, `Bash`, `BackgroundTask`,
`Glob`, `Grep`, `FetchUrl`, `ReadImage`, `AskUserQuestion`, `Memory` and
`History`.

- In the schema it is required (`#[schemars(required, with = "String")]`).
  The input struct keeps it `Option`, so calls without it (older sessions,
  models that skip it) still run.
- A call that runs without a required argument gets a
  `<system-reminder>` appended to its result (`missing_required` in
  `rho/src/agent.rs`). It doesn't fail.
- `title()` in `rho/src/acp/update.rs` uses the first line of the description, at
  most 160 characters, for tools in `DESCRIBED`. Other tools keep the
  input-key title. In MCP tools a `description` can be content, not a
  title. (Since 2026-10-15 MCP tools without their own `description`
  input get rho's, required; `Mcp` strips it before the call goes to the
  server and the tool is `titled`.)
- Zed's terminal card shows only the title in its header and only the
  output when opened, so for a `Bash` terminal card the output starts
  with `$ <command>` (see ADR 0055). Clients without terminals get the
  command as a `sh` block.

## Consequences

- Each call costs a few output tokens.
- New tools that should get a description title need the field and an
  entry in `DESCRIBED`. (Since ADR 0050 they say so with `Tool::titled`.)
- The reminder works for any required argument, not only `description`.
