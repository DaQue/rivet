//! A `filter=` driver, `ident` and `working-tree-encoding` are refused by name wherever git would
//! run the conversion — including the paths that used to skip it (stash, the work tree side of
//! `diff` and `blame`, conflicted merge results) — and the refusal changes nothing.

use std::path::Path;
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

fn rivet(dir: &Path, args: &[&str]) -> Output {
    run(env!("CARGO_BIN_EXE_rivet"), dir, args)
}

fn refused(out: &Output, what: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{what}: {err}");
    assert!(
        err.contains("filter=cat") || err.contains("`filter=cat`"),
        "{what}: {err}"
    );
}

#[test]
fn the_paths_that_skipped_conversion_refuse_a_configured_filter() {
    let dir = std::env::temp_dir().join(format!("rivet-convert-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "filter.cat.clean", "cat"]);
    git(&dir, &["config", "filter.cat.smudge", "cat"]);
    std::fs::write(dir.join("f.dat"), "one\ntwo\nthree\n").unwrap();
    git(&dir, &["add", "f.dat"]);
    git(&dir, &["commit", "-qm", "base"]);
    // The attribute arrives after the commit, so adding it does not run the filter itself.
    std::fs::write(dir.join(".gitattributes"), "*.dat filter=cat\n").unwrap();
    std::fs::write(dir.join("f.dat"), "one\nTWO\nthree\n").unwrap();
    let before = std::fs::read(dir.join("f.dat")).unwrap();

    for args in [
        vec!["diff"],
        vec!["blame", "f.dat"],
        vec!["stash", "push"],
        vec!["stash", "push", "-u"],
    ] {
        let out = rivet(&dir, &args);
        refused(&out, &args.join(" "));
        assert!(out.stdout.is_empty(), "{args:?} printed {:?}", out.stdout);
    }
    assert_eq!(std::fs::read(dir.join("f.dat")).unwrap(), before);
    assert!(
        git(&dir, &["stash", "list"]).is_empty(),
        "a refused stash must not leave an entry"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
