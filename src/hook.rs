use crate::config::Config;
use crate::transcript::{evaluate_transcript, Decision};
use serde_json::json;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Implements the `hook` subcommand: reads a Stop-hook JSON payload from
/// stdin and, if the rule says to block, prints the block JSON to stdout.
/// Never crashes: any failure here is logged to stderr and treated as allow.
pub fn run_hook(dry_run: bool) {
    if dry_run || std::env::var("VERIFY_GATE_DISABLE").as_deref() == Ok("1") {
        eprintln!("verify-gate: dry-run/VERIFY_GATE_DISABLE set, allowing");
        return;
    }

    let mut input_text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input_text) {
        eprintln!("verify-gate: failed to read stdin: {e}, allowing");
        return;
    }

    let input: serde_json::Value = match serde_json::from_str(&input_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("verify-gate: malformed stdin JSON: {e}, allowing");
            return;
        }
    };

    let stop_hook_active = input
        .get("stop_hook_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if stop_hook_active {
        // Already continuing because of a previous block: never block again,
        // or the agent loops forever.
        return;
    }

    let Some(transcript_path) = input.get("transcript_path").and_then(|v| v.as_str()) else {
        eprintln!("verify-gate: missing transcript_path in stdin, allowing");
        return;
    };

    let cwd: Option<PathBuf> = input.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
    let config = Config::load(cwd.as_deref());

    match evaluate_transcript(Path::new(transcript_path), &config) {
        Ok(report) => {
            if let Decision::Block { reason } = report.decision {
                let out = json!({ "decision": "block", "reason": reason });
                println!("{out}");
            }
        }
        Err(e) => {
            eprintln!("verify-gate: internal error evaluating transcript: {e:#}, allowing");
        }
    }
}
