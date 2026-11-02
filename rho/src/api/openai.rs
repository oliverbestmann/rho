//! OpenAI's `GET /v1/models` and `POST /v1/chat/completions` (streaming or
//! not): text, `data:` URL images, tool calls. Non-standard, so rho can use
//! another rho as provider: `reasoning_details` on assistant messages
//! (OpenRouter's shape) carries thinking blocks with their signatures,
//! `is_error` on tool messages, and model info in `/v1/models`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::provider::{
    Content, Event, Message, Price, Provider, Request, Role, StopReason, Thinking, ToolCallId,
    ToolName, ToolSpec, Usage,
};

pub(super) fn router() -> Router<Arc<super::Api>> {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
}

/// An error in OpenAI's shape.
#[derive(Debug)]
struct ApiError(StatusCode, &'static str, String);

impl ApiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message.into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let ApiError(status, kind, message) = self;
        json_response(
            status,
            &json!({"error": {"message": message, "type": kind}}),
        )
    }
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn models(State(api): State<Arc<super::Api>>) -> Response {
    let data: Vec<Value> = super::offered(&api.agent)
        .into_iter()
        .map(|(spec, info)| {
            let price = |p: &Price| {
                json!({
                    "input": p.input,
                    "output": p.output,
                    "cache_read": p.cache_read,
                    "cache_write": p.cache_write,
                })
            };
            json!({
                "id": spec.to_string(),
                "object": "model",
                "created": 0,
                "owned_by": spec.provider().as_str(),
                "name": info.name,
                "context_window": info.context_window,
                "pricing": info.price.as_ref().map(price),
                "pricing_above": info.price_above.as_ref().map(|(tokens, p)| {
                    json!({"tokens": tokens, "pricing": price(p)})
                }),
            })
        })
        .collect();
    json_response(StatusCode::OK, &json!({"object": "list", "data": data}))
}

#[derive(Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    stream_options: Option<StreamOptions>,
    max_tokens: Option<u32>,
    max_completion_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    #[serde(default)]
    tools: Vec<Tool>,
    tool_choice: Option<Value>,
    n: Option<u32>,
}

#[derive(Deserialize)]
struct Tool {
    function: FunctionDef,
}

#[derive(Deserialize)]
struct FunctionDef {
    name: String,
    #[serde(default)]
    description: String,
    parameters: Option<Value>,
}

#[derive(Deserialize)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: Option<MessageContent>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    reasoning_details: Vec<Reasoning>,
    tool_call_id: Option<String>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Deserialize)]
struct ToolCall {
    id: String,
    function: FunctionCall,
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

/// A thinking block in OpenRouter's `reasoning_details` shape.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum Reasoning {
    #[serde(rename = "reasoning.text")]
    Text {
        #[serde(default)]
        text: String,
        signature: Option<String>,
    },
    #[serde(rename = "reasoning.encrypted")]
    Encrypted { data: String },
    /// Summaries and other kinds cannot be sent back.
    #[serde(other)]
    Other,
}

/// A thinking block as `reasoning_details` entry; `None` for other blocks.
fn reasoning_json(c: &Content) -> Option<Value> {
    match c {
        Content::Thinking {
            thinking,
            signature,
        } => Some(json!({"type": "reasoning.text", "text": thinking, "signature": signature})),
        Content::RedactedThinking { data } => {
            Some(json!({"type": "reasoning.encrypted", "data": data}))
        }
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Deserialize)]
struct ImageUrl {
    url: String,
}

