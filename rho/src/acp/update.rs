//! Agent events, usage and forms as ACP messages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, ContentBlock, ContentChunk, Cost, Diff, ElicitationContentValue,
    ElicitationSchema, EnumOption, ImageContent, Meta, MultiSelectPropertySchema,
    SessionConfigKind, SessionInfoUpdate, SessionUpdate, StopReason, StringPropertySchema,
    Terminal, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, Usage as AcpUsage, UsageUpdate,
};

use crate::agent::AgentEvent;
use crate::extension::{Answers, Field, FieldKey, FieldKind, Form, red};
use crate::provider::{self, ImageSource, NoteType, ToolName, Usage};
use crate::tools;
use crate::tools::EDIT;

use super::PROFILE_OPTION;
use super::frontend::Published;

/// The form as an elicitation schema: one property per field.
pub(super) fn form_schema(form: &Form) -> ElicitationSchema {
    let mut schema = ElicitationSchema::new();
    for Field {
        key,
        title,
        description,
        kind,
        required,
    } in &form.fields
    {
        let (title, description) = (Some(title.clone()), description.clone());
        schema = match kind {
            FieldKind::Text => schema.property(
                key.to_string(),
                StringPropertySchema::new()
                    .title(title)
                    .description(description),
                *required,
            ),
            FieldKind::Choice { options, multiple } => {
                let options = options
                    .iter()
                    .map(|o| {
                        EnumOption::new(o.value.to_string(), &o.title)
                            .description(o.description.clone())
                    })
                    .collect();
                if *multiple {
                    schema.property(
                        key.to_string(),
                        MultiSelectPropertySchema::titled(options)
                            .title(title)
                            .description(description),
                        *required,
                    )
                } else {
                    schema.property(
                        key.to_string(),
                        StringPropertySchema::new()
                            .title(title)
                            .description(description)
                            .one_of(options),
                        *required,
                    )
                }
            }
        };
    }
    schema
}

/// The submitted values by field key; empty ones are dropped.
pub(super) fn form_answers(content: Option<BTreeMap<String, ElicitationContentValue>>) -> Answers {
    let mut answers = Answers::new();
    for (key, value) in content.unwrap_or_default() {
        let values = match value {
            ElicitationContentValue::String(s) => vec![s],
            ElicitationContentValue::StringArray(v) => v,
            ElicitationContentValue::Integer(i) => vec![i.to_string()],
            ElicitationContentValue::Number(n) => vec![n.to_string()],
            ElicitationContentValue::Boolean(b) => vec![b.to_string()],
            _ => vec![],
        };
        let values: Vec<String> = values
            .into_iter()
            .filter(|v| !v.trim().is_empty())
            .collect();
        if !values.is_empty() {
            answers.insert(FieldKey::new(key), values);
        }
    }
    answers
}

/// Token usage of one prompt for `PromptResponse`; `input_tokens` is the
/// uncached input, the cache counts are separate.
pub(super) fn turn_usage(u: Usage) -> AcpUsage {
    AcpUsage::new(
        u.input + u.output + u.cache_read + u.cache_write,
        u.input,
        u.output,
    )
    .cached_read_tokens(u.cache_read)
    .cached_write_tokens(u.cache_write)
}

/// The latest `AgentEvent::Usage`, so a model switch can republish it with
/// the new context window.
#[derive(Default)]
pub(super) struct LastUsage {
    model: String,
    /// Context in use: the last prompt plus its output.
    used: u64,
    request: Usage,
    total: Usage,
    cost: f64,
}

impl LastUsage {
    /// Nothing was used yet, like `Session::emit_usage` sends nothing then.
    pub(super) fn is_empty(&self) -> bool {
        self.total.prompt() + self.total.output == 0
    }
}

pub(super) fn meta(value: serde_json::Value) -> Meta {
    match value {
        serde_json::Value::Object(m) => m,
        _ => Meta::new(),
    }
}

pub(super) fn queue_update(queue: &[super::queue::Queued]) -> SessionUpdate {
    SessionUpdate::SessionInfoUpdate(
        SessionInfoUpdate::new().meta(meta(serde_json::json!({ "rho": { "queue": queue } }))),
    )
}

