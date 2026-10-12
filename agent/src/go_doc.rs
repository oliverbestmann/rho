//! `GoDoc`: Go documentation and source via `go doc` (pi's go-doc). Only
//! offered in sessions whose cwd is a Go module. Runs in the session's
//! sandbox, like Bash; the module cache (`~/go`) has to be readable.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse, truncate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub struct GoDoc;

impl Extension for GoDoc {
    async fn session_start(&self, session: &mut SessionSetup) {
        if session.ctx.cwd().join("go.mod").exists() {
            session.tools.push(Arc::new(GoDocTool { go: "go".into() }));
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

struct GoDocTool {
    /// The `go` binary; tests use a mock.
    go: PathBuf,
}

impl Tool for GoDocTool {
    fn rules(&self) -> &'static [&'static str] {
        &["Use GoDoc for Go documentation and source instead of searching the module cache."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("GoDoc"),
            description: "Look up Go documentation or source code via `go doc`. Pass a package path (e.g. 'fmt') \
                or package.Symbol (e.g. 'fmt.Println' or 'net/http.Client'). Set src to true to print source instead of docs. \
                Use it instead of grepping GOPATH to look up Go standard library or module documentation and source."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let mut cmd = tokio::process::Command::new(&self.go);
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
    use rho::store::SessionId;
    use serde_json::json;

    async fn tools(dir: &std::path::Path) -> Vec<String> {
        let ctx = Context::new(
            SessionId::new("s"),
            dir,
            Arc::new(NoFrontend),
            Default::default(),
        );
        let mut setup = SessionSetup {
            ctx,
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![Profile::write()],
            features: vec![],
        };
        GoDoc.session_start(&mut setup).await;
        setup
            .tools
            .iter()
            .map(|t| t.spec().name.to_string())
            .collect()
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
        let dir = tempfile::tempdir().unwrap();
        let go = crate::go_check::tests::mock_go(
            dir.path(),
            "case \"$*\" in *nosuchpkg*) echo 'doc: no such package' >&2; exit 1;; esac\necho \"$*\"\n",
        );
        let tool = GoDocTool { go };
        // Sandboxed with the base policy, which grants the mock's dir.
        let ctx = Context::new(
            SessionId::new("s"),
            dir.path(),
            Arc::new(NoFrontend),
            Default::default(),
        );
        let run = async |input| tool.run(input, &ctx).await;

        let out = run(json!({"target": "strings.TrimSpace"})).await.unwrap();
        assert_eq!(out.output, "doc -- strings.TrimSpace");
        let out = run(json!({"target": "fmt", "src": true, "all": true}))
            .await
            .unwrap();
        assert_eq!(out.output, "doc -src -all -- fmt");
        let err = run(json!({"target": "nosuchpkg.X"})).await.unwrap_err();
        assert!(err.to_string().contains("doc: no such package"), "{err}");
    }
}