/// The provider and request for `req`, checked against the config.
fn build(agent: &Agent, req: &ChatRequest) -> Result<(Arc<dyn Provider>, Request), ApiError> {
    let not_found = || {
        ApiError(
            StatusCode::NOT_FOUND,
            "model_not_found",
            format!("model {:?} is not offered here", req.model),
        )
    };
    let (spec, provider) = super::resolve(agent, &req.model).ok_or_else(not_found)?;
    if req.tool_choice.as_ref().is_some_and(|c| c != "auto") {
        return Err(ApiError::invalid("tool_choice must be \"auto\""));
    }
    if req.n.is_some_and(|n| n != 1) {
        return Err(ApiError::invalid("n must be 1"));
    }
    let thinking = match req.reasoning_effort.as_deref() {
        None | Some("none" | "minimal") => Thinking::Off,
        Some(effort) => effort
            .parse()
            .map_err(|e: anyhow::Error| ApiError::invalid(e.to_string()))?,
    };

    let mut system = vec![];
    let mut messages: Vec<Message> = vec![];
    for m in &req.messages {
        let content = contents(m.content.as_ref())?;
        match m.role.as_str() {
            "system" | "developer" => {
                for c in content {
                    let Content::Text { text } = c else {
                        return Err(ApiError::invalid("system messages take text only"));
                    };
                    system.push(text);
                }
                continue;
            }
            "user" => {
                super::push(&mut messages, Role::User, content);
                continue;
            }
            "tool" => {
                let id = m
                    .tool_call_id
                    .as_deref()
                    .ok_or_else(|| ApiError::invalid("tool messages need tool_call_id"))?;
                let result = super::tool_result(id, content, m.is_error);
                super::push(&mut messages, Role::User, vec![result]);
                continue;
            }
            "assistant" => {}
            role => return Err(ApiError::invalid(format!("unsupported role {role:?}"))),
        }
        if content.iter().any(|c| matches!(c, Content::Image { .. })) {
            return Err(ApiError::invalid("assistant messages take text only"));
        }
        let mut blocks: Vec<Content> = m
            .reasoning_details
            .iter()
            .filter_map(|r| match r {
                Reasoning::Text {
                    text,
                    signature: Some(signature),
                } => Some(Content::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                }),
                Reasoning::Encrypted { data } => {
                    Some(Content::RedactedThinking { data: data.clone() })
                }
                _ => None,
            })
            .collect();
        blocks.extend(content);
        for call in &m.tool_calls {
            let input = match call.function.arguments.trim() {
                "" => json!({}),
                args => serde_json::from_str(args).map_err(|e| {
                    ApiError::invalid(format!("tool call {:?}: arguments: {e}", call.id))
                })?,
            };
            blocks.push(Content::ToolUse {
                id: ToolCallId::new(&call.id),
                name: ToolName::new(&call.function.name),
                input,
            });
        }
        super::push(&mut messages, Role::Assistant, blocks);
    }
    if messages.is_empty() {
        return Err(ApiError::invalid("no user or assistant messages"));
    }
    Ok((
        provider,
        Request {
            model: spec.id().clone(),
            system: system.join("\n\n"),
            messages,
            tools: req
                .tools
                .iter()
                .map(|t| ToolSpec {
                    name: ToolName::new(&t.function.name),
                    description: t.function.description.clone(),
                    input_schema: t
                        .function
                        .parameters
                        .clone()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                })
                .collect(),
            max_tokens: req
                .max_completion_tokens
                .or(req.max_tokens)
                .unwrap_or(agent.max_tokens),
            thinking,
        },
    ))
}

fn contents(content: Option<&MessageContent>) -> Result<Vec<Content>, ApiError> {
    let parts = match content {
        None => return Ok(vec![]),
        Some(MessageContent::Text(text)) => return Ok(vec![Content::text(text)]),
        Some(MessageContent::Parts(parts)) => parts,
    };
    parts
        .iter()
        .map(|p| match p {
            Part::Text { text } => Ok(Content::text(text)),
            Part::ImageUrl { image_url } => image(&image_url.url),
        })
        .collect()
}

/// shortcut: only `data:` URLs, remote images are not fetched; fetch them if a client needs it.
fn image(url: &str) -> Result<Content, ApiError> {
    let (media_type, data) = url
        .strip_prefix("data:")
        .and_then(|u| u.split_once(";base64,"))
        .ok_or_else(|| ApiError::invalid("images must be base64 data: URLs"))?;
    super::image(media_type, data).map_err(|e| ApiError::invalid(format!("{e:#}")))
}

