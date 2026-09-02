use crate::config::Config;
use crate::glob::glob_matches;
use anyhow::{Context, Result};
use regex::{Regex, RegexSet};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct EditEvent {
    pub line: usize,
    pub tool_name: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerifyEvent {
    pub line: usize,
    pub tool_name: String,
    pub is_error: bool,
    pub result_snippet: String,
    /// Whether a matching `tool_result` was ever seen for this verification's
    /// `tool_use`. A verification whose result never arrives (the call was
    /// interrupted, rejected, or simply never completed before the transcript
    /// ends) must not count as a passing verification.
    pub resolved: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Block { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub last_edit: Option<EditEvent>,
    /// Files touched by qualifying edits since the last verification (only
    /// populated on the "no verification after the edit" path).
    pub files_since_verification: Vec<String>,
    pub verifications_after_last_edit: Vec<VerifyEvent>,
    pub decision: Decision,
}

impl Report {
    pub fn is_block(&self) -> bool {
        matches!(self.decision, Decision::Block { .. })
    }
}

pub fn evaluate_transcript(path: &Path, config: &Config) -> Result<Report> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening transcript {}", path.display()))?;
    evaluate_reader(BufReader::new(file), config)
}

pub fn evaluate_reader<R: Read>(reader: BufReader<R>, config: &Config) -> Result<Report> {
    let edit_tool_set: std::collections::HashSet<&str> =
        config.edit_tools.iter().map(|s| s.as_str()).collect();
    let edit_cmd_re = compile_pattern_set(
        &config.edit_command_patterns,
        "edit_command_patterns",
        &Config::default().edit_command_patterns,
    );
    let verify_cmd_re = compile_pattern_set(
        &config.verify_patterns,
        "verify_patterns",
        &Config::default().verify_patterns,
    );
    let verify_tool_re = compile_pattern_set(
        &config.verify_tools,
        "verify_tools",
        &Config::default().verify_tools,
    );
    let denial_re = compile_pattern_set(
        &config.denial_patterns,
        "denial_patterns",
        &Config::default().denial_patterns,
    );

    let mut edits: Vec<EditEvent> = Vec::new();
    let mut verifies: Vec<VerifyEvent> = Vec::new();
    // tool_use_id -> index into `verifies`, so a later tool_result can fill in is_error.
    let mut pending_verify: HashMap<String, usize> = HashMap::new();
    // Cap how many malformed-line warnings we print: a corrupt or truncated
    // transcript can contain hundreds of thousands of bad lines, and printing
    // one line per bad line can dump tens of MB into the harness's stderr pipe.
    const MAX_MALFORMED_LINE_WARNINGS: usize = 20;
    let mut malformed_lines = 0usize;

    for (idx, line_result) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = match line_result {
            Ok(l) => l,
            // A sticky I/O error (e.g. transcript_path is a directory, or a
            // failing volume) makes every subsequent read fail the same way
            // without ever consuming input, so `lines()` would never return
            // `None` if we kept looping. Bail out instead: the caller
            // (`hook`/`check`) already treats an `Err` here as an internal
            // error and fails open, which is the correct outcome for an
            // unreadable transcript.
            Err(e) => {
                return Err(e).context(format!("reading transcript at line {line_no}"));
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                malformed_lines += 1;
                if malformed_lines <= MAX_MALFORMED_LINE_WARNINGS {
                    eprintln!("verify-gate: skipping malformed line {line_no}: {e}");
                }
                continue;
            }
        };
        if record
            .get("isSidechain")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue; // subagent record: not the main session this Stop hook fires for
        }
        let rec_type = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let Some(content) = record
            .pointer("/message/content")
            .and_then(|v| v.as_array())
        else {
            continue;
        };

        match rec_type {
            "assistant" => {
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                        continue;
                    }
                    let tool_name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tool_id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);

                    let mut is_edit = edit_tool_set.contains(tool_name.as_str());
                    let mut is_verify = verify_tool_re.is_match(&tool_name);
                    let mut files = extract_files(&input);

                    if tool_name == "Bash" {
                        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        // Edit detection runs on the shell-SYNTAX view of the command with
                        // heredoc bodies stripped first: a heredoc body is data written to a
                        // file or piped to a program, not shell syntax, so its contents must
                        // never be scanned for redirect operators or path-like tokens. Quoted
                        // spans are data too (an awk comparison `'$1 >= 3'` or a jq filter must
                        // not read as a redirect), and `> /dev/null` discards output rather than
                        // editing a file.
                        let syntax = shell_syntax_view(&strip_heredoc_bodies(command));
                        if edit_cmd_re.is_match(&syntax) {
                            is_edit = true;
                            files = extract_bash_file_hint(&syntax)
                                .map(|f| vec![f])
                                .unwrap_or_else(|| vec![bash_label(command)]);
                        }
                        if verify_cmd_re.is_match(command) {
                            is_verify = true;
                        }
                    }

                    if is_edit && !all_ignored(&files, &config.ignore_paths) {
                        let visible_files: Vec<String> = files
                            .into_iter()
                            .filter(|f| !is_ignored_path(f, &config.ignore_paths))
                            .collect();
                        edits.push(EditEvent {
                            line: line_no,
                            tool_name: tool_name.clone(),
                            files: visible_files,
                        });
                    }
                    if is_verify {
                        let idx = verifies.len();
                        verifies.push(VerifyEvent {
                            line: line_no,
                            tool_name: tool_name.clone(),
                            is_error: false,
                            result_snippet: String::new(),
                            resolved: false,
                        });
                        if !tool_id.is_empty() {
                            pending_verify.insert(tool_id, idx);
                        }
                    }
                }
            }
            "user" => {
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let tool_use_id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(&idx) = pending_verify.get(tool_use_id) {
                        let is_error = block
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let text = result_text(block.get("content"));
                        // A permission denial means the call never ran: it is
                        // no evidence anything was checked, in either
                        // direction, so leave this verification unresolved
                        // exactly like an interrupted one.
                        if denial_re.is_match(&text) {
                            continue;
                        }
                        verifies[idx].is_error = is_error;
                        verifies[idx].result_snippet = text.chars().take(200).collect();
                        verifies[idx].resolved = true;
                    }
                }
            }
            _ => {}
        }
    }

    if malformed_lines > MAX_MALFORMED_LINE_WARNINGS {
        eprintln!(
            "verify-gate: ...and {} more malformed lines skipped (warnings capped at {MAX_MALFORMED_LINE_WARNINGS})",
            malformed_lines - MAX_MALFORMED_LINE_WARNINGS
        );
    }

    Ok(decide(edits, verifies, config))
}

