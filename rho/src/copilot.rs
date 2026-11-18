//! GitHub Copilot provider for Claude and GPT models, following pi-ai's
//! `github-copilot` provider.
//!
//! Copilot serves Claude over the Anthropic Messages API (`anthropic`
//! module), Gemini over OpenAI Chat Completions (`openai_chat` module) and
//! GPT over the OpenAI Responses API (`openai` module); the model id picks
//! the API. Differences to the direct APIs:
//! - Auth: a GitHub OAuth token from the device flow (`rho login
//!   github-copilot`) is exchanged for a short-lived Copilot token, stored as
//!   `access` with the GitHub token as `refresh` (pi's `auth.json` layout).
//! - The API host comes from the token's `proxy-ep` field (individual,
//!   business and enterprise plans have different hosts).
//! - VS Code Copilot Chat headers, plus `X-Initiator`, which tells Copilot
//!   whether the user or the agent started a request.
//!
//! Not supported: Kimi, GitHub Enterprise Server domains, enabling model
//! policies at login.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{AuthStore, ClientId, CredentialKind, Credentials, Secret};
use crate::provider::{
    Content, Event, ModelId, ModelInfo, Provider, ProviderName, Request, Role, Thinking, sse,
};
use crate::{anthropic, openai, openai_chat};

/// The API Copilot serves a model over.
#[derive(Clone, Copy, PartialEq)]
enum Api {
    Messages,
    Chat,
    Responses,
}

fn api(model: &ModelId) -> Api {
    match model.as_str() {
        m if m.starts_with("claude") => Api::Messages,
        m if m.starts_with("gemini") => Api::Chat,
        _ => Api::Responses,
    }
}

/// Key in the credential file and `rho login` argument.
pub const KEY: ProviderName = ProviderName::from_static("github-copilot");
/// VS Code Copilot Chat's OAuth app, like pi.
const CLIENT_ID: ClientId = ClientId::from_static("Iv1.b507a08c87ecfe98");
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const HEADERS: [(&str, &str); 4] = [
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];

#[derive(Clone)]
pub struct Copilot {
    http: reqwest::Client,
    auth: Arc<AuthStore>,
}

impl Copilot {
    /// `auth` must use the `KEY` entry.
    pub fn new(http: reqwest::Client, auth: Arc<AuthStore>) -> Self {
        Self { http, auth }
    }

    async fn send(self, request: Request, body: Value) -> Result<reqwest::Response> {
        let creds = self
            .auth
            .fresh(async |http, creds| copilot_token(http, &creds.refresh).await)
            .await?;
        let base = base_url(creds.access.expose());
        let req = match api(&request.model) {
            Api::Messages => self
                .http
                .post(format!("{base}/v1/messages"))
                .header("anthropic-version", "2023-06-01"),
            Api::Chat => self.http.post(format!("{base}/chat/completions")),
            Api::Responses => self.http.post(format!("{base}/responses")),
        };
        let mut req = req
            .json(&body)
            .bearer_auth(creds.access.expose())
            .header("accept", "text/event-stream")
            .header("openai-intent", "conversation-edits")
            .header("x-initiator", initiator(&request));
        for (name, value) in HEADERS {
            req = req.header(name, value);
        }
        if has_images(&request) {
            req = req.header("copilot-vision-request", "true");
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "github-copilot: {status}: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(resp)
    }
}

impl Provider for Copilot {
    fn stream(
        &self,
        request: Request,
        before_send: &dyn Fn(&mut Value),
    ) -> BoxStream<'static, Result<Event>> {
        let api = api(&request.model);
        let mut body = match api {
            Api::Messages => {
                serde_json::to_value(anthropic::build_body(&request)).expect("body is JSON")
            }
            Api::Chat => {
                let mut body = openai_chat::build_body(&request);
                body["reasoning_effort"] = chat_effort(request.thinking).into();
                body
            }
            Api::Responses => openai::build_body(&request),
        };
        before_send(&mut body);
        let send = self.clone().send(request, body);
        match api {
            Api::Messages => {
                let mut parser = anthropic::SseParser::default();
                sse::events(send, move |b| parser.feed(b))
            }
            Api::Chat => {
                let mut parser = openai_chat::SseParser::default();
                sse::events(send, move |b| parser.feed(b))
            }
            Api::Responses => {
                let mut parser = openai::SseParser::default();
                sse::events(send, move |b| parser.feed(b))
            }
        }
    }

    fn models(&self) -> Vec<ModelInfo> {
        crate::provider::embedded_models(&KEY)
    }
}

