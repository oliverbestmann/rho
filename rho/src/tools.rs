//! The `Tool` plug-in trait and the built-in tools.
//!
//! Built-in names use Claude Code casing (`Read`, `Write`, `Bash`), which is
//! what subscription requests are expected to carry.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::extension::{BoxFuture, Context, ToolOutput};
use crate::provider::{ImageSource, ToolName, ToolSpec};
use crate::sandbox::Access;

pub const READ: ToolName = ToolName::from_static("Read");
pub const READ_IMAGE: ToolName = ToolName::from_static("ReadImage");
pub const WRITE: ToolName = ToolName::from_static("Write");
pub const BASH: ToolName = ToolName::from_static("Bash");
pub const REQUEST_ACCESS: ToolName = ToolName::from_static("RequestAccess");

/// Output larger than this is cut off so it does not flood the context.
pub const MAX_OUTPUT: usize = 50 * 1024;

/// A tool the model can call. Implement `run` with `async fn`.
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Runs the tool. `Err` is reported to the model as a tool error.
    /// Return a `String` with `.into()`, or a `ToolOutput` to attach a diff.
    fn run(&self, input: Value, ctx: &Context) -> impl Future<Output = Result<ToolOutput>> + Send;
    /// The call as markdown for the user, shown instead of the raw input
    /// (e.g. a list of questions). `None` shows the default.
    fn describe(&self, _input: &Value) -> Option<String> {
        None
    }
    /// Lines for the system prompt's `<rules>` when the tool is present,
    /// e.g. to prefer it over a shell command.
    fn rules(&self) -> &'static [&'static str] {
        &[]
    }
    /// The icon clients show for its calls.
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }
    /// The `description` input says what a call is for and titles its card
    /// (ADR 0044). In other tools, e.g. MCP tools, it can be content.
    fn titled(&self) -> bool {
        false
    }
}

/// What a tool does, for the client's icon (ACP `ToolKind`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    Search,
    Fetch,
    SwitchMode,
    #[default]
    Other,
}

/// Object-safe form of [`Tool`], implemented for every tool.
pub trait DynTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn run<'a>(&'a self, input: Value, ctx: &'a Context) -> BoxFuture<'a, Result<ToolOutput>>;
    fn describe(&self, input: &Value) -> Option<String>;
    fn rules(&self) -> &'static [&'static str];
    fn kind(&self) -> ToolKind;
    fn titled(&self) -> bool;
}

impl<T: Tool> DynTool for T {
    fn spec(&self) -> ToolSpec {
        Tool::spec(self)
    }
    fn run<'a>(&'a self, input: Value, ctx: &'a Context) -> BoxFuture<'a, Result<ToolOutput>> {
        Box::pin(Tool::run(self, input, ctx))
    }
    fn describe(&self, input: &Value) -> Option<String> {
        Tool::describe(self, input)
    }
    fn rules(&self) -> &'static [&'static str] {
        Tool::rules(self)
    }
    fn kind(&self) -> ToolKind {
        Tool::kind(self)
    }
    fn titled(&self) -> bool {
        Tool::titled(self)
    }
}

pub fn builtin() -> Vec<Arc<dyn DynTool>> {
    vec![
        Arc::new(Read),
        Arc::new(ReadImage),
        Arc::new(Write),
        Arc::new(crate::edit::Edit),
        Arc::new(Bash),
        Arc::new(RequestAccess),
    ]
}

pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    cwd.join(path)
}

/// Cuts `s` to `MAX_OUTPUT` bytes (on a char boundary) and marks the cut.
pub fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        s.truncate(s.floor_char_boundary(MAX_OUTPUT));
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
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

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

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    command: String,
    /// working directory, absolute or relative to the session's; default the session's
    cwd: Option<String>,
    /// seconds, default 120
    timeout: Option<f64>,
    /// what the command does, at most 10 words; shown to the user as its title
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
        Ok(truncate(text).into())
    }
}

pub struct ReadImage;