/// Compiles a config-supplied pattern list, falling back to the built-in
/// default for that field (with a stderr warning) if the user's patterns
/// don't compile as regexes — consistent with how a malformed config file as
/// a whole already falls back to defaults, rather than treating a single bad
/// regex as a hard internal error.
fn compile_pattern_set(patterns: &[String], field_name: &str, fallback: &[String]) -> RegexSet {
    match RegexSet::new(patterns) {
        Ok(re) => re,
        Err(e) => {
            eprintln!(
                "verify-gate: invalid {field_name} ({e}), using the built-in default for this field"
            );
            RegexSet::new(fallback).expect("built-in default patterns must always compile")
        }
    }
}

fn decide(edits: Vec<EditEvent>, verifies: Vec<VerifyEvent>, config: &Config) -> Report {
    let Some(last_edit) = edits.last().cloned() else {
        return Report {
            last_edit: None,
            files_since_verification: Vec::new(),
            verifications_after_last_edit: Vec::new(),
            decision: Decision::Allow,
        };
    };

    // Only a verification whose tool_result actually arrived counts: a
    // tool_use with no result (interrupted, denied, or never completed)
    // is not evidence anything was checked. `>=` rather than `>`: one Bash
    // call can both edit and verify (`printf > x.json && curl ...`), and the
    // two events then share a line.
    let verifies_after: Vec<VerifyEvent> = verifies
        .iter()
        .filter(|v| v.line >= last_edit.line && v.resolved)
        .cloned()
        .collect();

    if verifies_after.is_empty() {
        let last_verify_before_line = verifies
            .iter()
            .filter(|v| v.line < last_edit.line)
            .map(|v| v.line)
            .max()
            .unwrap_or(0);

        let mut files: Vec<String> = Vec::new();
        let mut count = 0usize;
        for e in edits.iter().filter(|e| e.line > last_verify_before_line) {
            count += 1;
            for f in &e.files {
                if !files.contains(f) {
                    files.push(f.clone());
                }
            }
        }

        let decision = if count < config.min_edits {
            Decision::Allow
        } else {
            let file_list = if files.is_empty() {
                "an unnamed file".to_string()
            } else {
                files.join(", ")
            };
            Decision::Block {
                reason: format!(
                    "Edited without verification: {file_list}. run the tests/build or verify the behaviour live; if verification is genuinely impossible, say so explicitly in your final message."
                ),
            }
        };

        Report {
            last_edit: Some(last_edit),
            files_since_verification: files,
            verifications_after_last_edit: Vec::new(),
            decision,
        }
    } else {
        // Latest-wins: only the newest resolved verification(s) decide. The
        // previous rule blocked on ANY failure after the last edit, which
        // made a single benign failure unclearable: no number of later green
        // runs could ever lift the block until the next edit reset the
        // window. Verifications dispatched together in one assistant message
        // share a line; that tie is resolved conservatively, so one failure
        // among the batch still blocks.
        let latest_line = verifies_after
            .iter()
            .map(|v| v.line)
            .max()
            .expect("verifies_after is non-empty in this branch");
        let latest_failure = verifies_after
            .iter()
            .filter(|v| v.line == latest_line)
            .find(|v| v.is_error);
        let decision = if let Some(failed) = latest_failure {
            Decision::Block {
                reason: format!("last verification failed: {}", failed.result_snippet),
            }
        } else {
            Decision::Allow
        };
        Report {
            last_edit: Some(last_edit),
            files_since_verification: Vec::new(),
            verifications_after_last_edit: verifies_after,
            decision,
        }
    }
}

