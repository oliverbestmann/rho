//! OpenAI Chat Completions as a provider, for another rho's `/v1` (see
//! `api::openai`) under `"remotes"` in the config:
//!
//! ```json
//! { "remotes": { "home": { "url": "http://host:8080/v1", "token": "..." } } }
//! ```
//!
//! Models are then `home/anthropic/claude-sonnet-5-5`. rho's extensions to
//! the format carry everything across: thinking blocks with signatures in
//! `reasoning_details`, `is_error` and images on tool messages, cache usage,
//! model info. Other OpenAI-compatible servers work for the standard parts.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::provider::{
    Content, Event, ImageSource, ModelId, ModelInfo, Price, Provider, Request, Role, StopReason,
    ToolCallId, ToolName, Usage, sse,
};

/// `"remotes"` in the config: providers by name.
pub type Config = BTreeMap<String, Remote>;

pub const KEY: &str = "remotes";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// Base URL, up to and including `/v1`.
    pub url: String,
    /// Sent as bearer token: the remote's `api.token` or acp-token.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Clone)]
pub struct OpenAiChat {
    http: reqwest::Client,
    remote: Remote,
    /// From `GET /models`, see `refresh`.
    models: Arc<RwLock<Vec<ModelInfo>>>,
}

impl OpenAiChat {
    pub fn new(http: reqwest::Client, remote: Remote) -> Self {
        Self {
            http,
            remote,
            models: Arc::default(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.remote.url.trim_end_matches('/'))
    }

    fn request(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.remote.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// Loads the model list.
    pub async fn refresh(&self) -> Result<()> {
        let url = self.url("models");
        let resp = self.request(self.http.get(&url)).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("{url}: {status}: {text}");
        }
        let list: Value = serde_json::from_str(&text).with_context(|| url.clone())?;
        let models = list["data"]
            .as_array()
            .with_context(|| format!("{url}: no data"))?
            .iter()
            .filter_map(model_info)
            .collect();
        *self.models.write().unwrap() = models;
        Ok(())
    }

    async fn send(self, body: Value) -> Result<reqwest::Response> {
        if self.models.read().unwrap().is_empty()
            && let Err(e) = self.refresh().await
        {
            eprintln!("rho: models of {}: {e:#}", self.remote.url);
        }
        let url = self.url("chat/completions");
        let resp = self
            .request(self.http.post(&url))
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!("{url}: {status}: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp)
    }
}

impl Provider for OpenAiChat {
    fn stream(
        &self,
        request: Request,
        before_send: &dyn Fn(&mut Value),
    ) -> BoxStream<'static, Result<Event>> {
        let mut body = build_body(&request);
        before_send(&mut body);
        let mut parser = SseParser::default();
        sse::events(self.clone().send(body), move |b| parser.feed(b))
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models.read().unwrap().clone()
    }
}

/// An entry of `GET /models`, with rho's extra fields if present.
fn model_info(m: &Value) -> Option<ModelInfo> {
    let id = m["id"].as_str()?;
    let price = |p: &Value| {
        let n = |k: &str| p[k].as_f64();
        Some(Price {
            input: n("input")?,
            output: n("output")?,
            cache_read: n("cache_read")?,
            cache_write: n("cache_write")?,
        })
    };
    let above = &m["pricing_above"];
    Some(ModelInfo {
        id: ModelId::new(id),
        name: m["name"].as_str().unwrap_or(id).into(),
        context_window: m["context_window"].as_u64().unwrap_or(200_000),
        price: price(&m["pricing"]),
        price_above: above["tokens"].as_u64().zip(price(&above["pricing"])),
    })
}

/// Body of `POST /chat/completions`.
pub fn build_body(request: &Request) -> Value {
    let mut messages = vec![];
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for m in &request.messages {
        match m.role {
            Role::User => user(&mut messages, &m.content),
            Role::Assistant => messages.push(assistant(&m.content)),
        }
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_completion_tokens": request.max_tokens,
        // rho's names; "off" is sent too, the server's default may differ.
        "reasoning_effort": request.thinking.as_str(),
    });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }})
            })
            .collect();
    }
    body
}

/// A user message as user and tool messages, in order.
fn user(messages: &mut Vec<Value>, content: &[Content]) {
    let mut parts = vec![];
    let flush = |messages: &mut Vec<Value>, parts: &mut Vec<Value>| {
        if !parts.is_empty() {
            messages.push(json!({"role": "user", "content": std::mem::take(parts)}));
        }
    };
    for c in content {
        match c {
            Content::Text { text } => parts.push(json!({"type": "text", "text": text})),
            Content::Image { source } => parts.push(image(source)),
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
                ..
            } => {
                flush(messages, &mut parts);
                let content = if images.is_empty() {
                    json!(content)
                } else {
                    let text =
                        (!content.is_empty()).then(|| json!({"type": "text", "text": content}));
                    Value::Array(text.into_iter().chain(images.iter().map(image)).collect())
                };
                let mut tool =
                    json!({"role": "tool", "tool_call_id": tool_use_id, "content": content});
                if *is_error {
                    tool["is_error"] = json!(true);
                }
                messages.push(tool);
            }
            _ => {}
        }
    }
    flush(messages, &mut parts);
}

