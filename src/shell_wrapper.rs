//! Conservative parser for `<shell> -c "<script>"` invocations.
//!
//! Fail-closed contract: anything the parser cannot unambiguously model as a
//! sequence of simple commands must return [`Wrapped::Unparseable`], which
//! makes the caller fall back to today's whole-invocation approval behavior.
//! Only scripts whose accepted syntax is modeled as simple command words and
//! separators may be returned as [`Wrapped::Commands`]. Unquoted glob
//! characters are accepted verbatim as argument text, and the parser does not
//! distinguish a quoted literal `*` from an unquoted glob. This leaves two
//! accepted residual risks: the shell expands globs at execution time, and
//! those expansion results are not path-checked at approval time (a workspace
//! symlink could direct expansion outside). Planting a symlink to steer glob
//! expansion requires `ln` or another mutating command (prompt/deny-gated), and
//! `write_file` cannot create symlinks. Also, expansion can produce flag-like
//! words (for example, a file named `-o` or `--pre=x`) that policy/heuristic
//! gates never saw; that remains a documented residual risk.
//!
//! The `-l` flag has accepted residuals: login shells source `/etc/profile` and
//! user-owned `~/.bash_profile` (`HOME` passes through the isolated
//! environment). A profile may define functions called by the judged script,
//! and a profile `cd` can invalidate per-segment relative-path/outside
//! assumptions.
//!
//! Blocked-pattern entries `2>/dev/` and `> /dev/` are effectively dead for
//! wrapped scripts because exact-token redirect recognition and subsequent
//! scan tokenization cover those cases; they remain for direct-argv matching.

/// One simple command: program token plus arguments (quotes already removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    pub command: String,
    pub args: Vec<String>,
}

/// Outcome of inspecting a potential `<shell> -c "<script>"` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wrapped {
    /// Not a supported shell `-c` form at all; handle as an ordinary call.
    NotWrapper,
    /// A supported shell `-c` form whose script cannot be modeled safely;
    /// fall back to whole-invocation approval.
    Unparseable,
    /// The script is exactly the listed simple commands, in order.
    Commands(Vec<SimpleCommand>),
}

/// Inspect `command`/`args` of a shell tool call.
///
/// This parser deliberately accepts only syntax whose interpretation is
/// otherwise unambiguous: every accepted segment is represented by its
/// returned words in bash, sh, zsh, and dash. Unquoted glob arguments are the
/// exception; their expansion is deferred to shell execution. Three exact
/// unquoted harmless redirect tokens are consumed; other redirects, control
/// flow, and unmodeled syntax are rejected so the caller can approve the whole
/// invocation instead. Keep this fail-closed: accepted segments are judged
/// individually by the permission policy.
pub fn unwrap_shell_c(command: &str, args: &[String]) -> Wrapped {
    if !crate::tools::is_normalized_command_path(command) {
        return Wrapped::NotWrapper;
    }
    let shell = basename(command);
    if !matches!(shell.as_str(), "bash" | "sh" | "zsh" | "dash") {
        return Wrapped::NotWrapper;
    }

    let has_c_flag = args.iter().any(|arg| {
        arg.strip_prefix('-')
            .filter(|rest| !rest.starts_with('-'))
            .is_some_and(|rest| rest.contains('c'))
    });
    if !has_c_flag {
        return Wrapped::NotWrapper;
    }
    if args.len() != 2 {
        return Wrapped::Unparseable;
    }
    let flags = &args[0];
    if flags.len() < 2
        || !flags.starts_with('-')
        || flags[1..].chars().any(|c| !matches!(c, 'c' | 'e' | 'u' | 'l' | 'x'))
        || !flags[1..].contains('c')
        || args[1].is_empty()
    {
        return Wrapped::Unparseable;
    }
    if args[1].len() > 4096 {
        return Wrapped::Unparseable;
    }

    let segments = match tokenize(&args[1]) {
        Some(segments) => segments,
        None => return Wrapped::Unparseable,
    };
    if segments.is_empty() || segments.len() > 16 {
        return Wrapped::Unparseable;
    }
    let mut commands = Vec::with_capacity(segments.len());
    for mut words in segments {
        if words.is_empty() {
            return Wrapped::Unparseable;
        }
        let command = words.remove(0);
        if command.is_empty()
            || command.starts_with('-')
            || !command
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '+' | '-'))
            || command.contains('=')
        {
            return Wrapped::Unparseable;
        }
        let name = basename(&command);
        if is_builtin_or_keyword(&name) || is_nested_wrapper(&name) {
            return Wrapped::Unparseable;
        }
        commands.push(SimpleCommand {
            command,
            args: words,
        });
    }
    Wrapped::Commands(commands)
}

