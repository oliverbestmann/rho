//! `Memory`: persistent memories through the mcp-memory-service REST API
//! (pi's memory). `MCP_MEMORY_URL` sets the service (default
//! `http://localhost:6758`), `MCP_API_KEY` a bearer token. Responses are
//! formatted per action; unexpected shapes fall back to the raw JSON. Long
//! output is cut, the full JSON goes to a temporary file.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use reqwest::{Method, Url};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::ToolSpec;
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

const DEFAULT_URL: &str = "http://localhost:6758";
const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

pub struct Memory;

impl Extension for Memory {
    async fn session_start(&self, session: &mut SessionSetup) {
        let base = std::env::var("MCP_MEMORY_URL").unwrap_or_else(|_| DEFAULT_URL.into());
        session.tools.push(Arc::new(MemoryTool {
            base,
            api_key: std::env::var("MCP_API_KEY").ok(),
            http: reqwest::Client::new(),
        }));
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Search,
    Store,
    List,
    Get,
    Update,
    Delete,
    Health,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Action::Search => "search",
            Action::Store => "store",
            Action::List => "list",
            Action::Get => "get",
            Action::Update => "update",
            Action::Delete => "delete",
            Action::Health => "health",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Input {
    action: Action,
    /// Search query (search)
    query: Option<String>,
    /// Memory content (store)
    content: Option<String>,
    /// Memory content hash (get, update, delete)
    hash: Option<String>,
    /// Memory tags (store, update)
    tags: Option<Vec<String>>,
    /// Memory type (store, list, update)
    memory_type: Option<String>,
    /// Memory metadata (store, update)
    metadata: Option<Map<String, Value>>,
    /// Search result limit (default: 10)
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
    /// Minimum search similarity
    #[schemars(range(min = 0.0, max = 1.0))]
    threshold: Option<f64>,
    /// List page (default: 1)
    #[schemars(range(min = 1))]
    page: Option<u32>,
    /// List page size (default: 10)
    #[schemars(range(min = 1, max = 100))]
    page_size: Option<u32>,
    /// Tag filter (list)
    tag: Option<String>,
    /// Conversation ID; bypasses semantic deduplication (store)
    conversation_id: Option<String>,
    /// Agent ID added as an agent:<id> tag (store)
    agent_id: Option<String>,
}

struct MemoryTool {
    base: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

/// Method, URL and JSON body for the endpoint of `i.action`.
fn request(base: &str, i: &Input) -> Result<(Method, Url, Option<Value>)> {
    let hash;
    let mut url =
        Url::parse(base).with_context(|| format!("invalid memory service URL {base:?}"))?;
    let mut path = vec!["api", "memories"];
    let mut method = Method::GET;
    let mut body = None;
    let mut query: Vec<(&str, String)> = vec![];
    let nonempty = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
    let need_hash = || -> Result<String> {
        match nonempty(&i.hash) {
            Some(h) => Ok(h),
            None => bail!("hash is required for {}", i.action.name()),
        }
    };

    match i.action {
        Action::Search => {
            let Some(q) = nonempty(&i.query) else {
                bail!("query is required for search");
            };
            path = vec!["api", "search"];
            method = Method::POST;
            let mut b = json!({ "query": q, "n_results": i.limit.unwrap_or(10) });
            if let Some(t) = i.threshold {
                b["similarity_threshold"] = t.into();
            }
            body = Some(b);
        }
        Action::Store => {
            let Some(content) = nonempty(&i.content) else {
                bail!("content is required for store");
            };
            method = Method::POST;
            let mut b = json!({ "content": content, "tags": i.tags.clone().unwrap_or_default() });
            if let Some(t) = nonempty(&i.memory_type) {
                b["memory_type"] = t.into();
            }
            if let Some(m) = &i.metadata {
                b["metadata"] = m.clone().into();
            }
            if let Some(c) = nonempty(&i.conversation_id) {
                b["conversation_id"] = c.into();
            }
            body = Some(b);
        }
        Action::List => {
            query.push(("page", i.page.unwrap_or(1).to_string()));
            query.push(("page_size", i.page_size.unwrap_or(10).to_string()));
            if let Some(t) = nonempty(&i.tag) {
                query.push(("tag", t));
            }
            if let Some(t) = nonempty(&i.memory_type) {
                query.push(("memory_type", t));
            }
        }
        Action::Get | Action::Delete => {
            hash = need_hash()?;
            path.push(&hash);
            if i.action == Action::Delete {
                method = Method::DELETE;
            }
        }
        Action::Update => {
            hash = need_hash()?;
            path.push(&hash);
            method = Method::PUT;
            let mut b = json!({});
            if let Some(t) = &i.tags {
                b["tags"] = t.clone().into();
            }
            if let Some(t) = nonempty(&i.memory_type) {
                b["memory_type"] = t.into();
            }
            if let Some(m) = &i.metadata {
                b["metadata"] = m.clone().into();
            }
            body = Some(b);
        }
        Action::Health => path = vec!["api", "health"],
    }

    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid memory service URL {base:?}"))?
        .pop_if_empty()
        .extend(path);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Ok((method, url, body))
}

impl Tool for MemoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Memory".into(),
            description: format!(
                "Search, store, list, get, update, or delete persistent memories through {}. \
                 Output is truncated to {MAX_LINES} lines or {}KB.\n\
                 Use memory search at the start of a task for relevant decisions, conventions, errors, and unfinished work.\n\
                 Use memory store after substantial work to preserve reusable decisions and learnings, never secrets or transient progress.",
                self.base,
                MAX_BYTES / 1024
            ),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let (method, url, body) = request(&self.base, &i)?;
        let mut req = self.http.request(method, url);
        if let Some(body) = &body {
            req = req.json(body);
        }
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        if let Some(agent) = &i.agent_id {
            req = req.header("X-Agent-ID", agent);
        }
        let res = req.send().await.context("memory service unreachable")?;
        let status = res.status();
        let text = res.text().await?;
        if !status.is_success() {
            bail!("memory service failed: {status}: {text}");
        }

        // `raw` is the full pretty JSON: saved when the output is cut, and
        // shown when the response has an unexpected shape.
        let data: Option<Value> = if text.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&text).context("memory service sent invalid JSON")?)
        };
        let raw = match &data {
            Some(d) => serde_json::to_string_pretty(d)?,
            None => "(empty)".into(),
        };
        let formatted = data.as_ref().and_then(|d| format(i.action, d));
        let formatted = formatted.as_deref().unwrap_or(&raw);

        let (mut output, cut) = truncate_head(formatted);
        if cut {
            let path = std::env::temp_dir().join(format!(
                "rho-memory-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ));
            std::fs::write(&path, &raw)?;
            output += &format!("\n\n[Output truncated. Full output: {}]", path.display());
        }
        Ok(output.into())
    }
}

