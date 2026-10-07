//! Conservative scanner for `awk`-family arguments and program text.
//!
//! Fail-closed contract: when anything about an invocation is ambiguous or
//! unrecognized, the dangerous flags are set so the caller keeps requiring
//! approval. Two independent flags model the two hazards: [`AwkScan::may_execute`]
//! for running shell commands (`system()`, pipes to commands, gawk
//! `@include`/`@load`, and program sources that cannot be inspected) and
//! [`AwkScan::may_write`] for output redirection (`>`/`>>`) so a read-only
//! policy can deny writes even when the program is otherwise inert.

/// Outcome of scanning one `awk` argv.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AwkScan {
    /// The program may run a shell command: `system(...)`, a pipe to a
    /// command (`print | "cmd"`, `"cmd" | getline`, `|&`), or an unscannable
    /// program source.
    pub may_execute: bool,
    /// The program may write to a file or other destination: `print`/`printf`
    /// output redirected with `>` or `>>`.
    pub may_write: bool,
}

/// Scan the arguments of an awk-family invocation (`awk`, `gawk`, `mawk`,
/// `nawk`, `original-awk`). Conservative: when the program cannot be
/// inspected, or an option is not recognized, the dangerous flags are set so
/// the caller keeps requiring approval.
pub fn scan_awk_args(args: &[String]) -> AwkScan {
    let mut result = AwkScan::default();
    let mut i = 0;
    let mut terminated = false;

    // Option walk: the first non-option argument (after any `--`) is the
    // program text. Everything after it is an input file (a read) and is
    // deliberately not scanned.
    let program = loop {
        let Some(arg) = args.get(i) else {
            break None;
        };
        if terminated || !arg.starts_with('-') {
            break Some(arg.as_str());
        }
        if arg == "--" {
            terminated = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            match name {
                "field-separator" | "assign" => {
                    i += if value.is_some() { 1 } else { 2 };
                }
                // `--file`, `--source`, and anything else unrecognized: the
                // program comes from elsewhere (or cannot be classified), so
                // it cannot be inspected.
                _ => {
                    result.may_execute = true;
                    return result;
                }
            }
            continue;
        }
        if arg == "-F" || arg == "-v" || arg == "-W" {
            i += 2;
            continue;
        }
        if arg.starts_with("-F") || arg.starts_with("-v") || arg.starts_with("-W") {
            i += 1;
            continue;
        }
        if arg.starts_with("-f") || arg.starts_with("-e") {
            // The program text lives in a file (or a separate `-e`/`--file`
            // argument), so it cannot be inspected.
            result.may_execute = true;
            return result;
        }
        // Unrecognized option: fail closed.
        result.may_execute = true;
        return result;
    };
    let Some(program) = program else {
        // No program text at all: a malformed invocation the caller rejects.
        return result;
    };

    if contains_system_call(program) || contains_lone_pipe(program) || program.contains('@') {
        result.may_execute = true;
    }
    if has_output_redirect(program) {
        result.may_write = true;
    }
    result
}

/// `system` followed by optional whitespace and `(`.
fn contains_system_call(program: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = program[from..].find("system") {
        let rest = &program[from + offset + "system".len()..];
        if rest.trim_start().starts_with('(') {
            return true;
        }
        from += offset + "system".len();
    }
    false
}

/// A `|` that is not part of `||` (awk's logical OR, including the `||=`
/// assignment operator). `|&` contains such a `|` and is also flagged.
fn contains_lone_pipe(program: &str) -> bool {
    let bytes = program.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'|' {
            if bytes.get(i + 1) == Some(&b'|') {
                i += 2;
                continue;
            }
            return true;
        }
        i += 1;
    }
    false
}

/// Split the program into statements at `;`, `\n`, `{`, and `}`; a statement
/// whose text after a `print`/`printf` keyword holds a redirecting `>` writes
/// a file. Comparisons before the keyword (`$3 > 100 {print $1}`) do not.
fn has_output_redirect(program: &str) -> bool {
    program
        .split(|c| c == ';' || c == '\n' || c == '{' || c == '}')
        .any(|statement| {
            statement
                .find("print")
                .is_some_and(|keyword| redirect_gt(&statement[keyword + "print".len()..]))
        })
}

