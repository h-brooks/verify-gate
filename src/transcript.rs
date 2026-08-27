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
                        // Edit detection runs on the shell-SYNTAX view: quoted spans are data
                        // handed to a program, not shell syntax, so an awk comparison ('$1 >= 3')
                        // or a jq filter must not read as a redirect; likewise `> /dev/null`
                        // discards output rather than editing a file.
                        let syntax = shell_syntax_view(command);
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
                        verifies[idx].is_error = is_error;
                        verifies[idx].result_snippet = result_snippet(block.get("content"));
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
    // tool_use with no result (interrupted, rejected, or never completed)
    // is not evidence anything was checked.
    let verifies_after: Vec<VerifyEvent> = verifies
        .iter()
        .filter(|v| v.line > last_edit.line && v.resolved)
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
        // Block if ANY verification after the last edit failed, not just the
        // one with the highest tool_use line: tool calls can be dispatched
        // out of order relative to when their results come back, so "last by
        // tool_use line" can pick a result that isn't actually the most
        // recent thing that happened, and silently hide an earlier failure.
        let decision = if let Some(failed) = verifies_after.iter().rev().find(|v| v.is_error) {
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

fn extract_bash_file_hint(command: &str) -> Option<String> {
    command
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

fn result_snippet(content: Option<&Value>) -> String {
    let text = match content {
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
    };
    text.chars().take(200).collect()
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
}
