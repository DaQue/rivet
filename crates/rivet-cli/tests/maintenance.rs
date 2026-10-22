//! `gc` must not destroy anything when the repository is already damaged: a reachable object it
//! cannot read aborts the run before any file is touched. (The happy paths are the differential
//! scenarios `s_gc*` and `s_count_objects`.)

use std::path::Path;
use std::process::{Command, Output};

fn run(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("spawn")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = run("git", dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn loose_files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for fan in std::fs::read_dir(dir.join(".git/objects"))
        .unwrap()
        .flatten()
    {
        let name = fan.file_name().to_string_lossy().into_owned();
        if name.len() == 2 {
            for file in std::fs::read_dir(fan.path()).unwrap().flatten() {
                found.push(format!("{name}/{}", file.file_name().to_string_lossy()));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn gc_with_a_missing_reachable_object_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("a"), "a\n").unwrap();
    std::fs::write(p.join("b"), "b\n").unwrap();
    git(p, &["add", "a", "b"]);
    git(p, &["commit", "-qm", "one"]);
    git(p, &["tag", "t"]);

    let blob = git(p, &["rev-parse", "HEAD:b"]);
    let path = p.join(format!(".git/objects/{}/{}", &blob[..2], &blob[2..]));
    std::fs::remove_file(&path).unwrap();
    let before = loose_files(p);
    let refs_before = std::fs::read(p.join(".git/refs/heads/main")).unwrap();

    let out = run(env!("CARGO_BIN_EXE_rivet"), p, &["gc", "--prune=now"]);
    assert!(
        !out.status.success(),
        "gc must fail on a damaged repository"
    );
    assert_eq!(loose_files(p), before, "no object may be removed or added");
    assert!(
        !p.join(".git/packed-refs").exists(),
        "refs must not have been packed"
    );
    assert_eq!(
        std::fs::read(p.join(".git/refs/heads/main")).unwrap(),
        refs_before
    );
    assert!(
        std::fs::read_dir(p.join(".git/objects/pack"))
            .unwrap()
            .next()
            .is_none()
    );
}
