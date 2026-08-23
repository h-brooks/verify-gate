//! A tiny, deliberately limited glob matcher: enough for `**`, `*` and literal
//! segments in `ignore_paths` patterns. Not a general glob implementation —
//! see the README's "what it can't catch" section.
use regex::Regex;

pub fn glob_matches(pattern: &str, candidate: &str) -> bool {
    match compile(pattern) {
        Ok(re) => re.is_match(candidate),
        Err(_) => false,
    }
}

fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    // Single pass so glob wildcards we translate into regex syntax (e.g. the
    // '*' inside "[^/]*") never get re-interpreted as more glob wildcards.
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if chars.get(i + 2) == Some(&'/') {
                out.push_str("(?:.*/)?");
                i += 3;
            } else {
                out.push_str(".*");
                i += 2;
            }
        } else if chars[i] == '*' {
            out.push_str("[^/]*");
            i += 1;
        } else if chars[i] == '/' {
            out.push('/');
            i += 1;
        } else {
            out.push_str(&regex::escape(&chars[i].to_string()));
            i += 1;
        }
    }
    out.push('$');
    Regex::new(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_markdown_anywhere() {
        assert!(glob_matches("**/*.md", "readme.md"));
        assert!(glob_matches("**/*.md", "docs/readme.md"));
        assert!(glob_matches("**/*.md", "a/b/c.md"));
        assert!(!glob_matches("**/*.md", "readme.txt"));
    }

    #[test]
    fn matches_directory_wildcard() {
        assert!(glob_matches("**/.claude/**", ".claude/settings.json"));
        assert!(glob_matches("**/.claude/**", "sub/.claude/hooks/x.sh"));
        assert!(!glob_matches("**/.claude/**", "src/main.rs"));
    }

    #[test]
    fn matches_scratchpad() {
        assert!(glob_matches("**/scratchpad/**", "scratchpad/notes.txt"));
        assert!(glob_matches("**/scratchpad/**", "a/b/scratchpad/x/y.json"));
    }
}
