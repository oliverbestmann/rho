//! `WebSearch`: search the web with DuckDuckGo's HTML page (ADR 0077). There
//! is no free search API, so this posts the page's form and reads the result
//! links. DuckDuckGo answers clients it takes for bots (a `curl` user agent,
//! a Chrome one without browser headers) with a 202 challenge page.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use anyhow::{Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, ToolKind, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const URL: &str = "https://html.duckduckgo.com/html/";

/// reqwest (rustls) searches closer together than this often get the
/// challenge; curl does not, so DuckDuckGo likely checks the TLS fingerprint.
const GAP: Duration = Duration::from_secs(10);

/// When the last search was sent, for all sessions of the process.
static LAST: Mutex<Option<Instant>> = Mutex::const_new(None);

pub struct WebSearch;

impl Extension for WebSearch {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(Search {
            http: reqwest::Client::new(),
            url: URL.into(),
            gap: GAP,
        }));
    }
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    /// Search terms
    query: String,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

struct Search {
    http: reqwest::Client,
    url: String,
    gap: Duration,
}

impl Tool for Search {
    fn kind(&self) -> ToolKind {
        ToolKind::Search
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use WebSearch to find pages on the web, then FetchUrl to read them."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("WebSearch"),
            description: "Search the web (DuckDuckGo). Returns up to about 10 results, each with title, URL and snippet.".into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let mut last = LAST.lock().await;
        // One retry: a challenge usually ends within a gap.
        for _ in 0..2 {
            if let Some(t) = *last {
                tokio::time::sleep_until(t + self.gap).await;
            }
            let res = self
                .http
                .post(&self.url)
                .header(reqwest::header::USER_AGENT, "rho")
                .header(reqwest::header::ACCEPT_LANGUAGE, "en")
                .form(&[("q", i.query.as_str()), ("b", ""), ("kl", ""), ("df", "")])
                .send()
                .await;
            *last = Some(Instant::now());
            let res = res?;
            let status = res.status();
            if status == reqwest::StatusCode::ACCEPTED {
                continue;
            }
            if !status.is_success() {
                bail!("search failed: {status}");
            }
            return Ok(results(&res.text().await?)?.into());
        }
        bail!("DuckDuckGo blocked the search with a bot challenge, try again in a minute")
    }
}

/// The organic results as a numbered markdown list, ads left out.
fn results(html: &str) -> Result<String> {
    let mut out = Vec::new();
    for block in html.split(r#"class="result__a""#).skip(1) {
        let Some(link) = block.split_once("</a>").map(|(a, _)| a) else {
            continue;
        };
        if link.contains("duckduckgo.com/y.js") {
            continue;
        }
        let title = text(&format!("<a {link}</a>"))?;
        let snippet = match block.split_once(r#"class="result__snippet""#) {
            Some((_, s)) => s
                .split_once('>')
                .and_then(|(_, s)| s.split_once("</a>"))
                .map(|(s, _)| text(s))
                .transpose()?
                .unwrap_or_default(),
            None => String::new(),
        };
        out.push(format!("{}. {title}\n   {snippet}", out.len() + 1));
    }
    if out.is_empty() {
        return Ok("No results.".into());
    }
    Ok(out.join("\n"))
}

fn text(html: &str) -> Result<String> {
    Ok(htmd::convert(html)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::NoFrontend;
    use rho::store::SessionId;
    use serde_json::json;

    const PAGE: &str = r#"
        <div class="result results_links result--ad">
          <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x.com&amp;a=1">Ad</a>
          <a class="result__snippet" href="https://duckduckgo.com/y.js?a=1">Buy</a>
        </div>
        <div class="result results_links web-result">
          <a rel="nofollow" class="result__a" href="https://docs.rs/reqwest/?a=1&amp;b=2">reqwest &amp; Rust</a>
          <a class="result__url" href="https://docs.rs/reqwest/">docs.rs</a>
          <a class="result__snippet" href="https://docs.rs/reqwest/">An <b>HTTP</b> client.</a>
        </div>
        <div class="result results_links web-result">
          <a rel="nofollow" class="result__a" href="https://example.com/">Example</a>
        </div>"#;

    #[test]
    fn parses_results_without_ads() {
        assert_eq!(
            results(PAGE).unwrap(),
            "1. [reqwest & Rust](https://docs.rs/reqwest/?a=1&b=2)\n   An **HTTP** client.\n\
             2. [Example](https://example.com/)\n   "
        );
        assert_eq!(results("<html></html>").unwrap(), "No results.");
    }

    #[tokio::test]
    async fn posts_the_form_and_reports_challenges() {
        use axum::http::{HeaderMap, StatusCode};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static BLOCKED: AtomicUsize = AtomicUsize::new(0);
        let blocked = &BLOCKED;
        let app = axum::Router::new()
            .route(
                "/ok",
                axum::routing::post(|h: HeaderMap, body: String| async move {
                    assert_eq!(h["user-agent"], "rho");
                    assert_eq!(body, "q=rust+http&b=&kl=&df=");
                    PAGE
                }),
            )
            .route(
                "/blocked",
                axum::routing::post(move || {
                    blocked.fetch_add(1, Ordering::Relaxed);
                    async { (StatusCode::ACCEPTED, "anomaly") }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            Arc::new(NoFrontend),
            Default::default(),
        );
        let search = |path: &str| Search {
            http: reqwest::Client::new(),
            url: format!("{base}{path}"),
            gap: Duration::ZERO,
        };
        let input = json!({"query": "rust http"});

        let out = search("/ok").run(input.clone(), &ctx).await.unwrap();
        assert!(
            out.output.starts_with("1. [reqwest & Rust]"),
            "{}",
            out.output
        );

        let err = search("/blocked").run(input, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("bot challenge"), "{err}");
        assert_eq!(BLOCKED.load(Ordering::Relaxed), 2, "retries once");
    }
}
