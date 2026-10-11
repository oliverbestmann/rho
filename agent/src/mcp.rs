//! `Mcp`: tools from MCP servers over Streamable HTTP, configured in
//! `mcp.json` in rho's config dir (Claude's format):
//!
//! ```json
//! { "mcpServers": { "github": { "type": "http", "url": "https://...",
//!     "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" } } } }
//! ```
//!
//! `${VAR}` in header values comes from the environment. Each remote tool
//! becomes `mcp__<server>__<tool>`. Servers are connected on the first
//! session and shared by later ones, so the tool list (and the prompt
//! cache) stays the same. Problems are shown with the first prompt: notices
//! sent during `session/new` do not reach the client.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rho::extension::{Context, Extension, PromptSetup, SessionSetup, ToolOutput};
use rho::provider::{ImageSource, ToolSpec};
use rho::tools::{Tool, truncate};
use serde::Deserialize;
use serde_json::{Value, json};

const CONFIG_FILE: &str = "mcp.json";
const PROTOCOL: &str = "2025-06-18";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize)]
struct ServerConfig {
    #[serde(rename = "type")]
    kind: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

pub struct Mcp {
    servers: Vec<Arc<Server>>,
    /// Config problems, shown in every session.
    problems: Vec<String>,
    /// Problems per session id, shown with its first prompt.
    pending: Mutex<HashMap<String, Vec<String>>>,
}

impl Mcp {
    pub fn from_config() -> Self {
        Self::load(&rho::prompt::config_dir())
    }

    fn load(dir: &Path) -> Self {
        let path = dir.join(CONFIG_FILE);
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        };
        let (mut servers, mut problems) = (vec![], vec![]);
        match config {
            Ok(config) => {
                for (name, c) in config.mcp_servers {
                    match Server::new(&name, c) {
                        Ok(s) => servers.push(Arc::new(s)),
                        Err(e) => problems.push(format!("MCP server {name}: {e:#}")),
                    }
                }
            }
            Err(e) => problems.push(e),
        }
        Self {
            servers,
            problems,
            pending: Mutex::default(),
        }
    }
}

impl Extension for Mcp {
    async fn session_start(&self, session: &mut SessionSetup) {
        let mut problems = self.problems.clone();
        // Spawned so the servers connect in parallel; awaited in order so
        // the tool order is stable.
        let connects: Vec<_> = self
            .servers
            .iter()
            .map(|s| {
                let s = s.clone();
                tokio::spawn(async move { (s.tools().await, s) })
            })
            .collect();
        for c in connects {
            let Ok((tools, server)) = c.await else {
                continue;
            };
            match tools {
                Ok(tools) => session.tools.extend(
                    tools
                        .iter()
                        .filter_map(|t| McpTool::new(&server, t))
                        .map(|t| Arc::new(t) as _),
                ),
                Err(e) => problems.push(format!("MCP server {}: {e:#}", server.name)),
            }
        }
        if !problems.is_empty() {
            let id = session.ctx.session_id().to_string();
            self.pending.lock().unwrap().insert(id, problems);
        }
    }

    fn session_end(&self, ctx: &Context) {
        self.pending.lock().unwrap().remove(ctx.session_id());
    }

    async fn before_prompt(&self, ctx: &Context, _prompt: &mut PromptSetup) {
        let problems = self.pending.lock().unwrap().remove(ctx.session_id());
        for p in problems.into_iter().flatten() {
            ctx.notify("MCP", &p, true);
        }
    }
}

/// The server's MCP session, set by `initialize`.
struct Conn {
    session: Option<String>,
    version: String,
}

/// Returned when the server forgot our MCP session (HTTP 404).
#[derive(Debug)]
struct Expired;

impl std::fmt::Display for Expired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MCP session expired")
    }
}

impl std::error::Error for Expired {}

struct Server {
    name: String,
    url: String,
    headers: HeaderMap,
    http: reqwest::Client,
    next_id: AtomicU64,
    conn: Mutex<Option<Conn>>,
    /// `tools/list` of the first successful connect.
    tools: tokio::sync::Mutex<Option<Vec<Value>>>,
}

