//! Project context files (`AGENTS.md`, `CLAUDE.md`, ... in the cwd and its
//! parents) are read only with the user's consent: before the first prompt
//! of a session rho asks about each directory it has no answer for, and
//! remembers "always"/"never" per directory in
//! `~/.config/rho/project_context.json`. The approved files go at the end
//! of the system prompt, fixed for the session.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use rho::extension::{
    Choice, ChoiceValue, Context, Extension, Field, FieldKey, FieldKind, Form, FormReply,
    PromptSetup,
};

pub struct ProjectContext {
    /// Directory -> read its context file.
    prefs: PathBuf,
}

/// What to append to the system prompt, in the session's `Context::state`.
#[derive(Default)]
struct Section(OnceLock<String>);

impl Default for ProjectContext {
    fn default() -> Self {
        Self {
            prefs: rho::config_dir().join("project_context.json"),
        }
    }
}

type Prefs = BTreeMap<String, bool>;

const ALWAYS: ChoiceValue = ChoiceValue::from_static("always");
const NEVER: ChoiceValue = ChoiceValue::from_static("never");

impl ProjectContext {
    fn load(&self) -> Prefs {
        std::fs::read_to_string(&self.prefs)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Asks about the files without a remembered answer and returns the ones
    /// to read.
    async fn approved(&self, ctx: &Context) -> Vec<(PathBuf, String)> {
        let mut dirs: Vec<_> = ctx.cwd().ancestors().collect();
        dirs.reverse();
        let found: Vec<_> = dirs
            .into_iter()
            .filter_map(|d| Some((d.display().to_string(), rho::prompt::context_file(d)?)))
            .collect();
        let prefs = self.load();
        let unknown: Vec<_> = found
            .iter()
            .filter(|(d, _)| !prefs.contains_key(d))
            .map(|(_, (path, _))| path)
            .collect();
        let mut answers = Prefs::new();
        if !unknown.is_empty() {
            let form = Form {
                message: "Read these project instructions into the system prompt?".into(),
                fields: unknown
                    .iter()
                    .map(|path| Field {
                        key: FieldKey::new(path.display().to_string()),
                        title: path.display().to_string(),
                        description: None,
                        kind: FieldKind::Choice {
                            options: vec![
                                choice(ALWAYS, "Always read"),
                                choice(NEVER, "Never read"),
                            ],
                            multiple: false,
                        },
                        required: true,
                    })
                    .collect(),
            };
            match ctx.ask(&form).await {
                FormReply::Answers(a) => {
                    for path in &unknown {
                        let key = FieldKey::new(path.display().to_string());
                        if let Some(v) = a.get(&key).and_then(|v| v.first()) {
                            let dir = path.parent().unwrap().display().to_string();
                            answers.insert(dir, *v == ALWAYS.as_str());
                        }
                    }
                    self.save(&answers);
                }
                // Asked again next session.
                FormReply::Declined => {}
                // No forms: ask per file, for this session only.
                FormReply::Unsupported => {
                    for path in &unknown {
                        let title = format!("Read project instructions {}?", path.display());
                        let dir = path.parent().unwrap().display().to_string();
                        answers.insert(dir, ctx.confirm(None, &title).await);
                    }
                }
            }
        }
        found
            .into_iter()
            .filter(|(d, _)| prefs.get(d).or(answers.get(d)) == Some(&true))
            .map(|(_, file)| file)
            .collect()
    }

    /// Merges `answers` into the file.
    // shortcut: read-modify-write without a lock, so two sessions answering
    // at once can lose one answer; use a lock file if that happens.
    fn save(&self, answers: &Prefs) {
        let mut prefs = self.load();
        prefs.extend(answers.clone());
        if let Some(dir) = self.prefs.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.prefs, serde_json::to_string_pretty(&prefs).unwrap());
    }
}

fn choice(value: ChoiceValue, title: &str) -> Choice {
    Choice {
        value,
        title: title.into(),
        description: None,
    }
}

impl Extension for ProjectContext {
    async fn before_prompt(&self, ctx: &Context, prompt: &mut PromptSetup) {
        let state = ctx.state::<Section>();
        let section = match state.0.get() {
            Some(section) => section,
            None => {
                let files = self.approved(ctx).await;
                let section = if files.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\n<project_context>\n{}\n</project_context>",
                        rho::prompt::context_text(&files)
                    )
                };
                state.0.get_or_init(|| section)
            }
        };
        prompt.system += section;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use rho::extension::NoFrontend;
    use rho::store::SessionId;

    #[tokio::test]
    async fn reads_only_approved_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let (outer, inner, asked) = (
            tmp.path().join("outer"),
            tmp.path().join("outer/inner"),
            tmp.path().join("outer/inner/asked"),
        );
        std::fs::create_dir_all(&asked).unwrap();
        std::fs::write(outer.join("AGENTS.md"), "outer rules").unwrap();
        std::fs::write(inner.join("CLAUDE.md"), "inner rules").unwrap();
        std::fs::write(asked.join("AGENTS.md"), "asked rules").unwrap();
        let prefs = tmp.path().join("prefs.json");
        let remembered = Prefs::from([
            (outer.display().to_string(), true),
            (inner.display().to_string(), false),
        ]);
        std::fs::write(&prefs, serde_json::to_string(&remembered).unwrap()).unwrap();

        let ext = ProjectContext {
            prefs: prefs.clone(),
        };
        // NoFrontend has no forms and denies confirmations.
        let ctx = Context::new(
            SessionId::new("s"),
            &asked,
            Arc::new(NoFrontend),
            Default::default(),
        );
        let mut setup = PromptSetup {
            content: vec![],
            system: "base".into(),
        };
        ext.before_prompt(&ctx, &mut setup).await;
        let p = setup.system;
        assert!(p.starts_with("base\n\n<project_context>\n"), "{p}");
        assert!(
            p.ends_with("outer rules\n</project_instructions>\n</project_context>"),
            "{p}"
        );
        assert!(
            !p.contains("inner rules") && !p.contains("asked rules"),
            "{p}"
        );
        // Confirmations are not remembered.
        assert_eq!(ext.load(), remembered);
    }
}
