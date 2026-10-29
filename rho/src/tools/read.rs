//! `Read`: a text file, by lines.

use anyhow::{Context as _, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::{Tool, ToolKind, input_schema, max_output, parse, resolve};

pub const READ: ToolName = ToolName::from_static("Read");

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ReadInput {
    path: String,
    /// first line to read, from 1; default 1
    offset: Option<usize>,
    /// number of lines to read; default all, up to maxOutput
    limit: Option<usize>,
    /// output cap in bytes; default 50 KB, at most 64 KB
    #[schemars(range(min = 1, max = 65536))]
    max_output: Option<usize>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

pub struct Read;

impl Tool for Read {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Read to examine files instead of cat or sed."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: READ,
            description:
                "Read a text file. Relative paths are resolved against the working directory. Sandboxed like Bash. For images use ReadImage."
                    .into(),
            input_schema: input_schema::<ReadInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: ReadInput = parse(input)?;
        let path = resolve(ctx.cwd(), &i.path);
        let text = ctx
            .run_sandboxed(move || std::fs::read_to_string(path))
            .await?
            .with_context(|| format!("reading {}", i.path))?;
        let max = max_output(i.max_output);
        Ok(lines(&text, i.offset.unwrap_or(1).max(1), i.limit, max).into())
    }
}

/// `limit` lines of `text` from line `first` (1-based), cut to whole lines
/// within `max` bytes with a note where to go on.
fn lines(text: &str, first: usize, limit: Option<usize>, max: usize) -> String {
    let mut out: String = text
        .split_inclusive('\n')
        .skip(first - 1)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    if out.len() <= max {
        return out;
    }
    out.truncate(out.floor_char_boundary(max));
    // A line longer than `max` stays cut; the next starts after it.
    let kept = match out.rfind('\n') {
        Some(end) => {
            out.truncate(end + 1);
            out.matches('\n').count()
        }
        None => 1,
    };
    out + &format!("[output truncated; read on with offset {}]", first + kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::MAX_OUTPUT;
    use crate::tools::test_ctx as ctx;
    use serde_json::json;

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

    #[test]
    fn read_pages_by_lines() {
        assert_eq!(lines("a\nb\nc", 2, Some(1), MAX_OUTPUT), "b\n");
        assert_eq!(lines("a\nb\nc", 2, None, MAX_OUTPUT), "b\nc");
        assert_eq!(lines("a\n", 5, None, MAX_OUTPUT), "");
        // Whole lines only, and where to go on.
        let line = "x".repeat(MAX_OUTPUT / 2) + "\n";
        let out = lines(&line.repeat(3), 2, None, MAX_OUTPUT);
        assert_eq!(
            out,
            line.clone() + "[output truncated; read on with offset 3]"
        );
        let out = lines(&"y".repeat(MAX_OUTPUT + 1), 1, None, MAX_OUTPUT);
        assert!(out.ends_with("y[output truncated; read on with offset 2]"));
    }
}