fn extract_files(input: &Value) -> Vec<String> {
    let mut files = Vec::new();
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            files.push(s.to_string());
        }
    }
    files
}

/// Shell-syntax view of a command: quoted spans collapse to a single space and
/// backslash-escaped characters are dropped, because that text is data handed to
/// a program, not shell syntax — an awk program like `'$1 >= 3'` or a jq filter
/// like `'.tag_name'` must never read as a redirect. Redirects whose target is a
/// `/dev/` sink are also dropped: `> /dev/null` discards output, it edits nothing.
fn shell_syntax_view(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(' ');
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(' ');
            }
            // Backslash escapes the next char (single quotes excepted, where it
            // is literal data and the arm below drops it anyway).
            '\\' if !in_single => {
                chars.next();
                out.push(' ');
            }
            _ if in_single || in_double => {}
            _ => out.push(c),
        }
    }
    static DEV_SINK: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let dev_sink = DEV_SINK
        .get_or_init(|| Regex::new(r"[0-9]*>{1,2}\s*/dev/\w+").expect("static regex compiles"));
    dev_sink.replace_all(&out, " ").into_owned()
}

/// Strips heredoc bodies (`<<EOF`, `<<'EOF'`, `<<"EOF"`, `<<-EOF`, ...) out of a
/// Bash command, because a heredoc body is data written to a file or piped to
/// a program, not shell syntax: it must never be scanned for redirect
/// operators or path-like tokens by the edit-detection code below. The line
/// that opens the heredoc is kept (it carries the real redirect, e.g.
/// `cat > file.py <<'EOF'`); every line up to and including the terminator
/// line is dropped.
fn strip_heredoc_bodies(command: &str) -> String {
    static HEREDOC_START: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let heredoc_start = HEREDOC_START.get_or_init(|| {
        // No backreference to the opening quote (the `regex` crate doesn't
        // support them): a mismatched quote around the tag is not something
        // real shell input produces, so it's fine to just take the tag name.
        Regex::new(r#"<<(-)?\s*['"]?([A-Za-z_][A-Za-z0-9_]*)"#).expect("static regex compiles")
    });

    let lines: Vec<&str> = command.split('\n').collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        out.push(line);
        i += 1;
        let Some(caps) = heredoc_start.captures(line) else {
            continue;
        };
        let strip_tabs = caps.get(1).is_some(); // `<<-` allows leading tabs on the terminator
        let tag = caps.get(2).unwrap().as_str();
        while i < lines.len() {
            let body_line = lines[i];
            i += 1;
            let compare = if strip_tabs {
                body_line.trim_start_matches('\t')
            } else {
                body_line
            };
            if compare == tag {
                break; // terminator line consumed and dropped, not shell syntax either
            }
            // body line dropped: heredoc bodies are data, not shell syntax
        }
    }
    out.join("\n")
}

