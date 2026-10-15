//! Replaces a session's history with a summary (pi's compaction prompts).
//!
//! If the prompt cache is probably still warm, the session model summarizes
//! from the cached prefix (same system, tools, thinking and history), so the
//! history costs cache-read prices. Otherwise everything would be re-read at
//! full price anyway, so a cheaper model gets the conversation as text
//! without tool outputs.

use std::time::Duration;

use anyhow::{Result, bail};
use futures::StreamExt;

use crate::agent::{Agent, Session, push_user};
use crate::provider::{Content, Event, Message, Request, Role, StopReason, Thinking, Usage};

/// Anthropic's default cache lifetime, refreshed on every hit.
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// Treat the cache as expired a bit early: the clock starts at request start
/// on our side, the request may take a while to arrive.
const CACHE_MARGIN: Duration = Duration::from_secs(30);

const SYSTEM: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const INSTRUCTIONS: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work. Do not call any tools.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// The first message after a compaction: `SUMMARY_START`, the summary, `SUMMARY_END`.
pub(crate) const SUMMARY_START: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub(crate) const SUMMARY_END: &str = "\n</summary>";

/// The summary in a compacted history's first text block.
pub(crate) fn summary_of(text: &str) -> Option<&str> {
    text.strip_prefix(SUMMARY_START)?.strip_suffix(SUMMARY_END)
}

/// Tool call arguments (e.g. whole files for Write) are cut to this many chars.
const MAX_ARGS: usize = 2000;
const MAX_SUMMARY_TOKENS: u32 = 16_000;

impl Agent {
    /// Summarizes the history and replaces it with the summary. `focus` is
    /// optional extra instruction from the user (`/compact <focus>`).
    pub async fn compact(&self, session: &mut Session, focus: &str) -> Result<()> {
        if session.history.is_empty() {
            bail!("nothing to compact");
        }
        let mut instructions = INSTRUCTIONS.to_string();
        if !focus.trim().is_empty() {
            instructions += &format!("\n\nAdditional focus: {}", focus.trim());
        }

        let warm = session
            .last_request
            .is_some_and(|t| t.elapsed() + CACHE_MARGIN < CACHE_TTL);
        let (provider, request, label) = match &session.compact_model {
            Some(cheap) if !warm => {
                let provider = session.ctx.registry().resolve(cheap)?;
                let text = format!(
                    "<conversation>\n{}\n</conversation>\n\n{instructions}",
                    serialize(&session.history)
                );
                let request = Request {
                    model: cheap.id().clone(),
                    system: SYSTEM.into(),
                    messages: vec![Message {
                        role: Role::User,
                        content: vec![Content::text(text)],
                    }],
                    tools: vec![],
                    max_tokens: MAX_SUMMARY_TOKENS,
                    thinking: Thinking::Off,
                };
                (provider, request, format!("{cheap}, cache expired"))
            }
            _ => {
                // Must match the last run's request prefix to read the cache.
                let mut messages = session.history.clone();
                push_user(&mut messages, vec![Content::text(instructions)]);
                let request = Request {
                    model: session.model.id().clone(),
                    system: session.system.clone(),
                    messages,
                    tools: session.tool_specs(),
                    max_tokens: self.max_tokens,
                    thinking: session.thinking,
                };
                let state = if warm { "cache warm" } else { "cache expired" };
                (
                    session.provider.clone(),
                    request,
                    format!("{}, {state}", session.model),
                )
            }
        };

        let mut stream = session.ctx.stream(&*provider, request);
        let mut summary = String::new();
        let mut usage = Usage::default();
        while let Some(event) = stream.next().await {
            match event? {
                Event::TextDelta(t) => summary.push_str(&t),
                Event::Usage(u) => usage = u,
                Event::Done(StopReason::MaxTokens) => bail!("summary hit the token limit"),
                _ => {}
            }
        }
        if summary.trim().is_empty() {
            bail!("model returned no summary");
        }

        let mut summary = summary.trim().to_string();
        // The call that set it is gone with the history.
        if let Some(title) = session.ctx.title() {
            summary += &format!("\n\nThe current title of the session is: {title}");
        }
        session.history = vec![Message {
            role: Role::User,
            content: vec![Content::text(format!(
                "{SUMMARY_START}{summary}{SUMMARY_END}"
            ))],
        }];
        // The new prefix is not cached yet.
        session.last_request = None;
        session.save_compacted().await;
        session.ctx.notify(
            &format!("Compacted ({label})"),
            &format!(
                "input {} + cache read {} + cache write {} | output {}\n\n{}",
                usage.input,
                usage.cache_read,
                usage.cache_write,
                usage.output,
                summary.trim()
            ),
            false,
        );
        Ok(())
    }
}

/// The history as plain text, so the model does not continue it. Tool
/// outputs are left out; tool calls stay so the summary knows what was done.
fn serialize(history: &[Message]) -> String {
    let mut parts = vec![];
    for msg in history {
        for block in &msg.content {
            match (msg.role, block) {
                (Role::User, Content::Text { text }) => parts.push(format!("[User]: {text}")),
                (Role::User, Content::Image { .. }) => parts.push("[User]: [image]".into()),
                (Role::Assistant, Content::Text { text }) => {
                    parts.push(format!("[Assistant]: {text}"))
                }
                (_, Content::Thinking { thinking, .. }) => {
                    parts.push(format!("[Assistant thinking]: {thinking}"))
                }
                (_, Content::ToolUse { name, input, .. }) => {
                    let mut args = input.to_string();
                    if args.len() > MAX_ARGS {
                        let cut = args.floor_char_boundary(MAX_ARGS);
                        args = format!("{}... [{} more chars]", &args[..cut], args.len() - cut);
                    }
                    parts.push(format!("[Assistant tool call]: {name}({args})"))
                }
                _ => {}
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCallId;
    use crate::provider::ToolName;
    use serde_json::json;

    #[test]
    fn serialize_drops_tool_outputs_and_cuts_args() {
        let history = vec![
            Message {
                role: Role::User,
                content: vec![Content::text("fix it")],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::text("looking"),
                    Content::ToolUse {
                        id: ToolCallId::new("1"),
                        name: ToolName::new("Write"),
                        input: json!({"content": "x".repeat(5000)}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: ToolCallId::new("1"),
                    content: "SECRET OUTPUT".into(),
                    is_error: false,
                    images: vec![],
                }],
            },
        ];
        let text = serialize(&history);
        assert!(text.starts_with("[User]: fix it\n\n[Assistant]: looking\n\n[Assistant tool call]: Write({\"content\":\"xxx"), "{text}");
        assert!(text.contains("more chars]"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.len() < 2200, "{}", text.len());
    }
}
