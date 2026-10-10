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

/// Creates empty `SYSTEM.md` and `APPEND_SYSTEM.md` in `config_dir` if
/// missing, so users find where to put them. Best effort.
pub fn ensure_files(config_dir: &Path) {
    let _ = std::fs::create_dir_all(config_dir);
    for name in ["SYSTEM.md", "APPEND_SYSTEM.md"] {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(config_dir.join(name));
    }
}

/// `<rules>` are each tool's `Tool::rules`, then the general ones.
pub fn build(
    preamble: &str,
    tools: &[Arc<dyn DynTool>],
    config_dir: Option<&Path>,
    roots: &[PathBuf],
) -> String {
    let cwd = &roots[0];
    // Empty files count as absent: `ensure_files` creates them empty.
    let read = |dir: &Path, name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .filter(|s| !s.trim().is_empty())
    };
    let mut sections: Vec<(&str, String)> = vec![];

    let custom = config_dir.and_then(|d| read(d, "SYSTEM.md"));
    let mut prompt = match custom {
        Some(custom) => custom.trim().to_string(),
        None => {
            let list: Vec<String> = tools
                .iter()
                .map(|t| {
                    let s = t.spec();
                    format!(
                        "- {}: {}",
                        s.name,
                        s.description.lines().next().unwrap_or("")
                    )
                })
                .collect();
            sections.push((
                "tools",
                if list.is_empty() {
                    "(none)".into()
                } else {
                    list.join("\n")
                },
            ));
            let mut rules: Vec<&str> = tools.iter().flat_map(|t| t.rules()).copied().collect();
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
    prompt + &dirs_section(&roots[1..])
}

const DIRS_OPEN: &str = "\n\n<additional_directories>\n";
const DIRS_CLOSE: &str = "\n</additional_directories>";

/// The `<additional_directories>` section, empty without any.
fn dirs_section(dirs: &[PathBuf]) -> String {
    if dirs.is_empty() {
        return String::new();
    }
    let list: Vec<String> = dirs.iter().map(|r| format!("- {}", r.display())).collect();
    format!(
        "{DIRS_OPEN}Also part of this workspace, with the same access as the cwd. Relative paths resolve against the cwd, so use absolute paths for these:\n{}{DIRS_CLOSE}",
        list.join("\n")
    )
}

/// Replaces the additional directories in a built prompt `system`. The
/// section follows `<cwd>`; extensions may have appended more after it.
pub fn replace_dirs(system: &str, cwd: &Path, dirs: &[PathBuf]) -> String {
    let section = dirs_section(dirs);
    if let Some(start) = system.find(DIRS_OPEN)
        && let Some(len) = system[start..].find(DIRS_CLOSE)
    {
        let end = start + len + DIRS_CLOSE.len();
        return format!("{}{section}{}", &system[..start], &system[end..]);
    }
    let cwd = format!("<cwd>\n{}\n</cwd>", cwd.display());
    match system.find(&cwd) {
        Some(at) => {
            let end = at + cwd.len();
            format!("{}{section}{}", &system[..end], &system[end..])
        }
        None => format!("{system}{section}"),
    }
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
            std::slice::from_ref(&cwd),
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

        // Empty files count as absent.
        ensure_files(config.path());
        let empty = build(
            PREAMBLE,
            &crate::tools::builtin(),
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert_eq!(empty, p);
        std::fs::write(config.path().join("APPEND_SYSTEM.md"), " \n").unwrap();
        let p = build(
            PREAMBLE,
            &crate::tools::builtin(),
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert!(!p.contains("<addendum>"), "{p}");
        std::fs::write(config.path().join("APPEND_SYSTEM.md"), "# Mine\nuse rg\n").unwrap();

        // SYSTEM.md replaces preamble, tools and rules.
        std::fs::write(config.path().join("SYSTEM.md"), "Custom.\n").unwrap();
        let p = build(
            PREAMBLE,
            &crate::tools::builtin(),
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert!(p.starts_with("Custom.\n\n<addendum>"), "{p}");
        assert!(!p.contains("<tools>"), "{p}");
        assert!(!p.contains("<additional_directories>"), "{p}");

        // Additional directories follow the cwd.
        let roots = [
            cwd.clone(),
            PathBuf::from("/extra/one"),
            PathBuf::from("/extra/two"),
        ];
        let p = build(PREAMBLE, &[], None, &roots);
        let section = p.split("<additional_directories>").nth(1).unwrap();
        assert!(p.find("<cwd>").unwrap() < p.find("<additional_directories>").unwrap());
        assert!(section.contains("absolute paths"), "{p}");
        assert!(
            section.ends_with("- /extra/one\n- /extra/two\n</additional_directories>"),
            "{p}"
        );

        // Replacing them keeps whatever extensions appended.
        let none = build(PREAMBLE, &[], None, &roots[..1]);
        let p = p + "\n\nfrom an extension";
        let removed = replace_dirs(&p, &cwd, &[]);
        assert_eq!(removed, format!("{none}\n\nfrom an extension"));
        assert_eq!(replace_dirs(&removed, &cwd, &roots[1..]), p);
        let one = replace_dirs(&p, &cwd, &roots[2..]);
        assert!(one.contains("- /extra/two\n</additional_directories>\n\nfrom an extension"));
        assert!(!one.contains("/extra/one"), "{one}");
    }
}
