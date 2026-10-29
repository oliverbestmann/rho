//! `Write`: creates or overwrites a file.

use anyhow::{Context as _, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::{Tool, ToolKind, input_schema, parse, resolve};

pub const WRITE: ToolName = ToolName::from_static("Write");

#[derive(Deserialize, JsonSchema)]
struct WriteInput {
    path: String,
    content: String,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

pub struct Write;

impl Tool for Write {
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Write only for new files or complete rewrites."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: WRITE,
            description:
                "Create or overwrite a file with the given content. Creates parent directories. Sandboxed like Bash."
                    .into(),
            input_schema: input_schema::<WriteInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: WriteInput = parse(input)?;
        let path = resolve(ctx.cwd(), &i.path);
        let len = i.content.len();
        ctx.run_sandboxed(move || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, i.content)
        })
        .await?
        .with_context(|| format!("writing {}", i.path))?;
        Ok(format!("wrote {len} bytes to {}", i.path).into())
    }
}
