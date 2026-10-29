//! System prompt, built like pi's: preamble, tools, rules, then the
//! addendum (`config.d/*APPEND_SYSTEM.md`), the config dir's context file
//! and the cwd, each in `<section>` tags. `config.d/*PREAMBLE_SYSTEM.md`
//! replaces the preamble. Files with the same suffix are joined in name order.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tools::DynTool;

pub const PREAMBLE: &str = "You are an expert coding assistant operating inside rho, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.";

/// Looked up in that order; the first one present in a directory counts.
const CONTEXT_FILES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

pub const PREAMBLE_SUFFIX: &str = "PREAMBLE_SYSTEM.md";
pub const APPEND_SUFFIX: &str = "APPEND_SYSTEM.md";

/// Creates `config.d` and, without any preamble file in it,
/// `00-rho.PREAMBLE_SYSTEM.md` with `PREAMBLE`, to be edited. Best effort.
pub fn ensure_files(config_dir: &Path) {
    let dir = config_dir.join(crate::config::DIR);
    let _ = std::fs::create_dir_all(&dir);
    if crate::config::files(config_dir, PREAMBLE_SUFFIX).is_ok_and(|f| f.is_empty()) {
        let _ = std::fs::write(
            dir.join(format!("00-rho.{PREAMBLE_SUFFIX}")),
            format!("{PREAMBLE}\n"),
        );
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
    let mut sections: Vec<(&str, String)> = vec![];

    let mut prompt = config_dir
        .and_then(|d| crate::config::text(d, PREAMBLE_SUFFIX))
        .unwrap_or_else(|| preamble.to_string());

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
    rules.push("Tool calls in one response run one after another, in the order given. Batch independent calls; calls that depend on each other may share a batch.");
    rules.push("Be concise in your responses");
    rules.push("Show file paths clearly when working with files");
    let rules: Vec<String> = rules.iter().map(|r| format!("- {r}")).collect();
    sections.push(("rules", rules.join("\n")));

    if let Some(addendum) = config_dir.and_then(|d| crate::config::text(d, APPEND_SUFFIX)) {
        sections.push(("addendum", addendum));
    }

    // Project context files in the cwd's ancestors need the user's consent;
    // the agent binary's `ProjectContext` extension adds them.
    if let Some(file) = config_dir.and_then(context_file) {
        sections.push(("project_context", context_text(&[file])));
    }

    sections.push(("cwd", cwd.display().to_string()));
    for (name, text) in sections {
        prompt += &format!("\n\n<{name}>\n{text}\n</{name}>");
    }
    prompt + &dirs_section(&roots[1..])
}

/// Empty files count as absent.
fn read(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The first context file (`AGENTS.md`, `CLAUDE.md`, ...) in `dir`.
pub fn context_file(dir: &Path) -> Option<(PathBuf, String)> {
    CONTEXT_FILES
        .iter()
        .find_map(|name| Some((dir.join(name), read(dir, name)?)))
}

/// The body of a `<project_context>` section with `files` (path, content).
pub fn context_text(files: &[(PathBuf, String)]) -> String {
    let mut text = String::from("Project-specific instructions and guidelines:");
    for (path, content) in files {
        text += &format!(
            "\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            path.display(),
            content.trim()
        );
    }
    text
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
        let tools: Vec<Arc<dyn DynTool>> = vec![Arc::new(crate::tools::Read)];
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path().join("sub");
        std::fs::create_dir(&cwd).unwrap();
        let d = config.path().join(crate::config::DIR);
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("APPEND_SYSTEM.md"), "# Mine\nuse rg\n").unwrap();
        std::fs::write(config.path().join("AGENTS.md"), "global rules").unwrap();
        std::fs::write(project.path().join("CLAUDE.md"), "project rules").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "sub rules").unwrap();

        let p = build(
            PREAMBLE,
            &tools,
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert!(p.starts_with(PREAMBLE), "{p}");
        assert!(p.contains("<tools>\n- Read: Read a text file."), "{p}");
        assert!(p.contains("- Use Read to examine files"), "{p}");
        assert!(
            p.contains("- Tool calls in one response run one after another"),
            "{p}"
        );
        assert!(p.contains("<addendum>\n# Mine\nuse rg\n</addendum>"), "{p}");
        assert!(p.contains("global rules"), "{p}");
        assert!(
            !p.contains("project rules") && !p.contains("sub rules"),
            "{p}"
        );
        assert!(
            p.ends_with(&format!("<cwd>\n{}\n</cwd>", cwd.display())),
            "{p}"
        );

        // The written preamble is the default; empty files count as absent.
        ensure_files(config.path());
        let written = d.join("00-rho.PREAMBLE_SYSTEM.md");
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            format!("{PREAMBLE}\n")
        );
        let empty = build(
            PREAMBLE,
            &tools,
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert_eq!(empty, p);
        std::fs::write(d.join("APPEND_SYSTEM.md"), " \n").unwrap();
        let p = build(
            PREAMBLE,
            &tools,
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert!(!p.contains("<addendum>"), "{p}");
        std::fs::write(d.join("APPEND_SYSTEM.md"), "# Mine\nuse rg\n").unwrap();

        // Preamble files replace only the preamble, joined in name order.
        std::fs::write(&written, "Custom.\n").unwrap();
        std::fs::write(d.join("50-more.PREAMBLE_SYSTEM.md"), "More.").unwrap();
        // An existing preamble file is kept.
        ensure_files(config.path());
        let p = build(
            PREAMBLE,
            &tools,
            Some(config.path()),
            std::slice::from_ref(&cwd),
        );
        assert!(p.starts_with("Custom.\n\nMore.\n\n<tools>\n- Read:"), "{p}");
        assert!(p.contains("- Use Read to examine files"), "{p}");
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
