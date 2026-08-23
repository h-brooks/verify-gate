//! Integration tests over `verify-gate hook`, the actual seam the harness
//! drives: JSON on stdin, JSON (or nothing) on stdout, always exit 0.
mod common;

use predicates::prelude::*;
use std::path::Path;

fn fixture_path(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

// `cwd` is set to the same isolated, empty directory `isolated_cmd` points
// HOME at, so Config::load can't pick up a stray `.verify-gate.toml` from
// wherever `cargo test` happens to be run either.
fn stdin_for(transcript: &str, stop_hook_active: bool, cwd: &Path) -> String {
    serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": fixture_path(transcript),
        "cwd": cwd.to_string_lossy(),
        "hook_event_name": "Stop",
        "stop_hook_active": stop_hook_active,
    })
    .to_string()
}

#[test]
fn blocks_with_reason_json_and_exits_zero() {
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("hook")
        .write_stdin(stdin_for("edit_no_verification.jsonl", false, &home))
        .assert()
        .success() // hook ALWAYS exits 0; the block signal is in stdout JSON
        .stdout(
            predicate::str::contains("\"decision\":\"block\"")
                .and(predicate::str::contains("src/widget.rs")),
        );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn allows_with_no_stdout_when_verified() {
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("hook")
        .write_stdin(stdin_for("edit_then_verified.jsonl", false, &home))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn stop_hook_active_always_allows_even_when_it_would_block() {
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("hook")
        .write_stdin(stdin_for("edit_no_verification.jsonl", true, &home))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn malformed_stdin_allows_and_logs_to_stderr() {
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("hook")
        .write_stdin("{ this is not valid json")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn dry_run_flag_always_allows() {
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("hook")
        .arg("--dry-run")
        .write_stdin(stdin_for("edit_no_verification.jsonl", false, &home))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn verify_gate_disable_env_always_allows() {
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("hook")
        .env("VERIFY_GATE_DISABLE", "1")
        .write_stdin(stdin_for("edit_no_verification.jsonl", false, &home))
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn missing_transcript_path_allows_and_logs_to_stderr() {
    let (mut cmd, home) = common::isolated_cmd();
    let stdin = serde_json::json!({
        "session_id": "sess-1",
        "hook_event_name": "Stop",
        "stop_hook_active": false,
    })
    .to_string();
    cmd.arg("hook")
        .write_stdin(stdin)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn nonexistent_transcript_file_allows_and_logs_to_stderr() {
    let (mut cmd, home) = common::isolated_cmd();
    let stdin = serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": "/nonexistent/path/does-not-exist.jsonl",
        "hook_event_name": "Stop",
        "stop_hook_active": false,
    })
    .to_string();
    cmd.arg("hook")
        .write_stdin(stdin)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn directory_transcript_path_allows_quickly_instead_of_hanging() {
    // Same root cause as the `check`-side regression test, exercised through
    // the actual harness seam: a Stop hook must never hang the turn, and an
    // unreadable transcript must fail open (allow) rather than loop.
    let dir = std::env::temp_dir().join(format!(
        "verify-gate-hook-dir-transcript-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let home = common::isolated_home();

    let stdin = serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": dir.to_string_lossy(),
        "cwd": home.to_string_lossy(),
        "hook_event_name": "Stop",
        "stop_hook_active": false,
    })
    .to_string();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_verify-gate"))
        .arg("hook")
        .env("HOME", &home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();

    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            child.kill().ok();
            child.wait().ok();
            std::fs::remove_dir_all(&dir).ok();
            std::fs::remove_dir_all(&home).ok();
            panic!("verify-gate hook hung on a directory transcript path");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    let output = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&home).ok();

    assert!(status.success(), "hook must always exit 0");
    assert!(
        output.stdout.is_empty(),
        "must fail open with no block JSON"
    );
}
