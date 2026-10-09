//! A small web page to follow this process's sessions and queue messages
//! for them, built only on extension hooks.
//!
//! What it sees: every user-side message as it goes to the model (prompts,
//! queued and steering messages, profile notes; from `before_request`), tool
//! calls as they start and end, and the model's text and thinking once its
//! turn is done (`turn_end`; hooks do not see the stream). A message sent
//! from the page goes to `Context::send` with `trigger_turn`: during a run it
//! rides along with the next tool results, when idle it starts a run.
//!
//! Listens on `RHO_WEB_UI` (default `127.0.0.1:8787`, `off` to disable); if
//! the port is taken (another rho), the next free one of the following ten.
//! `/web` in a session shows its address. Only requests with a localhost
//! `Host` header and, for sending, a JSON body are accepted, so other web
//! sites cannot send prompts (DNS rebinding, cross-site form posts).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rho::extension::{
    Context, Decision, Extension, SessionSetup, Settle, SlashCommand, ToolCall, ToolOutput, TurnEnd,
};
use rho::provider::{Content, Message, Request as ModelRequest, Role};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_ADDR: &str = "127.0.0.1:8787";
const PORT_TRIES: u16 = 10;
/// Tool output kept per call; the page is for following along.
const MAX_OUTPUT: usize = 4000;
const PAGE: &str = include_str!("web_ui.html");

pub struct WebUi {
    /// `ip:port` to listen on, or `off`.
    listen: String,
    hub: Arc<Hub>,
    /// Where the page is served, once the first session started it.
    addr: OnceLock<Result<SocketAddr, String>>,
}

#[derive(Default)]
struct Hub {
    sessions: Mutex<BTreeMap<String, View>>,
}

/// What the page shows of one session.
struct View {
    ctx: Context,
    /// Append-only; the page asks for those after the last one it has.
    events: Vec<Value>,
    /// Model requests so far. Events carry theirs, so the page can put the
    /// model's text (known at the turn's end) before its tool calls.
    turn: u64,
    running: bool,
    /// Unix ms of the last event.
    updated: u64,
    /// Context in use after the last request, in tokens.
    context: u64,
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

impl View {
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

    fn summary(&self, id: &str) -> Value {
        json!({
            "id": id,
            "title": self.ctx.title(),
            "cwd": self.ctx.cwd(),
            "profile": self.ctx.profile().name,
            "running": self.running,
            "updated": self.updated,
            "context": self.context,
            "queued": self.queued,
        })
    }
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

impl Hub {
    fn with(&self, ctx: &Context, f: impl FnOnce(&mut View)) {
        if let Some(view) = self.sessions.lock().unwrap().get_mut(ctx.session_id()) {
            f(view);
        }
    }
}

impl WebUi {
    /// Listens on `RHO_WEB_UI`, default `127.0.0.1:8787`.
    pub fn from_env() -> Self {
        Self::listen(std::env::var("RHO_WEB_UI").unwrap_or_else(|_| DEFAULT_ADDR.into()))
    }

    /// Listens on `listen` (`ip:port`, or `off`) once the first session starts.
    pub fn listen(listen: impl Into<String>) -> Self {
        Self {
            listen: listen.into(),
            hub: Arc::default(),
            addr: OnceLock::new(),
        }
    }

    /// Starts the server on first use; the address or why there is none.
    fn serve(&self) -> &Result<SocketAddr, String> {
        self.addr.get_or_init(|| {
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
            let app = router(self.hub.clone(), addr.port());
            tokio::spawn(async move { axum::serve(listener, app).await });
            Ok(addr)
        })
    }
}

impl Extension for WebUi {
    async fn session_start(&self, session: &mut SessionSetup) {
        let _ = self.serve();
        session.commands.push(SlashCommand {
            name: "web".into(),
            description: "Show the address of the web view".into(),
            hint: None,
        });
        let ctx = session.ctx.clone();
        let mut view = View {
            ctx: ctx.clone(),
            events: vec![],
            turn: 0,
            running: false,
            updated: now_ms(),
            context: 0,
            queued: vec![],
        };
        if session.resumed
            && let Some(store) = ctx.store()
            && let Ok(history) = store.history(ctx.session_id()).await
        {
            for Message { role, content } in &history {
                // Like live: a user message starts a turn, the model's
                // answer belongs to it. A tool result shows with its call.
                match role {
                    Role::User => {
                        view.turn += 1;
                        view.push_user(content, true);
                    }
                    Role::Assistant => view.push_model(content, true),
                }
            }
        }
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
            v.turn += 1;
            v.running = true;
            // New since the last request: the prompt, or tool results with
            // queued messages and notes.
            if let Some(last) = request.messages.last() {
                v.push_user(&last.content, false);
            }
        });
    }

    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        self.hub.with(ctx, |v| {
            v.push(
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
            v.push(Slot::Tool, end);
        });
    }

    async fn turn_end(&self, ctx: &Context, turn: &TurnEnd<'_>) {
        self.hub.with(ctx, |v| {
            v.context = turn.usage.prompt() + turn.usage.output;
            v.push_model(turn.assistant, false);
        });
    }

    async fn before_settle(&self, ctx: &Context, _settle: &Settle<'_>) -> Option<String> {
        // Another extension may still continue the run; its next request
        // sets this again.
        self.hub.with(ctx, |v| {
            v.running = false;
            v.updated = now_ms();
        });
        None
    }

