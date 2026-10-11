//! Anthropic Messages API provider using a Claude subscription OAuth token.
//!
//! Subscription requests must look like Claude Code requests, otherwise
//! Anthropic bills them as third-party usage ("You're out of extra usage").
//! The shaping follows pi-ai and @gotgenes/pi-anthropic-auth:
//! - Bearer token plus `anthropic-beta: claude-code-20250219,oauth-2025-04-20`,
//!   `user-agent: claude-cli/<version>` and `x-app: cli` headers
//! - system blocks: billing header, Claude Code identity, then our prompt
//!
//! Prompt caching uses three breakpoints, like pi-ai: last tool, last system
//! block, and the last block of the final user message. Each request then
//! reads the previous request's prefix from cache.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::AuthStore;
use crate::provider::{
    Content, Event, ImageSource, Message, ModelId, ModelInfo, Price, Provider, ProviderName,
    Request, Role, StopReason, Thinking, ToolCallId, ToolName, ToolSpec, Usage,
};

/// Claude Code version we claim to be; keep in sync with pi-ai.
pub const CLAUDE_CODE_VERSION: &str = "2.1.280";
/// Name in the registry and the credential file.
pub const NAME: ProviderName = ProviderName::from_static("anthropic");
pub const API_URL: &str = "https://api.anthropic.com/v1/messages";
const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BETAS: &str = "claude-code-20250219,oauth-2025-04-20";
const BILLING_SALT: &str = "59cf53e54c78";

/// Models with adaptive thinking (Claude 4.6+): id, name, context window and
/// USD per million tokens (input, output, cache read, cache write), plus
/// the price for prompts above 100k tokens where it differs. Prices are from
/// pi's model list; update them with new models.
type ModelRow = (&'static str, &'static str, u64, [f64; 4], Option<[f64; 4]>);
const MODELS: &[ModelRow] = &[
    (
        "claude-opus-5-5",
        "Claude Opus 5.5",
        1_000_000,
        [4.0, 20.0, 0.2, 5.0],
        None,
    ),
    (
        "claude-opus-5",
        "Claude Opus 5",
        1_000_000,
        [5.0, 25.0, 0.5, 6.25],
        None,
    ),
    (
        "claude-opus-4-8",
        "Claude Opus 4.8",
        1_000_000,
        [5.0, 25.0, 0.5, 6.25],
        None,
    ),
    (
        "claude-opus-4-7",
        "Claude Opus 4.7",
        1_000_000,
        [5.0, 25.0, 0.5, 6.25],
        None,
    ),
    (
        "claude-opus-4-6",
        "Claude Opus 4.6",
        1_000_000,
        [5.0, 25.0, 0.5, 6.25],
        None,
    ),
    (
        "claude-sonnet-5-5",
        "Claude Sonnet 5.5",
        1_000_000,
        [2.0, 10.0, 0.1, 2.5],
        None,
    ),
    (
        "claude-sonnet-5",
        "Claude Sonnet 5",
        1_000_000,
        [2.0, 10.0, 0.2, 2.5],
        None,
    ),
    (
        "claude-sonnet-4-6",
        "Claude Sonnet 4.6",
        1_000_000,
        [3.0, 15.0, 0.3, 3.75],
        None,
    ),
    (
        "claude-haiku-5-5",
        "Claude Haiku 5.5",
        1_000_000,
        [0.1, 0.5, 0.01, 0.125],
        Some([0.5, 2.5, 0.05, 0.625]),
    ),
    (
        "claude-fable-5-1",
        "Claude Fable 5.1",
        1_000_000,
        [10.0, 50.0, 0.25, 12.5],
        None,
    ),
    (
        "claude-fable-5",
        "Claude Fable 5",
        1_000_000,
        [10.0, 50.0, 1.0, 12.5],
        None,
    ),
];

pub fn price([input, output, cache_read, cache_write]: [f64; 4]) -> Price {
    Price {
        input,
        output,
        cache_read,
        cache_write,
    }
}

#[derive(Clone)]
pub struct Anthropic {
    http: reqwest::Client,
    auth: Arc<AuthStore>,
    url: String,
}

impl Anthropic {
    pub fn new(http: reqwest::Client, auth: Arc<AuthStore>) -> Self {
        Self {
            http,
            auth,
            url: API_URL.into(),
        }
    }

