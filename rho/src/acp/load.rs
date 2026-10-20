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
use super::{ConnId, State, commands, register, send_commands};

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
        Ok((frontend, options, commands)) => {
            responder.respond(response(options))?;
            frontend.ready(state.conn);
            send_commands(cx, &sid, commands)
        }
        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
    }
}

/// The open session (also one opened by another connection), or the stored
/// one continued.
/// Returns its frontend, config options and commands. Call
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
)> {
    let conn = state.conn;
    // An open session takes the new directories after the replay, so the
    // notice follows the history.
    let (session, ctx, frontend, commands, dirs) = match state.hub.open(sid) {
        Some((session, ctx, frontend, commands)) => {
            if ctx.cwd() != cwd {
                anyhow::bail!("session {} belongs to {}", sid.0, ctx.cwd().display());
            }
            frontend.attach(state.viewer(cx, false));
            (session, ctx, frontend, commands, Some(additional_dirs))
        }
        None => {
            let frontend = AcpFrontend::new(sid, &cwd);
            // The notices of session_start hooks follow the replay.
            frontend.attach(state.viewer(cx, true));
            let session = agent
                .resume_session(
                    &store::SessionId::new(&*sid.0),
                    &cwd,
                    additional_dirs,
                    frontend.clone(),
                )
                .await?;
            let (ctx, commands) = (session.ctx.clone(), commands(agent, &session));
            let session = register(
                agent,
                &state.hub,
                sid,
                session,
                frontend.clone(),
                commands.clone(),
            );
            (session, ctx, frontend, commands, None)
        }
    };
    let (stored, diffs) = match ctx.store() {
        Some(store) if replay => {
            let id = store::SessionId::new(&*sid.0);
            (store.messages(&id).await?, store.diffs(&id).await?)
        }
        _ => Default::default(),
    };
    let Ok(mut session) = session.try_lock() else {
        // A turn holds the session: replay what is saved, the live updates
        // follow. shortcut: the new directories are not applied; a lock-free
        // copy of the roots would fix it.
        if replay {
            let messages: Vec<_> = stored.into_iter().map(|m| (m.epoch, m.message)).collect();
            replay_history(&frontend, conn, &ctx, &messages, &diffs);
        }
        let options = frontend.options();
        return Ok((frontend, options, commands));
    };
    if replay {
        // Compaction keeps older epochs in the store; show them too.
        let epoch = session.epoch();
        let earlier = stored.into_iter().filter(|m| m.epoch < epoch);
        let mut messages: Vec<_> = earlier.map(|m| (m.epoch, m.message)).collect();
        messages.extend(session.history.iter().map(|m| (epoch, m.clone())));
        replay_history(&frontend, conn, &ctx, &messages, &diffs);
    }
    if let Some(dirs) = dirs
        && agent.set_additional_dirs(&mut session, dirs)
    {
        session.save().await;
    }
    // A new connection has not seen it; the frontend also needs it to
    // republish on a model switch.
    session.emit_usage();
    let options = frontend.remember(config_options(&session));
    Ok((frontend, options, commands))
}

/// Sends `(epoch, message)`s as session updates to `conn`, as if they were
/// happening now. A compaction summary (the first text of an epoch after the
/// first) shows as a notice, like when it was made. Edits show their diff
/// while it is in `diffs` (see `Store::save_diffs`).
fn replay_history(
    frontend: &AcpFrontend,
    conn: ConnId,
    ctx: &Context,
    messages: &[(i64, provider::Message)],
    diffs: &HashMap<ToolCallId, crate::extension::Diff>,
) {
    let emit = |ev: AgentEvent<'_>| frontend.send(ev, To::Only(conn));
    let flush = |user: &mut Vec<ContentBlock>| {
        for block in user.drain(..) {
            frontend.send_update(
                SessionUpdate::UserMessageChunk(ContentChunk::new(block)),
                To::Only(conn),
            );
        }
    };
    let mut epoch = 0;
    for (msg_epoch, msg) in messages {
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
    if let Some(title) = ctx.title() {
        emit(AgentEvent::Title(&title));
    }
}