async fn chat(State(api): State<Arc<super::Api>>, body: Bytes) -> Response {
    let req: ChatRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return ApiError::invalid(e.to_string()).into_response(),
    };
    let (provider, mut request) = match build(&api.agent, &req) {
        Ok(built) => built,
        Err(e) => return e.into_response(),
    };
    let meta = Meta {
        id: format!("chatcmpl-{:016x}", getrandom::u64().unwrap_or_default()),
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        model: req.model,
    };
    api.restore_thinking(&mut request).await;
    let events = provider.stream(request, &|_| {});
    if req.stream {
        let usage = req.stream_options.is_some_and(|o| o.include_usage);
        return stream(api, events, meta, usage);
    }
    complete(&api, events, meta).await
}

struct Meta {
    id: String,
    created: u64,
    model: String,
}

fn finish_reason(stop: Option<StopReason>) -> &'static str {
    match stop {
        Some(StopReason::MaxTokens) => "length",
        Some(StopReason::Refusal) => "content_filter",
        Some(StopReason::ToolUse) => "tool_calls",
        _ => "stop",
    }
}

fn tool_call_json(index: usize, c: &Content) -> Option<Value> {
    let Content::ToolUse { id, name, input } = c else {
        return None;
    };
    Some(json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": input.to_string()},
    }))
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "prompt_tokens": u.prompt(),
        "completion_tokens": u.output,
        "total_tokens": u.prompt() + u.output,
        // `cache_write_tokens` is not OpenAI's, OpenRouter uses the same name.
        "prompt_tokens_details": {
            "cached_tokens": u.cache_read,
            "cache_write_tokens": u.cache_write,
        },
    })
}

async fn complete(
    api: &super::Api,
    mut events: futures::stream::BoxStream<'static, anyhow::Result<Event>>,
    meta: Meta,
) -> Response {
    let mut reply = super::Reply::default();
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => reply.add(event),
            Err(e) => {
                return ApiError(StatusCode::BAD_GATEWAY, "api_error", format!("{e:#}"))
                    .into_response();
            }
        }
    }
    api.save_thinking(&reply).await;
    let calls: Vec<Value> = reply
        .blocks
        .iter()
        .filter(|c| matches!(c, Content::ToolUse { .. }))
        .enumerate()
        .filter_map(|(i, c)| tool_call_json(i, c))
        .collect();
    let mut message = json!({
        "role": "assistant",
        "content": if reply.text.is_empty() && !calls.is_empty() { Value::Null } else { json!(reply.text) },
    });
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    if !reply.thinking.is_empty() {
        message["reasoning"] = json!(reply.thinking);
    }
    let details: Vec<Value> = reply.blocks.iter().filter_map(reasoning_json).collect();
    if !details.is_empty() {
        message["reasoning_details"] = json!(details);
    }
    json_response(
        StatusCode::OK,
        &json!({
            "id": meta.id,
            "object": "chat.completion",
            "created": meta.created,
            "model": meta.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason(reply.stop),
            }],
            "usage": usage_json(&reply.usage),
        }),
    )
}

