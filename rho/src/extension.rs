//! Extension API: hooks into the agent loop, plus the `Context` handle that hooks
//! and tools use to talk to the session (send messages, ask the user, call a
//! model).
//!
//! Modeled on pi's extension events; see `docs` in the README for the mapping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use futures::StreamExt;
pub use futures::future::BoxFuture;
use serde_json::Value;

use crate::agent::AgentEvent;
use crate::provider::{
    Content, Event, Message, ModelSpec, Registry, Request, StopReason, Thinking, ToolCallId,
    ToolName, Usage,
};

/// What a session runs with and has used so far, see `Context::session_info`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionInfo {
    /// `None` for a `Context` outside an agent session.
    pub model: Option<ModelSpec>,
    pub context_window: u64,
    pub thinking: Thinking,
    /// The current profile's id.
    pub profile: ProfileId,
    /// Usage of the latest request.
    pub last_usage: Usage,
    /// Usage of all requests so far.
    pub usage: Usage,
    /// USD so far, 0 if the price is unknown.
    pub cost: f64,
    /// Model requests so far.
    pub requests: u32,
}

impl SessionInfo {
    /// Context in use: the latest prompt plus its output.
    pub fn context_used(&self) -> u64 {
        self.last_usage.prompt() + self.last_usage.output
    }
}
use crate::sandbox::{Access, Policy};
use crate::store::{SessionId, Store};
use crate::tools::DynTool;

/// Hooks into the agent loop. Every method is optional; implement them with
/// `async fn`.
///
/// Hooks run in registration order. Extensions handle their own errors (for
/// example by `ctx.notify(..)`); a hook cannot fail the turn.
pub trait Extension: Send + Sync {
    /// A session was created: register tools, add to the system prompt.
    fn session_start(&self, _session: &mut SessionSetup) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The session is being dropped: clean up (kill background work).
    fn session_end(&self, _ctx: &Context) {}

    /// A user prompt (or a triggered message) is about to start a run.
    fn before_prompt(
        &self,
        _ctx: &Context,
        _prompt: &mut PromptSetup,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Last chance to edit a model request (context pruning, extra params).
    fn before_request(
        &self,
        _ctx: &Context,
        _request: &mut Request,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The model wants to call a tool. Can change the input or block it; the
    /// block reason is returned to the model as a tool error.
    fn tool_call(
        &self,
        _ctx: &Context,
        _call: &mut ToolCall,
    ) -> impl Future<Output = Decision> + Send {
        async { Decision::Allow }
    }

    /// A tool finished (or was blocked). Can rewrite the result.
    fn tool_result(
        &self,
        _ctx: &Context,
        _call: &ToolCall,
        _result: &mut ToolOutput,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// One model request and its tool calls are done.
    fn turn_end(&self, _ctx: &Context, _turn: &TurnEnd<'_>) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// A run (a prompt or triggered message, until the model stops) ended,
    /// after its turn report. Not called when the run is cancelled.
    fn run_end(&self, _ctx: &Context, _run: &RunEnd) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The model stopped. Return a message to continue the run with it
    /// (e.g. failing test output), or `None` to let the run end.
    fn before_settle(
        &self,
        _ctx: &Context,
        _settle: &Settle<'_>,
    ) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }

    /// The user ran `/name args` for a command this extension added to
    /// `SessionSetup::commands`. Return the reply shown to the user, or
    /// `None` if the command is not this extension's. Not sent to the model.
    fn command(
        &self,
        _ctx: &Context,
        _name: &CommandName,
        _args: &str,
    ) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }
}

/// Object-safe form of [`Extension`], implemented for every extension.
/// `async fn` traits cannot be `dyn` (yet), so this boxes the futures.
/// Remove it once async fn in dyn traits is stable.
pub trait DynExtension: Send + Sync {
    fn session_start<'a>(&'a self, session: &'a mut SessionSetup) -> BoxFuture<'a, ()>;
    fn session_end(&self, ctx: &Context);
    fn before_prompt<'a>(
        &'a self,
        ctx: &'a Context,
        prompt: &'a mut PromptSetup,
    ) -> BoxFuture<'a, ()>;
    fn before_request<'a>(
        &'a self,
        ctx: &'a Context,
        request: &'a mut Request,
    ) -> BoxFuture<'a, ()>;
    fn tool_call<'a>(&'a self, ctx: &'a Context, call: &'a mut ToolCall)
    -> BoxFuture<'a, Decision>;
    fn tool_result<'a>(
        &'a self,
        ctx: &'a Context,
        call: &'a ToolCall,
        result: &'a mut ToolOutput,
    ) -> BoxFuture<'a, ()>;
    fn turn_end<'a>(&'a self, ctx: &'a Context, turn: &'a TurnEnd<'_>) -> BoxFuture<'a, ()>;
    fn run_end<'a>(&'a self, ctx: &'a Context, run: &'a RunEnd) -> BoxFuture<'a, ()>;
    fn before_settle<'a>(
        &'a self,
        ctx: &'a Context,
        settle: &'a Settle<'_>,
    ) -> BoxFuture<'a, Option<String>>;
    fn command<'a>(
        &'a self,
        ctx: &'a Context,
        name: &'a CommandName,
        args: &'a str,
    ) -> BoxFuture<'a, Option<String>>;
}