fn basename(command: &str) -> String {
    let name = command
        .rsplit('/')
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_owned()
}

fn is_nested_wrapper(name: &str) -> bool {
    matches!(
        name,
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish" | "env" | "xargs" | "sudo"
            | "su" | "doas" | "nohup"
    )
}

/// This list must grow whenever a policy allow-rule is added for a name that
/// is also a shell builtin: such a command cannot safely be judged as an
/// external simple command here.
fn is_builtin_or_keyword(name: &str) -> bool {
    matches!(
        name,
        "cd" | "pushd" | "popd" | "dirs" | "eval" | "exec" | "source" | "." | ":"
            | "set" | "unset" | "export" | "readonly" | "declare" | "typeset" | "local"
            | "alias" | "unalias" | "trap" | "if" | "then" | "else" | "elif" | "fi"
            | "for" | "while" | "until" | "do" | "done" | "case" | "esac" | "in"
            | "function" | "select" | "coproc" | "time" | "command" | "builtin" | "exit"
            | "return" | "break" | "continue" | "shift" | "getopts" | "hash" | "type"
            | "let" | "read" | "mapfile" | "readarray" | "printf" | "enable" | "fc"
            | "history" | "shopt" | "umask" | "ulimit" | "wait" | "jobs" | "fg" | "bg"
            | "disown" | "kill" | "[" | "[[" | "((" | "noglob" | "nocorrect" | "repeat"
            | "zmodload" | "autoload" | "setopt" | "unsetopt" | "emulate" | "rehash"
            | "whence" | "where" | "vared" | "zcompile" | "sched" | "print" | "integer"
            | "float" | "zle" | "r" | "-"
    )
}

/// Tokenize only literal words and simple command separators. `None` means
/// the script contains syntax whose shell interpretation is not modeled.
fn tokenize(script: &str) -> Option<Vec<Vec<String>>> {
    for c in script.chars() {
        if c.is_control() && !matches!(c, '\n') {
            return None;
        }
        if c.is_whitespace() && !matches!(c, ' ' | '\t' | '\n') {
            return None;
        }
    }

    let chars: Vec<char> = script.chars().collect();
    let (mut i, mut words, mut word, mut word_started) = (0, Vec::new(), String::new(), false);
    let mut segments = Vec::new();
    let mut last_separator: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        if !word_started {
            if let Some(token_len) = harmless_redirect_len(&chars, i) {
                // A redirect cannot stand in for the segment's command word.
                if words.is_empty() {
                    return None;
                }
                i += token_len;
                continue;
            }
        }
        match c {
            ' ' | '\t' => {
                if word_started {
                    words.push(std::mem::take(&mut word));
                    word_started = false;
                }
                i += 1;
            }
            '\n' | ';' | '|' | '&' => {
                if c == '&' && chars.get(i + 1) != Some(&'&') {
                    // Reject lone `&`; `&&` is one separator token.
                    return None;
                }
                if c == '|' && chars.get(i + 1) == Some(&'&') {
                    return None;
                }
                if c == ';' && chars.get(i + 1) == Some(&';') {
                    return None;
                }
                if word_started {
                    words.push(std::mem::take(&mut word));
                    word_started = false;
                }
                if words.is_empty() {
                    return None;
                }
                segments.push(std::mem::take(&mut words));
                last_separator = Some(c);
                i += if matches!(c, '&' | '|') && chars.get(i + 1) == Some(&c) {
                    2
                } else {
                    1
                };
                if i == chars.len() && !matches!(c, ';' | '\n') {
                    return None;
                }
            }
            '\'' => {
                word_started = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    return None;
                }
                last_separator = None;
            }
            '"' => {
                word_started = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    match chars[i] {
                        '"' => {
                            closed = true;
                            i += 1;
                            break;
                        }
                        '$' | '`' => return None,
                        '\\' => {
                            let next = *chars.get(i + 1)?;
                            if !matches!(next, '"' | '\\') {
                                return None;
                            }
                            word.push(next);
                            i += 2;
                        }
                        ch => {
                            word.push(ch);
                            i += 1;
                        }
                    }
                }
                if !closed {
                    return None;
                }
                last_separator = None;
            }
            '\\' => {
                let next = *chars.get(i + 1)?;
                if next == '\n' {
                    return None;
                }
                word.push(next);
                word_started = true;
                last_separator = None;
                i += 2;
            }
            '=' if word.is_empty() => return None,
            '$' | '`' | '(' | ')' | '{' | '}' | '<' | '>' | '!' | '#' | '~' | '^' => return None,
            _ => {
                word.push(c);
                word_started = true;
                last_separator = None;
                i += 1;
            }
        }
    }
    if word_started {
        words.push(word);
    }
    if !words.is_empty() {
        segments.push(words);
    } else if last_separator.is_none() || !matches!(last_separator, Some(';' | '\n')) {
        return None;
    }
    Some(segments)
}

