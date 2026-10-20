//! Compare `merge::merge_trees` + the index/work-tree writers against a real `git merge`.
//!
//! Three trees and a policy table are easy to get plausibly wrong, and a wrong merge is invisible
//! until someone loses work. Each test builds a real divergence, lets Git merge it, records the
//! work tree and `git ls-files -s`, then rewinds and runs Rivet's merge on the identical state.
//! Anything that differs — a file's bytes, its mode, its index stage — is a bug.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_core::hash::ObjectFormat;
use rivet_core::index::Index;
use rivet_core::merge::{
    merge_trees, update_index_after_merge, write_conflict_stages, write_merged_worktree, MergeOptions,
};
use rivet_core::object::Commit;
use rivet_core::repo::Repository;

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
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "rivet-merge-tree-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
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

fn commit(dir: &Path, message: &str) {
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", message]);
}

/// Every file under `root`, skipping `.git`, with its bytes and executable bit.
fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, bool)> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, (Vec<u8>, bool)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let rel = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                walk(base, &path, out);
            } else if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(rel, (target.to_string_lossy().into_owned().into_bytes(), false));
            } else {
                use std::os::unix::fs::PermissionsExt;
                let exec = meta.permissions().mode() & 0o111 != 0;
                out.insert(rel, (std::fs::read(&path).unwrap(), exec));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn index_entries(dir: &Path) -> Vec<(String, u32, String, u8)> {
    let listing = git_ok(dir, &["ls-files", "-s"]);
    let mut rows = Vec::new();
    for line in listing.lines() {
        let (meta, path) = line.split_once('\t').expect("ls-files row");
        let mut parts = meta.split_whitespace();
        let mode = u32::from_str_radix(parts.next().unwrap(), 8).unwrap();
        let oid = parts.next().unwrap().to_string();
        let stage: u8 = parts.next().unwrap().parse().unwrap();
        rows.push((path.to_string(), mode, oid, stage));
    }
    rows.sort();
    rows
}

fn tree_of(repo: &Repository, commit_hex: &str) -> rivet_core::hash::ObjectId {
    let oid = rivet_core::hash::ObjectId::from_hex(ObjectFormat::Sha1, bstr::BStr::new(commit_hex.trim().as_bytes()))
        .unwrap();
    let object = repo.odb().read(&oid).unwrap();
    let parsed = Commit::parse(repo.odb().format(), object.data()).unwrap();
    parsed.tree
}

/// Run a scenario: build history, let Git merge, then rewind and merge with Rivet.
fn run_scenario(tag: &str, expect_conflicts: bool, build: impl Fn(&Path)) {
    if !have_git() {
        return;
    }
    let dir = temp_dir(tag);
    git_ok(&dir, &["init", "-q", "-b", "master"]);
    git_ok(&dir, &["config", "user.email", "test@example.com"]);
    git_ok(&dir, &["config", "user.name", "Test"]);

    build(&dir);
    let base_hex = git_ok(&dir, &["merge-base", "master", "topic"]);
    let ours_hex = git_ok(&dir, &["rev-parse", "master"]);
    let theirs_hex = git_ok(&dir, &["rev-parse", "topic"]);

    // Git's answer.
    let (merged_ok, _) = git(&dir, &["merge", "topic"]);
    assert_eq!(!merged_ok, expect_conflicts, "git merge conflict state differed");
    let expected_files = snapshot(&dir);
    let expected_index = index_entries(&dir);

    // Rewind to the pre-merge state.
    let (in_merge, _) = git(&dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]);
    if in_merge {
        git_ok(&dir, &["merge", "--abort"]);
    } else {
        git_ok(&dir, &["reset", "--hard", "HEAD^"]);
    }
    git_ok(&dir, &["clean", "-fdq"]);
    let _ = git_ok(&dir, &["checkout", "-q", "master"]);

    // Rivet's answer, on the identical starting state.
    let mut repo = Repository::open_or_discover(&dir).expect("open repo");
    let ours_tree = tree_of(&repo, &ours_hex);
    let theirs_tree = tree_of(&repo, &theirs_hex);
    let base_tree = tree_of(&repo, &base_hex);
    let mut options = MergeOptions::new();
    // Git labels the markers with the names the user typed.
    options.labels = (b"HEAD".to_vec().into(), b"topic".to_vec().into());
    let result = merge_trees(repo.odb(), Some(base_tree), ours_tree, theirs_tree, &options)
        .expect("merge runs");

    {
        let mut index = repo.index().clone();
        update_index_after_merge(&mut index, repo.odb(), &result).expect("index");
        write_conflict_stages(&mut index, &result.conflicts).expect("stages");
        write_merged_worktree(repo.worktree().expect("worktree"), &index, repo.odb(), &result, &options)
            .expect("worktree");
        let ours: Vec<(String, u32, String, u8)> = index
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.path.to_string(),
                    entry.mode.to_u32(),
                    entry.oid.to_hex().to_string(),
                    entry.stage() as u8,
                )
            })
            .collect();
        let mut ours = ours;
        ours.sort();
        if result.clean {
            assert!(result.tree.is_some(), "a clean merge must produce a tree");
        }
        assert_eq!(ours, expected_index, "index differs from git for {tag}");
    }

    assert_eq!(snapshot(&dir), expected_files, "work tree differs from git for {tag}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clean_merge_of_non_overlapping_edits_matches_git() {
    run_scenario("clean", false, |dir| {
        write(dir, "a.txt", b"one\ntwo\nthree\nfour\nfive\n");
        write(dir, "keep.txt", b"same\n");
        commit(dir, "base");
        git_ok(dir, &["checkout", "-q", "-b", "topic"]);
        write(dir, "a.txt", b"one\ntwo\nthree\nfour\nFIVE\n");
        commit(dir, "theirs");
        git_ok(dir, &["checkout", "-q", "master"]);
        write(dir, "a.txt", b"one\nTWO\nthree\nfour\nfive\n");
        write(dir, "added-by-ours.txt", b"ours\n");
        commit(dir, "ours");
    });
}