impl Tool for ReadImage {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: READ_IMAGE,
            description:
                "Look at an image file (PNG, JPEG, GIF or WebP, at most 5 MB). Relative paths are resolved against the working directory. Sandboxed like Read."
                    .into(),
            input_schema: input_schema::<ReadInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        use base64::Engine;
        let i: ReadInput = parse(input)?;
        let path = resolve(ctx.cwd(), &i.path);
        // The size first, so a huge file is not read for nothing.
        let bytes = ctx
            .run_sandboxed(move || {
                let len = std::fs::metadata(&path)?.len();
                if len > ImageSource::MAX_BYTES as u64 {
                    return Err(std::io::Error::other(format!(
                        "{:.1} MB is too large, the limit is {} MB; make a smaller copy first",
                        len as f64 / (1024.0 * 1024.0),
                        ImageSource::MAX_BYTES / (1024 * 1024)
                    )));
                }
                std::fs::read(&path)
            })
            .await?
            .with_context(|| format!("reading {}", i.path))?;
        let Some(media_type) = image_type(&bytes) else {
            bail!("{} is not a PNG, JPEG, GIF or WebP image", i.path);
        };
        let source = ImageSource::Base64 {
            media_type: media_type.into(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        };
        Ok(ToolOutput {
            output: format!(
                "{} ({media_type}, {} KB)",
                i.path,
                bytes.len().div_ceil(1024)
            ),
            images: vec![source],
            ..Default::default()
        })
    }
}

/// The media type from the file's magic bytes; extensions can lie.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    Some(match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => return None,
    })
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

pub struct Bash;

impl Tool for Bash {
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn titled(&self) -> bool {
        true
    }

    fn rules(&self) -> &'static [&'static str] {
        &["Use Bash for file operations like ls, mv or mkdir."]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: BASH,
            description: "Run a shell command in the working directory. Returns stdout, stderr and the exit code. \
                Commands run in a sandbox: they can read system directories and read/write the workspace directories, \
                nothing else unless granted. If something fails with 'Permission denied' on a path outside these, \
                ask for it with RequestAccess.".into(),
            input_schema: input_schema::<BashInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: BashInput = parse(input)?;
        let timeout = Duration::try_from_secs_f64(i.timeout.unwrap_or(120.0))
            .ok()
            .filter(|t| !t.is_zero())
            .context("timeout must be a positive number of seconds")?;
        let cwd = i.cwd.map_or(ctx.cwd().into(), |d| resolve(ctx.cwd(), &d));
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&i.command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so a timeout or cancel can kill what it started.
            .process_group(0)
            .kill_on_drop(true);
        ctx.sandbox_command(&mut cmd)
            .context("setting up the sandbox")?;
        let child = cmd.spawn()?;
        // Cancelling the prompt drops this future; take the processes with it.
        let mut group = KillGroup(child.id());
        let out = tokio::time::timeout(timeout, child.wait_with_output()).await;
        let Ok(out) = out else {
            bail!("command timed out after {timeout:?}");
        };
        let out = out?;
        // Finished: processes left in the group closed their output and run
        // on (e.g. `nohup cmd >log &`).
        group.0 = None;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let text = truncate(text);
        if !out.status.success() {
            bail!("{text}\n[{}]", out.status);
        }
        Ok(text.into())
    }
}