/// Server-sent `chat.completion.chunk`s, then `[DONE]`. A dropped client
/// drops the provider stream, which cancels the request.
fn stream(
    api: Arc<super::Api>,
    mut events: futures::stream::BoxStream<'static, anyhow::Result<Event>>,
    meta: Meta,
    include_usage: bool,
) -> Response {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        let chunk = |delta: Value, finish: Option<&str>, usage: Option<Value>| {
            let mut c = json!({
                "id": meta.id,
                "object": "chat.completion.chunk",
                "created": meta.created,
                "model": meta.model,
                "choices": if usage.is_some() {
                    json!([])
                } else {
                    json!([{"index": 0, "delta": delta, "finish_reason": finish}])
                },
            });
            if let Some(u) = usage {
                c["usage"] = u;
            }
            format!("data: {c}\n\n")
        };
        let send = |line: String| tx.send(line);
        if send(chunk(
            json!({"role": "assistant", "content": ""}),
            None,
            None,
        ))
        .await
        .is_err()
        {
            return;
        }
        let mut reply = super::Reply::default();
        let mut calls = 0;
        while let Some(event) = events.next().await {
            let delta = match &event {
                Ok(Event::TextDelta(t)) => Some(json!({"content": t})),
                Ok(Event::ThinkingDelta(t)) => Some(json!({"reasoning": t})),
                Ok(Event::Block(c)) => {
                    if let Some(call) = tool_call_json(calls, c) {
                        calls += 1;
                        Some(json!({"tool_calls": [call]}))
                    } else {
                        reasoning_json(c).map(|r| json!({"reasoning_details": [r]}))
                    }
                }
                Ok(_) => None,
                Err(e) => {
                    let error =
                        json!({"error": {"message": format!("{e:#}"), "type": "api_error"}});
                    let _ = send(format!("data: {error}\n\n")).await;
                    return;
                }
            };
            reply.add(event.expect("errors returned above"));
            if let Some(delta) = delta
                && send(chunk(delta, None, None)).await.is_err()
            {
                return;
            }
        }
        api.save_thinking(&reply).await;
        let mut tail = vec![chunk(json!({}), Some(finish_reason(reply.stop)), None)];
        if include_usage {
            tail.push(chunk(Value::Null, None, Some(usage_json(&reply.usage))));
        }
        tail.push("data: [DONE]\n\n".into());
        for line in tail {
            if send(line).await.is_err() {
                return;
            }
        }
    });
    let body =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(models: &[&str]) -> Agent {
        struct Nothing;
        impl Provider for Nothing {
            fn stream(
                &self,
                _: Request,
                _: &dyn Fn(&mut Value),
            ) -> futures::stream::BoxStream<'static, anyhow::Result<Event>> {
                futures::stream::empty().boxed()
            }
        }
        let mut agent = Agent::new(Arc::new(Nothing), crate::provider::ModelSpec::new("p", "m"));
        agent
            .registry
            .register(crate::provider::ProviderName::new("p"), Arc::new(Nothing));
        agent.api = serde_json::from_value(json!({"enabled": true, "models": models})).unwrap();
        agent
    }

    fn request(body: Value) -> ChatRequest {
        serde_json::from_value(body).unwrap()
    }

    fn png() -> String {
        use base64::Engine;
        let mut out = vec![];
        image::RgbImage::new(4, 4)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    #[test]
    fn maps_messages_and_merges_roles() {
        let png = png();
        let req = request(json!({
            "model": "p/m",
            "max_tokens": 100,
            "reasoning_effort": "low",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "developer", "content": [{"type": "text", "text": "really"}]},
                {"role": "user", "content": "hi"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}},
                ]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "and?"},
            ],
        }));
        let (_, r) = build(&agent(&["p/*"]), &req).unwrap();
        assert_eq!(r.model.as_str(), "m");
        assert_eq!(r.system, "be brief\n\nreally");
        assert_eq!((r.max_tokens, r.thinking), (100, Thinking::Low));
        assert_eq!(
            r.messages,
            [
                Message {
                    role: Role::User,
                    content: vec![
                        Content::text("hi"),
                        Content::text("look"),
                        Content::image("image/png", png),
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Content::text("ok")]
                },
                Message {
                    role: Role::User,
                    content: vec![Content::text("and?")]
                },
            ]
        );
    }

    #[test]
    fn maps_tools_calls_results_and_reasoning() {
        let png = png();
        let req = request(json!({
            "model": "p/m",
            "tool_choice": "auto",
            "tools": [
                {"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "g"}},
            ],
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "sure",
                 "reasoning_details": [
                    {"type": "reasoning.text", "text": "hm", "signature": "sig"},
                    {"type": "reasoning.text", "text": "unsigned"},
                    {"type": "reasoning.encrypted", "data": "enc"},
                    {"type": "reasoning.summary", "summary": "s"},
                 ],
                 "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}},
                    {"id": "c2", "type": "function", "function": {"name": "g", "arguments": ""}},
                 ]},
                {"role": "tool", "tool_call_id": "c1", "content": "one"},
                {"role": "tool", "tool_call_id": "c2", "is_error": true, "content": [
                    {"type": "text", "text": "two"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}},
                ]},
                {"role": "user", "content": "next"},
            ],
        }));
        let (_, r) = build(&agent(&["p/*"]), &req).unwrap();
        assert_eq!(r.tools.len(), 2);
        assert_eq!(r.tools[0].input_schema, json!({"type": "object"}));
        assert_eq!(r.tools[1].input_schema["type"], "object");
        let result = |id: &str, text: &str, is_error, images| Content::ToolResult {
            tool_use_id: ToolCallId::new(id),
            content: text.into(),
            is_error,
            images,
            shown: vec![],
        };
        let Content::Image { source } = Content::image("image/png", png) else {
            unreachable!()
        };
        assert_eq!(
            r.messages[1..],
            [
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Thinking {
                            thinking: "hm".into(),
                            signature: "sig".into()
                        },
                        Content::RedactedThinking { data: "enc".into() },
                        Content::text("sure"),
                        Content::ToolUse {
                            id: ToolCallId::new("c1"),
                            name: ToolName::new("f"),
                            input: json!({"a": 1})
                        },
                        Content::ToolUse {
                            id: ToolCallId::new("c2"),
                            name: ToolName::new("g"),
                            input: json!({})
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        result("c1", "one", false, vec![]),
                        result("c2", "two", true, vec![source]),
                        Content::text("next"),
                    ],
                },
            ]
        );
    }

    #[test]
    fn rejects_what_it_cannot_do() {
        let user = json!({"role": "user", "content": "hi"});
        let err = |models: &[&str], body: Value| {
            let mut body = body;
            body["model"] = body.get("model").cloned().unwrap_or(json!("p/m"));
            let Err(e) = build(&agent(models), &request(body)) else {
                panic!("accepted");
            };
            (e.0.as_u16(), e.2)
        };
        assert_eq!(err(&["q/*"], json!({"messages": [user]})).0, 404);
        assert_eq!(err(&[], json!({"messages": [user]})).0, 404);
        assert_eq!(
            err(&["*"], json!({"model": "other/m", "messages": [user]})).0,
            404,
            "allowed but no such provider"
        );
        let all = &["*"][..];
        for (body, want) in [
            (
                json!({"messages": [user], "tool_choice": "required"}),
                "tool_choice",
            ),
            (json!({"messages": [user], "n": 2}), "n must be 1"),
            (
                json!({"messages": [{"role": "tool", "content": "x"}]}),
                "tool_call_id",
            ),
            (
                json!({"messages": [{"role": "function", "content": "x"}]}),
                "unsupported role",
            ),
            (
                json!({"messages": [user, {"role": "assistant", "tool_calls": [
                    {"id": "c", "type": "function", "function": {"name": "f", "arguments": "{"}}]}]}),
                "arguments",
            ),
            (
                json!({"messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "https://x/y.png"}}]}]}),
                "data: URLs",
            ),
            (
                json!({"messages": [{"role": "system", "content": "s"}]}),
                "no user",
            ),
            (
                json!({"messages": [user], "reasoning_effort": "huge"}),
                "thinking must be",
            ),
        ] {
            let (status, message) = err(all, body);
            assert_eq!(status, 400, "{message}");
            assert!(message.contains(want), "{message}");
        }
    }
}
