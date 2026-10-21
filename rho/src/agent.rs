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
    Context, Decision, Diff, DynExtension, Frontend, Profile, ProfileId, PromptSetup, RunEnd,
    SessionInfo, SessionSetup, Settle, SlashCommand, ToolCall, ToolOutput, TurnEnd,
};
use crate::feature::{Feature, FeatureId};
use crate::provider::{
    Content, Event, Message, ModelSpec, NoteType, Provider, Registry, Request, Role, StopReason,
    Thinking, ToolCallId, ToolName, ToolSpec, Usage,
};
use crate::store::{SessionId, Store};

mod commands;
mod persist;
mod usage;

use crate::tools::{DynTool, ToolKind};
use persist::Persist;
pub(crate) use usage::count;
use usage::turn_report;

/// Picks the database for a session from its cwd, see `Agent::sessions_db`.
pub type SessionsDb = Arc<dyn Fn(&Path) -> PathBuf + Send + Sync>;

/// What the agent is doing, for frontends (CLI printer, ACP session updates).
#[derive(Clone, Copy, Debug, PartialEq)]
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
        /// From `Tool::kind`; `Other` for an unknown tool.
        kind: ToolKind,
        /// From `Tool::titled`: `input.description` titles the call.
        titled: bool,
    },
    /// Output of a running tool call, from `Context::stream_output`; the
    /// `ToolEnd` output still has all of it.
    ToolStream {
        id: &'a ToolCallId,
        chunk: &'a str,
        stderr: bool,
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
        kind: crate::provider::NoteType,
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
    /// `model` / `compact_model` were given explicitly: they win over the
    /// `models.json` defaults, and a session fails if its path forbids them.
    pub model_pinned: bool,
    pub compact_pinned: bool,
    /// Allowed, default and compaction models by cwd, from `models.json`.
    pub models: crate::models::Config,
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
    /// For compacting when the prompt cache expired.
    pub compact_model: Option<ModelSpec>,
    /// Config id of the model option, see `models::Policy::key`.
    pub model_key: String,
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
    /// `None` before the first request (and after a compaction).
    pub(crate) seen: Option<(ProfileId, Vec<FeatureId>)>,
    /// The cwd, then the additional directories.
    roots: Vec<PathBuf>,
    extensions: Vec<Arc<dyn DynExtension>>,
    persist: Option<Persist>,
    /// Stored with open tool calls that run again on the next run, see
    /// `resume_session`.
    resume: bool,
}

fn notify_dirs_changed(session: &Session) {
    let text = session.dirs_notice();
    session.ctx.notify(
        NoteType::Info,
        "Additional directories changed",
        text.as_deref().unwrap_or("(none)"),
        false,
    );
}

