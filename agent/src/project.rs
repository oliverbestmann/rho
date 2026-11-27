//! Named projects (ADR 0078): sessions join one with `/project <name>` and
//! share its text (goal, decisions, work in progress) and a work log, kept
//! in `sessions.db`. The model sees the text as a system reminder when it
//! joins, after a resume, after a compaction or `/clear`, and when another
//! session changed it; it writes both with the `Project` tool.

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use rho::extension::{
    CommandName, Context, Extension, PromptSetup, SessionSetup, SlashCommand, ToolCall, ToolOutput,
};
use rho::provider::{Content, NoteType, ToolName, ToolSpec};
use rho::store::{LogEntry, LogKind, PROJECT_KEY, Project, Store};
use rho::tools::{Tool, input_schema, parse, share_file::PRESENT_DIFF};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::git_gate::{SHELL_TOOLS, runs_git};

const NAME: ToolName = ToolName::from_static("Project");
const JOIN: CommandName = CommandName::from_static("project");
const LOG: CommandName = CommandName::from_static("project:log");

/// Log entries listed in the full reminder.
const LISTED: usize = 50;
/// Over this, the model is asked to condense the text.
const MAX_TEXT: usize = 8 * 1024;

const SYSTEM: &str = "\n\n<projects>\nA session can belong to a named project, which the user joins with `/project <name>`. \
    All sessions of a project share its project text (goal, conventions, decisions, work in progress, open items) and its work log. \
    If this session belongs to a project: keep the project text current with the Project tool, \
    add a log entry when you finish a piece of work, and add one before you call Compact.\n</projects>";

const FINISHED: &str = "If this finishes a piece of work, add a log entry with the Project tool (action `log`) \
    and update the project text (action `edit`).";

const LOG_REQUEST: &str = "<system-reminder>\nThe user asks for a project log entry: add one with the Project tool (action `log`) \
    for the work since the last entry, and update the project text if it is out of date.\n</system-reminder>";

const LINE_SYSTEM: &str = "You write one line, at most 12 words, that says what was worked on in a conversation. \
    Reply with the line only, no quotes or markdown.";

pub struct Projects;

/// The project text version the model last saw in this epoch; `None`
/// until it gets the full reminder.
#[derive(Default)]
struct Seen(Mutex<Option<i64>>);

impl Seen {
    fn get(&self) -> Option<i64> {
        *self.0.lock().unwrap()
    }

    fn set(&self, version: Option<i64>) {
        *self.0.lock().unwrap() = version;
    }
}

fn project_of(ctx: &Context) -> Option<String> {
    ctx.stored(PROJECT_KEY)
}

