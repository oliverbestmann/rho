//! Names the session from its first prompt with a cheap model (pi's
//! session-title). Runs in the background, so it does not delay the run.

use std::collections::HashSet;
use std::sync::Mutex;

use rho::extension::{Context, Extension, PromptSetup};
use rho::provider::Content;

const MODEL: &str = "anthropic/claude-haiku-5-5";
const CONTEXT_LIMIT: usize = 800;
const SYSTEM: &str = "Write a concise 3-7 word session title for a conversation that starts with the following user message. Return only the title, with no quotes, markdown, label, or punctuation at the end.";

#[derive(Default)]
pub struct SessionTitle {
    /// Sessions that already had their first prompt.
    seen: Mutex<HashSet<String>>,
}

impl Extension for SessionTitle {
    async fn before_prompt(&self, ctx: &Context, prompt: &mut PromptSetup) {
        if !self.seen.lock().unwrap().insert(ctx.session_id().into()) {
            return;
        }
        let text: String = prompt
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(CONTEXT_LIMIT)
            .collect();
        if text.trim().is_empty() {
            return;
        }
        let ctx = ctx.clone();
        tokio::spawn(async move {
            match ctx.complete(MODEL, SYSTEM, &text).await {
                Ok(reply) => {
                    let title = clean_title(&reply);
                    if !title.is_empty() {
                        ctx.set_title(&title);
                    }
                }
                Err(e) => ctx.notify(
                    "session title",
                    &format!("Could not generate session title: {e:#}"),
                    true,
                ),
            }
        });
    }

    fn session_end(&self, ctx: &Context) {
        self.seen.lock().unwrap().remove(ctx.session_id());
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
    use super::clean_title;

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
}