/// Commands that unambiguously edit a file without a `>`/`>>` redirect
/// operator to key off (their target is a plain argument instead).
fn edit_keyword_re() -> &'static Regex {
    static EDIT_KEYWORD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    EDIT_KEYWORD.get_or_init(|| {
        Regex::new(r"\bsed -i|\btee\b|\bgit apply\b|\bpatch\b").expect("static regex compiles")
    })
}

/// Matches a `>`/`>>` redirect operator that writes to fd 1 (or explicit
/// `1>`), the same operator shape `edit_command_patterns`'s default treats as
/// an edit: `2>`, `3>`, etc. are excluded (their digit sits where `(^|\s)`
/// needs to match). `>&`-style fd duplications are excluded separately by
/// `redirect_matches` below, since the `regex` crate has no lookahead.
fn redirect_op_re() -> &'static Regex {
    static REDIRECT_OP: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REDIRECT_OP.get_or_init(|| Regex::new(r"(?:^|\s)1?>{1,2}").expect("static regex compiles"))
}

fn split_command_segments(command: &str) -> Vec<&str> {
    static SEGMENT_SPLIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = SEGMENT_SPLIT
        .get_or_init(|| Regex::new(r"&&|\|\||;|\n|\|").expect("static regex compiles"));
    re.split(command).collect()
}

/// Redirect-operator matches in `segment`, excluding `>&`-style fd
/// duplications (`>&2`, `1>&2`): those don't write to a file at all.
fn redirect_matches(segment: &str) -> impl Iterator<Item = regex::Match<'_>> {
    redirect_op_re()
        .find_iter(segment)
        .filter(|m| !segment[m.end()..].starts_with('&'))
}

/// The first `>`/`>>` target in `segment` that isn't a `/dev/` sink or an fd
/// duplication. Returns `None` both when there's no redirect operator at
/// all, and when every redirect operator present only targets a sink -- the
/// two cases are told apart by the caller via `redirect_matches`.
fn redirect_target_in_segment(segment: &str) -> Option<String> {
    for m in redirect_matches(segment) {
        let rest = segment[m.end()..].trim_start();
        let Some(raw_target) = rest.split_whitespace().next() else {
            continue;
        };
        let target = raw_target.trim_matches(|c| c == '"' || c == '\'');
        if target.is_empty() {
            continue;
        }
        if target == "/dev/null" || target.starts_with("/dev/") {
            continue;
        }
        return Some(target.to_string());
    }
    None
}

/// Last non-flag, path-like token in `segment`, ignoring anything that itself
/// contains a redirect operator. This is the fallback used only for commands
/// (`sed -i`, `tee`, `git apply`, `patch`) whose file target is a plain
/// argument rather than a `>`/`>>` redirect target.
fn last_path_like_token(segment: &str) -> Option<String> {
    segment
        .split_whitespace()
        .rev()
        .find(|tok| {
            !tok.starts_with('-')
                && !tok.contains('>')
                && !tok.contains('<')
                && (tok.contains('/') || tok.contains('.'))
        })
        .map(|s| s.trim_matches(|c| c == '"' || c == '\'').to_string())
}

/// Finds the file a Bash edit command targets, scoped to the command segment
/// (split on `&&`, `||`, `;`, `|`, and newlines) that actually performs the
/// edit, rather than the last path-like token anywhere in the whole command:
/// a later, unrelated segment in the same compound command must never
/// override the real target.
fn extract_bash_file_hint(command: &str) -> Option<String> {
    // Track `cd <dir>` in earlier segments so a relative target written after
    // it resolves under that directory: `cd /x/scratchpad && printf > f.json`
    // edits /x/scratchpad/f.json, and ignore-path globs must see that.
    let mut last_cd: Option<String> = None;
    for segment in split_command_segments(command) {
        let mut words = segment.split_whitespace();
        if words.next() == Some("cd") {
            if let Some(dir) = words.next() {
                let dir = dir.trim_matches(|c| c == '"' || c == '\'');
                if !dir.is_empty() && !dir.starts_with('-') {
                    last_cd = Some(dir.to_string());
                }
            }
        }
        if let Some(target) = redirect_target_in_segment(segment) {
            return Some(join_cd(&last_cd, target));
        }
        if redirect_matches(segment).next().is_some() {
            // A redirect operator is present but every target it points at
            // is a sink or fd duplication: this segment is conclusively not
            // an edit, so don't fall through to the generic token scan below
            // and risk picking up an unrelated argument as a false hint.
            continue;
        }
        if edit_keyword_re().is_match(segment) {
            if let Some(target) = last_path_like_token(segment) {
                return Some(join_cd(&last_cd, target));
            }
        }
    }
    None
}