impl Server {
    fn new(name: &str, c: ServerConfig) -> Result<Self> {
        match c.kind.as_deref() {
            Some("http") => {}
            other => bail!("type {other:?} is not supported, only \"http\""),
        }
        let url = c.url.context("no url")?;
        let mut headers = HeaderMap::new();
        for (k, v) in &c.headers {
            let mut value =
                HeaderValue::from_str(&expand(v).with_context(|| format!("header {k}"))?)
                    .with_context(|| format!("header {k}"))?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_bytes(k.as_bytes())?, value);
        }
        Ok(Self {
            name: name.into(),
            url,
            headers,
            http: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            conn: Mutex::default(),
            tools: tokio::sync::Mutex::default(),
        })
    }

    /// The server's tools; connects on the first call (or after a failure).
    async fn tools(&self) -> Result<Vec<Value>> {
        let mut cached = self.tools.lock().await;
        if let Some(tools) = &*cached {
            return Ok(tools.clone());
        }
        let tools = tokio::time::timeout(CONNECT_TIMEOUT, async {
            self.initialize().await?;
            self.list_tools().await
        })
        .await
        .context("no answer within 10 s")??;
        *cached = Some(tools.clone());
        Ok(tools)
    }

    async fn initialize(&self) -> Result<()> {
        *self.conn.lock().unwrap() = None;
        let params = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "rho", "version": env!("CARGO_PKG_VERSION")},
        });
        let (result, session) = self.request("initialize", params).await?;
        let version = result["protocolVersion"]
            .as_str()
            .unwrap_or(PROTOCOL)
            .into();
        *self.conn.lock().unwrap() = Some(Conn { session, version });
        self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Value>> {
        let (mut tools, mut cursor) = (vec![], None::<String>);
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?.0;
            tools.extend(result["tools"].as_array().cloned().unwrap_or_default());
            match result["nextCursor"].as_str() {
                Some(c) => cursor = Some(c.into()),
                None => return Ok(tools),
            }
        }
    }

    /// `tools/call`, with one reconnect if the MCP session expired.
    async fn call(&self, tool: &str, input: Value) -> Result<Value> {
        let params = json!({"name": tool, "arguments": input});
        match self.request("tools/call", params.clone()).await {
            Err(e) if e.is::<Expired>() => {
                self.initialize().await?;
                Ok(self.request("tools/call", params).await?.0)
            }
            r => Ok(r?.0),
        }
    }

    /// A JSON-RPC request: its result and the `Mcp-Session-Id` header.
    async fn request(&self, method: &str, params: Value) -> Result<(Value, Option<String>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let res = self
            .post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let header = |name: &str| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        };
        let session = header("mcp-session-id");
        let sse = header(CONTENT_TYPE.as_str()).is_some_and(|t| t.contains("text/event-stream"));
        let body = res.text().await?;
        let msg = if sse {
            sse_message(&body, id).context("no response in the event stream")?
        } else {
            serde_json::from_str(&body).context("invalid JSON-RPC response")?
        };
        if let Some(err) = msg.get("error") {
            bail!("{}", err["message"].as_str().unwrap_or(&err.to_string()));
        }
        Ok((msg["result"].clone(), session))
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        let (req, has_session) = {
            let mut req = self
                .http
                .post(&self.url)
                .headers(self.headers.clone())
                .header(ACCEPT, "application/json, text/event-stream")
                .json(body);
            let conn = self.conn.lock().unwrap();
            let mut has_session = false;
            if let Some(c) = &*conn {
                req = req.header("MCP-Protocol-Version", &c.version);
                if let Some(s) = &c.session {
                    req = req.header("Mcp-Session-Id", s);
                    has_session = true;
                }
            }
            (req, has_session)
        };
        let res = req.send().await?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND && has_session {
            return Err(Expired.into());
        }
        if !status.is_success() {
            bail!("HTTP {status}");
        }
        Ok(res)
    }
}

