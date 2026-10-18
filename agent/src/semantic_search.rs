//! `SemanticSearch`: search code by meaning with the zvec-grep CLI (`zg`).
//! Only offered when `zg` is on `PATH`. Runs `zg` in direct mode (no
//! daemon, which would run outside the sandbox) under the session's
//! sandbox, plus read-write access to zg's home and the index directory.
//!
//! rho finds the index itself: under Landlock a parent's index can exist but
//! be unreadable, and zg's own lookup would pick it and fail. Without a
//! readable index, one is created at the directory the search starts from.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use rho::extension::{Context, Extension, SessionSetup, ToolOutput};
use rho::provider::{ToolName, ToolSpec};
use rho::sandbox::{Access, Policy};
use rho::tools::{Tool, input_schema, parse, truncate};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const MODEL: &str = "local/potion-code-16m-v2";
const INDEX_DIR: &str = ".zvec-grep";

pub struct SemanticSearch;

impl Extension for SemanticSearch {
    async fn session_start(&self, session: &mut SessionSetup) {
        let Some(zg) = find_zg(std::env::var_os("PATH").as_deref()) else {
            return;
        };
        let home = match std::env::var_os("ZVEC_GREP_HOME") {
            Some(home) => PathBuf::from(home),
            None => match std::env::var_os("HOME") {
                Some(home) => Path::new(&home).join(INDEX_DIR),
                None => return,
            },
        };
        session
            .tools
            .push(Arc::new(SemanticSearchTool { zg, home }));
    }
}

/// The canonical path of `zg` in `path` (a `PATH` value), so its directory
/// (which holds the libraries it loads) can be allowed in the sandbox.
fn find_zg(path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join("zg"))
        .find(|p| p.is_file())?
        .canonicalize()
        .ok()
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    /// What to look for, in natural language or code, e.g. 'where are retries configured'
    query: String,
    /// Directory to search, absolute or relative to the cwd; default the cwd
    path: Option<String>,
    /// Number of results; default 7, at most 50
    limit: Option<usize>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

struct SemanticSearchTool {
    /// The `zg` binary; tests use a mock.
    zg: PathBuf,
    /// zg's global state (`ZVEC_GREP_HOME`): workspace registry and models.
    home: PathBuf,
}

impl Tool for SemanticSearchTool {
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::Search
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &[
            "Use SemanticSearch for questions about meaning, architecture or call chains, and Grep for exact names or strings.",
        ]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from_static("SemanticSearch"),
            description: "Search code by meaning (embeddings, via zvec-grep). Returns the best matching \
                code chunks with file paths and line numbers. Builds an index on first use, which can take a while."
                .into(),
            input_schema: input_schema::<Input>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: Input = parse(input)?;
        let start = ctx.cwd().join(i.path.as_deref().unwrap_or("."));
        let start = start
            .canonicalize()
            .with_context(|| format!("{} not found", start.display()))?;
        if !start.is_dir() {
            bail!("{} is not a directory", start.display());
        }

        // Under the session's rules, like a Read would.
        let find = start.clone();
        let found = ctx
            .run_sandboxed(move || {
                find.ancestors()
                    .find(|d| std::fs::read(d.join(INDEX_DIR).join("manifest.json")).is_ok())
                    .map(Path::to_path_buf)
            })
            .await?;
        std::fs::create_dir_all(&self.home)
            .with_context(|| format!("creating {}", self.home.display()))?;