impl<T: Extension> DynExtension for T {
    fn session_start<'a>(&'a self, session: &'a mut SessionSetup) -> BoxFuture<'a, ()> {
        Box::pin(Extension::session_start(self, session))
    }
    fn session_end(&self, ctx: &Context) {
        Extension::session_end(self, ctx)
    }
    fn before_prompt<'a>(
        &'a self,
        ctx: &'a Context,
        prompt: &'a mut PromptSetup,
    ) -> BoxFuture<'a, ()> {
        Box::pin(Extension::before_prompt(self, ctx, prompt))
    }
    fn before_request<'a>(
        &'a self,
        ctx: &'a Context,
        request: &'a mut Request,
    ) -> BoxFuture<'a, ()> {
        Box::pin(Extension::before_request(self, ctx, request))
    }
    fn tool_call<'a>(
        &'a self,
        ctx: &'a Context,
        call: &'a mut ToolCall,
    ) -> BoxFuture<'a, Decision> {
        Box::pin(Extension::tool_call(self, ctx, call))
    }
    fn tool_result<'a>(
        &'a self,
        ctx: &'a Context,
        call: &'a ToolCall,
        result: &'a mut ToolOutput,
    ) -> BoxFuture<'a, ()> {
        Box::pin(Extension::tool_result(self, ctx, call, result))
    }
    fn turn_end<'a>(&'a self, ctx: &'a Context, turn: &'a TurnEnd<'_>) -> BoxFuture<'a, ()> {
        Box::pin(Extension::turn_end(self, ctx, turn))
    }
    fn run_end<'a>(&'a self, ctx: &'a Context, run: &'a RunEnd) -> BoxFuture<'a, ()> {
        Box::pin(Extension::run_end(self, ctx, run))
    }
    fn before_settle<'a>(
        &'a self,
        ctx: &'a Context,
        settle: &'a Settle<'_>,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(Extension::before_settle(self, ctx, settle))
    }
    fn command<'a>(
        &'a self,
        ctx: &'a Context,
        name: &'a CommandName,
        args: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(Extension::command(self, ctx, name, args))
    }
}

pub struct SessionSetup {
    pub ctx: Context,
    /// Starts with the agent's tools. The system prompt is built after
    /// `session_start`, so tools added here are listed in it too.
    pub tools: Vec<Arc<dyn DynTool>>,
    /// Appended to the system prompt (after `<cwd>`); starts empty.
    pub system: String,
    /// Slash commands to offer the user; handled by `Extension::command`.
    pub commands: Vec<SlashCommand>,
    /// The session continues a stored one (ACP `session/load` or
    /// `session/resume`); its history is in `ctx.store()`.
    pub resumed: bool,
    /// Profiles the user can switch between; starts with `Profile::write`.
    /// Extensions add their own (or hide their tools in `write`).
    pub profiles: Vec<Profile>,
}

crate::id_type! {
    /// A `Profile`'s id, e.g. `write` or `plan`; stored with the session.
    pub ProfileId
}

/// `Profile::WRITE`, the profile every session has.
impl Default for ProfileId {
    fn default() -> Self {
        Profile::WRITE
    }
}

/// A way of working the user (or an extension, `Context::set_profile`) can
/// switch to during a session, e.g. a read-only "plan" profile. Switching
/// tells the model which tools appeared or disappeared, and the profile's
/// instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub description: String,
    /// Tools the model does not get in this profile; calls to them fail.
    pub hidden_tools: Vec<ToolName>,
    /// Sandboxed tools can only read: read-write rules apply as read-only.
    pub read_only: bool,
    /// Told to the model when the session switches to this profile.
    pub instructions: Option<String>,
}

impl Profile {
    pub const WRITE: ProfileId = ProfileId::from_static("write");

