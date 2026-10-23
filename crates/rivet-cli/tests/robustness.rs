//! Regression tests for the bugs fixed while finishing `apply` and `gc --cruft`. Each asserts the
//! state the command leaves behind (index, work tree, refs, objects), read back with real git,
//! not only the exit code. (The byte-for-byte comparisons with git are the differential scenarios
//! `s_apply_*` and `s_gc_cruft*`.)

use std::path::Path;
use std::process::{Command, Output};

fn command(program: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("TZ", "UTC")
        .output()
        .expect("spawn")
}

fn rivet(dir: &Path, args: &[&str]) -> Output {
    command(env!("CARGO_BIN_EXE_rivet"), dir, args)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = command("git", dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn numbered(count: usize) -> String {
    (1..=count).map(|n| format!("{n}\n")).collect()
}

/// A repository with `a.txt` (twenty numbered lines) committed on `main`.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), numbered(20)).unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-qm", "base"]);
    dir
}

/// `git diff` of changing line 5 of `a.txt`, written to `<dir>/../<name>`; the tree is restored.
fn patch_line_five(dir: &Path, name: &str) -> std::path::PathBuf {
    let mut text = numbered(20);
    text = text.replacen("5\n", "FIVE\n", 1);
    std::fs::write(dir.join("a.txt"), text).unwrap();
    let diff = command("git", dir, &["diff", "--full-index"]).stdout;
    git(dir, &["checkout", "-q", "--", "a.txt"]);
    let path = dir.parent().unwrap().join(name);
    std::fs::write(&path, diff).unwrap();
    path
}

#[test]
fn apply_stat_summary_and_numstat_leave_the_tree_and_index_alone_until_apply_is_given() {
    let dir = repo();
    let p = dir.path();
    let patch = patch_line_five(p, "stat.patch");
    let patch = patch.to_str().unwrap();

    let out = rivet(p, &["apply", "--stat", "--numstat", "--summary", patch]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains(" a.txt |    2 +-\n"), "{text}");
    assert!(text.contains("1\t1\ta.txt\n"), "{text}");
    assert_eq!(
        git(p, &["status", "--porcelain"]),
        "",
        "nothing was applied"
    );

    let out = rivet(p, &["apply", "--stat", "--apply", "--index", patch]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(git(p, &["status", "--porcelain"]), "M  a.txt");
    assert_eq!(git(p, &["show", ":a.txt"]).lines().nth(4), Some("FIVE"));
}

#[test]
fn apply_reject_writes_the_failed_hunk_applies_the_rest_and_leaves_the_index_alone() {
    let dir = repo();
    let p = dir.path();
    // Two hunks: lines 3 and 18 change. The work tree has line 18 changed already.
    let mut text = numbered(20);
    text = text
        .replacen("\n3\n", "\nTHREE\n", 1)
        .replacen("\n18\n", "\nEIGHTEEN\n", 1);
    std::fs::write(p.join("a.txt"), text).unwrap();
    let diff = command("git", p, &["diff"]).stdout;
    git(p, &["checkout", "-q", "--", "a.txt"]);
    let patch = p.parent().unwrap().join("two.patch");
    std::fs::write(&patch, diff).unwrap();
    std::fs::write(
        p.join("a.txt"),
        numbered(20).replacen("\n18\n", "\neighteen\n", 1),
    )
    .unwrap();
    git(p, &["add", "a.txt"]);

    let out = rivet(
        p,
        &["apply", "--reject", "--index", patch.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("Rejected hunk #2."), "{err}");
    assert!(err.contains("error: patch failed: a.txt:15"), "{err}");

    let tree = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(
        tree.lines().nth(2),
        Some("THREE"),
        "the fitting hunk applied"
    );
    assert_eq!(
        tree.lines().nth(17),
        Some("eighteen"),
        "the other line is untouched"
    );
    let rej = std::fs::read_to_string(p.join("a.txt.rej")).unwrap();
    assert!(
        rej.starts_with("diff a/a.txt b/a.txt\t(rejected hunks)\n@@ -15,"),
        "{rej}"
    );
    assert!(rej.contains("-18\n+EIGHTEEN\n"), "{rej}");
    // A failed run does not stage anything: the index still holds the pre-patch content.
    assert_eq!(git(p, &["show", ":a.txt"]).lines().nth(2), Some("3"));
}

#[test]
fn apply_three_way_conflict_leaves_the_three_stages_and_markers() {
    let dir = repo();
    let p = dir.path();
    let patch = patch_line_five(p, "conflict.patch");
    std::fs::write(
        p.join("a.txt"),
        numbered(20).replacen("5\n", "five-local\n", 1),
    )
    .unwrap();
    git(p, &["commit", "-qam", "local"]);

    let out = rivet(p, &["apply", "--3way", patch.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("Applied patch to 'a.txt' with conflicts."));
    let stages = git(p, &["ls-files", "-u"]);
    let numbers: Vec<&str> = stages
        .lines()
        .map(|l| l.split_whitespace().nth(2).unwrap())
        .collect();
    assert_eq!(numbers, ["1", "2", "3"], "{stages}");
    let tree = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert!(
        tree.contains("<<<<<<< ours\nfive-local\n=======\nFIVE\n>>>>>>> theirs\n"),
        "{tree}"
    );
}

#[test]
fn apply_three_way_with_a_garbled_index_id_falls_back_to_a_direct_apply_instead_of_panicking() {
    let dir = repo();
    let p = dir.path();
    let patch = patch_line_five(p, "garbled.patch");
    let text = std::fs::read_to_string(&patch).unwrap();
    // A non-ASCII "id": slicing it to an even length used to panic inside the char.
    let garbled: String = text
        .lines()
        .map(|l| {
            if l.starts_with("index ") {
                "index a\u{e9}\u{e9}\u{e9}..b\u{e9} 100644".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&patch, garbled).unwrap();

    let out = rivet(p, &["apply", "--3way", patch.to_str().unwrap()]);
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("Falling back to direct application..."));
    assert_eq!(git(p, &["status", "--porcelain"]), "M  a.txt");
    assert_eq!(git(p, &["show", ":a.txt"]).lines().nth(4), Some("FIVE"));
}

#[test]
fn apply_refuses_a_binary_patch_without_full_ids_and_applies_none_of_the_patch() {
    let dir = repo();
    let p = dir.path();
    std::fs::write(p.join("bin.dat"), b"\0\x01abc").unwrap();
    git(p, &["add", "bin.dat"]);
    git(p, &["commit", "-qm", "bin"]);
    std::fs::write(p.join("bin.dat"), b"\0\x02xyz!").unwrap();
    std::fs::write(p.join("a.txt"), numbered(20).replacen("5\n", "FIVE\n", 1)).unwrap();
    let diff = command("git", p, &["diff"]).stdout;
    git(p, &["checkout", "-q", "--", "."]);
    let patch = p.parent().unwrap().join("bin.patch");
    std::fs::write(&patch, diff).unwrap();

    let out = rivet(p, &["apply", patch.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out)
            .contains("error: cannot apply binary patch to 'bin.dat' without full index line"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        git(p, &["status", "--porcelain"]),
        "",
        "nothing was applied"
    );
}

#[test]
fn a_truncated_index_entry_is_an_error_not_a_panic_and_the_index_file_is_untouched() {
    let dir = repo();
    let p = dir.path();
    let index = p.join(".git/index");
    let bytes = std::fs::read(&index).unwrap();
    // Header (12), then 61 bytes of entry (the fixed part, the object id and one byte of the flags
    // word), then what the reader takes for the trailing checksum (20).
    let cut = &bytes[..12 + 61 + 20];
    std::fs::write(&index, cut).unwrap();
    let head = git(p, &["rev-parse", "HEAD"]);

    let out = rivet(p, &["ls-files", "-s"]);
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(&index).unwrap(),
        cut,
        "the index must not be rewritten"
    );
    assert_eq!(git(p, &["rev-parse", "HEAD"]), head);
}

#[test]
fn log_with_a_leading_three_dot_spec_naming_nothing_fails_cleanly_and_moves_no_ref() {
    let dir = repo();
    let p = dir.path();
    let head = git(p, &["rev-parse", "HEAD"]);
    for spec in ["...x", "...", "...:@{99999999999}"] {
        let out = rivet(p, &["log", "--oneline", spec]);
        assert!(
            !stderr(&out).contains("panicked"),
            "{spec}: {}",
            stderr(&out)
        );
    }
    assert_eq!(git(p, &["rev-parse", "HEAD"]), head);
    // The symmetric difference with an empty left side is HEAD...side, as in git.
    git(p, &["branch", "side"]);
    std::fs::write(p.join("b.txt"), "b\n").unwrap();
    git(p, &["add", "b.txt"]);
    git(p, &["commit", "-qm", "ahead"]);
    let ours = rivet(p, &["log", "--oneline", "...side"]);
    let theirs = command("git", p, &["log", "--oneline", "...side"]);
    assert_eq!(ours.stdout, theirs.stdout);
}

fn set_mtime(path: &Path, epoch: u32) {
    let status = Command::new("touch")
        .arg("-d")
        .arg(format!("@{epoch}"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

fn loose_path(p: &Path, oid: &str) -> std::path::PathBuf {
    p.join(format!(".git/objects/{}/{}", &oid[..2], &oid[2..]))
}

const OLD: u32 = 1_788_220_800; // 2026-09-01
const CUTOFF: u32 = 1_791_158_400; // 2026-10-05T00:00:00Z
const YOUNG: u32 = 1_792_454_400; // 2026-10-20

fn write_blob(p: &Path, text: &str) -> String {
    let file = p.parent().unwrap().join("blob.tmp");
    std::fs::write(&file, text).unwrap();
    git(p, &["hash-object", "-w", file.to_str().unwrap()])
}

/// `git <args>` with `GIT_INDEX_FILE` pointing at a scratch index, so the real one is untouched.
fn git_scratch(p: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(p)
        .env("GIT_INDEX_FILE", p.join(".git/scratch-index"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// An unreachable commit (young) over an old tree and blob, and an old lone blob.
fn unreachable_chain(p: &Path) -> (String, String, String, String) {
    let old_blob = write_blob(p, "old-blob\n");
    git_scratch(
        p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{old_blob},kept"),
        ],
    );
    let tree = git_scratch(p, &["write-tree"]);
    std::fs::remove_file(p.join(".git/scratch-index")).unwrap();
    let young = git(p, &["commit-tree", &tree, "-m", "young unreachable"]);
    let lone = write_blob(p, "lone-old\n");
    for oid in [&old_blob, &tree, &lone] {
        set_mtime(&loose_path(p, oid), OLD);
    }
    set_mtime(&loose_path(p, &young), YOUNG);
    (young, tree, old_blob, lone)
}

#[test]
fn gc_without_cruft_keeps_the_old_objects_a_young_unreachable_commit_still_needs() {
    let dir = repo();
    let p = dir.path();
    let (young, tree, old_blob, lone) = unreachable_chain(p);

    let out = rivet(p, &["gc", "--no-cruft", "--prune=2026-10-05T00:00:00Z"]);
    assert!(out.status.success(), "{}", stderr(&out));
    for oid in [&young, &tree, &old_blob] {
        assert!(
            loose_path(p, oid).exists(),
            "{oid} must survive as a loose object"
        );
    }
    assert!(
        !loose_path(p, &lone).exists(),
        "the unrelated old blob is pruned"
    );
    git(p, &["fsck", "--no-dangling"]);
}

#[test]
fn gc_cruft_packs_the_young_commit_with_its_old_tree_and_blob_and_records_their_ages() {
    let dir = repo();
    let p = dir.path();
    let (young, tree, old_blob, lone) = unreachable_chain(p);

    let out = rivet(p, &["gc", "--cruft", "--prune=2026-10-05T00:00:00Z"]);
    assert!(out.status.success(), "{}", stderr(&out));
    for oid in [&young, &tree, &old_blob, &lone] {
        assert!(!loose_path(p, oid).exists(), "{oid} is not loose any more");
    }
    for oid in [&young, &tree, &old_blob] {
        git(p, &["cat-file", "-e", oid]);
    }
    assert!(command("git", p, &["cat-file", "-e", &lone]).status.code() != Some(0));

    let pack_dir = p.join(".git/objects/pack");
    let mtimes: Vec<_> = std::fs::read_dir(&pack_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|path| path.extension().is_some_and(|e| e == "mtimes"))
        .collect();
    assert_eq!(mtimes.len(), 1);
    let data = std::fs::read(&mtimes[0]).unwrap();
    assert_eq!(&data[..4], b"MTME");
    // header (12) + three objects + two 20-byte checksums
    assert_eq!(data.len(), 12 + 3 * 4 + 40);
    let mut ages: Vec<u32> = data[12..12 + 12]
        .chunks(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    ages.sort_unstable();
    assert_eq!(ages, [CUTOFF, CUTOFF, YOUNG]);
    git(p, &["fsck", "--no-dangling"]);
}

#[test]
fn log_graph_draws_a_root_that_is_not_shown_last_one_column_to_the_right() {
    let dir = repo();
    let p = dir.path();
    // One root: the graph is exactly git's.
    let ours = rivet(p, &["log", "--graph", "--oneline", "--all"]);
    assert_eq!(
        String::from_utf8_lossy(&ours.stdout),
        git(p, &["log", "--graph", "--oneline", "--all"]) + "\n"
    );
    // A second, unrelated root: git indents the one that is not shown last.
    git(p, &["checkout", "-q", "--orphan", "other"]);
    git(p, &["rm", "-rqf", "."]);
    std::fs::write(p.join("z.txt"), "z\n").unwrap();
    git(p, &["add", "z.txt"]);
    git(p, &["commit", "-qm", "second root"]);
    git(p, &["checkout", "-q", "main"]);
    let out = rivet(p, &["log", "--graph", "--oneline", "--all"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        text,
        git(p, &["log", "--graph", "--oneline", "--all"]) + "\n"
    );
    // Guard against the oracle and rivet agreeing on an unshifted drawing: the root shown first is
    // drawn two characters in, with the other root back in column zero below it.
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("  * "), "{text}");
    assert!(lines[1].starts_with("* "), "{text}");
    // Without --graph the same walk is fine.
    assert!(rivet(p, &["log", "--oneline", "--all"]).status.success());
}
