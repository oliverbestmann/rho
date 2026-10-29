//! The `Tool` plug-in trait and the built-in tools. `ToolExtension` turns a
//! tool into the extension that adds it to every session (`builtin()`).
//!
//! Built-in names use Claude Code casing (`Read`, `Write`, `Bash`), which is
//! what subscription requests are expected to carry.

mod bash;
mod edit;
mod read;
mod read_image;
mod request_access;
mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::extension::{BoxFuture, Context, DynExtension, Extension, SessionSetup, ToolOutput};
use crate::provider::ToolSpec;

pub use bash::{BASH, Bash};
pub use edit::{EDIT, Edit};
pub use read::{READ, Read};
pub use read_image::{READ_IMAGE, ReadImage};
pub use request_access::{REQUEST_ACCESS, RequestAccess};
pub use write::{WRITE, Write};

/// Output larger than this is cut off so it does not flood the context.
pub const MAX_OUTPUT: usize = 50 * 1024;
/// The most a call can ask for with `maxOutput` (Read, Bash).
pub const MAX_OUTPUT_LIMIT: usize = 64 * 1024;

/// A call's `maxOutput`, kept within 1..=`MAX_OUTPUT_LIMIT`.
fn max_output(asked: Option<usize>) -> usize {
    asked.unwrap_or(MAX_OUTPUT).clamp(1, MAX_OUTPUT_LIMIT)
}

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
    /// A call that rho's stop (a restart) cut off may run again when the
    /// session continues: the tool only reads, or only asks the user.
    fn repeatable(&self) -> bool {
        matches!(self.kind(), ToolKind::Read | ToolKind::Search)
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
    fn repeatable(&self) -> bool;
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
    fn repeatable(&self) -> bool {
        Tool::repeatable(self)
    }
}

/// An extension that adds one tool to every session:
/// `Arc::new(ToolExtension::from(MyTool))`.
pub struct ToolExtension(Arc<dyn DynTool>);

// The orphan rule forbids `From<T: Tool> for Arc<dyn DynExtension>`, so the
// conversion goes through this local type.
impl<T: Tool + 'static> From<T> for ToolExtension {
    fn from(tool: T) -> Self {
        Self(Arc::new(tool))
    }
}

impl Extension for ToolExtension {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(self.0.clone());
    }
}

/// The built-in tools' extensions, in the order their tools are listed.
/// Register them first, so extensions that wrap tools see them.
pub fn builtin() -> Vec<Arc<dyn DynExtension>> {
    vec![
        Arc::new(ToolExtension::from(Read)),
        Arc::new(ToolExtension::from(ReadImage)),
        Arc::new(ToolExtension::from(Write)),
        Arc::new(ToolExtension::from(Edit)),
        Arc::new(ToolExtension::from(Bash)),
        Arc::new(ToolExtension::from(RequestAccess)),
        Arc::new(ToolExtension::from(crate::compact::Compact)),
    ]
}

pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    cwd.join(path)
}

/// Cuts `s` to `MAX_OUTPUT` bytes (on a char boundary) and marks the cut.
pub fn truncate(s: String) -> String {
    truncate_to(s, MAX_OUTPUT)
}

fn truncate_to(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(s.floor_char_boundary(max));
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
#[cfg(test)]
pub(crate) fn test_ctx(dir: &Path) -> Context {
    Context::new(
        crate::store::SessionId::new("t"),
        dir,
        Arc::new(crate::extension::NoFrontend),
        Default::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::{Bash, Read, RequestAccess, Write, test_ctx as ctx};
    use super::{MAX_OUTPUT, MAX_OUTPUT_LIMIT, Tool, truncate};
    use crate::sandbox::Access;
    use serde_json::json;

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

    #[tokio::test]
    async fn max_output_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let big = "x".repeat(100 * 1024);
        std::fs::write(dir.path().join("big"), &big).unwrap();
        let print = format!("head -c {} /dev/zero | tr '\\0' x", big.len());
        for (max, kept) in [
            (json!(null), MAX_OUTPUT),
            (json!(10), 10),
            (json!(1 << 20), MAX_OUTPUT_LIMIT),
        ] {
            let out = Read
                .run(json!({"path": "big", "maxOutput": max}), &ctx)
                .await
                .unwrap();
            assert_eq!(out.output.matches('x').count(), kept, "Read {max}");
            let out = Bash
                .run(json!({"command": print, "maxOutput": max}), &ctx)
                .await
                .unwrap();
            assert_eq!(out.output.matches('x').count(), kept, "Bash {max}");
        }
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(MAX_OUTPUT);
        let t = truncate(s);
        assert!(t.ends_with("[output truncated]"));
    }
}
