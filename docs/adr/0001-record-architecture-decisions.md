# 1. Record architecture decisions

- Status: Accepted
- Date: 2026-10-10

## Context

rho grew quickly (18 commits in two days) and many of its choices are only
explained in doc comments, commit messages and the README. Several of them
are non-obvious and easy to undo by accident, for example why every request
carries every tool, or why the sandbox runs file access on a fresh thread.

## Decision

Keep Architecture Decision Records in `docs/adr/`, one Markdown file per
decision, numbered `NNNN-title.md`, in Nygard's format: Status, Context,
Decision, Consequences. ADRs point to the code that implements them.

The first set (0002-0023) was derived from the existing code and its git
history.

## Consequences

- Changing one of these behaviours means writing a new ADR that supersedes
  the old one, not just changing code.
- The README stays the user guide; ADRs hold the reasoning.
