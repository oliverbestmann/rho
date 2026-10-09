//! The `rho` binary: picks the extensions, everything else lives in the rho crate.

mod ask_user;
mod background_task;
mod go_check;
mod ripgrep_gate;
mod session_title;

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rho::run(rho::Options {
        extensions: vec![
            Arc::new(rho::sandbox::GrantCommands),
            Arc::new(ripgrep_gate::RipgrepGate),
            Arc::new(go_check::GoCheck::default()),
            Arc::new(session_title::SessionTitle::default()),
            Arc::new(background_task::BackgroundTasks::default()),
            Arc::new(ask_user::AskUser),
        ],
        ..Default::default()
    })
    .await
}