/// The JSON-RPC message with `id` from a complete SSE body.
// shortcut: reads the whole stream, no progress or server requests; upgrade to streaming if a server needs them.
fn sse_message(body: &str, id: u64) -> Option<Value> {
    let mut data = String::new();
    for line in body.lines().chain([""]) {
        if line.is_empty() {
            if let Ok(msg) = serde_json::from_str::<Value>(&data)
                && msg["id"] == id
            {
                return Some(msg);
            }
            data.clear();
        } else if let Some(d) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(d.strip_prefix(' ').unwrap_or(d));
        }
    }
    None
}

/// `${VAR}` replaced with the environment variable.
fn expand(s: &str) -> Result<String> {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| anyhow!("unclosed ${{"))?;
        let var = &after[..end];
        out.push_str(&std::env::var(var).map_err(|_| anyhow!("${{{var}}} is not set"))?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `mcp__server__tool` with what the API allows: `[A-Za-z0-9_-]`, 64 chars.
fn tool_name(server: &str, tool: &str) -> String {
    let mut name: String = format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.truncate(64);
    name
}

struct McpTool {
    server: Arc<Server>,
    /// The tool's name on the server.
    remote: String,
    spec: ToolSpec,
}

impl McpTool {
    fn new(server: &Arc<Server>, tool: &Value) -> Option<Self> {
        let remote = tool["name"].as_str()?.to_string();
        Some(Self {
            spec: ToolSpec {
                name: tool_name(&server.name, &remote),
                description: tool["description"].as_str().unwrap_or_default().into(),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            },
            server: server.clone(),
            remote,
        })
    }
}

impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        Ok(to_output(&self.server.call(&self.remote, input).await?))
    }
}

