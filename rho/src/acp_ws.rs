//! ACP over WebSocket (`rho acp --listen ADDR`): one JSON-RPC message per
//! text frame, as acp-ui sends it. Every connection is its own ACP
//! connection to the shared agent, like a separate `rho acp` on stdio.
//!
//! Every client needs the token, also from this machine: any website can
//! open a WebSocket to localhost. It comes as `Authorization: Bearer <token>`
//! or, from browsers (which cannot set headers), as a `bearer.<token>`
//! subprotocol next to `acp.v1`.

use std::io;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::{ConnectTo, Lines};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::{SinkExt, StreamExt, future, stream};
use sha2::{Digest, Sha256};

use crate::agent::Agent;

/// In the config dir, unless `RHO_ACP_TOKEN` is set.
pub const TOKEN_FILE: &str = "acp-token";
const SUBPROTOCOL: &str = "acp.v1";
const BEARER: &str = "bearer.";

struct Server {
    agent: Arc<Agent>,
    token: String,
}

/// Serves ACP on `/` until the listener fails.
pub async fn serve(
    agent: Arc<Agent>,
    listener: tokio::net::TcpListener,
    token: String,
) -> Result<()> {
    let app = Router::new()
        .route("/", get(upgrade))
        .with_state(Arc::new(Server { agent, token }));
    axum::serve(listener, app).await.context("acp websocket")
}

/// `RHO_ACP_TOKEN`, else the token file, created with a random token.
pub fn token(config_dir: &Path) -> Result<String> {
    if let Ok(t) = std::env::var("RHO_ACP_TOKEN")
        && !t.is_empty()
    {
        return Ok(t);
    }
    let path = config_dir.join(TOKEN_FILE);
    match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_string()),
        Ok(_) => anyhow::bail!("{} is empty", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    std::fs::create_dir_all(config_dir)?;
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(token.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(token)
}

async fn upgrade(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorized(&headers, &server.token) {
        eprintln!("rho acp: rejected connection, missing or wrong token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Only `acp.v1` is echoed, never the token.
    ws.protocols([SUBPROTOCOL])
        .on_upgrade(async move |socket| {
            eprintln!("rho acp: connection opened");
            let acp = crate::acp::agent(server.agent.clone());
            match acp.connect_to(lines(socket)).await {
                Ok(()) => eprintln!("rho acp: connection closed"),
                Err(e) => eprintln!("rho acp: connection ended: {e}"),
            }
        })
        .into_response()
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let protocols = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|p| p.trim().strip_prefix(BEARER));
    // Comparing digests, so the time taken says nothing about the token.
    let want = Sha256::digest(token);
    bearer
        .into_iter()
        .chain(protocols)
        .any(|t| Sha256::digest(t.trim()) == want)
}

/// The socket as JSON-RPC lines. A frame may hold several lines (stdio
/// bridges forward stdout chunks as they come).
fn lines(
    socket: WebSocket,
) -> Lines<
    impl futures::Sink<String, Error = io::Error> + Send + 'static,
    impl futures::Stream<Item = io::Result<String>> + Send + 'static,
> {
    let (tx, rx) = socket.split();
    let tx = tx
        .with(|line: String| future::ready(Ok::<_, axum::Error>(Message::Text(line.into()))))
        .sink_map_err(io::Error::other);
    let rx = rx.flat_map(|frame| {
        stream::iter(match frame {
            Ok(Message::Text(text)) => text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| Ok(l.to_string()))
                .collect(),
            Ok(Message::Binary(_)) => vec![Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "binary frames are not part of ACP",
            ))],
            // Ping, pong and close are handled by the socket.
            Ok(_) => vec![],
            Err(e) => vec![Err(io::Error::other(e))],
        })
    });
    Lines::new(tx, rx)
}
