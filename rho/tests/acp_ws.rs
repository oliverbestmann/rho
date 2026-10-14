//! ACP over WebSocket: the token is required, browsers send it as a
//! subprotocol, and a frame may carry several messages.

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use rho::agent::Agent;
use rho::provider::ModelSpec;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

mod common;

async fn server() -> String {
    let agent = Agent::new(
        common::Scripted::new(vec![]),
        ModelSpec::new("test", "m"),
        vec![],
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    tokio::spawn(rho::acp_ws::serve(
        Arc::new(agent),
        listener,
        "secret".into(),
    ));
    url
}

fn initialize(id: u64) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"protocolVersion": 1}})
        .to_string()
}

#[tokio::test]
async fn needs_the_token() {
    let url = server().await;
    let mut wrong = url.as_str().into_client_request().unwrap();
    wrong
        .headers_mut()
        .insert("Authorization", "Bearer nope".parse().unwrap());
    for request in [url.as_str().into_client_request().unwrap(), wrong] {
        let err = tokio_tungstenite::connect_async(request).await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    let mut header = url.as_str().into_client_request().unwrap();
    header
        .headers_mut()
        .insert("Authorization", "Bearer secret".parse().unwrap());
    tokio_tungstenite::connect_async(header).await.unwrap();
}

#[tokio::test]
async fn browser_style_subprotocol_and_batched_frames() {
    let url = server().await;
    let mut request = url.as_str().into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "acp.v1, bearer.secret".parse().unwrap(),
    );
    let (mut ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "acp.v1");

    // Two messages in one frame, as stdio bridges forward them.
    let frame = format!("{}\n{}\n", initialize(1), initialize(2));
    ws.send(Message::text(frame)).await.unwrap();
    let mut ids = vec![];
    while ids.len() < 2 {
        let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
            continue;
        };
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["result"]["agentInfo"]["name"], "rho");
        ids.push(reply["id"].as_u64().unwrap());
    }
    ids.sort();
    assert_eq!(ids, [1, 2]);
}
