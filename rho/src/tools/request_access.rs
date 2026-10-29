//! `RequestAccess`: the model asks the user for more sandbox access.

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::{Tool, input_schema, parse, resolve};
use crate::sandbox::Access;

pub const REQUEST_ACCESS: ToolName = ToolName::from_static("RequestAccess");

#[derive(Deserialize, JsonSchema)]
struct RequestAccessInput {
    /// file or directory; relative paths are resolved against the working directory
    path: String,
    access: Access,
    /// why the access is needed, shown to the user
    reason: String,
}

/// Lets the model ask the user for more sandbox access.
pub struct RequestAccess;

impl Tool for RequestAccess {
    /// It only asks; the grant follows the answer.
    fn repeatable(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: REQUEST_ACCESS,
            description: "Ask the user to allow Bash, Read, ReadImage and Write to access a file or directory (and everything below it) for the rest of the session.".into(),
            input_schema: input_schema::<RequestAccessInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: RequestAccessInput = parse(input)?;
        if ctx.sandbox().is_none() {
            return Ok("This session is not sandboxed; no access needed.".into());
        }
        // Canonical, so the user approves the real target and not a symlink.
        let path = resolve(ctx.cwd(), &i.path)
            .canonicalize()
            .with_context(|| format!("{} does not exist", i.path))?;
        let mode = match i.access {
            Access::Read => "read",
            Access::ReadWrite => "read-write",
        };
        let title = format!("Allow {mode} access to {}?\n{}", path.display(), i.reason);
        if !ctx.confirm(None, &title).await {
            bail!("the user denied {mode} access to {}", path.display());
        }
        ctx.grant(&path, i.access);
        Ok(format!("granted {mode} access to {}", path.display()).into())
    }
}
