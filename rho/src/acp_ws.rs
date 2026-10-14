//! ACP over WebSocket (`rho acp --listen ADDR`): one JSON-RPC message per
//! text frame, as acp-ui sends it. Every connection is its own ACP
//! connection to the shared agent, like a separate `rho acp` on stdio.
//!
//! Every client needs the token, also from this machine: any website can
//! open a WebSocket to localhost. It comes as `Authorization: Bearer <token>`
//! or, from browsers (which cannot set headers), as a `bearer.<token>`
//! subprotocol next to `acp.v1`.
//!
//! `rho daemon` serves it on a Unix socket in a private directory, without a
//! token, and `rho acp --daemon` pumps an editor's stdio into that socket.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{ConnectTo, Lines};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::Listener;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::{SinkExt, StreamExt, future, stream};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite;

use crate::acp::Hub;
use crate::agent::Agent;

/// In the config dir, unless `RHO_ACP_TOKEN` is set.
pub const TOKEN_FILE: &str = "acp-token";
const SUBPROTOCOL: &str = "acp.v1";
const BEARER: &str = "bearer.";

/// The daemon's directory in the config dir: socket, lock and log.
pub const DAEMON_DIR: &str = "daemon";
const SOCKET_FILE: &str = "acp.sock";
const LOCK_FILE: &str = "lock";
const LOG_FILE: &str = "log";

struct Server {
    agent: Arc<Agent>,
    hub: Arc<Hub>,
    /// `None` lets everyone in who can reach the listener.
    token: Option<String>,
}

/// Serves ACP on `/` until the listener fails. Connections share the open
/// sessions of `hub`.
pub async fn serve<L>(
    agent: Arc<Agent>,
    hub: Arc<Hub>,
    listener: L,
    token: Option<String>,
) -> Result<()>
where
    L: Listener,
    L::Addr: std::fmt::Debug,
{
    let app = Router::new()
        .route("/", get(upgrade))
        .with_state(Arc::new(Server { agent, hub, token }));
    axum::serve(listener, app).await.context("acp websocket")
}

/// The daemon's socket: listening, plus the lock that keeps it the only
/// daemon (hold it as long as the listener). `None` if another daemon runs.
pub fn bind_daemon(dir: &Path) -> Result<Option<(std::fs::File, tokio::net::UnixListener)>> {
    private_dir(dir)?;
    let lock_path = dir.join(LOCK_FILE);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("locking {}", lock_path.display()));
        }
    }
    // Left over from a daemon that died; we hold the lock, so nobody uses it.
    let socket = dir.join(SOCKET_FILE);
    match std::fs::remove_file(&socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", socket.display()));
        }
        _ => {}
    }
    let listener = tokio::net::UnixListener::bind(&socket)
        .with_context(|| format!("listening on {}", socket.display()))?;
    Ok(Some((lock, listener)))
}

/// Only the user may enter: the socket has no token.
fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", dir.display()))
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

/// `rho acp --daemon`: connects to the daemon in `dir`, starting it with
/// `daemon_args` if none answers, and pumps stdio through until either side
/// ends.
pub async fn connect_stdio(dir: &Path, daemon_args: &[String]) -> Result<()> {
    let socket = socket_path(dir);
    let stream = match tokio::net::UnixStream::connect(&socket).await {
        Ok(s) => {
            if !daemon_args.is_empty() {
                eprintln!("rho acp: daemon already running, ignoring {daemon_args:?}");
            }
            s
        }
        Err(_) => {
            start_daemon(dir, daemon_args)?;
            wait_for(&socket).await?
        }
    };
    let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
        .await
        .context("connecting to the rho daemon")?;
    pump(tokio::io::stdin(), tokio::io::stdout(), ws).await
}

fn start_daemon(dir: &Path, args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    private_dir(dir)?;
    let log_path = dir.join(LOG_FILE);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Not killed with the editor's process group.
        .process_group(0)
        .spawn()
        .context("starting the rho daemon")?;
    Ok(())
}

async fn wait_for(socket: &Path) -> Result<tokio::net::UnixStream> {
    let mut last = None;
    for _ in 0..50 {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last.unwrap()).with_context(|| format!("rho daemon did not answer on {}", socket.display()))
}

/// Lines from `input` go out as text frames; frames come in as lines on
/// `output`. Ends when either side does.
pub async fn pump<S>(
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    ws: tokio_tungstenite::WebSocketStream<S>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tungstenite::Message;
    let (mut tx, mut rx) = ws.split();
    let up = async {
        let mut lines = tokio::io::BufReader::new(input).lines();
        while let Some(line) = lines.next_line().await? {
            if !line.trim().is_empty() {
                tx.send(Message::text(line)).await?;
            }
        }
        tx.close().await?;
        anyhow::Ok(())
    };
    let down = async {
        while let Some(frame) = rx.next().await {
            match frame? {
                Message::Text(text) => {
                    output.write_all(text.as_bytes()).await?;
                    if !text.ends_with('\n') {
                        output.write_all(b"\n").await?;
                    }
                    output.flush().await?;
                }
                Message::Binary(_) => anyhow::bail!("binary frames are not part of ACP"),
                // The stream ends when the daemon drops the connection.
                _ => {}
            }
        }
        anyhow::Ok(())
    };
    // After our close, wait for the daemon's, so it sees a clean end.
    tokio::pin!(down);
    tokio::select! {
        r = &mut down => r,
        r = up => {
            r?;
            down.await
        }
    }
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
    if let Some(token) = &server.token
        && !authorized(&headers, token)
    {
        eprintln!("rho acp: rejected connection, missing or wrong token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Only `acp.v1` is echoed, never the token.
    ws.protocols([SUBPROTOCOL])
        .on_upgrade(async move |socket| {
            eprintln!("rho acp: connection opened");
            let acp = crate::acp::agent_in(server.agent.clone(), server.hub.clone());
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
