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

pub mod sse;

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
        id: ToolCallId,
        name: ToolName,
        input: Value,
    },
    ToolResult {
        tool_use_id: ToolCallId,
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
    /// Text for the model that the user did not write (reminders, check
    /// output). Sent as text, hidden in the UI (see `for_model`).
    SystemNote {
        text: String,
    },
    /// A notice for the user (`Context::notify`), kept for the replay.
    /// Never sent to the model.
    UserNote {
        kind: NoteType,
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// How a `Content::UserNote` is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteType {
    /// A short notice, like a turn report: a collapsed tool call in ACP.
    #[default]
    Info,
    /// A longer message, like a slash command's reply: shown as text.
    Text,
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text { text: text.into() }
    }

    pub fn system_note(text: impl Into<String>) -> Self {
        Content::SystemNote { text: text.into() }
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

crate::id_type! {
    /// The id a model gives a tool call; its result refers to it.
    pub ToolCallId
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
    /// Longest side the Anthropic API accepts in requests with many images.
    pub const MAX_SIDE: u32 = 2000;

    /// Scales the image down to `MAX_SIDE`, then fails if a provider would
    /// still reject it. A rejected image in the history fails every later
    /// request of the session, so images are fitted before they are added.
    pub fn fit(&mut self) -> Result<()> {
        use base64::Engine;
        let base64 = &base64::engine::general_purpose::STANDARD;
        let ImageSource::Base64 { media_type, data } = self;
        if !Self::MEDIA_TYPES.contains(&media_type.as_str()) {
            anyhow::bail!(
                "unsupported image type {media_type:?}, expected one of {}",
                Self::MEDIA_TYPES.join(", ")
            );
        }
        let mut bytes = base64.decode(&*data).context("image is not valid base64")?;
        if let Some((smaller_type, smaller)) = shrink(&bytes)? {
            *media_type = smaller_type.into();
            *data = base64.encode(&smaller);
            bytes = smaller;
        }
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

/// `bytes` scaled down to fit `ImageSource::MAX_SIDE`, as (media type,
/// bytes); `None` if it fits already.
fn shrink(bytes: &[u8]) -> Result<Option<(&'static str, Vec<u8>)>> {
    use image::ImageReader;
    let reader = || ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format();
    let (width, height) = reader()?
        .into_dimensions()
        .context("cannot read the image")?;
    let max = ImageSource::MAX_SIDE;
    if width.max(height) <= max {
        return Ok(None);
    }
    let jpeg = reader()?.format() == Some(image::ImageFormat::Jpeg);
    let img = reader()?.decode().context("cannot read the image")?.resize(
        max,
        max,
        image::imageops::FilterType::Triangle,
    );
    let mut out = vec![];
    // `image` writes only lossless WebP, so all but JPEG become PNG.
    if jpeg {
        img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out, 85,
        ))?;
        Ok(Some(("image/jpeg", out)))
    } else {
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
        Ok(Some(("image/png", out)))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

/// The history as the model gets it: system notes become text, user notes
/// are dropped, and so are messages left empty. Requests that should share
/// a cached prefix must all be built from it.
pub fn for_model(history: &[Message]) -> Vec<Message> {
    history
        .iter()
        .filter_map(|m| {
            let content: Vec<Content> = m
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::SystemNote { text } => Some(Content::text(text.clone())),
                    Content::UserNote { .. } => None,
                    c => Some(c.clone()),
                })
                .collect();
            (!content.is_empty()).then_some(Message {
                role: m.role,
                content,
            })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub model: ModelId,
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

/// From `[input, output, cache read, cache write]`.
impl From<[f64; 4]> for Price {
    fn from([input, output, cache_read, cache_write]: [f64; 4]) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }
}

/// A model a provider offers, for model pickers and usage reporting.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: ModelId,
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
    /// `before_send` must be called with the JSON body right before it is
    /// posted, see `Extension::before_send`.
    fn stream(
        &self,
        request: Request,
        before_send: &dyn Fn(&mut Value),
    ) -> BoxStream<'static, Result<Event>>;

    /// The models offered to the user to pick from. Other models can still
    /// be requested by id.
    fn models(&self) -> Vec<ModelInfo> {
        vec![]
    }

    fn model_info(&self, model: &ModelId) -> Option<ModelInfo> {
        self.models().into_iter().find(|m| m.id == *model)
    }

    /// Context window of `model` in tokens, for usage reporting.
    fn context_window(&self, model: &ModelId) -> u64 {
        self.model_info(model).map_or(200_000, |m| m.context_window)
    }
}

/// A model as "provider/model", e.g. `anthropic/claude-sonnet-5-5`: the
/// provider's name in the `Registry` and the id sent in `Request::model`.
/// Parse user input with `FromStr`; `Display` gives the same form back.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct ModelSpec {
    provider: ProviderName,
    id: ModelId,
}

impl ModelSpec {
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self::of(ProviderName::new(provider), ModelId::new(id))
    }

    pub fn of(provider: ProviderName, id: ModelId) -> Self {
        Self { provider, id }
    }

    /// For constants, e.g. a default model.
    pub const fn from_static(provider: &'static str, id: &'static str) -> Self {
        Self {
            provider: ProviderName::from_static(provider),
            id: ModelId::from_static(id),
        }
    }

    pub fn provider(&self) -> &ProviderName {
        &self.provider
    }

    pub fn id(&self) -> &ModelId {
        &self.id
    }
}

crate::id_type! {
    /// A provider's name in the `Registry`, e.g. `anthropic`; also its
    /// entry in the credential file.
    pub ProviderName
}

crate::id_type! {
    /// A model's id at its provider, without the provider name, as in
    /// `Request::model`.
    pub ModelId
}

crate::id_type! {
    /// A tool's name, as the model calls it.
    pub ToolName
}

impl std::str::FromStr for ModelSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.split_once('/') {
            Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
                Ok(Self::new(provider, id))
            }
            _ => anyhow::bail!("model must be provider/model, got {s:?}"),
        }
    }
}

