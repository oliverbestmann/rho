//! Per-session filesystem sandbox for `Bash`, `Read` and `Write` (and any
//! extension using `Context::run_sandboxed` / `Context::sandbox_command`),
//! enforced with Landlock via nono.
//!
//! Deny by default: a session starts with read access to system directories
//! (enough to run programs) and read-write access to its workspace
//! directories. Everything else comes from the user's `sandbox.json` in the
//! config dir, from extensions (`ctx.grant`), or from the model through the
//! `RequestAccess` tool after the user approves.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::extension::{Context, Extension, SessionSetup, SlashCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    ReadWrite,
}

/// One allowed directory (with everything below it) or file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub path: PathBuf,
    pub access: Access,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub rules: Vec<Rule>,
}

/// Read-only system paths a shell needs to start programs. Missing ones are skipped.
const SYSTEM_READ: &[&str] = &[
    "/nix/store",
    "/run/current-system",
    "/etc",
    "/bin",
    "/sbin",
    "/usr",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/sys",
];

/// Device files programs expect to write to.
const DEVICES_RW: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

/// `sandbox.json` in the config dir. Paths are absolute or start with `~/`.
///
/// ```json
/// { "read": ["~/.config/git"], "read_write": ["~/go", "~/.cache", "/tmp"] }
/// ```
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Turn the sandbox off.
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub read: Vec<PathBuf>,
    #[serde(default)]
    pub read_write: Vec<PathBuf>,
}

impl Config {
    pub const FILE: &str = "sandbox.json";

    /// A missing file is the default (sandboxed, no extra paths); an invalid one is an error.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join(Self::FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut config: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for p in config.read.iter_mut().chain(&mut config.read_write) {
            if let (Ok(rest), Some(home)) = (p.strip_prefix("~"), &home) {
                *p = home.join(rest);
            }
            if !p.is_absolute() {
                bail!("{}: {} is not absolute", path.display(), p.display());
            }
        }
        Ok(config)
    }

    /// The starting policy for a session with these workspace directories.
    pub fn policy(&self, roots: &[PathBuf]) -> Option<Policy> {
        if self.disabled {
            return None;
        }
        let mut policy = Policy::base(roots);
        for p in &self.read {
            policy.allow(p, Access::Read);
        }
        for p in &self.read_write {
            policy.allow(p, Access::ReadWrite);
        }
        Some(policy)
    }
}

impl Policy {
    /// System directories read-only, the workspace `roots` read-write, nothing else.
    pub fn base(roots: &[PathBuf]) -> Self {
        let mut policy = Self::default();
        for p in SYSTEM_READ {
            policy.allow(p, Access::Read);
        }
        for p in DEVICES_RW {
            policy.allow(p, Access::ReadWrite);
        }
        for root in roots {
            policy.allow(root, Access::ReadWrite);
        }
        policy
    }

    pub fn allow(&mut self, path: impl Into<PathBuf>, access: Access) {
        self.rules.push(Rule {
            path: path.into(),
            access,
        });
    }

    /// Restricts `cmd` to this policy once it is spawned. The rules are
    /// resolved now; the child only runs allocation-free syscalls before exec.
    #[cfg(target_os = "linux")]
    pub fn apply(&self, cmd: &mut tokio::process::Command) -> Result<()> {
        let prepared = self.prepare()?;
        // SAFETY: `apply_raw` only issues raw syscalls and does not allocate,
        // so it is safe between fork and exec.
        unsafe {
            cmd.pre_exec(move || prepared.apply_raw().map_err(raw_error));
        }
        Ok(())
    }

