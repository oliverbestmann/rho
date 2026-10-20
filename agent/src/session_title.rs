//! Lets the model name the session with the `SetSessionTitle` tool: once at
//! the start and again when the conversation drifts. Compaction tells the
//! model the current title (see `rho::compact`).

use std::sync::Arc;

use anyhow::{Result, bail};
use rho::extension::{Context, Extension, PromptSetup, SessionSetup, ToolOutput};
use rho::provider::{Content, ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const NAME: ToolName = ToolName::from_static("SetSessionTitle");
const SYSTEM: &str = "\n\n<session-title>\nYou must call the SetSessionTitle tool to set an initial title as soon as possible. \
    Update the title whenever the conversation drifts too far from it.\n</session-title>";
const REMINDER: &str = "<system-reminder>\nThe session has no title yet. Call the SetSessionTitle tool to set one.\n</system-reminder>";

pub struct SessionTitle;

impl Extension for SessionTitle {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.system += SYSTEM;
        session.tools.push(Arc::new(SetSessionTitle));
    }

    /// After the first turn, every input reminds the model until it sets a title.
    async fn before_prompt(&self, ctx: &Context, prompt: &mut PromptSetup) {
        if ctx.session_info().requests > 0 && ctx.title().is_none() {
            prompt.content.push(Content::system_note(REMINDER));
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    /// Concise 3-7 word title, no quotes or markdown.
    title: String,
}

struct SetSessionTitle;

impl Tool for SetSessionTitle {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME,
            description: "Set the title of this session, shown to the user in the session list."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let input: Input = parse(input)?;
        let title = clean_title(&input.title);
        if title.is_empty() {
            bail!("title is empty");
        }
        ctx.set_title(&title);
        Ok(format!("Title set to: {title}").into())
    }
}

/// First line, without a `Title:` label, wrapping quotes/markdown or extra spaces, max 80 chars.
fn clean_title(reply: &str) -> String {
    let line = reply.trim().lines().next().unwrap_or_default();
    let line = match line.get(..6) {
        Some(label) if label.eq_ignore_ascii_case("title:") => &line[6..],
        _ => line,
    };
    let wrap = |c: char| "\"'`*_".contains(c);
    let line = line.trim().trim_matches(wrap);
    let words = line.split_whitespace().collect::<Vec<_>>().join(" ");
    words
        .chars()
        .take(80)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use rho::agent::Agent;
    use rho::extension::NoFrontend;
    use rho::provider::{Event, ModelSpec, Provider, Request, StopReason};
    use rho::store::SessionId;
    use serde_json::json;

    struct Echo;

    impl Provider for Echo {
        fn stream(&self, _: Request, _: &dyn Fn(&mut Value)) -> BoxStream<'static, Result<Event>> {
            let events = [
                Event::TextDelta("ok".into()),
                Event::Done(StopReason::EndTurn),
            ];
            stream::iter(events.map(Ok)).boxed()
        }
    }

    #[tokio::test]
    async fn reminds_after_the_first_turn_until_titled() {
        let mut agent = Agent::new(Arc::new(Echo), ModelSpec::new("test", "m"), vec![]);
        agent.extensions.push(Arc::new(SessionTitle));
        let mut s = agent
            .start_session(SessionId::new("s"), "/tmp", vec![], Arc::new(NoFrontend))
            .await
            .unwrap();
        let reminded = |s: &rho::agent::Session| {
            // The last user message with text: a turn report note follows it.
            let last_prompt = s.history.iter().rev().find(|m| {
                m.role == rho::provider::Role::User
                    && m.content.iter().any(|c| matches!(c, Content::Text { .. }))
            });
            last_prompt
                .unwrap()
                .content
                .contains(&Content::system_note(REMINDER))
        };
        agent.run(&mut s, vec![Content::text("hi")]).await.unwrap();
        assert!(!reminded(&s));
        agent
            .run(&mut s, vec![Content::text("more")])
            .await
            .unwrap();
        assert!(reminded(&s));
        s.ctx.set_title("Greeting");
        agent
            .run(&mut s, vec![Content::text("again")])
            .await
            .unwrap();
        assert!(!reminded(&s));
    }

    #[test]
    fn cleans_titles() {
        assert_eq!(clean_title("Fix the build"), "Fix the build");
        assert_eq!(
            clean_title("  Title: \"Fix  the build\"\nmore"),
            "Fix the build"
        );
        assert_eq!(clean_title("**Fix the build**"), "Fix the build");
        assert_eq!(clean_title(&"a".repeat(100)).len(), 80);
        assert_eq!(clean_title(""), "");
    }

    #[tokio::test]
    async fn sets_the_title() {
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            Arc::new(NoFrontend),
            Default::default(),
        );
        let out = SetSessionTitle
            .run(json!({"title": " **Fix the build** "}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.output, "Title set to: Fix the build");
        assert_eq!(ctx.title().as_deref(), Some("Fix the build"));
        assert!(
            SetSessionTitle
                .run(json!({"title": "  "}), &ctx)
                .await
                .is_err()
        );
    }
}
