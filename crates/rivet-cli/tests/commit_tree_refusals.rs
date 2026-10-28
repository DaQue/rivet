//! `commit-tree` refuses `-S` and a non-UTF-8 `i18n.commitEncoding` by name and writes no
//! object. Git signs, or adds an `encoding` header, so there is no differential scenario for the
//! refusal; the rest of `commit-tree` is the scenario `s_commit_tree`.

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rivet"))
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

/// Loose object files under `.git/objects`, which is all `write-tree` and `commit-tree` make.
fn object_count(dir: &Path) -> usize {
    let mut count = 0;
    for fan in std::fs::read_dir(dir.join(".git/objects"))
        .unwrap()
        .flatten()
    {
        if fan.file_name().len() == 2 {
            count += std::fs::read_dir(fan.path()).unwrap().count();
        }
    }
    count
}

#[test]
fn signing_and_foreign_encodings_are_refused_by_name_and_write_nothing() {
    let dir = std::env::temp_dir().join(format!("rivet-commit-tree-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(run(&dir, &["init", "-q"]).status.success());
    let tree = run(&dir, &["write-tree"]);
    let tree = String::from_utf8(tree.stdout).unwrap().trim().to_string();
    assert_eq!(tree.len(), 40, "write-tree must print an id");

    let before = object_count(&dir);
    assert_eq!(before, 1, "only the empty tree exists so far");
    let signed = run(&dir, &["commit-tree", &tree, "-S", "-m", "x"]);
    assert_eq!(signed.status.code(), Some(128));
    assert!(
        String::from_utf8_lossy(&signed.stderr).contains("signing (-S) is not supported"),
        "{}",
        String::from_utf8_lossy(&signed.stderr)
    );

    assert!(
        run(&dir, &["config", "i18n.commitEncoding", "latin1"])
            .status
            .success()
    );
    let encoded = run(&dir, &["commit-tree", &tree, "-m", "x"]);
    assert_eq!(encoded.status.code(), Some(128));
    assert!(
        String::from_utf8_lossy(&encoded.stderr)
            .contains("i18n.commitEncoding other than UTF-8 is not supported"),
        "{}",
        String::from_utf8_lossy(&encoded.stderr)
    );
    assert_eq!(object_count(&dir), before);
    let _ = std::fs::remove_dir_all(&dir);
}
