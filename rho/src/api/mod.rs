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

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use serde::Deserialize;

use crate::agent::Agent;
use crate::models::Pattern;
use crate::provider::{
    Content, Event, ImageSource, Message, ModelSpec, Provider, Role, StopReason, ToolCallId, Usage,
};

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
        .with_state(agent)
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

    #[test]
    fn config_is_off_and_empty_by_default() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!config.enabled);
        assert!(!config.allows(&ModelSpec::new("p", "m")));
        assert_eq!(config.token, None);
        assert!(serde_json::from_value::<Config>(serde_json::json!({"enable": true})).is_err());
    }
}
