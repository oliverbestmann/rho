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
    /// An image from the user, e.g. pasted into the ACP client.
    Image {
        source: ImageSource,
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
        /// Sent after `content`. Not in the Anthropic format: providers
        /// translate results with images (see `anthropic::build_body`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageSource>,
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

    /// A base64 image; see `ImageSource::check` for what models accept.
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Content::Image {
            source: ImageSource::Base64 {
                media_type: media_type.into(),
                data: data.into(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
}

impl ImageSource {
    /// Media types the Anthropic API accepts.
    pub const MEDIA_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];
    /// Largest image the Anthropic API accepts, in decoded bytes.
    pub const MAX_BYTES: usize = 5 * 1024 * 1024;

    /// Fails if a provider would reject the image. A rejected image in the
    /// history fails every later request of the session, so prompts are
    /// checked before they are added.
    pub fn check(&self) -> Result<()> {
        use base64::Engine;
        let ImageSource::Base64 { media_type, data } = self;
        if !Self::MEDIA_TYPES.contains(&media_type.as_str()) {
            anyhow::bail!(
                "unsupported image type {media_type:?}, expected one of {}",
                Self::MEDIA_TYPES.join(", ")
            );
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("image is not valid base64")?;
        if bytes.len() > Self::MAX_BYTES {
            anyhow::bail!(
                "image is {:.1} MB, the limit is {} MB",
                bytes.len() as f64 / (1024.0 * 1024.0),
                Self::MAX_BYTES / (1024 * 1024)
            );
        }
        Ok(())
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

impl Thinking {
    pub const ALL: [Thinking; 6] = [
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The name `FromStr` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
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

/// USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Price {
    pub fn cost(&self, usage: &Usage) -> f64 {
        (usage.input as f64 * self.input
            + usage.output as f64 * self.output
            + usage.cache_read as f64 * self.cache_read
            + usage.cache_write as f64 * self.cache_write)
            / 1e6
    }
}

/// A model a provider offers, for model pickers and usage reporting.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// Without the provider name, as in `Request::model`.
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub price: Option<Price>,
    /// Price for requests whose prompt is larger than the given token count
    /// (long-context pricing applies to the whole request).
    pub price_above: Option<(u64, Price)>,
}

impl ModelInfo {
    /// USD for one request, `None` if the price is unknown.
    pub fn cost(&self, usage: &Usage) -> Option<f64> {
        let price = match self.price_above {
            Some((tokens, above)) if usage.prompt() > tokens => above,
            _ => self.price?,
        };
        Some(price.cost(usage))
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

impl std::ops::Sub for Usage {
    type Output = Usage;

    /// Saturating, e.g. the usage of one prompt: totals after minus before.
    fn sub(self, o: Self) -> Usage {
        Usage {
            input: self.input.saturating_sub(o.input),
            output: self.output.saturating_sub(o.output),
            cache_read: self.cache_read.saturating_sub(o.cache_read),
            cache_write: self.cache_write.saturating_sub(o.cache_write),
        }
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

    /// The models offered to the user to pick from. Other models can still
    /// be requested by id.
    fn models(&self) -> Vec<ModelInfo> {
        vec![]
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.models().into_iter().find(|m| m.id == model)
    }

    /// Context window of `model` in tokens, for usage reporting.
    fn context_window(&self, model: &str) -> u64 {
        self.model_info(model).map_or(200_000, |m| m.context_window)
    }
}

/// Providers by name; models are addressed as `provider/model`.
#[derive(Default, Clone)]
pub struct Registry(HashMap<String, Arc<dyn Provider>>);

impl Registry {
    pub fn register(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        self.0.insert(name.into(), provider);
    }

    /// Every provider's models as `("provider/model", info)`, providers in name order.
    pub fn models(&self) -> Vec<(String, ModelInfo)> {
        let mut providers: Vec<_> = self.0.iter().collect();
        providers.sort_by_key(|(name, _)| name.as_str());
        providers
            .into_iter()
            .flat_map(|(name, provider)| {
                provider
                    .models()
                    .into_iter()
                    .map(move |m| (format!("{name}/{}", m.id), m))
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn price(input: f64) -> Price {
        Price {
            input,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }

    #[test]
    fn long_prompts_use_the_higher_price_for_the_whole_request() {
        let model = ModelInfo {
            id: "m".into(),
            name: "M".into(),
            context_window: 1_000_000,
            price: Some(price(1.0)),
            price_above: Some((100_000, price(5.0))),
        };
        let usage = |input| Usage {
            input,
            ..Default::default()
        };
        assert_eq!(model.cost(&usage(100_000)), Some(0.1));
        assert_eq!(model.cost(&usage(200_000)), Some(1.0));
        let unpriced = ModelInfo {
            price: None,
            price_above: None,
            ..model
        };
        assert_eq!(unpriced.cost(&usage(1)), None);
    }

    #[test]
    fn usage_difference_saturates() {
        let a = Usage {
            input: 5,
            output: 1,
            cache_read: 0,
            cache_write: 2,
        };
        let b = Usage {
            input: 3,
            output: 4,
            ..Default::default()
        };
        assert_eq!(
            a - b,
            Usage {
                input: 2,
                output: 0,
                cache_read: 0,
                cache_write: 2
            }
        );
    }

    #[test]
    fn images_use_the_anthropic_wire_format() {
        assert_eq!(
            serde_json::to_value(Content::image("image/png", "AAAA")).unwrap(),
            serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"},
            })
        );
    }

    #[test]
    fn image_checks() {
        let source = |media_type: &str, data: &str| ImageSource::Base64 {
            media_type: media_type.into(),
            data: data.into(),
        };
        assert!(source("image/png", "iVBORw0K").check().is_ok());
        let err = |s: ImageSource| s.check().unwrap_err().to_string();
        assert!(err(source("image/tiff", "AAAA")).contains("unsupported image type"));
        assert!(err(source("image/png", "not base64!")).contains("base64"));
        use base64::Engine;
        let big =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; ImageSource::MAX_BYTES + 1]);
        assert!(err(source("image/png", &big)).contains("limit is 5 MB"));
    }

    #[test]
    fn thinking_names_roundtrip() {
        for t in Thinking::ALL {
            assert_eq!(t.as_str().parse::<Thinking>().unwrap(), t);
        }
    }
}
