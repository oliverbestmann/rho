//! Picking a directory for a new session from the web client, and the limit
//! for web sessions: under `$HOME` or `/tmp` (ADR 0057).

use std::path::{Path, PathBuf};

use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

/// The subdirectories of the path's last level that start with its last
/// part: `~/src/rh` lists `~/src` for `rh*`, `~/src/` lists all.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_rho/dirs", response = DirsResponse)]
pub(super) struct DirsRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub(super) struct DirsResponse {
    /// Names, not paths. Only allowed directories and their parents.
    pub entries: Vec<String>,
    /// The canonical path, if the path is an allowed directory.
    pub dir: Option<PathBuf>,
}

/// Creates the path's last level; its parent must be allowed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_rho/mkdir", response = MkdirResponse)]
pub(super) struct MkdirRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub(super) struct MkdirResponse {
    /// Canonical.
    pub dir: PathBuf,
}

const MAX_ENTRIES: usize = 50;

/// Canonical `$HOME` and `/tmp`, the ones that exist.
pub(super) fn roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [home, Some(PathBuf::from("/tmp"))]
        .into_iter()
        .flatten()
        .filter_map(|p| p.canonicalize().ok())
        .collect()
}

/// The canonical `path`, if it is under one of `roots`. Symlinks and `..`
/// are resolved first, so they can't lead out.
pub(super) fn allowed(path: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("{}", path.display()))?;
    if !roots.iter().any(|r| canonical.starts_with(r)) {
        bail!("{} is not under ~ or /tmp", path.display());
    }
    Ok(canonical)
}

pub(super) fn dirs(path: &str, roots: &[PathBuf]) -> Result<DirsResponse> {
    let full = expand(path)?;
    let dir = allowed(&full, roots).ok().filter(|d| d.is_dir());
    // As text: `Path` would drop a trailing `.`, the start of a hidden name.
    let text = full.to_str().context("path is not UTF-8")?;
    let (parent, prefix) = text.rsplit_once('/').context("no `/`")?;
    let parent = Path::new(if parent.is_empty() { "/" } else { parent });
    // Typing goes on: an unreadable level just has nothing to offer.
    let Ok(read) = std::fs::read_dir(parent) else {
        return Ok(DirsResponse {
            entries: vec![],
            dir,
        });
    };
    let mut entries: Vec<String> = read
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix) && (!n.starts_with('.') || prefix.starts_with('.')))
        .filter(|n| {
            // Parents of the roots too, so `/` leads to `/home/me`.
            parent.join(n).canonicalize().is_ok_and(|c| {
                c.is_dir() && roots.iter().any(|r| c.starts_with(r) || r.starts_with(&c))
            })
        })
        .collect();
    entries.sort();
    entries.truncate(MAX_ENTRIES);
    Ok(DirsResponse { entries, dir })
}

pub(super) fn mkdir(path: &str, roots: &[PathBuf]) -> Result<PathBuf> {
    let full = expand(path.trim_end_matches('/'))?;
    let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else {
        bail!("{path} has no name to create");
    };
    let dir = allowed(parent, roots)?.join(name);
    std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `~` is `$HOME`; anything else must be absolute.
fn expand(path: &str) -> Result<PathBuf> {
    let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let full = match path {
        "~" => home(),
        p => match p.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(p),
        },
    };
    if !full.is_absolute() {
        bail!("{path} is not an absolute path");
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Vec<PathBuf>, String) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("home");
        for d in ["home/src", "home/scratch", "home/.config", "outside"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(root.join("some-file"), "").unwrap();
        std::os::unix::fs::symlink(base.join("outside"), root.join("sneaky")).unwrap();
        let root_str = root.to_str().unwrap().to_string();
        (tmp, vec![root], root_str)
    }

    #[test]
    fn lists_the_current_level() {
        let (_tmp, roots, root) = setup();
        let all = dirs(&format!("{root}/"), &roots).unwrap();
        // No file, no hidden dir, no symlink out of the root.
        assert_eq!(all.entries, ["scratch", "src"]);
        assert_eq!(all.dir.as_deref(), Some(Path::new(&root)));

        let s = dirs(&format!("{root}/sr"), &roots).unwrap();
        assert_eq!(s.entries, ["src"]);
        assert_eq!(s.dir, None);

        let hidden = dirs(&format!("{root}/."), &roots).unwrap();
        assert_eq!(hidden.entries, [".config"]);
    }

    #[test]
    fn leads_from_above_to_the_roots_only() {
        let (_tmp, roots, root) = setup();
        let parent = Path::new(&root).parent().unwrap().to_str().unwrap();
        let up = dirs(&format!("{parent}/"), &roots).unwrap();
        assert_eq!(up.entries, ["home"]);
        assert_eq!(up.dir, None);
        assert!(dirs("relative/", &roots).is_err());
    }

    #[test]
    fn allows_only_paths_under_the_roots() {
        let (_tmp, roots, root) = setup();
        assert!(allowed(Path::new(&format!("{root}/src")), &roots).is_ok());
        assert!(allowed(Path::new(&format!("{root}/src/../../outside")), &roots).is_err());
        assert!(allowed(Path::new(&format!("{root}/sneaky")), &roots).is_err());
        assert!(allowed(Path::new(&format!("{root}/missing")), &roots).is_err());
    }

    #[test]
    fn creates_one_level_under_the_roots() {
        let (_tmp, roots, root) = setup();
        let dir = mkdir(&format!("{root}/new/"), &roots).unwrap();
        assert!(dir.is_dir());
        assert!(mkdir(&format!("{root}/new"), &roots).is_err(), "exists");
        assert!(mkdir(&format!("{root}/a/b"), &roots).is_err(), "two levels");
        assert!(mkdir(&format!("{root}/sneaky/x"), &roots).is_err());
        assert!(mkdir(&format!("{root}/.."), &roots).is_err());
    }
}
