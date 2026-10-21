//! Runs the differential scenarios in `tools/difftest/` — the same operation performed by real
//! `git` and by `rivet`, with transcripts and repository state compared — as part of `cargo test`.
//!
//! The scenarios need `/usr/bin/git` and `bash`; without them the test is skipped, not failed.

use std::path::PathBuf;
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn every_scenario_matches_real_git() {
    if !have("git") || !have("bash") {
        eprintln!("skipping: git and bash are required");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("tools/difftest/scenarios.sh");
    let output = Command::new("bash")
        .arg(&script)
        .env("RIVET", env!("CARGO_BIN_EXE_rivet"))
        .output()
        .expect("run scenarios.sh");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "differential scenarios diverged from git:\n{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("failures: 0"), "unexpected scenario summary:\n{text}");
}

#[test]
fn corrupt_repositories_fail_cleanly_without_panics_or_hangs() {
    if !have("git") || !have("bash") {
        eprintln!("skipping: git and bash are required");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("bash")
        .arg(root.join("tools/difftest/robustness.sh"))
        .env("RIVET", env!("CARGO_BIN_EXE_rivet"))
        .output()
        .expect("run robustness.sh");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success() && text.contains("ROBUST-OK"), "robustness failures:\n{text}");
}
