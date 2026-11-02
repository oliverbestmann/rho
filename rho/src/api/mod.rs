//! Model APIs on the daemon's listeners, so other clients can use rho's
//! providers: OpenAI's `/v1` (`openai`). Off unless the config enables it:
//!
//! ```json
//! { "api": { "enabled": true, "models": ["anthropic/*"] } }
//! ```
//!
//! Requests go straight to the provider: no session, no tools, no extension
//! hooks. The TCP listener needs `token` as API key, else the acp-token
//! (see `acp_ws`).

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use serde::Deserialize;

use crate::agent::Agent;
use crate::models::Pattern;
use crate::provider::{Content, ImageSource, ModelSpec, Provider};

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
