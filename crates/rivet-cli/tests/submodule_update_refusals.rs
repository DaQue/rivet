//! `submodule update` flags that cannot be matched are refused by name, never half-handled.
//! (The flags that work are the differential scenario `s_submodule_update_depth`.)

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

fn git(dir: &Path, args: &[&str]) {
    let mut full = vec!["-c", "protocol.file.allow=always"];
    full.extend_from_slice(args);
    let out = run("/usr/bin/git", dir, &full);
    assert!(out.status.success(), "{args:?}: {:?}", out);
}

/// A clone of a superproject whose submodule `s` (a `file://` URL) is not yet initialised.
fn fixture(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "sub"]);
    std::fs::write(dir.join("sub/a"), "a\n").unwrap();
    git(&dir.join("sub"), &["add", "a"]);
    git(&dir.join("sub"), &["commit", "-q", "-m", "one"]);
    git(&dir, &["init", "-q", "sup"]);
    let url = format!("file://{}", dir.join("sub").display());
    git(&dir.join("sup"), &["submodule", "add", "-q", &url, "s"]);
    git(&dir.join("sup"), &["commit", "-q", "-m", "add"]);
    git(&dir, &["clone", "-q", "sup", "c"]);
    dir.join("c")
}

#[test]
fn depth_on_a_url_clone_is_refused_and_leaves_nothing_behind() {
    let clone = fixture("sm-depth-url");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    let out = run(
        rivet,
        &clone,
        &["submodule", "update", "--init", "--depth", "1"],
    );
    assert_eq!(out.status.code(), Some(128));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--depth with a file:// or network URL is not implemented"),
        "{stderr}"
    );
    assert!(!clone.join("s/.git").exists());
    assert!(!clone.join(".git/modules/s").exists());
    let _ = std::fs::remove_dir_all(clone.parent().unwrap());
}

#[test]
fn single_branch_on_a_source_with_several_branches_is_refused_and_leaves_nothing_behind() {
    let clone = fixture("sm-single-branch");
    git(&clone.parent().unwrap().join("sub"), &["branch", "other"]);
    let rivet = env!("CARGO_BIN_EXE_rivet");
    let out = run(
        rivet,
        &clone,
        &["submodule", "update", "--init", "--single-branch"],
    );
    assert_eq!(out.status.code(), Some(128));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "--single-branch (or --depth) on a source with several branches is not implemented"
        ),
        "{stderr}"
    );
    assert!(!clone.join("s/.git").exists());
    assert!(!clone.join(".git/modules/s").exists());
    let _ = std::fs::remove_dir_all(clone.parent().unwrap());
}

#[test]
fn filter_is_refused_by_name() {
    let clone = fixture("sm-reference");
    let rivet = env!("CARGO_BIN_EXE_rivet");
    let flag = "--filter";
    let out = run(rivet, &clone, &["submodule", "update", "--init", flag]);
    assert!(!out.status.success(), "{flag}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("{flag} is not implemented")),
        "{stderr}"
    );
    assert!(!clone.join("s/.git").exists());
    let _ = std::fs::remove_dir_all(clone.parent().unwrap());
}