        let root = match found {
            Some(root) => root,
            None => {
                // Creating the index dir doubles as the check that `start` is writable.
                let dir = start.join(INDEX_DIR);
                if ctx
                    .run_sandboxed(move || std::fs::create_dir_all(dir))
                    .await?
                    .is_err()
                {
                    bail!(
                        "No index found and {} is not writable; use Grep or ask the user to run `zg --index`.",
                        start.display()
                    );
                }
                let hash = Sha256::digest(start.as_os_str().as_encoded_bytes());
                let name = format!(
                    "{}-{}",
                    start
                        .file_name()
                        .map_or("root".into(), |n| n.to_string_lossy()),
                    hash[..4]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
                let index = [OsStr::new("--index"), start.as_os_str()];
                let rest = ["--mode", "direct", "--embedding", MODEL, "--device", "cpu"];
                let args = index
                    .into_iter()
                    .chain(rest.map(OsStr::new))
                    .chain([OsStr::new("--name"), OsStr::new(&name)]);
                self.zg(ctx, &start, args).await?;
                start
            }
        };

        let limit = i.limit.unwrap_or(7).clamp(1, 50).to_string();
        let args = [
            "--mode",
            "direct",
            "--refresh",
            "wait",
            "--compact",
            "--preview",
            "short",
            "--limit",
            &limit,
            "--",
            &i.query,
        ];
        let out = self.zg(ctx, &root, args.map(OsStr::new)).await?;
        let text = out.trim();
        Ok(if text.is_empty() {
            "(no matches)".into()
        } else {
            truncate(text.into()).into()
        })
    }
}

impl SemanticSearchTool {
    /// The session's effective sandbox, forked for one zg call in `root`:
    /// plus zg's dir (it loads its libraries from there), its home and the
    /// index. `None` when the session is unsandboxed.
    fn sandbox(&self, ctx: &Context, root: &Path) -> Option<Policy> {
        let mut policy = ctx.effective_sandbox()?;
        if let Some(dir) = self.zg.parent() {
            policy.allow(dir, Access::Read);
        }
        policy.allow(&self.home, Access::ReadWrite);
        policy.allow(root.join(INDEX_DIR), Access::ReadWrite);
        Some(policy)
    }

