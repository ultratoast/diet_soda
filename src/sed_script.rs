//! Conservative scanner for GNU `sed` arguments and scripts.
//!
//! Fail-closed contract: when anything about an invocation is ambiguous or
//! unrecognized, `may_execute` is `true` so the caller keeps requiring
//! approval. This models GNU sed. BSD/macOS sed differs (notably, `-i` takes
//! a separate suffix argument and there is no `e` command); the scanner keeps
//! the GNU model and treats deviations conservatively. A slash in an in-place
//! suffix also requires approval, closing the symlinked-parent path gap.

/// Outcome of scanning one `sed` argv.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SedScan {
    /// True when the invocation could execute a command (`e`, `s///e`,
    /// `-f` script files, unrecognized syntax).
    pub may_execute: bool,
    /// Filenames embedded in the script (`w`/`r`/`s///w` targets, `-i`
    /// suffixes containing `/`) that path checks must also consider.
    pub paths: Vec<String>,
    /// Positional input files (arguments after the script expression).
    pub files: Vec<String>,
    /// Slash-bearing `-i`/`--in-place` backup suffix, if any.
    pub backup_suffix: Option<String>,
}

/// Scan `sed` argv (without the program name).
pub fn scan_sed_args(args: &[String]) -> SedScan {
    let mut result = SedScan::default();
    let mut expressions = Vec::new();
    let mut positional = Vec::new();
    let mut backup_suffix = None;
    let mut i = 0;
    let mut terminated = false;

    while i < args.len() {
        let arg = &args[i];
        if terminated {
            i += 1;
            continue;
        }
        if arg == "--" {
            terminated = true;
            i += 1;
            continue;
        }
        if arg.starts_with("--") {
            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (arg.as_str(), None),
            };
            match name {
                "--quiet" | "--silent" | "--regexp-extended" | "--separate" | "--unbuffered"
                | "--null-data" | "--posix" | "--debug" | "--follow-symlinks" | "--binary"
                | "--sandbox" | "--help" | "--version" => {
                    if value.is_some() {
                        result.may_execute = true;
                    }
                }
                "--in-place" => {
                    if let Some(suffix) = value.filter(|suffix| suffix.contains('/')) {
                        result.may_execute = true;
                        backup_suffix = Some(suffix.to_owned());
                    }
                }
                "--expression" => {
                    let script = if let Some(value) = value {
                        Some(value.to_owned())
                    } else {
                        i += 1;
                        args.get(i).cloned()
                    };
                    if let Some(script) = script {
                        expressions.push(script);
                    } else {
                        result.may_execute = true;
                    }
                }
                "--line-length" => {
                    if value.is_none() {
                        i += 1;
                        if i >= args.len() {
                            result.may_execute = true;
                        }
                    }
                }
                "--file" => result.may_execute = true,
                _ => result.may_execute = true,
            }
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            let cluster = &arg[1..];
            let letters: Vec<char> = cluster.chars().collect();
            let mut j = 0;
            while j < letters.len() {
                match letters[j] {
                    'n' | 'E' | 'r' | 's' | 'u' | 'z' | 'b' => j += 1,
                    'i' => {
                        let suffix: String = letters[j + 1..].iter().collect();
                        if suffix.contains('/') {
                            result.may_execute = true;
                            backup_suffix = Some(suffix);
                        }
                        break;
                    }
                    'e' => {
                        if j + 1 < letters.len() {
                            expressions.push(letters[j + 1..].iter().collect());
                        } else {
                            i += 1;
                            if let Some(script) = args.get(i) {
                                expressions.push(script.clone());
                            } else {
                                result.may_execute = true;
                            }
                        }
                        break;
                    }
                    'l' => {
                        if j + 1 == letters.len() {
                            i += 1;
                            if i >= args.len() {
                                result.may_execute = true;
                            }
                        }
                        break;
                    }
                    'f' => {
                        result.may_execute = true;
                        break;
                    }
                    _ => {
                        result.may_execute = true;
                        break;
                    }
                }
            }
            i += 1;
            continue;
        }
        positional.push(arg.clone());
        i += 1;
    }

    let scripts = if expressions.is_empty() {
        let mut positional = positional.into_iter();
        let script = positional.next();
        result.files.extend(positional);
        script.into_iter().collect::<Vec<_>>()
    } else {
        result.files = positional;
        expressions
    };
    result.backup_suffix = backup_suffix;
    if scripts.is_empty() {
        result.may_execute = true;
    }
    for script in scripts {
        // GNU's historical matcher is not bracket-aware. When `[` appears
        // before its closing delimiter, also scan the bracket-aware reading;
        // either interpretation failing must remain fail-closed.
        for bracket_aware in [false, true] {
            let scan = ScriptParser::new(&script, bracket_aware).scan();
            result.may_execute |= scan.may_execute;
            for path in scan.paths {
                push_unique(&mut result.paths, path);
            }
        }
    }
    result
}

