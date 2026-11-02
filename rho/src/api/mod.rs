//! Model APIs on the daemon's listeners, so other clients can use rho's
//! providers: OpenAI's `/v1` (`openai`). Off unless the config enables it:
//!
//! ```json
//! { "api": { "enabled": true, "models": ["anthropic/*"] } }
//! ```
//!
//! Requests go straight to the provider: no session, no rho tools or system
//! prompt, no extension hooks; the client brings its own tools. The TCP listener needs `token` as API key, else the acp-token
//! (see `acp_ws`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;

use crate::agent::Agent;
use crate::models::Pattern;
use crate::provider::{
    Content, Event, ImageSource, Message, ModelSpec, Provider, Request, Role, StopReason,
    ToolCallId, Usage,
};
use crate::store::Store;

mod openai;

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

/// The API routes, for any router state.
pub fn router<S: Clone + Send + Sync + 'static>(agent: Arc<Agent>) -> Router<S> {
    openai::router()
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(Arc::new(Api {
            agent,
            store: OnceCell::new(),
        }))
}

/// Saved thinking blocks older than this are deleted.
const THINKING_DAYS: i64 = 90;

/// The routes' state.
struct Api {
    agent: Arc<Agent>,
    /// For thinking blocks; opened on first use, `None` without sessions db.
    store: OnceCell<Option<Store>>,
}

impl Api {
    async fn store(&self) -> Option<&Store> {
        self.store
            .get_or_init(async || {
                // The API has no cwd; the daemon uses one database for all.
                let store = match self.agent.open_store(Path::new("/")).await {
                    Ok(store) => store?,
                    Err(e) => {
                        eprintln!("rho api: no thinking store: {e:#}");
                        return None;
                    }
                };
                let pruned = store.clone();
                tokio::spawn(async move {
                    loop {
                        if let Err(e) = pruned.prune_api_thinking(THINKING_DAYS).await {
                            eprintln!("rho api: pruning thinking: {e:#}");
                        }
                        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
                    }
                });
                Some(store)
            })
            .await
            .as_ref()
    }

    /// Puts saved thinking blocks back into assistant messages with tool
    /// calls that came without them; clients rarely send them. All of them,
    /// so requests keep a stable prefix for the prompt cache. Without saved
    /// blocks a message stays as it is: with adaptive thinking that is valid.
    async fn restore_thinking(&self, request: &mut Request) {
        let missing: Vec<(usize, String)> = request
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Assistant && !m.content.iter().any(is_thinking))
            .filter_map(|(i, m)| Some((i, thinking_key(&m.content)?)))
            .collect();
        if missing.is_empty() {
            return;
        }
        let Some(store) = self.store().await else {
            return;
        };
        let keys = missing.iter().map(|(_, k)| k.clone()).collect();
        let found = match store.api_thinking(keys).await {
            Ok(found) => found,
            Err(e) => {
                eprintln!("rho api: reading thinking: {e:#}");
                return;
            }
        };
        for ((i, _), blocks) in missing.into_iter().zip(found) {
            if let Some(blocks) = blocks {
                request.messages[i].content.splice(0..0, blocks);
            }
        }
    }

    /// Saves the thinking blocks of a reply with tool calls.
    async fn save_thinking(&self, reply: &Reply) {
        let Some(key) = thinking_key(&reply.blocks) else {
            return;
        };
        let blocks: Vec<Content> = reply
            .blocks
            .iter()
            .filter(|c| is_thinking(c))
            .cloned()
            .collect();
        if blocks.is_empty() {
            return;
        }
        let Some(store) = self.store().await else {
            return;
        };
        if let Err(e) = store.save_api_thinking(key, blocks).await {
            eprintln!("rho api: saving thinking: {e:#}");
        }
    }
}

fn is_thinking(c: &Content) -> bool {
    matches!(
        c,
        Content::Thinking { .. } | Content::RedactedThinking { .. }
    )
}

