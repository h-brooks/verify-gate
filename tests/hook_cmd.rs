//! Integration tests over `verify-gate hook`, the actual seam the harness
//! drives: JSON on stdin, JSON (or nothing) on stdout, always exit 0.
use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn fixture_path(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn stdin_for(transcript: &str, stop_hook_active: bool) -> String {
    serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": fixture_path(transcript),
        "cwd": std::env::temp_dir().to_string_lossy(),
        "hook_event_name": "Stop",
        "stop_hook_active": stop_hook_active,
    })
    .to_string()
}

#[test]
fn blocks_with_reason_json_and_exits_zero() {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .write_stdin(stdin_for("edit_no_verification.jsonl", false))
        .assert()
        .success() // hook ALWAYS exits 0; the block signal is in stdout JSON
        .stdout(
            predicate::str::contains("\"decision\":\"block\"")
                .and(predicate::str::contains("src/widget.rs")),
        );
}

#[test]
fn allows_with_no_stdout_when_verified() {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .write_stdin(stdin_for("edit_then_verified.jsonl", false))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn stop_hook_active_always_allows_even_when_it_would_block() {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .write_stdin(stdin_for("edit_no_verification.jsonl", true))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn malformed_stdin_allows_and_logs_to_stderr() {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .write_stdin("{ this is not valid json")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn dry_run_flag_always_allows() {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .arg("--dry-run")
        .write_stdin(stdin_for("edit_no_verification.jsonl", false))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn verify_gate_disable_env_always_allows() {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .env("VERIFY_GATE_DISABLE", "1")
        .write_stdin(stdin_for("edit_no_verification.jsonl", false))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn missing_transcript_path_allows_and_logs_to_stderr() {
    let stdin = serde_json::json!({
        "session_id": "sess-1",
        "hook_event_name": "Stop",
        "stop_hook_active": false,
    })
    .to_string();
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .write_stdin(stdin)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn nonexistent_transcript_file_allows_and_logs_to_stderr() {
    let stdin = serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": "/nonexistent/path/does-not-exist.jsonl",
        "hook_event_name": "Stop",
        "stop_hook_active": false,
    })
    .to_string();
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("hook")
        .write_stdin(stdin)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}
