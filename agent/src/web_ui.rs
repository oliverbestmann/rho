//! A small web page to follow sessions and queue messages for them, built
//! only on extension hooks.
//!
//! What it sees of this process's open sessions: every user-side message as
//! it goes to the model (prompts, queued and steering messages, profile
//! notes; from `before_request`), tool calls as they start and end, and the
//! model's text and thinking once its turn is done (`turn_end`; hooks do not
//! see the stream). Other sessions in the same databases (closed, or open in
//! another rho) are listed too and show their stored history, read-only.
//!
//! A message sent from the page is shown in the session's client as a notice
//! and goes to `Context::send` with `trigger_turn`: during a run it rides
//! along with the next tool results, when idle it starts a run.
//!
//! Configured in `web.json` in the config dir:
//!
//! ```json
//! { "listen": "0.0.0.0:8787", "password": "something long" }
//! ```
//!
//! `listen` defaults to `0.0.0.0:8787` (`RHO_WEB_UI` overrides it, `off`
//! disables the page); if the port is taken (another rho), the next free one
//! of the following ten. Requests from this machine need no password; others
//! need `password` (the page asks once and keeps it), and are refused without
//! one. Only `localhost` or IP addresses are accepted as `Host` (no DNS
//! rebinding).

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rho::extension::{
    Context, Decision, Extension, SessionSetup, Settle, SlashCommand, ToolCall, ToolOutput, TurnEnd,
};
use rho::provider::{Content, Message, Request as ModelRequest, Role};
use rho::store::{SessionInfo, Store};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_ADDR: &str = "0.0.0.0:8787";
const PORT_TRIES: u16 = 10;
/// Tool output kept per call; the page is for following along.
const MAX_OUTPUT: usize = 4000;
/// Sessions in the list.
const LIST_LIMIT: usize = 100;
const PASSWORD_HEADER: &str = "x-rho-password";
const CONFIG_FILE: &str = "web.json";
const PAGE: &str = include_str!("web_ui.html");

/// `web.json`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `ip:port`, or `off`.
    pub listen: Option<String>,
    /// Needed by everyone but this machine.
    pub password: Option<String>,
}

pub struct WebUi {
    /// `ip:port` to listen on, or `off`.
    listen: String,
    /// Why there is no web view (an invalid `web.json`).
    problem: Option<String>,
    hub: Arc<Hub>,
    /// Where the page is served, once the first session started it.
    addr: OnceLock<Result<SocketAddr, String>>,
}

struct Hub {
    /// Required on API requests from other machines; without one they are refused.
    password: Option<String>,
    sessions: Mutex<BTreeMap<String, View>>,
    /// The databases of the sessions seen so far, for the stored ones.
    stores: Mutex<Vec<Store>>,
}

/// An open session.
struct View {
    ctx: Context,
    log: Timeline,
    running: bool,
    /// Context in use after the last request, in tokens.
    context: u64,
}

/// What the page shows of a session's conversation.
#[derive(Default)]
struct Timeline {
    /// Append-only; the page asks for those after the last one it has.
    events: Vec<Value>,
    /// Model requests so far. Events carry theirs, so the page can put the
    /// model's text (known at the turn's end) before its tool calls.
    turn: u64,
    /// Unix ms of the last event.
    updated: u64,
    /// Sent from the page, not yet delivered to the model.
    queued: Vec<String>,
}

/// Where an event goes within its turn.
#[derive(Clone, Copy)]
enum Slot {
    User = 0,
    Model = 1,
    Tool = 2,
}

impl Timeline {
    /// A stored conversation. Like live: a user message starts a turn, the
    /// model's answer belongs to it, a tool result shows with its call.
    fn from_history(history: &[Message]) -> Self {
        let mut log = Self::default();
        for Message { role, content } in history {
            match role {
                Role::User => {
                    log.turn += 1;
                    log.push_user(content, true);
                }
                Role::Assistant => log.push_model(content, true),
            }
        }
        log
    }

