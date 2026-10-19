//! Compare `worktree::status` against real `git status --porcelain`.
//!
//! These tests are the only evidence that the status comparison is right: the shapes of
//! `StatusEntry` and the untracked lists are easy to satisfy with plausible-looking code that
//! reports the wrong classification. Every expectation is read back from `git` itself, never
//! written by hand.

use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_core::repo::Repository;
use rivet_core::worktree::status::{compute, StatusOptions, UntrackedMode};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, contents).expect("write");
}

/// A repository with one commit, plus a mixture of untracked, ignored and untracked-directory
/// paths for the comparisons below.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "T"]);
    write(&root.join(".gitignore"), "ignored.log\nbuild/\n");
    write(&root.join("tracked.txt"), "tracked\n");
    write(&root.join("tracked_b.txt"), "b\n");
    write(&root.join("tracked_c.txt"), "c\n");
    write(&root.join("exec.txt"), "#!/bin/sh\n");
    write(&root.join("src/tracked.rs"), "fn main() {}\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "initial"]);
    (dir, root)
}

/// The untracked and ignored lists `rivet` computes, rendered the way Git renders them.
fn rivet_paths(root: &Path, options: &StatusOptions) -> (Vec<String>, Vec<String>) {
    let repo = Repository::open_or_discover(root).expect("open");
    let head_tree = repo
        .head_tree()
        .expect("head tree")
        .map(|tree| tree.id(repo.odb().format()))
        .transpose()
        .expect("tree id");
    let status = compute(
        repo.worktree().expect("worktree"),
        repo.index(),
        head_tree,
        repo.odb(),
        repo.ignore().expect("ignore"),
        options,
    )
    .expect("status");
    let render = |paths: &[bstr::BString]| {
        paths
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
    };
    (render(&status.untracked), render(&status.ignored))
}

/// The `??` or `!!` lines of `git status --porcelain`, with the prefix removed.
fn git_paths(root: &Path, extra: &[&str], marker: &str) -> Vec<String> {
    let mut args = vec!["status", "--porcelain"];
    args.extend_from_slice(extra);
    git(root, &args)
        .lines()
        .filter(|line| line.starts_with(marker))
        .map(|line| line[3..].to_string())
        .collect()
}

#[test]
fn untracked_normal_matches_git() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("new.txt"), "x\n");
    write(&root.join("untracked_dir/a.txt"), "a\n");
    write(&root.join("untracked_dir/b.txt"), "b\n");
    write(&root.join("src/untracked.rs"), "fn f() {}\n");
    write(&root.join("ignored.log"), "noise\n");
    write(&root.join("build/out.o"), "binary\n");

    let mut options = StatusOptions::new();
    options.untracked(UntrackedMode::Normal);
    let (untracked, ignored) = rivet_paths(&root, &options);

    assert_eq!(untracked, git_paths(&root, &[], "?? "));
    assert!(ignored.is_empty(), "ignored requires the option: {ignored:?}");
    // The untracked directory is reported once, with the trailing slash Git uses.
    assert!(untracked.contains(&"untracked_dir/".to_string()), "{untracked:?}");
    assert!(!untracked.iter().any(|p| p.starts_with("untracked_dir/a")), "{untracked:?}");
}

#[test]
fn untracked_all_matches_git() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("new.txt"), "x\n");
    write(&root.join("untracked_dir/a.txt"), "a\n");
    write(&root.join("untracked_dir/b.txt"), "b\n");
    write(&root.join("src/untracked.rs"), "fn f() {}\n");

    let mut options = StatusOptions::new();
    options.untracked(UntrackedMode::All);
    let (untracked, _) = rivet_paths(&root, &options);

    assert_eq!(untracked, git_paths(&root, &["-uall"], "?? "));
    assert!(untracked.contains(&"untracked_dir/a.txt".to_string()), "{untracked:?}");
    assert!(!untracked.iter().any(|p| p.ends_with('/')), "{untracked:?}");
}

#[test]
fn untracked_none_reports_nothing() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("new.txt"), "x\n");

    let mut options = StatusOptions::new();
    options.untracked(UntrackedMode::No);
    let (untracked, _) = rivet_paths(&root, &options);

    assert!(untracked.is_empty(), "{untracked:?}");
}

#[test]
fn ignored_list_matches_git() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("ignored.log"), "noise\n");
    write(&root.join("build/out.o"), "binary\n");
    write(&root.join("new.txt"), "x\n");

    let mut options = StatusOptions::new();
    options.ignored(true);
    let (untracked, ignored) = rivet_paths(&root, &options);

    assert_eq!(ignored, git_paths(&root, &["--ignored"], "!! "));
    // An ignore rule does not hide an untracked path from the untracked list; it removes it
    // from that list only by matching it.
    assert_eq!(untracked, git_paths(&root, &[], "?? "));
}

