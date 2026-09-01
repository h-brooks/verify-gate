use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Rule configuration for verify-gate. Every field has a built-in default;
/// a config file only needs to set the fields it wants to override.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub edit_tools: Vec<String>,
    pub edit_command_patterns: Vec<String>,
    pub ignore_paths: Vec<String>,
    pub verify_patterns: Vec<String>,
    pub verify_tools: Vec<String>,
    pub min_edits: usize,
}

/// Mirrors `Config` but every field is optional, for partial TOML overrides.
#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    edit_tools: Option<Vec<String>>,
    edit_command_patterns: Option<Vec<String>>,
    ignore_paths: Option<Vec<String>>,
    verify_patterns: Option<Vec<String>>,
    verify_tools: Option<Vec<String>>,
    min_edits: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            edit_tools: vec![
                "Edit".to_string(),
                "Write".to_string(),
                "MultiEdit".to_string(),
                "NotebookEdit".to_string(),
            ],
            edit_command_patterns: vec![
                r"\bsed -i".to_string(),
                r"\btee ".to_string(),
                r"\bcat >".to_string(),
                r"\bgit apply".to_string(),
                r"\bpatch ".to_string(),
                // A plain stdout/fd-1 redirect to a file: `> file` or `>> file`,
                // requiring whitespace right before the `>` so this doesn't fire
                // on `2>/dev/null` (digit immediately before `>`), and requiring
                // a non-`&` target so `>&2`/`1>&2` duplications don't count.
                r"(^|\s)1?>{1,2}\s*[^&\s]".to_string(),
            ],
            ignore_paths: vec![
                "**/*.md".to_string(),
                "**/.claude/**".to_string(),
                "**/memory/**".to_string(),
                "**/scratchpad/**".to_string(),
            ],
            verify_patterns: vec![
                r"cargo (test|build|check|clippy|run)".to_string(),
                r"(npm|pnpm|yarn|bun) (test|run|exec)".to_string(),
                r"npx (vitest|jest|playwright|tsc)".to_string(),
                r"pytest".to_string(),
                r"python -m (pytest|unittest)".to_string(),
                r"go (test|build)".to_string(),
                r"\bmake\b( test| check)?".to_string(),
                r"curl ".to_string(),
                r"\bhttp ".to_string(),
                r"playwright".to_string(),
                r"\btsc\b".to_string(),
                r"eslint".to_string(),
                r"ruff".to_string(),
                r"mypy".to_string(),
                r"(?i)screenshot".to_string(),
            ],
            verify_tools: vec![r"^mcp__(playwright|claude-in-chrome)".to_string()],
            min_edits: 1,
        }
    }
}

impl Config {
    /// Loads config from `<cwd>/.verify-gate.toml`, falling back to
    /// `~/.config/verify-gate/config.toml`, falling back to built-in defaults.
    /// Only the first file found is read; fields it omits fall back to defaults.
    pub fn load(cwd: Option<&Path>) -> Self {
        let cwd_path = cwd
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let candidates = [
            Some(cwd_path.join(".verify-gate.toml")),
            global_config_path(),
        ];
        for candidate in candidates.into_iter().flatten() {
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                match toml::from_str::<PartialConfig>(&text) {
                    Ok(partial) => return Self::default().merge(partial),
                    Err(e) => {
                        eprintln!(
                            "verify-gate: failed to parse config {}: {e}, using defaults",
                            candidate.display()
                        );
                        return Self::default();
                    }
                }
            }
        }
        Self::default()
    }

    fn merge(mut self, partial: PartialConfig) -> Self {
        if let Some(v) = partial.edit_tools {
            self.edit_tools = v;
        }
        if let Some(v) = partial.edit_command_patterns {
            self.edit_command_patterns = v;
        }
        if let Some(v) = partial.ignore_paths {
            self.ignore_paths = v;
        }
        if let Some(v) = partial.verify_patterns {
            self.verify_patterns = v;
        }
        if let Some(v) = partial.verify_tools {
            self.verify_tools = v;
        }
        if let Some(v) = partial.min_edits {
            self.min_edits = v;
        }
        self
    }
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("verify-gate")
            .join("config.toml")
    })
}

/// The template written by `verify-gate init`. Kept in sync with `Config::default()`
/// by a test that parses it back and compares.
pub const INIT_TEMPLATE: &str = r#"# verify-gate configuration
# Any field left out falls back to the built-in default shown here.

# Tool names that always count as an edit.
edit_tools = ["Edit", "Write", "MultiEdit", "NotebookEdit"]

# Bash commands matching any of these patterns also count as an edit.
edit_command_patterns = [
    "\\bsed -i",
    "\\btee ",
    "\\bcat >",
    "\\bgit apply",
    "\\bpatch ",
    "(^|\\s)1?>{1,2}\\s*[^&\\s]",
]

# Edits to paths matching any of these globs never require verification.
ignore_paths = ["**/*.md", "**/.claude/**", "**/memory/**", "**/scratchpad/**"]

# Bash commands matching any of these patterns count as verification.
verify_patterns = [
    "cargo (test|build|check|clippy|run)",
    "(npm|pnpm|yarn|bun) (test|run|exec)",
    "npx (vitest|jest|playwright|tsc)",
    "pytest",
    "python -m (pytest|unittest)",
    "go (test|build)",
    "\\bmake\\b( test| check)?",
    "curl ",
    "\\bhttp ",
    "playwright",
    "\\btsc\\b",
    "eslint",
    "ruff",
    "mypy",
    "(?i)screenshot",
]

# Tool names matching any of these patterns count as verification
# (e.g. browser-automation MCP tools).
verify_tools = ["^mcp__(playwright|claude-in-chrome)"]

# Minimum number of qualifying edits before the gate engages at all.
min_edits = 1
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_template_matches_defaults() {
        let partial: PartialConfig = toml::from_str(INIT_TEMPLATE).expect("template parses");
        let from_template = Config::default().merge(partial);
        assert_eq!(from_template, Config::default());
    }

    fn verify_re() -> regex::RegexSet {
        regex::RegexSet::new(&Config::default().verify_patterns).expect("default patterns compile")
    }

    #[test]
    fn bare_tsc_pattern_does_not_match_inside_a_filename() {
        // "tsc" is a literal substring of "tsconfig.json"; just reading that
        // file is not running the TypeScript compiler.
        let cmd = "cat tsconfig.json";
        assert!(!verify_re().is_match(cmd));
    }

    #[test]
    fn tsc_still_matches_a_standalone_invocation() {
        let cmd = "tsc --noEmit";
        assert!(verify_re().is_match(cmd));
    }

    #[test]
    fn bare_make_pattern_does_not_match_makefile_ish_prose() {
        let cmd = "echo 'update the Makefile to remake the docs bundle'";
        assert!(!verify_re().is_match(cmd));
    }

    #[test]
    fn make_still_matches_plain_and_test_and_check_invocations() {
        assert!(verify_re().is_match("make"));
        assert!(verify_re().is_match("make test"));
        assert!(verify_re().is_match("make check"));
    }
}
