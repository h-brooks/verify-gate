//! Shared test-isolation helpers.
//!
//! `Config::load` falls back to the *developer's real* `~/.config/verify-gate/config.toml`
//! and, for `check`, the process's real cwd. Left alone, every integration test
//! silently depends on whatever happens to be on the machine running `cargo test`.
//! Every test that spawns the binary must isolate both HOME and cwd through
//! `isolated_cmd`, pointing them at a fresh directory guaranteed to contain no
//! `.verify-gate.toml` of any kind.
#![allow(dead_code)]

use assert_cmd::Command;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh, empty directory, unique per call even across tests running in
/// parallel within the same process (pid + call counter).
pub fn isolated_home() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("verify-gate-isolated-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `verify-gate` command with HOME and cwd both pointed at a fresh, empty
/// directory, so neither the real developer HOME nor the crate's own cwd can
/// leak a `.verify-gate.toml` into the test. Caller is responsible for
/// removing the returned directory when done (or writing a test config into
/// it first, to test config loading deliberately).
pub fn isolated_cmd() -> (Command, PathBuf) {
    let home = isolated_home();
    let mut cmd = Command::cargo_bin("verify-gate").unwrap();
    cmd.env("HOME", &home).current_dir(&home);
    (cmd, home)
}