/// Identifies a response by its tool calls (name and input, keys sorted):
/// clients send those back, ids not always (Ollama). `None` without calls.
fn thinking_key(content: &[Content]) -> Option<String> {
    let calls: Vec<Value> = content
        .iter()
        .filter_map(|c| match c {
            Content::ToolUse { name, input, .. } => Some(json!([name, sorted(input)])),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return None;
    }
    let hash = Sha256::digest(Value::Array(calls).to_string());
    Some(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// `value` with all object keys sorted (serde_json keeps insertion order here).
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sorted(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        v => v.clone(),
    }
}

/// The offered model `name` and its provider; `None` if not offered.
fn resolve(agent: &Agent, name: &str) -> Option<(ModelSpec, Arc<dyn Provider>)> {
    let spec: ModelSpec = name.parse().ok()?;
    if !agent.api.allows(&spec) {
        return None;
    }
    let provider = agent.registry.resolve(&spec).ok()?;
    Some((spec, provider))
}

/// Appends `content` as a `role` message; Anthropic wants alternating roles,
/// so it joins the last message if that has the same role.
fn push(messages: &mut Vec<Message>, role: Role, content: Vec<Content>) {
    if content.is_empty() {
        return;
    }
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.extend(content),
        _ => messages.push(Message { role, content }),
    }
}

/// A tool result from text and image parts.
fn tool_result(id: &str, content: Vec<Content>, is_error: bool) -> Content {
    let (mut text, mut images) = (vec![], vec![]);
    for c in content {
        match c {
            Content::Text { text: t } => text.push(t),
            Content::Image { source } => images.push(source),
            _ => {}
        }
    }
    Content::ToolResult {
        tool_use_id: ToolCallId::new(id),
        content: text.join("\n"),
        is_error,
        images,
        shown: vec![],
    }
}

/// A response collected from the provider's events.
#[derive(Default)]
struct Reply {
    text: String,
    /// Reasoning text, for display.
    thinking: String,
    /// Thinking blocks and tool calls, in order.
    blocks: Vec<Content>,
    /// The last report; each replaces the one before.
    usage: Usage,
    stop: Option<StopReason>,
}

impl Reply {
    fn add(&mut self, event: Event) {
        match event {
            Event::TextDelta(t) => self.text.push_str(&t),
            Event::ThinkingDelta(t) => self.thinking.push_str(&t),
            Event::Block(c) => self.blocks.push(c),
            Event::Usage(u) => self.usage = u,
            Event::Done(stop) => self.stop = Some(stop),
        }
    }
}

/// A base64 image, fit for the models.
fn image(media_type: &str, data: &str) -> anyhow::Result<Content> {
    let mut source = ImageSource::Base64 {
        media_type: media_type.into(),
        data: data.into(),
    };
    source.fit()?;
    Ok(Content::Image { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, input: Value) -> Content {
        Content::ToolUse {
            id: ToolCallId::new(id),
            name: crate::provider::ToolName::new("f"),
            input,
        }
    }

    #[test]
    fn thinking_key_ignores_ids_and_key_order() {
        let a = thinking_key(&[call("1", json!({"x": {"p": 1, "q": 2}, "y": 3}))]);
        let b = thinking_key(&[
            Content::text("t"),
            call("2", json!({"y": 3, "x": {"q": 2, "p": 1}})),
        ]);
        assert!(a.is_some());
        assert_eq!(a, b);
        assert_ne!(a, thinking_key(&[call("1", json!({"y": 4}))]));
        assert_eq!(thinking_key(&[Content::text("t")]), None);
    }

    #[tokio::test]
    async fn saved_thinking_is_found_and_pruned() {
        let store = Store::memory().unwrap();
        let blocks = vec![Content::RedactedThinking { data: "d".into() }];
        store
            .save_api_thinking("k".into(), blocks.clone())
            .await
            .unwrap();
        let found = store
            .api_thinking(vec!["k".into(), "x".into()])
            .await
            .unwrap();
        assert_eq!(found, [Some(blocks), None]);
        assert_eq!(store.prune_api_thinking(1).await.unwrap(), 0);
        assert_eq!(store.prune_api_thinking(-1).await.unwrap(), 1);
    }

    #[test]
    fn config_is_off_and_empty_by_default() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!config.enabled);
        assert!(!config.allows(&ModelSpec::new("p", "m")));
        assert_eq!(config.token, None);
        assert!(serde_json::from_value::<Config>(serde_json::json!({"enable": true})).is_err());
    }
}
