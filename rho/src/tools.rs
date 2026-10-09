//! The `Tool` plug-in trait and the built-in tools.
//!
//! Built-in names use Claude Code casing (`Read`, `Write`, `Bash`), which is
//! what subscription requests are expected to carry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::extension::{BoxFuture, Context};
use crate::provider::ToolSpec;

/// Output larger than this is cut off so it does not flood the context.
pub const MAX_OUTPUT: usize = 50 * 1024;

/// A tool the model can call. Implement `run` with `async fn`.
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Runs the tool. `Err` is reported to the model as a tool error.
    fn run(&self, input: Value, ctx: &Context) -> impl Future<Output = Result<String>> + Send;
}

/// Object-safe form of [`Tool`], implemented for every tool.
pub trait DynTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn run<'a>(&'a self, input: Value, ctx: &'a Context) -> BoxFuture<'a, Result<String>>;
}

impl<T: Tool> DynTool for T {
    fn spec(&self) -> ToolSpec {
        Tool::spec(self)
    }
    fn run<'a>(&'a self, input: Value, ctx: &'a Context) -> BoxFuture<'a, Result<String>> {
        Box::pin(Tool::run(self, input, ctx))
    }
}

pub fn builtin() -> Vec<Arc<dyn DynTool>> {
    vec![Arc::new(Read), Arc::new(Write), Arc::new(Bash)]
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    cwd.join(path)
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        let mut end = MAX_OUTPUT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n[output truncated]");
    }
    s
}

/// Parses a tool's input into its input struct.
pub fn parse<T: DeserializeOwned>(input: Value) -> Result<T> {
    serde_json::from_value(input).context("invalid tool input")
}

/// The JSON schema of a tool's input struct, for `ToolSpec::input_schema`.
/// Doc comments on fields become their descriptions.
pub fn input_schema<T: JsonSchema>() -> Value {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .with(|s| s.meta_schema = None)
        .into_generator()
        .into_root_schema_for::<T>();
    schema.remove("title");
    schema.to_value()
}

#[derive(Deserialize, JsonSchema)]
struct ReadInput {
    path: String,
}

#[derive(Deserialize, JsonSchema)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    command: String,
    /// seconds, default 120
    timeout: Option<f64>,
}

pub struct Read;

impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description:
                "Read a text file. Relative paths are resolved against the working directory."
                    .into(),
            input_schema: input_schema::<ReadInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<String> {
        let cwd = ctx.cwd();
        {
            let i: ReadInput = parse(input)?;
            let text = tokio::fs::read_to_string(resolve(cwd, &i.path))
                .await
                .with_context(|| format!("reading {}", i.path))?;
            Ok(truncate(text))
        }
    }
}

pub struct Write;

impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description:
                "Create or overwrite a file with the given content. Creates parent directories."
                    .into(),
            input_schema: input_schema::<WriteInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<String> {
        let cwd = ctx.cwd();
        {
            let i: WriteInput = parse(input)?;
            let path = resolve(cwd, &i.path);
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir).await?;
            }
            tokio::fs::write(&path, &i.content)
                .await
                .with_context(|| format!("writing {}", i.path))?;
            Ok(format!("wrote {} bytes to {}", i.content.len(), i.path))
        }
    }
}

pub struct Bash;

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "Run a shell command in the working directory. Returns stdout, stderr and the exit code.".into(),
            input_schema: input_schema::<BashInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<String> {
        let cwd = ctx.cwd();
        {
            let i: BashInput = parse(input)?;
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&i.command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                // Cancelling the prompt drops this future; take the process with it.
                .kill_on_drop(true)
                .output();
            let timeout = Duration::from_secs_f64(i.timeout.unwrap_or(120.0));
            let Ok(out) = tokio::time::timeout(timeout, child).await else {
                bail!("command timed out after {timeout:?}");
            };
            let out = out?;
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let text = truncate(text);
            if !out.status.success() {
                bail!("{text}\n[{}]", out.status);
            }
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bash, Context, MAX_OUTPUT, Read, Tool, Write, truncate};
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;

    fn ctx(dir: &Path) -> Context {
        Context::new(
            "t",
            dir,
            Arc::new(crate::extension::NoFrontend),
            Default::default(),
        )
    }

    #[test]
    fn schema_matches_input_struct() {
        assert_eq!(
            Bash.spec().input_schema,
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": ["number", "null"], "format": "double", "description": "seconds, default 120"},
                },
                "required": ["command"],
            })
        );
    }

    #[tokio::test]
    async fn write_then_read() {
        let dir = tempfile::tempdir().unwrap();
        Write
            .run(
                json!({"path": "a/b.txt", "content": "hello"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        let out = Read
            .run(json!({"path": "a/b.txt"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test]
    async fn read_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Read.run(json!({"path": "nope"}), &ctx(dir.path()))
                .await
                .is_err()
        );
        assert!(Read.run(json!({}), &ctx(dir.path())).await.is_err());
    }

    #[tokio::test]
    async fn bash_runs_in_cwd_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "").unwrap();
        let out = Bash
            .run(json!({"command": "ls"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out.trim(), "marker");

        let err = Bash
            .run(
                json!({"command": "echo oops >&2; exit 3"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("oops") && err.contains('3'), "{err}");

        let err = Bash
            .run(
                json!({"command": "sleep 5", "timeout": 0.1}),
                &ctx(dir.path()),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(MAX_OUTPUT);
        let t = truncate(s);
        assert!(t.ends_with("[output truncated]"));
    }
}
