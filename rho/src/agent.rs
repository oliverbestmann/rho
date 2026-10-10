//! The provider-agnostic agent loop: stream a response, run requested tools,
//! feed results back, repeat until the model stops asking for tools. Extension
//! hooks run at each step.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use futures::StreamExt;
use serde_json::Value;

use crate::extension::{
    Context, Decision, Diff, DynExtension, Frontend, PromptSetup, SessionSetup, Settle,
    SlashCommand, ToolCall, ToolOutput, TurnEnd,
};
use crate::provider::{
    Content, Event, Message, Provider, Registry, Request, Role, StopReason, Thinking, ToolSpec,
    Usage,
};
use crate::store::{State, Store};
use crate::tools::DynTool;

/// Picks the database for a session from its cwd, see `Agent::sessions_db`.
pub type SessionsDb = Arc<dyn Fn(&Path) -> PathBuf + Send + Sync>;

/// What the agent is doing, for frontends (CLI printer, ACP session updates).
#[derive(Debug, PartialEq)]
pub enum AgentEvent<'a> {
    Text(&'a str),
    Thinking(&'a str),
    /// Token usage of one model request.
    Usage {
        /// "provider/model" the request went to.
        model: &'a str,
        usage: Usage,
        context_window: u64,
        /// Everything the session has used so far, including this request.
        total: Usage,
        /// USD for the session so far; requests to models without a known
        /// price are not included.
        cost: f64,
    },
    ToolStart {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
        /// The call as markdown, from `Tool::describe`.
        description: Option<&'a str>,
    },
    ToolEnd {
        id: &'a str,
        output: &'a str,
        is_error: bool,
        diff: Option<&'a Diff>,
        /// Images the tool returned, see `ToolOutput::images`.
        images: &'a [crate::provider::ImageSource],
    },
    /// Shown to the user, not sent to the model (from `Context::notify`).
    Notice {
        title: &'a str,
        text: &'a str,
        is_error: bool,
    },
    Title(&'a str),
    /// The session switched model; the context window may differ.
    Model {
        spec: &'a str,
        context_window: u64,
    },
    /// The session switched to this profile (`Context::set_profile`).
    Profile(&'a str),
}

pub struct Agent {
    /// The provider and model new sessions start with; a session can switch
    /// with `Agent::set_model`.
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// `model` as "provider/model", if it came from the registry.
    pub model_spec: Option<String>,
    /// Preamble of the system prompt, see `prompt::build`.
    pub system: String,
    /// Where `PREAMBLE_SYSTEM.md`, `APPEND_SYSTEM.md` and a global `AGENTS.md` are read from.
    pub config_dir: Option<PathBuf>,
    /// Cheaper "provider/model" for compacting when the prompt cache expired.
    pub compact_model: Option<String>,
    pub tools: Vec<Arc<dyn DynTool>>,
    pub extensions: Vec<Arc<dyn DynExtension>>,
    /// Available to extensions through `Context::complete`.
    pub registry: Registry,
    pub max_tokens: u32,
    pub max_turns: usize,
    /// Reasoning effort new sessions start with.
    pub thinking: Thinking,
    /// Starting sandbox for each session, from `sandbox.json`.
    pub sandbox: crate::sandbox::Config,
    /// Where sessions are saved, by cwd. Each session opens its own
    /// connection. `None`: sessions are not saved and cannot be resumed.
    pub sessions_db: Option<SessionsDb>,
}

/// One conversation. Tools and system prompt are fixed at session start.
pub struct Session {
    pub ctx: Context,
    pub provider: Arc<dyn Provider>,
    /// As in `Request::model`.
    pub model: String,
    /// "provider/model" if known, else the same as `model`.
    pub model_spec: String,
    pub thinking: Thinking,
    /// Usage of all requests so far, see `AgentEvent::Usage`.
    pub usage: Usage,
    /// Usage of the latest request, see `Session::usage_report`.
    pub last_usage: Usage,
    pub cost: f64,
    tools: Vec<Arc<dyn DynTool>>,
    /// `tools` specs, built once: every request sends the same bytes, and
    /// schemas are not regenerated per request.
    specs: Vec<ToolSpec>,
    pub system: String,
    /// Slash commands registered by extensions.
    pub commands: Vec<SlashCommand>,
    pub history: Vec<Message>,
    /// Start of the last model request, to guess if the prompt cache is warm.
    pub last_request: Option<Instant>,
    /// The profile the model was last told about; `None` before the first
    /// request.
    profile_seen: Option<String>,
    /// The cwd, then the additional directories.
    roots: Vec<PathBuf>,
    extensions: Vec<Arc<dyn DynExtension>>,
    persist: Option<Persist>,
}

fn notify_dirs_changed(session: &Session) {
    let text = session.dirs_notice();
    session.ctx.notify(
        "Additional directories changed",
        text.as_deref().unwrap_or("(none)"),
        false,
    );
}

/// How much of a session is in its store.
struct Persist {
    store: Store,
    epoch: i64,
    /// `seq` of `history[0]`.
    base: i64,
    /// Messages saved, and the block count of the last one: `push_user` can
    /// merge into it.
    saved: usize,
    last_len: usize,
    /// The session row exists.
    created: bool,
    /// A save failed and the user was told; not repeated.
    failed: bool,
}

impl Session {
    /// Publishes the usage so far (e.g. for a resumed session), if any.
    pub fn emit_usage(&self) {
        if self.usage.prompt() + self.usage.output == 0 {
            return;
        }
        self.ctx.emit(AgentEvent::Usage {
            model: &self.model_spec,
            usage: self.last_usage,
            context_window: self.provider.context_window(&self.model),
            total: self.usage,
            cost: self.cost,
        });
    }

    /// Specs of all tools. Every request carries all of them, whatever the
    /// profile, so a profile switch keeps the prompt cache; the model learns
    /// what it may call from `profile_note`.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    /// The tool called `name`.
    pub fn tool(&self, name: &str) -> Option<&Arc<dyn DynTool>> {
        let i = self.specs.iter().position(|s| s.name == name)?;
        Some(&self.tools[i])
    }

    /// What the model needs to know about the profile, once per change: on
    /// the first request if the profile restricts anything, then after each
    /// switch. The tools it must not call, the ones it may call again, the
    /// sandbox, and the profile's instructions.
    fn profile_note(&mut self) -> Option<String> {
        let profile = self.ctx.profile();
        let seen = self.profile_seen.replace(profile.id.clone());
        if seen.as_ref() == Some(&profile.id) {
            return None;
        }
        let profiles = self.ctx.profiles();
        let before = seen
            .as_ref()
            .and_then(|id| profiles.iter().find(|p| &p.id == id));
        let changed = |gone: bool| -> Vec<&str> {
            self.specs
                .iter()
                .map(|s| &s.name)
                .filter(|n| {
                    let was = before.is_none_or(|b| b.shows(n));
                    if gone {
                        was && !profile.shows(n)
                    } else {
                        !was && profile.shows(n)
                    }
                })
                .map(String::as_str)
                .collect()
        };
        let (removed, added) = (changed(true), changed(false));
        let sandboxed = self.ctx.sandbox().is_some();
        let mut note = if seen.is_none() {
            // Nothing to say about an unrestricted profile.
            if removed.is_empty()
                && !(profile.read_only && sandboxed)
                && profile.instructions.is_none()
            {
                return None;
            }
            format!(
                "<system-reminder>\nThe session is in the {} profile ({}).",
                profile.name, profile.description
            )
        } else {
            format!(
                "<system-reminder>\nThe session switched to the {} profile ({}).",
                profile.name, profile.description
            )
        };
        if !removed.is_empty() {
            note += &format!(
                " Do not call these tools now, calls fail: {}.",
                removed.join(", ")
            );
        }
        if !added.is_empty() {
            note += &format!(" You can call these tools again: {}.", added.join(", "));
        }
        if sandboxed && (profile.read_only || before.is_some_and(|b| b.read_only)) {
            note += if profile.read_only {
                " The sandbox is read-only now: commands cannot change any files."
            } else {
                " The sandbox allows writing to the workspace again."
            };
        }
        if let Some(text) = &profile.instructions {
            note += &format!("\n\n{text}");
        }
        Some(note + "\n</system-reminder>")
    }

    /// The additional directories as a list for a notice, `None` without any.
    pub fn dirs_notice(&self) -> Option<String> {
        let dirs = &self.roots[1..];
        (!dirs.is_empty()).then(|| {
            dirs.iter()
                .map(|d| format!("- {}", d.display()))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    /// Writes new history and the session state (model, usage, ...) to the
    /// store, if the session has one. A failure is shown to the user once
    /// and does not stop the session.
    pub async fn save(&mut self) {
        if let Err(e) = self.try_save().await
            && let Some(p) = &mut self.persist
            && !p.failed
        {
            p.failed = true;
            self.ctx.notify(
                "Session not saved",
                &format!("{}: {e:#}", p.store.path().display()),
                true,
            );
        }
    }

    /// `save`, but the error is returned instead of shown.
    async fn try_save(&mut self) -> Result<()> {
        let Some(p) = &mut self.persist else {
            return Ok(());
        };
        let id = self.ctx.session_id().to_string();
        // Rewrite the last saved message if it grew, and drop rows past the
        // end if the history shrank.
        let from = match p.saved.checked_sub(1) {
            Some(last) if self.history.get(last).map(|m| m.content.len()) != Some(p.last_len) => {
                last
            }
            _ => p.saved,
        }
        .min(self.history.len());
        let state = State {
            id: id.clone(),
            cwd: self.roots[0].clone(),
            additional_dirs: self.roots[1..].to_vec(),
            title: self.ctx.title(),
            model: self.model_spec.clone(),
            thinking: self.thinking,
            usage: self.usage,
            last_usage: self.last_usage,
            cost: self.cost,
            profile: self.ctx.profile().id,
        };
        let messages = self.history[from..].to_vec();
        let result = async {
            p.store.save_state(state).await?;
            p.store
                .save_messages(&id, p.epoch, p.base + from as i64, messages)
                .await
        }
        .await;
        result?;
        p.created = true;
        p.saved = self.history.len();
        p.last_len = self.history.last().map_or(0, |m| m.content.len());
        Ok(())
    }

    /// The history was replaced (compaction): keep the old messages under
    /// their epoch and save the new history under the next one.
    pub(crate) async fn save_compacted(&mut self) {
        if let Some(p) = &mut self.persist
            && p.created
        {
            match p.store.next_epoch(self.ctx.session_id()).await {
                Ok((epoch, base)) => {
                    (p.epoch, p.base, p.saved, p.last_len) = (epoch, base, 0, 0);
                }
                Err(e) => {
                    self.ctx
                        .notify("Session not saved", &format!("{e:#}"), true);
                    return;
                }
            }
        }
        self.save().await;
    }

    /// Markdown summary of token, cache and cost usage, for `/usage`.
    pub fn usage_report(&self) -> String {
        let (last, total) = (&self.last_usage, &self.usage);
        let mut out = format!("**Usage** · `{}`\n\n", self.model_spec);
        if total.prompt() + total.output == 0 {
            out += "No requests yet.";
            return out;
        }
        out += "| | Last request | Session |\n|---|---:|---:|\n";
        for (label, f) in [
            (
                "Input (uncached)",
                (|u: &Usage| u.input) as fn(&Usage) -> u64,
            ),
            ("Cache read", |u| u.cache_read),
            ("Cache write", |u| u.cache_write),
            // Every input token is one of the three above.
            ("**Input total**", |u| u.prompt()),
            ("Output", |u| u.output),
        ] {
            out += &format!("| {label} | {} | {} |\n", count(f(last)), count(f(total)));
        }
        out += &format!(
            "| Cache hit rate | {} | {} |\n\n",
            hit_rate(last),
            hit_rate(total)
        );
        let window = self.provider.context_window(&self.model);
        let used = last.prompt() + last.output;
        out += &format!(
            "- Context: {} / {} ({:.1}%)\n",
            count(used),
            count(window),
            used as f64 * 100.0 / window.max(1) as f64
        );
        if self.cost > 0.0 {
            out += &format!("- Cost: ${:.4}\n", self.cost);
        }
        let ttl = crate::compact::CACHE_TTL;
        out += &match self.last_request.map(|t| t.elapsed()) {
            Some(age) if age < ttl => {
                let left = (ttl - age).as_secs();
                format!(
                    "- Cache: warm, expires in ~{}m {:02}s",
                    left / 60,
                    left % 60
                )
            }
            _ => format!(
                "- Cache: cold (no request in the last {}m)",
                ttl.as_secs() / 60
            ),
        };
        out
    }

    /// Streams one model response to the frontend. Returns its content, why
    /// it stopped and its usage, which is also added to the session's.
    async fn stream(&mut self, request: Request) -> Result<(Vec<Content>, StopReason, Usage)> {
        let ctx = self.ctx.clone();
        self.last_request = Some(Instant::now());
        let mut stream = self.provider.stream(request);
        let mut content: Vec<Content> = Vec::new();
        let mut stop = StopReason::EndTurn;
        let mut usage = Usage::default();
        while let Some(event) = stream.next().await {
            match event? {
                Event::TextDelta(delta) => {
                    ctx.emit(AgentEvent::Text(&delta));
                    match content.last_mut() {
                        Some(Content::Text { text }) => text.push_str(&delta),
                        _ => content.push(Content::text(delta)),
                    }
                }
                Event::ThinkingDelta(delta) => ctx.emit(AgentEvent::Thinking(&delta)),
                Event::Block(block) => content.push(block),
                Event::Usage(u) => {
                    // A request can report usage more than once; each
                    // report replaces the last, so only add the change.
                    self.usage += u - usage;
                    self.cost += self.price(&u) - self.price(&usage);
                    usage = u;
                    self.last_usage = u;
                    ctx.emit(AgentEvent::Usage {
                        model: &self.model_spec,
                        usage,
                        context_window: self.provider.context_window(&self.model),
                        total: self.usage,
                        cost: self.cost,
                    });
                }
                Event::Done(reason) => stop = reason,
            }
        }
        Ok((content, stop, usage))
    }

    /// USD for one request to the session's model, 0 if the price is unknown.
    fn price(&self, usage: &Usage) -> f64 {
        self.provider
            .model_info(&self.model)
            .and_then(|m| m.cost(usage))
            .unwrap_or(0.0)
    }
}

/// 1234567 -> "1,234,567".
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Share of the prompt read from cache.
fn hit_rate(u: &Usage) -> String {
    match u.prompt() {
        0 => "–".into(),
        p => format!("{:.0}%", u.cache_read as f64 * 100.0 / p as f64),
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for ext in &self.extensions {
            ext.session_end(&self.ctx);
        }
    }
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        tools: Vec<Arc<dyn DynTool>>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            model_spec: None,
            system: crate::prompt::PREAMBLE.into(),
            config_dir: None,
            compact_model: None,
            tools,
            extensions: vec![],
            registry: Registry::default(),
            max_tokens: 32_000,
            max_turns: 50,
            thinking: Thinking::default(),
            sandbox: Default::default(),
            sessions_db: None,
        }
    }

    /// The store for sessions in `cwd`, `None` if sessions are not saved.
    async fn open_store(&self, cwd: &Path) -> Result<Option<Store>> {
        let Some(db) = &self.sessions_db else {
            return Ok(None);
        };
        let path = db(cwd);
        Ok(Some(
            tokio::task::spawn_blocking(move || Store::open(path)).await??,
        ))
    }

    /// A new session. It is saved right away if `sessions_db` is set, so a
    /// client can load it even before the first prompt.
    pub async fn start_session(
        &self,
        id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        additional_dirs: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
    ) -> Result<Session> {
        let mut roots = vec![cwd.into()];
        roots.extend(additional_dirs);
        let store = self.open_store(&roots[0]).await?;
        let mut session = self.setup(id.into(), roots, frontend, store, false).await;
        session.try_save().await.context("saving the new session")?;
        Ok(session)
    }

    /// Continues stored session `id`. `cwd` must be the one it started in;
    /// `additional_dirs` replace the stored ones.
    pub async fn resume_session(
        &self,
        id: &str,
        cwd: impl Into<PathBuf>,
        additional_dirs: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
    ) -> Result<Session> {
        let mut roots = vec![cwd.into()];
        roots.extend(additional_dirs);
        let store = self
            .open_store(&roots[0])
            .await?
            .context("sessions are not saved")?;
        let loaded = store
            .load(id)
            .await?
            .with_context(|| format!("unknown session {id}"))?;
        let info = loaded.info;
        if info.cwd != roots[0] {
            bail!("session {id} belongs to {}", info.cwd.display());
        }
        let dirs_changed = info.additional_dirs != roots[1..];
        let mut session = self
            .setup(id.into(), roots, frontend, Some(store), true)
            .await;
        if dirs_changed {
            notify_dirs_changed(&session);
        }
        session.ctx.restore_title(info.title);
        if info.model != session.model_spec
            && let Err(e) = self.set_model(&mut session, &info.model)
        {
            session.ctx.notify(
                "Model unavailable",
                &format!("{e:#}; using {}", session.model_spec),
                true,
            );
        }
        // The model knows the stored profile; if it is gone (its extension
        // was removed), the first request tells it about the fallback.
        if let Err(e) = session.ctx.restore_profile(&info.profile) {
            session
                .ctx
                .notify("Profile unavailable", &format!("{e:#}"), true);
        }
        session.profile_seen = Some(info.profile);
        session.thinking = info.thinking;
        session.usage = info.usage;
        session.last_usage = info.last_usage;
        session.cost = info.cost;
        let p = session.persist.as_mut().expect("store");
        (p.epoch, p.base, p.created) = (info.epoch, loaded.base, true);
        p.saved = loaded.history.len();
        p.last_len = loaded.history.last().map_or(0, |m| m.content.len());
        session.history = loaded.history;
        Ok(session)
    }

    async fn setup(
        &self,
        id: String,
        roots: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
        store: Option<Store>,
        resumed: bool,
    ) -> Session {
        let ctx = Context::new(id, roots[0].clone(), frontend, self.registry.clone());
        ctx.set_sandbox(self.sandbox.policy(&roots));
        if let Some(store) = &store {
            ctx.set_store(store.clone());
        }
        let mut setup = SessionSetup {
            ctx: ctx.clone(),
            tools: self.tools.clone(),
            system: String::new(),
            commands: vec![],
            resumed,
            profiles: vec![crate::extension::Profile::write()],
        };
        for ext in &self.extensions {
            ext.session_start(&mut setup).await;
        }
        ctx.set_profiles(setup.profiles);
        // After the hooks, so the tools extensions added are listed too.
        let system = crate::prompt::build(
            &self.system,
            &setup.tools,
            self.config_dir.as_deref(),
            &roots,
        ) + &setup.system;
        Session {
            ctx,
            provider: self.provider.clone(),
            model: self.model.clone(),
            model_spec: self
                .model_spec
                .clone()
                .unwrap_or_else(|| self.model.clone()),
            thinking: self.thinking,
            usage: Usage::default(),
            last_usage: Usage::default(),
            cost: 0.0,
            specs: setup.tools.iter().map(|t| t.spec()).collect(),
            tools: setup.tools,
            system,
            commands: setup.commands,
            history: vec![],
            last_request: None,
            profile_seen: None,
            roots,
            extensions: self.extensions.clone(),
            persist: store.map(|store| Persist {
                store,
                epoch: 0,
                base: 0,
                saved: 0,
                last_len: 0,
                created: false,
                failed: false,
            }),
        }
    }

    /// Replaces the additional directories of an open session: its sandbox,
    /// system prompt (a cold prompt cache) and store. Sandbox access granted
    /// during the session is kept. Tells the user and returns `true` if they
    /// changed.
    pub fn set_additional_dirs(&self, session: &mut Session, dirs: Vec<PathBuf>) -> bool {
        if session.roots[1..] == dirs[..] {
            return false;
        }
        let mut roots = vec![session.roots[0].clone()];
        roots.extend(dirs);
        if let Some(current) = session.ctx.sandbox() {
            // Rules beyond the old starting policy were granted later.
            let mut start = self
                .sandbox
                .policy(&session.roots)
                .map(|p| p.rules)
                .unwrap_or_default();
            let granted = current.rules.into_iter().filter(|rule| {
                match start.iter().position(|r| r == rule) {
                    Some(i) => {
                        start.remove(i);
                        false
                    }
                    None => true,
                }
            });
            let mut policy = self
                .sandbox
                .policy(&roots)
                .unwrap_or_else(|| crate::sandbox::Policy::base(&roots));
            policy.rules.extend(granted);
            session.ctx.set_sandbox(Some(policy));
        }
        session.system = crate::prompt::replace_dirs(&session.system, &roots[0], &roots[1..]);
        // Only the tools before the system prompt are still cached.
        session.last_request = None;
        session.roots = roots;
        notify_dirs_changed(session);
        true
    }

    /// Switches the session to `spec` ("provider/model"), for the next run.
    /// The new model starts with a cold prompt cache.
    pub fn set_model(&self, session: &mut Session, spec: &str) -> Result<()> {
        let (provider, model) = self.registry.resolve(spec)?;
        session.provider = provider;
        session.model = model;
        session.model_spec = spec.to_string();
        session.last_request = None;
        session.ctx.emit(AgentEvent::Model {
            spec,
            context_window: session.provider.context_window(&session.model),
        });
        Ok(())
    }

    /// Runs an extension's slash command and shows its reply. Returns `false`
    /// if no extension handled it.
    pub async fn command(&self, session: &Session, name: &str, args: &str) -> bool {
        for ext in &self.extensions {
            if let Some(reply) = ext.command(&session.ctx, name, args).await {
                session.ctx.emit(AgentEvent::Text(&reply));
                return true;
            }
        }
        false
    }

    /// Runs a turn for messages sent with `trigger_turn` while idle.
    /// Returns `None` if there is nothing to run.
    pub async fn run_triggered(&self, session: &mut Session) -> Option<Result<StopReason>> {
        if !session.ctx.has_triggered() {
            return None;
        }
        Some(self.run(session, vec![]).await)
    }

    /// Runs one user prompt to completion, appending to the session history.
    ///
    /// History stays valid for the API if this future is dropped (cancelled)
    /// or fails: an assistant turn is only appended together with its tool
    /// results, and a dangling user message gets the next prompt merged in.
    pub async fn run(&self, session: &mut Session, prompt: Vec<Content>) -> Result<StopReason> {
        let ctx = session.ctx.clone();
        let exts = &self.extensions;
        // A stop requested while idle must not end this new run.
        ctx.take_stop();

        // Messages queued while idle go first, then the prompt.
        let mut content = ctx.drain_inbox();
        content.extend(prompt);
        let mut setup = PromptSetup {
            content,
            system: session.system.clone(),
        };
        for ext in exts {
            ext.before_prompt(&ctx, &mut setup).await;
        }
        if let Some(stop) = stopped(&ctx) {
            return Ok(stop);
        }
        push_user(&mut session.history, setup.content);
        session.save().await;

        let mut tools_called = Vec::new();
        for _ in 0..self.max_turns {
            // The history ends with a user message here (prompt or tool results).
            if let Some(note) = session.profile_note() {
                push_user(&mut session.history, vec![Content::text(note)]);
            }
            let mut request = Request {
                model: session.model.clone(),
                system: setup.system.clone(),
                messages: session.history.clone(),
                tools: session.tool_specs(),
                max_tokens: self.max_tokens,
                thinking: session.thinking,
            };
            for ext in exts {
                ext.before_request(&ctx, &mut request).await;
            }
            if let Some(stop) = stopped(&ctx) {
                return Ok(stop);
            }

            let (content, stop, usage) = session.stream(request).await?;

            let mut results = Vec::new();
            for block in &content {
                let Content::ToolUse { id, name, input } = block else {
                    continue;
                };
                let mut call = ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
                // Every tool_use needs a result, even when skipped.
                let out = if ctx.is_stopped() {
                    ToolOutput {
                        output: "skipped: run stopped".into(),
                        is_error: true,
                        ..Default::default()
                    }
                } else {
                    self.call_tool(session, &mut call).await
                };
                tools_called.push(call.name);
                results.push(Content::ToolResult {
                    tool_use_id: call.id,
                    content: out.output,
                    is_error: out.is_error,
                    images: out.images,
                });
            }

            let turn = TurnEnd {
                assistant: &content,
                tool_results: &results,
                usage,
            };
            for ext in exts {
                ext.turn_end(&ctx, &turn).await;
            }

            // The API rejects empty assistant messages.
            if !content.is_empty() {
                session.history.push(Message {
                    role: Role::Assistant,
                    content,
                });
            }
            if !results.is_empty() {
                // Steering: messages sent meanwhile ride along with the results.
                results.extend(ctx.drain_inbox());
                session.history.push(Message {
                    role: Role::User,
                    content: results,
                });
                session.save().await;
                if let Some(stop) = stopped(&ctx) {
                    return Ok(stop);
                }
                continue;
            }
            // The final assistant message (and usage).
            session.save().await;

            let settle = Settle {
                stop,
                history: &session.history,
                tools_called: &tools_called,
            };
            let mut more = Vec::new();
            for ext in exts {
                if let Some(text) = ext.before_settle(&ctx, &settle).await {
                    more.push(Content::text(text));
                }
            }
            if let Some(stop) = stopped(&ctx) {
                return Ok(stop);
            }
            more.extend(ctx.drain_if_triggered());
            if more.is_empty() {
                return Ok(stop);
            }
            push_user(&mut session.history, more);
            session.save().await;
        }
        Ok(StopReason::MaxTurns)
    }

    async fn call_tool(&self, session: &Session, call: &mut ToolCall) -> ToolOutput {
        let ctx = &session.ctx;
        // Announce first, so a confirm prompt can refer to the visible call.
        let tool = session.tool(&call.name);
        let description = tool.and_then(|t| t.describe(&call.input));
        ctx.emit(AgentEvent::ToolStart {
            id: &call.id,
            name: &call.name,
            input: &call.input,
            description: description.as_deref(),
        });
        let profile = ctx.profile();
        let mut decision = if profile.shows(&call.name) {
            Decision::Allow
        } else {
            Decision::Block(format!(
                "{} is not available in the {} profile",
                call.name, profile.name
            ))
        };
        for ext in &self.extensions {
            if decision != Decision::Allow {
                break;
            }
            decision = ext.tool_call(ctx, call).await;
        }
        let result = match decision {
            Decision::Block(reason) => Err(anyhow::anyhow!(reason)),
            Decision::Allow => match tool {
                Some(tool) => tool.run(call.input.clone(), ctx).await,
                None => Err(anyhow::anyhow!("unknown tool {}", call.name)),
            },
        };
        let mut out = match result {
            Ok(output) => output,
            Err(e) => ToolOutput {
                output: format!("{e:#}"),
                is_error: true,
                ..Default::default()
            },
        };
        for ext in &self.extensions {
            ext.tool_result(ctx, call, &mut out).await;
        }
        // An image the API rejects would fail every later request.
        if let Some(e) = out.images.iter().find_map(|i| i.check().err()) {
            out = ToolOutput {
                output: format!("{e:#}"),
                is_error: true,
                ..Default::default()
            };
        }
        ctx.emit(AgentEvent::ToolEnd {
            id: &call.id,
            output: &out.output,
            is_error: out.is_error,
            diff: out.diff.as_ref(),
            images: &out.images,
        });
        out
    }
}

/// Consumes a pending `Context::stop` and tells the user why.
fn stopped(ctx: &Context) -> Option<StopReason> {
    let reason = ctx.take_stop()?;
    ctx.notify("Run stopped", &reason, false);
    Some(StopReason::Stopped)
}

/// Appends user content, merging into a dangling user message so roles keep
/// alternating.
pub(crate) fn push_user(history: &mut Vec<Message>, content: Vec<Content>) {
    match history.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(content),
        _ => history.push(Message {
            role: Role::User,
            content,
        }),
    }
}