impl TryFrom<String> for ModelSpec {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl std::fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.id)
    }
}

/// Providers by name, see `ModelSpec`; optionally limited to some models
/// (`models.json`).
#[derive(Default, Clone)]
pub struct Registry {
    providers: HashMap<ProviderName, Arc<dyn Provider>>,
    allow: Option<Arc<[crate::models::Pattern]>>,
}

impl Registry {
    pub fn register(&mut self, name: ProviderName, provider: Arc<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    /// The same providers, limited to the models `allow` matches.
    pub fn restricted(&self, allow: Option<Arc<[crate::models::Pattern]>>) -> Self {
        Self {
            providers: self.providers.clone(),
            allow,
        }
    }

    pub fn allows(&self, spec: &ModelSpec) -> bool {
        crate::models::allows(self.allow.as_deref(), spec)
    }

    /// Every allowed model of every provider, providers in name order.
    pub fn models(&self) -> Vec<(ModelSpec, ModelInfo)> {
        let mut providers: Vec<_> = self.providers.iter().collect();
        providers.sort_by_key(|(name, _)| name.as_str());
        providers
            .into_iter()
            .flat_map(|(name, provider)| {
                provider
                    .models()
                    .into_iter()
                    .map(move |m| (ModelSpec::of(name.clone(), m.id.clone()), m))
            })
            .filter(|(spec, _)| self.allows(spec))
            .collect()
    }

    /// The provider serving `spec`; fails if `spec` is not allowed.
    pub fn resolve(&self, spec: &ModelSpec) -> Result<Arc<dyn Provider>> {
        if !self.allows(spec) {
            anyhow::bail!("model {spec} is not allowed here (models.json)");
        }
        let name = spec.provider();
        let provider = self
            .providers
            .get(name)
            .with_context(|| format!("unknown provider {:?}", name.as_str()))?;
        Ok(provider.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ModelId;

    #[test]
    fn for_model_sends_system_notes_as_text_and_drops_user_notes() {
        let note = Content::UserNote {
            kind: NoteType::Info,
            title: "Turn".into(),
            text: String::new(),
            is_error: false,
        };
        let msg = |role, content| Message { role, content };
        let history = [
            msg(
                Role::User,
                vec![Content::text("hi"), Content::system_note("r")],
            ),
            msg(Role::Assistant, vec![Content::text("ok")]),
            msg(Role::User, vec![note.clone()]),
        ];
        assert_eq!(
            for_model(&history),
            [
                msg(Role::User, vec![Content::text("hi"), Content::text("r")]),
                msg(Role::Assistant, vec![Content::text("ok")]),
            ]
        );
        // Saved as JSON with the session.
        let json = serde_json::to_string(&note).unwrap();
        assert_eq!(json, r#"{"type":"user_note","kind":"info","title":"Turn"}"#);
        assert_eq!(serde_json::from_str::<Content>(&json).unwrap(), note);
    }

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
            id: ModelId::new("m"),
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
        use base64::Engine;
        let b64 = &base64::engine::general_purpose::STANDARD;
        let png = |w, h| {
            let mut out = vec![];
            image::RgbImage::new(w, h)
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            b64.encode(out)
        };
        let small = png(10, 10);
        let mut fits = source("image/png", &small);
        fits.fit().unwrap();
        assert_eq!(fits, source("image/png", &small), "left as it is");

        let err = |mut s: ImageSource| s.fit().unwrap_err().to_string();
        assert!(err(source("image/tiff", "AAAA")).contains("unsupported image type"));
        assert!(err(source("image/png", "not base64!")).contains("base64"));
        assert!(err(source("image/png", "AAAA")).contains("cannot read"));
        // Noise does not compress: 2000x1000 RGB is about 6 MB.
        let mut seed = 1u32;
        let noise = image::RgbImage::from_fn(2000, 1000, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb(seed.to_le_bytes()[..3].try_into().unwrap())
        });
        let mut big = vec![];
        noise
            .write_to(&mut std::io::Cursor::new(&mut big), image::ImageFormat::Png)
            .unwrap();
        assert!(err(source("image/png", &b64.encode(big))).contains("limit is 5 MB"));
    }

    #[test]
    fn large_images_are_scaled_down() {
        use base64::Engine;
        let b64 = &base64::engine::general_purpose::STANDARD;
        let encode = |format| {
            let mut out = vec![];
            image::RgbImage::new(1024, 6144)
                .write_to(&mut std::io::Cursor::new(&mut out), format)
                .unwrap();
            b64.encode(out)
        };
        for (format, from, to) in [
            (image::ImageFormat::Jpeg, "image/jpeg", "image/jpeg"),
            (image::ImageFormat::Png, "image/png", "image/png"),
            (image::ImageFormat::WebP, "image/webp", "image/png"),
        ] {
            let mut s = ImageSource::Base64 {
                media_type: from.into(),
                data: encode(format),
            };
            s.fit().unwrap();
            let ImageSource::Base64 { media_type, data } = &s;
            assert_eq!(media_type, to);
            let img = image::load_from_memory(&b64.decode(data).unwrap()).unwrap();
            assert_eq!((img.width(), img.height()), (333, 2000), "{from}");
        }
    }

    #[test]
    fn model_specs_parse_and_print() {
        let spec: ModelSpec = "openrouter/meta/llama".parse().unwrap();
        assert_eq!(
            (spec.provider().as_str(), spec.id().as_str()),
            ("openrouter", "meta/llama")
        );
        assert_eq!(spec.to_string(), "openrouter/meta/llama");
        for bad in ["m", "/m", "p/"] {
            assert!(bad.parse::<ModelSpec>().is_err(), "{bad}");
        }
    }

    #[test]
    fn thinking_names_roundtrip() {
        for t in Thinking::ALL {
            assert_eq!(t.as_str().parse::<Thinking>().unwrap(), t);
        }
    }
}
