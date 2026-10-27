//! `/session:join <id>` (ADR 0065): the session becomes an alias of another
//! one. Its client gets that one's history and from then on talks to it.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ConfigOptionUpdate, PromptResponse, SessionId, SessionUpdate, StopReason,
};
use agent_client_protocol::{Client, ConnectionTo, Error, Responder};
use anyhow::{Context as _, bail};

use crate::agent::Agent;

use super::State;
use super::frontend::{Cursor, To};
use super::load::{Opened, Replay, open_stored, ready};

pub(super) const COMMAND: &str = "session:join";

/// Runs `/session:join <target>` in `sid` and answers its prompt. `target`
/// is a session id or a prefix that only one session has.
pub(super) async fn join(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: SessionId,
    target: String,
    responder: Responder<PromptResponse>,
) -> Result<(), Error> {
    let Opened {
        frontend,
        options,
        commands,
        resume,
    } = match open_joined(agent, state, cx, &sid, &target).await {
        Ok(opened) => opened,
        Err(e) => {
            return responder.respond_with_error(Error::invalid_params().data(format!("{e:#}")));
        }
    };
    responder.respond(PromptResponse::new(StopReason::EndTurn))?;
    // The client still shows the options of the session it left.
    let update = SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options));
    frontend.send_update(update, To::Only(state.conn));
    ready(agent, state, cx, &sid, &frontend, commands, resume)
}

/// Makes `sid` an alias of `target` in the store and the hub, ends the
/// session `sid` was, and opens `target` for the connection with a full
/// replay. Other connections that look at `sid` lose it until they load it
/// again.
async fn open_joined(
    agent: &Arc<Agent>,
    state: &Arc<State>,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
    target: &str,
) -> anyhow::Result<Opened> {
    if target.is_empty() {
        bail!("usage: /{COMMAND} <session id or its start>, see /sessions:list");
    }
    // It would leave the session it stands for to the other clients.
    if state.hub.is_alias(sid) {
        bail!("this session joined another one already; join from a new session");
    }
    let (session, ..) = state
        .hub
        .session(sid, state.conn)
        .context("unknown session")?;
    if !state.hub.idle(sid) {
        bail!("wait until the turn and the queued prompts are done");
    }
    let info = {
        let mut session = session.lock().await;
        session.save().await;
        let store = session.ctx.store().context("sessions are not saved")?;
        store.join(session.ctx.session_id(), target).await?
    };
    let joined = SessionId::new(info.id.as_str());
    // The last references: the session ends here.
    drop(state.hub.alias(sid, &joined));
    drop(session);
    // A client that keeps a cursor (the web client) starts over.
    let replay = Replay::Since(Some(Cursor::default()));
    open_stored(
        agent,
        state,
        cx,
        &joined,
        sid,
        info.cwd,
        info.additional_dirs,
        replay,
    )
    .await
}
