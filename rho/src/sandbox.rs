//! Per-session filesystem sandbox for `Bash`, enforced with Landlock (via
//! nono) on each spawned command.
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
    /// Run Bash unsandboxed.
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
        let prepared = nono::sandbox::prepare_landlock_with_abi(&caps, &abi)?;
        // SAFETY: `apply_raw` only issues raw syscalls and does not allocate,
        // so it is safe between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                prepared
                    .apply_raw()
                    .map_err(|e| std::io::Error::from_raw_os_error(e.errno()))
            });
        }
        Ok(())
    }

    // ponytail: Linux (Landlock) only; macOS would need nono's Seatbelt backend.
    #[cfg(not(target_os = "linux"))]
    pub fn apply(&self, _cmd: &mut tokio::process::Command) -> Result<()> {
        anyhow::bail!(
            "the Bash sandbox is only supported on Linux; set \"disabled\": true in sandbox.json"
        )
    }
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
