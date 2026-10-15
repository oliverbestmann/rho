//! `FetchUrl`: fetch a URL and return it as markdown (pi's fetch-url). HTML
//! is converted with `htmd` (a turndown port), other content is returned as
//! is. The request runs in rho's process: the sandbox covers files, not the
//! network.

use std::sync::Arc;

use anyhow::{Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_MAX_LENGTH: usize = 64_000;

pub struct FetchUrl;

impl Extension for FetchUrl {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(Fetch::default()));
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Input {
    /// URL to fetch
    url: String,
    /// Max characters to return (default: 64000)
    #[schemars(range(min = 1))]
    max_length: Option<usize>,
    /// Return raw response body instead of converting HTML to markdown (default: false)
    #[serde(default)]
    raw: bool,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

#[derive(Default)]
struct Fetch {
    http: reqwest::Client,
}

impl Tool for Fetch {
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::Fetch
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use FetchUrl to read web pages and files from URLs instead of curl or wget in Bash."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("FetchUrl"),
            description: "Fetch a URL and return its content as markdown, with HTML markup converted to readable text.".into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, _ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let res = self
            .http
            .get(&i.url)
            .header(reqwest::header::USER_AGENT, "rho")
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            bail!("fetch failed: {status}");
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = res.text().await?;
        let text = if !i.raw && content_type.contains("html") {
            to_markdown(&body)?
        } else {
            body.trim().to_string()
        };
        Ok(cut(&text, i.max_length.unwrap_or(DEFAULT_MAX_LENGTH)).into())
    }
}

fn to_markdown(html: &str) -> Result<String> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "head", "template"])
        .build();
    Ok(converter.convert(html)?.trim().to_string())
}

/// The first `max` characters, with a note if there were more.
fn cut(text: &str, max: usize) -> String {
    if text.is_empty() {
        return "(empty)".into();
    }
    match text.char_indices().nth(max) {
        Some((end, _)) => format!(
            "{}\n… truncated ({} chars total)",
            &text[..end],
            text.chars().count()
        ),
        None => text.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use rho::extension::NoFrontend;
    use rho::store::SessionId;
    use serde_json::json;

    #[test]
    fn cuts_by_characters() {
        assert_eq!(cut("", 3), "(empty)");
        assert_eq!(cut("äöü", 3), "äöü");
        assert_eq!(cut("äöüß", 3), "äöü\n… truncated (4 chars total)");
    }

    #[tokio::test]
    async fn fetches_and_converts_html() {
        let app = axum::Router::new()
            .route(
                "/page",
                axum::routing::get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        "<html><head><title>T</title><style>p{}</style></head>\
                         <body><h1>Hi</h1><script>x()</script><p>Some <b>bold</b> text</p></body></html>",
                    )
                }),
            )
            .route("/plain", axum::routing::get(|| async { "  <b>not html</b>\n" }))
            .route(
                "/missing",
                axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
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
        let fetch = Fetch::default();
        let get = |path: &str, extra: Value| {
            let mut input = json!({ "url": format!("{base}{path}") });
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            fetch.run(input, &ctx)
        };

        let out = get("/page", json!({})).await.unwrap().output;
        assert!(
            out.contains("Hi") && out.contains("Some **bold** text"),
            "{out}"
        );
        assert!(
            !out.contains("x()") && !out.contains("p{}") && !out.contains('T'),
            "{out}"
        );

        let out = get("/page", json!({"raw": true})).await.unwrap().output;
        assert!(out.starts_with("<html>"), "{out}");

        let out = get("/page", json!({"maxLength": 4})).await.unwrap().output;
        assert!(
            out.starts_with("# Hi\n… truncated") || out.starts_with("Hi\n=\n…"),
            "{out}"
        );

        let out = get("/plain", json!({})).await.unwrap().output;
        assert_eq!(out, "<b>not html</b>");

        let err = get("/missing", json!({})).await.unwrap_err();
        assert_eq!(err.to_string(), "fetch failed: 404 Not Found");
    }
}