/// Return the length of an exact accepted redirect token at a word boundary,
/// but only when its following character ends the token unambiguously.
fn harmless_redirect_len(chars: &[char], start: usize) -> Option<usize> {
    for token in ["2>&1", "2>/dev/null", ">/dev/null"] {
        let token_len = token.chars().count();
        if !token
            .chars()
            .enumerate()
            .all(|(offset, expected)| chars.get(start + offset) == Some(&expected))
        {
            continue;
        }
        let end = start + token_len;
        let has_valid_follower = match chars.get(end) {
            None => true,
            Some(c) if c.is_whitespace() || matches!(c, ';' | '|') => true,
            Some('&') => chars.get(end + 1) == Some(&'&'),
            _ => false,
        };
        if has_valid_follower {
            return Some(token_len);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{unwrap_shell_c, SimpleCommand, Wrapped};

    fn w(cmd: &str, args: &[&str]) -> Wrapped {
        unwrap_shell_c(cmd, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn sv(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).to_string()).collect()
    }

    fn one(command: &str, args: &[&str]) -> Wrapped {
        Wrapped::Commands(vec![SimpleCommand {
            command: command.into(),
            args: sv(args),
        }])
    }

    #[test]
    fn parses_literal_commands_and_quoting() {
        assert_eq!(w("bash", &["-c", "cargo test --locked"]), one("cargo", &["test", "--locked"]));
        assert_eq!(
            w("/bin/bash", &["-ec", "cargo test && git status"]),
            Wrapped::Commands(vec![
                SimpleCommand { command: "cargo".into(), args: sv(&["test"]) },
                SimpleCommand { command: "git".into(), args: sv(&["status"]) },
            ])
        );
        assert_eq!(
            w("zsh", &["-c", "grep -r 'a b' src | head -5"]),
            Wrapped::Commands(vec![
                SimpleCommand { command: "grep".into(), args: sv(&["-r", "a b", "src"]) },
                SimpleCommand { command: "head".into(), args: sv(&["-5"]) },
            ])
        );
        assert_eq!(w("dash", &["-c", "echo 'a;b'"]), one("echo", &["a;b"]));
        assert_eq!(
            w("bash", &["-c", "python3 -c \"print('x y')\""]),
            one("python3", &["-c", "print('x y')"])
        );
        assert_eq!(w("sh", &["-c", "echo 'a'\"b\"c ''"]), one("echo", &["abc", ""]));
        assert_eq!(w("bash", &["-c", "echo a\\ b"]), one("echo", &["a b"]));
        assert_eq!(w("bash", &["-c", "cargo test;"]), one("cargo", &["test"]));
        assert_eq!(w("bash", &["-c", "cargo test\n"]), one("cargo", &["test"]));
        assert_eq!(w("bash", &["-c", "echo \"a\\\\b\""]), one("echo", &["a\\b"]));
        assert_eq!(
            w("bash", &["-c", "ls -la | wc -l | cat"]),
            Wrapped::Commands(vec![
                SimpleCommand { command: "ls".into(), args: sv(&["-la"]) },
                SimpleCommand { command: "wc".into(), args: sv(&["-l"]) },
                SimpleCommand { command: "cat".into(), args: vec![] },
            ])
        );
        assert_eq!(w("/usr/bin/dash", &["-c", "true"]), one("true", &[]));
    }

    #[test]
    fn parses_unquoted_globs_as_argument_text() {
        for (script, args) in [
            ("ls *", &["*"][..]),
            ("ls src/*.rs", &["src/*.rs"][..]),
            ("ls ?", &["?"][..]),
            ("ls [a]", &["[a]"][..]),
            ("ls [a]*", &["[a]*"][..]),
            ("echo *", &["*"][..]),
        ] {
            assert_eq!(
                w("sh", &["-c", script]),
                one(script.split(' ').next().unwrap(), args),
                "{script:?}"
            );
        }
        assert_eq!(
            w("sh", &["-c", "wc -l src/*.rs src/provider/*.rs | sort -rn | head -40"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "wc".into(),
                    args: sv(&["-l", "src/*.rs", "src/provider/*.rs"]),
                },
                SimpleCommand {
                    command: "sort".into(),
                    args: sv(&["-rn"]),
                },
                SimpleCommand {
                    command: "head".into(),
                    args: sv(&["-40"]),
                },
            ])
        );

        let production_script =
            "head -30 src/provider/catalog.rs; echo ---; ls tests; echo ---; grep -rn 'usage\"' src/provider.rs | head -3; grep -rn 'include_usage' src/provider.rs";
        let parsed = w("sh", &["-c", production_script]);
        let Wrapped::Commands(commands) = parsed else {
            panic!("expected production read-only script to parse: {parsed:?}");
        };
        // The final `grep` after the second semicolon is also a command.
        assert_eq!(commands.len(), 7);
        assert_eq!(
            commands[1],
            SimpleCommand {
                command: "echo".into(),
                args: sv(&["---"]),
            }
        );
        assert_eq!(commands[4].command, "grep");
        assert!(commands[4].args.contains(&"usage\"".into()));
        assert_eq!(
            commands[5],
            SimpleCommand {
                command: "head".into(),
                args: sv(&["-3"]),
            }
        );
        assert_eq!(
            commands[6],
            SimpleCommand {
                command: "grep".into(),
                args: sv(&["-rn", "include_usage", "src/provider.rs"]),
            }
        );
    }

    #[test]
    fn parses_adjacent_command_separators() {
        let pair = |left: &str, right: &str| {
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: left.into(),
                    args: vec![],
                },
                SimpleCommand {
                    command: right.into(),
                    args: vec![],
                },
            ])
        };
        for script in ["ls|wc", "ls| wc", "ls |wc"] {
            assert_eq!(w("bash", &["-c", script]), pair("ls", "wc"), "{script:?}");
        }
        assert_eq!(w("bash", &["-c", "ls || wc"]), pair("ls", "wc"));
        assert_eq!(w("bash", &["-c", "a&&b"]), pair("a", "b"));
        assert_eq!(w("bash", &["-c", "ls & wc"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", "ls &"]), Wrapped::Unparseable);
    }

    #[test]
    fn parses_login_flags_and_harmless_redirect_tokens() {
        assert_eq!(
            w("bash", &["-lc", "cargo check --locked --lib 2>&1 | tail -30"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cargo".into(),
                    args: sv(&["check", "--locked", "--lib"]),
                },
                SimpleCommand {
                    command: "tail".into(),
                    args: sv(&["-30"]),
                },
            ])
        );
        assert_eq!(
            w(
                "sh",
                &["-lc", "ls x/*.rlib 2>/dev/null | head; ls y/*.rmeta 2>/dev/null | head"]
            ),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&["x/*.rlib"]),
                },
                SimpleCommand {
                    command: "head".into(),
                    args: vec![],
                },
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&["y/*.rmeta"]),
                },
                SimpleCommand {
                    command: "head".into(),
                    args: vec![],
                },
            ])
        );
        assert_eq!(w("bash", &["-c", "x >/dev/null"]), one("x", &[]));
        assert_eq!(
            w("bash", &["-c", "x 2>&1|tail -1"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "x".into(),
                    args: vec![],
                },
                SimpleCommand {
                    command: "tail".into(),
                    args: sv(&["-1"]),
                },
            ])
        );
        assert_eq!(w("bash", &["-c", "echo '2>&1'"]), one("echo", &["2>&1"]));
        assert_eq!(
            w("bash", &["-c", "echo \"2>/dev/null\""]),
            one("echo", &["2>/dev/null"])
        );
    }

    #[test]
    fn rejects_non_exact_or_misplaced_redirect_tokens() {
        for script in [
            "x2>&1",
            "x &>/dev/null",
            "x 1>&2",
            "x 2>f",
            "x > f",
            "2>&1 ls",
            "x 2>&1y",
        ] {
            assert_eq!(w("bash", &["-c", script]), Wrapped::Unparseable, "{script:?}");
        }
    }

    #[test]
    fn distinguishes_non_wrappers() {
        for (cmd, args) in [
            ("cargo", &["test"][..]),
            ("bash", &["script.sh"][..]),
            ("bash", &[][..]),
            ("bash", &["-n", "script.sh"][..]),
            ("./bash", &["-c", "ls"][..]),
            ("/tmp/y/sh", &["-c", "ls"][..]),
            ("python3", &["-c", "x"][..]),
        ] {
            assert_eq!(w(cmd, args), Wrapped::NotWrapper, "{cmd} {args:?}");
        }
    }

    #[test]
    fn rejects_ambiguous_scripts_and_invocations() {
        let scripts = [
            "cargo test $(whoami)", "cargo test `id`", "echo $HOME", "echo \"$HOME\"",
            "cargo test > out", "cargo test >> out", "cargo test < i", "cat <<EOF",
            "FOO=1 cargo test", "a+=b cmd", "a & b", "a &", "a;; b", "a |& b",
            "echo =x",
            "(cargo test)", "{ cargo test; }", "! cargo test", "~/x", "echo ~", "ls ^x", "=ls",
            "cd src && ls", "cd",
            "pushd x", "nocorrect rm x", "noglob ls", "eval x", "export A=b", "source x",
            "printf -v x y", "bash -c ls", "env ls", "echo 'a", "echo \"a", "echo a\\\n b",
            "echo a\\", "echo a\rb", "echo a\u{a0}b", "echo a\u{b}b", "echo \"a\\nb\"",
            "print x", "time ls", "[[ x ]]", "((1))",
        ];
        for script in scripts {
            assert_eq!(w("bash", &["-c", script]), Wrapped::Unparseable, "{script:?}");
        }
        assert_eq!(w("bash", &["-c", "* ls"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", "?.exe"]), Wrapped::Unparseable);
        for args in [
            vec!["-c", "cargo", "test"], vec!["-ic", "ls"],
            vec!["-c", ""], vec!["-c"], vec!["--norc", "-c", "ls"],
        ] {
            assert_eq!(w("bash", &args), Wrapped::Unparseable, "{args:?}");
        }
        // Login mode is allowed in a combined flag cluster; separate flags
        // remain unsupported because the wrapper shape is exactly two args.
        assert_eq!(w("bash", &["-lc", "ls"]), one("ls", &[]));
        assert_eq!(w("bash", &["-l", "-c", "ls"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-lc"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-ic", "ls"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", "test -f x"]), one("test", &["-f", "x"]));
        assert_eq!(w("bash", &["-c", "sh -c x"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", "bash -c ls"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", "echo a\\\nb"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", &format!("{}", "x".repeat(5000))]), Wrapped::Unparseable);
        let seventeen = (1..=17).map(|i| format!("a{i}")).collect::<Vec<_>>().join(" && ");
        assert_eq!(w("bash", &["-c", &seventeen]), Wrapped::Unparseable);
    }
}
