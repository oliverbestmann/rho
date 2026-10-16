//! Best-effort shell formatting for showing a command: one command per
//! line, a break before `&&`, `||` and `|`, and indented `if` and loops.
//! Only for display; what runs is the command as the model wrote it.

#[derive(Debug, PartialEq)]
enum Tok {
    /// A word with its quotes, `$(..)`, `(..)` and `${..}` as written.
    Word(String),
    /// `&&`, `||`, `|` or `|&`: the next line starts with it.
    Op(&'static str),
    /// `;`, a newline or `&`: the line ends (after the `&`).
    End(&'static str),
}

/// `command` formatted, or as it is when it has what this does not handle:
/// heredocs, `case` (its `)` patterns) or unbalanced quotes and brackets.
pub(super) fn format(command: &str) -> String {
    let skip = command.contains("<<") || command.split_whitespace().any(|w| w == "case");
    let Some(tokens) = lex(command).filter(|_| !skip) else {
        return command.to_string();
    };
    let mut f = Formatter::default();
    for tok in tokens {
        match tok {
            Tok::End("&") => {
                f.line.push_str(" &");
                f.flush();
            }
            Tok::End(_) => f.flush(),
            Tok::Op(op) => {
                f.flush();
                f.line = op.to_string();
                f.continued = true;
            }
            Tok::Word(w) => f.word(&w),
        }
    }
    f.flush();
    f.lines.join("\n")
}

#[derive(Default)]
struct Formatter {
    lines: Vec<String>,
    line: String,
    indent: usize,
    /// The line starts with an operator, so it is indented one more.
    continued: bool,
}

impl Formatter {
    fn word(&mut self, w: &str) {
        if self.line.is_empty() {
            match w {
                // `if x; then` and `while x; do` stay on one line.
                "then" | "do" if !self.lines.is_empty() => {
                    let last = self.lines.last_mut().unwrap();
                    last.push_str("; ");
                    last.push_str(w);
                    self.indent += 1;
                    return;
                }
                "else" => {
                    self.indent = self.indent.saturating_sub(1);
                    self.line.push_str(w);
                    self.flush();
                    self.indent += 1;
                    return;
                }
                "elif" | "fi" | "done" | "}" => self.indent = self.indent.saturating_sub(1),
                _ => {}
            }
        } else {
            self.line.push(' ');
        }
        self.line.push_str(w);
        if w == "{" {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let depth = self.indent + usize::from(self.continued);
        let line = std::mem::take(&mut self.line);
        if line == "{" || line.ends_with(" {") {
            self.indent += 1;
        }
        self.lines.push(format!("{}{line}", "  ".repeat(depth)));
        self.continued = false;
    }
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let (mut toks, mut word, mut i) = (Vec::new(), String::new(), 0);
    let end_word = |toks: &mut Vec<Tok>, word: &mut String| {
        if !word.is_empty() {
            toks.push(Tok::Word(std::mem::take(word)));
        }
    };
    while i < c.len() {
        let next = c.get(i + 1).copied();
        // `>&2`, `2>&1`, `&>` and `>|` are redirections, not operators.
        let redirect = word.ends_with(['>', '<']);
        let (tok, len) = match c[i] {
            ' ' | '\t' => (None, 1),
            '\\' if next == Some('\n') => (None, 2),
            '\n' => (Some(Tok::End("\n")), 1),
            ';' => (Some(Tok::End(";")), 1 + usize::from(next == Some(';'))),
            '&' if next == Some('&') => (Some(Tok::Op("&&")), 2),
            '|' if next == Some('|') => (Some(Tok::Op("||")), 2),
            '|' if next == Some('&') => (Some(Tok::Op("|&")), 2),
            '&' | '|' if !redirect && next != Some('>') => {
                let tok = if c[i] == '&' {
                    Tok::End("&")
                } else {
                    Tok::Op("|")
                };
                (Some(tok), 1)
            }
            '#' if word.is_empty() => {
                let end = c[i..]
                    .iter()
                    .position(|&x| x == '\n')
                    .map_or(c.len(), |p| i + p);
                word.extend(&c[i..end]);
                i = end;
                continue;
            }
            _ => {
                let end = quoted_end(&c, i)?;
                word.extend(&c[i..end]);
                i = end;
                continue;
            }
        };
        end_word(&mut toks, &mut word);
        toks.extend(tok);
        i += len;
    }
    end_word(&mut toks, &mut word);
    Some(toks)
}

/// The end of what starts at `i`: a quoted string, a backtick command, a
/// bracketed `(..)`, `$(..)` or `${..}`, or else the one character.
fn quoted_end(c: &[char], i: usize) -> Option<usize> {
    let close = match (c[i], c.get(i + 1)) {
        ('\'', _) => return Some(i + 1 + c[i + 1..].iter().position(|&x| x == '\'')? + 1),
        ('"' | '`', _) => {
            let mut j = i + 1;
            while *c.get(j)? != c[i] {
                j += if c[j] == '\\' { 2 } else { 1 };
            }
            return Some(j + 1);
        }
        ('\\', Some(_)) => return Some(i + 2),
        ('$', Some('(')) | ('(', _) => ')',
        ('$', Some('{')) => '}',
        _ => return Some(i + 1),
    };
    let open = if close == ')' { '(' } else { '{' };
    let (mut j, mut depth) = (if c[i] == '$' { i + 2 } else { i + 1 }, 1);
    while depth > 0 {
        match *c.get(j)? {
            '\'' | '"' | '`' | '\\' => j = quoted_end(c, j)?,
            x => {
                depth += usize::from(x == open);
                depth -= usize::from(x == close);
                j += 1;
            }
        }
    }
    Some(j)
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn breaks_chains_and_pipes() {
        assert_eq!(format("ls -la"), "ls -la");
        assert_eq!(
            format("cd web && npm ci | tail -3 || echo failed >&2; ls 2>&1"),
            "cd web\n  && npm ci\n  | tail -3\n  || echo failed >&2\nls 2>&1"
        );
        assert_eq!(format("sleep 1 & wait"), "sleep 1 &\nwait");
    }

    #[test]
    fn indents_blocks() {
        assert_eq!(
            format("if [ -f a ]; then echo a; elif true; then :; else echo b; fi"),
            "if [ -f a ]; then\n  echo a\nelif true; then\n  :\nelse\n  echo b\nfi"
        );
        assert_eq!(
            format("for f in *.rs; do wc -l $f; done && f() { a; b; }"),
            "for f in *.rs; do\n  wc -l $f\ndone\n  && f() {\n  a\n  b\n}"
        );
    }

    #[test]
    fn indents_every_loop() {
        for kw in ["while", "until"] {
            assert_eq!(
                format(&format!("{kw} ! curl -s x; do sleep 1; done")),
                format!("{kw} ! curl -s x; do\n  sleep 1\ndone")
            );
        }
    }

    #[test]
    fn keeps_quotes_and_substitutions_whole() {
        assert_eq!(
            format(r#"echo "a; b && c" 'x | y' $(cd d && ls) `a;b` ${x:-;} (cd e; ls)"#),
            r#"echo "a; b && c" 'x | y' $(cd d && ls) `a;b` ${x:-;} (cd e; ls)"#
        );
        assert_eq!(format("echo a # b; c\nls"), "echo a # b; c\nls");
    }

    #[test]
    fn leaves_what_it_does_not_handle() {
        for cmd in [
            "cat <<EOF\na; b\nEOF",
            "case $x in a) b;; esac",
            "echo 'open",
        ] {
            assert_eq!(format(cmd), cmd);
        }
    }
}
