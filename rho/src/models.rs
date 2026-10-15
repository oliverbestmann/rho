//! `models.json`: which models a session may use, which one it starts with
//! and which one compacts, by the session's cwd.
//!
//! ```json
//! { "rules": [
//!     { "path": "~/github", "allow": ["github-copilot/*"],
//!       "default": "github-copilot/claude-sonnet-5.5",
//!       "compact": "github-copilot/claude-haiku-5.5" },
//!     { "path": "~/github/sandbox-repo", "allow": ["*"] }
//! ] }
//! ```
//!
//! Each key is inherited on its own: the longest rule path containing the
//! cwd that sets it wins.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::provider::{ModelSpec, ProviderName};

/// Config id of the model option for paths without a rule.
pub const MODEL_KEY: &str = "model";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    path: PathBuf,
    /// `path` as written, for the model option's config id.
    #[serde(skip)]
    written: String,
    allow: Option<Arc<[Pattern]>>,
    default: Option<ModelSpec>,
    compact: Option<ModelSpec>,
}

/// `*`, `provider/*` or `provider/model`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub enum Pattern {
    Any,
    Provider(ProviderName),
    Model(ModelSpec),
}

impl TryFrom<String> for Pattern {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        Ok(match s.strip_suffix("/*") {
            _ if s == "*" => Self::Any,
            Some(p) if !p.is_empty() && !p.contains('/') => Self::Provider(ProviderName::new(p)),
            _ => Self::Model(s.parse()?),
        })
    }
}

impl Pattern {
    pub fn matches(&self, spec: &ModelSpec) -> bool {
        match self {
            Self::Any => true,
            Self::Provider(p) => spec.provider() == p,
            Self::Model(m) => m == spec,
        }
    }
}

/// What applies to one cwd.
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// `None`: every model.
    pub allow: Option<Arc<[Pattern]>>,
    pub default: Option<ModelSpec>,
    pub compact: Option<ModelSpec>,
    /// Config id of the model option, so clients remember a pick per rule.
    pub key: String,
}

impl Policy {
    pub fn allows(&self, spec: &ModelSpec) -> bool {
        allows(self.allow.as_deref(), spec)
    }
}

pub(crate) fn allows(allow: Option<&[Pattern]>, spec: &ModelSpec) -> bool {
    allow.is_none_or(|a| a.iter().any(|p| p.matches(spec)))
}

impl Config {
    pub const FILE: &str = "models.json";

