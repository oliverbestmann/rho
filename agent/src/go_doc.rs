//! `GoDoc`: Go documentation and source via `go doc` (pi's go-doc). Only
//! offered in sessions whose cwd is a Go module. Runs in the session's
//! sandbox, like Bash; the module cache (`~/go`) has to be readable.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::ToolSpec;
use rho::tools::{Tool, input_schema, parse, truncate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub struct GoDoc;

impl Extension for GoDoc {
    async fn session_start(&self, session: &mut SessionSetup) {
        if session.ctx.cwd().join("go.mod").exists() {
            session.tools.push(Arc::new(GoDocTool));
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    /// Package path or package.Symbol to look up, e.g. 'fmt' or 'fmt.Println'
    target: String,
    /// Show source code instead of documentation (go doc -src)
    #[serde(default)]
    src: bool,
    /// Show all docs for the package, including unexported (go doc -all)
    #[serde(default)]
    all: bool,
}

struct GoDocTool;

impl Tool for GoDocTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "GoDoc".into(),
            description: "Look up Go documentation or source code via `go doc`. Pass a package path (e.g. 'fmt') \
                or package.Symbol (e.g. 'fmt.Println' or 'net/http.Client'). Set src to true to print source instead of docs. \
                Use it instead of grepping GOPATH to look up Go standard library or module documentation and source."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let mut cmd = tokio::process::Command::new("go");
        cmd.arg("doc");
        if i.src {
            cmd.arg("-src");
        }
        if i.all {
            cmd.arg("-all");
        }
        cmd.arg("--")
            .arg(&i.target)
            .current_dir(ctx.cwd())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        ctx.sandbox_command(&mut cmd)
            .context("setting up the sandbox")?;
        let out = cmd.output().await.context("running go doc")?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let text = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            bail!("go doc failed ({}): {}", out.status, text.trim());
        }
        let text = stdout.trim();
        Ok(if text.is_empty() {
            "(no output)".into()
        } else {
            truncate(text.into()).into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::{NoFrontend, Profile};
    use serde_json::json;

    async fn tools(dir: &std::path::Path) -> Vec<String> {
        let ctx = Context::new("s", dir, Arc::new(NoFrontend), Default::default());
        let mut setup = SessionSetup {
            ctx,
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![Profile::write()],
        };
        GoDoc.session_start(&mut setup).await;
        setup.tools.iter().map(|t| t.spec().name).collect()
    }

    #[tokio::test]
    async fn only_in_go_modules() {
        let dir = tempfile::tempdir().unwrap();
        assert!(tools(dir.path()).await.is_empty());
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/m\n\ngo 1.21\n",
        )
        .unwrap();
        assert_eq!(tools(dir.path()).await, ["GoDoc"]);
    }

    #[tokio::test]
    async fn looks_up_docs() {
        if std::process::Command::new("go")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("go not installed, skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module example.com/m\n\ngo 1.21\n",
        )
        .unwrap();
        let ctx = Context::new("s", dir.path(), Arc::new(NoFrontend), Default::default());
        // Unsandboxed: the test machine's Go cache may be anywhere.
        ctx.set_sandbox(None);

        let out = GoDocTool
            .run(json!({"target": "strings.TrimSpace"}), &ctx)
            .await
            .unwrap();
        assert!(
            out.output.contains("func TrimSpace(s string) string"),
            "{}",
            out.output
        );
        let out = GoDocTool
            .run(json!({"target": "strings.TrimSpace", "src": true}), &ctx)
            .await
            .unwrap();
        assert!(out.output.contains("return"), "{}", out.output);
        assert!(
            GoDocTool
                .run(json!({"target": "nosuchpkg.X"}), &ctx)
                .await
                .is_err()
        );
    }
}
