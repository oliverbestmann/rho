# 67. Built-in tools are extensions

- Status: Accepted
- Date: 2026-10-28

## Context

Tools reached a session in two ways. The built-in ones were passed to
`Agent::new` (`Options.tools`, default `tools::builtin()`), and all other
tools came from extensions through `SessionSetup::tools`.

## Decision

- `ToolExtension::from(tool)` turns any `Tool` into an extension that adds
  it to every session. The orphan rule forbids a direct
  `From<T: Tool> for Arc<dyn DynExtension>`, so the conversion goes through
  this local type: `Arc::new(ToolExtension::from(MyTool))`.
- Each built-in tool (`Read`, `ReadImage`, `Write`, `Edit`, `Bash`,
  `RequestAccess`, `Compact`) is registered as its own `ToolExtension`.
- `rho::builtin()` (`tools::builtin`) returns these seven extensions in the
  old tool order. `Options::default().extensions` is `rho::builtin()`, and
  the `rho` binary puts it first in its list.
- `Agent::new(provider, model)` takes no tools anymore, and `Agent.tools` and
  `Options.tools` are gone. Tools come only from extensions.
- The tool code lives in `rho/src/tools/`, one file per tool, with the
  `Tool` trait and helpers in `mod.rs`. `Compact` stays in `compact.rs`.
  `rho::edit` is gone; `EDIT` and `Edit` are in `rho::tools`.

## Consequences

- A binary leaves out a built-in tool by not registering its extension.
- The built-in extensions must come before extensions that change other
  tools (`Secrets` wraps `Write` and `Edit`), as plain tools did before.
- The tool order is unchanged, so the tool list and system prompt are the
  same as before and prompt caches stay valid.
- Tests and extensions with their own tools can use `ToolExtension` too.