impl Extension for Projects {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.system += SYSTEM;
        session.tools.push(Arc::new(ProjectTool));
        session.commands.push(SlashCommand {
            name: JOIN,
            description: "Join a project, or list the projects".into(),
            hint: Some("name".into()),
        });
        session.commands.push(SlashCommand {
            name: LOG,
            description: "Ask the model for a project log entry".into(),
            hint: None,
        });
        let ctx = &session.ctx;
        if session.resumed || project_of(ctx).is_some() {
            return;
        }
        let Some(store) = ctx.store() else { return };
        if let Ok(Some(name)) = store.last_project(ctx.cwd()).await {
            ctx.notify(
                NoteType::Hint,
                "",
                &format!("Last project here: {name}. Run `/project {name}` to join it."),
                false,
            );
        }
    }

    async fn command(&self, ctx: &Context, name: &CommandName, args: &str) -> Option<String> {
        let reply = if *name == JOIN {
            join(ctx, args.trim()).await
        } else if *name == LOG {
            request_log(ctx)
        } else {
            return None;
        };
        Some(reply.unwrap_or_else(|e| format!("{e:#}")))
    }

    async fn before_prompt(&self, ctx: &Context, prompt: &mut PromptSetup) {
        let (Some(name), Some(store)) = (project_of(ctx), ctx.store()) else {
            return;
        };
        let Ok(Some(p)) = store.project(&name).await else {
            return;
        };
        let seen = ctx.state::<Seen>();
        let note = match seen.get() {
            None => full(store, &p, false).await,
            Some(v) if v != p.version => changed(&p),
            Some(_) => return,
        };
        seen.set(Some(p.version));
        prompt.content.push(Content::system_note(note));
    }

    /// The model lost the reminder with the history: send it again, mid-run
    /// after the Compact tool, else with the next prompt. A compaction also
    /// gets a log line.
    async fn compacted(&self, ctx: &Context, summary: Option<&str>) {
        let (Some(name), Some(store)) = (project_of(ctx), ctx.store()) else {
            return;
        };
        if let Some(summary) = summary {
            tokio::spawn(log_compaction(
                ctx.clone(),
                store.clone(),
                name.clone(),
                summary.to_string(),
            ));
        }
        let Ok(Some(p)) = store.project(&name).await else {
            return;
        };
        ctx.state::<Seen>().set(Some(p.version));
        ctx.send(full(store, &p, summary.is_some()).await, false);
    }

    async fn tool_result(&self, ctx: &Context, call: &ToolCall, result: &mut ToolOutput) {
        if result.is_error || project_of(ctx).is_none() {
            return;
        }
        let finished = call.name == PRESENT_DIFF
            || call.name == crate::plan_mode::PRESENT
            || (SHELL_TOOLS.contains(&call.name)
                && runs_git(
                    call.input["command"].as_str().unwrap_or_default(),
                    &["commit"],
                ));
        if finished {
            result.output += &format!("\n\n<system-reminder>\n{FINISHED}\n</system-reminder>");
        }
    }

    async fn redact(&self, ctx: &Context, phrase: &str) -> Option<String> {
        let store = ctx.store()?;
        Some(
            match store.redact_projects(phrase, rho::store::MASK).await {
                Ok(n) => format!("Projects: {n} texts and log entries changed"),
                Err(e) => format!("Projects: failed: {e:#}"),
            },
        )
    }
}

fn valid_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// `/project [name]`.
async fn join(ctx: &Context, name: &str) -> Result<String> {
    let store = ctx.store().context("this session is not saved")?;
    let current = project_of(ctx);
    if name.is_empty() {
        let projects = store.projects().await?;
        if projects.is_empty() {
            return Ok("No projects yet. Run `/project <name>` to start one.".into());
        }
        let lines = projects.iter().map(|p| {
            let mark = if current.as_deref() == Some(&p.name) {
                " (this session)"
            } else {
                ""
            };
            format!("- {}{mark}", p.name)
        });
        return Ok(format!(
            "Projects:\n{}",
            lines.collect::<Vec<_>>().join("\n")
        ));
    }
    if !valid_name(name) {
        bail!("A project name has letters, digits, `.`, `_` and `-`, at most 64.");
    }
    let p = store.open_project(name).await?;
    ctx.set_stored(PROJECT_KEY, name);
    // The next prompt gets the full reminder.
    ctx.state::<Seen>().set(None);
    Ok(if p.version == 0 {
        format!("Joined the new project {name}. The model fills in its text as you work.")
    } else {
        format!("Joined project {name}.")
    })
}

/// `/project:log`.
fn request_log(ctx: &Context) -> Result<String> {
    if project_of(ctx).is_none() {
        bail!("This session belongs to no project. Run `/project <name>` to join one.");
    }
    ctx.send(LOG_REQUEST, true);
    Ok("Asked the model for a log entry.".into())
}

fn entry_line(e: &LogEntry) -> String {
    let kind = match e.kind {
        LogKind::Entry => "",
        LogKind::Compaction => " [compaction]",
    };
    format!("#{} {}{kind} {}", e.id, e.date, e.title)
}

fn text_block(p: &Project) -> String {
    let text = if p.text.trim().is_empty() {
        "(empty)"
    } else {
        p.text.trim()
    };
    format!(
        "<project name=\"{}\" version=\"{}\">\n{text}\n</project>",
        p.name, p.version
    )
}