    /// Runs zg in `root` (it takes the query root from its cwd), streams
    /// stderr to the user and returns stdout.
    async fn zg(
        &self,
        ctx: &Context,
        root: &Path,
        args: impl IntoIterator<Item = &OsStr>,
    ) -> Result<String> {
        let mut cmd = tokio::process::Command::new(&self.zg);
        cmd.args(args)
            .current_dir(root)
            .env("ZVEC_GREP_HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(policy) = self.sandbox(ctx, root) {
            policy.apply(&mut cmd).context("setting up the sandbox")?;
        }
        let mut child = cmd.spawn().context("running zg")?;
        let mut stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let out = async {
            let mut buf = vec![];
            stdout.read_to_end(&mut buf).await.map(|_| buf)
        };
        let err = async {
            let mut lines = BufReader::new(stderr).lines();
            let mut all = String::new();
            while let Some(line) = lines.next_line().await? {
                // onnxruntime warns about the CPU on every start.
                if line.contains("cpuid_info") {
                    continue;
                }
                ctx.stream_output(&format!("{line}\n"), true);
                all += &line;
                all.push('\n');
            }
            std::io::Result::Ok(all)
        };
        let (out, err) = tokio::try_join!(out, err).context("reading zg output")?;
        let status = child.wait().await.context("waiting for zg")?;
        if !status.success() {
            bail!("zg failed ({status}): {}", err.trim());
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::extension::{NoFrontend, Profile};
    use rho::store::SessionId;
    use serde_json::json;

    /// Sandboxed with the base policy, which grants only `dir`.
    fn ctx(dir: &Path) -> Context {
        Context::new(
            SessionId::new("s"),
            dir,
            Arc::new(NoFrontend),
            Default::default(),
        )
    }

    /// A tool with a mock `zg` that logs `<cwd> <args>` to `<home>/calls`,
    /// creates a manifest on `--index` and prints `hit` on search.
    struct Mock {
        tool: SemanticSearchTool,
        _bin: tempfile::TempDir,
        _home: tempfile::TempDir,
    }

    fn mock() -> Mock {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let go = crate::go_check::tests::mock_go(
            bin.path(),
            "echo \"$PWD $*\" >> \"$ZVEC_GREP_HOME/calls\"\n\
             [ \"$1\" = --index ] && touch \"$2/.zvec-grep/manifest.json\" && exit 0\n\
             echo hit\n",
        );
        let zg = bin.path().join("zg");
        std::fs::rename(go, &zg).unwrap();
        let tool = SemanticSearchTool {
            zg,
            home: home.path().canonicalize().unwrap(),
        };
        Mock {
            tool,
            _bin: bin,
            _home: home,
        }
    }

    impl Mock {
        /// The calls since the last one, with `dir` shortened to `D`.
        fn calls(&self, dir: &Path) -> String {
            let path = self.tool.home.join("calls");
            let calls = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::remove_file(&path).ok();
            calls.replace(dir.to_str().unwrap(), "D")
        }
    }

    const SEARCH: &str = "--mode direct --refresh wait --compact --preview short";

    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn creates_an_index_then_searches() {
        let m = mock();
        let (_dir, dir) = tempdir();
        let ctx = ctx(&dir);
        let before = ctx.sandbox();
        let out = m.tool.run(json!({"query": "retry"}), &ctx).await.unwrap();
        assert_eq!(out.output, "hit");
        // The zg grants were only for the calls.
        assert_eq!(ctx.sandbox(), before);
        let name = dir.file_name().unwrap().to_str().unwrap();
        let calls = m.calls(&dir);
        let (index, search) = calls.split_once('\n').unwrap();
        assert!(
            index.starts_with(&format!(
                "D --index D --mode direct --embedding {MODEL} --device cpu --name {name}-"
            )),
            "{calls}"
        );
        assert_eq!(search, format!("D {SEARCH} --limit 7 -- retry\n"));

        // The index exists now.
        m.tool
            .run(json!({"query": "x", "limit": 99}), &ctx)
            .await
            .unwrap();
        assert_eq!(m.calls(&dir), format!("D {SEARCH} --limit 50 -- x\n"));
    }

    #[tokio::test]
    async fn uses_a_readable_parent_index() {
        let m = mock();
        let (_dir, dir) = tempdir();
        std::fs::create_dir_all(dir.join("child/.zvec-grep")).unwrap();
        std::fs::create_dir_all(dir.join(".zvec-grep")).unwrap();
        std::fs::write(dir.join(".zvec-grep/manifest.json"), "{}").unwrap();
        // Relative path; the child's empty index dir has no manifest.
        m.tool
            .run(json!({"query": "q", "path": "child"}), &ctx(&dir))
            .await
            .unwrap();
        assert_eq!(m.calls(&dir), format!("D {SEARCH} --limit 7 -- q\n"));
    }

    #[tokio::test]
    async fn skips_an_unreadable_parent_index() {
        let m = mock();
        let (_dir, dir) = tempdir();
        let child = dir.join("child");
        std::fs::create_dir_all(dir.join(".zvec-grep")).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(dir.join(".zvec-grep/manifest.json"), "{}").unwrap();
        // The base policy grants only the child; absolute path.
        m.tool
            .run(
                json!({"query": "q", "path": child.to_str().unwrap()}),
                &ctx(&child),
            )
            .await
            .unwrap();
        let calls = m.calls(&dir);
        assert!(calls.starts_with("D/child --index D/child "), "{calls}");
        assert!(calls.ends_with(&format!("\nD/child {SEARCH} --limit 7 -- q\n")));
    }

    #[tokio::test]
    async fn read_only_without_index_fails() {
        let m = mock();
        let (_dir, dir) = tempdir();
        let ctx = ctx(&dir);
        ctx.set_profiles(vec![Profile {
            read_only: true,
            ..Profile::write()
        }]);
        let err = m.tool.run(json!({"query": "q"}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("not writable"), "{err}");
        assert_eq!(m.calls(&dir), "");
    }

    #[test]
    fn finds_zg_on_path() {
        let (_a, a) = tempdir();
        let (_b, b) = tempdir();
        std::fs::write(b.join("zg"), "").unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(find_zg(Some(&path)), Some(b.join("zg")));
        assert_eq!(find_zg(Some(a.as_os_str())), None);
        assert_eq!(find_zg(None), None);
    }
}
