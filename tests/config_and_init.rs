//! Tests for `.verify-gate.toml` loading (via `check`, run with a temp cwd)
//! and the `init` subcommand.
use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "verify-gate-test-{label}-{}-{}",
        std::process::id(),
        label.len() // cheap uniqueness nudge alongside pid
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn custom_min_edits_allows_below_threshold() {
    let dir = temp_dir("min-edits");
    std::fs::write(dir.join(".verify-gate.toml"), "min_edits = 3\n").unwrap();

    Command::cargo_bin("verify-gate")
        .unwrap()
        .current_dir(&dir)
        .arg("check")
        .arg(fixture("two_edits_no_verification.jsonl"))
        .assert()
        .success()
        .stdout(predicate::str::contains("decision: allow"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn custom_ignore_paths_exempts_extra_extension() {
    let dir = temp_dir("ignore-paths");
    std::fs::write(
        dir.join(".verify-gate.toml"),
        r#"ignore_paths = ["**/*.rs"]"#,
    )
    .unwrap();

    // edit_no_verification.jsonl edits src/widget.rs; with *.rs ignored,
    // there are no qualifying edits left at all.
    Command::cargo_bin("verify-gate")
        .unwrap()
        .current_dir(&dir)
        .arg("check")
        .arg(fixture("edit_no_verification.jsonl"))
        .assert()
        .success()
        .stdout(predicate::str::contains("decision: allow"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_config_falls_back_to_defaults() {
    let dir = temp_dir("bad-config");
    std::fs::write(dir.join(".verify-gate.toml"), "not valid toml [[[").unwrap();

    // Falls back to defaults, so the plain unverified-edit case still blocks.
    Command::cargo_bin("verify-gate")
        .unwrap()
        .current_dir(&dir)
        .arg("check")
        .arg(fixture("edit_no_verification.jsonl"))
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("failed to parse config"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn init_writes_config_and_prints_settings_snippet() {
    let dir = temp_dir("init");

    Command::cargo_bin("verify-gate")
        .unwrap()
        .current_dir(&dir)
        .arg("init")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("wrote .verify-gate.toml")
                .and(predicate::str::contains("\"Stop\""))
                .and(predicate::str::contains("verify-gate hook")),
        );

    let written = std::fs::read_to_string(dir.join(".verify-gate.toml")).unwrap();
    assert!(written.contains("min_edits"));

    // Running init again must not clobber an existing file.
    std::fs::write(dir.join(".verify-gate.toml"), "min_edits = 99\n").unwrap();
    Command::cargo_bin("verify-gate")
        .unwrap()
        .current_dir(&dir)
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("already exists"));
    let unchanged = std::fs::read_to_string(dir.join(".verify-gate.toml")).unwrap();
    assert_eq!(unchanged, "min_edits = 99\n");

    std::fs::remove_dir_all(&dir).ok();
}
