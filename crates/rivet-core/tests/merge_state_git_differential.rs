//! The merge state files must be the ones Git itself writes and reads.
//!
//! `MERGE_HEAD`, `MERGE_MSG` and `ORIG_HEAD` are a format shared with `git`: a merge started by
//! Rivet has to be visible to `git status`, resumable by `git commit`, and abortable by
//! `git merge --abort`. The test writes the state with Rivet and asks Git to read it.

use std::path::{Path, PathBuf};
use std::process::Command;

use bstr::BString;

use rivet_core::hash::{ObjectFormat, ObjectId};
use rivet_core::merge::{MergeState, read_merge_state, write_merge_state};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
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

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("rivet-merge-state-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(ObjectFormat::Sha1, bstr::BStr::new(hex.trim().as_bytes())).expect("oid")
}

#[test]
fn written_state_is_readable_by_git_and_by_rivet() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("roundtrip");
    git(&dir, &["init", "-q", "-b", "master"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(dir.join("a.txt"), b"two\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "theirs"]);
    git(&dir, &["checkout", "-q", "master"]);
    std::fs::write(dir.join("a.txt"), b"one\nmore\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "ours"]);

    let ours = git(&dir, &["rev-parse", "master"]);
    let theirs = git(&dir, &["rev-parse", "topic"]);
    let git_dir = dir.join(".git");
    let format = ObjectFormat::Sha1;

    let mut state = MergeState::new();
    state.merge_head.push(oid(&theirs));
    state.orig_head = Some(oid(&ours));
    state.head_commit = Some(oid(&ours));
    state.merge_msg = BString::from("Merge branch 'topic'");
    write_merge_state(&git_dir, &state).expect("write state");

    // Git reads the same files.
    assert_eq!(
        git(&dir, &["rev-parse", "MERGE_HEAD"]).trim(),
        theirs.trim()
    );
    assert_eq!(git(&dir, &["rev-parse", "ORIG_HEAD"]).trim(), ours.trim());
    let msg = std::fs::read_to_string(git_dir.join("MERGE_MSG")).expect("MERGE_MSG");
    assert!(
        msg.contains("Merge branch 'topic'"),
        "MERGE_MSG was {msg:?}"
    );

    // And Rivet reads back what it wrote.
    let read = read_merge_state(&git_dir, format)
        .expect("read")
        .expect("some");
    assert_eq!(read.merge_head, vec![oid(&theirs)]);
    assert_eq!(read.orig_head, Some(oid(&ours)));
    assert!(read.merge_msg.starts_with(b"Merge branch 'topic'"));

    let _ = std::fs::remove_dir_all(&dir);
}