#[test]
fn conflicting_edits_match_git() {
    run_scenario("conflict", true, |dir| {
        write(dir, "a.txt", b"one\ntwo\nthree\n");
        commit(dir, "base");
        git_ok(dir, &["checkout", "-q", "-b", "topic"]);
        write(dir, "a.txt", b"one\nTHEIRS\nthree\n");
        commit(dir, "theirs");
        git_ok(dir, &["checkout", "-q", "master"]);
        write(dir, "a.txt", b"one\nOURS\nthree\n");
        commit(dir, "ours");
    });
}

#[test]
fn add_add_conflict_matches_git() {
    run_scenario("addadd", true, |dir| {
        write(dir, "base.txt", b"base\n");
        commit(dir, "base");
        git_ok(dir, &["checkout", "-q", "-b", "topic"]);
        write(dir, "new.txt", b"theirs\n");
        commit(dir, "theirs");
        git_ok(dir, &["checkout", "-q", "master"]);
        write(dir, "new.txt", b"ours\n");
        commit(dir, "ours");
    });
}

#[test]
fn modify_delete_conflict_matches_git() {
    run_scenario("moddel", true, |dir| {
        write(dir, "gone.txt", b"one\ntwo\n");
        write(dir, "stay.txt", b"stay\n");
        commit(dir, "base");
        git_ok(dir, &["checkout", "-q", "-b", "topic"]);
        write(dir, "gone.txt", b"one\nTWO\n");
        commit(dir, "theirs");
        git_ok(dir, &["checkout", "-q", "master"]);
        git_ok(dir, &["rm", "-q", "gone.txt"]);
        commit(dir, "ours");
    });
}

#[test]
fn both_sides_deleting_the_same_file_is_clean() {
    run_scenario("bothdel", false, |dir| {
        write(dir, "gone.txt", b"one\n");
        write(dir, "stay.txt", b"stay\n");
        commit(dir, "base");
        git_ok(dir, &["checkout", "-q", "-b", "topic"]);
        git_ok(dir, &["rm", "-q", "gone.txt"]);
        commit(dir, "theirs");
        git_ok(dir, &["checkout", "-q", "master"]);
        git_ok(dir, &["rm", "-q", "gone.txt"]);
        commit(dir, "ours");
    });
}