/// The project text and the latest log entries.
async fn full(store: &Store, p: &Project, compacted: bool) -> String {
    let log = store.log(&p.name, LISTED).await.unwrap_or_default();
    let mut note = format!(
        "<system-reminder>\nThis session belongs to project {}. Its project text, shared by all its sessions:\n{}\n",
        p.name,
        text_block(p)
    );
    if log.is_empty() {
        note += "The work log is empty.\n";
    } else {
        note += "Latest work log entries, newest first (read one with the Project tool, action `read_log`):\n";
        for e in &log {
            note += &entry_line(e);
            note += "\n";
        }
    }
    if p.text.len() > MAX_TEXT {
        note += &format!("{}\n", too_long(p.text.len()));
    }
    if compacted {
        note += &format!("The conversation was just compacted. {FINISHED}\n");
    }
    note + "</system-reminder>"
}

fn changed(p: &Project) -> String {
    format!(
        "<system-reminder>\nAnother session changed the project text of {}:\n{}\n</system-reminder>",
        p.name,
        text_block(p)
    )
}

fn too_long(len: usize) -> String {
    format!(
        "The project text is {} KB, over {} KB: condense it.",
        len.div_ceil(1024),
        MAX_TEXT / 1024
    )
}

/// Asks the cheap model for a one-line title of the compacted work and logs it.
async fn log_compaction(ctx: Context, store: Store, name: String, summary: String) {
    let info = ctx.session_info();
    let line = match info.cheap_model() {
        Some(model) => ctx.complete(model, LINE_SYSTEM, &summary).await.ok(),
        None => None,
    };
    let line = line
        .as_deref()
        .and_then(|l| l.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|l| l.trim_matches(['"', '*', '`']).to_string())
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "Compacted".into());
    let _ = store
        .add_log(&name, ctx.session_id(), LogKind::Compaction, &line, "")
        .await;
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Read,
    Edit,
    Log,
    ReadLog,
    SearchLog,
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    action: Action,
    /// Exact text to replace; empty only while the project text is empty (edit)
    old_text: Option<String>,
    /// Replacement text (edit)
    new_text: Option<String>,
    /// Short title of the finished work (log)
    title: Option<String>,
    /// What was done, files, decisions, loose ends (log)
    text: Option<String>,
    /// Entry id (read_log)
    id: Option<i64>,
    /// Words to find in titles and texts (search_log)
    query: Option<String>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

struct ProjectTool;

impl Tool for ProjectTool {
    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME,
            description: "Read and change the project this session belongs to: its shared project text and its work log. \
                Actions: `read` the project text; `edit` it by replacing `old_text` with `new_text` exactly \
                (fails if another session changed it since you last saw it); \
                `log` finished work with `title` and `text` (what was done, files, decisions, loose ends); \
                `read_log` one entry by `id`; `search_log` entries containing `query`. \
                For the conversation behind an entry, use the History tool with its session id and message number."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let input: Input = parse(input)?;
        let name = project_of(ctx).context(
            "This session belongs to no project. The user can join one with `/project <name>`.",
        )?;
        let store = ctx.store().context("this session is not saved")?;
        let need = |v: Option<String>, field: &str| {
            v.filter(|s| !s.trim().is_empty())
                .with_context(|| format!("`{field}` is required"))
        };
        let seen = ctx.state::<Seen>();
        let p = store.open_project(&name).await?;
        let out = match input.action {
            Action::Read => {
                seen.set(Some(p.version));
                text_block(&p)
            }
            Action::Edit => {
                if seen.get() != Some(p.version) {
                    seen.set(Some(p.version));
                    bail!(
                        "The project text changed since you last saw it. Edit this text:\n{}",
                        text_block(&p)
                    );
                }
                let new = replace(
                    &p.text,
                    input.old_text.as_deref().unwrap_or_default(),
                    input.new_text.as_deref().unwrap_or_default(),
                )?;
                let Some(v) = store
                    .set_project_text(&name, p.version, &new, ctx.session_id())
                    .await?
                else {
                    bail!(
                        "Another session changed the project text just now. Read it and try again."
                    );
                };
                seen.set(Some(v));
                let mut out = format!("Updated the project text of {name} (version {v}).");
                if new.len() > MAX_TEXT {
                    out += &format!("\n{}", too_long(new.len()));
                }
                out
            }
            Action::Log => {
                let title = need(input.title, "title")?;
                let text = need(input.text, "text")?;
                let id = store
                    .add_log(&name, ctx.session_id(), LogKind::Entry, title.trim(), &text)
                    .await?;
                format!("Logged #{id} in project {name}.")
            }
            Action::ReadLog => {
                let id = input.id.context("`id` is required")?;
                let e = store
                    .log_entry(&name, id)
                    .await?
                    .with_context(|| format!("no log entry #{id} in project {name}"))?;
                format!(
                    "{}\nSession {}, message {} (History tool, action `read`).\n\n{}",
                    entry_line(&e),
                    e.session_id,
                    e.seq,
                    e.text
                )
            }
            Action::SearchLog => {
                let query = need(input.query, "query")?;
                let found = store.search_log(&name, query.trim(), 20).await?;
                if found.is_empty() {
                    format!("No log entries in {name} contain {query:?}.")
                } else {
                    found.iter().map(entry_line).collect::<Vec<_>>().join("\n")
                }
            }
        };
        Ok(out.into())
    }
}

