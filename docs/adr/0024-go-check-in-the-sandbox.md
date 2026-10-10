# 24. GoCheck runs in the sandbox

- Status: Accepted
- Date: 2026-10-10

## Context

`GoCheck` (ADR 0022) runs `go test ./...` and `go vet ./...` when a run
settles. `go test` compiles and runs code the model has just written, so
running it outside the sandbox (ADR 0012) let that code do anything the
user can, although the same code run through `Bash` is restricted.

## Decision

- The checks are restricted with `Context::sandbox_command`, like `Bash`,
  `GoDoc` and `BackgroundTask`. If the sandbox cannot be set up, the check
  fails with that error.
- `GoCheck` does not run in a read-only profile (e.g. `plan`): files
  cannot have changed there, and `go` could not write its build cache, so
  every check would fail.

## Consequences

- `go` needs its caches in the sandbox: the module cache and `GOCACHE`
  read-write (`~/go`, `~/.cache` in `sandbox.json`), and the temp dir for
  test binaries. Without them every check fails and the model is asked to
  fix a permission error, at most 3 times in a row (ADR 0022).
- Tests that need paths outside the workspace need those paths granted,
  the same as when the model runs them with `Bash`.

Code: `agent/src/go_check.rs`.
