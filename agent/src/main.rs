//! The `rho` binary: picks the extensions, everything else lives in the rho crate.

mod go_check;
mod ripgrep_gate;

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rho::run(rho::Options {
        extensions: vec![
            Arc::new(rho::sandbox::GrantCommands),
            Arc::new(ripgrep_gate::RipgrepGate),
            Arc::new(go_check::GoCheck::default()),
        ],
        ..Default::default()
    })
    .await
}