    /// A missing file means no rules; an invalid one is an error.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join(Self::FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text, &path)
    }

    /// `text` as the content of `file`.
    pub fn parse(text: &str, file: &Path) -> Result<Self> {
        let mut config: Self =
            serde_json::from_str(text).with_context(|| format!("parsing {}", file.display()))?;
        for r in &mut config.rules {
            r.written = r.path.display().to_string();
        }
        crate::sandbox::expand(config.rules.iter_mut().map(|r| &mut r.path), file)?;
        Ok(config)
    }

    pub fn policy(&self, cwd: &Path) -> Policy {
        // The longest path; on equal paths the later rule.
        let first = |f: fn(&Rule) -> bool| {
            self.rules
                .iter()
                .enumerate()
                .filter(|(_, r)| f(r) && cwd.starts_with(&r.path))
                .max_by_key(|(i, r)| (r.path.components().count(), *i))
                .map(|(_, r)| r)
        };
        Policy {
            allow: first(|r| r.allow.is_some()).and_then(|r| r.allow.clone()),
            default: first(|r| r.default.is_some()).and_then(|r| r.default.clone()),
            compact: first(|r| r.compact.is_some()).and_then(|r| r.compact.clone()),
            key: first(|r| r.allow.is_some() || r.default.is_some())
                .map_or(MODEL_KEY.into(), |r| format!("{MODEL_KEY}:{}", r.written)),
        }
    }

    /// Fails if a rule path would start sessions on a model it does not
    /// allow. `model` and `compact` are the fallbacks; `None` if given
    /// explicitly (`-m`, `RHO_COMPACT_MODEL`), those are checked per session.
    /// The policy only changes at rule paths, so checking those covers all.
    pub fn check(&self, model: Option<&ModelSpec>, compact: Option<&ModelSpec>) -> Result<()> {
        for rule in &self.rules {
            let p = self.policy(&rule.path);
            let start = model.map(|m| p.default.as_ref().unwrap_or(m));
            let cheap = compact.map(|c| p.compact.as_ref().unwrap_or(c));
            for (what, spec) in [("default", start), ("compaction", cheap)] {
                if let Some(spec) = spec.filter(|s| !p.allows(s)) {
                    bail!(
                        "{}: the {what} model {spec} is not allowed in {}",
                        Self::FILE,
                        rule.written
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> Config {
        Config::parse(json, Path::new("models.json")).unwrap()
    }

    fn spec(s: &str) -> ModelSpec {
        s.parse().unwrap()
    }

    const RULES: &str = r#"{ "rules": [
        { "path": "/w", "allow": ["cop/*"], "default": "cop/big", "compact": "cop/small" },
        { "path": "/w/free", "allow": ["*"] },
        { "path": "/w/opus", "default": "cop/opus" },
        { "path": "/w/free/x", "compact": "any/m" }
    ] }"#;

    #[test]
    fn keys_inherit_separately() {
        let c = config(RULES);
        let p = c.policy(Path::new("/w/repo"));
        assert_eq!(p.default, Some(spec("cop/big")));
        assert_eq!(p.compact, Some(spec("cop/small")));
        assert!(p.allows(&spec("cop/x")) && !p.allows(&spec("other/x")));
        assert_eq!(p.key, "model:/w");

        let p = c.policy(Path::new("/w/free/x/y"));
        assert!(p.allows(&spec("other/x")));
        assert_eq!(p.default, Some(spec("cop/big")));
        assert_eq!(p.compact, Some(spec("any/m")));
        // `compact` alone does not split the remembered pick.
        assert_eq!(p.key, "model:/w/free");

        let p = c.policy(Path::new("/w/opus"));
        assert_eq!(p.default, Some(spec("cop/opus")));
        assert!(!p.allows(&spec("other/x")));
    }

    #[test]
    fn matches_whole_components() {
        let c = config(RULES);
        let p = c.policy(Path::new("/wx"));
        assert_eq!(
            p,
            Policy {
                allow: None,
                default: None,
                compact: None,
                key: MODEL_KEY.into()
            }
        );
        assert_eq!(c.policy(Path::new("/w")).key, "model:/w");
    }

    #[test]
    fn later_rule_wins_on_equal_paths() {
        let c = config(
            r#"{ "rules": [ { "path": "/w", "default": "a/1" }, { "path": "/w", "default": "a/2" } ] }"#,
        );
        assert_eq!(c.policy(Path::new("/w/x")).default, Some(spec("a/2")));
    }

    #[test]
    fn patterns() {
        let p = |s: &str| Pattern::try_from(s.to_string());
        assert_eq!(p("*").unwrap(), Pattern::Any);
        assert!(p("a/*").unwrap().matches(&spec("a/m")));
        assert!(!p("a/*").unwrap().matches(&spec("b/m")));
        assert!(p("a/m").unwrap().matches(&spec("a/m")));
        assert!(!p("a/m").unwrap().matches(&spec("a/n")));
        assert!(p("nope").is_err());
        assert!(
            Config::parse(r#"{"rules":[{"path":"/w","allow":["x"]}]}"#, Path::new("m")).is_err()
        );
        assert!(Config::parse(r#"{"rules":[{"path":"w"}]}"#, Path::new("m")).is_err());
    }

    #[test]
    fn check_finds_forbidden_starts() {
        let c = config(RULES);
        assert!(c.check(Some(&spec("x/y")), Some(&spec("x/z"))).is_ok());
        let c = config(r#"{ "rules": [ { "path": "/w", "allow": ["cop/*"] } ] }"#);
        let err = c.check(Some(&spec("x/y")), None).unwrap_err().to_string();
        assert!(
            err.contains("default model x/y is not allowed in /w"),
            "{err}"
        );
        assert!(c.check(None, Some(&spec("x/z"))).is_err());
        // Explicit models are checked per session.
        assert!(c.check(None, None).is_ok());
        // A forbidden inherited default shows at the deeper rule.
        let c = config(
            r#"{ "rules": [ { "path": "/w", "default": "a/1" }, { "path": "/w/x", "allow": ["b/*"] } ] }"#,
        );
        assert!(c.check(Some(&spec("b/1")), None).is_err());
    }
}
