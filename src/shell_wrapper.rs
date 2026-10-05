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
//! A leading literal `cd <dir>` chained with `&&` is modeled so callers can
//! account for the directory change. `CDPATH` remains a login-shell residual
//! for all four shells: a `-l` profile can export it, making bare `cd dir`
//! resolve to `$CDPATH/dir` instead of `./dir` (bash/sh/dash accept bare dirs).
//! The zsh `./`/`../`/`/` prefix rule mitigates its always-sourced `~/.zshenv`;
//! non-zsh shells rely on `env_clear` stripping `CDPATH` for non-login calls.
//! This sits alongside the `-l` profile residual above, where profile code can
//! also invalidate per-segment assumptions. Directory existence and workspace
//! containment are checked at approval time, with the usual TOCTOU risk.
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

/// The separator token that FOLLOWS a parsed segment. Internal to the parser:
/// used only to decide whether a `cd` is safely chainable. `End` marks the
/// last segment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sep {
    And,
    Or,
    Pipe,
    Semi,
    Newline,
    End,
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
        || flags[1..]
            .chars()
            .any(|c| !matches!(c, 'c' | 'e' | 'u' | 'l' | 'x'))
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
    let mut cd_chain_ok = true;
    for (mut words, sep) in segments {
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
        if command == "cd" {
            if !cd_chain_ok || sep != Sep::And || words.len() != 1 {
                return Wrapped::Unparseable;
            }
            let dir = &words[0];
            if dir.is_empty()
                || matches!(dir.chars().next(), Some('-' | '+' | '~'))
                || dir
                    .chars()
                    .any(|c| matches!(c, '$' | '`' | '*' | '?' | '['))
                || (shell == "zsh"
                    && !["./", "../", "/"]
                        .iter()
                        .any(|prefix| dir.starts_with(prefix)))
            {
                return Wrapped::Unparseable;
            }
            commands.push(SimpleCommand {
                command,
                args: words,
            });
            continue;
        }
        if is_builtin_or_keyword(&name) || is_nested_wrapper(&name) {
            return Wrapped::Unparseable;
        }
        commands.push(SimpleCommand {
            command,
            args: words,
        });
        cd_chain_ok = false;
    }
    Wrapped::Commands(commands)
}

/// Collapse chains of bare `env` launchers at the head of an argv.
///
/// Some launchers routinely emit `command` = "/usr/bin/env" with `args[0]`
/// also "/usr/bin/env". A bare `env` rewrites neither the argv nor the
/// environment, so the launcher is transparent and is collapsed: the first
/// non-environment token becomes the program. `env` with variable
/// assignments (`env FOO=bar prog`) or flags (`env -i prog`) is NOT
/// transparent and is left wrapped. Returns `None` when the program token was
/// never replaced, so the caller keeps the existing wrapper approval for the
/// original launcher.
pub fn unwrap_env_chain(command: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    let mut command = command.to_owned();
    let mut rest: Vec<String> = args.to_vec();
    let mut replaced = false;
    while basename_is_env(&command) {
        let Some(first) = rest.first().cloned() else {
            break;
        };
        if basename_is_env(&first) {
            rest.remove(0);
            continue;
        }
        // An empty token carries no program name; treat it like a flag so
        // ("", []) never becomes a replacement program.
        if first.is_empty() || first.starts_with('-') || first.contains('=') {
            break;
        }
        // The argv outside-path scan only inspects args, so a path-shaped
        // program token must not be moved out of it.
        #[cfg(windows)]
        let path_shaped = first.contains('/') || first.contains('\\');
        #[cfg(not(windows))]
        let path_shaped = first.contains('/');
        if path_shaped {
            break;
        }
        command = first;
        rest.remove(0);
        replaced = true;
    }
    replaced.then_some((command, rest))
}

fn basename_is_env(token: &str) -> bool {
    // Match `basename`'s split policy: '/' always, '\' only on Windows.
    #[cfg(windows)]
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    #[cfg(not(windows))]
    let name = token.rsplit('/').next().unwrap_or(token);
    name.eq_ignore_ascii_case("env") || name.eq_ignore_ascii_case("env.exe")
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
        "bash"
            | "sh"
            | "zsh"
            | "dash"
            | "ksh"
            | "fish"
            | "env"
            | "xargs"
            | "sudo"
            | "su"
            | "doas"
            | "nohup"
    )
}