/// `text` with `old` replaced by `new`; `old` must occur once, or be empty
/// while `text` is.
fn replace(text: &str, old: &str, new: &str) -> Result<String> {
    if old.is_empty() {
        if text.trim().is_empty() {
            return Ok(new.to_string());
        }
        bail!("`old_text` is empty, but the project text is not: quote the part to replace");
    }
    match text.matches(old).count() {
        0 => bail!("`old_text` is not in the project text"),
        1 => Ok(text.replacen(old, new, 1)),
        n => bail!("`old_text` occurs {n} times; quote more of the text around it"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use rho::agent::{Agent, Session};
    use rho::extension::NoFrontend;
    use rho::provider::{Event, ModelSpec, Provider, Request, Role, StopReason};
    use rho::store::SessionId;
    use serde_json::json;

    struct Echo;

    impl Provider for Echo {
        fn stream(&self, _: Request, _: &dyn Fn(&mut Value)) -> BoxStream<'static, Result<Event>> {
            let events = [
                Event::TextDelta("ok".into()),
                Event::Done(StopReason::EndTurn),
            ];
            stream::iter(events.map(Ok)).boxed()
        }
    }

    fn agent(db: std::path::PathBuf) -> Agent {
        let mut agent = Agent::new(Arc::new(Echo), ModelSpec::new("test", "m"));
        agent.sessions_db = Some(Arc::new(move |_| db.clone()));
        agent.extensions.push(Arc::new(Projects));
        agent
    }

    async fn start(agent: &Agent, id: &str, cwd: &std::path::Path) -> Session {
        agent
            .start_session(SessionId::new(id), cwd, vec![], Arc::new(NoFrontend))
            .await
            .unwrap()
    }

    /// The system notes of the last user message with a prompt.
    fn reminders(s: &Session) -> String {
        let last = s.history.iter().rev().find(|m| {
            m.role == Role::User && m.content.iter().any(|c| matches!(c, Content::Text { .. }))
        });
        last.unwrap()
            .content
            .iter()
            .filter_map(|c| match c {
                Content::SystemNote { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    async fn call(s: &Session, input: Value) -> Result<String> {
        Ok(ProjectTool.run(input, &s.ctx).await?.output)
    }

    #[tokio::test]
    async fn sessions_share_the_text_and_see_changes() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path().join("sessions.db"));
        let (mut a, mut b) = (
            start(&agent, "a", dir.path()).await,
            start(&agent, "b", dir.path()).await,
        );
        assert!(call(&a, json!({"action": "read"})).await.is_err());
        assert!(join(&a.ctx, "rho").await.unwrap().contains("new project"));
        agent
            .slash_command(&mut b, "project", "rho")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(project_of(&b.ctx).as_deref(), Some("rho"));

        agent.run(&mut a, vec![Content::text("hi")]).await.unwrap();
        assert!(reminders(&a).contains("(empty)"), "{}", reminders(&a));
        let edit = json!({"action": "edit", "old_text": "", "new_text": "Goal: threads"});
        call(&a, edit).await.unwrap();
        // Its own edit is no news to `a`.
        agent
            .run(&mut a, vec![Content::text("more")])
            .await
            .unwrap();
        assert_eq!(reminders(&a), "");

        // `b` saw nothing yet: it gets the full text, and only changes after.
        agent.run(&mut b, vec![Content::text("hi")]).await.unwrap();
        assert!(reminders(&b).contains("Goal: threads"));
        // A stale edit fails with the current text.
        let stale = json!({"action": "edit", "old_text": "threads", "new_text": "x"});
        call(
            &a,
            json!({"action": "edit", "old_text": "threads", "new_text": "projects"}),
        )
        .await
        .unwrap();
        let err = call(&b, stale.clone()).await.unwrap_err().to_string();
        assert!(err.contains("Goal: projects"), "{err}");
        let err = call(&b, stale).await.unwrap_err().to_string();
        assert!(err.contains("not in the project text"), "{err}");
        agent.run(&mut a, vec![Content::text("x")]).await.unwrap();
        agent.run(&mut b, vec![Content::text("x")]).await.unwrap();
        // `b` already got the text from the failed edit.
        assert_eq!(reminders(&b), "");
    }

    #[tokio::test]
    async fn log_entries_are_listed_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path().join("sessions.db"));
        let mut s = start(&agent, "s", dir.path()).await;
        agent
            .slash_command(&mut s, "project", "rho")
            .await
            .unwrap()
            .unwrap();
        agent.run(&mut s, vec![Content::text("hi")]).await.unwrap();
        let out = call(
            &s,
            json!({"action": "log", "title": "WebSearch tool", "text": "agent/src/web_search.rs"}),
        )
        .await
        .unwrap();
        assert!(out.starts_with("Logged #1"), "{out}");
        let entry = call(&s, json!({"action": "read_log", "id": 1}))
            .await
            .unwrap();
        assert!(entry.contains("WebSearch tool") && entry.contains("Session s, message"));
        let found = call(&s, json!({"action": "search_log", "query": "web_search"}))
            .await
            .unwrap();
        assert!(found.starts_with("#1 "), "{found}");

        // After /clear (or a compaction) the next prompt shows the log again.
        Projects.compacted(&s.ctx, None).await;
        agent
            .run(&mut s, vec![Content::text("next")])
            .await
            .unwrap();
        assert!(reminders(&s).contains("#1 "), "{}", reminders(&s));
        // So does the first prompt after a resume.
        drop(s);
        let mut s = agent
            .resume_session(
                &SessionId::new("s"),
                dir.path(),
                vec![],
                Arc::new(NoFrontend),
            )
            .await
            .unwrap();
        agent
            .run(&mut s, vec![Content::text("back")])
            .await
            .unwrap();
        assert!(
            reminders(&s).contains("WebSearch tool"),
            "{}",
            reminders(&s)
        );
    }

    #[tokio::test]
    async fn knows_the_last_project_and_reminds_after_commits() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path().join("sessions.db"));
        let mut s = start(&agent, "s", dir.path()).await;
        agent
            .slash_command(&mut s, "project", "rho")
            .await
            .unwrap()
            .unwrap();
        agent.run(&mut s, vec![Content::text("hi")]).await.unwrap();
        drop(s);
        let s = start(&agent, "t", dir.path()).await;
        let store = s.ctx.store().unwrap();
        assert_eq!(
            store.last_project(dir.path()).await.unwrap().as_deref(),
            Some("rho")
        );

        let mut out = ToolOutput::from("done");
        let commit = ToolCall {
            id: rho::provider::ToolCallId::new("c"),
            name: rho::tools::BASH,
            input: json!({"command": "git commit -m x"}),
        };
        Projects.tool_result(&s.ctx, &commit, &mut out).await;
        assert_eq!(out.output, "done", "no project, no reminder");
        join(&s.ctx, "rho").await.unwrap();
        Projects.tool_result(&s.ctx, &commit, &mut out).await;
        assert!(out.output.contains(FINISHED));
    }

    #[test]
    fn replaces_once() {
        assert_eq!(replace("", "", "a").unwrap(), "a");
        assert!(replace("a", "", "b").is_err());
        assert_eq!(replace("a b", "b", "c").unwrap(), "a c");
        assert!(replace("b b", "b", "c").is_err());
        assert!(replace("a", "x", "c").is_err());
        assert!(valid_name("rho-2.x") && !valid_name("-x") && !valid_name("a b"));
    }
}
