use crate::config::Config;
use crate::glob::glob_matches;
use anyhow::{Context, Result};
use regex::RegexSet;
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
    let edit_cmd_re =
        RegexSet::new(&config.edit_command_patterns).context("compiling edit_command_patterns")?;
    let verify_cmd_re =
        RegexSet::new(&config.verify_patterns).context("compiling verify_patterns")?;
    let verify_tool_re = RegexSet::new(&config.verify_tools).context("compiling verify_tools")?;

    let mut edits: Vec<EditEvent> = Vec::new();
    let mut verifies: Vec<VerifyEvent> = Vec::new();
    // tool_use_id -> index into `verifies`, so a later tool_result can fill in is_error.
    let mut pending_verify: HashMap<String, usize> = HashMap::new();

    for (idx, line_result) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                eprintln!("verify-gate: read error at line {line_no}: {e}");
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("verify-gate: skipping malformed line {line_no}: {e}");
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
                        if edit_cmd_re.is_match(command) {
                            is_edit = true;
                            files = extract_bash_file_hint(command)
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
                    }
                }
            }
            _ => {}
        }
    }

    Ok(decide(edits, verifies, config))
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

    let verifies_after: Vec<VerifyEvent> = verifies
        .iter()
        .filter(|v| v.line > last_edit.line)
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
            Decision::Block {
                reason: format!(
                    "Edited without verification: {}. run the tests/build or verify the behaviour live; if verification is genuinely impossible, say so explicitly in your final message.",
                    files.join(", ")
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
        let latest = verifies_after.last().unwrap();
        let decision = if latest.is_error {
            Decision::Block {
                reason: format!("last verification failed: {}", latest.result_snippet),
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

fn extract_bash_file_hint(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .rev()
        .find(|tok| !tok.starts_with('-') && (tok.contains('/') || tok.contains('.')))
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