/// This list must grow whenever a policy allow-rule is added for a name that
/// is also a shell builtin: such a command cannot safely be judged as an
/// external simple command here.
fn is_builtin_or_keyword(name: &str) -> bool {
    matches!(
        name,
        "cd" | "pushd"
            | "popd"
            | "dirs"
            | "eval"
            | "exec"
            | "source"
            | "."
            | ":"
            | "set"
            | "unset"
            | "export"
            | "readonly"
            | "declare"
            | "typeset"
            | "local"
            | "alias"
            | "unalias"
            | "trap"
            | "if"
            | "then"
            | "else"
            | "elif"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "in"
            | "function"
            | "select"
            | "coproc"
            | "time"
            | "command"
            | "builtin"
            | "exit"
            | "return"
            | "break"
            | "continue"
            | "shift"
            | "getopts"
            | "hash"
            | "type"
            | "let"
            | "read"
            | "mapfile"
            | "readarray"
            | "printf"
            | "enable"
            | "fc"
            | "history"
            | "shopt"
            | "umask"
            | "ulimit"
            | "wait"
            | "jobs"
            | "fg"
            | "bg"
            | "disown"
            | "kill"
            | "["
            | "[["
            | "(("
            | "noglob"
            | "nocorrect"
            | "repeat"
            | "zmodload"
            | "autoload"
            | "setopt"
            | "unsetopt"
            | "emulate"
            | "rehash"
            | "whence"
            | "where"
            | "vared"
            | "zcompile"
            | "sched"
            | "print"
            | "integer"
            | "float"
            | "zle"
            | "r"
            | "-"
    )
}

