//! `session/load` and `session/resume`: open a stored session (or attach
//! to an open one) and replay its history.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
use super::frontend::{AcpFrontend, Cursor, To};
use super::update::image_block;
use super::{State, commands, register, run_unprompted, send_commands};

/// What a connection that opens a session gets before the response.
pub(super) enum Replay {
    /// `session/resume`: nothing.
    Nothing,
    /// `session/load`: the transcript, after the cursor if the client sent
    /// one as `_meta.rho.since`.
    Since(Option<Cursor>),
}

impl Replay {
    /// For `session/load`. A `since` that cannot be read is in no
    /// transcript: the client gets a reset and everything.
    pub(super) fn of_load(meta: Option<&agent_client_protocol::schema::v1::Meta>) -> Self {
        let since = meta.and_then(|m| m.get("rho")?.get("since"));
        Replay::Since(since.map(|s| serde_json::from_value(s.clone()).unwrap_or_default()))
    }
}

/// `session/load` (with a replay) and `session/resume`: opens the session
/// (the one it joined, for an alias) with `open_stored`, answers with
/// `response`, then makes the connection ready and sends the commands.
#[expect(clippy::too_many_arguments, reason = "the two handlers' shared body")]
pub(super) async fn attach<T: JsonRpcResponse>(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: Replay,
    responder: Responder<T>,
    response: impl FnOnce(Vec<SessionConfigOption>) -> T,
) -> Result<(), Error> {
    let opened = async {
        // An alias opens its session where it is, whatever the client says.
        let (target, cwd, dirs) = match joined(agent, &sid, &cwd).await? {
            Some(info) => {
                let target = SessionId::new(info.id.as_str());
                state.hub.alias(&sid, &target);
                (target, info.cwd, info.additional_dirs)
            }
            None => (sid.clone(), cwd, additional_dirs),
        };
        open_stored(agent, state, cx, &target, &sid, cwd, dirs, replay).await
    };
    match opened.await {
        Ok(Opened {
            frontend,
            options,
            commands,
            resume,
        }) => {
            responder.respond(response(options))?;
            ready(agent, state, cx, &sid, &frontend, commands, resume)
        }
        Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
    }
}

/// The stored session `sid` joined, if it did (ADR 0065).
/// shortcut: looks in the database of `cwd` only; look in the target's if
/// `sessions_db` ever splits by cwd.
async fn joined(
    agent: &Agent,
    sid: &SessionId,
    cwd: &Path,
) -> anyhow::Result<Option<store::SessionInfo>> {
    let Some(db) = &agent.sessions_db else {
        return Ok(None);
    };
    let path = db(cwd);
    let store = tokio::task::spawn_blocking(move || store::Store::open(path)).await??;
    let info = store.session(&store::SessionId::new(&*sid.0)).await?;
    match info.and_then(|i| i.alias_of) {
        Some(target) => store.session(&target).await,
        None => Ok(None),
    }
}

/// After the response: the connection that opened the session as `sid` is
/// ready, gets the commands, and a session that goes on with open tool
/// calls starts that run.
pub(super) fn ready(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    frontend: &AcpFrontend,
    commands: Vec<AvailableCommand>,
    resume: Option<Context>,
) -> Result<(), Error> {
    frontend.ready(state.conn);
    if let Some(ctx) = resume {
        let (agent, hub) = (agent.clone(), state.hub.clone());
        let sid = sid.clone();
        tokio::spawn(async move { run_unprompted(&agent, &hub, &sid, &ctx).await });
    }
    send_commands(cx, sid, commands)
}

/// What `open_stored` opened.
pub(super) struct Opened {
    pub(super) frontend: Arc<AcpFrontend>,
    pub(super) options: Vec<SessionConfigOption>,
    pub(super) commands: Vec<AvailableCommand>,
    /// For a session that goes on with open tool calls, its context to
    /// start that run with.
    pub(super) resume: Option<Context>,
}

/// The open session `sid` (also one opened by another connection), or the
/// stored one continued. The connection, which knows it as `alias`, gets
/// `replay` first. Call `ready` after the response.
#[expect(clippy::too_many_arguments, reason = "load, resume and join")]
pub(super) async fn open_stored(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    alias: &SessionId,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    replay: Replay,
) -> anyhow::Result<Opened> {
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
            let frontend = AcpFrontend::new(&cwd);
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
    let viewer = state.viewer(cx, alias, false);
    match replay {
        Replay::Since(since) => {
            frontend.attach_replayed(viewer, since);
            if let Some(title) = ctx.title() {
                frontend.send(AgentEvent::Title(&title), To::Only(state.conn));
            }
        }
        Replay::Nothing => frontend.attach(viewer),
    }
    // A turn holds the session: the new directories are not applied.
    // shortcut: a lock-free copy of the roots would fix it.
    let Ok(mut session) = session.try_lock() else {
        // The usage the others saw; the run's next request sends a new one.
        frontend.resend_usage(state.conn, ctx.session_info().context_window);
        let options = frontend.options();
        return Ok(Opened {
            frontend,
            options,
            commands,
            resume: None,
        });
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
    Ok(Opened {
        frontend,
        options,
        commands,
        resume,
    })
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
                        card: None,
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
                        card: None,
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