    fn push(&mut self, slot: Slot, mut event: Value) {
        event["seq"] = self.events.len().into();
        event["turn"] = self.turn.into();
        event["slot"] = (slot as u8).into();
        self.events.push(event);
        self.updated = now_ms();
    }

    /// The model's side of a message: thinking, text and tool calls (the
    /// calls only from stored history; live ones come from the tool hooks).
    fn push_model(&mut self, content: &[Content], with_calls: bool) {
        for c in content {
            match c {
                Content::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                    self.push(Slot::Model, json!({"kind": "thinking", "text": thinking}))
                }
                Content::Text { text } if !text.trim().is_empty() => {
                    self.push(Slot::Model, json!({"kind": "assistant", "text": text}))
                }
                Content::ToolUse { id, name, input } if with_calls => self.push(
                    Slot::Tool,
                    json!({"kind": "tool", "id": id, "name": name, "input": input}),
                ),
                _ => {}
            }
        }
    }

    /// The user's side: text and images; tool results only from stored
    /// history. Delivered queued messages leave the queue.
    fn push_user(&mut self, content: &[Content], with_results: bool) {
        for c in content {
            match c {
                Content::Text { text } => {
                    if let Some(i) = self.queued.iter().position(|q| q == text) {
                        self.queued.remove(i);
                    }
                    self.push(Slot::User, json!({"kind": "user", "text": text}));
                }
                Content::Image { .. } => {
                    self.push(Slot::User, json!({"kind": "user", "text": "[image]"}))
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } if with_results => {
                    self.push(Slot::Tool, tool_end(tool_use_id, content, *is_error))
                }
                _ => {}
            }
        }
    }

    /// The summary plus the events from `since` on.
    fn page(&self, mut summary: Value, since: usize) -> Value {
        summary["total"] = self.events.len().into();
        summary["events"] = self.events.get(since..).unwrap_or_default().into();
        summary
    }
}

impl View {
    fn summary(&self, id: &str) -> Value {
        json!({
            "id": id,
            "title": self.ctx.title(),
            "cwd": self.ctx.cwd(),
            "profile": self.ctx.profile().name,
            "open": true,
            "running": self.running,
            "updated": self.log.updated,
            "context": self.context,
            "queued": self.log.queued,
        })
    }
}

/// A session that is not open in this process.
fn stored_summary(s: &SessionInfo) -> Value {
    let mut profile = s.profile.clone();
    if let Some(first) = profile.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    json!({
        "id": s.id,
        "title": s.title,
        "cwd": s.cwd,
        "profile": profile,
        "open": false,
        "running": false,
        "updated": s.updated_at,
        "context": s.last_usage.prompt() + s.last_usage.output,
        "queued": [],
    })
}

fn tool_end(id: &str, output: &str, is_error: bool) -> Value {
    let output = match output.char_indices().nth(MAX_OUTPUT) {
        Some((end, _)) => format!("{}\n[… cut for the web view]", &output[..end]),
        None => output.to_string(),
    };
    json!({"kind": "tool_end", "id": id, "output": output, "is_error": is_error})
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Whether an API request may go through: this machine always, others
/// with the password. The error is for the page.
fn admit(
    client: Option<IpAddr>,
    given: Option<&str>,
    password: Option<&str>,
) -> Result<(), &'static str> {
    if client.is_some_and(|ip| ip.to_canonical().is_loopback()) {
        return Ok(());
    }
    match password {
        None => Err(
            "No password is set. Put one in ~/.config/rho/web.json to use the web view from other devices.",
        ),
        Some(p) if given == Some(p) => Ok(()),
        Some(_) => Err("Wrong password."),
    }
}

/// `localhost` or an IP address, with or without port. A DNS rebinding
/// attack needs a domain name.
fn local_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    let name = name.trim_start_matches('[').trim_end_matches(']');
    name == "localhost" || name.parse::<IpAddr>().is_ok()
}