/// Tokenize only literal words and simple command separators. `None` means
/// the script contains syntax whose shell interpretation is not modeled.
fn tokenize(script: &str) -> Option<Vec<(Vec<String>, Sep)>> {
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
                let sep = match c {
                    '&' => Sep::And,
                    '|' if chars.get(i + 1) == Some(&'|') => Sep::Or,
                    '|' => Sep::Pipe,
                    ';' => Sep::Semi,
                    '\n' => Sep::Newline,
                    _ => unreachable!(),
                };
                segments.push((std::mem::take(&mut words), sep));
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
        segments.push((words, Sep::End));
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
    use super::{unwrap_env_chain, unwrap_shell_c, SimpleCommand, Wrapped};

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
        assert_eq!(
            w("bash", &["-c", "cargo test --locked"]),
            one("cargo", &["test", "--locked"])
        );
        assert_eq!(
            w("/bin/bash", &["-ec", "cargo test && git status"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cargo".into(),
                    args: sv(&["test"])
                },
                SimpleCommand {
                    command: "git".into(),
                    args: sv(&["status"])
                },
            ])
        );
        assert_eq!(
            w("zsh", &["-c", "grep -r 'a b' src | head -5"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "grep".into(),
                    args: sv(&["-r", "a b", "src"])
                },
                SimpleCommand {
                    command: "head".into(),
                    args: sv(&["-5"])
                },
            ])
        );
        assert_eq!(w("dash", &["-c", "echo 'a;b'"]), one("echo", &["a;b"]));
        assert_eq!(
            w("bash", &["-c", "python3 -c \"print('x y')\""]),
            one("python3", &["-c", "print('x y')"])
        );
        assert_eq!(
            w("sh", &["-c", "echo 'a'\"b\"c ''"]),
            one("echo", &["abc", ""])
        );
        assert_eq!(w("bash", &["-c", "echo a\\ b"]), one("echo", &["a b"]));
        assert_eq!(w("bash", &["-c", "cargo test;"]), one("cargo", &["test"]));
        assert_eq!(w("bash", &["-c", "cargo test\n"]), one("cargo", &["test"]));
        assert_eq!(
            w("bash", &["-c", "echo \"a\\\\b\""]),
            one("echo", &["a\\b"])
        );
        assert_eq!(
            w("bash", &["-c", "ls -la | wc -l | cat"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&["-la"])
                },
                SimpleCommand {
                    command: "wc".into(),
                    args: sv(&["-l"])
                },
                SimpleCommand {
                    command: "cat".into(),
                    args: vec![]
                },
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
            w(
                "sh",
                &[
                    "-c",
                    "wc -l src/*.rs src/provider/*.rs | sort -rn | head -40"
                ]
            ),
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
    fn parses_leading_cd_chain() {
        assert_eq!(
            w("bash", &["-c", "cd src && ls"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cd".into(),
                    args: sv(&["src"]),
                },
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&[]),
                },
            ])
        );
        assert_eq!(
            w("bash", &["-c", "cd ./a && cd b && ls -l"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cd".into(),
                    args: sv(&["./a"]),
                },
                SimpleCommand {
                    command: "cd".into(),
                    args: sv(&["b"]),
                },
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&["-l"]),
                },
            ])
        );
        assert_eq!(
            w("/bin/sh", &["-c", "cd /tmp && pwd"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cd".into(),
                    args: sv(&["/tmp"]),
                },
                SimpleCommand {
                    command: "pwd".into(),
                    args: sv(&[]),
                },
            ])
        );
        assert_eq!(
            w("bash", &["-c", "cd 'my dir' && ls"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cd".into(),
                    args: sv(&["my dir"]),
                },
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&[]),
                },
            ])
        );
    }

    #[test]
    fn rejects_unsafe_cd() {
        for script in [
            "cd",
            "cd src",
            "cd src && cd",
            "cd src | ls",
            "cd src || ls",
            "cd src ; ls",
            "ls && cd src",
            "cd -",
            "cd +2",
            "cd ~",
            "cd $HOME",
            "cd a b",
            "cd *",
            "cd src &&",
            "cd ''",
            "cd src\nls",
        ] {
            assert_eq!(
                w("bash", &["-c", script]),
                Wrapped::Unparseable,
                "{script:?}"
            );
        }
        assert_eq!(w("zsh", &["-c", "cd src && ls"]), Wrapped::Unparseable);
        assert_eq!(
            w("zsh", &["-c", "cd ./src && ls"]),
            Wrapped::Commands(vec![
                SimpleCommand {
                    command: "cd".into(),
                    args: sv(&["./src"]),
                },
                SimpleCommand {
                    command: "ls".into(),
                    args: sv(&[]),
                },
            ])
        );
    }

    #[test]
    fn parses_login_flags_and_harmless_redirect_tokens() {
        assert_eq!(
            w(
                "bash",
                &["-lc", "cargo check --locked --lib 2>&1 | tail -30"]
            ),
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
                &[
                    "-lc",
                    "ls x/*.rlib 2>/dev/null | head; ls y/*.rmeta 2>/dev/null | head"
                ]
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
            assert_eq!(
                w("bash", &["-c", script]),
                Wrapped::Unparseable,
                "{script:?}"
            );
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
            "cargo test $(whoami)",
            "cargo test `id`",
            "echo $HOME",
            "echo \"$HOME\"",
            "cargo test > out",
            "cargo test >> out",
            "cargo test < i",
            "cat <<EOF",
            "FOO=1 cargo test",
            "a+=b cmd",
            "a & b",
            "a &",
            "a;; b",
            "a |& b",
            "echo =x",
            "(cargo test)",
            "{ cargo test; }",
            "! cargo test",
            "~/x",
            "echo ~",
            "ls ^x",
            "=ls",
            "cd",
            "pushd x",
            "nocorrect rm x",
            "noglob ls",
            "eval x",
            "export A=b",
            "source x",
            "printf -v x y",
            "bash -c ls",
            "env ls",
            "echo 'a",
            "echo \"a",
            "echo a\\\n b",
            "echo a\\",
            "echo a\rb",
            "echo a\u{a0}b",
            "echo a\u{b}b",
            "echo \"a\\nb\"",
            "print x",
            "time ls",
            "[[ x ]]",
            "((1))",
        ];
        for script in scripts {
            assert_eq!(
                w("bash", &["-c", script]),
                Wrapped::Unparseable,
                "{script:?}"
            );
        }
        assert_eq!(w("bash", &["-c", "* ls"]), Wrapped::Unparseable);
        assert_eq!(w("bash", &["-c", "?.exe"]), Wrapped::Unparseable);
        for args in [
            vec!["-c", "cargo", "test"],
            vec!["-ic", "ls"],
            vec!["-c", ""],
            vec!["-c"],
            vec!["--norc", "-c", "ls"],
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
        assert_eq!(
            w("bash", &["-c", &format!("{}", "x".repeat(5000))]),
            Wrapped::Unparseable
        );
        let seventeen = (1..=17)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(" && ");
        assert_eq!(w("bash", &["-c", &seventeen]), Wrapped::Unparseable);
    }

    #[test]
    fn unwrap_env_chain_collapses_duplicated_launchers() {
        assert_eq!(
            unwrap_env_chain(
                "/usr/bin/env",
                &sv(&["/usr/bin/env", "sed", "-n", "1,2p", "f.txt"])
            ),
            Some(("sed".to_string(), sv(&["-n", "1,2p", "f.txt"])))
        );
    }

    #[test]
    fn unwrap_env_chain_unwraps_single_launcher() {
        assert_eq!(
            unwrap_env_chain("env", &sv(&["python3", "-c", "print(1)"])),
            Some(("python3".to_string(), sv(&["-c", "print(1)"])))
        );
    }

    #[test]
    fn unwrap_env_chain_keeps_variable_assignments_wrapped() {
        assert_eq!(unwrap_env_chain("env", &sv(&["FOO=1", "python3"])), None);
    }

    #[test]
    fn unwrap_env_chain_keeps_flags_wrapped() {
        assert_eq!(
            unwrap_env_chain("/usr/bin/env", &sv(&["-i", "python3"])),
            None
        );
    }

    #[test]
    fn unwrap_env_chain_ignores_other_programs() {
        assert_eq!(unwrap_env_chain("sed", &sv(&["-n", "1,2p", "f.txt"])), None);
    }

    #[test]
    fn unwrap_env_chain_without_a_program_is_unchanged() {
        assert_eq!(unwrap_env_chain("/usr/bin/env", &sv(&[])), None);
        assert_eq!(unwrap_env_chain("/usr/bin/env", &sv(&["env"])), None);
    }

    #[test]
    fn unwrap_env_chain_collapses_repeated_prefixes() {
        assert_eq!(
            unwrap_env_chain("env", &sv(&["/usr/bin/env", "env", "cargo", "test"])),
            Some(("cargo".to_string(), sv(&["test"])))
        );
    }

    #[test]
    fn unwrap_env_chain_keeps_path_shaped_programs_wrapped() {
        // The program token must stay in argv: the outside-path scan only
        // inspects args, never the replaced program.
        assert_eq!(
            unwrap_env_chain("/usr/bin/env", &sv(&["/opt/homebrew/bin/ls"])),
            None
        );
        assert_eq!(
            unwrap_env_chain(
                "/usr/bin/env",
                &sv(&["/usr/bin/env", "/opt/homebrew/bin/ls"])
            ),
            None
        );
    }

    #[test]
    fn unwrap_env_chain_keeps_empty_token_wrapped() {
        assert_eq!(unwrap_env_chain("/usr/bin/env", &sv(&[""])), None);
    }

    #[test]
    fn unwrap_env_chain_keeps_flag_forms_wrapped() {
        // `--` is a flag, and an env option's operand must not be mistaken
        // for the program.
        assert_eq!(unwrap_env_chain("/usr/bin/env", &sv(&["--", "sed"])), None);
        assert_eq!(
            unwrap_env_chain("/usr/bin/env", &sv(&["-u", "FOO", "sed"])),
            None
        );
        assert_eq!(
            unwrap_env_chain("/usr/bin/env", &sv(&["-C", "/tmp", "sed"])),
            None
        );
    }
}
