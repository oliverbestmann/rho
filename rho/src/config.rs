//! `config.d` in the config dir (ADR 0066). Its `*.json` files are applied
//! in file name order as JSON merge patches (RFC 7396): objects merge,
//! other values replace, `null` deletes. The result has one key per
//! subsystem (`sandbox`, `features`, `models`, `mcps`). Prompt files
//! (`*PREAMBLE_SYSTEM.md`, `*APPEND_SYSTEM.md`) are joined in name order.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

pub const DIR: &str = "config.d";

/// Files of the old layout, with where their content goes now.
const OLD: [(&str, &str); 6] = [
    ("sandbox.json", "\"sandbox\" in a config.d/*.json"),
    ("features.json", "\"features\" in a config.d/*.json"),
    ("models.json", "\"models\" in a config.d/*.json"),
    ("mcp.json", "\"mcps\" in a config.d/*.json"),
    ("PREAMBLE_SYSTEM.md", "config.d/00-rho.PREAMBLE_SYSTEM.md"),
    ("APPEND_SYSTEM.md", "config.d/APPEND_SYSTEM.md"),
];

/// Fails if a file of the old layout is still in `config_dir`, so its
/// content is not silently ignored.
pub fn check_old(config_dir: &Path) -> Result<()> {
    let old: Vec<String> = OLD
        .iter()
        .filter(|(name, _)| config_dir.join(name).symlink_metadata().is_ok())
        .map(|(name, to)| format!("- {name} -> {to}"))
        .collect();
    if !old.is_empty() {
        bail!(
            "these files in {} are no longer read; move them (new format: ADR 0066):\n{}",
            config_dir.display(),
            old.join("\n")
        );
    }
    Ok(())
}

/// The merged `config.d/*.json`; `{}` without any.
pub fn load(config_dir: &Path) -> Result<Value> {
    let mut config = Value::Object(Map::new());
    for path in files(config_dir, ".json")? {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let patch: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if !patch.is_object() {
            bail!("{}: not a JSON object", path.display());
        }
        merge(&mut config, patch);
    }
    Ok(config)
}

/// Applies `patch` to `base` as an RFC 7396 merge patch.
pub fn merge(base: &mut Value, patch: Value) {
    let Value::Object(patch) = patch else {
        *base = patch;
        return;
    };
    if !base.is_object() {
        *base = Value::Object(Map::new());
    }
    let base = base.as_object_mut().expect("an object");
    for (key, value) in patch {
        if value.is_null() {
            base.remove(&key);
        } else {
            merge(base.entry(key).or_insert(Value::Null), value);
        }
    }
}

/// `config[key]`, or the default if it is missing.
pub fn section<T: DeserializeOwned + Default>(config: &Value, key: &str) -> Result<T> {
    let Some(value) = config.get(key) else {
        return Ok(T::default());
    };
    T::deserialize(value).with_context(|| format!("{DIR}: {key}"))
}

/// The non-empty `config.d` files whose names end in `suffix`, trimmed and
/// joined in name order. Unreadable files are skipped.
pub fn text(config_dir: &Path, suffix: &str) -> Option<String> {
    let parts: Vec<String> = files(config_dir, suffix)
        .ok()?
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Files in `config.d` whose names end in `suffix`, by name. Hidden files
/// (editor swap files) are skipped.
pub(crate) fn files(config_dir: &Path, suffix: &str) -> Result<Vec<PathBuf>> {
    let dir = config_dir.join(DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut files = vec![];
    for entry in entries {
        let name = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .file_name();
        if let Some(name) = name.to_str()
            && name.ends_with(suffix)
            && !name.starts_with('.')
        {
            files.push(dir.join(name));
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_follows_rfc_7396() {
        let mut base = json!({"a": {"b": 1, "c": [1, 2]}, "d": 1, "e": "x"});
        merge(
            &mut base,
            json!({"a": {"c": [3], "n": {"x": null, "y": 1}}, "d": null, "e": {"f": 1}}),
        );
        assert_eq!(
            base,
            json!({"a": {"b": 1, "c": [3], "n": {"y": 1}}, "e": {"f": 1}})
        );
    }

    #[test]
    fn load_merges_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), json!({}));
        let d = dir.path().join(DIR);
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("50-b.json"), r#"{"x": {"a": 2, "c": null}}"#).unwrap();
        std::fs::write(d.join("10-a.json"), r#"{"x": {"a": 1, "b": 1, "c": 1}}"#).unwrap();
        std::fs::write(d.join("notes.txt"), "not json").unwrap();
        std::fs::write(d.join(".swap.json"), "not json").unwrap();
        let config = load(dir.path()).unwrap();
        assert_eq!(config, json!({"x": {"a": 2, "b": 1}}));

        #[derive(serde::Deserialize, Default, Debug, PartialEq)]
        #[serde(deny_unknown_fields)]
        struct X {
            a: u32,
            b: u32,
        }
        assert_eq!(section::<X>(&config, "x").unwrap(), X { a: 2, b: 1 });
        assert_eq!(section::<X>(&config, "none").unwrap(), X::default());

        std::fs::write(d.join("60-c.json"), r#"{"x": {"z": 1}}"#).unwrap();
        let err = section::<X>(&load(dir.path()).unwrap(), "x").unwrap_err();
        assert!(
            format!("{err:#}").starts_with("config.d: x: unknown field"),
            "{err:#}"
        );

        std::fs::write(d.join("70-bad.json"), "[1]").unwrap();
        let err = load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("70-bad.json"), "{err}");
    }

    #[test]
    fn text_joins_non_empty_files_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(text(dir.path(), "APPEND_SYSTEM.md"), None);
        let d = dir.path().join(DIR);
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("APPEND_SYSTEM.md"), "late\n").unwrap();
        std::fs::write(d.join("10-go.APPEND_SYSTEM.md"), "\n# Go\n").unwrap();
        std::fs::write(d.join("20-empty.APPEND_SYSTEM.md"), " \n").unwrap();
        std::fs::write(d.join("x.PREAMBLE_SYSTEM.md"), "other").unwrap();
        assert_eq!(
            text(dir.path(), "APPEND_SYSTEM.md").unwrap(),
            "# Go\n\nlate"
        );
    }

    #[test]
    fn old_files_fail() {
        let dir = tempfile::tempdir().unwrap();
        check_old(dir.path()).unwrap();
        std::fs::write(dir.path().join("mcp.json"), "{}").unwrap();
        let err = check_old(dir.path()).unwrap_err().to_string();
        assert!(err.contains("- mcp.json -> \"mcps\""), "{err}");
    }
}