    /// The default: every tool, the full sandbox.
    pub fn write() -> Self {
        Self {
            id: Self::WRITE,
            name: "Write".into(),
            description: "Read and change files".into(),
            hidden_tools: vec![],
            read_only: false,
            instructions: None,
        }
    }

    pub fn shows(&self, tool: &ToolName) -> bool {
        !self.hidden_tools.contains(tool)
    }
}

crate::id_type! {
    /// A slash command's name without the slash, e.g. `grant-read`.
    pub CommandName
}

crate::id_type! {
    /// Names a `Field` in `Answers`.
    pub FieldKey
}

crate::id_type! {
    /// A `Choice` as returned in `Answers`.
    pub ChoiceValue
}

#[derive(Clone, Debug, PartialEq)]
pub struct SlashCommand {
    pub name: CommandName,
    pub description: String,
    /// Shown as a placeholder for the arguments; `None` takes no arguments.
    pub hint: Option<String>,
}

pub struct PromptSetup {
    pub content: Vec<Content>,
    /// System prompt for this run only. Changing it per run breaks caching.
    pub system: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub input: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Allow,
    Block(String),
}

/// What a tool returns (and what `tool_result` hooks can rewrite).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolOutput {
    /// Sent to the model.
    pub output: String,
    pub is_error: bool,
    /// Images for the model, sent with `output`, and shown to the user.
    pub images: Vec<crate::provider::ImageSource>,
    /// A file change to show the user (ACP diff); not sent to the model.
    pub diff: Option<Diff>,
}

impl From<String> for ToolOutput {
    fn from(output: String) -> Self {
        Self {
            output,
            ..Default::default()
        }
    }
}

impl From<&str> for ToolOutput {
    fn from(output: &str) -> Self {
        output.to_string().into()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diff {
    /// Absolute.
    pub path: PathBuf,
    pub old: String,
    pub new: String,
}

pub struct TurnEnd<'a> {
    pub assistant: &'a [Content],
    pub tool_results: &'a [Content],
    pub usage: Usage,
}

pub struct RunEnd {
    /// `None` if the run failed.
    pub stop: Option<StopReason>,
    /// `Context::session_info` when the run started.
    pub before: SessionInfo,
}

pub struct Settle<'a> {
    pub stop: StopReason,
    pub history: &'a [Message],
    /// Names of the tools that ran during this run.
    pub tools_called: &'a [ToolName],
}

/// Questions to show the user in one dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    /// Shown above the fields.
    pub message: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// Names the field in `Answers`.
    pub key: FieldKey,
    pub title: String,
    pub description: Option<String>,
    pub kind: FieldKind,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    /// Free text.
    Text,
    Choice {
        options: Vec<Choice>,
        /// More than one option can be picked.
        multiple: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// What is returned in `Answers`.
    pub value: ChoiceValue,
    pub title: String,
    pub description: Option<String>,
}

/// Field key -> answer. One value for text and single choices; fields the
/// user left empty are missing.
pub type Answers = BTreeMap<FieldKey, Vec<String>>;

#[derive(Clone, Debug, PartialEq)]
pub enum FormReply {
    Answers(Answers),
    /// The user dismissed the form.
    Declined,
    /// The frontend cannot show forms.
    Unsupported,
}

/// The user-facing side of a session (CLI printer, ACP connection).
pub trait Frontend: Send + Sync {
    fn event(&self, event: AgentEvent<'_>);

    /// Ask the user to approve something. Without a UI this is `false`.
    fn confirm<'a>(
        &'a self,
        _tool_call_id: Option<&'a ToolCallId>,
        _title: &'a str,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// Ask the user to fill in a form. Without a UI this is `Unsupported`.
    fn ask<'a>(&'a self, _form: &'a Form) -> BoxFuture<'a, FormReply> {
        Box::pin(async { FormReply::Unsupported })
    }
}

/// A frontend that shows nothing and approves nothing.
pub struct NoFrontend;

impl Frontend for NoFrontend {
    fn event(&self, _event: AgentEvent<'_>) {}
}

/// Session handle for hooks and tools. Cheap to clone; keep it to send
/// messages later (e.g. from a background task).
#[derive(Clone)]
pub struct Context(Arc<Inner>);

struct Inner {
    session_id: SessionId,
    cwd: PathBuf,
    frontend: Arc<dyn Frontend>,
    registry: Registry,
    /// Messages for the model: (text, trigger_turn).
    inbox: Mutex<Vec<(String, bool)>>,
    /// Set by `Context::stop`, read by the agent loop.
    stop: Mutex<Option<String>>,
    wake: tokio::sync::Notify,
    /// What `Bash` may touch; `None` runs commands unsandboxed.
    sandbox: Mutex<Option<Policy>>,
    /// Where the session is saved; unset if it is not.
    store: OnceLock<Store>,
    title: Mutex<Option<String>>,
    /// The available profiles and the index of the current one.
    profiles: Mutex<(Vec<Profile>, usize)>,
    /// Kept up to date by the agent loop; `profile` is filled in on read.
    info: Mutex<SessionInfo>,
}

impl Context {
    pub fn new(
        session_id: SessionId,
        cwd: impl Into<PathBuf>,
        frontend: Arc<dyn Frontend>,
        registry: Registry,
    ) -> Self {
        let cwd = cwd.into();
        Self(Arc::new(Inner {
            session_id,
            sandbox: Mutex::new(Some(Policy::base(std::slice::from_ref(&cwd)))),
            cwd,
            frontend,
            registry,
            inbox: Mutex::default(),
            stop: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            store: OnceLock::new(),
            title: Mutex::default(),
            profiles: Mutex::new((vec![Profile::write()], 0)),
            info: Mutex::default(),
        }))
    }