/// Before a replay of everything to a client that asked for what came after
/// its cursor: it drops what it shows.
pub(super) fn reset_update() -> SessionUpdate {
    SessionUpdate::SessionInfoUpdate(
        SessionInfoUpdate::new().meta(meta(serde_json::json!({ "rho": { "reset": true } }))),
    )
}

pub(super) fn busy_update(on: bool) -> SessionUpdate {
    SessionUpdate::SessionInfoUpdate(
        SessionInfoUpdate::new().meta(meta(serde_json::json!({ "rho": { "running": on } }))),
    )
}

/// `usage_update`: context used/size and the cost are standard; the token
/// and cache breakdown is in `_meta.rho`.
pub(super) fn usage_update(last: &LastUsage, size: u64) -> SessionUpdate {
    let counts = |u: &Usage| {
        serde_json::json!({
            "input": u.input,
            "output": u.output,
            "cacheRead": u.cache_read,
            "cacheWrite": u.cache_write,
        })
    };
    let hit_rate = (last.request.prompt() > 0)
        .then(|| last.request.cache_read as f64 / last.request.prompt() as f64);
    let mut update = UsageUpdate::new(last.used, size).meta(meta(serde_json::json!({
        "rho": {
            "model": last.model,
            "request": counts(&last.request),
            "session": counts(&last.total),
            "cacheHitRate": hit_rate,
            "cacheTtlSecs": crate::compact::CACHE_TTL.as_secs(),
        }
    })));
    if last.cost > 0.0 {
        update = update.cost(Cost::new(last.cost, "USD"));
    }
    SessionUpdate::UsageUpdate(update)
}

pub(super) fn image_block(source: &ImageSource) -> ContentBlock {
    let ImageSource::Base64 { media_type, data } = source;
    ContentBlock::Image(ImageContent::new(data.clone(), media_type.clone()))
}

/// How a tool call is shown besides its title.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Shown {
    /// Markdown content: the command as a fenced block, or `Tool::describe`.
    Text(String),
    /// A terminal card (Zed's `_meta` terminal protocol, as pi-acp does).
    /// The command and output stream in as `terminal_output`; `ToolEnd`
    /// sends the exit code, and the output unless it `streamed`.
    Terminal {
        cwd: PathBuf,
        command: String,
        streamed: bool,
    },
}

impl Shown {
    pub(super) fn of(
        name: &ToolName,
        input: &serde_json::Value,
        description: Option<&str>,
        terminal_cwd: Option<&Path>,
    ) -> Option<Self> {
        if let Some(text) = description {
            return Some(Self::Text(text.into()));
        }
        let command = input["command"].as_str()?;
        Some(match terminal_cwd {
            Some(cwd) if *name == tools::BASH => Self::Terminal {
                cwd: input["cwd"].as_str().map_or(cwd.into(), |d| cwd.join(d)),
                command: command.into(),
                streamed: false,
            },
            _ => Self::Text(command_block(command)),
        })
    }
}

impl Shown {
    /// A terminal as a `sh` block for a client without terminals.
    pub(super) fn for_viewer(&self, terminal: bool) -> Self {
        match self {
            Self::Terminal { command, .. } if !terminal => Self::Text(command_block(command)),
            other => other.clone(),
        }
    }
}

/// Adds `data` to a terminal card. Zed shows only the title in the card's
/// header, so the command goes first in the output, seen when it is opened.
pub(super) fn terminal_output(id: &provider::ToolCallId, data: &str) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(id.to_string(), ToolCallUpdateFields::new()).meta(meta(
            serde_json::json!({"terminal_output": {"terminal_id": id, "data": data}}),
        )),
    )
}

/// The command as a fenced `sh` block, so Zed highlights it. The fence is
/// longer than any backtick run in the command.
pub(super) fn command_block(command: &str) -> String {
    let command = super::shell::format(command);
    let (mut run, mut longest) = (0, 0);
    for c in command.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}sh\n{command}\n{fence}")
}

/// Exit code from the `[exit status: N]` line that `Bash` appends to the
/// output of a failed command.
pub(super) fn exit_code(output: &str, is_error: bool) -> i64 {
    output
        .trim_end()
        .rsplit_once("[exit status: ")
        .and_then(|(_, rest)| rest.strip_suffix(']')?.parse().ok())
        .unwrap_or(i64::from(is_error))
}

pub(super) fn status(is_error: bool) -> ToolCallStatus {
    if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    }
}

