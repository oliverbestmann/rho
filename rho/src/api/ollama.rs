//! Ollama's `/api`: enough for clients that list models and chat, with
//! tools, thinking and images. Answers stream as NDJSON unless
//! `"stream": false`. Tool calls carry ids; for clients that drop them,
//! tool results match the open calls in order (by `tool_name` if given).
//! Thinking blocks never reach the client (Ollama's thinking is plain text);
//! the thinking store puts them back.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::provider::{
    Content, Event, Message, Provider, Request, Role, StopReason, Thinking, ToolName, ToolSpec,
};

/// Reported as Ollama's version; clients check it for features.
const VERSION: &str = "0.40.0";

pub(super) fn router() -> Router<Arc<super::Api>> {
    Router::new()
        .route(
            "/api/version",
            get(async || json_ok(json!({"version": VERSION}))),
        )
        .route("/api/tags", get(tags))
        .route("/api/ps", get(async || json_ok(json!({"models": []}))))
        .route("/api/show", post(show))
        .route("/api/chat", post(chat))
        .route("/api/generate", post(generate))
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": self.1}).to_string();
        (self.0, [(header::CONTENT_TYPE, "application/json")], body).into_response()
    }
}

fn json_ok(body: Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn now() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    crate::acp::rfc3339(ms)
}

fn details() -> Value {
    json!({
        "parent_model": "",
        "format": "api",
        "family": "rho",
        "families": ["rho"],
        "parameter_size": "",
        "quantization_level": "",
    })
}

async fn tags(State(api): State<Arc<super::Api>>) -> Response {
    let models: Vec<Value> = super::offered(&api.agent)
        .into_iter()
        .map(|(spec, _)| {
            json!({
                "name": spec.to_string(),
                "model": spec.to_string(),
                "modified_at": crate::acp::rfc3339(0),
                "size": 0,
                "digest": "",
                "details": details(),
            })
        })
        .collect();
    json_ok(json!({"models": models}))
}

#[derive(Deserialize)]
struct ShowRequest {
    #[serde(alias = "name")]
    model: String,
}

async fn show(State(api): State<Arc<super::Api>>, body: Bytes) -> Response {
    let req: ShowRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return invalid(e.to_string()).into_response(),
    };
    let (spec, provider) = match resolve(&api.agent, &req.model) {
        Ok(found) => found,
        Err(e) => return e.into_response(),
    };
    json_ok(json!({
        "modelfile": "",
        "parameters": "",
        "template": "",
        "details": details(),
        "model_info": {
            "general.architecture": "rho",
            "rho.context_length": provider.context_window(spec.id()),
        },
        "capabilities": ["completion", "tools", "vision", "thinking"],
        "modified_at": crate::acp::rfc3339(0),
    }))
}

/// Model names may carry Ollama's default tag.
fn resolve(
    agent: &Agent,
    name: &str,
) -> Result<(crate::provider::ModelSpec, Arc<dyn Provider>), ApiError> {
    let name = name.strip_suffix(":latest").unwrap_or(name);
    super::resolve(agent, name)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("model {name:?} not found")))
}

#[derive(Deserialize)]
struct ChatRequest {
    model: String,
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    tools: Vec<Tool>,
    stream: Option<bool>,
    think: Option<Value>,
    options: Option<Options>,
}

#[derive(Deserialize)]
struct Options {
    num_predict: Option<i64>,
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
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    images: Vec<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct ToolCall {
    id: Option<String>,
    function: FunctionCall,
}

#[derive(Deserialize)]
struct FunctionCall {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// `think`: `true` is rho's default effort, a string an effort name.
fn thinking(think: Option<&Value>) -> Result<Thinking, ApiError> {
    match think {
        None | Some(Value::Null | Value::Bool(false)) => Ok(Thinking::Off),
        Some(Value::Bool(true)) => Ok(Thinking::default()),
        Some(Value::String(s)) => s.parse().map_err(|e: anyhow::Error| invalid(e.to_string())),
        Some(v) => Err(invalid(format!("think must be a bool or effort, got {v}"))),
    }
}

fn max_tokens(agent: &Agent, options: Option<&Options>) -> u32 {
    options
        .and_then(|o| o.num_predict)
        .filter(|n| *n > 0)
        .map_or(agent.max_tokens, |n| n.min(u32::MAX.into()) as u32)
}

/// Ollama sends images as bare base64; the type comes from the bytes.
fn image(data: &str) -> Result<Content, ApiError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| invalid("images must be base64"))?;
    let format = image::guess_format(&bytes).map_err(|_| invalid("unknown image type"))?;
    super::image(format.to_mime_type(), data).map_err(|e| invalid(format!("{e:#}")))
}