impl Hub {
    fn with(&self, ctx: &Context, f: impl FnOnce(&mut View)) {
        if let Some(view) = self.sessions.lock().unwrap().get_mut(ctx.session_id()) {
            f(view);
        }
    }

    fn stores(&self) -> Vec<Store> {
        self.stores.lock().unwrap().clone()
    }
}

impl Config {
    /// `web.json` in `dir`; missing is the default.
    pub fn load(dir: &std::path::Path) -> Result<Self, String> {
        let path = dir.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

impl WebUi {
    /// From `web.json` in rho's config dir; `RHO_WEB_UI` overrides `listen`.
    pub fn from_config() -> Self {
        match Config::load(&rho::prompt::config_dir()) {
            Ok(mut config) => {
                if let Ok(listen) = std::env::var("RHO_WEB_UI") {
                    config.listen = Some(listen);
                }
                Self::new(config)
            }
            Err(e) => Self {
                problem: Some(e),
                ..Self::new(Config::default())
            },
        }
    }

    pub fn new(config: Config) -> Self {
        Self {
            listen: config.listen.unwrap_or_else(|| DEFAULT_ADDR.into()),
            problem: None,
            hub: Arc::new(Hub {
                password: config.password.filter(|p| !p.is_empty()),
                sessions: Mutex::default(),
                stores: Mutex::default(),
            }),
            addr: OnceLock::new(),
        }
    }

    /// Listens on `listen` (`ip:port`, or `off`), without a password.
    #[cfg(test)]
    fn listen(listen: impl Into<String>) -> Self {
        Self::new(Config {
            listen: Some(listen.into()),
            password: None,
        })
    }

    /// Starts the server on first use; the address or why there is none.
    fn serve(&self) -> &Result<SocketAddr, String> {
        self.addr.get_or_init(|| {
            if let Some(problem) = &self.problem {
                return Err(problem.clone());
            }
            let want = &self.listen;
            if want == "off" {
                return Err("disabled (RHO_WEB_UI=off)".into());
            }
            let addr: SocketAddr = want
                .parse()
                .map_err(|e| format!("RHO_WEB_UI={want}: {e}"))?;
            let listener = (0..PORT_TRIES)
                .find_map(|i| {
                    let port = addr.port().checked_add(i)?;
                    std::net::TcpListener::bind((addr.ip(), port)).ok()
                })
                .ok_or_else(|| format!("no free port from {addr} on"))?;
            let addr = listener.local_addr().map_err(|e| e.to_string())?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            let listener =
                tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?;
            let app = router(self.hub.clone());
            let app = app.into_make_service_with_connect_info::<SocketAddr>();
            tokio::spawn(async move { axum::serve(listener, app).await });
            Ok(addr)
        })
    }

    /// The link for `/web`.
    fn link(&self, session_id: &str) -> String {
        let addr = match self.serve() {
            Ok(addr) => *addr,
            Err(e) => return format!("No web view: {e}"),
        };
        if !addr.ip().is_unspecified() {
            return format!("Web view: http://{addr}/#{session_id}");
        }
        let other = if self.hub.password.is_some() {
            "From another device, use this machine's address instead of 127.0.0.1 and the password from web.json."
        } else {
            "Other devices need a password: set one in ~/.config/rho/web.json."
        };
        format!(
            "Web view: http://127.0.0.1:{}/#{session_id}\n\n{other}",
            addr.port()
        )
    }
}

impl Extension for WebUi {
    async fn session_start(&self, session: &mut SessionSetup) {
        let _ = self.serve();
        session.commands.push(SlashCommand {
            name: "web".into(),
            description: "Show the link to the web view".into(),
            hint: None,
        });
        let ctx = session.ctx.clone();
        if let Some(store) = ctx.store() {
            let mut stores = self.hub.stores.lock().unwrap();
            if !stores.iter().any(|s| s.path() == store.path()) {
                stores.push(store.clone());
            }
        }
        let mut log = Timeline::default();
        if session.resumed
            && let Some(store) = ctx.store()
            && let Ok(history) = store.history(ctx.session_id()).await
        {
            log = Timeline::from_history(&history);
        }
        log.updated = now_ms();
        let view = View {
            ctx: ctx.clone(),
            log,
            running: false,
            context: 0,
        };
        self.hub
            .sessions
            .lock()
            .unwrap()
            .insert(ctx.session_id().into(), view);
    }

    fn session_end(&self, ctx: &Context) {
        self.hub.sessions.lock().unwrap().remove(ctx.session_id());
    }

    async fn before_request(&self, ctx: &Context, request: &mut ModelRequest) {
        self.hub.with(ctx, |v| {
            v.log.turn += 1;
            v.running = true;
            // New since the last request: the prompt, or tool results with
            // queued messages and notes.
            if let Some(last) = request.messages.last() {
                v.log.push_user(&last.content, false);
            }
        });
    }

    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        self.hub.with(ctx, |v| {
            v.log.push(
                Slot::Tool,
                json!({"kind": "tool", "id": call.id, "name": call.name, "input": call.input}),
            )
        });
        Decision::Allow
    }

    async fn tool_result(&self, ctx: &Context, call: &ToolCall, result: &mut ToolOutput) {
        self.hub.with(ctx, |v| {
            let mut end = tool_end(&call.id, &result.output, result.is_error);
            // Calls blocked before the hooks ran have no start event.
            end["name"] = call.name.clone().into();
            end["input"] = call.input.clone();
            v.log.push(Slot::Tool, end);
        });
    }

    async fn turn_end(&self, ctx: &Context, turn: &TurnEnd<'_>) {
        self.hub.with(ctx, |v| {
            v.context = turn.usage.prompt() + turn.usage.output;
            v.log.push_model(turn.assistant, false);
        });
    }

    async fn before_settle(&self, ctx: &Context, _settle: &Settle<'_>) -> Option<String> {
        // Another extension may still continue the run; its next request
        // sets this again.
        self.hub.with(ctx, |v| {
            v.running = false;
            v.log.updated = now_ms();
        });
        None
    }

    async fn command(&self, ctx: &Context, name: &str, _args: &str) -> Option<String> {
        (name == "web").then(|| self.link(ctx.session_id()))
    }
}

fn router(hub: Arc<Hub>) -> Router {
    let password = hub.password.clone();
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/sessions", get(list))
        .route("/api/sessions/{id}", get(events))
        .route("/api/sessions/{id}/send", post(send))
        .layer(axum::middleware::from_fn(
            move |req: Request, next: Next| {
                let host_ok = req
                    .headers()
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .is_some_and(local_host);
                let client = req
                    .extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|c| c.0.ip());
                let given = req
                    .headers()
                    .get(PASSWORD_HEADER)
                    .and_then(|h| h.to_str().ok());
                let refused = if !host_ok {
                    Some((StatusCode::FORBIDDEN, "Unknown host."))
                } else if req.uri().path().starts_with("/api/") {
                    admit(client, given, password.as_deref())
                        .err()
                        .map(|e| (StatusCode::UNAUTHORIZED, e))
                } else {
                    None
                };
                async move {
                    match refused {
                        None => next.run(req).await,
                        Some(refusal) => refusal.into_response(),
                    }
                }
            },
        ))
        .with_state(hub)
}

