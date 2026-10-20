//! Saving a session: its history and state go to its `Store`, if it has one.

use anyhow::Result;

use crate::extension::Diff;
use crate::provider::Content;
use crate::provider::{NoteType, ToolCallId};
use crate::store::{State, Store};

use super::Session;

/// How much of a session is in its store.
pub(super) struct Persist {
    pub(super) store: Store,
    pub(super) epoch: i64,
    /// `seq` of the first block of `history[0]`.
    pub(super) base: i64,
    /// Messages saved, and the block count of the last one: `push_user` can
    /// merge into it.
    pub(super) saved: usize,
    pub(super) last_len: usize,
    /// Blocks saved in this epoch.
    pub(super) blocks: usize,
    /// The session row exists.
    pub(super) created: bool,
    /// A save failed and the user was told; not repeated.
    failed: bool,
    /// Edit diffs not saved yet.
    pub(super) diffs: Vec<(ToolCallId, Diff)>,
    /// Streamed output of tool calls, not saved yet.
    pub(super) outputs: Vec<(ToolCallId, String)>,
}

impl Persist {
    /// Nothing saved yet.
    pub(super) fn new(store: Store) -> Self {
        Self {
            store,
            epoch: 0,
            base: 0,
            saved: 0,
            last_len: 0,
            blocks: 0,
            created: false,
            failed: false,
            diffs: vec![],
            outputs: vec![],
        }
    }
}

/// The blocks of `messages` with their role, in order.
fn blocks_of(
    messages: &[crate::provider::Message],
) -> impl Iterator<Item = (crate::provider::Role, Content)> + '_ {
    messages
        .iter()
        .flat_map(|m| m.content.iter().map(|b| (m.role, b.clone())))
}

impl Session {
    /// Number of compactions of the stored session; 0 if not stored.
    pub(crate) fn epoch(&self) -> i64 {
        self.persist.as_ref().map_or(0, |p| p.epoch)
    }

    /// Writes new history and the session state (model, usage, ...) to the
    /// store, if the session has one. A failure is shown to the user once
    /// and does not stop the session.
    pub async fn save(&mut self) {
        self.push_notes();
        if let Err(e) = self.try_save().await
            && let Some(p) = &mut self.persist
            && !p.failed
        {
            p.failed = true;
            self.ctx.notify(
                NoteType::Info,
                "Session not saved",
                &format!("{}: {e:#}", p.store.path().display()),
                true,
            );
        }
    }

    /// Adds the notices shown since the last call to the history.
    pub(super) fn push_notes(&mut self) {
        let notes = self.ctx.take_notes();
        if !notes.is_empty() {
            super::push_user(&mut self.history, notes);
        }
    }

    /// `save`, but the error is returned instead of shown.
    pub(super) async fn try_save(&mut self) -> Result<()> {
        let Some(p) = &mut self.persist else {
            return Ok(());
        };
        let id = self.ctx.session_id().clone();
        // The history only grows (blocks appended to the last message, new
        // messages), so the new blocks are appended. Anything else rewrites
        // the epoch.
        let grew = self.history.len() >= p.saved
            && p.saved
                .checked_sub(1)
                .is_none_or(|last| self.history[last].content.len() >= p.last_len);
        let (seq, blocks) = if grew {
            let tail = p
                .saved
                .checked_sub(1)
                .map_or(&[][..], |last| &self.history[last].content[p.last_len..]);
            let role = p.saved.checked_sub(1).map(|last| self.history[last].role);
            let mut blocks: Vec<_> = tail.iter().map(|b| (role.unwrap(), b.clone())).collect();
            blocks.extend(blocks_of(&self.history[p.saved..]));
            (p.base + p.blocks as i64, blocks)
        } else {
            (p.base, blocks_of(&self.history).collect())
        };
        let count = blocks.len();
        let state = State {
            id: id.clone(),
            cwd: self.roots[0].clone(),
            additional_dirs: self.roots[1..].to_vec(),
            title: self.ctx.title(),
            model: self.model.clone(),
            thinking: self.thinking,
            usage: self.usage,
            last_usage: self.last_usage,
            cost: self.cost,
            profile: self.ctx.profile().id,
            features: self.ctx.active_features(),
            extension_state: self.ctx.all_stored(),
        };
        let diffs = std::mem::take(&mut p.diffs);
        let outputs = std::mem::take(&mut p.outputs);
        p.store.save_state(state).await?;
        if !blocks.is_empty() || !grew {
            p.store.save_items(&id, p.epoch, seq, blocks).await?;
        }
        if !diffs.is_empty() {
            p.store.save_diffs(&id, diffs).await?;
        }
        if !outputs.is_empty() {
            p.store.save_outputs(&id, outputs).await?;
        }
        p.created = true;
        p.blocks = (seq - p.base) as usize + count;
        p.saved = self.history.len();
        p.last_len = self.history.last().map_or(0, |m| m.content.len());
        Ok(())
    }

    /// The history was replaced (compaction): keep the old messages under
    /// their epoch and save the new history under the next one.
    pub(crate) async fn save_compacted(&mut self) {
        if let Some(p) = &mut self.persist
            && p.created
        {
            match p.store.next_epoch(self.ctx.session_id()).await {
                Ok((epoch, base)) => {
                    (p.epoch, p.base, p.saved, p.last_len, p.blocks) = (epoch, base, 0, 0, 0);
                }
                Err(e) => {
                    self.ctx
                        .notify(NoteType::Info, "Session not saved", &format!("{e:#}"), true);
                    return;
                }
            }
        }
        self.save().await;
    }
}