/// Kills the process group led by this pid when dropped.
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: plain syscall; the group was created by `process_group(0)`.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::{
        Access, Bash, Context, MAX_OUTPUT, Read, ReadImage, RequestAccess, Tool, Write, truncate,
    };
    use crate::provider::ImageSource;
    use crate::store::SessionId;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx(dir: &Path) -> Context {
        Context::new(
            SessionId::new("t"),
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
                    "cwd": {"type": ["string", "null"], "description": "working directory, absolute or relative to the session's; default the session's"},
                    "timeout": {"type": ["number", "null"], "format": "double", "description": "seconds, default 120"},
                    "description": {"type": "string", "description": "what the command does, at most 10 words; shown to the user as its title"},
                },
                "required": ["command", "description"],
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
        assert_eq!(out.output, "hello");
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
        assert_eq!(out.output.trim(), "marker");

        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner"), "").unwrap();
        let out = Bash
            .run(json!({"command": "ls", "cwd": "sub"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(out.output.trim(), "inner");

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

    #[tokio::test]
    async fn bash_rejects_bad_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        for timeout in [json!(-1), json!(0), json!(1e300)] {
            let input = json!({"command": "true", "timeout": timeout});
            let err = Bash.run(input, &ctx(dir.path())).await.unwrap_err();
            assert!(err.to_string().contains("positive"), "{err}");
        }
    }

    /// Whether `pid` is gone (or a zombie, waiting to be reaped).
    #[cfg(target_os = "linux")]
    fn dead(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, s)| s.starts_with('Z')),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bash_timeout_kills_what_the_command_started() {
        let dir = tempfile::tempdir().unwrap();
        let input = json!({"command": "sleep 30 & echo $! > pid; wait", "timeout": 0.5});
        let err = Bash.run(input, &ctx(dir.path())).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        let pid = pid.trim();
        for _ in 0..50 {
            if dead(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sleep {pid} survived the timeout");
    }

    #[tokio::test]
    async fn tools_are_sandboxed_to_cwd_and_grants() {
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let secret = other.path().join("secret");
        std::fs::write(&secret, "hidden").unwrap();
        let ctx = ctx(cwd.path());
        let cat = json!({"command": format!("cat {}", secret.display())});
        let touch = json!({"command": format!("touch {}/new", other.path().display())});

        let err = Bash.run(cat.clone(), &ctx).await.unwrap_err().to_string();
        assert!(err.contains("Permission denied"), "{err}");
        // NoFrontend denies the request.
        let ask = json!({"path": other.path(), "access": "read", "reason": "test"});
        assert!(RequestAccess.run(ask, &ctx).await.is_err());

        let read = json!({"path": secret});
        let write = json!({"path": other.path().join("new"), "content": "x"});
        let err = format!("{:#}", Read.run(read.clone(), &ctx).await.unwrap_err());
        assert!(err.contains("Permission denied"), "{err}");
        assert!(Write.run(write.clone(), &ctx).await.is_err());
        // Only the throwaway thread was restricted.
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "hidden");

        ctx.grant(other.path(), Access::Read);
        assert_eq!(Bash.run(cat, &ctx).await.unwrap().output, "hidden");
        assert_eq!(Read.run(read, &ctx).await.unwrap().output, "hidden");
        assert!(Write.run(write.clone(), &ctx).await.is_err());
        let err = Bash.run(touch.clone(), &ctx).await.unwrap_err().to_string();
        assert!(err.contains("Permission denied"), "{err}");

        ctx.grant(other.path(), Access::ReadWrite);
        Bash.run(touch, &ctx).await.unwrap();
        Write.run(write, &ctx).await.unwrap();
    }

    /// A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    #[tokio::test]
    async fn read_image_returns_the_image() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        // The type comes from the content, not the extension.
        std::fs::write(dir.path().join("pic.jpg"), &png).unwrap();
        std::fs::write(dir.path().join("notes.png"), "text").unwrap();
        let ctx = ctx(dir.path());

        let out = ReadImage
            .run(json!({"path": "pic.jpg"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.output, "pic.jpg (image/png, 1 KB)");
        assert_eq!(
            out.images,
            [ImageSource::Base64 {
                media_type: "image/png".into(),
                data: PNG.into()
            }]
        );

        let err = |path: &str| {
            let ctx = ctx.clone();
            let path = path.to_string();
            async move {
                let e = ReadImage.run(json!({"path": path}), &ctx).await;
                format!("{:#}", e.unwrap_err())
            }
        };
        assert!(err("notes.png").await.contains("not a PNG"));
        assert!(err("missing.png").await.contains("reading missing.png"));
        let big = std::fs::File::create(dir.path().join("big.png")).unwrap();
        big.set_len(ImageSource::MAX_BYTES as u64 + 1).unwrap();
        assert!(err("big.png").await.contains("too large"));
    }

    #[tokio::test]
    async fn read_image_is_sandboxed() {
        let (cwd, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let pic = other.path().join("pic.gif");
        std::fs::write(&pic, b"GIF89a").unwrap();
        let ctx = ctx(cwd.path());
        let input = json!({"path": pic});
        let err = format!(
            "{:#}",
            ReadImage.run(input.clone(), &ctx).await.unwrap_err()
        );
        assert!(err.contains("Permission denied"), "{err}");
        ctx.grant(other.path(), Access::Read);
        let out = ReadImage.run(input, &ctx).await.unwrap();
        assert_eq!(out.images.len(), 1);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(MAX_OUTPUT);
        let t = truncate(s);
        assert!(t.ends_with("[output truncated]"));
    }
}
