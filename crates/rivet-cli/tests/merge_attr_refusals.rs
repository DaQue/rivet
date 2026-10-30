//! The `merge` and `conflict-marker-size` attributes are refused by name where git would act on
//! them (a three-way content merge, a content conflict), and the refusal changes nothing; the
//! values git treats as the default merge the way git does.

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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("/usr/bin/git", dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository where `main` and `side` both rewrite line three of `f.txt`, with `attributes`
/// committed on `main`.
fn fixture(name: &str, attributes: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-merge-attr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "base"]);
    git(&dir, &["checkout", "-qb", "side"]);
    std::fs::write(dir.join("f.txt"), "a\nb\nSIDE\nd\ne\n").unwrap();
    git(&dir, &["commit", "-qam", "side"]);
    git(&dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("f.txt"), "a\nb\nMAIN\nd\ne\n").unwrap();
    std::fs::write(dir.join(".gitattributes"), attributes).unwrap();
    git(&dir, &["add", "f.txt", ".gitattributes"]);
    git(&dir, &["commit", "-qm", "main"]);
    dir
}

fn copy(dir: &Path, to: &str) -> PathBuf {
    let target = dir.with_file_name(format!(
        "{}-{to}",
        dir.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&target);
    assert!(
        Command::new("cp")
            .arg("-a")
            .arg(dir)
            .arg(&target)
            .status()
            .unwrap()
            .success()
    );
    target
}

fn refused(attributes: &str, name: &str, needle: &str) {
    let dir = fixture(name, attributes);
    git(&dir, &["config", "merge.mine.driver", "true"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let before = std::fs::read(dir.join("f.txt")).unwrap();
    let out = run(env!("CARGO_BIN_EXE_rivet"), &dir, &["merge", "side"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{attributes}: {err}");
    assert!(err.contains(needle), "{attributes}: {err}");
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(dir.join("f.txt")).unwrap(), before);
    assert!(!dir.join(".git/MERGE_HEAD").exists(), "{attributes}");
}

#[test]
fn merge_drivers_and_marker_sizes_are_refused() {
    refused("f.txt merge=union\n", "union", "merge=union");
    refused("f.txt merge=binary\n", "binary", "merge=binary");
    refused("f.txt -merge\n", "unset", "-merge");
    refused("f.txt merge=mine\n", "driver", "merge=mine");
    refused(
        "f.txt conflict-marker-size=11\n",
        "size",
        "conflict-marker-size=11",
    );
}

#[test]
fn default_values_merge_like_git() {
    // `merge=mine` names no configured driver here, which git merges as text.
    for attributes in [
        "f.txt merge=text\n",
        "f.txt merge\n",
        "f.txt conflict-marker-size=7\n",
        "f.txt merge=mine\n",
    ] {
        let dir = fixture("default", attributes);
        let theirs = copy(&dir, "git");
        let ours = run(env!("CARGO_BIN_EXE_rivet"), &dir, &["merge", "side"]);
        let real = run("/usr/bin/git", &theirs, &["merge", "side"]);
        assert_eq!(ours.status.code(), real.status.code(), "{attributes}");
        assert_eq!(
            std::fs::read(dir.join("f.txt")).unwrap(),
            std::fs::read(theirs.join("f.txt")).unwrap(),
            "{attributes}"
        );
    }
}
