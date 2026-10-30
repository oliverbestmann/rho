//! The OpenAI-compatible endpoints: off by default, need the token, list
//! only allowed models, answer with JSON or server-sent chunks.

use std::sync::Arc;

use rho::agent::Agent;
use rho::provider::{
    Event, ModelId, ModelInfo, ModelSpec, Provider, ProviderName, Request, StopReason, Usage,
};
use serde_json::{Value, json};

mod common;

/// Scripted, plus two models to list.
struct Listed(Arc<common::Scripted>);

impl Provider for Listed {
    fn stream(
        &self,
        request: Request,
        before_send: &dyn Fn(&mut Value),
    ) -> futures::stream::BoxStream<'static, anyhow::Result<Event>> {
        self.0.stream(request, before_send)
    }

    fn models(&self) -> Vec<ModelInfo> {
        ["m", "n"]
            .map(|id| ModelInfo {
                id: ModelId::new(id),
                name: id.into(),
                context_window: 1000,
                price: None,
                price_above: None,
            })
            .into()
    }
}

fn reply(text: &str) -> Vec<Event> {
    vec![
        common::text(text),
        common::text("!"),
        Event::Usage(Usage {
            input: 3,
            output: 2,
            cache_read: 1,
            cache_write: 5,
        }),
        Event::Done(StopReason::EndTurn),
    ]
}

async fn server(api: Value, responses: Vec<Vec<Event>>) -> (String, Arc<common::Scripted>) {
    let scripted = common::Scripted::new(responses);
    let provider = Arc::new(Listed(scripted.clone()));
    let mut agent = Agent::new(provider.clone(), ModelSpec::new("test", "m"));
    agent.registry.register(ProviderName::new("test"), provider);
    agent.api = serde_json::from_value(api).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(rho::acp_ws::serve(
        Arc::new(agent),
        Arc::default(),
        listener,
        Some("secret".into()),
        None,
    ));
    (url, scripted)
}

fn enabled() -> Value {
    json!({"enabled": true, "models": ["test/m"]})
}

#[tokio::test]
async fn off_by_default_and_needs_the_token() {
    let http = reqwest::Client::new();
    let (url, _) = server(json!({}), vec![]).await;
    let off = http.get(format!("{url}/models")).bearer_auth("secret");
    assert_eq!(off.send().await.unwrap().status(), 404);

    let (url, _) = server(enabled(), vec![]).await;
    let models = format!("{url}/models");
    assert_eq!(http.get(&models).send().await.unwrap().status(), 401);
    let wrong = http.get(&models).bearer_auth("nope").send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    let list: Value = http
        .get(&models)
        .bearer_auth("secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["test/m"], "only allowed models");
}

#[tokio::test]
async fn own_token_replaces_the_acp_token() {
    let api = json!({"enabled": true, "models": ["*"], "token": "api-key"});
    let (url, _) = server(api, vec![]).await;
    let http = reqwest::Client::new();
    let status = |token: &'static str| {
        let request = http.get(format!("{url}/models")).bearer_auth(token);
        async move { request.send().await.unwrap().status() }
    };
    assert_eq!(status("secret").await, 401);
    assert_eq!(status("api-key").await, 200);
}

#[tokio::test]
async fn chat_completion() {
    let (url, scripted) = server(enabled(), vec![reply("hi")]).await;
    let http = reqwest::Client::new();
    let chat = |model: &str| {
        http.post(format!("{url}/chat/completions"))
            .bearer_auth("secret")
            .json(&json!({"model": model, "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hello"},
            ]}))
            .send()
    };
    let denied = chat("test/n").await.unwrap();
    assert_eq!(denied.status(), 404);
    let body: Value = denied.json().await.unwrap();
    assert_eq!(body["error"]["type"], "model_not_found");

    let body: Value = chat("test/m").await.unwrap().json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], "test/m");
    assert_eq!(
        body["choices"][0],
        json!({"index": 0, "message": {"role": "assistant", "content": "hi!"}, "finish_reason": "stop"})
    );
    assert_eq!(
        body["usage"],
        json!({"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11,
            "prompt_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 5}})
    );
    let request = scripted.requests.lock().unwrap().pop().unwrap();
    assert_eq!(
        (request.model.as_str(), request.system.as_str()),
        ("m", "sys")
    );
}

#[tokio::test]
async fn streamed_chat_completion() {
    let (url, _) = server(enabled(), vec![reply("hi")]).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/chat/completions"))
        .bearer_auth("secret")
        .json(&json!({
            "model": "test/m",
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "hello"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = response.text().await.unwrap();
    let events: Vec<&str> = text
        .split("\n\n")
        .filter(|e| !e.is_empty())
        .map(|e| e.strip_prefix("data: ").unwrap())
        .collect();
    assert_eq!(events.last(), Some(&"[DONE]"));
    let chunks: Vec<Value> = events[..events.len() - 1]
        .iter()
        .map(|e| serde_json::from_str(e).unwrap())
        .collect();
    let deltas: Vec<&Value> = chunks
        .iter()
        .filter_map(|c| c["choices"].get(0))
        .map(|c| &c["delta"])
        .collect();
    assert_eq!(
        deltas,
        [
            &json!({"role": "assistant", "content": ""}),
            &json!({"content": "hi"}),
            &json!({"content": "!"}),
            &json!({}),
        ]
    );
    let finish = &chunks[chunks.len() - 2]["choices"][0]["finish_reason"];
    assert_eq!(finish, "stop");
    let usage = &chunks.last().unwrap()["usage"];
    assert_eq!(usage["total_tokens"], 11);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 1);
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk")
    );
}
