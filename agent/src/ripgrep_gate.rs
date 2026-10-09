//! Blocks `grep` in Bash calls and tells the model to use `rg`.

use rho::extension::{Context, Decision, Extension, ToolCall};

pub struct RipgrepGate;

/// Same as the regex `\b(grep|egrep|fgrep)\b`: `ripgrep` or `my_grep` pass.
fn uses_grep(command: &str) -> bool {
    command
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| matches!(word, "grep" | "egrep" | "fgrep"))
}

impl Extension for RipgrepGate {
    async fn tool_call(&self, _ctx: &Context, call: &mut ToolCall) -> Decision {
        let command = call.input["command"].as_str().unwrap_or_default();
        if call.name == "Bash" && uses_grep(command) {
            return Decision::Block(
                "Use `rg` (ripgrep) instead of `grep`. Rewrite the command with `rg`.".into(),
            );
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_grep_words_only() {
        for cmd in [
            "grep foo .",
            "ls | grep x",
            "cat f|egrep y",
            "git-grep z",
            "(fgrep a)",
        ] {
            assert!(uses_grep(cmd), "{cmd}");
        }
        for cmd in [
            "rg foo",
            "ripgrep foo",
            "my_grep x",
            "grepper",
            "echo grep_x",
        ] {
            assert!(!uses_grep(cmd), "{cmd}");
        }
    }
}