    async fn send(self, request: Request) -> Result<reqwest::Response> {
        let token = self.auth.access_token().await?;
        let billing = first_user_text(&request).and_then(billing_header);
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(token.expose())
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", BETAS)
            .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
            .header("x-app", "cli")
            .header("accept", "text/event-stream")
            .json(&build_body(&request, billing.as_deref()))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "anthropic: {status}: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(resp)
    }
}

/// Events of a streamed response; `send` makes the request, `feed` parses
/// the body chunks (e.g. `SseParser::feed`).
pub fn events(
    send: impl Future<Output = Result<reqwest::Response>> + Send + 'static,
    mut feed: impl FnMut(&[u8]) -> Vec<Result<Event>> + Send + 'static,
) -> BoxStream<'static, Result<Event>> {
    stream::once(send)
        .map_ok(|resp| resp.bytes_stream().map_err(anyhow::Error::from))
        .try_flatten()
        .flat_map(move |chunk| {
            stream::iter(match chunk {
                Ok(bytes) => feed(&bytes),
                Err(e) => vec![Err(e)],
            })
        })
        .boxed()
}

/// The `data:` payloads of the complete server-sent event lines in `buf`.
pub fn sse_data(buf: &mut Vec<u8>, bytes: &[u8]) -> Vec<String> {
    buf.extend_from_slice(bytes);
    let mut out = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        if let Some(data) = String::from_utf8_lossy(&line)
            .trim_end()
            .strip_prefix("data:")
        {
            out.push(data.trim().to_string());
        }
    }
    out
}

impl Provider for Anthropic {
    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event>> {
        let mut parser = SseParser::default();
        events(self.clone().send(request), move |b| parser.feed(b))
    }

    fn models(&self) -> Vec<ModelInfo> {
        MODELS
            .iter()
            .map(|&(id, name, context_window, prices, above)| ModelInfo {
                id: ModelId::from_static(id),
                name: name.into(),
                context_window,
                price: Some(price(prices)),
                price_above: above.map(|p| (100_000, price(p))),
            })
            .collect()
    }

    /// Unlisted models get the 1M window of the current ones.
    fn context_window(&self, model: &ModelId) -> u64 {
        self.model_info(model)
            .map_or(1_000_000, |m| m.context_window)
    }
}

/// Request body of `POST /v1/messages`.
#[derive(Serialize)]
pub struct Body<'a> {
    model: &'a ModelId,
    max_tokens: u32,
    stream: bool,
    system: Vec<Cached<SystemBlock<'a>>>,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Cached<&'a ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
}