fn contents(text: &str, images: &[String]) -> Result<Vec<Content>, ApiError> {
    let text = (!text.is_empty()).then(|| Ok(Content::text(text)));
    text.into_iter()
        .chain(images.iter().map(|i| image(i)))
        .collect()
}

fn build_chat(agent: &Agent, req: &ChatRequest) -> Result<Request, ApiError> {
    let mut system = vec![];
    let mut messages: Vec<Message> = vec![];
    // Calls of the last assistant message without a result yet: id, name.
    let mut open: Vec<(String, String)> = vec![];
    for (i, m) in req.messages.iter().enumerate() {
        let content = contents(&m.content, &m.images)?;
        match m.role.as_str() {
            "system" if m.images.is_empty() => system.push(m.content.clone()),
            "user" => super::push(&mut messages, Role::User, content),
            "tool" => {
                let id = match &m.tool_call_id {
                    Some(id) => {
                        open.retain(|(open, _)| open != id);
                        id.clone()
                    }
                    None => {
                        let name = m.tool_name.as_deref();
                        let pos = open
                            .iter()
                            .position(|(_, n)| name.is_none_or(|name| name == n))
                            .ok_or_else(|| invalid("tool message without an open tool call"))?;
                        open.remove(pos).0
                    }
                };
                let result = super::tool_result(&id, content, false);
                super::push(&mut messages, Role::User, vec![result]);
            }
            "assistant" if m.images.is_empty() => {
                // Thinking is ignored: Ollama's is unsigned, the store has it.
                let mut blocks = content;
                open.clear();
                for (n, call) in m.tool_calls.iter().enumerate() {
                    let id = call.id.clone().unwrap_or_else(|| format!("call_{i}_{n}"));
                    let input = match &call.function.arguments {
                        Value::Null => json!({}),
                        Value::String(s) => serde_json::from_str(s)
                            .map_err(|e| invalid(format!("tool call arguments: {e}")))?,
                        args => args.clone(),
                    };
                    open.push((id.clone(), call.function.name.clone()));
                    blocks.push(Content::ToolUse {
                        id: crate::provider::ToolCallId::new(id),
                        name: ToolName::new(&call.function.name),
                        input,
                    });
                }
                super::push(&mut messages, Role::Assistant, blocks);
            }
            "system" | "assistant" => {
                return Err(invalid(format!("{} messages take no images", m.role)));
            }
            role => return Err(invalid(format!("unsupported role {role:?}"))),
        }
    }
    Ok(Request {
        model: crate::provider::ModelId::new(""),
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
        max_tokens: max_tokens(agent, req.options.as_ref()),
        thinking: thinking(req.think.as_ref())?,
    })
}