fn push_unique(paths: &mut Vec<String>, path: String) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

struct ScriptParser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    bracket_aware: bool,
    result: SedScan,
}

impl<'a> ScriptParser<'a> {
    fn new(input: &'a str, bracket_aware: bool) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            depth: 0,
            bracket_aware,
            result: SedScan::default(),
        }
    }

    fn scan(mut self) -> SedScan {
        while self.pos < self.bytes.len() && !self.result.may_execute {
            self.skip_horizontal();
            match self.peek() {
                None => break,
                Some(b';' | b'\n') => self.pos += 1,
                Some(b'}') => {
                    if self.depth == 0 {
                        self.result.may_execute = true;
                    } else {
                        self.depth -= 1;
                        self.pos += 1;
                    }
                }
                Some(b'#') => self.skip_line(),
                _ => self.parse_command(),
            }
        }
        if self.depth != 0 {
            self.result.may_execute = true;
        }
        self.result
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_horizontal(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.pos += 1;
        }
    }

    fn skip_line(&mut self) {
        while let Some(byte) = self.peek() {
            self.pos += 1;
            if byte == b'\n' {
                break;
            }
        }
    }

    fn parse_command(&mut self) {
        if self.parse_address_list().is_err() {
            self.result.may_execute = true;
            return;
        }
        self.skip_horizontal();
        let Some(command) = self.peek() else {
            self.result.may_execute = true;
            return;
        };
        self.pos += 1;
        let mut consumed_line = false;
        match command {
            b'e' => self.result.may_execute = true,
            b's' => self.parse_substitute(),
            b'y' => self.parse_transliterate(),
            b'a' | b'i' | b'c' => {
                self.skip_text_command();
                consumed_line = true;
            }
            b'w' | b'W' | b'r' | b'R' => {
                let filename = self.rest_of_line().trim().to_owned();
                if filename.is_empty() {
                    self.result.may_execute = true;
                } else {
                    push_unique(&mut self.result.paths, filename);
                }
            }
            b'b' | b't' | b'T' => {
                self.skip_to_command_end();
            }
            b':' => {
                if self.skip_to_command_end() {
                    self.result.may_execute = true;
                }
            }
            b'l' | b'L' | b'q' | b'Q' => self.parse_optional_number(),
            b'{' => {
                self.depth += 1;
                consumed_line = true;
            }
            b'=' | b'd' | b'D' | b'g' | b'G' | b'h' | b'H' | b'n' | b'N' | b'p' | b'P' | b'x'
            | b'z' | b'F' => {}
            _ => self.result.may_execute = true,
        }
        if !self.result.may_execute && !consumed_line {
            self.skip_horizontal();
            if !matches!(self.peek(), None | Some(b';' | b'\n' | b'}')) {
                self.result.may_execute = true;
            }
        }
    }

    fn parse_address_list(&mut self) -> Result<(), ()> {
        let save = self.pos;
        self.skip_horizontal();
        if !self.parse_address_atom()? {
            self.pos = save;
            return Ok(());
        }
        self.skip_horizontal();
        if self.peek() == Some(b',') {
            self.pos += 1;
            self.skip_horizontal();
            match self.peek() {
                Some(b'+') | Some(b'~') => {
                    self.pos += 1;
                    if !self.consume_digits() {
                        return Err(());
                    }
                }
                _ if !self.parse_address_atom()? => return Err(()),
                _ => {}
            }
            self.skip_horizontal();
        }
        if self.peek() == Some(b'!') {
            self.pos += 1;
        }
        Ok(())
    }

    fn parse_address_atom(&mut self) -> Result<bool, ()> {
        match self.peek() {
            Some(b'$') => {
                self.pos += 1;
                Ok(true)
            }
            Some(b'/') => {
                self.pos += 1;
                self.parse_address_regex(b'/')?;
                Ok(true)
            }
            Some(b'\\') if self.pos + 1 < self.bytes.len() => {
                let delimiter = self.bytes[self.pos + 1];
                if delimiter == b'\n' {
                    return Err(());
                }
                self.pos += 2;
                self.parse_address_regex(delimiter)?;
                Ok(true)
            }
            Some(byte) if byte.is_ascii_digit() => {
                self.consume_digits();
                if self.peek() == Some(b'~') {
                    self.pos += 1;
                    if !self.consume_digits() {
                        return Err(());
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn parse_address_regex(&mut self, delimiter: u8) -> Result<(), ()> {
        let end = scan_delimited(self.bytes, self.pos, delimiter, self.bracket_aware).ok_or(())?;
        self.pos = end;
        while matches!(self.peek(), Some(b'I' | b'M' | b'0'..=b'7')) {
            self.pos += 1;
        }
        if matches!(self.peek(), Some(b'\'')) {
            return Err(());
        }
        Ok(())
    }

    fn consume_digits(&mut self) -> bool {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos > start
    }

    fn parse_substitute(&mut self) {
        let Some(delimiter) = self.peek() else {
            self.result.may_execute = true;
            return;
        };
        if delimiter == b'\n' {
            self.result.may_execute = true;
            return;
        }
        self.pos += 1;
        let Some(pattern_end) = scan_delimited(self.bytes, self.pos, delimiter, self.bracket_aware)
        else {
            self.result.may_execute = true;
            return;
        };
        self.pos = pattern_end;
        let Some(replacement_end) =
            scan_delimited(self.bytes, self.pos, delimiter, self.bracket_aware)
        else {
            self.result.may_execute = true;
            return;
        };
        self.pos = replacement_end;
        loop {
            match self.peek() {
                Some(b'g' | b'n' | b'N' | b'p' | b'I' | b'i' | b'M' | b'F') => self.pos += 1,
                Some(b'0'..=b'9') => self.pos += 1,
                Some(b'e') => {
                    self.result.may_execute = true;
                    return;
                }
                Some(b'w') => {
                    self.pos += 1;
                    while matches!(self.peek(), Some(b' ' | b'\t')) {
                        self.pos += 1;
                    }
                    let filename = self.rest_of_line().trim().to_owned();
                    if filename.is_empty() {
                        self.result.may_execute = true;
                    } else {
                        push_unique(&mut self.result.paths, filename);
                    }
                    return;
                }
                _ => return,
            }
        }
    }

    fn parse_transliterate(&mut self) {
        let Some(delimiter) = self.peek() else {
            self.result.may_execute = true;
            return;
        };
        if delimiter == b'\n' {
            self.result.may_execute = true;
            return;
        }
        self.pos += 1;
        let Some(first_end) = scan_delimited(self.bytes, self.pos, delimiter, self.bracket_aware)
        else {
            self.result.may_execute = true;
            return;
        };
        let Some(second_end) = scan_delimited(self.bytes, first_end, delimiter, self.bracket_aware)
        else {
            self.result.may_execute = true;
            return;
        };
        self.pos = second_end;
    }

    fn skip_text_command(&mut self) {
        // The classic GNU form starts with `a\` (or `i\`, `c\`) as the
        // entire command line. Its following text lines may themselves be
        // continued with backslash-newline. In the one-line form, however,
        // a trailing backslash is ambiguous across GNU implementations, so
        // fail closed rather than joining the next line into command text.
        let classic = self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'\n');
        if classic {
            self.pos += 2;
        }
        loop {
            match self.peek() {
                None => return,
                Some(b'\\') if self.bytes.get(self.pos + 1) == Some(&b'\n') => {
                    if !classic {
                        self.result.may_execute = true;
                        self.skip_line();
                        return;
                    }
                    self.pos += 2;
                }
                Some(b'\n') => {
                    self.pos += 1;
                    return;
                }
                _ => self.pos += 1,
            }
        }
    }

    fn skip_to_command_end(&mut self) -> bool {
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b';' | b'\n' | b'}')) {
            self.pos += 1;
        }
        self.input[start..self.pos].trim().is_empty()
    }

    fn parse_optional_number(&mut self) {
        self.skip_horizontal();
        if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.consume_digits();
        }
    }

    fn rest_of_line(&mut self) -> &str {
        let start = self.pos;
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.pos += 1;
        }
        &self.input[start..self.pos]
    }
}

