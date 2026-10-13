#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use rho::agent::{Agent, AgentEvent, Session};
use rho::extension::Frontend;
use rho::provider::{Event, Provider, Request};
use rho::store::SessionId;

/// Replays one scripted response per request and records the requests.
#[derive(Default)]
pub struct Scripted {
    pub responses: Mutex<Vec<Vec<Event>>>,
    pub requests: Mutex<Vec<Request>>,
    /// `{system, messages}` of each request, after the `before_send` hooks.
    pub bodies: Mutex<Vec<serde_json::Value>>,
}

impl Scripted {
    pub fn new(mut responses: Vec<Vec<Event>>) -> Arc<Self> {
        responses.reverse();
        Arc::new(Self {
            responses: Mutex::new(responses),
            ..Default::default()
        })
    }
}

impl Provider for Scripted {
    fn stream(
        &self,
        request: Request,
        before_send: &dyn Fn(&mut serde_json::Value),
    ) -> BoxStream<'static, Result<Event>> {
        let mut body = serde_json::json!({"system": request.system, "messages": request.messages});
        before_send(&mut body);
        self.bodies.lock().unwrap().push(body);
        self.requests.lock().unwrap().push(request);
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .expect("unexpected request");
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
}

pub fn text(t: &str) -> Event {
    Event::TextDelta(t.into())
}

/// Records every event as its Debug string.
#[derive(Default)]
pub struct Log(pub Mutex<Vec<String>>);

impl Log {
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Frontend for Log {
    fn event(&self, ev: AgentEvent<'_>) {
        self.0.lock().unwrap().push(format!("{ev:?}"));
    }
}

pub async fn session(agent: &Agent, cwd: &std::path::Path) -> (Session, Arc<Log>) {
    let log = Arc::new(Log::default());
    (
        agent
            .start_session(SessionId::new("test"), cwd, vec![], log.clone())
            .await
            .unwrap(),
        log,
    )
}
