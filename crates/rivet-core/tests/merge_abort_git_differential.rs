//! `merge --abort` must restore exactly what Git restores.
//!
//! The abort is a state-machine operation with three moving parts — the work tree, the index and
//! the `MERGE_*` files — and getting any one of them wrong leaves a repository that reports
//! changes nobody made. The test runs Git's abort, records the result, replays the conflicting
//! merge and runs Rivet's abort on the same state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_core::merge::abort_merge;

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .output()
        .expect("git runs");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let (ok, out) = git(dir, args);
    assert!(ok, "git {:?} failed", args);
    out
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-merge-abort-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, path: &str, data: &[u8]) {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, data).unwrap();
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let rel = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(base, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn index_rows(dir: &Path) -> Vec<(String, String, u8)> {
    let listing = git_ok(dir, &["ls-files", "-s"]);
    let mut rows: Vec<(String, String, u8)> = listing
        .lines()
        .map(|line| {
            let (meta, path) = line.split_once('\t').unwrap();
            let mut parts = meta.split_whitespace();
            let _mode = parts.next().unwrap();
            let oid = parts.next().unwrap().to_string();
            let stage: u8 = parts.next().unwrap().parse().unwrap();
            (path.to_string(), oid, stage)
        })
        .collect();
    rows.sort();
    rows
}

fn build_conflicting_repo(dir: &Path) {
    git_ok(dir, &["init", "-q", "-b", "master"]);
    git_ok(dir, &["config", "user.email", "test@example.com"]);
    git_ok(dir, &["config", "user.name", "Test"]);
    write(dir, "a.txt", b"one\ntwo\nthree\n");
    write(dir, "keep.txt", b"keep\n");
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", "base"]);
    git_ok(dir, &["checkout", "-q", "-b", "topic"]);
    write(dir, "a.txt", b"one\nTHEIRS\nthree\n");
    write(dir, "theirs-only.txt", b"theirs\n");
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", "theirs"]);
    git_ok(dir, &["checkout", "-q", "master"]);
    write(dir, "a.txt", b"one\nOURS\nthree\n");
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", "ours"]);
}

#[test]
fn abort_restores_worktree_and_index_like_git() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("worktree");
    build_conflicting_repo(&dir);

    // Git's abort.
    let (ok, _) = git(&dir, &["merge", "topic"]);
    assert!(!ok, "the scenario must conflict");
    git_ok(&dir, &["merge", "--abort"]);
    let expected_files = snapshot(&dir);
    let expected_index = index_rows(&dir);

    // Rivet's abort, from the same conflicting state.
    let (ok, _) = git(&dir, &["merge", "topic"]);
    assert!(!ok, "the scenario must conflict");
    let format = rivet_core::hash::ObjectFormat::Sha1;
    let git_dir = dir.join(".git");
    let mut index = rivet_core::index::Index::load(&git_dir.join("index"), format).expect("index");
    let odb = rivet_core::odb::Odb::open(&git_dir, format).expect("odb");
    let config = rivet_core::config::Config::default();
    let worktree = rivet_core::worktree::Worktree::new(Some(&dir), format, &config).expect("wt");
    abort_merge(&worktree, &mut index, &odb, &git_dir).expect("abort runs");

    assert_eq!(snapshot(&dir), expected_files, "work tree differs after abort");
    assert_eq!(index_rows(&dir), expected_index, "index differs after abort");
    assert!(!dir.join(".git/MERGE_HEAD").exists(), "MERGE_HEAD left behind");
    assert!(!dir.join(".git/MERGE_MSG").exists(), "MERGE_MSG left behind");
    let _ = std::fs::remove_dir_all(&dir);
}