/// The client's icon for a tool, from `Tool::kind`.
pub(super) fn tool_kind(kind: tools::ToolKind) -> ToolKind {
    match kind {
        tools::ToolKind::Read => ToolKind::Read,
        tools::ToolKind::Edit => ToolKind::Edit,
        tools::ToolKind::Execute => ToolKind::Execute,
        tools::ToolKind::Search => ToolKind::Search,
        tools::ToolKind::Fetch => ToolKind::Fetch,
        tools::ToolKind::SwitchMode => ToolKind::SwitchMode,
        tools::ToolKind::Other => ToolKind::Other,
    }
}

pub(super) fn to_update(
    ev: AgentEvent,
    n: u64,
    shown: Option<&Shown>,
    cwd: &Path,
    published: &mut Published,
) -> SessionUpdate {
    let last_usage = &mut published.usage;
    match ev {
        AgentEvent::Profile(id) => {
            for o in &mut published.options {
                if *o.id.0 == *PROFILE_OPTION
                    && let SessionConfigKind::Select(s) = &mut o.kind
                {
                    s.current_value = id.to_string().into();
                }
            }
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(published.options.clone()))
        }
        // A whole note in one chunk; `_meta.rho.note` lets clients style it.
        AgentEvent::Notice {
            kind: NoteType::Text,
            title,
            text,
            ..
        } => {
            let text = match (title.is_empty(), text.is_empty()) {
                (true, _) => text.to_string(),
                (false, true) => format!("**{title}**"),
                (false, false) => format!("**{title}**\n\n{text}"),
            };
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text.into())
                    .meta(meta(serde_json::json!({ "rho": { "note": true } }))),
            )
        }
        // Rendered as a finished tool call, like pi-acp does for extension messages.
        AgentEvent::Notice {
            kind: NoteType::Info,
            title,
            text,
            is_error,
            card,
        } => SessionUpdate::ToolCall(
            // Clients update a known tool call id in place.
            ToolCall::new(
                card.map_or_else(|| format!("notice-{n}"), str::to_string),
                title,
            )
            .kind(ToolKind::Other)
            .status(status(is_error))
            .content(if text.is_empty() {
                vec![]
            } else {
                vec![text.into()]
            }),
        ),
        AgentEvent::Progress { id, title } => SessionUpdate::ToolCall(
            ToolCall::new(id.to_string(), title)
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress),
        ),
        AgentEvent::Title(t) => {
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(t.to_string()))
        }
        AgentEvent::Text(t) => SessionUpdate::AgentMessageChunk(ContentChunk::new(t.into())),
        AgentEvent::Thinking(t) => SessionUpdate::AgentThoughtChunk(ContentChunk::new(t.into())),
        AgentEvent::Usage {
            model,
            usage,
            context_window,
            total,
            cost,
        } => {
            *last_usage = LastUsage {
                model: model.to_string(),
                used: usage.prompt() + usage.output,
                request: usage,
                total,
                cost,
            };
            usage_update(last_usage, context_window)
        }
        AgentEvent::Model {
            spec,
            context_window,
        } => {
            last_usage.model = spec.to_string();
            usage_update(last_usage, context_window)
        }
        AgentEvent::ToolStart {
            id,
            name,
            input,
            kind,
            titled,
            ..
        } => {
            let title = || title(name, input, titled);
            let call = match shown {
                Some(Shown::Terminal { cwd, command, .. }) => ToolCall::new(
                    id.to_string(),
                    call_description(input, titled).map_or(command.clone(), |_| title()),
                )
                .content(vec![ToolCallContent::Terminal(Terminal::new(
                    id.to_string(),
                ))])
                .meta(meta(serde_json::json!({
                    "terminal_info": {"terminal_id": id, "cwd": cwd},
                }))),
                Some(Shown::Text(text)) => {
                    ToolCall::new(id.to_string(), title()).content(vec![text.as_str().into()])
                }
                None => ToolCall::new(id.to_string(), title()),
            };
            // Lets the client follow along to the file.
            let file = [tools::READ, tools::READ_IMAGE, tools::WRITE, EDIT].contains(name);
            let locations = match input.get("path").and_then(|p| p.as_str()) {
                Some(path) if file => vec![ToolCallLocation::new(tools::resolve(cwd, path))],
                _ => vec![],
            };
            SessionUpdate::ToolCall(
                call.kind(tool_kind(kind))
                    .status(ToolCallStatus::InProgress)
                    .locations(locations)
                    .raw_input(input.clone()),
            )
        }
        // Only for terminals; `AcpFrontend::send` caps and colours them first.
        AgentEvent::ToolStream { id, chunk, .. } => terminal_output(id, chunk),
        AgentEvent::ToolEnd {
            id,
            output,
            is_error,
            diff,
            images,
        } => {
            let fields = ToolCallUpdateFields::new().status(status(is_error));
            if let Some(Shown::Terminal { streamed, .. }) = shown {
                // Streamed output is shown already; an error may add a line,
                // like the exit status or a timeout.
                let data = match (*streamed, is_error) {
                    (false, _) => output.to_string(),
                    (true, true) => {
                        red(&format!("\n{}", output.lines().last().unwrap_or_default()))
                    }
                    (true, false) => String::new(),
                };
                return SessionUpdate::ToolCallUpdate(
                    ToolCallUpdate::new(id.to_string(), fields).meta(meta(serde_json::json!({
                        "terminal_output": {"terminal_id": id, "data": data},
                        "terminal_exit": {
                            "terminal_id": id,
                            "exit_code": exit_code(output, is_error),
                            "signal": null,
                        },
                    }))),
                );
            }
            let mut content = vec![];
            if let Some(Shown::Text(text)) = shown {
                content.push(text.as_str().into());
            }
            if let Some(d) = diff {
                content.push(Diff::new(&d.path, &d.new).old_text(d.old.clone()).into());
            }
            content.push(output.into());
            content.extend(images.iter().map(|i| image_block(i).into()));
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_string(),
                fields.content(content),
            ))
        }
    }
}

