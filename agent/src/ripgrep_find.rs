//! `Grep`: find files by name or search their contents with ripgrep (pi's
//! ripgrep-find). Runs `rg` in the session's sandbox, like Bash.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::ToolSpec;
use rho::tools::{Tool, input_schema, parse, truncate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_MAX_RESULTS: usize = 50;

pub struct RipgrepFind;

impl Extension for RipgrepFind {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(Grep));
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Input {
    /// Regex pattern to search file contents for
    pattern: Option<String>,
    /// Glob to filter filenames, e.g. '*.go' or 'src/**/*.ts'
    glob: Option<String>,
    /// Directory to search (default: cwd)
    path: Option<String>,
    /// Lines of context around each match (content search only)
    #[schemars(range(min = 0))]
    context: Option<u32>,
    /// Max lines/files to return (default: 50)
    #[schemars(range(min = 1))]
    max_results: Option<usize>,
}

struct Grep;

impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Grep".into(),
            description: "Find files by name (ripgrep --files) or search file contents (ripgrep pattern match). \
                Respects .gitignore. Provide `pattern` to search contents, or `glob` to just list matching filenames."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let mut cmd = tokio::process::Command::new("rg");
        cmd.args(args(&i))
            .current_dir(ctx.cwd())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        ctx.sandbox_command(&mut cmd)
            .context("setting up the sandbox")?;
        let out = cmd.output().await.context("running rg")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        // rg exits 1 when there are no matches: an empty result, not an error.
        let code = out.status.code();
        if !matches!(code, Some(0 | 1)) {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let text = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            bail!("rg failed ({}): {}", out.status, text.trim());
        }
        Ok(limit(&stdout, i.max_results.unwrap_or(DEFAULT_MAX_RESULTS)).into())
    }
}

fn args(i: &Input) -> Vec<String> {
    let mut args = vec![];
    match &i.pattern {
        // `--regexp` so a pattern starting with `-` is not taken for a flag.
        Some(p) if !p.is_empty() => args.extend(["--regexp".into(), p.clone()]),
        _ => args.push("--files".into()),
    }
    if let Some(glob) = i.glob.as_ref().filter(|g| !g.is_empty()) {
        args.extend(["--glob".into(), glob.clone()]);
    }
    if i.pattern.as_ref().is_some_and(|p| !p.is_empty())
        && let Some(n) = i.context.filter(|n| *n > 0)
    {
        args.extend(["--context".into(), n.to_string()]);
    }
    args.extend(["--color".into(), "never".into(), "--".into()]);
    let path = i.path.as_deref().unwrap_or(".");
    args.push(path.strip_prefix('@').unwrap_or(path).into());
    args
}

/// The first `max` non-empty lines, with a note if there were more.
fn limit(stdout: &str, max: usize) -> String {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return "(no matches)".into();
    }
    let mut text = truncate(lines[..lines.len().min(max)].join("\n"));
    if lines.len() > max {
        text += &format!("\n… truncated ({} total)", lines.len());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::NoFrontend;
    use serde_json::json;

    fn args_of(input: Value) -> Vec<String> {
        args(&parse(input).unwrap())
    }

    #[test]
    fn builds_rg_arguments() {
        assert_eq!(
            args_of(json!({"glob": "*.go"})),
            ["--files", "--glob", "*.go", "--color", "never", "--", "."]
        );
        assert_eq!(
            args_of(json!({"pattern": "-x", "context": 2, "path": "@src"})),
            [
                "--regexp",
                "-x",
                "--context",
                "2",
                "--color",
                "never",
                "--",
                "src"
            ]
        );
        // Context only applies to content searches.
        assert_eq!(
            args_of(json!({"context": 2})),
            ["--files", "--color", "never", "--", "."]
        );
    }

    #[test]
    fn limits_lines() {
        assert_eq!(limit("", 5), "(no matches)");
        assert_eq!(limit("a\n\nb\n", 5), "a\nb");
        assert_eq!(limit("a\nb\nc\n", 2), "a\nb\n… truncated (3 total)");
    }

    #[tokio::test]
    async fn searches_files() {
        if std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("rg not installed, skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "nothing\n").unwrap();
        let ctx = Context::new("s", dir.path(), Arc::new(NoFrontend), Default::default());

        let out = Grep.run(json!({"pattern": "wor"}), &ctx).await.unwrap();
        assert_eq!(out.output, "./a.txt:world");
        let out = Grep.run(json!({"glob": "*.md"}), &ctx).await.unwrap();
        assert_eq!(out.output, "./b.md");
        let out = Grep.run(json!({"pattern": "nope"}), &ctx).await.unwrap();
        assert_eq!(out.output, "(no matches)");
        assert!(Grep.run(json!({"pattern": "("}), &ctx).await.is_err());
    }
}
