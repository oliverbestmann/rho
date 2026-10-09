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
use crate::provider::{Content, Event, Message, Registry, Request, StopReason, Thinking, Usage};
use crate::sandbox::{Access, Policy};
use crate::store::Store;
use crate::tools::DynTool;

/// Hooks into the agent loop. Every method is optional; implement them with
/// `async fn`.
///
/// Hooks run in registration order. Extensions handle their own errors (for
/// example by `ctx.notify(..)`); a hook cannot fail the turn.
pub trait Extension: Send + Sync {
    /// A session was created: register tools, adjust the system prompt.
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
        _name: &str,
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
    fn before_settle<'a>(
        &'a self,
        ctx: &'a Context,
        settle: &'a Settle<'_>,
    ) -> BoxFuture<'a, Option<String>>;
    fn command<'a>(
        &'a self,
        ctx: &'a Context,
        name: &'a str,
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
        name: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(Extension::command(self, ctx, name, args))
    }
}

pub struct SessionSetup {
    pub ctx: Context,
    pub tools: Vec<Arc<dyn DynTool>>,
    pub system: String,
    /// Slash commands to offer the user; handled by `Extension::command`.
    pub commands: Vec<SlashCommand>,
    /// The session continues a stored one (ACP `session/load` or
    /// `session/resume`); its history is in `ctx.store()`.
    pub resumed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SlashCommand {
    /// Without the slash, e.g. `grant-read`.
    pub name: String,
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
    pub id: String,
    pub name: String,
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

pub struct Settle<'a> {
    pub stop: StopReason,
    pub history: &'a [Message],
    /// Names of the tools that ran during this run.
    pub tools_called: &'a [String],
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
    pub key: String,
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
    pub value: String,
    pub title: String,
    pub description: Option<String>,
}

/// Field key -> answer. One value for text and single choices; fields the
/// user left empty are missing.
pub type Answers = BTreeMap<String, Vec<String>>;

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
        _tool_call_id: Option<&'a str>,
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
    session_id: String,
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
}

impl Context {
    pub fn new(
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        frontend: Arc<dyn Frontend>,
        registry: Registry,
    ) -> Self {
        let cwd = cwd.into();
        Self(Arc::new(Inner {
            session_id: session_id.into(),
            sandbox: Mutex::new(Some(Policy::base(std::slice::from_ref(&cwd)))),
            cwd,
            frontend,
            registry,
            inbox: Mutex::default(),
            stop: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            store: OnceLock::new(),
            title: Mutex::default(),
        }))
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

    pub fn session_id(&self) -> &str {
        &self.0.session_id
    }

    pub fn cwd(&self) -> &Path {
        &self.0.cwd
    }

    /// The session's current sandbox policy (`sandbox::Config::policy` for agent sessions).
    pub fn sandbox(&self) -> Option<Policy> {
        self.0.sandbox.lock().unwrap().clone()
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
        match self.sandbox() {
            Some(policy) => policy.run(f).await,
            None => Ok(tokio::task::spawn_blocking(f).await?),
        }
    }

    /// Restricts a command to the session's sandbox before it is spawned.
    pub fn sandbox_command(&self, cmd: &mut tokio::process::Command) -> Result<()> {
        match self.sandbox() {
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
            let (id, title) = (self.session_id().to_string(), title.to_string());
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

    pub async fn confirm(&self, tool_call_id: Option<&str>, title: &str) -> bool {
        self.0.frontend.confirm(tool_call_id, title).await
    }

    /// Shows a form and waits for the user to fill it in.
    pub async fn ask(&self, form: &Form) -> FormReply {
        self.0.frontend.ask(form).await
    }

    /// One-off completion outside the conversation, e.g. to name the session.
    pub async fn complete(&self, model: &str, system: &str, prompt: &str) -> Result<String> {
        let (provider, model) = self.0.registry.resolve(model)?;
        let mut stream = provider.stream(Request {
            model,
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
