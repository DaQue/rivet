//! `clone --filter` cases that would need a real filtered transfer are refused by name, never
//! cloned in full while claiming to be partial. (The cases that work are `s_clone_filter`.)

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "n")
        .env("GIT_AUTHOR_EMAIL", "n@e")
        .env("GIT_COMMITTER_NAME", "n")
        .env("GIT_COMMITTER_EMAIL", "n@e")
        .output()
        .expect("spawn")
}

fn git(dir: &Path, args: &[&str]) {
    let out = run("/usr/bin/git", dir, args);
    assert!(out.status.success(), "{args:?}: {out:?}");
}

fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "src"]);
    std::fs::write(dir.join("src/a"), "a\n").unwrap();
    git(&dir.join("src"), &["add", "a"]);
    git(&dir.join("src"), &["commit", "-q", "-m", "one"]);
    dir
}

fn refused(dir: &Path, args: &[&str], needle: &str) {
    let rivet = env!("CARGO_BIN_EXE_rivet");
    let mut full = vec!["clone"];
    full.extend_from_slice(args);
    let out = run(rivet, dir, &full);
    assert_eq!(out.status.code(), Some(128), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(needle), "{stderr}");
    assert!(
        !dir.join("dest").exists(),
        "a refused clone left a directory"
    );
}

#[test]
fn a_source_that_honours_filters_is_refused_and_leaves_nothing_behind() {
    let dir = fixture("clone-filter-allow");
    git(
        &dir.join("src"),
        &["config", "uploadpack.allowFilter", "true"],
    );
    let url = format!("file://{}", dir.join("src").display());
    refused(
        &dir,
        &["--filter=blob:none", &url, "dest"],
        "--filter against this source is not implemented",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bundle_source_is_refused() {
    let dir = fixture("clone-filter-bundle");
    git(
        &dir.join("src"),
        &["bundle", "create", "../b.bundle", "--all"],
    );
    refused(
        &dir,
        &["--filter=blob:none", "b.bundle", "dest"],
        "--filter against this source is not implemented",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_valid_spec_rivet_cannot_honour_is_refused_by_name() {
    let dir = fixture("clone-filter-combine");
    refused(
        &dir,
        &["--filter=combine:blob:none+tree:0", "src", "dest"],
        "--filter=combine:blob:none+tree:0 is not implemented",
    );
    let _ = std::fs::remove_dir_all(&dir);
}
