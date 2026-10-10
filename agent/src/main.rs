//! The `rho` binary: picks the extensions, everything else lives in the rho crate.

mod ask_user;
mod background_task;
mod fetch_url;
mod git_gate;
mod go_check;
mod go_doc;
mod memory;
mod plan_mode;
mod ripgrep_find;
mod session_title;
mod web_ui;

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rho::run(rho::Options {
        extensions: vec![
            // First, so it sees every tool call before others can block it.
            Arc::new(web_ui::WebUi::from_config()),
            Arc::new(rho::sandbox::GrantCommands),
            Arc::new(git_gate::GitGate),
            Arc::new(go_check::GoCheck::default()),
            Arc::new(ripgrep_find::RipgrepFind),
            Arc::new(go_doc::GoDoc),
            Arc::new(fetch_url::FetchUrl),
            Arc::new(memory::Memory),
            Arc::new(session_title::SessionTitle::default()),
            Arc::new(background_task::BackgroundTasks::default()),
            Arc::new(ask_user::AskUser),
            Arc::new(plan_mode::PlanMode),
        ],
        ..Default::default()
    })
    .await
}