async fn chat(State(api): State<Arc<super::Api>>, body: Bytes) -> Response {
    let req: ChatRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return invalid(e.to_string()).into_response(),
    };
    let built = resolve(&api.agent, &req.model).and_then(|(spec, provider)| {
        let mut request = build_chat(&api.agent, &req)?;
        request.model = spec.id().clone();
        Ok((provider, request))
    });
    match built {
        Ok((provider, request)) => {
            let shape = Shape::Chat;
            respond(api, provider, request, req.model, req.stream, shape).await
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct GenerateRequest {
    model: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    system: String,
    #[serde(default)]
    images: Vec<String>,
    stream: Option<bool>,
    think: Option<Value>,
    options: Option<Options>,
    #[serde(default)]
    suffix: String,
    #[serde(default)]
    raw: bool,
    #[serde(default)]
    template: String,
    #[serde(default)]
    context: Vec<Value>,
}

async fn generate(State(api): State<Arc<super::Api>>, body: Bytes) -> Response {
    let req: GenerateRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return invalid(e.to_string()).into_response(),
    };
    let built = (|| {
        if !req.suffix.is_empty() || req.raw || !req.template.is_empty() || !req.context.is_empty()
        {
            return Err(invalid(
                "suffix, raw, template and context are not supported",
            ));
        }
        let (spec, provider) = resolve(&api.agent, &req.model)?;
        let content = contents(&req.prompt, &req.images)?;
        let request = Request {
            model: spec.id().clone(),
            system: req.system.clone(),
            messages: (!content.is_empty())
                .then_some(Message {
                    role: Role::User,
                    content,
                })
                .into_iter()
                .collect(),
            tools: vec![],
            max_tokens: max_tokens(&api.agent, req.options.as_ref()),
            thinking: thinking(req.think.as_ref())?,
        };
        Ok((provider, request))
    })();
    match built {
        Ok((provider, request)) => {
            let shape = Shape::Generate;
            respond(api, provider, request, req.model, req.stream, shape).await
        }
        Err(e) => e.into_response(),
    }
}

/// `/api/chat` answers in `message`, `/api/generate` in `response`.
#[derive(Clone, Copy)]
enum Shape {
    Chat,
    Generate,
}

impl Shape {
    /// The answer part of a response object.
    fn body(self, model: &str, text: &str, thinking: &str, calls: Vec<Value>) -> Value {
        let mut out = json!({"model": model, "created_at": now()});
        let target = match self {
            Shape::Chat => {
                out["message"] = json!({"role": "assistant", "content": text});
                &mut out["message"]
            }
            Shape::Generate => {
                out["response"] = json!(text);
                &mut out
            }
        };
        if !thinking.is_empty() {
            target["thinking"] = json!(thinking);
        }
        if !calls.is_empty() {
            target["tool_calls"] = json!(calls);
        }
        out
    }
}

fn tool_call_json(index: usize, c: &Content) -> Option<Value> {
    let Content::ToolUse { id, name, input } = c else {
        return None;
    };
    Some(json!({"id": id, "function": {"index": index, "name": name, "arguments": input}}))
}

/// The fields of the last response object.
fn done(out: &mut Value, reply: &super::Reply, started: Instant) {
    let u = &reply.usage;
    for (key, value) in [
        ("done", json!(true)),
        (
            "done_reason",
            json!(match reply.stop {
                Some(StopReason::MaxTokens) => "length",
                _ => "stop",
            }),
        ),
        ("total_duration", json!(started.elapsed().as_nanos() as u64)),
        ("prompt_eval_count", json!(u.prompt())),
        ("prompt_eval_cached_count", json!(u.cache_read)),
        ("eval_count", json!(u.output)),
    ] {
        out[key] = value;
    }
}

async fn respond(
    api: Arc<super::Api>,
    provider: Arc<dyn Provider>,
    mut request: Request,
    model: String,
    stream: Option<bool>,
    shape: Shape,
) -> Response {
    // Ollama loads the model then; there is nothing to load.
    if request.messages.is_empty() {
        let mut out = shape.body(&model, "", "", vec![]);
        out["done"] = json!(true);
        out["done_reason"] = json!("load");
        return json_ok(out);
    }
    let started = Instant::now();
    api.restore_thinking(&mut request).await;
    let events = provider.stream(request, &|_| {});
    if stream.unwrap_or(true) {
        return ndjson(api, events, model, shape, started);
    }
    let mut events = events;
    let mut reply = super::Reply::default();
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => reply.add(event),
            Err(e) => return ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
        }
    }
    api.save_thinking(&reply).await;
    let calls = reply
        .blocks
        .iter()
        .filter(|c| matches!(c, Content::ToolUse { .. }))
        .enumerate()
        .filter_map(|(i, c)| tool_call_json(i, c))
        .collect();
    let mut out = shape.body(&model, &reply.text, &reply.thinking, calls);
    done(&mut out, &reply, started);
    json_ok(out)
}

