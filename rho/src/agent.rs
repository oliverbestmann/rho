//! The provider-agnostic agent loop: stream a response, run requested tools,
//! feed results back, repeat until the model stops asking for tools. Extension
//! hooks run at each step.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use futures::StreamExt;
use serde_json::Value;

use crate::extension::{
    CommandName, Context, Decision, Diff, DynExtension, Frontend, Profile, ProfileId, PromptSetup,
    RunEnd, SessionInfo, SessionSetup, Settle, SlashCommand, ToolCall, ToolOutput, TurnEnd,
};
use crate::feature::{Feature, FeatureId};
use crate::provider::{
    Content, Event, Message, ModelSpec, Provider, Registry, Request, Role, StopReason, Thinking,
    ToolCallId, ToolName, ToolSpec, Usage,
};
use crate::store::{SessionId, State, Store};
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
        /// The model the request went to.
        model: &'a ModelSpec,
        usage: Usage,
        context_window: u64,
        /// Everything the session has used so far, including this request.
        total: Usage,
        /// USD for the session so far; requests to models without a known
        /// price are not included.
        cost: f64,
    },
    ToolStart {
        id: &'a ToolCallId,
        name: &'a ToolName,
        input: &'a Value,
        /// The call as markdown, from `Tool::describe`.
        description: Option<&'a str>,
    },
    ToolEnd {
        id: &'a ToolCallId,
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
        spec: &'a ModelSpec,
        context_window: u64,
    },
    /// The session switched to this profile (`Context::set_profile`).
    Profile(&'a ProfileId),
}

pub struct Agent {
    /// The provider and model new sessions start with; a session can switch
    /// with `Agent::set_model`.
    pub provider: Arc<dyn Provider>,
    pub model: ModelSpec,
    /// Preamble of the system prompt, see `prompt::build`.
    pub system: String,
    /// Where `PREAMBLE_SYSTEM.md`, `APPEND_SYSTEM.md` and a global `AGENTS.md` are read from.
    pub config_dir: Option<PathBuf>,
    /// Cheaper model for compacting when the prompt cache expired.
    pub compact_model: Option<ModelSpec>,
    pub tools: Vec<Arc<dyn DynTool>>,
    pub extensions: Vec<Arc<dyn DynExtension>>,
    /// Available to extensions through `Context::complete`.
    pub registry: Registry,
    pub max_tokens: u32,
    /// Reasoning effort new sessions start with.
    pub thinking: Thinking,
    /// Starting sandbox for each session, from `sandbox.json`.
    pub sandbox: crate::sandbox::Config,
    /// Features each session starts with, from `features.json`.
    pub features: Vec<Feature>,
    /// Where sessions are saved, by cwd. Each session opens its own
    /// connection. `None`: sessions are not saved and cannot be resumed.
    pub sessions_db: Option<SessionsDb>,
}