/// Open sessions, then stored ones, newest first.
async fn list(State(hub): State<Arc<Hub>>) -> Json<Value> {
    let (mut list, mut seen) = {
        let sessions = hub.sessions.lock().unwrap();
        let list: Vec<Value> = sessions.iter().map(|(id, v)| v.summary(id)).collect();
        (list, sessions.keys().cloned().collect::<HashSet<_>>())
    };
    for store in hub.stores() {
        let Ok(page) = store.list(None, None, LIST_LIMIT).await else {
            continue;
        };
        for s in page.sessions {
            if seen.insert(s.id.clone()) {
                list.push(stored_summary(&s));
            }
        }
    }
    list.sort_by_key(|s| std::cmp::Reverse(s["updated"].as_u64()));
    list.truncate(LIST_LIMIT);
    Json(list.into())
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: usize,
}

async fn events(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Query(q): Query<Since>,
) -> Response {
    let open = {
        let sessions = hub.sessions.lock().unwrap();
        sessions
            .get(&id)
            .map(|v| v.log.page(v.summary(&id), q.since))
    };
    if let Some(page) = open {
        return Json(page).into_response();
    }
    for store in hub.stores() {
        if let Ok(Some(info)) = store.session(&id).await {
            let history = store.history(&id).await.unwrap_or_default();
            let log = Timeline::from_history(&history);
            return Json(log.page(stored_summary(&info), q.since)).into_response();
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

#[derive(Deserialize)]
struct Send {
    text: String,
}

async fn send(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Json(body): Json<Send>,
) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message").into_response();
    }
    let ctx = {
        let mut sessions = hub.sessions.lock().unwrap();
        let Some(v) = sessions.get_mut(&id) else {
            return (
                StatusCode::CONFLICT,
                "This session is not open in this rho; open it in your editor to continue it.",
            )
                .into_response();
        };
        v.log.queued.push(text.clone());
        v.log.updated = now_ms();
        v.ctx.clone()
    };
    // So the editor shows where the next run comes from.
    ctx.notify("Message from the web view", &text, false);
    ctx.send(text, true);
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use rho::agent::{Agent, AgentEvent};
    use rho::extension::{Frontend, NoFrontend};
    use rho::provider::{Event, Provider, StopReason};

    /// Replays one scripted response per request.
    struct Scripted(Mutex<Vec<Vec<Event>>>);

    impl Provider for Scripted {
        fn stream(&self, _request: ModelRequest) -> BoxStream<'static, anyhow::Result<Event>> {
            let events = self.0.lock().unwrap().remove(0);
            stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }

    /// Records the notices the editor would show.
    #[derive(Default)]
    struct Notices(Mutex<Vec<String>>);

    impl Frontend for Notices {
        fn event(&self, ev: AgentEvent<'_>) {
            if let AgentEvent::Notice { title, text, .. } = ev {
                self.0.lock().unwrap().push(format!("{title}: {text}"));
            }
        }
    }

    fn kinds(events: &Value) -> Vec<String> {
        let mut events: Vec<&Value> = events.as_array().unwrap().iter().collect();
        // As the page orders them.
        events.sort_by_key(|e| (e["turn"].as_u64(), e["slot"].as_u64(), e["seq"].as_u64()));
        // The page shows a result in its call.
        let calls: Vec<&Value> = events
            .iter()
            .filter(|e| e["kind"] == "tool")
            .map(|e| &e["id"])
            .collect();
        events
            .iter()
            .filter(|e| !(e["kind"] == "tool_end" && calls.contains(&&e["id"])))
            .map(|e| {
                let detail = e["text"]
                    .as_str()
                    .or(e["name"].as_str())
                    .unwrap_or_default();
                format!("{} {detail}", e["kind"].as_str().unwrap())
            })
            .collect()
    }

    /// An HTTP client for the web view of `web`; from this machine, so
    /// without a password.
    fn client(web: &WebUi) -> (String, reqwest::Client) {
        let addr = *web.addr.get().unwrap().as_ref().unwrap();
        (format!("http://{addr}"), reqwest::Client::new())
    }

    async fn get_json(http: &reqwest::Client, url: String) -> Value {
        http.get(url).send().await.unwrap().json().await.unwrap()
    }

    #[tokio::test]
    async fn follows_a_run_and_queues_messages() {
        let provider = Arc::new(Scripted(Mutex::new(vec![
            vec![
                Event::TextDelta("Let me look.".into()),
                Event::Block(Content::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "echo hi"}),
                }),
                Event::Done(StopReason::ToolUse),
            ],
            vec![
                Event::TextDelta("Done.".into()),
                Event::Done(StopReason::EndTurn),
            ],
            vec![
                Event::TextDelta("Got it.".into()),
                Event::Done(StopReason::EndTurn),
            ],
        ])));
        let web = Arc::new(WebUi::listen("127.0.0.1:0"));
        let mut agent = Agent::new(provider, "m", rho::tools::builtin());
        agent.extensions.push(web.clone());
        let dir = tempfile::tempdir().unwrap();
        let notices = Arc::new(Notices::default());
        let mut session = agent
            .start_session("s1", dir.path(), vec![], notices.clone())
            .await
            .unwrap();
        agent
            .run(&mut session, vec![Content::text("hi")])
            .await
            .unwrap();

        let (base, http) = client(&web);
        let list = get_json(&http, format!("{base}/api/sessions")).await;
        assert_eq!(list[0]["id"], "s1");
        assert_eq!(list[0]["profile"], "Write");
        assert_eq!(list[0]["open"], true);
        let s = get_json(&http, format!("{base}/api/sessions/s1")).await;
        assert_eq!(s["running"], false);
        assert_eq!(
            kinds(&s["events"]),
            [
                "user hi",
                "assistant Let me look.",
                "tool Bash",
                "assistant Done."
            ]
        );
        let end = s["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "tool_end")
            .unwrap();
        assert_eq!(end["output"], "hi\n");
        let page = http.get(&base).send().await.unwrap().text().await.unwrap();
        assert!(page.contains("<title>rho</title>"));
        let since = get_json(&http, format!("{base}/api/sessions/s1?since=5")).await;
        assert_eq!(
            (since["events"].clone(), since["total"].clone()),
            (json!([]), json!(5))
        );
        let missing = http.get(format!("{base}/api/sessions/nope")).send().await;
        assert_eq!(missing.unwrap().status(), 404);

        // Other hosts (DNS rebinding) and non-JSON posts: refused.
        let foreign = http
            .get(format!("{base}/api/sessions"))
            .header(header::HOST, "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), 403);
        let send = format!("{base}/api/sessions/s1/send");
        let form = http
            .post(&send)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(r#"{"text": "x"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(form.status(), 415);
        assert!(!session.ctx.has_triggered());

        // Queued while idle: the editor is told, and it starts a run (ACP
        // does this in the background).
        let sent = http.post(&send).json(&json!({"text": "more"})).send().await;
        assert_eq!(sent.unwrap().status(), 204);
        assert_eq!(
            *notices.0.lock().unwrap(),
            ["Message from the web view: more"]
        );
        let s = get_json(&http, format!("{base}/api/sessions/s1")).await;
        assert_eq!(s["queued"], json!(["more"]));
        assert!(session.ctx.has_triggered());
        agent.run_triggered(&mut session).await.unwrap().unwrap();
        let s = get_json(&http, format!("{base}/api/sessions/s1")).await;
        assert_eq!(s["queued"], json!([]));
        let kinds = kinds(&s["events"]);
        assert_eq!(kinds[4..], ["user more", "assistant Got it."]);

        drop(session);
        let list = get_json(&http, format!("{base}/api/sessions")).await;
        assert_eq!(list, json!([]));
    }

    #[tokio::test]
    async fn stored_sessions_are_listed_and_shown() {
        let provider = Arc::new(Scripted(Mutex::new(vec![
            vec![
                Event::Block(Content::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "false"}),
                }),
                Event::Done(StopReason::ToolUse),
            ],
            vec![
                Event::TextDelta("It failed.".into()),
                Event::Done(StopReason::EndTurn),
            ],
        ])));
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let web = Arc::new(WebUi::listen("127.0.0.1:0"));
        let mut agent = Agent::new(provider, "m", rho::tools::builtin());
        agent.extensions.push(web.clone());
        agent.sessions_db = Some(Arc::new(move |_| db.clone()));
        let mut session = agent
            .start_session("s1", dir.path(), vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        agent
            .run(&mut session, vec![Content::text("try")])
            .await
            .unwrap();
        drop(session);
        let open = agent
            .start_session("s2", dir.path(), vec![], Arc::new(NoFrontend))
            .await
            .unwrap();

        // Closed, but stored: listed after the newer open one, read-only.
        let (base, http) = client(&web);
        let list = get_json(&http, format!("{base}/api/sessions")).await;
        let ids: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["id"].clone(), s["open"].clone()))
            .collect();
        assert_eq!(
            ids,
            [(json!("s2"), json!(true)), (json!("s1"), json!(false))]
        );
        let s = get_json(&http, format!("{base}/api/sessions/s1")).await;
        assert_eq!(s["profile"], "Write");
        assert_eq!(
            kinds(&s["events"]),
            ["user try", "tool Bash", "assistant It failed."]
        );
        assert_eq!(s["events"][2]["is_error"], true);
        let sent = http
            .post(format!("{base}/api/sessions/s1/send"))
            .json(&json!({"text": "hello?"}))
            .send()
            .await
            .unwrap();
        assert_eq!(sent.status(), 409);
        drop(open);

        // Loaded again: the same history, now live.
        let _session = agent
            .resume_session("s1", dir.path(), vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        let s = get_json(&http, format!("{base}/api/sessions/s1")).await;
        assert_eq!(s["open"], true);
        assert_eq!(
            kinds(&s["events"]),
            ["user try", "tool Bash", "assistant It failed."]
        );
    }

    #[test]
    fn hosts_must_be_local_names_or_addresses() {
        for ok in [
            "localhost:8787",
            "127.0.0.1:8787",
            "192.168.1.20:8787",
            "[::1]:8787",
            "10.0.0.2",
        ] {
            assert!(local_host(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:8787",
            "localhost.evil.example:8787",
        ] {
            assert!(!local_host(bad), "{bad}");
        }
    }

    #[test]
    fn other_machines_need_the_password() {
        let (local, lan) = (
            Some("127.0.0.1".parse().unwrap()),
            Some("192.168.1.5".parse().unwrap()),
        );
        let mapped = Some("::ffff:127.0.0.1".parse().unwrap());
        assert!(admit(local, None, None).is_ok());
        assert!(admit(mapped, None, Some("pw")).is_ok());
        assert!(
            admit(lan, Some("pw"), None)
                .unwrap_err()
                .contains("web.json")
        );
        assert!(admit(lan, None, Some("pw")).is_err());
        assert!(admit(lan, Some("nope"), Some("pw")).is_err());
        assert!(admit(lan, Some("pw"), Some("pw")).is_ok());
        assert!(admit(None, None, Some("pw")).is_err());
    }

    #[tokio::test]
    async fn config_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(dir.path()).unwrap().password.is_none());
        let file = dir.path().join(CONFIG_FILE);
        std::fs::write(&file, r#"{"listen": "127.0.0.1:0", "password": "pw"}"#).unwrap();
        let web = WebUi::new(Config::load(dir.path()).unwrap());
        assert_eq!(web.hub.password.as_deref(), Some("pw"));
        assert!(web.link("s").starts_with("Web view: http://127.0.0.1:"));
        std::fs::write(&file, r#"{"pasword": "typo"}"#).unwrap();
        assert!(
            Config::load(dir.path())
                .unwrap_err()
                .contains("unknown field")
        );
    }

    #[test]
    fn can_be_turned_off() {
        let web = WebUi::listen("off");
        assert!(web.link("s").contains("disabled"));
    }
}