/// One JSON object per line; errors as `{"error": ...}`. A dropped client
/// drops the provider stream, which cancels the request.
fn ndjson(
    api: Arc<super::Api>,
    mut events: BoxStream<'static, anyhow::Result<Event>>,
    model: String,
    shape: Shape,
    started: Instant,
) -> Response {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        let mut reply = super::Reply::default();
        let mut calls = 0;
        while let Some(event) = events.next().await {
            let line = match &event {
                Ok(Event::TextDelta(t)) => Some(shape.body(&model, t, "", vec![])),
                Ok(Event::ThinkingDelta(t)) => Some(shape.body(&model, "", t, vec![])),
                Ok(Event::Block(c)) => tool_call_json(calls, c).map(|call| {
                    calls += 1;
                    shape.body(&model, "", "", vec![call])
                }),
                Ok(_) => None,
                Err(e) => {
                    let _ = tx
                        .send(format!("{}\n", json!({"error": format!("{e:#}")})))
                        .await;
                    return;
                }
            };
            reply.add(event.expect("errors returned above"));
            if let Some(mut line) = line {
                line["done"] = json!(false);
                if tx.send(format!("{line}\n")).await.is_err() {
                    return;
                }
            }
        }
        api.save_thinking(&reply).await;
        let mut out = shape.body(&model, "", "", vec![]);
        done(&mut out, &reply, started);
        let _ = tx.send(format!("{out}\n")).await;
    });
    let body =
        futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, std::convert::Infallible>);
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: Value) -> ChatRequest {
        serde_json::from_value(body).unwrap()
    }

    fn agent() -> Agent {
        struct Nothing;
        impl Provider for Nothing {
            fn stream(
                &self,
                _: Request,
                _: &dyn Fn(&mut Value),
            ) -> BoxStream<'static, anyhow::Result<Event>> {
                futures::stream::empty().boxed()
            }
        }
        Agent::new(Arc::new(Nothing), crate::provider::ModelSpec::new("p", "m"))
    }

    #[test]
    fn tool_results_without_ids_match_open_calls() {
        let req = request(json!({"model": "p/m", "think": "low", "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "thinking": "unsigned", "tool_calls": [
                {"function": {"name": "a", "arguments": {"x": 1}}},
                {"function": {"name": "b", "arguments": {}}},
            ]},
            {"role": "tool", "tool_name": "b", "content": "B"},
            {"role": "tool", "content": "A"},
        ]}));
        let built = build_chat(&agent(), &req).unwrap();
        assert_eq!(built.thinking, Thinking::Low);
        let ids = |m: &Message| -> Vec<String> {
            m.content
                .iter()
                .map(|c| match c {
                    Content::ToolUse { id, .. } => id.to_string(),
                    Content::ToolResult { tool_use_id, .. } => tool_use_id.to_string(),
                    _ => panic!("unexpected {c:?}"),
                })
                .collect()
        };
        assert_eq!(ids(&built.messages[1]), ["call_1_0", "call_1_1"]);
        assert_eq!(ids(&built.messages[2]), ["call_1_1", "call_1_0"]);

        let stray = request(json!({"model": "p/m", "messages": [
            {"role": "tool", "content": "?"},
        ]}));
        assert!(build_chat(&agent(), &stray).is_err());
    }

    #[test]
    fn think_values() {
        assert_eq!(thinking(None).unwrap(), Thinking::Off);
        assert_eq!(thinking(Some(&json!(true))).unwrap(), Thinking::High);
        assert_eq!(thinking(Some(&json!("max"))).unwrap(), Thinking::Max);
        assert!(thinking(Some(&json!(3))).is_err());
    }
}