/// Any block plus an optional cache breakpoint.
#[derive(Serialize)]
struct Cached<T> {
    #[serde(flatten)]
    block: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl<T> Cached<T> {
    fn new(block: T) -> Self {
        Self {
            block,
            cache_control: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CacheControl {
    Ephemeral,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SystemBlock<'a> {
    Text { text: &'a str },
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: Role,
    content: Vec<Cached<WireBlock<'a>>>,
}

/// A block as the API wants it: `Content` serializes directly, except tool
/// results with images, whose content becomes a list of blocks.
#[derive(Serialize)]
#[serde(untagged)]
enum WireBlock<'a> {
    Direct(&'a Content),
    ToolResult(ToolResultBlocks<'a>),
}

impl<'a> From<&'a Content> for WireBlock<'a> {
    fn from(c: &'a Content) -> Self {
        match c {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
            } if !images.is_empty() => {
                // Text blocks must not be empty.
                let text = (!content.is_empty()).then(|| ResultPart::Text { text: content });
                WireBlock::ToolResult(ToolResultBlocks {
                    tool_use_id,
                    content: text
                        .into_iter()
                        .chain(images.iter().map(|source| ResultPart::Image { source }))
                        .collect(),
                    is_error: *is_error,
                })
            }
            c => WireBlock::Direct(c),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename = "tool_result")]
struct ToolResultBlocks<'a> {
    tool_use_id: &'a ToolCallId,
    content: Vec<ResultPart<'a>>,
    is_error: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResultPart<'a> {
    Text { text: &'a str },
    Image { source: &'a ImageSource },
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ThinkingConfig {
    Adaptive { display: ThinkingDisplay },
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ThinkingDisplay {
    /// Returns readable thinking text.
    Summarized,
}

#[derive(Debug, PartialEq, Serialize)]
struct OutputConfig {
    effort: Effort,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// Sets a breakpoint on the last element.
fn cache_last<T>(blocks: &mut [Cached<T>]) {
    if let Some(last) = blocks.last_mut() {
        last.cache_control = Some(CacheControl::Ephemeral);
    }
}

pub fn build_body<'a>(request: &'a Request, billing: Option<&'a str>) -> Body<'a> {
    let mut system: Vec<_> = billing
        .into_iter()
        .chain([IDENTITY])
        .chain(Some(request.system.as_str()).filter(|s| !s.is_empty()))
        .map(|text| Cached::new(SystemBlock::Text { text }))
        .collect();
    cache_last(&mut system);

    let mut messages: Vec<_> = request
        .messages
        .iter()
        .map(|m: &Message| WireMessage {
            role: m.role,
            content: m
                .content
                .iter()
                .filter(|c| !crate::openai::is_reasoning(c))
                .map(|c| Cached::new(c.into()))
                .collect(),
        })
        .collect();
    if let Some(last) = messages.last_mut().filter(|m| m.role == Role::User) {
        cache_last(&mut last.content);
    }

    let mut tools: Vec<_> = request.tools.iter().map(Cached::new).collect();
    cache_last(&mut tools);

    // Adaptive thinking only (Claude 4.6+); no budget_tokens models.
    let effort = match request.thinking {
        Thinking::Off => None,
        Thinking::Low => Some(Effort::Low),
        Thinking::Medium => Some(Effort::Medium),
        Thinking::High => Some(Effort::High),
        Thinking::XHigh => Some(Effort::XHigh),
        Thinking::Max => Some(Effort::Max),
    };
    Body {
        model: &request.model,
        max_tokens: request.max_tokens,
        stream: true,
        system,
        messages,
        tools,
        thinking: effort.is_some().then_some(ThinkingConfig::Adaptive {
            display: ThinkingDisplay::Summarized,
        }),
        output_config: effort.map(|effort| OutputConfig { effort }),
    }
}

/// Text of the first user message, which the billing header is derived from.
fn first_user_text(request: &Request) -> Option<&str> {
    request
        .messages
        .iter()
        .find(|m| m.role == Role::User)?
        .content
        .iter()
        .find_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
}

/// `x-anthropic-billing-header` text derived from the first user message.
/// Mirrors `buildBillingHeaderValue` in pi-anthropic-auth, including its
/// UTF-16 indexing.
pub fn billing_header(first_user_text: &str) -> Option<String> {
    if first_user_text.is_empty() {
        return None;
    }
    let hex = |data: &[u8]| -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let utf16: Vec<u16> = first_user_text.encode_utf16().collect();
    let sampled: String = [4, 7, 20]
        .iter()
        .map(|&i| {
            utf16
                .get(i)
                .map_or("0".to_string(), |u| String::from_utf16_lossy(&[*u]))
        })
        .collect();
    let cch = &hex(first_user_text.as_bytes())[..5];
    let suffix = &hex(format!("{BILLING_SALT}{sampled}{CLAUDE_CODE_VERSION}").as_bytes())[..3];
    Some(format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION}.{suffix}; cc_entrypoint=sdk-cli; cch={cch};"
    ))
}

/// One server-sent event of the Messages API stream.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    MessageStart {
        message: MessageStart,
    },
    ContentBlockStart {
        index: u64,
        content_block: BlockStart,
    },
    ContentBlockDelta {
        index: u64,
        delta: Delta,
    },
    ContentBlockStop {
        index: u64,
    },
    MessageDelta {
        delta: MessageDelta,
        #[serde(default)]
        usage: WireUsage,
    },
    MessageStop,
    Error {
        error: ApiError,
    },
    /// `ping` and anything newer.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageStart {
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockStart {
    ToolUse {
        id: ToolCallId,
        name: ToolName,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Text arrives as deltas only.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDelta {
    stop_reason: Option<WireStopReason>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireStopReason {
    ToolUse,
    MaxTokens,
    Refusal,
    /// `end_turn`, `stop_sequence`, `pause_turn`, ...
    #[serde(other)]
    EndTurn,
}

/// Usage fields; `null` or missing means "unchanged".
#[derive(Default, Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

/// A content block still being streamed.
enum Partial {
    Tool {
        id: ToolCallId,
        name: ToolName,
        json: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    Redacted(String),
}

/// Incremental parser for the Messages API server-sent events.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Non-text blocks being streamed, by content block index.
    blocks: HashMap<u64, Partial>,
    stop: Option<StopReason>,
    usage: Usage,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<Event>> {
        let mut out = Vec::new();
        for data in sse_data(&mut self.buf, bytes) {
            if let Err(e) = self.handle(&data, &mut out) {
                out.push(Err(e));
            }
        }
        out
    }

    fn handle(&mut self, data: &str, out: &mut Vec<Result<Event>>) -> Result<()> {
        match serde_json::from_str(data)? {
            StreamEvent::MessageStart { message } => self.update_usage(message.usage),
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let partial = match content_block {
                    BlockStart::ToolUse { id, name } => Partial::Tool {
                        id,
                        name,
                        json: String::new(),
                    },
                    BlockStart::Thinking {
                        thinking,
                        signature,
                    } => Partial::Thinking {
                        thinking,
                        signature,
                    },
                    BlockStart::RedactedThinking { data } => Partial::Redacted(data),
                    BlockStart::Other => return Ok(()),
                };
                self.blocks.insert(index, partial);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                match (delta, self.blocks.get_mut(&index)) {
                    (Delta::Text { text }, _) => out.push(Ok(Event::TextDelta(text))),
                    (Delta::InputJson { partial_json }, Some(Partial::Tool { json, .. })) => {
                        json.push_str(&partial_json)
                    }
                    (
                        Delta::Thinking { thinking: delta },
                        Some(Partial::Thinking { thinking, .. }),
                    ) => {
                        thinking.push_str(&delta);
                        out.push(Ok(Event::ThinkingDelta(delta)));
                    }
                    (
                        Delta::Signature { signature: delta },
                        Some(Partial::Thinking { signature, .. }),
                    ) => signature.push_str(&delta),
                    _ => {}
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                let block = match self.blocks.remove(&index) {
                    None => return Ok(()),
                    Some(Partial::Tool { id, name, json }) => Content::ToolUse {
                        id,
                        name,
                        input: if json.trim().is_empty() {
                            serde_json::Value::Object(Default::default())
                        } else {
                            serde_json::from_str(&json)?
                        },
                    },
                    Some(Partial::Thinking {
                        thinking,
                        signature,
                    }) => Content::Thinking {
                        thinking,
                        signature,
                    },
                    Some(Partial::Redacted(data)) => Content::RedactedThinking { data },
                };
                out.push(Ok(Event::Block(block)));
            }
            StreamEvent::MessageDelta { delta, usage } => {
                self.update_usage(usage);
                self.stop = delta.stop_reason.map(|r| match r {
                    WireStopReason::ToolUse => StopReason::ToolUse,
                    WireStopReason::MaxTokens => StopReason::MaxTokens,
                    WireStopReason::Refusal => StopReason::Refusal,
                    WireStopReason::EndTurn => StopReason::EndTurn,
                });
            }
            StreamEvent::MessageStop => {
                out.push(Ok(Event::Usage(self.usage)));
                out.push(Ok(Event::Done(self.stop.unwrap_or(StopReason::EndTurn))));
            }
            StreamEvent::Error { error } => {
                bail!("anthropic stream error: {}: {}", error.kind, error.message)
            }
            StreamEvent::Other => {}
        }
        Ok(())
    }

    /// Usage arrives in `message_start` and (cumulatively) in
    /// `message_delta`; later non-null values win.
    fn update_usage(&mut self, u: WireUsage) {
        let usage = &mut self.usage;
        for (new, field) in [
            (u.input_tokens, &mut usage.input),
            (u.output_tokens, &mut usage.output),
            (u.cache_read_input_tokens, &mut usage.cache_read),
            (u.cache_creation_input_tokens, &mut usage.cache_write),
        ] {
            if let Some(n) = new {
                *field = n;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Wire JSON of the body for `request`, with the billing header it gets in `send`.
    fn wire(request: &Request) -> Value {
        let billing = first_user_text(request).and_then(billing_header);
        serde_json::to_value(build_body(request, billing.as_deref())).unwrap()
    }

    #[test]
    fn billing_header_matches_reference() {
        // Reference values computed with pi-anthropic-auth's algorithm.
        assert_eq!(
            billing_header("Say hello to the rho agent").unwrap(),
            "x-anthropic-billing-header: cc_version=2.1.280.632; cc_entrypoint=sdk-cli; cch=192bc;"
        );
        assert_eq!(
            billing_header("hi").unwrap(),
            "x-anthropic-billing-header: cc_version=2.1.280.d7b; cc_entrypoint=sdk-cli; cch=8f434;"
        );
        assert_eq!(billing_header(""), None);
    }

    fn request(model: &str, thinking: Thinking) -> Request {
        Request {
            model: ModelId::new(model),
            system: "be brief".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::text("hi")],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolUse {
                        id: ToolCallId::new("t1"),
                        name: ToolName::new("Bash"),
                        input: json!({}),
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![Content::ToolResult {
                        tool_use_id: ToolCallId::new("t1"),
                        content: "ok".into(),
                        is_error: false,
                        images: vec![],
                    }],
                },
            ],
            tools: vec![
                ToolSpec {
                    name: ToolName::new("Read"),
                    description: "read".into(),
                    input_schema: json!({"type": "object"}),
                },
                ToolSpec {
                    name: ToolName::new("Bash"),
                    description: "run".into(),
                    input_schema: json!({"type": "object"}),
                },
            ],
            max_tokens: 10_000,
            thinking,
        }
    }

    #[test]
    fn body_is_shaped_for_subscription() {
        let body = wire(&request("claude-sonnet-4-6", Thinking::Off));
        let system: Vec<&str> = body["system"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert!(system[0].starts_with("x-anthropic-billing-header:"));
        assert_eq!(system[1], IDENTITY);
        assert_eq!(system[2], "be brief");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "text", "text": "hi"})
        );
        assert_eq!(body["tools"][1]["name"], "Bash");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn cache_breakpoints_on_tools_system_and_last_user_message() {
        let body = wire(&request("claude-sonnet-4-6", Thinking::Off));
        let eph = json!({"type": "ephemeral"});
        let marked = |v: &Value| v.get("cache_control") == Some(&eph);

        let tools = body["tools"].as_array().unwrap();
        assert!(!marked(&tools[0]) && marked(&tools[1]));
        let system = body["system"].as_array().unwrap();
        assert!(marked(system.last().unwrap()));
        assert_eq!(system.iter().filter(|b| marked(b)).count(), 1);
        let messages = body["messages"].as_array().unwrap();
        assert!(!marked(&messages[0]["content"][0]));
        assert!(marked(&messages[2]["content"][0]));
        // Anthropic allows at most 4 breakpoints.
        assert_eq!(body.to_string().matches("cache_control").count(), 3);
    }

    #[test]
    fn tool_results_with_images_become_block_lists() {
        let mut req = request("claude-sonnet-4-6", Thinking::Off);
        let plain = wire(&req)["messages"][2]["content"][0].clone();
        assert_eq!(
            plain,
            json!({"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false,
                   "cache_control": {"type": "ephemeral"}})
        );

        let Content::ToolResult { images, .. } = &mut req.messages[2].content[0] else {
            unreachable!()
        };
        images.push(ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        });
        assert_eq!(
            wire(&req)["messages"][2]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ],
                "is_error": false,
                "cache_control": {"type": "ephemeral"},
            })
        );
    }

    #[test]
    fn thinking_is_adaptive() {
        let body = wire(&request("claude-opus-4-8", Thinking::Medium));
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "medium"}));

        let body = wire(&request("claude-opus-4-8", Thinking::Off));
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none());
    }

    #[test]
    fn thinking_blocks_roundtrip_in_wire_format() {
        let blocks = vec![
            Content::Thinking {
                thinking: "hmm".into(),
                signature: "sig".into(),
            },
            Content::RedactedThinking { data: "xx".into() },
        ];
        assert_eq!(
            json!(blocks),
            json!([
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "redacted_thinking", "data": "xx"},
            ])
        );
    }

