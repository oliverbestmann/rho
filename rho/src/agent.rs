//! The provider-agnostic agent loop: stream a response, run requested tools,
//! feed results back, repeat until the model stops asking for tools. Extension
//! hooks run at each step.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::StreamExt;
use serde_json::Value;

use crate::extension::{
    Context, Decision, DynExtension, Frontend, PromptSetup, SessionSetup, Settle, SlashCommand,
    ToolCall, ToolOutput, TurnEnd,
};
use crate::provider::{
    Content, Event, Message, Provider, Registry, Request, Role, StopReason, Thinking, Usage,
};
use crate::tools::DynTool;

/// What the agent is doing, for frontends (CLI printer, ACP session updates).
#[derive(Debug, PartialEq)]
pub enum AgentEvent<'a> {
    Text(&'a str),
    Thinking(&'a str),
    /// Token usage of one model request.
    Usage {
        usage: Usage,
        context_window: u64,
    },
    ToolStart {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolEnd {
        id: &'a str,
        output: &'a str,
        is_error: bool,
    },
    /// Shown to the user, not sent to the model (from `Context::notify`).
    Notice {
        title: &'a str,
        text: &'a str,
        is_error: bool,
    },
    Title(&'a str),
}

pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    /// Preamble of the system prompt, see `prompt::build`.
    pub system: String,
    /// Where `SYSTEM.md`, `APPEND_SYSTEM.md` and a global `AGENTS.md` are read from.
    pub config_dir: Option<PathBuf>,
    /// Cheaper "provider/model" for compacting when the prompt cache expired.
    pub compact_model: Option<String>,
    pub tools: Vec<Arc<dyn DynTool>>,
    pub extensions: Vec<Arc<dyn DynExtension>>,
    /// Available to extensions through `Context::complete`.
    pub registry: Registry,
    pub max_tokens: u32,
    pub max_turns: usize,
    pub thinking: Thinking,
    /// Starting sandbox for each session, from `sandbox.json`.
    pub sandbox: crate::sandbox::Config,
}

/// One conversation. Tools and system prompt are fixed at session start.
pub struct Session {
    pub ctx: Context,
    pub tools: Vec<Arc<dyn DynTool>>,
    pub system: String,
    /// Slash commands registered by extensions.
    pub commands: Vec<SlashCommand>,
    pub history: Vec<Message>,
    /// Start of the last model request, to guess if the prompt cache is warm.
    pub last_request: Option<Instant>,
    extensions: Vec<Arc<dyn DynExtension>>,
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
        }
    }

    pub async fn start_session(
        &self,
        id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        additional_dirs: Vec<PathBuf>,
        frontend: Arc<dyn Frontend>,
    ) -> Session {
        let mut roots = vec![cwd.into()];
        roots.extend(additional_dirs);
        let system = crate::prompt::build(
            &self.system,
            &self.tools,
            self.config_dir.as_deref(),
            &roots,
        );
        let ctx = Context::new(id, roots[0].clone(), frontend, self.registry.clone());
        ctx.set_sandbox(self.sandbox.policy(&roots));
        let mut setup = SessionSetup {
            ctx: ctx.clone(),
            tools: self.tools.clone(),
            system,
            commands: vec![],
        };
        for ext in &self.extensions {
            ext.session_start(&mut setup).await;
        }
        Session {
            ctx,
            tools: setup.tools,
            system: setup.system,
            commands: setup.commands,
            history: vec![],
            last_request: None,
            extensions: self.extensions.clone(),
        }
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

        let mut tools_called = Vec::new();
        for _ in 0..self.max_turns {
            let mut request = Request {
                model: self.model.clone(),
                system: setup.system.clone(),
                messages: session.history.clone(),
                tools: session.tools.iter().map(|t| t.spec()).collect(),
                max_tokens: self.max_tokens,
                thinking: self.thinking,
            };
            for ext in exts {
                ext.before_request(&ctx, &mut request).await;
            }
            if let Some(stop) = stopped(&ctx) {
                return Ok(stop);
            }

            session.last_request = Some(Instant::now());
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
                        usage = u;
                        ctx.emit(AgentEvent::Usage {
                            usage,
                            context_window: self.provider.context_window(&self.model),
                        });
                    }
                    Event::Done(reason) => stop = reason,
                }
            }

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
                    }
                } else {
                    self.call_tool(session, &mut call).await
                };
                tools_called.push(call.name);
                results.push(Content::ToolResult {
                    tool_use_id: call.id,
                    content: out.output,
                    is_error: out.is_error,
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
                if let Some(stop) = stopped(&ctx) {
                    return Ok(stop);
                }
                continue;
            }

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
        }
        Ok(StopReason::MaxTurns)
    }

    async fn call_tool(&self, session: &Session, call: &mut ToolCall) -> ToolOutput {
        let ctx = &session.ctx;
        // Announce first, so a confirm prompt can refer to the visible call.
        ctx.emit(AgentEvent::ToolStart {
            id: &call.id,
            name: &call.name,
            input: &call.input,
        });
        let mut decision = Decision::Allow;
        for ext in &self.extensions {
            decision = ext.tool_call(ctx, call).await;
            if decision != Decision::Allow {
                break;
            }
        }
        let result = match decision {
            Decision::Block(reason) => Err(anyhow::anyhow!(reason)),
            Decision::Allow => match session.tools.iter().find(|t| t.spec().name == call.name) {
                Some(tool) => tool.run(call.input.clone(), ctx).await,
                None => Err(anyhow::anyhow!("unknown tool {}", call.name)),
            },
        };
        let mut out = match result {
            Ok(output) => ToolOutput {
                output,
                is_error: false,
            },
            Err(e) => ToolOutput {
                output: format!("{e:#}"),
                is_error: true,
            },
        };
        for ext in &self.extensions {
            ext.tool_result(ctx, call, &mut out).await;
        }
        ctx.emit(AgentEvent::ToolEnd {
            id: &call.id,
            output: &out.output,
            is_error: out.is_error,
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
