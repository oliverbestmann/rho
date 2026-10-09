//! Provider-neutral conversation types and the `Provider` plug-in trait.
//!
//! The message types mirror the Anthropic wire format so they serialize
//! directly; other providers translate from them.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// Must be sent back unchanged (with signature) in later requests.
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text { text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub thinking: Thinking,
}

/// Reasoning effort. Providers map it to their own knobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Thinking {
    Off,
    Low,
    Medium,
    #[default]
    High,
    XHigh,
    Max,
}

impl std::str::FromStr for Thinking {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "off" => Self::Off,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => anyhow::bail!("thinking must be off|low|medium|high|xhigh|max, got {s:?}"),
        })
    }
}

/// Token counts for one model request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    /// Prompt size: everything the model read for this request.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    /// The agent loop hit its turn limit.
    MaxTurns,
    /// An extension called `Context::stop`.
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    TextDelta(String),
    /// Reasoning text, for display only; the full block follows as `Block`.
    ThinkingDelta(String),
    /// A complete non-text block (tool call, thinking) to keep in history.
    Block(Content),
    Usage(Usage),
    Done(StopReason),
}

/// A model backend. Implementations stream events for a single request;
/// dropping the stream cancels the request.
pub trait Provider: Send + Sync {
    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event>>;

    /// Context window of `model` in tokens, for usage reporting.
    fn context_window(&self, _model: &str) -> u64 {
        200_000
    }
}

/// Providers by name; models are addressed as `provider/model`.
#[derive(Default, Clone)]
pub struct Registry(HashMap<String, Arc<dyn Provider>>);

impl Registry {
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        self.0.insert(name.into(), provider);
    }

    pub fn resolve(&self, spec: &str) -> Result<(Arc<dyn Provider>, String)> {
        let (name, model) = spec
            .split_once('/')
            .with_context(|| format!("model must be provider/model, got {spec:?}"))?;
        let provider = self
            .0
            .get(name)
            .with_context(|| format!("unknown provider {name:?}"))?;
        Ok((provider.clone(), model.to_string()))
    }
}