    /// The session's model, thinking level, profile and usage right now.
    pub fn session_info(&self) -> SessionInfo {
        SessionInfo {
            profile: self.profile().id,
            ..self.0.info.lock().unwrap().clone()
        }
    }

    pub(crate) fn set_session_info(&self, info: SessionInfo) {
        *self.0.info.lock().unwrap() = info;
    }

    /// The current profile.
    pub fn profile(&self) -> Profile {
        let (profiles, current) = &*self.0.profiles.lock().unwrap();
        profiles[*current].clone()
    }

    pub fn profiles(&self) -> Vec<Profile> {
        self.0.profiles.lock().unwrap().0.clone()
    }

    /// Replaces the profiles; the current one is the first. Agent sessions
    /// get theirs from `SessionSetup::profiles`.
    pub fn set_profiles(&self, profiles: Vec<Profile>) {
        assert!(!profiles.is_empty(), "a session needs a profile");
        *self.0.profiles.lock().unwrap() = (profiles, 0);
    }

    /// Switches to profile `id`, from the next model request on (also in
    /// the middle of a run), and tells the frontend. The model is told
    /// with that request.
    pub fn set_profile(&self, id: &ProfileId) -> Result<()> {
        if self.restore_profile(id)? {
            self.emit(AgentEvent::Profile(id));
        }
        Ok(())
    }

    /// `set_profile` without telling the frontend. Returns whether it changed.
    pub(crate) fn restore_profile(&self, id: &ProfileId) -> Result<bool> {
        let (profiles, current) = &mut *self.0.profiles.lock().unwrap();
        let Some(i) = profiles.iter().position(|p| p.id == *id) else {
            anyhow::bail!("unknown profile {:?}", id.as_str());
        };
        Ok(std::mem::replace(current, i) != i)
    }

    /// The database this session is saved in, `None` if it is not saved.
    /// Use it to read past sessions; writing is up to rho.
    pub fn store(&self) -> Option<&Store> {
        self.0.store.get()
    }

    pub(crate) fn set_store(&self, store: Store) {
        let _ = self.0.store.set(store);
    }

    pub fn title(&self) -> Option<String> {
        self.0.title.lock().unwrap().clone()
    }

    /// Sets the title without showing or saving it (restored sessions).
    pub(crate) fn restore_title(&self, title: Option<String>) {
        *self.0.title.lock().unwrap() = title;
    }

    pub fn session_id(&self) -> &SessionId {
        &self.0.session_id
    }

    pub fn cwd(&self) -> &Path {
        &self.0.cwd
    }

    /// The session's sandbox policy (`sandbox::Config::policy` for agent
    /// sessions, plus grants). A read-only profile restricts it further, see
    /// `effective_sandbox`.
    pub fn sandbox(&self) -> Option<Policy> {
        self.0.sandbox.lock().unwrap().clone()
    }

    /// What sandboxed code runs under: `sandbox` with the current profile
    /// applied.
    pub fn effective_sandbox(&self) -> Option<Policy> {
        let policy = self.sandbox()?;
        Some(if self.profile().read_only {
            policy.read_only()
        } else {
            policy
        })
    }

    /// Replace the policy; `None` turns the sandbox off for this session.
    pub fn set_sandbox(&self, policy: Option<Policy>) {
        *self.0.sandbox.lock().unwrap() = policy;
    }