/// Resolves a relative edit target under the last `cd` directory seen in the
/// same command, when there is one. Absolute (and `~`-prefixed) targets are
/// left alone.
fn join_cd(last_cd: &Option<String>, target: String) -> String {
    if target.starts_with('/') || target.starts_with('~') {
        return target;
    }
    match last_cd {
        Some(dir) => format!("{}/{}", dir.trim_end_matches('/'), target),
        None => target,
    }
}

fn bash_label(command: &str) -> String {
    let truncated: String = command.chars().take(60).collect();
    format!("$ {truncated}")
}

fn is_ignored_path(file: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| glob_matches(g, file))
}

fn all_ignored(files: &[String], globs: &[String]) -> bool {
    if files.is_empty() {
        return false;
    }
    files.iter().all(|f| is_ignored_path(f, globs))
}

/// Full text of a tool_result's content (truncation happens at the caller,
/// after denial-pattern matching has seen the whole text).
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| item.to_string())
            })
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bash_file_hint_finds_sed_target() {
        assert_eq!(
            extract_bash_file_hint("sed -i s/a/b/ src/x.rs"),
            Some("src/x.rs".to_string())
        );
    }

    #[test]
    fn extract_bash_file_hint_finds_plain_redirect_target() {
        assert_eq!(
            extract_bash_file_hint("echo fn main(){} > src/main.rs"),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn extract_bash_file_hint_ignores_trailing_stderr_redirect() {
        // A trailing `2>/dev/null` must never be picked as the "file", even
        // when it's the last path-looking token in the command.
        assert_eq!(
            extract_bash_file_hint("sed -i s/a/b/ file.txt 2>/dev/null"),
            Some("file.txt".to_string())
        );
    }

    #[test]
    fn extract_bash_file_hint_none_when_nothing_path_like() {
        assert_eq!(extract_bash_file_hint("ls -la"), None);
    }

    #[test]
    fn bash_label_truncates_and_prefixes() {
        let long = "x".repeat(100);
        let label = bash_label(&long);
        assert_eq!(label, format!("$ {}", "x".repeat(60)));
    }

    fn default_edit_re() -> RegexSet {
        compile_pattern_set(
            &Config::default().edit_command_patterns,
            "edit_command_patterns",
            &Config::default().edit_command_patterns,
        )
    }

    #[test]
    fn quoted_awk_comparison_is_not_an_edit() {
        // `>=` inside a single-quoted awk program is data, not a redirect.
        let cmd = "for f in $(ls dir | awk -F_ '$1 >= 20260617'); do echo dir/$f; done";
        assert!(!default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    #[test]
    fn quoted_jq_filter_is_not_an_edit() {
        let cmd = r#"gh api repos/o/r/releases --jq '.tag_name + " " + .published_at' 2>/dev/null"#;
        assert!(!default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    #[test]
    fn dev_null_redirect_is_not_an_edit() {
        let cmd = "npx tool check config.yml > /dev/null 2>&1 && echo ok";
        assert!(!default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    #[test]
    fn real_redirect_still_detected_through_view() {
        let cmd = "echo hello > out.txt";
        assert!(default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    #[test]
    fn sed_in_place_still_detected_through_view() {
        let cmd = "sed -i s/a/b/ src/x.rs";
        assert!(default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    #[test]
    fn quoted_mention_of_sed_is_not_an_edit() {
        let cmd = "echo 'use sed -i for that'";
        assert!(!default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    #[test]
    fn view_survives_escaped_quotes_inside_double_quotes() {
        // The escaped quote must not end the span and leak `>` as syntax.
        let cmd = r#"grep "a \" b > c" file.txt"#;
        assert!(!default_edit_re().is_match(&shell_syntax_view(cmd)));
    }

    /// Mirrors exactly what `evaluate_reader` does to a Bash command before
    /// hint extraction: strip heredoc bodies, then take the shell-syntax view.
    fn hint_via_pipeline(cmd: &str) -> Option<String> {
        extract_bash_file_hint(&shell_syntax_view(&strip_heredoc_bodies(cmd)))
    }

    #[test]
    fn heredoc_write_hint_ignores_body_text_uses_redirect_target() {
        // The heredoc body is data, not shell syntax; the hint must come from
        // the `>` target on the launching line, not from anything inside the
        // body or from a later, unrelated command.
        let cmd = "cat > scratch/build.py <<'EOF'\nsee /docs/api); note\nEOF\npython3 scratch/build.py";
        assert_eq!(hint_via_pipeline(cmd), Some("scratch/build.py".to_string()));
    }

    #[test]
    fn heredoc_write_hint_ignores_trailing_chained_commands() {
        // A heredoc-terminator line followed by further chained commands
        // (here ending in a piped `grep`) must not steal the hint away from
        // the original redirect target.
        let cmd = "cat > scratch/build.py <<'PYEOF'\nsee /docs/api); note\nPYEOF\npython3 scratch/build.py && true; grep -n \"x\" styles/components.css | head";
        assert_eq!(hint_via_pipeline(cmd), Some("scratch/build.py".to_string()));
    }

    #[test]
    fn redirect_to_dev_null_with_no_other_target_is_not_an_edit() {
        // `>/dev/null` discards output; a trailing read-only command in the
        // same compound command must not be mistaken for an edit either.
        let cmd = "qlmanage -t -s 1400 -o scratch/render input.svg >/dev/null 2>&1; ls scratch/render";
        let syntax = shell_syntax_view(&strip_heredoc_bodies(cmd));
        assert!(!default_edit_re().is_match(&syntax));
    }

    #[test]
    fn redirect_hint_scoped_to_segment_before_a_later_heredoc() {
        // The real edit is the `>` redirect in the first `&&`-segment; a
        // heredoc later in the same compound command (and the cleanup
        // commands after it) must not override that hint.
        let cmd = "sed -e 's/a/b/' input.txt > output.txt && python3 - <<'EOF'\nsee /docs/api); note\nEOF\nrm -f input.txt; ls -la /tmp/files*";
        assert_eq!(hint_via_pipeline(cmd), Some("output.txt".to_string()));
    }

    /// One assistant record holding `tool_use` blocks (id, tool name, input).
    fn assistant_record(uses: &[(&str, &str, Value)]) -> String {
        let blocks: Vec<Value> = uses
            .iter()
            .map(|(id, name, input)| {
                serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
            })
            .collect();
        serde_json::json!({
            "type": "assistant",
            "isSidechain": false,
            "message": {"role": "assistant", "content": blocks}
        })
        .to_string()
    }

    /// One user record holding a `tool_result` for `id`.
    fn result_record(id: &str, is_error: bool, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "isSidechain": false,
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": id, "is_error": is_error,
                "content": [{"type": "text", "text": text}]
            }]}
        })
        .to_string()
    }

    fn evaluate_lines(lines: &[String]) -> Report {
        let transcript = format!("{}\n", lines.join("\n"));
        let reader = BufReader::new(std::io::Cursor::new(transcript.into_bytes()));
        evaluate_reader(reader, &Config::default()).expect("evaluates cleanly")
    }

    fn edit_record(id: &str) -> String {
        assistant_record(&[(
            id,
            "Edit",
            serde_json::json!({"file_path": "src/x.rs", "old_string": "a", "new_string": "b"}),
        )])
    }

    fn verify_record(id: &str) -> String {
        assistant_record(&[(id, "Bash", serde_json::json!({"command": "cargo test"}))])
    }

    const DENIAL_TEXT: &str = "Permission for this action was denied by the Claude Code auto mode classifier. Reason: Blocked by classifier.";

    #[test]
    fn later_green_verification_clears_earlier_failure() {
        // A failed check followed by a passing one must not keep blocking:
        // the newest resolved verification decides.
        let report = evaluate_lines(&[
            edit_record("e1"),
            result_record("e1", false, "ok"),
            verify_record("v1"),
            result_record("v1", true, "exit code 1"),
            verify_record("v2"),
            result_record("v2", false, "all green"),
        ]);
        assert_eq!(report.decision, Decision::Allow);
    }

    #[test]
    fn latest_failing_verification_still_blocks() {
        let report = evaluate_lines(&[
            edit_record("e1"),
            result_record("e1", false, "ok"),
            verify_record("v1"),
            result_record("v1", false, "all green"),
            verify_record("v2"),
            result_record("v2", true, "exit code 1"),
        ]);
        assert!(report.is_block());
    }

    #[test]
    fn parallel_same_line_green_and_error_blocks() {
        // Two verifications dispatched in ONE assistant message share a line;
        // if either failed, the tie is resolved conservatively as a block.
        let parallel = assistant_record(&[
            ("v1", "Bash", serde_json::json!({"command": "cargo test"})),
            ("v2", "Bash", serde_json::json!({"command": "cargo build"})),
        ]);
        let report = evaluate_lines(&[
            edit_record("e1"),
            result_record("e1", false, "ok"),
            parallel,
            result_record("v1", false, "all green"),
            result_record("v2", true, "exit code 101"),
        ]);
        assert!(report.is_block());
    }

    #[test]
    fn permission_denial_is_not_a_failed_verification() {
        // A denied tool call never ran: it is no evidence either way, so the
        // earlier green verification still stands.
        let report = evaluate_lines(&[
            edit_record("e1"),
            result_record("e1", false, "ok"),
            verify_record("v1"),
            result_record("v1", false, "all green"),
            verify_record("v2"),
            result_record("v2", true, DENIAL_TEXT),
        ]);
        assert_eq!(report.decision, Decision::Allow);
    }

    #[test]
    fn denied_only_verification_leaves_edit_unverified() {
        // A denial is not a verification either: with nothing else after the
        // edit, this is the "edited without verification" case, not a
        // "verification failed" one.
        let report = evaluate_lines(&[
            edit_record("e1"),
            result_record("e1", false, "ok"),
            verify_record("v1"),
            result_record("v1", true, DENIAL_TEXT),
        ]);
        match &report.decision {
            Decision::Block { reason } => assert!(
                reason.contains("Edited without verification"),
                "wrong reason: {reason}"
            ),
            Decision::Allow => panic!("expected a block"),
        }
    }

    #[test]
    fn same_record_edit_and_verify_counts_as_verified() {
        // `printf > file && curl ...` edits and verifies in one Bash call:
        // the verification on the same line must count as after the edit.
        let combined = assistant_record(&[(
            "b1",
            "Bash",
            serde_json::json!({"command": "printf '%s' '{}' > data.json && curl -s -d @data.json https://api.example.com"}),
        )]);
        let report = evaluate_lines(&[combined.clone(), result_record("b1", false, "[]")]);
        assert_eq!(report.decision, Decision::Allow);

        let failing = evaluate_lines(&[combined, result_record("b1", true, "exit code 7")]);
        assert!(failing.is_block());
    }

    #[test]
    fn relative_redirect_after_cd_into_ignored_dir_is_ignored() {
        // `cd .../scratchpad && printf > file.json` writes under an ignored
        // path even though the redirect target is spelled relative.
        let record = assistant_record(&[(
            "b1",
            "Bash",
            serde_json::json!({"command": "cd /private/tmp/session/scratchpad && printf '%s' '{}' > sam_adv.json"}),
        )]);
        let report = evaluate_lines(&[record, result_record("b1", false, "")]);
        assert_eq!(report.decision, Decision::Allow);
        assert!(report.last_edit.is_none());
    }

    #[test]
    fn cd_tracking_absolutizes_relative_redirect_targets() {
        assert_eq!(
            hint_via_pipeline("cd /a/b && printf y > f.json"),
            Some("/a/b/f.json".to_string())
        );
        // Absolute targets are left alone.
        assert_eq!(
            hint_via_pipeline("cd /a/b && printf y > /c/f.json"),
            Some("/c/f.json".to_string())
        );
        // No cd: the relative target stays relative.
        assert_eq!(
            hint_via_pipeline("printf y > f.json"),
            Some("f.json".to_string())
        );
    }

    #[test]
    fn heredoc_write_target_under_ignored_path_produces_no_edit_event() {
        // Regression guard for the false-positive this fix closes: once the
        // hint correctly resolves to the redirect target, an ignored-path
        // target (e.g. under scratchpad/) must suppress the edit entirely,
        // exactly like a non-Bash edit tool would.
        let command = "cat > scratchpad/build.py <<'EOF'\nsee /docs/api); note\nEOF";
        let transcript = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "isSidechain": false,
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "Bash",
                        "input": {"command": command}
                    }]
                }
            })
        );
        let reader = BufReader::new(std::io::Cursor::new(transcript.into_bytes()));
        let report = evaluate_reader(reader, &Config::default()).expect("evaluates cleanly");
        assert_eq!(report.decision, Decision::Allow);
        assert!(report.last_edit.is_none());
    }
}