impl Session {
    /// Resumed with open tool calls that run again on the next run: start
    /// one with `Agent::run_triggered`, they may wait for the user.
    pub fn resuming(&self) -> bool {
        self.resume
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            model: Some(self.model.clone()),
            compact_model: self.compact_model.clone(),
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
    /// what it may call from `profile::note`.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    /// The session's tools by name.
    pub fn tools(&self) -> impl Iterator<Item = (&ToolName, &Arc<dyn DynTool>)> {
        self.specs.iter().map(|s| &s.name).zip(&self.tools)
    }

    /// The tool called `name`.
    pub fn tool(&self, name: &ToolName) -> Option<&Arc<dyn DynTool>> {
        let i = self.specs.iter().position(|s| s.name == *name)?;
        Some(&self.tools[i])
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
        let mut thought = false;
        while let Some(event) = stream.next().await {
            match event? {
                Event::TextDelta(delta) => {
                    ctx.emit(AgentEvent::Text(&delta));
                    match content.last_mut() {
                        Some(Content::Text { text }) => text.push_str(&delta),
                        _ => content.push(Content::text(delta)),
                    }
                }
                // Blank thinking (e.g. omitted or an empty summary part) would
                // show an empty thought; later whitespace separates words.
                Event::ThinkingDelta(delta) if thought || !delta.trim().is_empty() => {
                    thought = true;
                    ctx.emit(AgentEvent::Thinking(&delta));
                }
                Event::ThinkingDelta(_) => {}
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
            model_pinned: false,
            compact_pinned: false,
            models: Default::default(),
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
        let roots = [vec![cwd.into()], additional_dirs].concat();
        let store = self.open_store(&roots[0]).await?;
        let mut session = self.setup(id, roots, frontend, store, false).await?;
        session.try_save().await.context("saving the new session")?;
        session.ctx.record_notes();
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
        let roots = [vec![cwd.into()], additional_dirs].concat();
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
            .await?;
        if dirs_changed {
            notify_dirs_changed(&session);
        }
        session.ctx.restore_title(info.title);
        if info.model != session.model
            && let Err(e) = self.set_model(&mut session, &info.model)
        {
            session.ctx.notify(
                NoteType::Info,
                "Model unavailable",
                &format!("{e:#}; using {}", session.model),
                true,
            );
        }
        // The model knows the stored profile; if it is gone (its extension
        // was removed), the first request tells it about the fallback.
        if let Err(e) = session.ctx.restore_profile(&info.profile) {
            session.ctx.notify(
                NoteType::Info,
                "Profile unavailable",
                &format!("{e:#}"),
                true,
            );
        }
        // Stored features win over the defaults; the model knows them.
        for f in session.ctx.active_features() {
            let _ = session.ctx.set_feature(&f, false);
        }
        for f in &info.features {
            if let Err(e) = session.ctx.set_feature(f, true) {
                session.ctx.notify(
                    NoteType::Info,
                    "Feature unavailable",
                    &format!("{e:#}"),
                    true,
                );
            }
        }
        session.ctx.restore_stored(info.extension_state);
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
        p.blocks = loaded.history.iter().map(|m| m.content.len()).sum();
        session.history = loaded.history;
        // The first open call is where rho stopped. If it can run again (it
        // only asks or reads, or its hooks had not let it run yet), the run
        // goes on from there with the next one; else it was cut off.
        session.resume = match open_calls(&session.history).first() {
            Some(Content::ToolUse { id, name, .. }) => {
                !loaded.started.contains(id) || session.tool(name).is_some_and(|t| t.repeatable())
            }
            _ => false,
        };
        if !session.resume {
            close_open_calls(&mut session.history, INTERRUPTED);
        }
        // Setup notices are not saved: they would repeat on every resume.
        session.ctx.record_notes();
        Ok(session)
    }

    async fn setup(
        &self,
        id: SessionId,
        roots: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
        store: Option<Store>,
        resumed: bool,
    ) -> Result<Session> {
        let policy = self.models.policy(&roots[0]);
        let registry = self.registry.restricted(policy.allow);
        let model = match policy.default {
            Some(default) if !self.model_pinned => default,
            _ => self.model.clone(),
        };
        let compact_model = match policy.compact {
            Some(compact) if !self.compact_pinned => Some(compact),
            _ => self.compact_model.clone(),
        };
        // `self.provider` also serves agents without a registry (tests).
        let provider = if model == self.model && registry.allows(&model) {
            self.provider.clone()
        } else {
            registry.resolve(&model)?
        };
        let ctx = Context::new(id, roots[0].clone(), frontend, registry);
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
            provider,
            model,
            compact_model,
            model_key: policy.key,
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
            persist: store.map(Persist::new),
            resume: false,
        };
        session.publish_info();
        Ok(session)
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
        session.provider = session.ctx.registry().resolve(spec)?;
        session.model = spec.clone();
        session.last_request = None;
        session.publish_info();
        session.ctx.emit(AgentEvent::Model {
            spec,
            context_window: session.provider.context_window(spec.id()),
        });
        Ok(())
    }

    /// Runs a turn for messages sent with `trigger_turn` while idle, or to
    /// go on with the open tool calls of a resumed session (`resuming`).
    /// Returns `None` if there is nothing to run.
    pub async fn run_triggered(&self, session: &mut Session) -> Option<Result<StopReason>> {
        if !session.ctx.has_triggered() && !session.resume {
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
            session
                .ctx
                .notify(NoteType::Info, &format!("Turn: {report}"), "", false);
        }
        let run = RunEnd {
            stop: result.as_ref().ok().copied(),
            before,
        };
        for ext in &self.extensions {
            ext.run_end(&session.ctx, &run).await;
        }
        // Saves the turn report and the notices of `run_end` hooks.
        session.save().await;
        result
    }

    async fn run_prompt(&self, session: &mut Session, prompt: Vec<Content>) -> Result<StopReason> {
        let ctx = session.ctx.clone();
        let exts = &self.extensions;
        // A stop requested while idle must not end this new run, nor a
        // compaction left over from a stopped one.
        ctx.take_stop();
        ctx.take_compact();
        let mut tools_called = Vec::new();
        // A restart stopped the run in a call that can run again: it goes
        // on from there.
        if std::mem::take(&mut session.resume) {
            let calls = open_calls(&session.history);
            self.run_calls(session, &calls, &mut tools_called).await;
        }
        close_open_calls(&mut session.history, CANCELLED);

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
        session.push_notes();
        push_user(&mut session.history, setup.content);
        session.save().await;

        // No turn limit, like Claude Code and pi; the user cancels runaway runs.
        loop {
            // The history ends with a user message here (prompt or tool results).
            if let Some(note) =
                crate::profile::note(&session.ctx, &mut session.seen, &session.specs)
            {
                push_user(&mut session.history, vec![Content::system_note(note)]);
            }
            let mut request = Request {
                model: session.model.id().clone(),
                system: setup.system.clone(),
                messages: crate::provider::for_model(&session.history),
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
            // Saved before its tools run, so it survives a restart while
            // they do; `close_open_calls` repairs the history after one.
            // The API rejects empty assistant messages.
            if !content.is_empty() {
                session.history.push(Message {
                    role: Role::Assistant,
                    content: content.clone(),
                });
                session.save().await;
            }

            let results = self.run_calls(session, &content, &mut tools_called).await;

            let turn = TurnEnd {
                assistant: &content,
                tool_results: &results,
                usage,
            };
            for ext in exts {
                ext.turn_end(&ctx, &turn).await;
            }

            if !results.is_empty() {
                // Before the steering messages, so they are not summarized away.
                if let Some(focus) = ctx.take_compact()
                    && !ctx.is_stopped()
                {
                    let note = match self.compact(session, &focus).await {
                        Ok(()) => "You compacted the conversation with the Compact tool. Continue the task from the summary.".into(),
                        Err(e) => {
                            ctx.notify(NoteType::Info, "Compaction failed", &format!("{e:#}"), true);
                            format!("Compaction failed, the conversation is unchanged: {e:#}")
                        }
                    };
                    push_user(&mut session.history, vec![Content::system_note(note)]);
                }
                // Steering: messages sent meanwhile ride along with the results.
                push_user(&mut session.history, ctx.drain_inbox());
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
                    more.push(Content::system_note(text));
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

    /// Runs the tool calls in `blocks` one by one, saving each result as it
    /// comes, and returns the results.
    async fn run_calls(
        &self,
        session: &mut Session,
        blocks: &[Content],
        tools_called: &mut Vec<ToolName>,
    ) -> Vec<Content> {
        let mut results = Vec::new();
        for block in blocks {
            let Content::ToolUse { id, name, input } = block else {
                continue;
            };
            let mut call = ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            };
            // Every tool_use needs a result, even when skipped.
            let (out, shown) = if session.ctx.is_stopped() {
                let out = ToolOutput {
                    output: "skipped: run stopped".into(),
                    is_error: true,
                    ..Default::default()
                };
                (out, None)
            } else {
                self.call_tool(session, &mut call).await
            };
            tools_called.push(call.name);
            if let Some(p) = &mut session.persist {
                if let Some(diff) = out.diff {
                    p.diffs.push((call.id.clone(), diff));
                }
                if let Some(shown) = shown {
                    p.outputs.push((call.id.clone(), shown));
                }
            }
            let result = Content::ToolResult {
                tool_use_id: call.id,
                content: out.output,
                is_error: out.is_error,
                images: out.images,
            };
            push_user(&mut session.history, vec![result.clone()]);
            session.save().await;
            results.push(result);
        }
        results
    }

    /// Runs `call` and returns its output and what it streamed.
    async fn call_tool(
        &self,
        session: &Session,
        call: &mut ToolCall,
    ) -> (ToolOutput, Option<String>) {
        let ctx = &session.ctx;
        // Announce first, so a confirm prompt can refer to the visible call.
        let tool = session.tool(&call.name);
        let description = tool.and_then(|t| t.describe(&call.input));
        ctx.emit(AgentEvent::ToolStart {
            id: &call.id,
            name: &call.name,
            input: &call.input,
            description: description.as_deref(),
            kind: tool.map_or_else(ToolKind::default, |t| t.kind()),
            titled: tool.is_some_and(|t| t.titled()),
        });
        let mut pending = Pending {
            ctx,
            id: Some(call.id.clone()),
        };
        ctx.set_tool_call(Some(call.id.clone()));
        let out = self.decide_and_run(session, call, tool).await;
        pending.id = None;
        let shown = ctx.set_tool_call(None);
        ctx.emit(AgentEvent::ToolEnd {
            id: &call.id,
            output: &out.output,
            is_error: out.is_error,
            diff: out.diff.as_ref(),
            images: &out.images,
        });
        (out, shown)
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
        let mut missing = vec![];
        let result = match decision {
            Decision::Block(reason) => Err(anyhow::anyhow!(reason)),
            Decision::Allow => match tool {
                Some(tool) => {
                    let spec = session.specs.iter().find(|s| s.name == call.name);
                    if let Some(spec) = spec {
                        missing = missing_required(&spec.input_schema, &call.input);
                    }
                    if let Some(p) = &session.persist {
                        // Failing, it may run again after a restart; saves
                        // fail then too, and the user was told.
                        let _ = p.store.mark_started(ctx.session_id(), &call.id).await;
                    }
                    tool.run(call.input.clone(), ctx).await
                }
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
        // Tools run without some required arguments (e.g. `description`) for
        // older sessions; remind the model instead of failing the call.
        if !out.is_error && !missing.is_empty() {
            out.output += &format!(
                "\n\n<system-reminder>\nThis call left out the required argument(s) {}. Pass every required argument in every call.\n</system-reminder>",
                missing.join(", ")
            );
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

/// The schema's `required` keys missing from `input`, as `` `key` ``.
fn missing_required(schema: &serde_json::Value, input: &serde_json::Value) -> Vec<String> {
    let Some(required) = schema["required"].as_array() else {
        return vec![];
    };
    required
        .iter()
        .filter_map(|k| k.as_str())
        .filter(|k| input.get(k).is_none_or(|v| v.is_null()))
        .map(|k| format!("`{k}`"))
        .collect()
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
        self.ctx.set_tool_call(None);
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
    ctx.notify(NoteType::Info, "Run stopped", &reason, false);
    Some(StopReason::Stopped)
}

/// The `tool_use`s of the last assistant message that have no result yet.
fn open_calls(history: &[Message]) -> Vec<Content> {
    let Some(i) = history.iter().rposition(|m| m.role == Role::Assistant) else {
        return vec![];
    };
    let answered: Vec<&ToolCallId> = history
        .get(i + 1)
        .into_iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        })
        .collect();
    history[i]
        .content
        .iter()
        .filter(|c| matches!(c, Content::ToolUse { id, .. } if !answered.contains(&id)))
        .cloned()
        .collect()
}

/// The result of a tool call whose run was cancelled before it finished.
pub(crate) const CANCELLED: &str = "cancelled: the run was stopped before this call finished";
/// The result of a tool call that rho stopped (a restart) while it ran.
pub(crate) const INTERRUPTED: &str = "interrupted: rho stopped while this call ran";

/// Gives the tool calls of the last assistant message that have no result
/// an error result with `output`, at the end of the next message, and returns
/// their ids. A run cancelled during its tool calls, or a restart, leaves
/// them open; the API rejects such a history.
pub(crate) fn close_open_calls(history: &mut Vec<Message>, output: &str) -> Vec<ToolCallId> {
    let open: Vec<ToolCallId> = open_calls(history)
        .into_iter()
        .filter_map(|c| match c {
            Content::ToolUse { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    if open.is_empty() {
        return open;
    }
    let i = history
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .unwrap();
    if history.len() == i + 1 {
        history.push(Message {
            role: Role::User,
            content: vec![],
        });
    }
    // Appended, so `Session::save` appends too. Only notes can follow the
    // results until the next prompt, and the model does not get those.
    history[i + 1]
        .content
        .extend(open.iter().map(|id| Content::ToolResult {
            tool_use_id: id.clone(),
            content: output.into(),
            is_error: true,
            images: vec![],
        }));
    open
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

    fn call(id: &str) -> Content {
        Content::ToolUse {
            id: ToolCallId::new(id),
            name: ToolName::new("Bash"),
            input: serde_json::json!({}),
        }
    }

    fn result(id: &str, content: &str) -> Content {
        Content::ToolResult {
            tool_use_id: ToolCallId::new(id),
            content: content.into(),
            is_error: true,
            images: vec![],
        }
    }

    fn msg(role: Role, content: Vec<Content>) -> Message {
        Message { role, content }
    }

    #[test]
    fn open_calls_get_results_after_the_existing_ones() {
        // Stopped before the first result: a user message is added.
        let mut h = vec![
            msg(Role::User, vec![Content::text("hi")]),
            msg(Role::Assistant, vec![call("a"), call("b")]),
        ];
        let closed = close_open_calls(&mut h, "x");
        assert_eq!(closed, [ToolCallId::new("a"), ToolCallId::new("b")]);
        assert_eq!(
            h[2],
            msg(Role::User, vec![result("a", "x"), result("b", "x")])
        );

        // After one result: only the other one.
        let mut h = vec![
            msg(Role::Assistant, vec![call("a"), call("b")]),
            msg(Role::User, vec![result("a", "done")]),
        ];
        assert_eq!(close_open_calls(&mut h, "x"), [ToolCallId::new("b")]);
        assert_eq!(h[1].content, [result("a", "done"), result("b", "x")]);

        // Nothing open: unchanged.
        let before = h.clone();
        assert!(close_open_calls(&mut h, "x").is_empty());
        assert_eq!(h, before);
        assert!(close_open_calls(&mut vec![], "x").is_empty());
    }
}
