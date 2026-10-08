//! Minimal `.gitignore` support for the `glob`/`grep` workspace walk.
//!
//! Only the workspace-root `.gitignore` is honored; nested `.gitignore` files
//! are not read. The supported subset covers the rules that keep build output
//! and local state out of a scan:
//!
//! - blank lines and `#` comments are skipped, and trailing whitespace is
//!   trimmed;
//! - `!pattern` negates an earlier match, and the last matching rule wins;
//! - a trailing `/` restricts a rule to directories;
//! - a `/` anywhere but at the end anchors the rule to the workspace root
//!   (git's rule), and a leading `/` is stripped;
//! - any other pattern matches that name at any depth below the root;
//! - `*`, `?`, and `**` behave exactly as in the tools' glob matcher.
//!
//! Not supported: character classes, backslash escapes, and continuation
//! lines. An unsupported pattern simply fails to match, so a workspace using
//! them is walked more, never less.

use crate::tools::{glob_match, glob_match_from_root};
use std::path::Path;

struct Rule {
    anchored: bool,
    dir_only: bool,
    negated: bool,
    pattern: String,
}

/// Compiled rules from the workspace-root `.gitignore`.
#[derive(Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    /// Read `<workspace>/.gitignore`. A missing or unreadable file yields no
    /// rules, so a workspace without one is walked in full.
    pub fn load(workspace: &Path) -> Self {
        let text = std::fs::read_to_string(workspace.join(".gitignore")).unwrap_or_default();
        Self::parse(&text)
    }

    /// Compile `.gitignore` text. Line order is preserved because the last
    /// matching rule decides.
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for raw in text.lines() {
            let line = raw.trim_end();
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let negated = line.starts_with('!');
            let line = line.strip_prefix('!').unwrap_or(line);
            let dir_only = line.ends_with('/');
            let body = line.trim_end_matches('/');
            if body.is_empty() {
                continue;
            }
            let anchored = body.contains('/');
            let pattern = body.trim_start_matches('/').to_owned();
            if pattern.is_empty() {
                continue;
            }
            rules.push(Rule {
                anchored,
                dir_only,
                negated,
                pattern,
            });
        }
        Self { rules }
    }

    /// True when no rules were compiled, letting callers skip the work.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// True when the workspace-relative `path` (`/` separators, no leading
    /// slash) is ignored. `is_dir` gates rules written with a trailing `/`.
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let matched = if rule.anchored {
                glob_match_from_root(&rule.pattern, path)
            } else {
                glob_match(&rule.pattern, path)
            };
            if matched {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

#[cfg(test)]
mod tests {
    use super::IgnoreRules;

    #[test]
    fn anchored_rule_matches_only_at_root() {
        let rules = IgnoreRules::parse("/target/\n");
        assert!(rules.is_ignored("target", true));
        assert!(!rules.is_ignored("src/target", true));
        assert!(!rules.is_ignored("target", false));
    }

    #[test]
    fn bare_name_matches_at_any_depth() {
        let rules = IgnoreRules::parse(".diet_soda/\n");
        assert!(rules.is_ignored(".diet_soda", true));
        assert!(rules.is_ignored("a/b/.diet_soda", true));
        assert!(!rules.is_ignored("a/b/.diet_soda", false));
    }

    #[test]
    fn directory_only_rule_ignores_files() {
        let rules = IgnoreRules::parse("build/\n");
        assert!(rules.is_ignored("build", true));
        assert!(!rules.is_ignored("build", false));
    }

    #[test]
    fn negation_restores_a_previously_ignored_path() {
        let rules = IgnoreRules::parse(".env\n!.env.example\n");
        assert!(rules.is_ignored(".env", false));
        assert!(!rules.is_ignored(".env.example", false));
    }

    #[test]
    fn last_match_wins_in_file_order() {
        let rules = IgnoreRules::parse("!.env\n.env\n");
        assert!(rules.is_ignored(".env", false));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let rules = IgnoreRules::parse("# a comment\n\n   \n/target/\n");
        assert!(!rules.is_ignored("src", true));
        assert!(rules.is_ignored("target", true));
    }

    #[test]
    fn pattern_with_inner_slash_is_anchored() {
        let rules = IgnoreRules::parse("src/gen\n");
        assert!(rules.is_ignored("src/gen", true));
        assert!(rules.is_ignored("src/gen", false));
        assert!(!rules.is_ignored("other/src/gen", true));
    }

    #[test]
    fn glob_wildcards_work_in_rules() {
        let rules = IgnoreRules::parse("*.log\n**/tmp/\n");
        assert!(rules.is_ignored("a/b/x.log", false));
        assert!(rules.is_ignored("a/b/tmp", true));
        assert!(!rules.is_ignored("a/b/x.txt", false));
    }

    #[test]
    fn empty_text_yields_no_rules() {
        let rules = IgnoreRules::parse("");
        assert!(rules.is_empty());
        assert!(!rules.is_ignored("target", true));
    }
}
