//! `Discard`: the model drops old tool outputs from its context. Every tool
//! result in a request starts with `[output N, size]`; the model passes the
//! numbers. Only requests change: the history, the store and the user's
//! transcript keep the full outputs.
//!
//! A discard changes the request in the middle, so the prompt cache breaks
//! from there. Discards wait until the cache has certainly expired, then
//! cost nothing extra.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rho::compact::CACHE_TTL;
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{Content, ImageSource, Request, ToolCallId, ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DISCARD: ToolName = ToolName::from_static("Discard");
/// `Context::stored` key: the discarded outputs' tool call ids.
const IDS: &str = "discard.ids";
/// Our clock starts at request start, the provider's cache a bit later.
const MARGIN: Duration = Duration::from_secs(30);

pub struct Discards;

/// One session's state, in `Context::state`.
#[derive(Default)]
struct State(Mutex<Inner>);

#[derive(Default)]
struct Inner {
    /// Output `N` of the last request is `numbers[N - 1]`.
    numbers: Vec<ToolCallId>,
    /// Discarded, but the cache was still warm.
    pending: Vec<ToolCallId>,
    last_request: Option<Instant>,
}

impl Inner {
    fn cache_cold(&self) -> bool {
        self.last_request
            .is_none_or(|t| t.elapsed() > CACHE_TTL + MARGIN)
    }
}

impl Extension for Discards {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(Discard));
    }

    async fn before_request(&self, ctx: &Context, request: &mut Request) {
        let state = ctx.state::<State>();
        let mut inner = state.0.lock().unwrap();
        let mut discarded: Vec<ToolCallId> = ctx.stored(IDS).unwrap_or_default();
        if inner.cache_cold() {
            discarded.append(&mut inner.pending);
        }
        inner.numbers.clear();
        for content in request.messages.iter_mut().flat_map(|m| &mut m.content) {
            let Content::ToolResult {
                tool_use_id,
                content,
                images,
                ..
            } = content
            else {
                continue;
            };
            inner.numbers.push(tool_use_id.clone());
            let n = inner.numbers.len();
            if discarded.contains(tool_use_id) {
                *content = format!("[output {n} discarded]");
                images.clear();
            } else {
                let size = content.len() + images.iter().map(image_bytes).sum::<usize>();
                *content = format!("[output {n}, {}]\n{content}", bytes(size));
            }
        }
        // Outputs gone with a compaction.
        discarded.retain(|id| inner.numbers.contains(id));
        let Inner {
            numbers, pending, ..
        } = &mut *inner;
        pending.retain(|id| numbers.contains(id));
        ctx.set_stored(IDS, discarded);
        inner.last_request = Some(Instant::now());
    }
}

fn image_bytes(image: &ImageSource) -> usize {
    match image {
        ImageSource::Base64 { data, .. } => data.len(),
    }
}

fn bytes(n: usize) -> String {
    match n {
        ..1000 => format!("{n} B"),
        _ => format!("{} kB", n / 1000),
    }
}

#[derive(Deserialize, JsonSchema)]
struct DiscardInput {
    /// the N of the `[output N, ...]` labels to discard
    outputs: Vec<usize>,
}

struct Discard;

impl Tool for Discard {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: DISCARD,
            description: "Remove old tool outputs from your context, by the N of their `[output N, size]` label: large file reads, logs or images you no longer need. Each is replaced by `[output N discarded]`; the user still sees it. If you need one again, run its tool again. Only worth it for large outputs. Takes effect once the prompt cache has expired, so the cache is not lost.".into(),
            input_schema: input_schema::<DiscardInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: DiscardInput = parse(input)?;
        let discarded: Vec<ToolCallId> = ctx.stored(IDS).unwrap_or_default();
        let state = ctx.state::<State>();
        let mut inner = state.0.lock().unwrap();
        let mut ids = vec![];
        let mut bad = vec![];
        for n in i.outputs {
            match n.checked_sub(1).and_then(|i| inner.numbers.get(i)) {
                Some(id) if !discarded.contains(id) && !inner.pending.contains(id) => {
                    ids.push(id.clone())
                }
                _ => bad.push(n.to_string()),
            }
        }
        if !bad.is_empty() {
            bail!(
                "No discardable output: {}. Nothing was discarded.",
                bad.join(", ")
            );
        }
        inner.pending.extend(ids);
        Ok(if inner.cache_cold() {
            "Discarded from the next request on."
        } else {
            "Will be discarded once the prompt cache expires; until then they still count."
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::provider::{Message, Role};
    use rho::store::SessionId;
    use serde_json::json;

    fn ctx() -> Context {
        Context::new(
            SessionId::new("s"),
            std::path::Path::new("."),
            Arc::new(rho::extension::NoFrontend),
            Default::default(),
        )
    }

    fn result(id: &str, content: &str) -> Content {
        Content::ToolResult {
            tool_use_id: ToolCallId::new(id),
            content: content.into(),
            is_error: false,
            images: vec![],
            shown: vec![],
        }
    }

    fn request(results: Vec<Content>) -> Request {
        Request {
            model: rho::provider::ModelId::new("m"),
            system: String::new(),
            messages: vec![Message {
                role: Role::User,
                content: results,
            }],
            tools: vec![],
            max_tokens: 1,
            thinking: Default::default(),
        }
    }

    /// The tool results' texts after `before_request`.
    async fn send(ctx: &Context, results: Vec<Content>) -> Vec<String> {
        let mut req = request(results);
        Discards.before_request(ctx, &mut req).await;
        req.messages[0]
            .content
            .iter()
            .map(|c| match c {
                Content::ToolResult { content, .. } => content.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    fn expire(ctx: &Context) {
        let state = ctx.state::<State>();
        let mut inner = state.0.lock().unwrap();
        inner.last_request = Some(Instant::now() - CACHE_TTL - MARGIN * 2);
    }

    #[tokio::test]
    async fn labels_then_discards_once_the_cache_expired() {
        let ctx = ctx();
        let big = "x".repeat(2500);
        let results = || vec![result("a", &big), result("b", "small")];
        assert_eq!(
            send(&ctx, results()).await,
            [
                format!("[output 1, 2 kB]\n{big}"),
                "[output 2, 5 B]\nsmall".into()
            ]
        );

        let out = Discard.run(json!({"outputs": [1]}), &ctx).await.unwrap();
        assert!(
            out.output.contains("once the prompt cache expires"),
            "{}",
            out.output
        );
        // Warm: unchanged.
        assert!(send(&ctx, results()).await[0].ends_with(&big));
        expire(&ctx);
        assert_eq!(send(&ctx, results()).await[0], "[output 1 discarded]");
        // Stays discarded, also in later warm requests.
        assert_eq!(send(&ctx, results()).await[0], "[output 1 discarded]");
        // After a compaction the id is gone, and with it the stored entry.
        send(&ctx, vec![result("c", "new")]).await;
        assert_eq!(ctx.stored::<Vec<ToolCallId>>(IDS).unwrap(), []);
    }

    #[tokio::test]
    async fn unknown_or_discarded_numbers_are_errors() {
        let ctx = ctx();
        send(&ctx, vec![result("a", "x")]).await;
        let err = Discard
            .run(json!({"outputs": [0, 2]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("0, 2"), "{err}");
        Discard.run(json!({"outputs": [1]}), &ctx).await.unwrap();
        // Pending already.
        assert!(Discard.run(json!({"outputs": [1]}), &ctx).await.is_err());
    }
}