    /// Runs `f` (blocking file access) under the session's sandbox, on its
    /// own thread. Use it in tools and extensions to touch files with the
    /// same rules as Bash.
    pub async fn run_sandboxed<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        match self.effective_sandbox() {
            Some(policy) => policy.run(f).await,
            None => Ok(tokio::task::spawn_blocking(f).await?),
        }
    }

    /// Restricts a command to the session's sandbox before it is spawned.
    pub fn sandbox_command(&self, cmd: &mut tokio::process::Command) -> Result<()> {
        match self.effective_sandbox() {
            Some(policy) => policy.apply(cmd),
            None => Ok(()),
        }
    }

    /// Allow `path` (relative to the cwd) for the rest of the session.
    pub fn grant(&self, path: impl AsRef<Path>, access: Access) {
        if let Some(policy) = self.0.sandbox.lock().unwrap().as_mut() {
            policy.allow(self.0.cwd.join(path), access);
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.0.registry
    }

    pub fn emit(&self, event: AgentEvent<'_>) {
        self.0.frontend.event(event);
    }

    /// Show something to the user without sending it to the model.
    pub fn notify(&self, title: &str, text: &str, is_error: bool) {
        self.emit(AgentEvent::Notice {
            title,
            text,
            is_error,
        });
    }

    /// Shows the title and saves it with the session.
    pub fn set_title(&self, title: &str) {
        self.restore_title(Some(title.into()));
        // Saved sessions also get it with their next save; this covers a
        // title that arrives after the last one.
        if let Some(store) = self.store().cloned()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let (id, title) = (self.session_id().clone(), title.to_string());
            rt.spawn(async move { store.set_title(&id, &title).await });
        }
        self.emit(AgentEvent::Title(title));
    }

    /// Send a message to the model. During a run it is added at the next tool
    /// result boundary. When idle it waits for the next prompt, or starts a
    /// run right away if `trigger_turn` is set.
    pub fn send(&self, text: impl Into<String>, trigger_turn: bool) {
        self.0
            .inbox
            .lock()
            .unwrap()
            .push((text.into(), trigger_turn));
        self.0.wake.notify_one();
    }

    /// End the current run cleanly (like pi's `ctx.abort()`). The loop checks
    /// after `before_prompt` (the prompt and queued messages are dropped, like
    /// pi's `input` -> `handled`), after `before_request`
    /// (the request is not sent), between tool calls (remaining calls are
    /// skipped) and before continuing. The run returns `StopReason::Stopped`.
    pub fn stop(&self, reason: impl Into<String>) {
        *self.0.stop.lock().unwrap() = Some(reason.into());
    }

    pub fn is_stopped(&self) -> bool {
        self.0.stop.lock().unwrap().is_some()
    }

    pub(crate) fn take_stop(&self) -> Option<String> {
        self.0.stop.lock().unwrap().take()
    }

    pub async fn confirm(&self, tool_call_id: Option<&ToolCallId>, title: &str) -> bool {
        self.0.frontend.confirm(tool_call_id, title).await
    }

    /// Shows a form and waits for the user to fill it in.
    pub async fn ask(&self, form: &Form) -> FormReply {
        self.0.frontend.ask(form).await
    }

    /// One-off completion outside the conversation, e.g. to name the session.
    pub async fn complete(&self, model: &ModelSpec, system: &str, prompt: &str) -> Result<String> {
        let provider = self.0.registry.resolve(model)?;
        let mut stream = provider.stream(Request {
            model: model.id().clone(),
            system: system.into(),
            messages: vec![Message {
                role: crate::provider::Role::User,
                content: vec![Content::text(prompt)],
            }],
            tools: vec![],
            max_tokens: 1024,
            thinking: Thinking::Off,
        });
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let Event::TextDelta(t) = event? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    /// Resolves when a message was sent; check `has_triggered` afterwards.
    pub async fn wait_inbox(&self) {
        self.0.wake.notified().await;
    }

    pub fn has_triggered(&self) -> bool {
        self.0.inbox.lock().unwrap().iter().any(|(_, t)| *t)
    }

    pub(crate) fn drain_inbox(&self) -> Vec<Content> {
        let msgs = std::mem::take(&mut *self.0.inbox.lock().unwrap());
        msgs.into_iter().map(|(t, _)| Content::text(t)).collect()
    }

    pub(crate) fn drain_if_triggered(&self) -> Vec<Content> {
        if self.has_triggered() {
            self.drain_inbox()
        } else {
            vec![]
        }
    }
}