fn scan_delimited(bytes: &[u8], start: usize, delimiter: u8, bracket_aware: bool) -> Option<usize> {
    let mut pos = start;
    let mut in_bracket = false;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if bracket_aware && in_bracket {
            if byte == b']' {
                in_bracket = false;
            }
            pos += 1;
            continue;
        }
        if byte == b'\\' {
            pos += 1;
            if pos >= bytes.len() {
                return None;
            }
            pos += 1;
        } else if byte == delimiter {
            return Some(pos + 1);
        } else {
            if bracket_aware && byte == b'[' {
                in_bracket = true;
            }
            pos += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{scan_sed_args, SedScan};

    fn s(args: &[&str]) -> SedScan {
        scan_sed_args(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    fn safe(args: &[&str]) {
        assert!(
            !s(args).may_execute,
            "expected safe: {args:?}, got {:?}",
            s(args)
        );
    }

    fn unsafe_args(args: &[&str]) {
        assert!(
            s(args).may_execute,
            "expected unsafe: {args:?}, got {:?}",
            s(args)
        );
    }

    #[test]
    fn safe_scripts_and_options() {
        for args in [
            &["s/foo/bar/"][..],
            &["-n", "1,5p", "f"],
            &["-i", "s/a/b/g", "f"],
            &["-E", "s/(a|b)+/c/", "f"],
            &["/error/d", "f"],
            &["s/e/x/", "f"],
            &["-e", "s/a/b/", "-e", "3d", "f"],
            &["1,3{p;d}", "f"],
            &["-ne", "2p", "f"],
            &["s/x/y/w out.txt", "f"],
            &["r /etc/hosts", "f"],
            &["1~2d", "f"],
            &["0,/x/s/a/b/", "f"],
            &["/a/,+3d", "f"],
            &["y/abc/def/", "f"],
            &["s/a/b/gp", "f"],
            &["3q5", "f"],
            &["--expression=s/a/b/", "f"],
            &["--in-place", "s/a/b/", "f"],
        ] {
            safe(args);
        }
        assert_eq!(s(&["s/x/y/w out.txt", "f"]).paths, ["out.txt"]);
        assert_eq!(s(&["r /etc/hosts", "f"]).paths, ["/etc/hosts"]);
    }

    #[test]
    fn unsafe_scripts_and_options() {
        for args in [
            &["e"][..],
            &["1e echo hi", "f"],
            &["$!e", "f"],
            &["s/x/y/e", "f"],
            &["s/x/y/ge", "f"],
            &["-e", "s/x/y/e", "f"],
            &["p", "-e", "1e id"],
            &["--exp=1e id", "p"],
            &["--fi=s.sed", "f"],
            &["-ie", "1e id", "f"],
            &["-f", "s.sed", "f"],
            &["--file=s.sed", "f"],
            &["-nf", "s.sed", "f"],
            &["s/x/y", "f"],
            &["Z", "f"],
            &["-Q", "p"],
            &["-i.bak/x", "s/a/b/", "f"],
            &["s/[/]/x/", "f"],
            &[][..],
            &["-e"],
            &["s/a/b/q", "f"],
        ] {
            unsafe_args(args);
        }
        for script in [":x; e touch /tmp/pwned", ":x;e id", ":", "a foo\\\ne id"] {
            unsafe_args(&[script]);
        }
    }

    #[test]
    fn labels_and_text_command_continuations() {
        safe(&[":a;N;$!ba"]);
        safe(&["2{:a;p;b a}"]);
        safe(&["a\\\nfoo\n"]);
        safe(&["a foo\np"]);
        safe(&["a\\\nfoo\\\nbar\n"]);
    }

    #[test]
    fn collects_paths_and_checks_in_place_suffixes() {
        let scanned = s(&["s/x/y/w /etc/x", "f"]);
        assert!(!scanned.may_execute);
        assert_eq!(scanned.paths, ["/etc/x"]);

        let scanned = s(&["-i.sfx", "s/a/b/w out", "f"]);
        assert!(!scanned.may_execute);
        assert_eq!(scanned.paths, ["out"]);

        unsafe_args(&["-i.bak/x", "s/a/b/", "f"]);

        let scanned = s(&["-i.bak/x", "s/a/b/", "f"]);
        assert!(scanned.may_execute);
        assert_eq!(scanned.files, ["f"]);
        assert_eq!(scanned.backup_suffix.as_deref(), Some(".bak/x"));

        let scanned = s(&["-i", "s/a/b/", "f"]);
        assert_eq!(scanned.backup_suffix, None);
        assert_eq!(scanned.files, ["f"]);
    }

    #[test]
    fn comments_groups_and_permuted_options() {
        safe(&["# comment\n1p", "f"]);
        safe(&["#n\n1p", "f"]);
        safe(&["1,3{p;d}", "f"]);
        safe(&["p", "-e", "1d"]);
        unsafe_args(&["1{p", "f"]);
        unsafe_args(&["}p", "f"]);
    }
}
