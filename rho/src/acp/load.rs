//! `session/load` and `session/resume`: open a stored session (or attach
//! to an open one) and replay its history.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AvailableCommand, ContentBlock, ContentChunk, SessionConfigOption, SessionId, SessionUpdate,
};
use agent_client_protocol::{Client, ConnectionTo, Error, JsonRpcResponse, Responder};

use crate::agent::{Agent, AgentEvent};
use crate::extension::Context;
use crate::provider::{self, Content, NoteType, Role, ToolCallId};
use crate::store;
use crate::tools;

use super::config::config_options;
use super::frontend::{AcpFrontend, To};
use super::update::image_block;
use super::{State, commands, register, run_unprompted, send_commands};

/// `session/load` (with `replay`) and `session/resume`: opens the session
/// with `open_stored`, answers with `response`, then makes the connection
/// ready and sends the commands.
#[expect(clippy::too_many_arguments, reason = "the two handlers' shared body")]
pub(super) async fn attach<T: JsonRpcResponse>(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: bool,
    responder: Responder<T>,
    response: impl FnOnce(Vec<SessionConfigOption>) -> T,
) -> Result<(), Error> {
    match open_stored(agent, state, cx, &sid, cwd, additional_dirs, replay).await {
        Ok((frontend, options, commands, resume)) => {
            responder.respond(response(options))?;
            frontend.ready(state.conn);
            if let Some(ctx) = resume {
                let (agent, hub) = (agent.clone(), state.hub.clone());
                let sid = sid.clone();
                tokio::spawn(async move { run_unprompted(&agent, &hub, &sid, &ctx).await });
            }
            send_commands(cx, &sid, commands)
        }
        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
    }
}

/// The open session (also one opened by another connection), or the stored
/// one continued. With `replay` the connection gets the transcript first.
/// Returns its frontend, config options, commands and, for a session that
/// goes on with open tool calls, its context to start that run with. Call
/// `AcpFrontend::ready` after the response.
async fn open_stored(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: bool,
) -> anyhow::Result<(
    Arc<AcpFrontend>,
    Vec<SessionConfigOption>,
    Vec<AvailableCommand>,
    Option<Context>,
)> {
    let opening = state.hub.opening.lock().await;
    // An open session takes the new directories after the replay, so the
    // notice follows the history.
    let (session, ctx, frontend, commands, dirs, resuming) = match state.hub.open(sid) {
        Some((session, ctx, frontend, commands)) => {
            if ctx.cwd() != cwd {
                anyhow::bail!("session {} belongs to {}", sid.0, ctx.cwd().display());
            }
            (
                session,
                ctx,
                frontend,
                commands,
                Some(additional_dirs),
                false,
            )
        }
        None => {
            let frontend = AcpFrontend::new(sid, &cwd);
            let session = agent
                .resume_session(
                    &store::SessionId::new(&*sid.0),
                    &cwd,
                    additional_dirs,
                    frontend.clone(),
                )
                .await?;
            record_stored(&frontend, &session).await?;
            let resuming = session.resuming();
            let (ctx, commands) = (session.ctx.clone(), commands(agent, &session));
            let session = register(
                agent,
                &state.hub,
                sid,
                session,
                frontend.clone(),
                commands.clone(),
            );
            (session, ctx, frontend, commands, None, resuming)
        }
    };
    drop(opening);
    let viewer = state.viewer(cx, false);
    if replay {
        frontend.attach_replayed(viewer);
        if let Some(title) = ctx.title() {
            frontend.send(AgentEvent::Title(&title), To::Only(state.conn));
        }
    } else {
        frontend.attach(viewer);
    }
    // A turn holds the session: the new directories are not applied.
    // shortcut: a lock-free copy of the roots would fix it.
    let Ok(mut session) = session.try_lock() else {
        // The usage the others saw; the run's next request sends a new one.
        frontend.resend_usage(state.conn, ctx.session_info().context_window);
        let options = frontend.options();
        return Ok((frontend, options, commands, None));
    };
    if let Some(dirs) = dirs
        && agent.set_additional_dirs(&mut session, dirs)
    {
        session.save().await;
    }
    // A new connection has not seen it; the frontend also needs it to
    // republish on a model switch.
    session.emit_usage();
    let options = frontend.remember(config_options(&session));
    // Asks again what a restart left open, after the response.
    let resume = resuming.then_some(ctx);
    Ok((frontend, options, commands, resume))
}