/// One conversation. Tools and system prompt are fixed at session start.
pub struct Session {
    pub ctx: Context,
    pub provider: Arc<dyn Provider>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    /// Usage of all requests so far, see `AgentEvent::Usage`.
    pub usage: Usage,
    /// Usage of the latest request, see `Session::usage_report`.
    pub last_usage: Usage,
    pub cost: f64,
    /// Model requests so far.
    pub requests: u32,
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
    /// The profile and features that were on when the model was last told;
    /// `None` before the first request.
    seen: Option<(ProfileId, Vec<FeatureId>)>,
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
    /// Number of compactions of the stored session; 0 if not stored.
    pub(crate) fn epoch(&self) -> i64 {
        self.persist.as_ref().map_or(0, |p| p.epoch)
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            model: Some(self.model.clone()),
            context_window: self.provider.context_window(self.model.id()),
            thinking: self.thinking,
            profile: self.ctx.profile().id,
            last_usage: self.last_usage,
            usage: self.usage,
            cost: self.cost,
            requests: self.requests,
        }
    }

    /// Updates `Context::session_info`; call it after changing the model,
    /// thinking or usage fields directly.
    pub fn publish_info(&self) {
        self.ctx.set_session_info(self.info());
    }

    /// Publishes the usage so far (e.g. for a resumed session), if any.
    pub fn emit_usage(&self) {
        if self.usage.prompt() + self.usage.output == 0 {
            return;
        }
        self.ctx.emit(AgentEvent::Usage {
            model: &self.model,
            usage: self.last_usage,
            context_window: self.provider.context_window(self.model.id()),
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
    pub fn tool(&self, name: &ToolName) -> Option<&Arc<dyn DynTool>> {
        let i = self.specs.iter().position(|s| s.name == *name)?;
        Some(&self.tools[i])
    }

    /// What the model needs to know about the profile and features, once per
    /// change: on the first request if anything is restricted or on, then
    /// after each switch. The tools it must not call, the ones it may call
    /// now, the sandbox, and the instructions.
    fn profile_note(&mut self) -> Option<String> {
        let profile = self.ctx.profile();
        let features = self.ctx.features();
        let now = (profile.id.clone(), self.ctx.active_features());
        let seen = self.seen.replace(now.clone());
        if seen.as_ref() == Some(&now) {
            return None;
        }
        let active = &now.1;
        let profiles = self.ctx.profiles();
        let before = seen
            .as_ref()
            .and_then(|(id, _)| profiles.iter().find(|p| &p.id == id));
        let was_active = seen.as_ref().map(|(_, a)| a.as_slice());
        // Before the first request, every tool counts as callable.
        let callable = |p: Option<&Profile>, on: Option<&[FeatureId]>, n: &ToolName| {
            p.is_none_or(|p| p.shows(n))
                && on.is_none_or(|on| crate::feature::unlocks(&features, on, n))
        };
        let changed = |gone: bool| -> Vec<&str> {
            self.specs
                .iter()
                .map(|s| &s.name)
                .filter(|n| {
                    let was = callable(before, was_active, n);
                    let is = callable(Some(&profile), Some(active), n);
                    if gone { was && !is } else { !was && is }
                })
                .map(ToolName::as_str)
                .collect()
        };
        let (removed, added) = (changed(true), changed(false));
        let old = was_active.unwrap_or_default();
        let turned_on: Vec<_> = features
            .iter()
            .filter(|f| active.contains(&f.id) && !old.contains(&f.id))
            .collect();
        let turned_off = features
            .iter()
            .filter(|f| !active.contains(&f.id) && old.contains(&f.id));
        let switched = seen.as_ref().is_none_or(|(id, _)| *id != profile.id);
        let sandboxed = self.ctx.sandbox().is_some();
        let mut parts = vec![];
        if seen.is_none() {
            // Nothing to say about an unrestricted profile without features.
            if removed.is_empty()
                && !(profile.read_only && sandboxed)
                && profile.instructions.is_none()
                && turned_on.is_empty()
            {
                return None;
            }
            parts.push(format!(
                "The session is in the {} profile ({}).",
                profile.name, profile.description
            ));
        } else if switched {
            parts.push(format!(
                "The session switched to the {} profile ({}).",
                profile.name, profile.description
            ));
        }
        parts.extend(turned_on.iter().map(|f| f.turned_on(sandboxed)));
        parts.extend(turned_off.map(|f| format!("The {} feature is off now.", f.id)));
        if !removed.is_empty() {
            parts.push(format!(
                "Do not call these tools now, calls fail: {}.",
                removed.join(", ")
            ));
        }
        if !added.is_empty() {
            parts.push(format!(
                "You can call these tools now: {}.",
                added.join(", ")
            ));
        }
        if switched && sandboxed && (profile.read_only || before.is_some_and(|b| b.read_only)) {
            parts.push(if profile.read_only {
                "The sandbox is read-only now: commands cannot change any files.".into()
            } else {
                "The sandbox allows writing to the workspace again.".into()
            });
        }
        let mut note = format!("<system-reminder>\n{}", parts.join(" "));
        let instructions = turned_on.iter().map(|f| &f.instructions);
        let profile_text = switched.then_some(&profile.instructions);
        for text in profile_text.into_iter().chain(instructions).flatten() {
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
        let id = self.ctx.session_id().clone();
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
            model: self.model.clone(),
            thinking: self.thinking,
            usage: self.usage,
            last_usage: self.last_usage,
            cost: self.cost,
            profile: self.ctx.profile().id,
            features: self.ctx.active_features(),
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
        let mut out = format!("**Usage** · `{}`\n\n", self.model);
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
        let info = self.info();
        let (used, window) = (info.context_used(), info.context_window.max(1));
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
        self.requests += 1;
        let mut stream = ctx.stream(&*self.provider, request);
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
                    self.publish_info();
                    ctx.emit(AgentEvent::Usage {
                        model: &self.model,
                        usage,
                        context_window: self.provider.context_window(self.model.id()),
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
            .model_info(self.model.id())
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
/// One line: "2 requests · 3.4s · input 1,030 (98% cached, 0 written) · output 12 · $0.0123".
fn turn_report(requests: u32, took: Duration, u: &Usage, cost: f64) -> String {
    let mut out = format!(
        "{requests} request{} · {:.1}s · input {} ({} cached, {} written) · output {}",
        if requests == 1 { "" } else { "s" },
        took.as_secs_f64(),
        count(u.prompt()),
        hit_rate(u),
        count(u.cache_write),
        count(u.output),
    );
    if cost > 0.0 {
        out += &format!(" · ${cost:.4}");
    }
    out
}

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
        model: ModelSpec,
        tools: Vec<Arc<dyn DynTool>>,
    ) -> Self {
        Self {
            provider,
            model,
            system: crate::prompt::PREAMBLE.into(),
            config_dir: None,
            compact_model: None,
            tools,
            extensions: vec![],
            registry: Registry::default(),
            max_tokens: 32_000,
            thinking: Thinking::default(),
            sandbox: Default::default(),
            features: vec![],
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
        id: SessionId,
        cwd: impl Into<PathBuf>,
        additional_dirs: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
    ) -> Result<Session> {
        let mut roots = vec![cwd.into()];
        roots.extend(additional_dirs);
        let store = self.open_store(&roots[0]).await?;
        let mut session = self.setup(id, roots, frontend, store, false).await;
        session.try_save().await.context("saving the new session")?;
        Ok(session)
    }

    /// Continues stored session `id`. `cwd` must be the one it started in;
    /// `additional_dirs` replace the stored ones.
    pub async fn resume_session(
        &self,
        id: &SessionId,
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
            .setup(id.clone(), roots, frontend, Some(store), true)
            .await;
        if dirs_changed {
            notify_dirs_changed(&session);
        }
        session.ctx.restore_title(info.title);
        if info.model != session.model
            && let Err(e) = self.set_model(&mut session, &info.model)
        {
            session.ctx.notify(
                "Model unavailable",
                &format!("{e:#}; using {}", session.model),
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
        // Stored features win over the defaults; the model knows them.
        for f in session.ctx.active_features() {
            let _ = session.ctx.set_feature(&f, false);
        }
        for f in &info.features {
            if let Err(e) = session.ctx.set_feature(f, true) {
                session
                    .ctx
                    .notify("Feature unavailable", &format!("{e:#}"), true);
            }
        }
        session.seen = Some((info.profile, info.features));
        session.thinking = info.thinking;
        session.usage = info.usage;
        session.last_usage = info.last_usage;
        session.cost = info.cost;
        session.publish_info();
        let p = session.persist.as_mut().expect("store");
        (p.epoch, p.base, p.created) = (info.epoch, loaded.base, true);
        p.saved = loaded.history.len();
        p.last_len = loaded.history.last().map_or(0, |m| m.content.len());
        session.history = loaded.history;
        Ok(session)
    }

    async fn setup(
        &self,
        id: SessionId,
        roots: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
        store: Option<Store>,
        resumed: bool,
    ) -> Session {
        let ctx = Context::new(id, roots[0].clone(), frontend, self.registry.clone());
        ctx.set_sandbox(self.sandbox.policy(&roots));
        ctx.set_extensions(self.extensions.clone());
        if let Some(store) = &store {
            ctx.set_store(store.clone());
        }
        let mut setup = SessionSetup {
            ctx: ctx.clone(),
            tools: self.tools.clone(),
            system: String::new(),
            commands: vec![],
            resumed,
            profiles: vec![Profile::write()],
            features: self.features.clone(),
        };
        for ext in &self.extensions {
            ext.session_start(&mut setup).await;
        }
        ctx.set_profiles(setup.profiles);
        ctx.set_features(setup.features);
        // After the hooks, so the tools extensions added are listed too.
        let system = crate::prompt::build(
            &self.system,
            &setup.tools,
            self.config_dir.as_deref(),
            &roots,
        ) + &setup.system;
        let session = Session {
            ctx,
            provider: self.provider.clone(),
            model: self.model.clone(),
            thinking: self.thinking,
            usage: Usage::default(),
            last_usage: Usage::default(),
            cost: 0.0,
            requests: 0,
            specs: setup.tools.iter().map(|t| t.spec()).collect(),
            tools: setup.tools,
            system,
            commands: setup.commands,
            history: vec![],
            last_request: None,
            seen: None,
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
        };
        session.publish_info();
        session
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

    /// Switches the session to `spec`, for the next run. The new model
    /// starts with a cold prompt cache.
    pub fn set_model(&self, session: &mut Session, spec: &ModelSpec) -> Result<()> {
        session.provider = self.registry.resolve(spec)?;
        session.model = spec.clone();
        session.last_request = None;
        session.publish_info();
        session.ctx.emit(AgentEvent::Model {
            spec,
            context_window: session.provider.context_window(spec.id()),
        });
        Ok(())
    }

    /// Runs an extension's slash command and shows its reply. Returns `false`
    /// if no extension handled it.
    pub async fn command(&self, session: &Session, name: &CommandName, args: &str) -> bool {
        for ext in &self.extensions {
            if let Some(reply) = ext.command(&session.ctx, name, args).await {
                session.ctx.emit(AgentEvent::Text(&reply));
                return true;
            }
        }
        false
    }

    /// `/redact <phrase>`: replaces `phrase` with `store::MASK` in the
    /// session's history, in every stored session and, through each
    /// extension's `redact` command, in their stores. Shows a summary.
    pub async fn redact(&self, session: &mut Session, phrase: &str) -> Result<()> {
        if phrase.is_empty() {
            bail!("usage: /redact <phrase>");
        }
        for m in &mut session.history {
            if crate::store::redact_content(&mut m.content, phrase) {
                // The prompt cache no longer matches.
                session.last_request = None;
            }
        }
        if let Some(title) = session.ctx.title().filter(|t| t.contains(phrase)) {
            session
                .ctx
                .restore_title(Some(title.replace(phrase, crate::store::MASK)));
        }
        let mut lines = vec![];
        if let Some(p) = &session.persist {
            let (messages, sessions) = p.store.redact(phrase).await?;
            lines.push(format!(
                "- Sessions: {messages} messages in {sessions} sessions"
            ));
        }
        let name = CommandName::from_static("redact");
        for ext in &self.extensions {
            if let Some(reply) = ext.command(&session.ctx, &name, phrase).await {
                lines.push(format!("- {reply}"));
            }
        }
        session.save().await;
        session.ctx.emit(AgentEvent::Text(&format!(
            "**Redacted**\n\n{}",
            lines.join("\n")
        )));
        Ok(())
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
    ///
    /// Afterwards shows what the run cost as a notice.
    pub async fn run(&self, session: &mut Session, prompt: Vec<Content>) -> Result<StopReason> {
        // Also picks up fields changed directly, like the thinking level.
        session.publish_info();
        let (start, usage, cost, requests, before) = (
            Instant::now(),
            session.usage,
            session.cost,
            session.requests,
            session.ctx.session_info(),
        );
        let result = self.run_prompt(session, prompt).await;
        if session.requests > requests {
            let report = turn_report(
                session.requests - requests,
                start.elapsed(),
                &(session.usage - usage),
                session.cost - cost,
            );
            // In the title: notices show as collapsed tool calls.
            session.ctx.notify(&format!("Turn: {report}"), "", false);
        }
        let run = RunEnd {
            stop: result.as_ref().ok().copied(),
            before,
        };
        for ext in &self.extensions {
            ext.run_end(&session.ctx, &run).await;
        }
        result
    }

    async fn run_prompt(&self, session: &mut Session, prompt: Vec<Content>) -> Result<StopReason> {
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
        // No turn limit, like Claude Code and pi; the user cancels runaway runs.
        loop {
            // The history ends with a user message here (prompt or tool results).
            if let Some(note) = session.profile_note() {
                push_user(&mut session.history, vec![Content::text(note)]);
            }
            let mut request = Request {
                model: session.model.id().clone(),
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
        let mut pending = Pending {
            ctx,
            id: Some(call.id.clone()),
        };
        let out = self.decide_and_run(session, call, tool).await;
        pending.id = None;
        ctx.emit(AgentEvent::ToolEnd {
            id: &call.id,
            output: &out.output,
            is_error: out.is_error,
            diff: out.diff.as_ref(),
            images: &out.images,
        });
        out
    }

    async fn decide_and_run(
        &self,
        session: &Session,
        call: &mut ToolCall,
        tool: Option<&Arc<dyn DynTool>>,
    ) -> ToolOutput {
        let ctx = &session.ctx;
        let profile = ctx.profile();
        let mut decision = if !profile.shows(&call.name) {
            Decision::Block(format!(
                "{} is not available in the {} profile",
                call.name, profile.name
            ))
        } else if !ctx.tool_available(&call.name) {
            let ids: Vec<_> = ctx
                .features()
                .into_iter()
                .filter(|f| f.tools.contains(&call.name))
                .map(|f| f.id.to_string())
                .collect();
            Decision::Block(format!(
                "{} needs one of these features, which are off: {}. The user can turn one on with /enable",
                call.name,
                ids.join(", ")
            ))
        } else {
            Decision::Allow
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
        out
    }
}

/// Ends a started tool call in the frontend if the run is cancelled (its
/// future dropped) before the call finished; `id` is cleared once it did.
struct Pending<'a> {
    ctx: &'a Context,
    id: Option<ToolCallId>,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let Some(id) = &self.id else {
            return;
        };
        self.ctx.emit(AgentEvent::ToolEnd {
            id,
            output: "cancelled",
            is_error: true,
            diff: None,
            images: &[],
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_report_sums_up_the_run() {
        let u = Usage {
            input: 20,
            cache_read: 1010,
            cache_write: 1000,
            output: 12,
        };
        assert_eq!(
            turn_report(2, Duration::from_millis(3400), &u, 0.01234),
            "2 requests · 3.4s · input 2,030 (50% cached, 1,000 written) · output 12 · $0.0123"
        );
        assert!(!turn_report(1, Duration::ZERO, &u, 0.0).contains('$'));
    }
}