/// A `>` that is not part of `>=`, `->`, or `>>=`.
fn redirect_gt(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'>' {
            if bytes[i..].starts_with(b">>=") {
                i += 3;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'=') {
                i += 2;
                continue;
            }
            if i > 0 && bytes[i - 1] == b'-' {
                i += 1;
                continue;
            }
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{scan_awk_args, AwkScan};

    fn s(args: &[&str]) -> AwkScan {
        scan_awk_args(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    fn assert_flags(args: &[&str], may_execute: bool, may_write: bool, what: &str) {
        let scan = s(args);
        assert_eq!(
            scan.may_execute, may_execute,
            "{what}: expected may_execute={may_execute} for {args:?}, got {scan:?}"
        );
        assert_eq!(
            scan.may_write, may_write,
            "{what}: expected may_write={may_write} for {args:?}, got {scan:?}"
        );
    }

    #[test]
    fn read_only_prints_are_safe() {
        assert_flags(&["{print $1}"], false, false, "plain print");
        assert_flags(
            &["$3 > 100 {print $1}", "f"],
            false,
            false,
            "comparison precedes print",
        );
        assert_flags(
            &["$1==\"a\" || $2==\"b\"", "f"],
            false,
            false,
            "logical or is not a pipe",
        );
        assert_flags(
            &["{printf \"%s\", $1}"],
            false,
            false,
            "printf without redirect",
        );
    }

    #[test]
    fn redirected_output_marks_writes_only() {
        assert_flags(
            &["{print $1 > \"out\"}"],
            false,
            true,
            "print redirect marks write",
        );
        assert_flags(
            &["{printf \"%s\\n\", $1 >> \"log\"}"],
            false,
            true,
            "printf append redirect marks write",
        );
    }

    #[test]
    fn command_execution_marks_execute_only() {
        assert_flags(&["BEGIN{system(\"ls\")}", "f"], true, false, "system()");
        assert_flags(&["{print | \"sort\"}", "f"], true, false, "print pipe");
        assert_flags(&["\"ls\" | getline x", "f"], true, false, "getline pipe");
        assert_flags(
            &["{cmd |& \"tee log\"}", "f"],
            true,
            false,
            "coprocess pipe",
        );
        assert_flags(
            &["@include \"extra.awk\"", "f"],
            true,
            false,
            "gawk directive",
        );
    }

    #[test]
    fn recognized_options_walk_to_the_program() {
        assert_flags(&["-F:", "{print $1}", "f"], false, false, "attached -F");
        assert_flags(&["-F", ":", "{print $1}", "f"], false, false, "separate -F");
        assert_flags(
            &["--field-separator=:", "{print $1}", "f"],
            false,
            false,
            "long field separator with =",
        );
        assert_flags(
            &["--field-separator", ":", "{print $1}", "f"],
            false,
            false,
            "long field separator with value",
        );
        assert_flags(
            &["-v", "x=1", "{print x}", "f"],
            false,
            false,
            "separate -v",
        );
        assert_flags(&["-vx=1", "{print x}", "f"], false, false, "attached -v");
        assert_flags(
            &["--assign=x=1", "{print x}", "f"],
            false,
            false,
            "long assign",
        );
        assert_flags(&["-W", "v", "{print $1}", "f"], false, false, "legacy -W");
        assert_flags(
            &["--", "{print $1}", "f"],
            false,
            false,
            "option terminator",
        );
    }

    #[test]
    fn uninspectable_or_unknown_options_fail_closed() {
        assert_flags(&["-f", "prog.awk", "f"], true, false, "-f program file");
        assert_flags(&["-fprog.awk", "f"], true, false, "attached -f");
        assert_flags(&["--file", "prog.awk", "f"], true, false, "--file");
        assert_flags(&["--source", "{print}", "f"], true, false, "--source");
        assert_flags(&["-e", "{print}", "f"], true, false, "-e");
        assert_flags(&["-Z", "{print}", "f"], true, false, "unknown short option");
        assert_flags(
            &["--posix", "{print}", "f"],
            true,
            false,
            "unknown long option",
        );
    }

    #[test]
    fn missing_program_defaults_to_harmless() {
        assert_flags(&[], false, false, "no arguments");
        assert_flags(&["-F", ":"], false, false, "options without program");
        assert_flags(&["-W"], false, false, "dangling option value");
    }
}
