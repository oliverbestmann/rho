//! The `rho` binary: picks the extensions, everything else lives in the rho crate.

mod ask_user;
mod background_task;
mod context_notice;
mod fetch_url;
mod git_gate;
mod go_check;
mod go_doc;
mod history;
mod mcp;
mod memory;
mod plan_mode;
mod ponytail;
mod project_context;
mod ripgrep_find;
mod secrets;
mod session_title;
mod skills;

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secrets = secrets::Secrets::new();
    rho::run(rho::Options {
        extensions: vec![
            Arc::new(rho::sandbox::GrantCommands),
            Arc::new(git_gate::GitGate),
            Arc::new(go_check::GoCheck::default()),
            Arc::new(ripgrep_find::RipgrepFind),
            Arc::new(go_doc::GoDoc),
            Arc::new(fetch_url::FetchUrl),
            Arc::new(mcp::Mcp::from_config()),
            Arc::new(memory::Memory::default()),
            Arc::new(history::History::default()),
            Arc::new(session_title::SessionTitle::default()),
            Arc::new(background_task::BackgroundTasks::default()),
            Arc::new(context_notice::ContextNotice),
            Arc::new(ask_user::AskUser),
            Arc::new(plan_mode::PlanMode),
            Arc::new(ponytail::Ponytail),
            Arc::new(skills::Skills::from_config()),
            // After every extension that adds features, so all are listed.
            Arc::new(rho::feature::FeatureCommands),
            // Last, so the project instructions end the system prompt.
            Arc::new(project_context::ProjectContext::default()),
            // After every extension that changes prompts or tool output.
            Arc::new(secrets),
        ],
        ..Default::default()
    })
    .await
}
