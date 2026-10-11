//! OpenAI Responses API: request body and stream parser, following pi-ai's
//! `openai-responses`. Used by Copilot's GPT models.
//!
//! Requests are stateless (`store: false`). Reasoning comes back as an
//! encrypted item, which is kept as a `Content::Thinking` whose signature is
//! the item's JSON, and sent back unchanged in later requests.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::provider::{Content, Event, ImageSource, Request, Role, StopReason, Usage};

/// A thinking block holding an OpenAI reasoning item. Anthropic signatures
/// are base64, so they never start with `{`. Each API skips the other's.
pub fn is_reasoning(c: &Content) -> bool {
    matches!(c, Content::Thinking { signature, .. } if signature.starts_with('{'))
}

/// Body of `POST /responses`.
pub fn build_body(request: &Request) -> Value {
    let mut input = Vec::new();
    if !request.system.is_empty() {
        input.push(json!({"role": "developer", "content": request.system}));
    }
    for m in &request.messages {
        let mut parts = Vec::new();
        for c in &m.content {
            match (m.role, c) {
                (Role::User, Content::Text { text }) => {
                    parts.push(json!({"type": "input_text", "text": text}))
                }
                (Role::User, Content::Image { source }) => parts.push(image(source)),
                // Outputs go right after their calls, before any user text.
                (
                    _,
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        images,
                        ..
                    },
                ) => {
                    let output = if images.is_empty() {
                        json!(if content.is_empty() {
                            "(no tool output)"
                        } else {
                            content
                        })
                    } else {
                        let text = (!content.is_empty())
                            .then(|| json!({"type": "input_text", "text": content}));
                        Value::Array(text.into_iter().chain(images.iter().map(image)).collect())
                    };
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": output,
                    }));
                }
                (Role::Assistant, Content::Text { text }) => {
                    input.push(json!({"role": "assistant", "content": text}))
                }
                (
                    Role::Assistant,
                    Content::ToolUse {
                        id,
                        name,
                        input: args,
                    },
                ) => input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": args.to_string(),
                })),
                (Role::Assistant, Content::Thinking { signature, .. }) if is_reasoning(c) => {
                    if let Ok(item) = serde_json::from_str::<Value>(signature) {
                        input.push(item);
                    }
                }
                // Anthropic thinking, and blocks in the wrong role.
                _ => {}
            }
        }
        if !parts.is_empty() {
            input.push(json!({"role": "user", "content": parts}));
        }
    }

    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
        "store": false,
        "max_output_tokens": request.max_tokens,
    });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                })
            })
            .collect();
    }
    // shortcut: effort names are sent as they are; models without xhigh/max reject them, clamp per model if that bites.
    if request.thinking != crate::provider::Thinking::Off {
        body["reasoning"] = json!({"effort": request.thinking.as_str(), "summary": "auto"});
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    body
}

fn image(source: &ImageSource) -> Value {
    let ImageSource::Base64 { media_type, data } = source;
    json!({
        "type": "input_image",
        "detail": "auto",
        "image_url": format!("data:{media_type};base64,{data}"),
    })
}