#[test]
fn nested_gitignore_rules_apply_while_descending() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("src/.gitignore"), "*.log\n");
    git(&root, &["add", "src/.gitignore"]);
    git(&root, &["commit", "-qm", "nested ignore"]);
    write(&root.join("src/debug.log"), "noise\n");
    write(&root.join("src/kept.rs"), "fn k() {}\n");

    let mut options = StatusOptions::new();
    options.ignored(true);
    let (untracked, ignored) = rivet_paths(&root, &options);

    assert_eq!(ignored, git_paths(&root, &["--ignored"], "!! "));
    assert_eq!(untracked, git_paths(&root, &[], "?? "));
    assert!(ignored.contains(&"src/debug.log".to_string()), "{ignored:?}");
    assert!(!untracked.contains(&"src/debug.log".to_string()), "{untracked:?}");
}

#[test]
fn assume_valid_and_skip_worktree_are_not_reported() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    git(&root, &["update-index", "--assume-unchanged", "tracked_b.txt"]);
    git(&root, &["update-index", "--skip-worktree", "tracked_c.txt"]);
    write(&root.join("tracked_b.txt"), "changed but promised\n");
    write(&root.join("tracked_c.txt"), "changed but sparse\n");

    // Git reports neither path, and neither may Rivet.
    let porcelain = git(&root, &["status", "--porcelain"]);
    assert_eq!(porcelain, "", "git precondition: {porcelain}");

    let options = StatusOptions::new();
    let (staged, unstaged) = rivet_classifications(&root, &options);
    assert!(staged.is_empty(), "{staged:?}");
    assert!(unstaged.is_empty(), "{unstaged:?}");
}

/// `(code, path)` for each entry, in the order `compute` produced them.
fn rivet_classifications(
    root: &Path,
    options: &StatusOptions,
) -> (Vec<(char, String)>, Vec<(char, String)>) {
    let repo = Repository::open_or_discover(root).expect("open");
    let head_tree = repo
        .head_tree()
        .expect("head tree")
        .map(|tree| tree.id(repo.odb().format()))
        .transpose()
        .expect("tree id");
    let status = compute(
        repo.worktree().expect("worktree"),
        repo.index(),
        head_tree,
        repo.odb(),
        repo.ignore().expect("ignore"),
        options,
    )
    .expect("status");
    let render = |entries: &[rivet_core::worktree::status::StatusEntry]| {
        entries
            .iter()
            .map(|e| (e.kind.code(), String::from_utf8_lossy(&e.path).into_owned()))
            .collect::<Vec<_>>()
    };
    (render(&status.staged), render(&status.unstaged))
}

#[test]
fn staged_and_unstaged_classifications_match_git() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    // One change of each class, set up with Git itself so the expectation is Git's own.
    write(&root.join("tracked.txt"), "modified\n");
    std::fs::remove_file(root.join("tracked_b.txt")).expect("rm");
    git(&root, &["rm", "-q", "tracked_c.txt"]);
    write(&root.join("added.txt"), "new\n");
    git(&root, &["add", "added.txt"]);
    let mut perms = std::fs::metadata(root.join("exec.txt")).expect("stat").permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(root.join("exec.txt"), perms).expect("chmod");

    // Git's own answer, rewritten as (index code, work-tree code, path).
    let porcelain = git(&root, &["status", "--porcelain"]);
    let mut expected_staged: Vec<(char, String)> = Vec::new();
    let mut expected_unstaged: Vec<(char, String)> = Vec::new();
    for line in porcelain.lines() {
        let code = line[..1].chars().next().unwrap();
        let work = line[1..2].chars().next().unwrap();
        let path = line[3..].to_string();
        if code != ' ' && code != '?' {
            expected_staged.push((code, path.clone()));
        }
        if work != ' ' && work != '?' {
            expected_unstaged.push((work, path));
        }
    }
    expected_staged.sort();
    expected_unstaged.sort();

    let options = StatusOptions::new();
    let (mut staged, mut unstaged) = rivet_classifications(&root, &options);
    staged.sort();
    unstaged.sort();

    assert_eq!(staged, expected_staged, "porcelain: {porcelain}");
    // Git reports a mode-only change as a modification; `ChangeKind` deliberately reports it as
    // a type change, which the module documents as the one deviation from `git status`.
    assert_eq!(
        unstaged,
        vec![
            ('D', "tracked_b.txt".to_string()),
            ('M', "tracked.txt".to_string()),
            ('T', "exec.txt".to_string()),
        ],
        "porcelain: {porcelain}"
    );
}