    #[test]
    fn parses_stream_across_chunk_boundaries() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":2000,\"output_tokens\":1}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"see.\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hé\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{}}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"OPAQUE\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42,\"cache_read_input_tokens\":null}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        );
        let mut parser = SseParser::default();
        // Feed in 7-byte chunks to split lines and the multi-byte "é".
        let events: Vec<Event> = sse
            .as_bytes()
            .chunks(7)
            .flat_map(|c| parser.feed(c))
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            events,
            vec![
                Event::ThinkingDelta("Let me ".into()),
                Event::ThinkingDelta("see.".into()),
                Event::Block(Content::Thinking {
                    thinking: "Let me see.".into(),
                    signature: "SIG".into()
                }),
                Event::TextDelta("Hé".into()),
                Event::Block(Content::ToolUse {
                    id: ToolCallId::new("t1"),
                    name: ToolName::new("Bash"),
                    input: json!({"command": "ls"})
                }),
                Event::Block(Content::RedactedThinking {
                    data: "OPAQUE".into()
                }),
                Event::Usage(Usage {
                    input: 12,
                    output: 42,
                    cache_read: 2000,
                    cache_write: 100
                }),
                Event::Done(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn stream_error_event_is_an_error() {
        let mut parser = SseParser::default();
        let out =
            parser.feed(b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n");
        assert!(
            out[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("overloaded")
        );
    }
}
