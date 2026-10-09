//! System prompt, built like pi's: preamble, tools, rules, then the
//! `APPEND_SYSTEM.md` addendum, project context files and the cwd, each in
//! `<section>` tags. `SYSTEM.md` in the config dir replaces preamble, tools
//! and rules (pi's custom prompt).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tools::DynTool;

pub const PREAMBLE: &str = "You are an expert coding assistant operating inside rho, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.";

/// Looked up in the config dir, then in the cwd and each parent (outermost first).
const CONTEXT_FILES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// `$XDG_CONFIG_HOME/rho`, else `~/.config/rho`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("rho")
}

pub fn build(
    preamble: &str,
    tools: &[Arc<dyn DynTool>],
    config_dir: Option<&Path>,
    cwd: &Path,
) -> String {
    let read = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let mut sections: Vec<(&str, String)> = vec![];

    let custom = config_dir.and_then(|d| read(d, "SYSTEM.md"));
    let mut prompt = match custom {
        Some(custom) => custom.trim().to_string(),
        None => {
            let specs: Vec<_> = tools.iter().map(|t| t.spec()).collect();
            let list: Vec<String> = specs
                .iter()
                .map(|s| {
                    format!(
                        "- {}: {}",
                        s.name,
                        s.description.lines().next().unwrap_or("")
                    )
                })
                .collect();
            sections.push((
                "tools",
                format!(
                    "{}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.",
                    if list.is_empty() { "(none)".into() } else { list.join("\n") }
                ),
            ));
            let has = |name: &str| specs.iter().any(|s| s.name == name);
            let mut rules = vec![];
            if has("Bash") {
                rules.push("Use Bash for file operations like ls, rg, find");
            }
            if has("Read") {
                rules.push("Use Read to examine files instead of cat or sed.");
            }
            if has("Write") {
                rules.push("Use Write only for new files or complete rewrites.");
            }
            rules.push("Be concise in your responses");
            rules.push("Show file paths clearly when working with files");
            let rules: Vec<String> = rules.iter().map(|r| format!("- {r}")).collect();
            sections.push(("rules", rules.join("\n")));
            preamble.to_string()
        }
    };

    if let Some(addendum) = config_dir.and_then(|d| read(d, "APPEND_SYSTEM.md")) {
        sections.push(("addendum", addendum.trim().to_string()));
    }

    let mut files = vec![];
    let mut dirs: Vec<&Path> = cwd.ancestors().collect();
    dirs.reverse();
    for dir in config_dir.into_iter().chain(dirs) {
        let found = CONTEXT_FILES
            .iter()
            .find_map(|name| Some((dir.join(name), read(dir, name)?)));
        if let Some((path, content)) = found
            && !files.iter().any(|(p, _)| p == &path)
        {
            files.push((path, content));
        }
    }
    if !files.is_empty() {
        let mut text = String::from("Project-specific instructions and guidelines:");
        for (path, content) in files {
            text += &format!(
                "\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                path.display(),
                content.trim()
            );
        }
        sections.push(("project_context", text));
    }

    sections.push(("cwd", cwd.display().to_string()));
    for (name, text) in sections {
        prompt += &format!("\n\n<{name}>\n{text}\n</{name}>");
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_sections_from_files() {
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().join("sub");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(config.path().join("APPEND_SYSTEM.md"), "# Mine\nuse rg\n").unwrap();
        std::fs::write(config.path().join("AGENTS.md"), "global rules").unwrap();
        std::fs::write(project.path().join("CLAUDE.md"), "project rules").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "sub rules").unwrap();

        let p = build(
            PREAMBLE,
            &crate::tools::builtin(),
            Some(config.path()),
            &cwd,
        );
        assert!(p.starts_with(PREAMBLE), "{p}");
        assert!(p.contains("<tools>\n- Read: Read a text file."), "{p}");
        assert!(p.contains("- Use Read to examine files"), "{p}");
        assert!(p.contains("<addendum>\n# Mine\nuse rg\n</addendum>"), "{p}");
        let (g, pr, s) = (
            p.find("global rules").unwrap(),
            p.find("project rules").unwrap(),
            p.find("sub rules").unwrap(),
        );
        assert!(g < pr && pr < s, "config, then outermost to innermost: {p}");
        assert!(
            p.ends_with(&format!("<cwd>\n{}\n</cwd>", cwd.display())),
            "{p}"
        );

        // SYSTEM.md replaces preamble, tools and rules.
        std::fs::write(config.path().join("SYSTEM.md"), "Custom.\n").unwrap();
        let p = build(
            PREAMBLE,
            &crate::tools::builtin(),
            Some(config.path()),
            &cwd,
        );
        assert!(p.starts_with("Custom.\n\n<addendum>"), "{p}");
        assert!(!p.contains("<tools>"), "{p}");
    }
}
