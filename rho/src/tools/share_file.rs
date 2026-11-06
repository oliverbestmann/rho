//! `ShareFile`: a link to a file for the user. ACP clients get a
//! `resource_link` with the file's `file://` uri; the web client gets a
//! `/share/...` link that the daemon serves under the session's sandbox
//! (`web_link`, `acp_ws`).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::extension::{Context, ToolOutput};
use crate::provider::{ToolName, ToolSpec};

use super::{Tool, ToolKind, input_schema, parse};

pub const SHARE_FILE: ToolName = ToolName::from_static("ShareFile");

/// Starts the web links, see `web_link`.
pub const SHARE_PREFIX: &str = "/share/";

#[derive(Deserialize, JsonSchema)]
struct ShareFileInput {
    path: String,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

pub struct ShareFile;

impl Tool for ShareFile {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SHARE_FILE,
            description:
                "Share a file with the user as a link they can open, for example a report, a build artifact or a PDF. Relative paths are resolved against the working directory. Sandboxed like Read."
                    .into(),
            input_schema: input_schema::<ShareFileInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: ShareFileInput = parse(input)?;
        let path = shared_path(ctx.cwd(), &i.path);
        let file = open(ctx, path.clone()).await?;
        let len = file.metadata()?.len();
        Ok(format!(
            "shared with the user: {} ({} KB)",
            path.display(),
            len.div_ceil(1024)
        )
        .into())
    }
}

/// `path` against `cwd`, with `.` and `..` resolved by name: a browser
/// would resolve them in the link, which then no longer matches its
/// signature.
pub(crate) fn shared_path(cwd: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in cwd.join(path).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Opens the regular file at `path` under the session's sandbox. The open
/// file can be read outside of it.
pub async fn open(ctx: &Context, path: PathBuf) -> Result<std::fs::File> {
    let shown = path.display().to_string();
    let file = ctx
        .run_sandboxed(move || std::fs::File::open(&path))
        .await?
        .with_context(|| format!("opening {shown}"))?;
    if !file.metadata()?.is_file() {
        bail!("{shown} is not a file");
    }
    Ok(file)
}

/// The `file://` uri of `path`.
pub(crate) fn file_uri(path: &Path) -> String {
    format!("file://{}", encode(path))
}

/// `path` with everything but unreserved characters and `/` percent-encoded.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The browser link for a `file_uri` in session `sid`: `/share/<sid>/<mac>/<path>`.
/// The mac stands in for the token, which a link cannot carry.
pub(crate) fn web_link(key: &[u8; 32], sid: &str, file_uri: &str) -> Option<String> {
    let encoded = file_uri.strip_prefix("file://")?;
    let mac = sign(key, sid, encoded);
    Some(format!("{SHARE_PREFIX}{sid}/{mac}{encoded}"))
}

/// `mac` is `web_link`'s for `sid` and the (decoded) absolute `path`.
pub fn verify(key: &[u8; 32], sid: &str, mac: &str, path: &Path) -> bool {
    let want = sign(key, sid, &encode(path));
    // Comparing digests, so the time taken says nothing about the mac.
    Sha256::digest(want) == Sha256::digest(mac)
}

/// The key for share links, from the token that guards the listener.
pub fn share_key(token: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"rho share links\0")
        .chain_update(token)
        .finalize()
        .into()
}

/// HMAC-SHA256 of `sid`, NUL, `encoded`, cut to 128 bits, base64url.
fn sign(key: &[u8; 32], sid: &str, encoded: &str) -> String {
    use base64::Engine;
    let (mut inner, mut outer) = ([0x36u8; 64], [0x5cu8; 64]);
    for (i, k) in key.iter().enumerate() {
        inner[i] ^= k;
        outer[i] ^= k;
    }
    let inner = Sha256::new()
        .chain_update(inner)
        .chain_update(sid)
        .chain_update([0])
        .chain_update(encoded)
        .finalize();
    let mac = Sha256::new()
        .chain_update(outer)
        .chain_update(inner)
        .finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&mac[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_ctx;
    use serde_json::json;

    #[test]
    fn links_are_signed_per_session_and_path() {
        let key = share_key("token");
        let path = shared_path(Path::new("/w/sub"), "../a b/./ü.txt");
        assert_eq!(path, Path::new("/w/a b/ü.txt"));
        let uri = file_uri(&path);
        assert_eq!(uri, "file:///w/a%20b/%C3%BC.txt");

        let link = web_link(&key, "s1", &uri).unwrap();
        let rest = link.strip_prefix("/share/s1/").unwrap();
        let (mac, encoded) = rest.split_once('/').unwrap();
        assert_eq!(encoded, "w/a%20b/%C3%BC.txt");
        assert!(verify(&key, "s1", mac, &path));
        assert!(!verify(&key, "s2", mac, &path));
        assert!(!verify(&key, "s1", mac, Path::new("/w/other")));
        assert!(!verify(&share_key("other"), "s1", mac, &path));
    }

    #[tokio::test]
    async fn shares_only_files_the_sandbox_allows() {
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("r.txt"), "hi").unwrap();
        std::fs::write(other.path().join("s.txt"), "no").unwrap();
        let ctx = test_ctx(cwd.path());

        let out = ShareFile.run(json!({"path": "r.txt"}), &ctx).await.unwrap();
        assert!(out.output.ends_with("r.txt (1 KB)"), "{}", out.output);
        let outside = json!({"path": other.path().join("s.txt")});
        assert!(ShareFile.run(outside, &ctx).await.is_err());
        assert!(ShareFile.run(json!({"path": "."}), &ctx).await.is_err());
    }
}