pub(super) const MAX_TITLE: usize = 160;

/// Input keys shown in a tool call title, the first present one wins.
pub(super) const TITLE_KEYS: [&str; 11] = [
    "command", "url", "pattern", "glob", "target", "query", "path", "hash", "id", "feature",
    "action",
];

/// The `description` the model gave a call of a `Tool::titled` tool.
pub(super) fn call_description(input: &serde_json::Value, titled: bool) -> Option<&str> {
    let text = input["description"].as_str()?.trim();
    (titled && !text.is_empty()).then_some(text)
}

/// The call's description or "Bash first line of the command", at most
/// `MAX_TITLE` characters.
pub(super) fn title(name: &ToolName, input: &serde_json::Value, titled: bool) -> String {
    let arg = TITLE_KEYS.iter().find_map(|k| input[k].as_str());
    let title = match (call_description(input, titled), arg) {
        (Some(text), _) => text.lines().next().unwrap_or_default().to_string(),
        (None, Some(arg)) => format!("{name} {}", arg.lines().next().unwrap_or_default()),
        (None, None) => return name.to_string(),
    };
    match title.char_indices().nth(MAX_TITLE) {
        Some((end, _)) => format!("{}…", &title[..end]),
        None => title,
    }
}

pub(super) fn to_stop(stop: provider::StopReason) -> StopReason {
    match stop {
        provider::StopReason::MaxTokens => StopReason::MaxTokens,
        provider::StopReason::Refusal => StopReason::Refusal,
        provider::StopReason::Stopped => StopReason::Cancelled,
        provider::StopReason::EndTurn | provider::StopReason::ToolUse => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::extension::{Choice, ChoiceValue, Diff as FileDiff};
    use crate::provider::{ModelSpec, ToolCallId};

    fn update(ev: AgentEvent, shown: Option<&Shown>) -> serde_json::Value {
        let mut last = Published::default();
        serde_json::to_value(to_update(ev, 0, shown, Path::new("/w"), &mut last)).unwrap()
    }
    #[test]
    fn tool_end_sends_the_diff() {
        let diff = FileDiff {
            path: "/w/f.txt".into(),
            old: "a\n".into(),
            new: "b\n".into(),
        };
        let wire = update(
            AgentEvent::ToolEnd {
                id: &ToolCallId::new("t1"),
                output: "ok",
                is_error: false,
                diff: Some(&diff),
                images: &[],
            },
            None,
        );
        assert_eq!(
            wire["content"][0],
            json!({"type": "diff", "path": "/w/f.txt", "oldText": "a\n", "newText": "b\n"})
        );
        assert_eq!(wire["content"][1]["content"]["text"], "ok");
    }
    #[test]
    fn tool_end_shows_images_after_the_output() {
        let images = [crate::provider::ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }];
        let wire = update(
            AgentEvent::ToolEnd {
                id: &ToolCallId::new("t1"),
                output: "pic.png (image/png, 1 KB)",
                is_error: false,
                diff: None,
                images: &images,
            },
            None,
        );
        assert_eq!(
            wire["content"][1],
            json!({"type": "content", "content": {"type": "image", "mimeType": "image/png", "data": "AAAA"}})
        );
    }
    #[test]
    fn file_tools_have_a_location() {
        let start = |name: &str, input: &serde_json::Value, kind| {
            update(
                AgentEvent::ToolStart {
                    id: &ToolCallId::new("t1"),
                    name: &ToolName::new(name),
                    input,
                    description: None,
                    kind,
                    titled: false,
                },
                None,
            )
        };
        let wire = start("Edit", &json!({"path": "src/a.rs"}), tools::ToolKind::Edit);
        assert_eq!(wire["kind"], "edit");
        assert_eq!(wire["locations"], json!([{"path": "/w/src/a.rs"}]));
        let skill = json!({"name": "s", "path": "x.md"});
        let wire = start("ReadSkill", &skill, tools::ToolKind::Read);
        assert_eq!(wire["kind"], "read");
        assert_eq!(wire.get("locations"), None);
        let grep = start("Grep", &json!({}), tools::ToolKind::Search);
        assert_eq!(grep["kind"], "search");
    }
    #[test]
    fn commands_are_fenced_as_sh() {
        assert_eq!(command_block("ls -la"), "```sh\nls -la\n```");
        assert_eq!(command_block("echo ```"), "````sh\necho ```\n````");

        let input = json!({"command": "ls"});
        let shown = Shown::of(&ToolName::new("Bash"), &input, None, None).unwrap();
        let start = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Bash"),
            input: &input,
            description: None,
            kind: tools::ToolKind::Execute,
            titled: true,
        };
        let wire = update(start, Some(&shown));
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");

        // ToolEnd replaces the content, so the command comes first again.
        let end = AgentEvent::ToolEnd {
            id: &ToolCallId::new("t1"),
            output: "out",
            is_error: false,
            diff: None,
            images: &[],
        };
        let wire = update(end, Some(&shown));
        assert_eq!(wire["content"][0]["content"]["text"], "```sh\nls\n```");
        assert_eq!(wire["content"][1]["content"]["text"], "out");
    }
    #[test]
    fn tool_descriptions_replace_the_command_block() {
        let input = json!({"questions": []});
        let shown = Shown::of(&ToolName::new("Ask"), &input, Some("**Q?**"), None).unwrap();
        let start = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Ask"),
            input: &input,
            description: Some("**Q?**"),
            kind: tools::ToolKind::Other,
            titled: false,
        };
        assert_eq!(
            update(start, Some(&shown))["content"][0]["content"]["text"],
            "**Q?**"
        );
    }
    #[test]
    fn bash_is_a_terminal_when_the_client_supports_it() {
        let input = json!({"command": "false"});
        let shown = Shown::of(&ToolName::new("Bash"), &input, None, Some(Path::new("/w"))).unwrap();
        let start = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Bash"),
            input: &input,
            description: None,
            kind: tools::ToolKind::Execute,
            titled: true,
        };
        let wire = update(start, Some(&shown));
        assert_eq!(wire["title"], "false");
        assert_eq!(wire["kind"], "execute");
        let described = json!({"command": "false", "description": "Fail on purpose"});
        let start_described = AgentEvent::ToolStart {
            id: &ToolCallId::new("t1"),
            name: &ToolName::new("Bash"),
            input: &described,
            description: None,
            kind: tools::ToolKind::Execute,
            titled: true,
        };
        assert_eq!(
            update(start_described, Some(&shown))["title"],
            "Fail on purpose"
        );
        assert_eq!(
            wire["content"],
            json!([{"type": "terminal", "terminalId": "t1"}])
        );
        assert_eq!(
            wire["_meta"],
            json!({"terminal_info": {"terminal_id": "t1", "cwd": "/w"}})
        );

        let end = AgentEvent::ToolEnd {
            id: &ToolCallId::new("t1"),
            output: "boom\n[exit status: 3]",
            is_error: true,
            diff: None,
            images: &[],
        };
        let wire = update(end, Some(&shown));
        assert_eq!(wire["status"], "failed");
        assert_eq!(
            wire["_meta"]["terminal_output"]["data"],
            "boom\n[exit status: 3]"
        );
        assert_eq!(wire["_meta"]["terminal_exit"]["exit_code"], 3);
        assert!(wire.get("content").is_none());

        // Streamed output is not sent again, only an error's last line.
        let Shown::Terminal { cwd, command, .. } = shown.clone() else {
            unreachable!()
        };
        let streamed = Shown::Terminal {
            cwd,
            command,
            streamed: true,
        };
        let end = AgentEvent::ToolEnd {
            id: &ToolCallId::new("t1"),
            output: "boom\n[exit status: 3]",
            is_error: true,
            diff: None,
            images: &[],
        };
        assert_eq!(
            update(end, Some(&streamed))["_meta"]["terminal_output"]["data"],
            "\x1b[31m\n[exit status: 3]\x1b[0m"
        );

        // Other tools and clients without the capability keep the fenced block.
        assert!(matches!(
            Shown::of(
                &ToolName::new("BackgroundTask"),
                &input,
                None,
                Some(Path::new("/w"))
            ),
            Some(Shown::Text(_))
        ));
        assert!(matches!(
            Shown::of(&ToolName::new("Bash"), &input, None, None),
            Some(Shown::Text(_))
        ));
    }
    #[test]
    fn exit_codes() {
        assert_eq!(exit_code("x\n[exit status: 2]", true), 2);
        assert_eq!(exit_code("x", true), 1);
        assert_eq!(exit_code("x", false), 0);
    }
    #[test]
    fn long_titles_are_cut() {
        let long = "a".repeat(300);
        let t = title(&ToolName::new("Bash"), &json!({"command": long}), true);
        assert_eq!(t.chars().count(), MAX_TITLE + 1);
        assert!(t.ends_with('…'));
        assert_eq!(
            title(
                &ToolName::new("Bash"),
                &json!({"command": "ls\nmore"}),
                true
            ),
            "Bash ls"
        );
        let described = json!({"command": "ls", "description": "List files"});
        assert_eq!(
            title(&ToolName::new("Bash"), &described, true),
            "List files"
        );
        let read = json!({"path": "a", "description": "Check the config"});
        assert_eq!(
            title(&ToolName::new("Read"), &read, true),
            "Check the config"
        );
        // In tools that are not `titled` it may be content, e.g. an MCP tool
        // creating a ticket.
        let mcp = json!({"id": "t", "description": "long text"});
        assert_eq!(
            title(&ToolName::new("mcp__jira__create"), &mcp, false),
            "mcp__jira__create t"
        );
        let grep = json!({"pattern": "fn x", "path": "src"});
        assert_eq!(title(&ToolName::new("Grep"), &grep, true), "Grep fn x");
        let fetch = json!({"url": "https://x.y", "raw": true});
        assert_eq!(
            title(&ToolName::new("FetchUrl"), &fetch, true),
            "FetchUrl https://x.y"
        );
        let feature = json!({"feature": "ssh", "reason": "sign commits"});
        assert_eq!(
            title(&ToolName::new("RequestFeature"), &feature, false),
            "RequestFeature ssh"
        );
        let memory = json!({"action": "search", "query": "x", "description": "Find build notes"});
        assert_eq!(
            title(&ToolName::new("Memory"), &memory, true),
            "Find build notes"
        );
    }
    #[test]
    fn usage_update_has_context_cost_and_cache_breakdown() {
        let request = Usage {
            input: 10,
            output: 50,
            cache_read: 900,
            cache_write: 90,
        };
        let total = Usage {
            input: 20,
            output: 80,
            cache_read: 1000,
            cache_write: 100,
        };
        let mut last = Published::default();
        let wire = serde_json::to_value(to_update(
            AgentEvent::Usage {
                model: &ModelSpec::new("p", "m"),
                usage: request,
                context_window: 1_000_000,
                total,
                cost: 0.5,
            },
            0,
            None,
            Path::new("/w"),
            &mut last,
        ))
        .unwrap();
        assert_eq!(wire["used"], 1050);
        assert_eq!(wire["size"], 1_000_000);
        assert_eq!(wire["cost"], json!({"amount": 0.5, "currency": "USD"}));
        let rho = &wire["_meta"]["rho"];
        assert_eq!(rho["model"], "p/m");
        assert_eq!(
            rho["request"],
            json!({"input": 10, "output": 50, "cacheRead": 900, "cacheWrite": 90})
        );
        assert_eq!(rho["session"]["cacheRead"], 1000);
        assert_eq!(rho["cacheHitRate"], 0.9);
        assert_eq!(rho["cacheTtlSecs"], 300);

        // A model switch republishes the context with the new window.
        let wire = serde_json::to_value(to_update(
            AgentEvent::Model {
                spec: &ModelSpec::new("p", "small"),
                context_window: 200_000,
            },
            1,
            None,
            Path::new("/w"),
            &mut last,
        ))
        .unwrap();
        assert_eq!(
            (wire["used"].clone(), wire["size"].clone()),
            (json!(1050), json!(200_000))
        );
        assert_eq!(wire["_meta"]["rho"]["model"], "p/small");
    }
    #[test]
    fn turn_usage_reports_cache_tokens() {
        let wire = serde_json::to_value(turn_usage(Usage {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
        }))
        .unwrap();
        assert_eq!(
            wire,
            json!({
                "totalTokens": 10,
                "inputTokens": 1,
                "outputTokens": 2,
                "cachedReadTokens": 3,
                "cachedWriteTokens": 4,
            })
        );
    }
    fn sample_form() -> Form {
        Form {
            message: "Two questions".into(),
            fields: vec![
                Field {
                    key: FieldKey::new("q0"),
                    title: "Which db?".into(),
                    description: Some("Pick one".into()),
                    kind: FieldKind::Choice {
                        options: vec![
                            Choice {
                                value: ChoiceValue::new("pg"),
                                title: "Postgres".into(),
                                description: Some("SQL".into()),
                            },
                            Choice {
                                value: ChoiceValue::new("sqlite"),
                                title: "SQLite".into(),
                                description: None,
                            },
                        ],
                        multiple: false,
                    },
                    required: false,
                },
                Field {
                    key: FieldKey::new("q0_text"),
                    title: "Or your own".into(),
                    description: None,
                    kind: FieldKind::Text,
                    required: false,
                },
                Field {
                    key: FieldKey::new("q1"),
                    title: "Features?".into(),
                    description: None,
                    kind: FieldKind::Choice {
                        options: vec![Choice {
                            value: ChoiceValue::new("a"),
                            title: "A".into(),
                            description: None,
                        }],
                        multiple: true,
                    },
                    required: true,
                },
            ],
        }
    }
    #[test]
    fn forms_become_elicitation_schemas() {
        let wire = serde_json::to_value(form_schema(&sample_form())).unwrap();
        assert_eq!(wire["type"], "object");
        assert_eq!(wire["required"], json!(["q1"]));
        let q0 = &wire["properties"]["q0"];
        assert_eq!(q0["type"], "string");
        assert_eq!(q0["title"], "Which db?");
        assert_eq!(
            q0["oneOf"][0],
            json!({"const": "pg", "title": "Postgres", "description": "SQL"})
        );
        assert_eq!(wire["properties"]["q0_text"]["type"], "string");
        let q1 = &wire["properties"]["q1"];
        assert_eq!(q1["type"], "array");
        assert_eq!(q1["items"]["anyOf"][0]["const"], "a");
    }
    #[test]
    fn form_answers_drop_empty_values() {
        let content = BTreeMap::from([
            ("a".to_string(), ElicitationContentValue::from("x")),
            ("b".to_string(), ElicitationContentValue::from("  ")),
            (
                "c".to_string(),
                ElicitationContentValue::StringArray(vec!["1".into(), "2".into()]),
            ),
            (
                "d".to_string(),
                ElicitationContentValue::StringArray(vec![]),
            ),
        ]);
        let answers = form_answers(Some(content));
        assert_eq!(
            answers,
            Answers::from([
                (FieldKey::new("a"), vec!["x".into()]),
                (FieldKey::new("c"), vec!["1".into(), "2".into()]),
            ])
        );
        assert!(form_answers(None).is_empty());
    }
}
