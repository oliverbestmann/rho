//! Saving a session: its history and state go to its `Store`, if it has one.

use anyhow::Result;

use crate::extension::Diff;
use crate::provider::ToolCallId;
use crate::store::{State, Store};

use super::Session;

/// How much of a session is in its store.
pub(super) struct Persist {
    pub(super) store: Store,
    pub(super) epoch: i64,
    /// `seq` of `history[0]`.
    pub(super) base: i64,
    /// Messages saved, and the block count of the last one: `push_user` can
    /// merge into it.
    pub(super) saved: usize,
    pub(super) last_len: usize,
    /// The session row exists.
    pub(super) created: bool,
    /// A save failed and the user was told; not repeated.
    failed: bool,
    /// Edit diffs not saved yet.
    pub(super) diffs: Vec<(ToolCallId, Diff)>,
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
            created: false,
            failed: false,
            diffs: vec![],
        }
    }
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
        if let Err(e) = self.try_save().await
            && let Some(p) = &mut self.persist
            && !p.failed
        {
            p.failed = true;
            self.ctx.notify(
                "Session not saved",
                &format!("{}: {e:#}", p.store.path().display()),
                true,
            );
        }
    }

    /// `save`, but the error is returned instead of shown.
    pub(super) async fn try_save(&mut self) -> Result<()> {
        let Some(p) = &mut self.persist else {
            return Ok(());
        };
        let id = self.ctx.session_id().clone();
        // Rewrite the last saved message if it grew, and drop rows past the
        // end if the history shrank.
        let from = match p.saved.checked_sub(1) {
            Some(last) if self.history.get(last).map(|m| m.content.len()) != Some(p.last_len) => {
                last
            }
            _ => p.saved,
        }
        .min(self.history.len());
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
        };
        let messages = self.history[from..].to_vec();
        let diffs = std::mem::take(&mut p.diffs);
        p.store.save_state(state).await?;
        p.store
            .save_messages(&id, p.epoch, p.base + from as i64, messages)
            .await?;
        if !diffs.is_empty() {
            p.store.save_diffs(&id, diffs).await?;
        }
        p.created = true;
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
                    (p.epoch, p.base, p.saved, p.last_len) = (epoch, base, 0, 0);
                }
                Err(e) => {
                    self.ctx
                        .notify("Session not saved", &format!("{e:#}"), true);
                    return;
                }
            }
        }
        self.save().await;
    }
}