    /// Runs `f` on a new thread restricted to this policy, for in-process
    /// file access. Landlock restricts only the calling thread, so the rest of
    /// rho is unaffected; the thread ends with `f`. Never use a pooled thread
    /// (e.g. `spawn_blocking`) for this, it would stay restricted.
    #[cfg(target_os = "linux")]
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        let prepared = self.prepare()?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(prepared.apply_raw().map_err(raw_error).map(|()| f()));
        });
        Ok(rx.await.context("sandboxed thread panicked")??)
    }

    #[cfg(target_os = "linux")]
    fn prepare(&self) -> Result<nono::sandbox::PreparedLandlockSandbox> {
        use nono::{AccessMode, CapabilitySet, FsCapability};

        let mut caps = CapabilitySet::new();
        for rule in &self.rules {
            // Paths that do not exist (any more) cannot be opened; there is nothing to allow.
            if !rule.path.exists() {
                continue;
            }
            let mode = match rule.access {
                Access::Read => AccessMode::Read,
                Access::ReadWrite => AccessMode::ReadWrite,
            };
            caps.add_fs(if rule.path.is_dir() {
                FsCapability::new_dir(&rule.path, mode)?
            } else {
                FsCapability::new_file(&rule.path, mode)?
            });
        }
        let abi = nono::Sandbox::detect_abi()?;
        Ok(nono::sandbox::prepare_landlock_with_abi(&caps, &abi)?)
    }

    // ponytail: Linux (Landlock) only; macOS would need nono's Seatbelt backend.
    #[cfg(not(target_os = "linux"))]
    pub fn apply(&self, _cmd: &mut tokio::process::Command) -> Result<()> {
        bail!(UNSUPPORTED)
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn run<T: Send + 'static>(
        &self,
        _f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        bail!(UNSUPPORTED)
    }
}

#[cfg(target_os = "linux")]
fn raw_error(e: nono::sandbox::RawSandboxError) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.errno())
}

#[cfg(not(target_os = "linux"))]
const UNSUPPORTED: &str =
    "the sandbox is only supported on Linux; set \"disabled\": true in sandbox.json";

/// `/grant-read <path>` and `/grant-write <path>`: the user widens the
/// session's sandbox. The model is told with the next prompt.
pub struct GrantCommands;

impl Extension for GrantCommands {
    async fn session_start(&self, session: &mut SessionSetup) {
        for (name, what) in [("grant-read", "read"), ("grant-write", "read-write")] {
            session.commands.push(SlashCommand {
                name: name.into(),
                description: format!("Allow {what} access to a file or directory"),
                hint: Some("path".into()),
            });
        }
    }

    async fn command(&self, ctx: &Context, name: &str, args: &str) -> Option<String> {
        let (access, what) = match name {
            "grant-read" => (Access::Read, "read"),
            "grant-write" => (Access::ReadWrite, "read-write"),
            _ => return None,
        };
        Some(grant(ctx, args, access, what))
    }
}

fn grant(ctx: &Context, path: &str, access: Access, what: &str) -> String {
    if ctx.sandbox().is_none() {
        return "This session is not sandboxed.".into();
    }
    if path.is_empty() {
        return format!(
            "Usage: /grant-{} <path>",
            if access == Access::Read {
                "read"
            } else {
                "write"
            }
        );
    }
    let path = match ctx.cwd().join(path).canonicalize() {
        Ok(p) => p,
        Err(e) => return format!("Cannot grant {path}: {e}"),
    };
    ctx.grant(&path, access);
    ctx.send(
        format!("The user granted {what} access to {}.", path.display()),
        false,
    );
    format!("Granted {what} access to {}.", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_expands_home_and_rejects_relative() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(dir.path()).unwrap().policy(&[]).is_some());

        let file = dir.path().join(Config::FILE);
        std::fs::write(&file, r#"{"read": ["~/x"], "read_write": ["/tmp"]}"#).unwrap();
        let config = Config::load(dir.path()).unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(config.read, [home.join("x")]);
        let roots = [PathBuf::from("/w1"), PathBuf::from("/w2")];
        let rules = config.policy(&roots).unwrap().rules;
        for (path, access) in [
            ("/w1", Access::ReadWrite),
            ("/w2", Access::ReadWrite),
            ("/tmp", Access::ReadWrite),
        ] {
            assert!(
                rules.contains(&Rule {
                    path: path.into(),
                    access
                }),
                "{path}"
            );
        }

        std::fs::write(&file, r#"{"read": ["rel"]}"#).unwrap();
        assert!(Config::load(dir.path()).is_err());
        std::fs::write(&file, r#"{"disabled": true}"#).unwrap();
        assert!(Config::load(dir.path()).unwrap().policy(&roots).is_none());
    }
}