/// Incremental parser for the Responses API server-sent events.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    tool_use: bool,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<Event>> {
        let mut out = Vec::new();
        for data in crate::anthropic::sse_data(&mut self.buf, bytes) {
            if let Err(e) = self.handle(&data, &mut out) {
                out.push(Err(e));
            }
        }
        out
    }

    fn handle(&mut self, data: &str, out: &mut Vec<Result<Event>>) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let ev: Value = serde_json::from_str(data)?;
        let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
        match ev["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" | "response.refusal.delta" => {
                out.push(Ok(Event::TextDelta(text(&ev["delta"]))))
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                out.push(Ok(Event::ThinkingDelta(text(&ev["delta"]))))
            }
            "response.reasoning_summary_part.done" => {
                out.push(Ok(Event::ThinkingDelta("\n\n".into())))
            }
            "response.output_item.done" => {
                let item = &ev["item"];
                match item["type"].as_str().unwrap_or_default() {
                    // Without encrypted content the item cannot be sent back.
                    "reasoning" if item["encrypted_content"].is_string() => {
                        let summary: Vec<_> = item["summary"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|s| text(&s["text"]))
                            .collect();
                        out.push(Ok(Event::Block(Content::Thinking {
                            thinking: summary.join("\n\n"),
                            signature: item.to_string(),
                        })));
                    }
                    "function_call" => {
                        self.tool_use = true;
                        let args = item["arguments"].as_str().filter(|a| !a.trim().is_empty());
                        out.push(Ok(Event::Block(Content::ToolUse {
                            id: text(&item["call_id"]),
                            name: text(&item["name"]),
                            input: serde_json::from_str(args.unwrap_or("{}"))?,
                        })));
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = &ev["response"];
                let usage = &response["usage"];
                let n = |v: &Value| v.as_u64().unwrap_or(0);
                let cached = n(&usage["input_tokens_details"]["cached_tokens"]);
                out.push(Ok(Event::Usage(Usage {
                    // input_tokens includes the cached ones.
                    input: n(&usage["input_tokens"]).saturating_sub(cached),
                    output: n(&usage["output_tokens"]),
                    cache_read: cached,
                    cache_write: 0,
                })));
                let stop = match response["incomplete_details"]["reason"].as_str() {
                    None if self.tool_use => StopReason::ToolUse,
                    None => StopReason::EndTurn,
                    Some("max_output_tokens") => StopReason::MaxTokens,
                    Some("content_filter") => StopReason::Refusal,
                    Some(reason) => bail!("openai: response incomplete: {reason}"),
                };
                out.push(Ok(Event::Done(stop)));
            }
            "response.failed" => bail!("openai: response failed: {}", ev["response"]["error"]),
            "error" => bail!("openai stream error: {}: {}", ev["code"], ev["message"]),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Thinking, ToolSpec};

    #[test]
    fn body_replays_reasoning_calls_and_results() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "x"});
        let request = Request {
            model: "gpt-5.5".into(),
            system: "sys".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::text("hi")],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Thinking {
                            thinking: "claude".into(),
                            signature: "c2ln".into(),
                        },
                        Content::Thinking {
                            thinking: "gpt".into(),
                            signature: reasoning.to_string(),
                        },
                        Content::ToolUse {
                            id: "call_1".into(),
                            name: "Read".into(),
                            input: json!({"path": "a"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        Content::ToolResult {
                            tool_use_id: "call_1".into(),
                            content: String::new(),
                            is_error: false,
                            images: vec![],
                        },
                        Content::text("note"),
                    ],
                },
            ],
            tools: vec![ToolSpec {
                name: "Read".into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 100,
            thinking: Thinking::High,
        };
        let body = build_body(&request);
        assert_eq!(
            body["input"],
            json!([
                {"role": "developer", "content": "sys"},
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                reasoning,
                {"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "(no tool output)"},
                {"role": "user", "content": [{"type": "input_text", "text": "note"}]},
            ])
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["parameters"], json!({"type": "object"}));
    }

    #[test]
    fn parses_a_tool_call_response() {
        let events = [
            json!({"type": "response.reasoning_summary_text.delta", "delta": "hm"}),
            json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "x", "summary": [{"type": "summary_text", "text": "hm"}]}}),
            json!({"type": "response.output_text.delta", "delta": "ok"}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{\"path\":\"a\"}"}}),
            json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": 10, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 4}}}}),
        ];
        let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let mut parser = SseParser::default();
        // Split mid-line to check buffering.
        let (a, b) = sse.as_bytes().split_at(25);
        let out: Vec<Event> = [parser.feed(a), parser.feed(b)]
            .into_iter()
            .flatten()
            .map(Result::unwrap)
            .collect();
        assert_eq!(out[0], Event::ThinkingDelta("hm".into()));
        assert!(matches!(&out[1], Event::Block(c) if is_reasoning(c)));
        assert_eq!(out[2], Event::TextDelta("ok".into()));
        assert_eq!(
            out[3],
            Event::Block(Content::ToolUse {
                id: "call_1".into(),
                name: "Read".into(),
                input: json!({"path": "a"}),
            })
        );
        assert_eq!(
            out[4],
            Event::Usage(Usage {
                input: 6,
                output: 3,
                cache_read: 4,
                cache_write: 0,
            })
        );
        assert_eq!(out[5], Event::Done(StopReason::ToolUse));
    }
}
