//! Integration tests over `verify-gate check`, driving the built binary
//! against synthetic fixture transcripts (never real ones).
mod common;

use predicates::prelude::*;
use std::io::Write;
use std::path::Path;

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

// Every invocation runs with HOME and cwd pointed at a fresh empty directory
// (see tests/common/mod.rs) so the result can never depend on the developer's
// real `~/.config/verify-gate/config.toml` or a stray `.verify-gate.toml` at
// the crate root.
fn check(name: &str) -> assert_cmd::assert::Assert {
    let (mut cmd, home) = common::isolated_cmd();
    let result = cmd.arg("check").arg(fixture(name)).assert();
    std::fs::remove_dir_all(&home).ok();
    result
}

#[test]
fn no_edits_allows() {
    check("no_edits.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn edit_then_verified_allows() {
    check("edit_then_verified.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn edit_without_verification_blocks_and_names_file() {
    check("edit_no_verification.jsonl")
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("decision: block")
                .and(predicate::str::contains("src/widget.rs")),
        );
}

#[test]
fn edit_then_failed_verification_blocks() {
    check("edit_then_verification_error.jsonl")
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("decision: block")
                .and(predicate::str::contains("last verification failed")),
        );
}

#[test]
fn verification_then_another_edit_blocks() {
    check("verify_then_edit.jsonl")
        .failure()
        .code(1)
        .stdout(predicate::str::contains("decision: block"));
}

#[test]
fn edits_only_to_markdown_allow() {
    check("edits_only_md.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn sidechain_edits_are_ignored() {
    // The only edit in this transcript belongs to a subagent (isSidechain:
    // true); the main session made no edits, so the gate should not fire.
    check("sidechain_edit_ignored.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn mcp_browser_tool_counts_as_verification() {
    check("edit_then_mcp_verify.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn bash_echo_redirect_edit_without_verification_blocks() {
    // `echo ... > file` has whitespace before `>`, which the old
    // `\b(...|>{1,2} ?[^&]|...)` pattern's leading `\b` never matched (`\b`
    // requires a word character on one side, and a space isn't one).
    check("bash_edit_via_redirect_no_verification.jsonl")
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("decision: block")
                .and(predicate::str::contains("src/main.rs")),
        );
}

#[test]
fn bash_append_redirect_edit_without_verification_blocks() {
    check("bash_edit_via_append_redirect_no_verification.jsonl")
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("decision: block")
                .and(predicate::str::contains("src/generated.rs")),
        );
}

#[test]
fn readonly_grep_with_stderr_redirect_after_verified_edit_allows() {
    // `2>/dev/null` must never be misread as a file-write redirect (the
    // digit before `>` is not whitespace) nor picked up by
    // extract_bash_file_hint as the "edited" file.
    check("edit_then_verified_then_readonly_grep_stderr_redirect.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn interleaved_verification_results_blocks_on_any_failure_after_edit() {
    // The failing `cargo test` tool_use is issued before the passing `curl`
    // tool_use, but its result arrives after. Picking "verification with the
    // highest tool_use line" would wrongly report the passing curl as the
    // last word and allow; the rule must block if ANY verification after the
    // edit failed.
    check("interleaved_verification_failure_after_success_blocks.jsonl")
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("decision: block")
                .and(predicate::str::contains("last verification failed")),
        );
}

#[test]
fn unresolved_verification_tool_use_does_not_count_as_passing() {
    // The `cargo test` tool_use has no matching tool_result (the user
    // interrupted it) -- it must not count as a verification that happened.
    check("edit_then_unresolved_verification_blocks.jsonl")
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("decision: block")
                .and(predicate::str::contains("src/widget.rs")),
        );
}

#[test]
fn pytest_counts_as_verification() {
    check("edit_then_pytest_verified.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn npm_test_counts_as_verification() {
    check("edit_then_npm_test_verified.jsonl")
        .success()
        .stdout(predicate::str::contains("decision: allow"));
}

#[test]
fn directory_transcript_path_fails_fast_instead_of_hanging() {
    // A `BufReader::lines()` read error over a directory (EISDIR) never
    // consumes input, so every subsequent read fails identically -- looping
    // on it (as the old code's `continue` did) never terminates and floods
    // stderr. This must fail fast instead.
    let dir =
        std::env::temp_dir().join(format!("verify-gate-dir-transcript-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_verify-gate"))
        .arg("check")
        .arg(&dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
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
            panic!(
                "verify-gate check hung (or blocked on a full stderr pipe) on a directory transcript path"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    let output = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        !status.success(),
        "expected a non-zero exit for an unreadable transcript path"
    );
    assert!(
        output.stderr.len() < 10_000,
        "expected a single short diagnostic line, got {} bytes of stderr",
        output.stderr.len()
    );
}

#[test]
fn json_format_is_valid_and_sorted_deterministic() {
    let (mut cmd1, home1) = common::isolated_cmd();
    let out1 = cmd1
        .arg("check")
        .arg(fixture("edit_no_verification.jsonl"))
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home1).ok();
    let (mut cmd2, home2) = common::isolated_cmd();
    let out2 = cmd2
        .arg("check")
        .arg(fixture("edit_no_verification.jsonl"))
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home2).ok();
    assert_eq!(
        out1.stdout, out2.stdout,
        "output must be byte-identical across runs"
    );

    let value: serde_json::Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(value["decision"], "block");
    assert_eq!(value["files_edited"][0], "src/widget.rs");
}

/// Streams a transcript well beyond anything that would fit comfortably if
/// the whole file were buffered in memory, and asserts `check` still
/// completes quickly. This is a time-bound proxy for "doesn't hold the
/// whole file in memory", not a direct memory measurement: platform-portable
/// memory ceilings (ulimit -v, cgroups) are unreliable to assert against in
/// a plain `cargo test` run, whereas the code path is a single
/// `BufReader::lines()` loop (see `transcript::evaluate_reader`) that only
/// ever keeps per-line strings and the small classified edit/verify lists
/// alive — so a multi-tens-of-MB file completing in well under a second is
/// strong evidence the reader is not buffering the whole file.
#[test]
fn large_transcript_streams_without_loading_whole_file() {
    let dir = std::env::temp_dir().join(format!("verify-gate-large-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("large.jsonl");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        // ~60MB of filler lines that don't match any tool_use/tool_result shape.
        let filler = serde_json::json!({
            "type": "assistant",
            "isSidechain": false,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "x".repeat(500)}]}
        })
        .to_string();
        let target_bytes: usize = 60 * 1024 * 1024;
        let mut written = 0usize;
        while written < target_bytes {
            writeln!(f, "{filler}").unwrap();
            written += filler.len() + 1;
        }
        // One real edit with no verification after it, so the decision is exercised too.
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "type": "assistant",
                "isSidechain": false,
                "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "Edit", "input": {"file_path": "src/big.rs"}}]}
            })
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "type": "user",
                "isSidechain": false,
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok", "is_error": false}]}
            })
        )
        .unwrap();
    }

    let start = std::time::Instant::now();
    let (mut cmd, home) = common::isolated_cmd();
    cmd.arg("check")
        .arg(&path)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("src/big.rs"));
    let elapsed = start.elapsed();

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "check took too long over a large transcript: {elapsed:?}"
    );
}