/// Whole lines from the start, at most `MAX_LINES` lines and `MAX_BYTES`
/// bytes (pi's `truncateHead`). A first line over the limit is cut. Returns
/// whether anything was dropped.
fn truncate_head(text: &str) -> (String, bool) {
    let mut out = String::new();
    for (n, line) in text.split('\n').enumerate() {
        let sep = usize::from(n > 0);
        if n == MAX_LINES || out.len() + sep + line.len() > MAX_BYTES {
            if n == 0 {
                let mut end = MAX_BYTES;
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                out.push_str(&line[..end]);
            }
            return (out, true);
        }
        if sep == 1 {
            out.push('\n');
        }
        out.push_str(line);
    }
    (out, false)
}

fn format(action: Action, data: &Value) -> Option<String> {
    match action {
        Action::Search => format_search(data),
        Action::List => format_list(data),
        Action::Get => format_get(data),
        Action::Store | Action::Update => format_mutation(data),
        Action::Delete => format_delete(data),
        Action::Health => format_health(data),
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str().filter(|s| !s.is_empty())
}

fn join(parts: impl IntoIterator<Item = Option<String>>, sep: &str) -> String {
    let parts: Vec<String> = parts
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(sep)
}

/// A number as JavaScript prints it (`3`, not `3.0`).
fn number(v: &Value) -> String {
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
        _ => v.to_string(),
    }
}

