use std::io::Write as _;
use std::sync::{Arc, Mutex};

use crate::agent::{Agent, AgentEvent, SessionsDb};
use crate::anthropic::{self, Anthropic};
use crate::auth::{self, AuthStore, Pkce};
use crate::copilot::{self, Copilot};
use crate::extension::{DynExtension, Frontend};
use crate::provider::{Content, ModelSpec, ProviderName, Registry, Thinking, Usage};
use anyhow::{Context, Result, bail};

const USAGE: &str = "\
usage:
  rho [options] <prompt...>   one-shot: run the prompt, print the answer
  rho acp [options]           serve the Agent Client Protocol on stdio
  rho acp --daemon            ... through a running daemon
  rho daemon [options]        serve ACP for `acp --daemon` on ~/.config/rho/daemon/acp.sock
  rho daemon --listen ADDR    ... and on ws://ADDR/, with a bearer token
  rho login [anthropic]       sign in with a Claude Pro/Max subscription
  rho login github-copilot    sign in with GitHub Copilot (Claude and GPT models)

options:
  -m, --model provider/model                    env RHO_MODEL
  -t, --thinking off|low|medium|high|xhigh|max  env RHO_THINKING, default high

env:
  RHO_AUTH_FILE      credential file
  RHO_COMPACT_MODEL  model for /compact when the cache expired
  RHO_SESSIONS_DB    database for ACP sessions, default ~/.config/rho/sessions.db
  RHO_ACP_TOKEN      token for `daemon --listen`, default from ~/.config/rho/acp-token (created)

