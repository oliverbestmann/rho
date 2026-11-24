//! `/clear`: drops the conversation and starts a new epoch with only the
//! system prompt. The old messages stay in the store for search.

use rho::extension::{CommandName, Context, Extension, SessionSetup, SlashCommand};

const CLEAR: CommandName = CommandName::from_static("clear");
/// The title after `/clear`, until the model sets a new one.
pub const TITLE: &str = "New session";

pub struct Clear;

impl Extension for Clear {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.commands.push(SlashCommand {
            name: CLEAR,
            description: "Clear the conversation, keeping only the system prompt".into(),
            hint: None,
        });
    }

    async fn command(&self, ctx: &Context, name: &CommandName, _args: &str) -> Option<String> {
        if *name != CLEAR {
            return None;
        }
        ctx.request_clear();
        ctx.set_title(TITLE);
        Some("Cleared the conversation.".into())
    }
}
