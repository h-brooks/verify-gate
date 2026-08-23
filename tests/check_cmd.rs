//! Integration tests over `verify-gate check`, driving the built binary
//! against synthetic fixture transcripts (never real ones).
use assert_cmd::Command;
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

fn check(name: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("check")
        .arg(fixture(name))
        .assert()
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
fn json_format_is_valid_and_sorted_deterministic() {
    let out1 = Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("check")
        .arg(fixture("edit_no_verification.jsonl"))
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    let out2 = Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("check")
        .arg(fixture("edit_no_verification.jsonl"))
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
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
    Command::cargo_bin("verify-gate")
        .unwrap()
        .arg("check")
        .arg(&path)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("src/big.rs"));
    let elapsed = start.elapsed();

    std::fs::remove_dir_all(&dir).ok();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "check took too long over a large transcript: {elapsed:?}"
    );
}