/// Puts the history of a session that opens into its transcript: the
/// previous epoch from the store, then the current one, before the notices
/// of its start. Compaction keeps older epochs in the store; they are not
/// shown, so they do not fill the clients' memory.
async fn record_stored(
    frontend: &AcpFrontend,
    session: &crate::agent::Session,
) -> anyhow::Result<()> {
    let tools = session.tools().map(|(n, t)| (n.clone(), t.clone()));
    let _ = frontend.tools.set(tools.collect());
    let epoch = session.epoch();
    let (mut messages, diffs, outputs) = match session.ctx.store() {
        Some(store) => {
            let id = session.ctx.session_id();
            let stored = store.messages(id).await?;
            let earlier = stored.into_iter().filter(|m| m.epoch + 1 == epoch);
            let messages: Vec<_> = earlier.map(|m| (m.epoch, m.message)).collect();
            (messages, store.diffs(id).await?, store.outputs(id).await?)
        }
        None => Default::default(),
    };
    messages.extend(session.history.iter().map(|m| (epoch, m.clone())));
    frontend.record_first(|| record_history(frontend, &messages, &diffs, &outputs));
    Ok(())
}

/// Records `(epoch, message)`s in the transcript as the session updates
/// they were. A compaction summary (the first text of an epoch after the
/// first) shows as a notice, like when it was made. Edits show their diff
/// while it is in `diffs` (see `Store::save_diffs`), terminal calls their
/// streamed output while it is in `outputs`.
fn record_history(
    frontend: &AcpFrontend,
    messages: &[(i64, provider::Message)],
    diffs: &HashMap<ToolCallId, crate::extension::Diff>,
    outputs: &HashMap<ToolCallId, String>,
) {
    let emit = |ev: AgentEvent<'_>| frontend.send(ev, To::Record);
    let flush = |user: &mut Vec<ContentBlock>| {
        for block in user.drain(..) {
            frontend.record(SessionUpdate::UserMessageChunk(ContentChunk::new(block)));
        }
    };
    // Before the first: its first text may be a summary too.
    let mut epoch = -1;
    for (msg_epoch, msg) in messages {
        if epoch >= 0 && *msg_epoch != epoch {
            frontend.new_epoch();
        }
        let mut user: Vec<ContentBlock> = vec![];
        // Since notes are saved, compaction's own notice follows the summary.
        let noted = msg
            .content
            .iter()
            .any(|c| matches!(c, Content::UserNote { .. }));
        for block in &msg.content {
            let first = std::mem::replace(&mut epoch, *msg_epoch) != *msg_epoch;
            if first
                && let Content::Text { text } = block
                && let Some(summary) = crate::compact::summary_of(text)
            {
                if !noted {
                    emit(AgentEvent::Notice {
                        kind: NoteType::Info,
                        title: "Compacted",
                        text: summary,
                        is_error: false,
                    });
                }
                continue;
            }
            match block {
                Content::SystemNote { .. } => {}
                Content::UserNote {
                    kind,
                    title,
                    text,
                    is_error,
                } => {
                    flush(&mut user);
                    emit(AgentEvent::Notice {
                        kind: *kind,
                        title,
                        text,
                        is_error: *is_error,
                    });
                }
                Content::Text { text } if msg.role == Role::User => match user.last_mut() {
                    Some(ContentBlock::Text(last)) => {
                        last.text.push_str("\n\n");
                        last.text.push_str(text);
                    }
                    _ => user.push(text.as_str().into()),
                },
                Content::Image { source } => user.push(image_block(source)),
                Content::Text { text } => emit(AgentEvent::Text(text)),
                Content::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                    emit(AgentEvent::Thinking(thinking))
                }
                Content::Thinking { .. } => {}
                Content::ToolUse { id, name, input } => {
                    let tool = frontend.tools.get().and_then(|t| t.get(name));
                    let description = tool.and_then(|t| t.describe(input));
                    emit(AgentEvent::ToolStart {
                        id,
                        name,
                        input,
                        description: description.as_deref(),
                        kind: tool.map_or_else(tools::ToolKind::default, |t| t.kind()),
                        titled: tool.is_some_and(|t| t.titled()),
                    });
                    // The result is in the next message; the output goes
                    // between, as it streamed.
                    if let Some(output) = outputs.get(id) {
                        emit(AgentEvent::ToolStream {
                            id,
                            chunk: output,
                            stderr: false,
                        });
                    }
                }
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    images,
                } => emit(AgentEvent::ToolEnd {
                    id: tool_use_id,
                    output: content,
                    is_error: *is_error,
                    diff: diffs.get(tool_use_id),
                    images,
                }),
                Content::RedactedThinking { .. } => {}
            }
        }
        flush(&mut user);
    }
}
