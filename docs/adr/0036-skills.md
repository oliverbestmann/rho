# 36. Skills as an extension

- Status: Accepted
- Date: 2026-10-15

## Context

[Agent Skills](https://agentskills.io) are directories with a `SKILL.md`
(YAML frontmatter with `name` and `description`, then instructions) and
optional files. The model should know which skills exist, load one when a
task matches, and not need sandbox access to read it. The user should be
able to invoke a skill by hand.

## Decision

An extension, `Skills` (`agent/src/skills.rs`), without a core change. It
follows pi's model.

- Skills are loaded once at startup from `skills` in rho's config dir and
  from `~/.agents/skills`. A dir with a `SKILL.md` is a skill, other dirs
  are searched (not dot dirs or `node_modules`). The name is the frontmatter
  `name`, else the dir name; it may use letters, digits, `-` and `_`. The
  first skill of a name wins.
- Broken skills (no frontmatter, no description, bad name, duplicate) are
  skipped and reported once per session as a notification.
- The system prompt lists name and description of every skill, except those
  with `disable-model-invocation: true`.
- The `ReadSkill` tool returns SKILL.md without its frontmatter, or another
  file of the skill. The path is canonicalized and must stay inside the
  skill dir, so `..` and symlinks out of it fail. It reads the files itself,
  so the sandbox is not involved. Hidden skills can not be read with it.
  Running a skill's scripts with Bash still needs sandbox access; when
  `RequestAccess` is available, the tool output says to ask for the skill
  dir with it.
- `/skill:<name> args` grants read access to the skill dir for the session
  and sends a message to the model (`ctx.send`) with SKILL.md and the args.
  The reply to the user is only "Skill <name> loaded."
- SKILL.md is wrapped in `<skill name="..">`. Before each request the
  extension looks for that marker in the messages, in text and tool results,
  so a skill read with `ReadSkill` counts too. If the skill is in the
  context already, the command leaves SKILL.md out. After a compaction the
  marker is gone, so the next invocation sends it again.

## Consequences

- A new or changed skill needs a restart.
- No project-level skills: a project could ship instructions the user never
  looked at.
- The marker check is text based. Text that contains `<skill name="x"` makes
  skill `x` count as loaded.
- `serde_norway` is a new dependency for the frontmatter.
