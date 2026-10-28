//! Per-session filesystem sandbox for the built-in tools (and any extension
//! using `Context::run_sandboxed` / `Context::sandbox_command`), enforced
//! with Landlock via nono.
//!
//! Deny by default: a session starts with read access to system directories
//! (enough to run programs) and read-write access to its workspace
//! directories. Everything else comes from `"sandbox"` in the user's
//! config (`config.d`), from extensions (`ctx.grant`), or from the model through the
//! `RequestAccess` tool after the user approves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::extension::{CommandName, Context, Extension, SessionSetup, SlashCommand};

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

/// `"sandbox"` in the config (`config.d`). Paths are absolute or start with `~/`.
///
/// ```json
/// { "disabled": false, "paths": { "~/.config/git": "read", "~/go": "readwrite" } }
/// ```
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    /// Turn the sandbox off.
    pub disabled: bool,
    pub read: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    paths: Paths,
}

impl TryFrom<RawConfig> for Config {
    type Error = anyhow::Error;

    fn try_from(raw: RawConfig) -> Result<Self> {
        let (read, read_write) = split(raw.paths)?;
        Ok(Self {
            disabled: raw.disabled,
            read,
            read_write,
        })
    }
}

/// `{ "path": "read" | "readwrite" }`, a map so config files can add and
/// remove (`null`) paths.
pub(crate) type Paths = BTreeMap<PathBuf, Access>;

/// `paths` expanded, split into read and read-write.
pub(crate) fn split(paths: Paths) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (mut read, mut read_write) = (vec![], vec![]);
    for (mut path, access) in paths {
        expand(std::iter::once(&mut path))?;
        match access {
            Access::Read => read.push(path),
            Access::ReadWrite => read_write.push(path),
        }
    }
    Ok((read, read_write))
}

impl Config {
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

/// Expands `~/` in `paths` and fails if one is not absolute.
pub(crate) fn expand<'a>(paths: impl Iterator<Item = &'a mut PathBuf>) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for p in paths {
        if let (Ok(rest), Some(home)) = (p.strip_prefix("~"), &home) {
            *p = home.join(rest);
        }
        if !p.is_absolute() {
            bail!("{} is not absolute", p.display());
        }
    }
    Ok(())
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

    /// The same paths, all read-only except the device files programs
    /// expect to write to (e.g. `/dev/null`).
    pub fn read_only(&self) -> Self {
        let rules = self.rules.iter().map(|r| Rule {
            path: r.path.clone(),
            access: if DEVICES_RW.iter().any(|d| r.path == Path::new(d)) {
                r.access
            } else {
                Access::Read
            },
        });
        Self {
            rules: rules.collect(),
        }
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

    // shortcut: Linux (Landlock) only; macOS would need nono's Seatbelt backend.
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
    "the sandbox is only supported on Linux; set \"sandbox\": {\"disabled\": true} in config.d";

/// `/grant-read <path>` and `/grant-write <path>`: the user widens the
/// session's sandbox. The model is told with the next prompt. `/sandbox`
/// lists what sandboxed code may access now.
pub struct GrantCommands;

const SANDBOX: CommandName = CommandName::from_static("sandbox");

const GRANT_READ: CommandName = CommandName::from_static("grant-read");
const GRANT_WRITE: CommandName = CommandName::from_static("grant-write");

impl Extension for GrantCommands {
    async fn session_start(&self, session: &mut SessionSetup) {
        for (name, what) in [(GRANT_READ, "read"), (GRANT_WRITE, "read-write")] {
            session.commands.push(SlashCommand {
                name,
                description: format!("Allow {what} access to a file or directory"),
                hint: Some("path".into()),
            });
        }
        session.commands.push(SlashCommand {
            name: SANDBOX,
            description: "Show the paths the sandbox allows".into(),
            hint: None,
        });
    }

    async fn command(&self, ctx: &Context, name: &CommandName, args: &str) -> Option<String> {
        if *name == SANDBOX {
            return Some(match ctx.effective_sandbox() {
                Some(policy) => show(&policy),
                None => "This session is not sandboxed.".into(),
            });
        }
        let (access, what) = if *name == GRANT_READ {
            (Access::Read, "read")
        } else if *name == GRANT_WRITE {
            (Access::ReadWrite, "read-write")
        } else {
            return None;
        };
        Some(grant(ctx, args, access, what))
    }
}

/// Markdown: a list of the read and of the read-write paths.
fn show(policy: &Policy) -> String {
    let list = |title: &str, access: Access| {
        let rules = policy.rules.iter().filter(|r| r.access == access);
        let items: Vec<_> = rules.map(|r| format!("- `{}`", r.path.display())).collect();
        format!("{title}:\n{}", items.join("\n"))
    };
    format!(
        "{}\n\n{}",
        list("Read", Access::Read),
        list("Read-write", Access::ReadWrite)
    )
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
        let load = serde_json::from_str::<Config>;
        assert!(Config::default().policy(&[]).is_some());

        let config = load(r#"{"paths": {"~/x": "read", "/tmp": "readwrite"}}"#).unwrap();
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

        let mut policy = Policy::default();
        policy.allow("/r", Access::Read);
        policy.allow("/w", Access::ReadWrite);
        policy.allow("/x", Access::Read);
        assert_eq!(
            show(&policy),
            "Read:\n- `/r`\n- `/x`\n\nRead-write:\n- `/w`"
        );

        assert!(load(r#"{"paths": {"rel": "read"}}"#).is_err());
        assert!(load(r#"{"read": ["/x"]}"#).is_err());
        assert!(
            load(r#"{"disabled": true}"#)
                .unwrap()
                .policy(&roots)
                .is_none()
        );
    }
}