/// Copilot's Gemini models take only low, medium and high (some also
/// xhigh); thinking cannot be turned off.
fn chat_effort(thinking: Thinking) -> &'static str {
    match thinking {
        Thinking::Off | Thinking::Low => "low",
        Thinking::Medium => "medium",
        Thinking::High | Thinking::XHigh | Thinking::Max => "high",
    }
}

/// API host from the token's `proxy-ep=proxy.<host>` field, like pi.
fn base_url(token: &str) -> String {
    token
        .split(';')
        .find_map(|field| field.strip_prefix("proxy-ep="))
        .map_or(DEFAULT_BASE_URL.into(), |host| {
            format!("https://{}", host.replacen("proxy.", "api.", 1))
        })
}

/// "user" when a person wrote the last message, "agent" for tool results.
/// pi uses the role of the last message; rho sends tool results as user
/// messages, so this looks at the content.
fn initiator(request: &Request) -> &'static str {
    let agent = request.messages.last().is_some_and(|m| {
        m.role != Role::User
            || m.content
                .iter()
                .any(|c| matches!(c, Content::ToolResult { .. }))
    });
    if agent { "agent" } else { "user" }
}

fn has_images(request: &Request) -> bool {
    request.messages.iter().flat_map(|m| &m.content).any(|c| {
        matches!(c, Content::Image { .. })
            || matches!(c, Content::ToolResult { images, .. } if !images.is_empty())
    })
}

/// Exchanges the GitHub OAuth token for a Copilot API token.
pub async fn copilot_token(http: &reqwest::Client, github_token: &Secret) -> Result<Credentials> {
    #[derive(Deserialize)]
    struct TokenResponse {
        token: Secret,
        /// Unix seconds.
        expires_at: i64,
    }
    let mut req = http
        .get(COPILOT_TOKEN_URL)
        .bearer_auth(github_token.expose())
        .header("accept", "application/json");
    for (name, value) in HEADERS {
        req = req.header(name, value);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("copilot token request failed: {status}: {text}");
    }
    let t: TokenResponse = serde_json::from_str(&text).context("invalid copilot token response")?;
    Ok(Credentials {
        kind: CredentialKind::OAuth,
        access: t.token,
        refresh: github_token.clone(),
        expires: t.expires_at * 1000 - 5 * 60 * 1000,
    })
}

/// A started device flow: the user enters `user_code` at `verification_uri`.
#[derive(Deserialize)]
pub struct Device {
    device_code: Secret,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

pub async fn start_login(http: &reqwest::Client) -> Result<Device> {
    let resp = http
        .post(DEVICE_CODE_URL)
        .header("accept", "application/json")
        .form(&[("client_id", CLIENT_ID.as_str()), ("scope", "read:user")])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("device code request failed: {status}: {text}");
    }
    serde_json::from_str(&text).context("invalid device code response")
}

/// Waits until the user has entered the code, then fetches the Copilot token.
pub async fn finish_login(http: &reqwest::Client, device: &Device) -> Result<Credentials> {
    #[derive(Deserialize)]
    struct Poll {
        access_token: Option<Secret>,
        error: Option<String>,
        error_description: Option<String>,
        interval: Option<u64>,
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval;
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() >= deadline {
            bail!("device code expired, run `rho login {KEY}` again");
        }
        let poll: Poll = http
            .post(ACCESS_TOKEN_URL)
            .header("accept", "application/json")
            .form(&[
                ("client_id", CLIENT_ID.as_str()),
                ("device_code", device.device_code.expose()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("invalid access token response")?;
        if let Some(token) = poll.access_token {
            return copilot_token(http, &token).await;
        }
        match poll.error.as_deref() {
            Some("authorization_pending") => {}
            // RFC 8628: add 5 seconds unless the server says otherwise.
            Some("slow_down") => interval = poll.interval.unwrap_or(interval + 5),
            error => bail!(
                "device flow failed: {} {}",
                error.unwrap_or("no token"),
                poll.error_description.unwrap_or_default()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Thinking, ToolCallId};

    #[test]
    fn base_url_from_proxy_endpoint() {
        assert_eq!(
            base_url("tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=x"),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(base_url("tid=1;exp=2"), DEFAULT_BASE_URL);
    }

    #[test]
    fn tool_results_are_agent_initiated() {
        let request = |content| Request {
            model: ModelId::new("claude-sonnet-5.5"),
            system: String::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![content],
            }],
            tools: vec![],
            max_tokens: 1,
            thinking: Thinking::Off,
        };
        assert_eq!(initiator(&request(Content::text("hi"))), "user");
        let result = Content::ToolResult {
            tool_use_id: ToolCallId::new("t"),
            content: "ok".into(),
            is_error: false,
            images: vec![],
            shown: vec![],
        };
        assert_eq!(initiator(&request(result)), "agent");
    }
}