    async fn command(&self, ctx: &Context, name: &str, _args: &str) -> Option<String> {
        if name != "web" {
            return None;
        }
        Some(match self.serve() {
            Ok(addr) => format!("Web view: http://{addr}/#{}", ctx.session_id()),
            Err(e) => format!("No web view: {e}"),
        })
    }
}

fn router(hub: Arc<Hub>, port: u16) -> Router {
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/sessions", get(list))
        .route("/api/sessions/{id}", get(events))
        .route("/api/sessions/{id}/send", post(send))
        .layer(axum::middleware::from_fn(
            move |req: Request, next: Next| {
                let ok = req
                    .headers()
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .is_some_and(|h| allowed.iter().any(|a| a == h));
                async move {
                    if ok {
                        next.run(req).await
                    } else {
                        StatusCode::FORBIDDEN.into_response()
                    }
                }
            },
        ))
        .with_state(hub)
}

async fn list(State(hub): State<Arc<Hub>>) -> Json<Value> {
    let sessions = hub.sessions.lock().unwrap();
    let mut list: Vec<Value> = sessions.iter().map(|(id, v)| v.summary(id)).collect();
    list.sort_by_key(|s| std::cmp::Reverse(s["updated"].as_u64()));
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
    let sessions = hub.sessions.lock().unwrap();
    let Some(v) = sessions.get(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut out = v.summary(&id);
    out["events"] = v.events.get(q.since..).unwrap_or_default().into();
    Json(out).into_response()
}

#[derive(Deserialize)]
struct Send {
    text: String,
}

/// `Json` insists on `application/json`, which a cross-site form cannot send.
async fn send(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Json(body): Json<Send>,
) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message").into_response();
    }
    let mut sessions = hub.sessions.lock().unwrap();
    let Some(v) = sessions.get_mut(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    v.queued.push(text.clone());
    v.updated = now_ms();
    v.ctx.send(text, true);
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use rho::agent::Agent;
    use rho::extension::NoFrontend;
    use rho::provider::{Event, Provider, StopReason};

    /// Replays one scripted response per request.
    struct Scripted(Mutex<Vec<Vec<Event>>>);

    impl Provider for Scripted {
        fn stream(&self, _request: ModelRequest) -> BoxStream<'static, anyhow::Result<Event>> {
            let events = self.0.lock().unwrap().remove(0);
            stream::iter(events.into_iter().map(Ok)).boxed()
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
        let mut session = agent
            .start_session("s1", dir.path(), vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        agent
            .run(&mut session, vec![Content::text("hi")])
            .await
            .unwrap();

        let addr = *web.addr.get().unwrap().as_ref().unwrap();
        let http = reqwest::Client::new();
        let get = |path: &str| {
            let req = http.get(format!("http://{addr}{path}"));
            async move { req.send().await.unwrap() }
        };
        let list: Value = get("/api/sessions").await.json().await.unwrap();
        assert_eq!(list[0]["id"], "s1");
        assert_eq!(list[0]["profile"], "Write");
        let s: Value = get("/api/sessions/s1").await.json().await.unwrap();
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
        let page = get("/").await.text().await.unwrap();
        assert!(page.contains("<title>rho</title>"));
        let since: Value = get("/api/sessions/s1?since=5").await.json().await.unwrap();
        assert_eq!(since["events"], json!([]));
        assert_eq!(get("/api/sessions/nope").await.status(), 404);

        // Other hosts (DNS rebinding) and non-JSON posts (cross-site forms) are refused.
        let foreign = http
            .get(format!("http://{addr}/api/sessions"))
            .header(header::HOST, "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), 403);
        let send = format!("http://{addr}/api/sessions/s1/send");
        let form = http
            .post(&send)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(r#"{"text": "x"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(form.status(), 415);
        assert!(!session.ctx.has_triggered());

        // Queued while idle: it starts a run (ACP does this in the background).
        let sent = http.post(&send).json(&json!({"text": "more"})).send().await;
        assert_eq!(sent.unwrap().status(), 204);
        let s: Value = get("/api/sessions/s1").await.json().await.unwrap();
        assert_eq!(s["queued"], json!(["more"]));
        assert!(session.ctx.has_triggered());
        agent.run_triggered(&mut session).await.unwrap().unwrap();
        let s: Value = get("/api/sessions/s1").await.json().await.unwrap();
        assert_eq!(s["queued"], json!([]));
        let kinds = kinds(&s["events"]);
        assert_eq!(kinds[4..], ["user more", "assistant Got it."]);

        drop(session);
        let list: Value = get("/api/sessions").await.json().await.unwrap();
        assert_eq!(list, json!([]));
    }

    #[tokio::test]
    async fn resumed_sessions_show_their_history() {
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

        let _session = agent
            .resume_session("s1", dir.path(), vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        let hub = web.hub.sessions.lock().unwrap();
        let events = Value::from(hub["s1"].events.clone());
        assert_eq!(
            kinds(&events),
            ["user try", "tool Bash", "assistant It failed."]
        );
        assert_eq!(events[2]["is_error"], true);
    }

    #[test]
    fn can_be_turned_off() {
        let web = WebUi::listen("off");
        assert!(web.serve().as_ref().unwrap_err().contains("disabled"));
    }
}