/// Title (index, type, score), content, tags and hash; other fields
/// (timestamps, access counts) are left out.
fn format_memory(memory: Option<&Value>, title: Option<String>) -> String {
    let memory = memory.unwrap_or(&Value::Null);
    let tags: Vec<&str> = memory
        .get("tags")
        .and_then(Value::as_array)
        .map(|t| t.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let content = match memory.get("content") {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => "(no content)".into(),
        Some(other) => other.to_string(),
    };
    join(
        [
            title,
            Some(content),
            (!tags.is_empty()).then(|| format!("Tags: {}", tags.join(", "))),
            str_of(memory, "content_hash").map(|h| format!("Hash: {h}")),
        ],
        "\n",
    )
}

fn type_tag(memory: Option<&Value>) -> Option<String> {
    memory
        .and_then(|m| str_of(m, "memory_type"))
        .map(|t| format!("[{t}]"))
}

/// POST /api/search -> { query, total_found, results: [{ memory, similarity_score }] }
fn format_search(data: &Value) -> Option<String> {
    let results = data.get("results")?.as_array()?;
    let count = data
        .get("total_found")
        .filter(|v| !v.is_null())
        .map(number)
        .unwrap_or_else(|| results.len().to_string());
    let noun = if count == "1" { "memory" } else { "memories" };
    let query = str_of(data, "query")
        .map(|q| format!(" for {}", Value::from(q)))
        .unwrap_or_default();
    let mut parts = vec![format!("Found {count} {noun}{query}.")];
    for (n, result) in results.iter().enumerate() {
        let memory = result.get("memory");
        let score = result
            .get("similarity_score")
            .and_then(Value::as_f64)
            .map(|s| format!("score {s:.3}"));
        let title = join([Some(format!("{}.", n + 1)), type_tag(memory), score], " ");
        parts.push(format_memory(memory, Some(title)));
    }
    Some(parts.join("\n\n"))
}

/// GET /api/memories -> { memories: [...], total, page, page_size, has_more }
fn format_list(data: &Value) -> Option<String> {
    let memories = data.get("memories")?.as_array()?;
    let total = data
        .get("total")
        .filter(|v| !v.is_null())
        .map(number)
        .unwrap_or_else(|| memories.len().to_string());
    let heading = join(
        [
            Some(format!("{} of {total} memories", memories.len())),
            data.get("page")
                .filter(|v| !v.is_null())
                .map(|p| format!("page {}", number(p))),
            (data.get("has_more") == Some(&Value::Bool(true))).then(|| "(more available)".into()),
        ],
        " ",
    );
    let mut parts = vec![heading];
    for (n, memory) in memories.iter().enumerate() {
        let title = join([Some(format!("{}.", n + 1)), type_tag(Some(memory))], " ");
        parts.push(format_memory(Some(memory), Some(title)));
    }
    Some(parts.join("\n\n"))
}

/// GET /api/memories/{hash} -> the memory itself.
fn format_get(data: &Value) -> Option<String> {
    data.as_object()?.get("content")?;
    Some(format_memory(Some(data), type_tag(Some(data))))
}

/// POST /api/memories, PUT /api/memories/{hash} -> { success, message, memory }
fn format_mutation(data: &Value) -> Option<String> {
    let memory = data.as_object()?.get("memory")?;
    let memory = (!memory.is_null()).then_some(memory);
    Some(join(
        [
            str_of(data, "message").map(String::from),
            Some(format_memory(memory, None)),
        ],
        "\n",
    ))
}

/// DELETE /api/memories/{hash} -> { success, message }
fn format_delete(data: &Value) -> Option<String> {
    match data.as_object()?.get("message")? {
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// GET /api/health -> { status }
fn format_health(data: &Value) -> Option<String> {
    str_of(data, "status").map(|s| format!("Memory service status: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::NoFrontend;

    fn input(v: Value) -> Input {
        parse(v).unwrap()
    }

    #[test]
    fn builds_requests() {
        let base = "http://localhost:6758";
        let (m, url, body) = request(
            base,
            &input(json!({"action": "search", "query": "q", "threshold": 0.5})),
        )
        .unwrap();
        assert_eq!(
            (m, url.as_str()),
            (Method::POST, "http://localhost:6758/api/search")
        );
        assert_eq!(
            body.unwrap(),
            json!({"query": "q", "n_results": 10, "similarity_threshold": 0.5})
        );

        let (m, url, body) = request(
            base,
            &input(json!({"action": "list", "tag": "a b", "pageSize": 5})),
        )
        .unwrap();
        assert_eq!(m, Method::GET);
        assert_eq!(
            url.as_str(),
            "http://localhost:6758/api/memories?page=1&page_size=5&tag=a+b"
        );
        assert!(body.is_none());

        let (m, url, _) = request(
            "http://h/prefix/",
            &input(json!({"action": "delete", "hash": "a/b"})),
        )
        .unwrap();
        assert_eq!(
            (m, url.as_str()),
            (Method::DELETE, "http://h/prefix/api/memories/a%2Fb")
        );

        let (_, _, body) = request(base, &input(json!({"action": "store", "content": "c", "memoryType": "note", "metadata": {"k": 1}}))).unwrap();
        assert_eq!(
            body.unwrap(),
            json!({"content": "c", "tags": [], "memory_type": "note", "metadata": {"k": 1}})
        );

        let (m, _, body) = request(
            base,
            &input(json!({"action": "update", "hash": "h", "tags": ["x"]})),
        )
        .unwrap();
        assert_eq!((m, body.unwrap()), (Method::PUT, json!({"tags": ["x"]})));

        for (v, err) in [
            (json!({"action": "search"}), "query is required for search"),
            (json!({"action": "store"}), "content is required for store"),
            (json!({"action": "get"}), "hash is required for get"),
            (
                json!({"action": "update", "hash": ""}),
                "hash is required for update",
            ),
        ] {
            assert_eq!(request(base, &input(v)).unwrap_err().to_string(), err);
        }
    }

    #[test]
    fn formats_responses() {
        let search = json!({
            "query": "go",
            "total_found": 2,
            "results": [
                {"memory": {"content": "use rg", "content_hash": "h1", "tags": ["a", "b"], "memory_type": "note", "created_at": 1}, "similarity_score": 0.91234},
                {"memory": {"content": "x", "tags": []}},
            ]
        });
        assert_eq!(
            format(Action::Search, &search).unwrap(),
            "Found 2 memories for \"go\".\n\n1. [note] score 0.912\nuse rg\nTags: a, b\nHash: h1\n\n2.\nx"
        );
        assert_eq!(
            format(Action::Search, &json!({"total_found": 1, "results": []})).unwrap(),
            "Found 1 memory."
        );
        assert_eq!(format(Action::Search, &json!([])), None);

        let list = json!({"memories": [{"content": "c", "memory_type": "t"}], "total": 3, "page": 1, "has_more": true});
        assert_eq!(
            format(Action::List, &list).unwrap(),
            "1 of 3 memories page 1 (more available)\n\n1. [t]\nc"
        );
        assert_eq!(
            format(
                Action::Get,
                &json!({"content": "c", "memory_type": "t", "content_hash": "h"})
            )
            .unwrap(),
            "[t]\nc\nHash: h"
        );
        assert_eq!(format(Action::Get, &json!({"other": 1})), None);
        assert_eq!(
            format(
                Action::Store,
                &json!({"success": true, "message": "Stored", "memory": {"content": "c"}})
            )
            .unwrap(),
            "Stored\nc"
        );
        assert_eq!(format(Action::Update, &json!({"success": false})), None);
        assert_eq!(
            format(Action::Delete, &json!({"message": "Deleted"})).unwrap(),
            "Deleted"
        );
        assert_eq!(
            format(Action::Health, &json!({"status": "healthy"})).unwrap(),
            "Memory service status: healthy"
        );
        assert_eq!(format(Action::Health, &json!({})), None);
    }

    #[test]
    fn truncates_from_the_head() {
        assert_eq!(truncate_head("a\nb"), ("a\nb".into(), false));
        let many = vec!["x"; MAX_LINES + 1].join("\n");
        let (out, cut) = truncate_head(&many);
        assert!(cut && out.lines().count() == MAX_LINES);
        let long = "é".repeat(MAX_BYTES);
        let (out, cut) = truncate_head(&format!("{long}\nnext"));
        assert!(cut && out.len() == MAX_BYTES && !out.contains('\n'));
    }

    #[tokio::test]
    async fn talks_to_the_service() {
        use axum::routing::{get, post};
        let app = axum::Router::new()
            .route("/api/health", get(|| async { axum::Json(json!({"status": "healthy"})) }))
            .route(
                "/api/search",
                post(|headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                    let auth = headers["authorization"].to_str().unwrap().to_string();
                    let agent = headers["x-agent-id"].to_str().unwrap().to_string();
                    axum::Json(json!({"query": body["query"], "results": [{"memory": {"content": format!("{auth} {agent}")}}]}))
                }),
            )
            .route(
                "/api/memories/{hash}",
                get(|| async { (axum::http::StatusCode::NOT_FOUND, "no such memory") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let tool = MemoryTool {
            base,
            api_key: Some("k".into()),
            http: reqwest::Client::new(),
        };
        let ctx = Context::new("s", "/tmp", Arc::new(NoFrontend), Default::default());
        let out = tool.run(json!({"action": "health"}), &ctx).await.unwrap();
        assert_eq!(out.output, "Memory service status: healthy");
        let out = tool
            .run(
                json!({"action": "search", "query": "q", "agentId": "rho"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.output, "Found 1 memory for \"q\".\n\n1.\nBearer k rho");
        let err = tool
            .run(json!({"action": "get", "hash": "h"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "memory service failed: 404 Not Found: no such memory"
        );
    }
}