/// A `tools/call` result: text and text resources as output, valid images
/// as images.
fn to_output(result: &Value) -> ToolOutput {
    let (mut text, mut images) = (vec![], vec![]);
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("text") => text.push(item["text"].as_str().unwrap_or_default().to_string()),
            Some("resource") => {
                if let Some(t) = item["resource"]["text"].as_str() {
                    text.push(t.to_string());
                }
            }
            Some("image") => {
                let source = ImageSource::Base64 {
                    media_type: item["mimeType"].as_str().unwrap_or_default().into(),
                    data: item["data"].as_str().unwrap_or_default().into(),
                };
                match source.check() {
                    Ok(()) => images.push(source),
                    Err(e) => text.push(format!("[image left out: {e:#}]")),
                }
            }
            _ => {}
        }
    }
    if text.is_empty()
        && let Some(s) = result.get("structuredContent")
    {
        text.push(s.to_string());
    }
    let output = if text.is_empty() && images.is_empty() {
        "(no output)".into()
    } else {
        truncate(text.join("\n"))
    };
    ToolOutput {
        output,
        is_error: result["isError"].as_bool().unwrap_or(false),
        images,
        diff: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap as Headers, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use rho::extension::NoFrontend;

    #[test]
    fn expands_env_vars() {
        // SAFETY: tests in this module do not read the variable concurrently.
        unsafe { std::env::set_var("RHO_MCP_TEST_TOKEN", "abc") };
        assert_eq!(
            expand("Bearer ${RHO_MCP_TEST_TOKEN}!").unwrap(),
            "Bearer abc!"
        );
        assert_eq!(expand("plain").unwrap(), "plain");
        assert!(expand("${RHO_MCP_UNSET_VAR}").is_err());
        assert!(expand("${OPEN").is_err());
    }

    #[test]
    fn cleans_tool_names() {
        assert_eq!(tool_name("git hub", "get.issue"), "mcp__git_hub__get_issue");
        assert_eq!(tool_name("s", &"x".repeat(100)).len(), 64);
    }

    #[test]
    fn reads_sse_bodies() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n\n\
                    data: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":7,\"result\":1}\n\n";
        assert_eq!(sse_message(body, 7).unwrap()["result"], 1);
        assert!(sse_message(body, 8).is_none());
    }

    #[test]
    fn maps_results() {
        let out = to_output(&json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "resource", "resource": {"uri": "x", "text": "b"}},
            {"type": "image", "mimeType": "image/bmp", "data": "AAAA"},
        ], "isError": true}));
        assert!(out.is_error);
        assert!(
            out.output.starts_with("a\nb\n[image left out"),
            "{}",
            out.output
        );
        assert_eq!(
            to_output(&json!({"content": [], "structuredContent": {"n": 1}})).output,
            r#"{"n":1}"#
        );
    }

    /// A mock MCP server: answers with SSE when the path is `/sse`, forgets
    /// its session once after the first tool call.
    async fn mock(
        axum::extract::Path(mode): axum::extract::Path<String>,
        axum::extract::State(calls): axum::extract::State<Arc<Mutex<Vec<String>>>>,
        headers: Headers,
        axum::Json(msg): axum::Json<Value>,
    ) -> Response {
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let session = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string();
        let expired = {
            let mut calls = calls.lock().unwrap();
            calls.push(format!("{method} {session}"));
            method == "tools/call"
                && calls.iter().filter(|c| c.starts_with("tools/call")).count() == 2
        };
        if expired {
            return StatusCode::NOT_FOUND.into_response();
        }
        let result = match method.as_str() {
            "initialize" => json!({"protocolVersion": PROTOCOL, "capabilities": {"tools": {}}}),
            "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
            "tools/list" if msg["params"]["cursor"].is_null() => json!({
                "tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}],
                "nextCursor": "2",
            }),
            "tools/list" => json!({"tools": [{"name": "fail"}]}),
            "tools/call" => json!({
                "content": [{"type": "text", "text": msg["params"]["arguments"]["text"]}],
                "isError": msg["params"]["name"] == "fail",
            }),
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        let reply = json!({"jsonrpc": "2.0", "id": msg["id"], "result": result}).to_string();
        let session = [("mcp-session-id", "s1")];
        if mode == "sse" {
            let body = format!("event: message\ndata: {reply}\n\n");
            (session, [(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        } else {
            (session, [(header::CONTENT_TYPE, "application/json")], reply).into_response()
        }
    }

    #[tokio::test]
    async fn lists_and_calls_tools() {
        let calls = Arc::new(Mutex::new(vec![]));
        let app = axum::Router::new()
            .route("/{mode}", axum::routing::post(mock))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let dir = tempfile::tempdir().unwrap();
        let config = json!({"mcpServers": {
            "json": {"type": "http", "url": format!("{base}/json")},
            "sse": {"type": "http", "url": format!("{base}/sse")},
            "local": {"command": "npx"},
        }});
        std::fs::write(dir.path().join(CONFIG_FILE), config.to_string()).unwrap();
        let mcp = Mcp::load(dir.path());
        assert_eq!(mcp.servers.len(), 2);
        assert!(mcp.problems[0].contains("local"), "{:?}", mcp.problems);

        let ctx = Context::new("s", "/tmp", Arc::new(NoFrontend), Default::default());
        let mut setup = SessionSetup {
            ctx: ctx.clone(),
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![],
        };
        mcp.session_start(&mut setup).await;
        let names: Vec<_> = setup.tools.iter().map(|t| t.spec().name).collect();
        assert_eq!(
            names,
            [
                "mcp__json__echo",
                "mcp__json__fail",
                "mcp__sse__echo",
                "mcp__sse__fail"
            ]
        );

        for tool in &setup.tools[2..] {
            let out = tool.run(json!({"text": "hi"}), &ctx).await.unwrap();
            assert_eq!(out.output, "hi");
            assert_eq!(out.is_error, tool.spec().name.ends_with("fail"));
        }
        let calls = calls.lock().unwrap();
        // The second call got a 404, reconnected and was sent again.
        let sse: Vec<_> = calls[calls.len() - 5..].to_vec();
        assert_eq!(
            sse,
            [
                "tools/call s1",
                "tools/call s1",
                "initialize -",
                "notifications/initialized s1",
                "tools/call s1"
            ]
        );
    }
}
