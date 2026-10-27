//! The session's slash commands: rho's own (`/compact`, `/fork`, `/usage`,
//! `/redact`, `/sessions:list`) and the extensions'.

use anyhow::{Context as _, Result, bail};

use crate::extension::{CommandName, SlashCommand};

use crate::provider::{Content, NoteType};

use super::{Agent, Session};

impl Agent {
    /// The session's slash commands: rho's own, then the extensions'.
    pub fn commands(&self, session: &Session) -> Vec<SlashCommand> {
        let own = [
            (
                "compact",
                "Summarize the conversation to free up context",
                Some("optional focus for the summary"),
            ),
            (
                "fork",
                "Copy this session into a new one that goes on from here",
                None,
            ),
            (
                "usage",
                "Show token, cache and cost usage of this session",
                None,
            ),
            (
                "redact",
                "Mask a phrase in all saved sessions, the history index and memories",
                Some("exact phrase"),
            ),
            (
                "sessions:list",
                "List the sessions used last, with their ids",
                None,
            ),
        ];
        let own = own.map(|(name, description, hint)| SlashCommand {
            name: CommandName::from_static(name),
            description: description.into(),
            hint: hint.map(Into::into),
        });
        own.into_iter()
            .chain(session.commands.iter().cloned())
            .collect()
    }

    /// Runs `/name args`, one of `commands`. `None` if there is no such
    /// command: the text is a prompt for the model then.
    pub async fn slash_command(
        &self,
        session: &mut Session,
        name: &str,
        args: &str,
    ) -> Option<Result<()>> {
        let result = match name {
            "compact" => self.compact(session, args).await,
            "fork" => fork(session).await,
            "sessions:list" => list(session).await,
            "redact" => self.redact(session, args).await,
            "usage" => {
                let report = session.usage_report();
                reply(session, "/usage", &report);
                Ok(())
            }
            _ if session.commands.iter().any(|c| c.name.as_str() == name) => {
                self.command(session, &CommandName::new(name), args).await;
                Ok(())
            }
            _ => return None,
        };
        // Saves the reply, and `Context::set_stored` values a command changed.
        session.save().await;
        Some(result)
    }

    /// Runs an extension's slash command and shows its reply.
    async fn command(&self, session: &Session, name: &CommandName, args: &str) {
        for ext in &self.extensions {
            if let Some(text) = ext.command(&session.ctx, name, args).await {
                let title = format!("/{name} {args}");
                reply(session, title.trim_end(), &text);
                return;
            }
        }
    }

    /// `/redact <phrase>`: replaces `phrase` with `store::MASK` in the
    /// session's history, in every stored session and, through the
    /// extensions' `redact` hooks, in their stores. Shows a summary.
    pub async fn redact(&self, session: &mut Session, phrase: &str) -> Result<()> {
        if phrase.is_empty() {
            bail!("usage: /redact <phrase>");
        }
        for m in &mut session.history {
            if crate::store::redact_content(&mut m.content, phrase) {
                // The prompt cache no longer matches.
                session.last_request = None;
            }
        }
        if let Some(title) = session.ctx.title().filter(|t| t.contains(phrase)) {
            session
                .ctx
                .restore_title(Some(title.replace(phrase, crate::store::MASK)));
        }
        let mut lines = vec![];
        if let Some(p) = &session.persist {
            let (messages, sessions) = p.store.redact(phrase).await?;
            lines.push(format!(
                "- Sessions: {messages} messages in {sessions} sessions"
            ));
        }
        for ext in &self.extensions {
            if let Some(reply) = ext.redact(&session.ctx, phrase).await {
                lines.push(format!("- {reply}"));
            }
        }
        // Not the phrase: the note is saved.
        reply(session, "/redact", &lines.join("\n"));
        Ok(())
    }
}

/// `/fork`: copies the saved session under a new id, titled "fork of ...".
/// The reply, with a link for the web client, is only in this session; the
/// fork tells the model to retitle it with the next prompt.
async fn fork(session: &mut Session) -> Result<()> {
    session.try_save().await?;
    let store = &session
        .persist
        .as_ref()
        .context("sessions are not saved")?
        .store;
    let from = session.ctx.session_id();
    let original = session.ctx.title().unwrap_or_else(|| from.to_string());
    let title = format!("fork of {original}");
    let id = crate::store::SessionId::generate();
    // Sent with the user's next message: it merges into this one.
    let note = Content::system_note(format!(
        "This session is a fork of \"{original}\". Once the user's message shows where this fork is going, give the session a new title that fits it."
    ));
    store.fork(from, &id, &title, note).await?;
    // The web client's route; links in notes open in a new tab.
    let text = title.replace('[', "\\[").replace(']', "\\]");
    reply(
        session,
        "/fork",
        &format!("Forked into [{text}](/session/{id})."),
    );
    Ok(())
}

/// How many sessions `/sessions:list` shows.
const LISTED: usize = 10;

/// `/sessions:list`: the sessions updated last, but not this one and no
/// aliases (ADR 0065), with id, title and cwd.
async fn list(session: &Session) -> Result<()> {
    let store = session.ctx.store().context("sessions are not saved")?;
    // shortcut: fewer than LISTED if most of the newest are aliases; page on if that happens.
    let page = store.list(None, None, 5 * LISTED).await?;
    let this = session.ctx.session_id();
    let lines: Vec<_> = page
        .sessions
        .iter()
        .filter(|s| s.alias_of.is_none() && s.id != *this)
        .take(LISTED)
        .map(|s| {
            let title = s.title.as_deref().unwrap_or("(untitled)");
            format!("- `{}` {title} (`{}`)", s.id, s.cwd.display())
        })
        .collect();
    let text = if lines.is_empty() {
        "No other sessions.".to_string()
    } else {
        lines.join("\n")
    };
    reply(session, "/sessions:list", &text);
    Ok(())
}

/// Shows a command's reply as a text note, saved with the session.
fn reply(session: &Session, title: &str, text: &str) {
    session.ctx.notify(NoteType::Text, title, text, false);
}
