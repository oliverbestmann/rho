pub mod acp;
pub mod acp_ws;
pub mod agent;
pub mod anthropic;
pub mod auth;
pub mod cli;
pub mod compact;
pub mod config;
pub mod copilot;
pub mod extension;
pub mod feature;
pub mod form;
pub mod id;
pub mod models;
pub mod openai;
pub mod profile;
pub mod prompt;
pub mod provider;
pub mod sandbox;
pub mod store;
pub mod tools;

use std::path::PathBuf;

pub use cli::{Options, run};
pub use tools::builtin;

/// `$XDG_CONFIG_HOME/rho`, else `~/.config/rho`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("rho")
}
