//! `Edit`: exact text replacement, like pi's edit tool (without its fuzzy
//! matching). All edits match against the original file; the change is sent
//! to the user as a diff.

use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{Context, Diff, ToolOutput};
use crate::provider::{ToolName, ToolSpec};
use crate::tools::{Tool, input_schema, parse, resolve};

#[derive(Deserialize, JsonSchema)]
struct EditInput {
    /// Path to the file to edit (relative or absolute)
    path: String,
    /// One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.
    edits: Vec<Replace>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Replace {
    /// Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.
    old_text: String,
    /// Replacement text for this targeted edit.
    new_text: String,
}

pub const EDIT: ToolName = ToolName::from_static("Edit");

pub struct Edit;

impl Tool for Edit {
    fn rules(&self) -> &'static [&'static str] {
        &[
            "Use Edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one Edit call with multiple entries in edits[] instead of multiple Edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ]
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: EDIT,
            description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes. Sandboxed like Bash.".into(),
            input_schema: input_schema::<EditInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: EditInput = parse(fix_input(input))?;
        if i.edits.is_empty() {
            bail!("edits must contain at least one replacement");
        }
        let path = resolve(ctx.cwd(), &i.path);
        let shown = i.path.clone();
        let count = i.edits.len();
        let file = path.clone();
        // Read, replace and write all on the sandboxed thread.
        let (old, new) = ctx
            .run_sandboxed(move || -> Result<(String, String)> {
                let raw = std::fs::read_to_string(&file)
                    .map_err(|e| anyhow::anyhow!("could not edit {shown}: {e}"))?;
                let (bom, text) = match raw.strip_prefix('\u{feff}') {
                    Some(text) => ("\u{feff}", text),
                    None => ("", raw.as_str()),
                };
                let crlf = text.contains("\r\n");
                let old = text.replace("\r\n", "\n");
                let new = apply(&old, &i.edits, &shown)?;
                let out = if crlf {
                    new.replace('\n', "\r\n")
                } else {
                    new.clone()
                };
                std::fs::write(&file, format!("{bom}{out}"))?;
                Ok((old, new))
            })
            .await??;
        Ok(ToolOutput {
            output: format!("Successfully replaced {count} block(s) in {}.", i.path),
            is_error: false,
            diff: Some(Diff { path, old, new }),
            ..Default::default()
        })
    }
}

/// Models sometimes send `edits` as a JSON string or a single object, or
/// `oldText`/`newText` at the top level (pi accepts all of these).
fn fix_input(mut input: Value) -> Value {
    let Some(obj) = input.as_object_mut() else {
        return input;
    };
    if let Some(Value::String(s)) = obj.get("edits")
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
    {
        obj.insert("edits".into(), parsed);
    }
    if let Some(single @ Value::Object(_)) = obj.get("edits") {
        let single = single.clone();
        obj.insert("edits".into(), Value::Array(vec![single]));
    }
    if let (Some(old), Some(new)) = (obj.remove("oldText"), obj.remove("newText")) {
        let edits = obj.entry("edits").or_insert(Value::Array(vec![]));
        if let Value::Array(list) = edits {
            list.push(serde_json::json!({"oldText": old, "newText": new}));
        }
    }
    input
}

/// Applies all edits to LF-normalized `content`, each matched against the original.
fn apply(content: &str, edits: &[Replace], path: &str) -> Result<String> {
    let name = |i: usize| {
        if edits.len() == 1 {
            "the text".to_string()
        } else {
            format!("edits[{i}]")
        }
    };
    let mut matches = Vec::with_capacity(edits.len());
    for (i, edit) in edits.iter().enumerate() {
        let old = edit.old_text.replace("\r\n", "\n");
        if old.is_empty() {
            bail!("oldText of {} must not be empty in {path}", name(i));
        }
        let mut found = content.match_indices(&old).map(|(at, _)| at);
        let Some(at) = found.next() else {
            bail!(
                "could not find {} in {path}. The oldText must match exactly including all whitespace and newlines.",
                name(i)
            );
        };
        let more = found.count();
        if more > 0 {
            bail!(
                "found {} occurrences of {} in {path}. Each oldText must be unique; add more context.",
                more + 1,
                name(i)
            );
        }
        matches.push((at, old.len(), edit.new_text.replace("\r\n", "\n"), i));
    }
    matches.sort_by_key(|m| m.0);
    for pair in matches.windows(2) {
        if pair[0].0 + pair[0].1 > pair[1].0 {
            bail!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].3,
                pair[1].3
            );
        }
    }
    let mut out = content.to_string();
    for (at, len, new, _) in matches.into_iter().rev() {
        out.replace_range(at..at + len, &new);
    }
    if out == content {
        bail!("no changes made to {path}: the replacements produced identical content");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionId;
    use serde_json::json;
    use std::sync::Arc;

    fn edits(pairs: &[(&str, &str)]) -> Vec<Replace> {
        pairs
            .iter()
            .map(|(o, n)| Replace {
                old_text: o.to_string(),
                new_text: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn applies_disjoint_edits_against_the_original() {
        let out = apply("a b c", &edits(&[("c", "C"), ("a", "A b")]), "f").unwrap();
        assert_eq!(out, "A b b C");
    }

    #[test]
    fn rejects_missing_duplicate_overlapping_empty_and_noop() {
        let err = |e: &[(&str, &str)]| {
            apply("one two one", &edits(e), "f")
                .unwrap_err()
                .to_string()
        };
        assert!(err(&[("three", "x")]).contains("could not find"));
        assert!(err(&[("one", "x")]).contains("2 occurrences"));
        assert!(err(&[("one two", "x"), ("two one", "y")]).contains("overlap"));
        assert!(err(&[("", "x")]).contains("empty"));
        assert!(err(&[("two", "two")]).contains("no changes"));
    }

    #[test]
    fn accepts_the_odd_input_shapes() {
        let want = json!({"path": "f", "edits": [{"oldText": "a", "newText": "b"}]});
        let e = json!({"oldText": "a", "newText": "b"});
        for input in [
            json!({"path": "f", "edits": e.to_string()}),
            json!({"path": "f", "edits": e}),
            json!({"path": "f", "oldText": "a", "newText": "b"}),
            want.clone(),
        ] {
            assert_eq!(fix_input(input), want);
        }
    }

    #[tokio::test]
    async fn keeps_bom_and_crlf_and_returns_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "\u{feff}one\r\ntwo\r\n").unwrap();
        let ctx = Context::new(
            SessionId::new("t"),
            dir.path(),
            Arc::new(crate::extension::NoFrontend),
            Default::default(),
        );
        let input = json!({"path": "f.txt", "edits": [{"oldText": "one\ntwo", "newText": "1\n2"}]});
        let out = Edit.run(input, &ctx).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "\u{feff}1\r\n2\r\n"
        );
        let diff = out.diff.unwrap();
        assert_eq!(
            (diff.path, diff.old.as_str(), diff.new.as_str()),
            (file, "one\ntwo\n", "1\n2\n")
        );
    }

    #[tokio::test]
    async fn is_sandboxed() {
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let file = other.path().join("f");
        std::fs::write(&file, "a").unwrap();
        let ctx = Context::new(
            SessionId::new("t"),
            cwd.path(),
            Arc::new(crate::extension::NoFrontend),
            Default::default(),
        );
        let input = json!({"path": file, "edits": [{"oldText": "a", "newText": "b"}]});
        let err = format!("{:#}", Edit.run(input, &ctx).await.unwrap_err());
        assert!(err.contains("Permission denied"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a");
    }
}
