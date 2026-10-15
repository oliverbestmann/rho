# 12. Deny-by-default Landlock sandbox per session

- Status: Accepted
- Date: 2026-10-09

## Context

The model runs shell commands and reads/writes files. Approving each call
is tedious; running without limits risks damage outside the project.
Landlock (Linux) restricts filesystem access of a process or thread
without root, and the `nono` crate wraps it.

## Decision

- Every session has a `Policy` (list of path rules, read or read-write),
  deny by default:
  - system directories read-only (`/nix/store`, `/etc`, `/usr`, `/proc`, ...;
    missing ones skipped),
  - a few devices read-write (`/dev/null`, `/dev/tty`, ...),
  - the workspace read-write: the cwd plus ACP `additionalDirectories`,
  - extra paths from `~/.config/rho/sandbox.json` (`read`, `read_write`,
    absolute or `~/`; `disabled: true` turns the sandbox off). An invalid
    file stops rho at startup.
- Two ways to apply it, both exposed on `Context`:
  - child processes: `sandbox_command` restricts in `pre_exec`. The rules
    are prepared before the fork; the child only issues allocation-free
    syscalls.
  - in-process file access: `run_sandboxed` runs a closure on a **new
    thread** that is restricted and then ends. Landlock restricts only the
    calling thread, so the rest of rho is unaffected. Never a pooled thread
    (`spawn_blocking`), as it would stay restricted.
- All built-in file tools (`Bash`, `Read`, `ReadImage`, `Write`, `Edit`)
  and the extension tools and checks that touch files (`Glob`, `Grep`,
  `GoDoc`, `BackgroundTask`, `GoCheck`) use it.
- Grants during a session (ADR 0013) extend the policy and are **not**
  stored; a resumed session starts from `sandbox.json` again.
- A read-only profile turns every read-write rule into read-only, except
  the devices (ADR 0016).

## Consequences

- Linux only; elsewhere the sandbox must be disabled explicitly
  (`apply`/`run` fail with a hint). macOS would need nono's Seatbelt backend.
- The network is not sandboxed (`FetchUrl`, `Mcp`, the embedding model
  download, and anything `Bash` runs can reach it).
- Programs need their caches and config granted (`~/go`, `~/.cache`,
  `GOCACHE`, git config).
- Code that extensions run without `run_sandboxed` / `sandbox_command` is
  not restricted: `Memory` writes `memory.db` and the model cache, and
  `History` writes `history.db`, from rho's process (ADR 0030, 0032). (`GoCheck` ran `go test` / `go vet` unsandboxed until
  ADR 0024.)
- Rules for paths that do not exist when a command or thread starts are
  skipped, so a granted path that is created later only applies to the
  next call.

Code: `rho/src/sandbox.rs`, `Context::run_sandboxed` /
`sandbox_command` / `effective_sandbox`.
