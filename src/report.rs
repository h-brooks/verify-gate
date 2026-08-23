use crate::transcript::{Decision, Report};

pub fn format_text(report: &Report) -> String {
    let mut out = String::new();
    match &report.last_edit {
        Some(e) => {
            out.push_str(&format!("last edit: line {} ({})\n", e.line, e.tool_name));
            out.push_str(&format!("files edited: {}\n", edited_files_list(report)));
        }
        None => {
            out.push_str("last edit: none\n");
            out.push_str("files edited: (none)\n");
        }
    }
    if report.verifications_after_last_edit.is_empty() {
        out.push_str("verifications after last edit: none\n");
    } else {
        let list = report
            .verifications_after_last_edit
            .iter()
            .map(|v| {
                format!(
                    "{} (line {}{})",
                    v.tool_name,
                    v.line,
                    if v.is_error { ", failed" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("verifications after last edit: {list}\n"));
    }
    match &report.decision {
        Decision::Allow => out.push_str("decision: allow\n"),
        Decision::Block { reason } => {
            out.push_str("decision: block\n");
            out.push_str(&format!("reason: {reason}\n"));
        }
    }
    out
}

pub fn format_json(report: &Report) -> String {
    let value = serde_json::json!({
        "last_edit_line": report.last_edit.as_ref().map(|e| e.line),
        "last_edit_tool": report.last_edit.as_ref().map(|e| e.tool_name.clone()),
        "files_edited": edited_files(report),
        "verifications_after_last_edit": report
            .verifications_after_last_edit
            .iter()
            .map(|v| {
                serde_json::json!({
                    "line": v.line,
                    "tool_name": v.tool_name,
                    "is_error": v.is_error,
                })
            })
            .collect::<Vec<_>>(),
        "decision": match &report.decision {
            Decision::Allow => "allow",
            Decision::Block { .. } => "block",
        },
        "reason": match &report.decision {
            Decision::Allow => None,
            Decision::Block { reason } => Some(reason.clone()),
        },
    });
    serde_json::to_string_pretty(&value).unwrap()
}

fn edited_files(report: &Report) -> Vec<String> {
    if !report.files_since_verification.is_empty() {
        report.files_since_verification.clone()
    } else {
        report
            .last_edit
            .as_ref()
            .map(|e| e.files.clone())
            .unwrap_or_default()
    }
}

fn edited_files_list(report: &Report) -> String {
    let files = edited_files(report);
    if files.is_empty() {
        "(none)".to_string()
    } else {
        files.join(", ")
    }
}
