//! `Skills`: [Agent Skills](https://agentskills.io) from `skills` in rho's
//! config dir and `~/.agents/skills`, loaded once at startup (pi's model).
//!
//! The system prompt lists each skill's name and description. The model
//! loads a skill with `ReadSkill`, which reads the skill's files without
//! sandbox access. `/skill:<name> args` grants read access to the skill dir
//! and starts a run with the skill's SKILL.md (unless it is already in the
//! context) and the args. See ADR 0036.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use rho::extension::{
    CommandName, Context, Extension, PromptSetup, SessionSetup, SlashCommand, ToolOutput,
};
use rho::provider::{Content, Request, ToolName, ToolSpec};
use rho::sandbox::Access;
use rho::store::SessionId;
use rho::tools::{REQUEST_ACCESS, Tool, input_schema, parse, truncate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const SKILL_FILE: &str = "SKILL.md";
const COMMAND_PREFIX: &str = "skill:";
/// Starts every SKILL.md the model gets, see `Skill::block`.
const MARKER: &str = "<skill name=\"";

#[derive(Clone, Debug, PartialEq)]
struct Skill {
    name: String,
    description: String,
    /// Canonical.
    dir: PathBuf,
    /// `disable-model-invocation`: not listed in the system prompt, only
    /// usable with `/skill:<name>`.
    hidden: bool,
}

impl Skill {
    /// SKILL.md without its frontmatter, wrapped so `before_request` can
    /// tell it is in the context.
    fn block(&self) -> Result<String> {
        let text = std::fs::read_to_string(self.dir.join(SKILL_FILE))?;
        let body = split_frontmatter(&text).map_or(text.as_str(), |(_, body)| body);
        Ok(format!(
            "{MARKER}{}\" dir=\"{}\">\n{}\n</skill>",
            self.name,
            self.dir.display(),
            body.trim()
        ))
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation", default)]
    disable_model_invocation: bool,
}

#[derive(Default)]
struct SessionState {
    problems_shown: bool,
    /// Skills whose SKILL.md was in the latest request.
    in_context: HashSet<String>,
}

pub struct Skills {
    skills: Arc<Vec<Skill>>,
    problems: Vec<String>,
    sessions: Mutex<HashMap<SessionId, SessionState>>,
}

impl Skills {
    pub fn from_config() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        Self::load(&[
            rho::prompt::config_dir().join("skills"),
            home.join(".agents/skills"),
        ])
    }

    fn load(roots: &[PathBuf]) -> Self {
        let (mut skills, mut problems, mut seen) = (vec![], vec![], HashSet::new());
        for root in roots {
            scan(root, &mut skills, &mut problems, &mut seen);
        }
        Self {
            skills: Arc::new(skills),
            problems,
            sessions: Mutex::default(),
        }
    }

    fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// What `/skill:<name> args` sends to the model.
    fn message(&self, ctx: &Context, skill: &Skill, args: &str) -> Result<String> {
        let in_context = self
            .sessions
            .lock()
            .unwrap()
            .get(ctx.session_id())
            .is_some_and(|s| s.in_context.contains(&skill.name));
        let mut msg = format!(
            "The user invoked the skill {}. Its files are in {}; you may read them and run its scripts with Bash.",
            skill.name,
            skill.dir.display()
        );
        if in_context {
            msg += "\n\nIts SKILL.md is already in your context.";
        } else {
            msg += &format!("\n\n{}", skill.block()?);
        }
        if !args.is_empty() {
            msg += &format!("\n\nUser request: {args}");
        }
        Ok(msg)
    }
}

/// Adds the skills under `dir`: a dir with SKILL.md is a skill, others are
/// searched. `seen` holds canonical dirs, against symlink loops.
fn scan(
    dir: &Path,
    skills: &mut Vec<Skill>,
    problems: &mut Vec<String>,
    seen: &mut HashSet<PathBuf>,
) {
    let Ok(canonical) = dir.canonicalize() else {
        return;
    };
    if !seen.insert(canonical) {
        return;
    }
    let file = dir.join(SKILL_FILE);
    if file.is_file() {
        match load_skill(&file) {
            Ok(s) if skills.iter().any(|o| o.name == s.name) => problems.push(format!(
                "{}: a skill named {} was loaded already",
                file.display(),
                s.name
            )),
            Ok(s) => skills.push(s),
            Err(e) => problems.push(format!("{}: {e:#}", file.display())),
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with('.') && name != "node_modules" && path.is_dir() {
            scan(&path, skills, problems, seen);
        }
    }
}

fn load_skill(file: &Path) -> Result<Skill> {
    let text = std::fs::read_to_string(file)?;
    let (front, _) = split_frontmatter(&text).context("no frontmatter")?;
    let fm: Frontmatter = serde_norway::from_str(front).context("invalid frontmatter")?;
    let description = fm
        .description
        .filter(|d| !d.trim().is_empty())
        .context("no description")?;
    let dir = file.parent().context("no parent dir")?.canonicalize()?;
    let name = match fm.name {
        Some(n) => n,
        None => dir.file_name().unwrap_or_default().to_string_lossy().into(),
    };
    // It becomes part of `/skill:<name>` and the `<skill name="..">` marker.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid name {name:?}, use letters, digits, - and _");
    }
    Ok(Skill {
        name,
        description: description.trim().into(),
        dir,
        hidden: fm.disable_model_invocation,
    })
}

/// `---\n<frontmatter>\n---\n<body>` -> `(frontmatter, body)`.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    let after = &rest[end + 4..];
    let body = after.split_once('\n').map_or("", |(_, body)| body);
    Some((&rest[..end], body))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl Extension for Skills {
    async fn session_start(&self, session: &mut SessionSetup) {
        if self.skills.is_empty() {
            return;
        }
        let listed: Vec<_> = self.skills.iter().filter(|s| !s.hidden).collect();
        if !listed.is_empty() {
            let mut system = "\n\n<skills>\nSkills give instructions for specific tasks. \
                When a task matches a skill's description, load it with ReadSkill before you start.\n\
                <available_skills>\n"
                .to_string();
            for s in listed {
                system += &format!(
                    "<skill><name>{}</name><description>{}</description></skill>\n",
                    s.name,
                    escape(&s.description)
                );
            }
            system += "</available_skills>\n</skills>";
            session.system += &system;
        }
        session.tools.push(Arc::new(ReadSkill {
            skills: self.skills.clone(),
        }));
        for s in self.skills.iter() {
            session.commands.push(SlashCommand {
                name: CommandName::new(format!("{COMMAND_PREFIX}{}", s.name)),
                description: s.description.lines().next().unwrap_or_default().into(),
                hint: Some("optional request".into()),
            });
        }
    }

    fn session_end(&self, ctx: &Context) {
        self.sessions.lock().unwrap().remove(ctx.session_id());
    }

    async fn before_prompt(&self, ctx: &Context, _prompt: &mut PromptSetup) {
        let show = {
            let mut sessions = self.sessions.lock().unwrap();
            let state = sessions.entry(ctx.session_id().clone()).or_default();
            !std::mem::replace(&mut state.problems_shown, true)
        };
        if show {
            for p in &self.problems {
                ctx.notify("Skills", p, true);
            }
        }
    }

    async fn before_request(&self, ctx: &Context, request: &mut Request) {
        if self.skills.is_empty() {
            return;
        }
        // After a compaction the request holds only the new epoch, so this
        // forgets skills that were summarized away.
        let mut found = HashSet::new();
        for content in request.messages.iter().flat_map(|m| &m.content) {
            let text = match content {
                Content::Text { text } => text,
                Content::ToolResult { content, .. } => content,
                _ => continue,
            };
            for (i, _) in text.match_indices(MARKER) {
                let rest = &text[i + MARKER.len()..];
                if let Some((name, _)) = rest.split_once('"') {
                    found.insert(name.to_string());
                }
            }
        }
        let mut sessions = self.sessions.lock().unwrap();
        sessions
            .entry(ctx.session_id().clone())
            .or_default()
            .in_context = found;
    }

    async fn command(&self, ctx: &Context, name: &CommandName, args: &str) -> Option<String> {
        let skill = self.skill(name.as_str().strip_prefix(COMMAND_PREFIX)?)?;
        let msg = match self.message(ctx, skill, args) {
            Ok(msg) => msg,
            Err(e) => return Some(format!("Could not read the skill {}: {e:#}", skill.name)),
        };
        ctx.grant(&skill.dir, Access::Read);
        ctx.send(msg, true);
        Some(format!("Skill {} loaded.", skill.name))
    }
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    /// The skill's name, as in <available_skills>
    name: String,
    /// A file of the skill, relative to its dir. Default: SKILL.md
    path: Option<String>,
}

struct ReadSkill {
    skills: Arc<Vec<Skill>>,
}

impl Tool for ReadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("ReadSkill"),
            description: "Read a skill's SKILL.md, or another file of the skill (path relative \
                to the skill dir). Needs no sandbox access."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == i.name)
            .filter(|s| !s.hidden)
            .with_context(|| format!("unknown skill {:?}", i.name))?;
        let path = i.path.unwrap_or_else(|| SKILL_FILE.into());
        let file = skill
            .dir
            .join(&path)
            .canonicalize()
            .with_context(|| format!("{path} not found"))?;
        if !file.starts_with(&skill.dir) {
            bail!("{path} is not in the skill dir");
        }
        if file != skill.dir.join(SKILL_FILE) {
            let text = std::fs::read(&file).with_context(|| format!("reading {path}"))?;
            let text = String::from_utf8(text).with_context(|| format!("{path} is not text"))?;
            return Ok(truncate(text).into());
        }
        let mut out = format!(
            "{}\n\nRead the skill's other files with ReadSkill (path relative to the skill dir).",
            skill.block()?
        );
        if ctx.tool_available(&REQUEST_ACCESS) {
            out += &format!(
                " To run its scripts with Bash, ask for read access to {} with RequestAccess.",
                skill.dir.display()
            );
        }
        Ok(truncate(out).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::{NoFrontend, Profile};
    use rho::provider::{Message, Role, Thinking};
    use serde_json::json;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn ctx(dir: &Path) -> Context {
        Context::new(
            SessionId::new("s"),
            dir,
            Arc::new(NoFrontend),
            Default::default(),
        )
    }

    /// `root` with gh-stack (nested, block description, a reference), a
    /// hidden one named by its dir, a broken one and a duplicate.
    fn fixture(root: &Path) -> Skills {
        let a = root.join("a");
        write(
            &a.join("group/gh-stack/SKILL.md"),
            "---\nname: gh-stack\ndescription: |\n    Manages stacked PRs.\n    Use for stacks.\nmetadata:\n    author: github\n---\n# gh-stack\n\nRead references/x.md.\n",
        );
        write(&a.join("group/gh-stack/references/x.md"), "reference");
        write(
            &a.join("hidden/SKILL.md"),
            "---\ndescription: Hidden one\ndisable-model-invocation: true\n---\nbody",
        );
        write(&a.join("broken/SKILL.md"), "---\nname: broken\n---\nbody");
        write(
            &root.join("b/gh-stack/SKILL.md"),
            "---\nname: gh-stack\ndescription: other\n---\n",
        );
        Skills::load(&[a, root.join("b"), root.join("missing")])
    }

    #[test]
    fn loads_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills = fixture(dir.path());
        let names: Vec<_> = skills.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gh-stack", "hidden"]);
        let gh = skills.skill("gh-stack").unwrap();
        assert_eq!(gh.description, "Manages stacked PRs.\nUse for stacks.");
        assert!(!gh.hidden && skills.skill("hidden").unwrap().hidden);
        assert_eq!(skills.problems.len(), 2, "{:?}", skills.problems);
        assert!(skills.problems[0].contains("no description"));
        assert!(skills.problems[1].contains("loaded already"));
    }

    #[tokio::test]
    async fn adds_prompt_tool_and_commands() {
        let dir = tempfile::tempdir().unwrap();
        let skills = fixture(dir.path());
        let mut setup = SessionSetup {
            ctx: ctx(dir.path()),
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![Profile::write()],
            features: vec![],
        };
        skills.session_start(&mut setup).await;
        assert!(setup.system.contains("<name>gh-stack</name>"));
        assert!(!setup.system.contains("hidden"));
        assert_eq!(setup.tools[0].spec().name.as_str(), "ReadSkill");
        let commands: Vec<_> = setup.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(commands, ["skill:gh-stack", "skill:hidden"]);
    }

    #[tokio::test]
    async fn reads_skill_files() {
        let dir = tempfile::tempdir().unwrap();
        let skills = fixture(dir.path());
        write(&dir.path().join("secret.md"), "secret");
        std::os::unix::fs::symlink(
            dir.path().join("secret.md"),
            dir.path().join("a/group/gh-stack/link.md"),
        )
        .unwrap();
        let tool = ReadSkill {
            skills: skills.skills.clone(),
        };
        let ctx = ctx(dir.path());
        let run = async |input| tool.run(input, &ctx).await;

        let out = run(json!({"name": "gh-stack"})).await.unwrap().output;
        assert!(out.starts_with("<skill name=\"gh-stack\" dir=\""), "{out}");
        assert!(out.contains("\n# gh-stack\n") && !out.contains("metadata"));
        assert!(out.contains("RequestAccess"));
        let out = run(json!({"name": "gh-stack", "path": "references/x.md"}));
        assert_eq!(out.await.unwrap().output, "reference");
        for path in ["../../hidden/SKILL.md", "link.md"] {
            let err = run(json!({"name": "gh-stack", "path": path})).await;
            assert!(
                err.unwrap_err()
                    .to_string()
                    .contains("not in the skill dir")
            );
        }
        assert!(run(json!({"name": "nope"})).await.is_err());
        assert!(run(json!({"name": "hidden"})).await.is_err());
    }

    #[tokio::test]
    async fn command_grants_and_sends_the_skill_once_per_context() {
        let (dir, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let skills = fixture(dir.path());
        let ctx = ctx(cwd.path());
        let gh = skills.skill("gh-stack").unwrap();
        let file = gh.dir.join(SKILL_FILE);
        let read = async || {
            let file = file.clone();
            ctx.run_sandboxed(move || std::fs::read(file).is_ok())
                .await
                .unwrap()
        };
        assert!(!read().await);

        let name = CommandName::new("skill:gh-stack");
        let reply = skills.command(&ctx, &name, "go").await;
        assert_eq!(reply.unwrap(), "Skill gh-stack loaded.");
        assert!(ctx.has_triggered());
        assert!(read().await);

        let sent = skills.message(&ctx, gh, "split it").unwrap();
        assert!(sent.contains("<skill name=\"gh-stack\""), "{sent}");
        assert!(sent.ends_with("User request: split it"), "{sent}");

        // The model saw it: the next invocation leaves it out.
        let mut request = Request {
            model: rho::provider::ModelId::new("m"),
            system: String::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::text(sent)],
            }],
            tools: vec![],
            max_tokens: 1,
            thinking: Thinking::Off,
        };
        skills.before_request(&ctx, &mut request).await;
        let sent = skills.message(&ctx, gh, "").unwrap();
        assert!(sent.contains("already in your context") && !sent.contains(MARKER));

        let other = CommandName::new("memory");
        assert!(skills.command(&ctx, &other, "").await.is_none());
    }
}
