//! `Glob` finds files by name and `Grep` searches their contents, with
//! ripgrep, like Claude Code's tools of the same names. Runs `rg` in the
//! session's sandbox, like Bash.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse, truncate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_MAX_RESULTS: usize = 50;

pub struct RipgrepFind;

impl Extension for RipgrepFind {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(Glob));
        session.tools.push(Arc::new(Grep));
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Input {
    /// Regex pattern to search file contents for
    pattern: String,
    /// Glob to filter filenames, e.g. '*.go' or 'src/**/*.ts'
    glob: Option<String>,
    /// Directory to search (default: cwd)
    path: Option<String>,
    /// files_with_matches (default): only the file names; content: the matching lines as path:line:text; count: matches per file
    #[serde(default)]
    output_mode: OutputMode,
    /// Lines of context around each match (content mode only)
    #[schemars(range(min = 0))]
    context: Option<u32>,
    /// Max lines/files to return (default: 50)
    #[schemars(range(min = 1))]
    max_results: Option<usize>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GlobInput {
    /// Glob for file names, e.g. '**/*.go' or 'src/**/*.ts'
    pattern: String,
    /// Directory to search (default: cwd)
    path: Option<String>,
    /// Max files to return (default: 50)
    #[schemars(range(min = 1))]
    max_results: Option<usize>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

struct Grep;

impl Tool for Grep {
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::Search
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Grep to search file contents instead of rg or grep in Bash."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("Grep"),
            description: "Search file contents with ripgrep. Respects .gitignore. \
                Lists the matching files by default; set outputMode to content for the matching lines."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        rg(args(&i), i.max_results, ctx).await
    }
}

struct Glob;

impl Tool for Glob {
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::Search
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Glob to find files by name instead of find or ls in Bash."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("Glob"),
            description:
                "Find files by name with a glob pattern (ripgrep --files). Respects .gitignore. \
                Returns the paths, most recently modified first."
                    .into(),
            input_schema: input_schema::<GlobInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: GlobInput = parse(input)?;
        rg(glob_args(&i), i.max_results, ctx).await
    }
}

/// Runs `rg` with `args` in the session's sandbox; at most `max` lines.
async fn rg(args: Vec<String>, max: Option<usize>, ctx: &Context) -> Result<ToolOutput> {
    let mut cmd = tokio::process::Command::new("rg");
    cmd.args(args)
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
    Ok(limit(&stdout, max.unwrap_or(DEFAULT_MAX_RESULTS)).into())
}

fn args(i: &Input) -> Vec<String> {
    // `--regexp` so a pattern starting with `-` is not taken for a flag.
    let mut args = vec!["--regexp".into(), i.pattern.clone()];
    if let Some(glob) = i.glob.as_ref().filter(|g| !g.is_empty()) {
        args.extend(["--glob".into(), glob.clone()]);
    }
    match i.output_mode {
        OutputMode::FilesWithMatches => args.push("--files-with-matches".into()),
        OutputMode::Count => args.push("--count".into()),
        OutputMode::Content => {
            // Line numbers let the model Read or Edit around a match.
            args.push("--line-number".into());
            if let Some(n) = i.context.filter(|n| *n > 0) {
                args.extend(["--context".into(), n.to_string()]);
            }
        }
    }
    finish(args, i.path.as_deref())
}

fn glob_args(i: &GlobInput) -> Vec<String> {
    let args = vec![
        "--files".into(),
        "--glob".into(),
        i.pattern.clone(),
        "--sortr".into(),
        "modified".into(),
    ];
    finish(args, i.path.as_deref())
}

fn finish(mut args: Vec<String>, path: Option<&str>) -> Vec<String> {
    args.extend(["--color".into(), "never".into(), "--".into()]);
    let path = path.unwrap_or(".");
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
    use rho::store::SessionId;
    use serde_json::json;

    fn args_of(input: Value) -> Vec<String> {
        args(&parse(input).unwrap())
    }

    #[test]
    fn builds_rg_arguments() {
        assert!(parse::<Input>(json!({"glob": "*.go"})).is_err());
        assert_eq!(
            args_of(
                json!({"pattern": "-x", "outputMode": "content", "context": 2, "path": "@src"})
            ),
            [
                "--regexp",
                "-x",
                "--line-number",
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
            args_of(json!({"pattern": "x", "context": 2})),
            [
                "--regexp",
                "x",
                "--files-with-matches",
                "--color",
                "never",
                "--",
                "."
            ]
        );
        assert_eq!(
            args_of(json!({"pattern": "x", "outputMode": "count"})),
            ["--regexp", "x", "--count", "--color", "never", "--", "."]
        );
        let glob: GlobInput = parse(json!({"pattern": "*.go", "path": "src"})).unwrap();
        assert_eq!(
            glob_args(&glob),
            [
                "--files", "--glob", "*.go", "--sortr", "modified", "--color", "never", "--", "src"
            ]
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
        let ctx = Context::new(
            SessionId::new("s"),
            dir.path(),
            Arc::new(NoFrontend),
            Default::default(),
        );

        let out = Grep.run(json!({"pattern": "wor"}), &ctx).await.unwrap();
        assert_eq!(out.output, "./a.txt");
        let content = json!({"pattern": "wor", "outputMode": "content"});
        let out = Grep.run(content, &ctx).await.unwrap();
        assert_eq!(out.output, "./a.txt:2:world");
        let count = json!({"pattern": "o", "glob": "*.txt", "outputMode": "count"});
        assert_eq!(Grep.run(count, &ctx).await.unwrap().output, "./a.txt:2");
        // Newest first.
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("a.txt"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let out = Glob.run(json!({"pattern": "*"}), &ctx).await.unwrap();
        assert_eq!(out.output, "./a.txt\n./b.md");
        let out = Glob.run(json!({"pattern": "*.md"}), &ctx).await.unwrap();
        assert_eq!(out.output, "./b.md");
        let out = Grep.run(json!({"pattern": "nope"}), &ctx).await.unwrap();
        assert_eq!(out.output, "(no matches)");
        assert!(Grep.run(json!({"pattern": "("}), &ctx).await.is_err());
    }
}