fn assistant(content: &[Content]) -> Value {
    let mut texts = vec![];
    let mut reasoning = vec![];
    let mut calls = vec![];
    for c in content {
        match c {
            Content::Text { text } => texts.push(json!({"type": "text", "text": text})),
            Content::Thinking {
                thinking,
                signature,
            } => reasoning
                .push(json!({"type": "reasoning.text", "text": thinking, "signature": signature})),
            Content::RedactedThinking { data } => {
                reasoning.push(json!({"type": "reasoning.encrypted", "data": data}))
            }
            Content::ToolUse { id, name, input } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": input.to_string()},
            })),
            _ => {}
        }
    }
    let mut message = json!({
        "role": "assistant",
        // A list keeps separate text blocks apart.
        "content": match texts.len() {
            0 => Value::Null,
            1 => texts.pop().unwrap()["text"].take(),
            _ => json!(texts),
        },
    });
    if !reasoning.is_empty() {
        message["reasoning_details"] = json!(reasoning);
    }
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    message
}

fn image(source: &ImageSource) -> Value {
    let ImageSource::Base64 { media_type, data } = source;
    json!({"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{data}")}})
}

/// Incremental parser for `chat.completion.chunk` server-sent events.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Tool calls by index: id, name, arguments so far.
    calls: Vec<(String, String, String)>,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<Event>> {
        let mut out = Vec::new();
        for data in sse::data(&mut self.buf, bytes) {
            if let Err(e) = self.handle(&data, &mut out) {
                out.push(Err(e));
            }
        }
        out
    }

    fn handle(&mut self, data: &str, out: &mut Vec<Result<Event>>) -> Result<()> {
        let chunk: Value = serde_json::from_str(data)?;
        if let Some(error) = chunk.get("error") {
            bail!("remote: {}", error["message"].as_str().unwrap_or(data));
        }
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
        if let Some(choice) = chunk["choices"].get(0) {
            let delta = &choice["delta"];
            if let Some(t) = text(&delta["reasoning"]) {
                out.push(Ok(Event::ThinkingDelta(t)));
            }
            if let Some(t) = text(&delta["content"]) {
                out.push(Ok(Event::TextDelta(t)));
            }
            for r in delta["reasoning_details"].as_array().into_iter().flatten() {
                let block = match r["type"].as_str() {
                    Some("reasoning.text") if r["signature"].is_string() => Content::Thinking {
                        thinking: r["text"].as_str().unwrap_or_default().into(),
                        signature: r["signature"].as_str().unwrap_or_default().into(),
                    },
                    Some("reasoning.encrypted") => Content::RedactedThinking {
                        data: r["data"].as_str().unwrap_or_default().into(),
                    },
                    // Summaries and unsigned text cannot be sent back.
                    _ => continue,
                };
                out.push(Ok(Event::Block(block)));
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let i = call["index"].as_u64().unwrap_or(0) as usize;
                if self.calls.len() <= i {
                    self.calls.resize_with(i + 1, Default::default);
                }
                let (id, name, args) = &mut self.calls[i];
                let f = &call["function"];
                id.push_str(call["id"].as_str().unwrap_or_default());
                name.push_str(f["name"].as_str().unwrap_or_default());
                args.push_str(f["arguments"].as_str().unwrap_or_default());
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                let tool_use = !self.calls.is_empty();
                for (id, name, args) in self.calls.drain(..) {
                    let args = if args.trim().is_empty() { "{}" } else { &args };
                    out.push(Ok(Event::Block(Content::ToolUse {
                        input: serde_json::from_str(args)
                            .with_context(|| format!("tool call {id}: arguments"))?,
                        id: ToolCallId::new(id),
                        name: ToolName::new(name),
                    })));
                }
                out.push(Ok(Event::Done(match reason {
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    _ if tool_use => StopReason::ToolUse,
                    _ => StopReason::EndTurn,
                })));
            }
        }
        let usage = &chunk["usage"];
        if usage.is_object() {
            let n = |v: &Value| v.as_u64().unwrap_or(0);
            let details = &usage["prompt_tokens_details"];
            let (cache_read, cache_write) = (
                n(&details["cached_tokens"]),
                n(&details["cache_write_tokens"]),
            );
            out.push(Ok(Event::Usage(Usage {
                // prompt_tokens includes the cached ones.
                input: n(&usage["prompt_tokens"]).saturating_sub(cache_read + cache_write),
                output: n(&usage["completion_tokens"]),
                cache_read,
                cache_write,
            })));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fragmented_tool_calls() {
        let chunks = [
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Read", "arguments": "{\"pa"}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a\"}"}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 4}}}),
        ];
        let sse: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        let out: Vec<Event> = SseParser::default()
            .feed(sse.as_bytes())
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            out,
            [
                Event::Block(Content::ToolUse {
                    id: ToolCallId::new("c1"),
                    name: ToolName::new("Read"),
                    input: json!({"path": "a"}),
                }),
                Event::Done(StopReason::ToolUse),
                Event::Usage(Usage {
                    input: 6,
                    output: 2,
                    cache_read: 4,
                    cache_write: 0,
                }),
            ]
        );
    }

    #[test]
    fn reads_model_info() {
        let m = model_info(&json!({
            "id": "anthropic/x", "name": "X", "context_window": 1000,
            "pricing": {"input": 1.0, "output": 2.0, "cache_read": 0.1, "cache_write": 1.25},
            "pricing_above": null,
        }))
        .unwrap();
        assert_eq!(
            (m.id.as_str(), m.name.as_str(), m.context_window),
            ("anthropic/x", "X", 1000)
        );
        assert_eq!(m.price, Some([1.0, 2.0, 0.1, 1.25].into()));
        assert_eq!(m.price_above, None);
    }
}
