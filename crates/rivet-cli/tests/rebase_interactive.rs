//! What `rebase --interactive` refuses, and that a refusal never costs a commit. Everything it does
//! implement is compared with real Git by the `s_rebase_i*` scenarios.

use std::path::Path;
use std::process::{Command, Output};

fn run(program: &str, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_EDITOR", ":")
        .envs(env.iter().copied())
        .output()
        .expect("spawn")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("git", dir, args, &[]);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn rivet(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    run(env!("CARGO_BIN_EXE_rivet"), dir, args, env)
}

/// `main` has moved on from `base`; `topic` holds two commits on top of `base`.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("f"), "1\n").unwrap();
    git(p, &["add", "f"]);
    git(p, &["commit", "-qm", "base"]);
    git(p, &["switch", "-q", "-c", "topic"]);
    for name in ["one", "two"] {
        std::fs::write(p.join(name), name).unwrap();
        git(p, &["add", name]);
        git(p, &["commit", "-qm", name]);
    }
    git(p, &["switch", "-q", "main"]);
    std::fs::write(p.join("m"), "m\n").unwrap();
    git(p, &["add", "m"]);
    git(p, &["commit", "-qm", "main moves"]);
    git(p, &["switch", "-q", "topic"]);
    dir
}

fn assert_untouched(p: &Path, head: &str) {
    assert_eq!(git(p, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(p, &["symbolic-ref", "HEAD"]), "refs/heads/topic");
    assert!(!p.join(".git/rebase-merge").exists());
    assert_eq!(git(p, &["status", "--porcelain"]), "");
}

#[test]
fn options_that_are_not_implemented_are_refused_by_name() {
    let dir = repo();
    let p = dir.path();
    let head = git(p, &["rev-parse", "HEAD"]);
    for (args, name) in [
        (
            vec!["rebase", "--exec", "true", "main"],
            "--exec without -i",
        ),
        (vec!["rebase", "-x", "true", "main"], "--exec without -i"),
        (
            vec!["rebase", "-i", "--rebase-merges", "main"],
            "--rebase-merges",
        ),
        (
            vec!["rebase", "--autosquash", "main"],
            "--autosquash without -i",
        ),
        (vec!["rebase", "-i", "-S", "main"], "--gpg-sign"),
    ] {
        let out = rivet(p, &args, &[]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(128), "{args:?}: {err}");
        assert!(
            err.contains(&format!("rebase {name} is not implemented")),
            "{args:?}: {err}"
        );
        assert_untouched(p, &head);
    }
}

#[test]
fn todo_commands_that_are_not_implemented_are_refused_before_anything_moves() {
    let dir = repo();
    let p = dir.path();
    let head = git(p, &["rev-parse", "HEAD"]);
    let (edit, name) = ("1i merge topic", "merge");
    let script = p.join(".git/seq-edit.sh");
    std::fs::write(&script, format!("#!/bin/sh\nsed -i -e '{edit}' \"$1\"\n")).unwrap();
    let out = rivet(
        p,
        &["rebase", "-i", "main"],
        &[("GIT_SEQUENCE_EDITOR", &format!("sh {}", script.display()))],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(128), "{edit}: {err}");
    assert!(err.contains("is not implemented"), "{edit}: {err}");
    assert!(err.contains(name), "{edit}: {err}");
    assert_untouched(p, &head);
}

#[test]
fn an_empty_message_puts_the_branch_back_where_it_was() {
    let dir = repo();
    let p = dir.path();
    let head = git(p, &["rev-parse", "HEAD"]);
    let seq = p.join(".git/seq-reword.sh");
    std::fs::write(&seq, "#!/bin/sh\nsed -i -e '1s/^pick/reword/' \"$1\"\n").unwrap();
    let editor = p.join(".git/empty-editor.sh");
    std::fs::write(&editor, "#!/bin/sh\n: >\"$1\"\n").unwrap();
    let out = rivet(
        p,
        &["rebase", "-i", "main"],
        &[
            ("GIT_SEQUENCE_EDITOR", &format!("sh {}", seq.display())),
            ("GIT_EDITOR", &format!("sh {}", editor.display())),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("empty commit message"));
    assert_untouched(p, &head);
    assert_eq!(git(p, &["log", "--format=%s", "main..topic"]), "two\none");
}

#[test]
fn autosquash_cases_that_need_fixup_c_or_nested_chains_are_refused_before_anything_moves() {
    for (subjects, expect) in [
        (&["amend! one"][..], "needs 'fixup -C'"),
        (&["fixup! one", "fixup! fixup! one", "squash! two"][..], ""),
    ] {
        let dir = repo();
        let p = dir.path();
        for (n, subject) in subjects.iter().enumerate() {
            std::fs::write(p.join(format!("x{n}")), "x").unwrap();
            git(p, &["add", "."]);
            git(p, &["commit", "-qm", subject]);
        }
        let head = git(p, &["rev-parse", "HEAD"]);
        let out = rivet(p, &["rebase", "-i", "--autosquash", "main"], &[]);
        let err = String::from_utf8_lossy(&out.stderr);
        if expect.is_empty() {
            // Fully supported: runs to the end with no stop.
            assert_eq!(out.status.code(), Some(0), "{subjects:?}: {err}");
            assert_eq!(git(p, &["log", "--format=%s", "-1"]), "two");
            continue;
        }
        assert_eq!(out.status.code(), Some(128), "{subjects:?}: {err}");
        assert!(err.contains(expect), "{subjects:?}: {err}");
        assert_untouched(p, &head);
    }
}

#[test]
fn autosquash_onto_a_fixup_commit_is_refused_before_anything_moves() {
    let dir = repo();
    let p = dir.path();
    std::fs::write(p.join("x0"), "x").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", "fixup! one"]);
    let target = git(p, &["rev-parse", "--short", "HEAD"]);
    std::fs::write(p.join("x1"), "x").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-qm", &format!("fixup! {target}")]);
    let head = git(p, &["rev-parse", "HEAD"]);
    let out = rivet(p, &["rebase", "-i", "--autosquash", "main"], &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(128), "{err}");
    assert!(err.contains("targets the fixup commit"), "{err}");
    assert_untouched(p, &head);
}