files in ~/.config/rho/config.d, each kind in file name order:
  *.json                merged (objects merge, later values win, null deletes) into
                        sandbox, features, models (by path), mcps (MCP servers over HTTP)
                        and api (OpenAI-compatible /v1 on the daemon's listeners)
  *PREAMBLE_SYSTEM.md   joined, replace the preamble (00-rho.PREAMBLE_SYSTEM.md is
                        written on startup without one)
  *APPEND_SYSTEM.md     joined, added to the system prompt
files in ~/.config/rho: AGENTS.md, project_context.json (approved project AGENTS.md dirs);
empty files are ignored";

/// What a `rho` binary is made of. Start from the defaults and change what
/// you need:
///
/// ```ignore
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     rho::run(rho::Options {
///         extensions: [rho::builtin(), vec![Arc::new(MyExtension)]].concat(),
///         ..Default::default()
///     })
///     .await
/// }
/// ```
pub struct Options {
    /// Providers by name; `-m name/model` picks one. Default: `anthropic`
    /// and `github-copilot` (auth from `RHO_AUTH_FILE` / `~/.config/rho/auth.json`).
    pub providers: Registry,
    /// Default: `rho::builtin()`, the built-in tools (`Read`, `ReadImage`,
    /// `Write`, `Edit`, `Bash`, `RequestAccess`, `Compact`). Keep them first
    /// when adding your own.
    pub extensions: Vec<Arc<dyn DynExtension>>,
    /// Unless `-m` or `RHO_MODEL` is set.
    pub model: ModelSpec,
    /// For `/compact` with an expired cache, unless `RHO_COMPACT_MODEL` is set.
    /// Default: none, the session model compacts.
    pub compact_model: Option<ModelSpec>,
    /// Database for ACP sessions, by cwd; `None` does not save them.
    /// Default: `store::default_path()` (`RHO_SESSIONS_DB` or
    /// `~/.config/rho/sessions.db`) for every cwd. One-shot runs are not saved.
    pub sessions_db: Option<SessionsDb>,
}

impl Default for Options {
    fn default() -> Self {
        let http = reqwest::Client::new();
        let store = |key| Arc::new(AuthStore::new(AuthStore::default_path(), http.clone(), key));
        let mut providers = Registry::default();
        providers.register(
            anthropic::NAME,
            Arc::new(Anthropic::new(http.clone(), store(anthropic::NAME))),
        );
        providers.register(
            copilot::KEY,
            Arc::new(Copilot::new(http.clone(), store(copilot::KEY))),
        );
        Self {
            providers,
            extensions: crate::tools::builtin(),
            model: ModelSpec::from_static("anthropic", "claude-sonnet-5-5"),
            compact_model: None,
            sessions_db: Some(Arc::new(|_| crate::store::default_path())),
        }
    }
}

/// The `rho` command line (one-shot, `acp`, `login`).
pub async fn run(options: Options) -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let model_flag = take_flag(&mut args, "-m", "--model")?;
    let thinking_flag = take_flag(&mut args, "-t", "--thinking")?;
    let model: Option<ModelSpec> = model_flag
        .or_else(|| std::env::var("RHO_MODEL").ok())
        .map(|m| m.parse())
        .transpose()?;
    let thinking: Thinking = thinking_flag
        .or_else(|| std::env::var("RHO_THINKING").ok())
        .map(|t| t.parse())
        .transpose()?
        .unwrap_or_default();

    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => {
            eprintln!("{USAGE}");
            Ok(())
        }
        Some("login") => match args.get(1).map(ProviderName::new) {
            None => login().await,
            Some(p) if p == anthropic::NAME => login().await,
            Some(p) if p == copilot::KEY => login_copilot().await,
            Some(other) => bail!("unknown provider {:?}\n{USAGE}", other.as_str()),
        },
        Some("acp") => {
            if args.iter().any(|a| a == "--daemon") {
                let dir = crate::config_dir().join(crate::acp_ws::DAEMON_DIR);
                let result = crate::acp_ws::connect_stdio(&dir).await;
                // Tokio's stdin reader blocks in a thread that cannot be
                // cancelled, so the runtime would wait for the editor forever.
                if let Err(e) = result {
                    eprintln!("Error: {e:?}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            let agent = Arc::new(acp_agent(options, model, thinking)?);
            use agent_client_protocol::ConnectTo;
            crate::acp::agent(agent)
                .connect_to(agent_client_protocol::Stdio::new())
                .await
                .map_err(|e| anyhow::anyhow!("acp: {e}"))
        }
        Some("daemon") => {
            let listen = take_flag(&mut args, "-l", "--listen")?;
            let config_dir = crate::config_dir();
            let dir = config_dir.join(crate::acp_ws::DAEMON_DIR);
            let Some((_lock, socket)) = crate::acp_ws::bind_daemon(&dir)? else {
                eprintln!("rho daemon: already running");
                return Ok(());
            };
            eprintln!("rho daemon: {}", crate::acp_ws::socket_path(&dir).display());
            let agent = Arc::new(acp_agent(options, model, thinking)?);
            // One hub: editors and acp-ui see the same sessions.
            let hub = Arc::new(crate::acp::Hub::default());
            let unix = crate::acp_ws::serve(agent.clone(), hub.clone(), socket, None, None);
            let Some(addr) = listen else {
                return unix.await;
            };
            let token = crate::acp_ws::token(&config_dir)?;
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("listening on {addr}"))?;
            eprintln!(
                "rho daemon: ws://{}/ (token from RHO_ACP_TOKEN or {})",
                listener.local_addr()?,
                config_dir.join(crate::acp_ws::TOKEN_FILE).display()
            );
            let web = crate::acp_ws::web_dir();
            if let Some(dir) = &web {
                let addr = listener.local_addr()?;
                eprintln!("rho daemon: web client http://{addr}/ ({})", dir.display());
            }
            let tcp = crate::acp_ws::serve(agent, hub, listener, Some(token), web);
            tokio::try_join!(unix, tcp).map(|_| ())
        }
        Some(_) => one_shot(make_agent(options, model, thinking)?, args.join(" ")).await,
    }
}

/// Removes `short value` / `long value` from `args`.
fn take_flag(args: &mut Vec<String>, short: &str, long: &str) -> Result<Option<String>> {
    let Some(i) = args.iter().position(|a| a == short || a == long) else {
        return Ok(None);
    };
    args.remove(i);
    if i >= args.len() {
        bail!("{long} needs a value\n{USAGE}");
    }
    Ok(Some(args.remove(i)))
}

/// An agent that saves sessions, for ACP.
fn acp_agent(options: Options, model: Option<ModelSpec>, thinking: Thinking) -> Result<Agent> {
    let sessions_db = options.sessions_db.clone();
    let mut agent = make_agent(options, model, thinking)?;
    agent.sessions_db = sessions_db;
    Ok(agent)
}

/// `model`: from `-m` / `RHO_MODEL`, it wins over the `models` config defaults.
fn make_agent(options: Options, model: Option<ModelSpec>, thinking: Thinking) -> Result<Agent> {
    let model_pinned = model.is_some();
    let model = model.unwrap_or(options.model);
    let provider = options.providers.resolve(&model)?;
    let mut agent = Agent::new(provider, model);
    agent.thinking = thinking;
    agent.model_pinned = model_pinned;
    agent.registry = options.providers;
    let config_dir = crate::config_dir();
    crate::config::check_old(&config_dir)?;
    crate::prompt::ensure_files(&config_dir);
    let config = crate::config::load(&config_dir)?;
    agent.sandbox = crate::config::section(&config, "sandbox")?;
    agent.features = crate::feature::load(&config)?;
    agent.models = crate::config::section(&config, crate::models::Config::KEY)?;
    agent.api = crate::config::section(&config, crate::openai_api::Config::KEY)?;
    agent.config_dir = Some(config_dir);
    let compact = match std::env::var("RHO_COMPACT_MODEL") {
        Ok(m) => {
            agent.compact_pinned = true;
            Some(m.parse().context("RHO_COMPACT_MODEL")?)
        }
        Err(_) => options.compact_model,
    };
    agent.models.check(
        (!agent.model_pinned).then_some(&agent.model),
        compact.as_ref().filter(|_| !agent.compact_pinned),
    )?;
    agent.compact_model = compact;
    agent.extensions = options.extensions;
    Ok(agent)
}

/// Prints the answer to stdout and everything else to stderr. No UI, so
/// confirmations are denied.
#[derive(Default)]
struct Cli {
    state: Mutex<CliState>,
}

#[derive(Default)]
struct CliState {
    thinking: bool,
    usage: Usage,
    requests: u32,
}

impl Frontend for Cli {
    fn event(&self, ev: AgentEvent<'_>) {
        let mut st = self.state.lock().unwrap();
        // Close a thinking run with a newline before anything else is shown.
        let was_thinking =
            std::mem::replace(&mut st.thinking, matches!(ev, AgentEvent::Thinking(_)));
        if was_thinking && !st.thinking {
            eprintln!();
        }
        match ev {
            AgentEvent::Text(t) => {
                let mut stdout = std::io::stdout();
                let _ = write!(stdout, "{t}");
                let _ = stdout.flush();
            }
            // Dim and on stderr, so stdout carries only the answer.
            AgentEvent::Thinking(t) => eprint!("\x1b[2m{t}\x1b[0m"),
            AgentEvent::Profile(id) => eprintln!("\n[profile: {id}]"),
            AgentEvent::Usage { usage, .. } => {
                st.usage += usage;
                st.requests += 1;
            }
            AgentEvent::ToolStart { name, input, .. } => eprintln!("\n> {name} {input}"),
            AgentEvent::ToolStream { chunk, stderr, .. } => {
                let (on, off) = if stderr {
                    ("\x1b[31m", "\x1b[0m")
                } else {
                    ("", "")
                };
                eprint!("{on}{chunk}{off}");
            }
            AgentEvent::ToolEnd {
                is_error: true,
                output,
                ..
            } => eprintln!("! {}", output.lines().next().unwrap_or_default()),
            AgentEvent::Notice {
                title,
                text,
                is_error,
                ..
            } => eprintln!(
                "\n[{}{title}]{}{text}",
                if is_error { "failed: " } else { "" },
                if text.is_empty() { "" } else { "\n" }
            ),
            AgentEvent::Progress { title, .. } => eprintln!("\n[{title}]"),
            AgentEvent::ToolEnd { .. } | AgentEvent::Title(_) | AgentEvent::Model { .. } => {}
        }
    }
}

async fn one_shot(agent: Agent, prompt: String) -> Result<()> {
    let cli = Arc::new(Cli::default());
    let mut session = agent
        .start_session(
            crate::store::SessionId::new("cli"),
            std::env::current_dir()?,
            vec![],
            cli.clone(),
        )
        .await?;
    let mut stop = agent.run(&mut session, vec![Content::text(prompt)]).await?;
    // shortcut: only handles messages already triggered when the run ends; a
    // one-shot run does not wait for background work that reports later.
    while let Some(next) = agent.run_triggered(&mut session).await {
        stop = next?;
    }
    println!();
    let st = cli.state.lock().unwrap();
    eprintln!(
        "[{} requests | input {} + cache read {} + cache write {} | output {}]",
        st.requests, st.usage.input, st.usage.cache_read, st.usage.cache_write, st.usage.output
    );
    if stop != crate::provider::StopReason::EndTurn {
        eprintln!("[stopped: {stop:?}]");
    }
    Ok(())
}

async fn login() -> Result<()> {
    let http = &reqwest::Client::new();
    let store = AuthStore::new(AuthStore::default_path(), http.clone(), anthropic::NAME);
    let pkce = Pkce::generate()?;
    eprintln!(
        "Open this URL, sign in, then paste the code shown by Anthropic:\n\n{}\n",
        pkce.authorize_url()
    );
    eprint!("code: ");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading code")?;
    let creds = auth::exchange_code(http, &line, &pkce).await?;
    store.save(&creds)?;
    eprintln!("Saved credentials to {}", store.path().display());
    Ok(())
}

async fn login_copilot() -> Result<()> {
    let http = &reqwest::Client::new();
    let store = AuthStore::new(AuthStore::default_path(), http.clone(), copilot::KEY);
    let device = copilot::start_login(http).await?;
    eprintln!(
        "Open {} and enter the code {}\nWaiting...",
        device.verification_uri, device.user_code
    );
    let creds = copilot::finish_login(http, &device).await?;
    store.save(&creds)?;
    eprintln!("Saved credentials to {}", store.path().display());
    Ok(())
}
