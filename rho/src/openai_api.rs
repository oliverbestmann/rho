//! OpenAI-compatible endpoints on the daemon's listeners, so other clients
//! can use rho's providers: `GET /v1/models` and `POST /v1/chat/completions`
//! (streaming or not), text and image (`data:` URL) input only. Off unless
//! the config enables it:
//!
//! ```json
//! { "api": { "enabled": true, "models": ["anthropic/*"] } }
//! ```
//!
//! Requests go straight to the provider: no session, no tools, no extension
//! hooks. The TCP listener needs `token` as API key, else the acp-token
//! (see `acp_ws`).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::models::Pattern;
use crate::provider::{
    Content, Event, ImageSource, Message, ModelSpec, Provider, Request, Role, StopReason, Thinking,
    Usage,
};

/// `"api"` in the config.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub enabled: bool,
    /// Models the endpoints offer; none if empty.
    #[serde(default)]
    pub models: Vec<Pattern>,
    /// API key on the TCP listener instead of the ACP token.
    #[serde(default)]
    pub token: Option<String>,
}

impl Config {
    pub const KEY: &str = "api";

    pub fn allows(&self, spec: &ModelSpec) -> bool {
        self.models.iter().any(|p| p.matches(spec))
    }
}

/// Requests carry images as base64; axum's default limit is 2 MB.
const BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The `/v1` routes, for any router state.
pub fn router<S: Clone + Send + Sync + 'static>(agent: Arc<Agent>) -> Router<S> {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(agent)
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

async fn models(State(agent): State<Arc<Agent>>) -> Response {
    let data: Vec<Value> = agent
        .registry
        .models()
        .into_iter()
        .filter(|(spec, _)| agent.api.allows(spec))
        .map(|(spec, _)| {
            json!({
                "id": spec.to_string(),
                "object": "model",
                "created": 0,
                "owned_by": spec.provider().as_str(),
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
    tools: Option<Vec<Value>>,
    n: Option<u32>,
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
    let spec: ModelSpec = req.model.parse().map_err(|_| not_found())?;
    if !agent.api.allows(&spec) {
        return Err(not_found());
    }
    let provider = agent.registry.resolve(&spec).map_err(|_| not_found())?;
    if req.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        return Err(ApiError::invalid("tools are not supported"));
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
        let role = match m.role.as_str() {
            "system" | "developer" => {
                for c in content {
                    let Content::Text { text } = c else {
                        return Err(ApiError::invalid("system messages take text only"));
                    };
                    system.push(text);
                }
                continue;
            }
            "user" => Role::User,
            "assistant" => Role::Assistant,
            role => return Err(ApiError::invalid(format!("unsupported role {role:?}"))),
        };
        if role == Role::Assistant && content.iter().any(|c| matches!(c, Content::Image { .. })) {
            return Err(ApiError::invalid("assistant messages take text only"));
        }
        if content.is_empty() {
            continue;
        }
        // Anthropic wants alternating roles.
        match messages.last_mut() {
            Some(last) if last.role == role => last.content.extend(content),
            _ => messages.push(Message { role, content }),
        }
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
            tools: vec![],
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
    let mut source = ImageSource::Base64 {
        media_type: media_type.into(),
        data: data.into(),
    };
    source
        .fit()
        .map_err(|e| ApiError::invalid(format!("{e:#}")))?;
    Ok(Content::Image { source })
}

async fn chat(State(agent): State<Arc<Agent>>, body: Bytes) -> Response {
    let req: ChatRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return ApiError::invalid(e.to_string()).into_response(),
    };
    let (provider, request) = match build(&agent, &req) {
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
    let events = provider.stream(request, &|_| {});
    if req.stream {
        let usage = req.stream_options.is_some_and(|o| o.include_usage);
        return stream(events, meta, usage);
    }
    complete(events, meta).await
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
        _ => "stop",
    }
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
    mut events: futures::stream::BoxStream<'static, anyhow::Result<Event>>,
    meta: Meta,
) -> Response {
    let (mut text, mut usage, mut stop) = (String::new(), Usage::default(), None);
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::TextDelta(t)) => text.push_str(&t),
            // Each report replaces the last.
            Ok(Event::Usage(u)) => usage = u,
            Ok(Event::Done(reason)) => stop = Some(reason),
            Ok(_) => {}
            Err(e) => {
                return ApiError(StatusCode::BAD_GATEWAY, "api_error", format!("{e:#}"))
                    .into_response();
            }
        }
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
                "message": {"role": "assistant", "content": text},
                "finish_reason": finish_reason(stop),
            }],
            "usage": usage_json(&usage),
        }),
    )
}

/// Server-sent `chat.completion.chunk`s, then `[DONE]`. A dropped client
/// drops the provider stream, which cancels the request.
fn stream(
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
        let (mut usage, mut stop) = (Usage::default(), None);
        while let Some(event) = events.next().await {
            let line = match event {
                Ok(Event::TextDelta(t)) => chunk(json!({"content": t}), None, None),
                Ok(Event::Usage(u)) => {
                    usage = u;
                    continue;
                }
                Ok(Event::Done(reason)) => {
                    stop = Some(reason);
                    continue;
                }
                Ok(_) => continue,
                Err(e) => {
                    let error =
                        json!({"error": {"message": format!("{e:#}"), "type": "api_error"}});
                    let _ = send(format!("data: {error}\n\n")).await;
                    return;
                }
            };
            if send(line).await.is_err() {
                return;
            }
        }
        let mut tail = vec![chunk(json!({}), Some(finish_reason(stop)), None)];
        if include_usage {
            tail.push(chunk(Value::Null, None, Some(usage_json(&usage))));
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
        let mut agent = Agent::new(Arc::new(Nothing), ModelSpec::new("p", "m"));
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
    fn config_is_off_and_empty_by_default() {
        let config: Config = serde_json::from_value(json!({})).unwrap();
        assert!(!config.enabled);
        assert!(!config.allows(&ModelSpec::new("p", "m")));
        assert_eq!(config.token, None);
        assert!(serde_json::from_value::<Config>(json!({"enable": true})).is_err());
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
                json!({"messages": [user], "tools": [{"type": "function"}]}),
                "tools",
            ),
            (json!({"messages": [user], "n": 2}), "n must be 1"),
            (
                json!({"messages": [{"role": "tool", "content": "x"}]}),
                "unsupported role",
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
