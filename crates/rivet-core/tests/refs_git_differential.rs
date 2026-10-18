//! Differential test against **real git** for the refs layer.
//!
//! Provenance: every expectation below is produced by running git 2.56 in a throwaway
//! repository and reading back what git itself reports. The repository state is built by git
//! (commits, branches, annotated and lightweight tags, `pack-refs`, detached and unborn HEADs),
//! never by rivet, so a disagreement is rivet's bug.
//!
//! What "Git-compatible" means for refs: `HEAD` classification (attached / detached / unborn)
//! matches `git symbolic-ref` + `git rev-parse`; branch and tag enumeration matches
//! `git for-each-ref`; resolution and peeling match `git rev-parse <ref>` and `<ref>^{commit}`;
//! a loose file shadows a stale packed entry exactly as git does; and refs rivet writes are
//! visible to `git rev-parse` with the value and existence git expects.

use bstr::BStr;
use rivet_core::{HeadState, ObjectFormat, ObjectId, Odb, RefName, RefStore, RefTarget};
use std::path::Path;
use std::process::Command;

/// Run git in `dir` with a hermetic config (no user/system config, fixed identity and clock).
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Rivet Test",
            "-c",
            "user.email=rivet@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(ObjectFormat::Sha1, BStr::new(hex)).expect("git printed a sha1")
}

fn name(full: &str) -> RefName {
    RefName::from_bytes(full.as_bytes()).expect("valid ref name")
}

/// `(full name, oid)` pairs from `git for-each-ref`, sorted the way git sorts them.
fn git_refs(dir: &Path, prefix: &str) -> Vec<(String, String)> {
    let out = git(
        dir,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    );
    out.lines()
        .map(|line| {
            let (n, o) = line.split_once(' ').expect("for-each-ref line");
            (n.to_string(), o.to_string())
        })
        .collect()
}

fn rivet_pairs(pairs: Vec<(RefName, ObjectId)>) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .map(|(n, o)| {
            (
                String::from_utf8(n.as_bytes().to_vec()).expect("utf8 ref name"),
                o.to_hex().to_string(),
            )
        })
        .collect()
}

#[test]
fn head_branches_and_tags_match_git_after_pack_refs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "one"]);
    std::fs::write(dir.join("a.txt"), b"two\n").unwrap();
    git(dir, &["commit", "-q", "-am", "two"]);
    git(dir, &["branch", "feature"]);
    git(dir, &["tag", "light"]);
    git(dir, &["tag", "-a", "ann", "-m", "annotated"]);
    // Move everything git can into packed-refs; HEAD stays a loose symbolic file.
    git(dir, &["pack-refs", "--all"]);

    let git_dir = dir.join(".git");
    let store = RefStore::open(&git_dir, ObjectFormat::Sha1).expect("open ref store");

    assert_eq!(
        store.head().unwrap(),
        HeadState::Attached {
            branch: name("refs/heads/main"),
            oid: oid(&git(dir, &["rev-parse", "HEAD"])),
        }
    );
    assert_eq!(store.head_branch().unwrap(), Some(name("refs/heads/main")));

    assert_eq!(
        rivet_pairs(store.list_branches().unwrap()),
        git_refs(dir, "refs/heads")
    );
    assert_eq!(
        rivet_pairs(store.list_tags().unwrap()),
        git_refs(dir, "refs/tags")
    );

    // Resolution agrees with `git rev-parse` for a packed branch, a packed lightweight tag and
    // a packed annotated tag (which names a tag object, not the commit).
    for r in ["refs/heads/feature", "refs/tags/light", "refs/tags/ann"] {
        assert_eq!(
            store.resolve_oid(&name(r)).unwrap(),
            oid(&git(dir, &["rev-parse", r])),
            "resolve_oid({r})"
        );
    }

    // Peeling an annotated tag to its commit is what `rev-parse ann^{commit}` answers.
    let odb = Odb::open(&git_dir, ObjectFormat::Sha1).expect("open odb");
    assert_eq!(
        store.peel_to_commit(&name("refs/tags/ann"), &odb).unwrap(),
        oid(&git(dir, &["rev-parse", "refs/tags/ann^{commit}"]))
    );
    assert_eq!(
        store.peel_to_commit(&name("refs/heads/main"), &odb).unwrap(),
        oid(&git(dir, &["rev-parse", "refs/heads/main^{commit}"]))
    );
}

#[test]
fn a_loose_ref_shadows_the_stale_packed_entry_like_git() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "one"]);
    git(dir, &["checkout", "-q", "-b", "other"]);
    std::fs::write(dir.join("a.txt"), b"two\n").unwrap();
    git(dir, &["commit", "-q", "-am", "two"]);
    git(dir, &["pack-refs", "--all"]);
    // Rewrite `other` while it is packed: git writes a loose file and leaves the stale
    // packed line in place.
    git(dir, &["checkout", "-q", "main"]);
    git(dir, &["branch", "-f", "other", "main"]);

    let git_dir = dir.join(".git");
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).unwrap_or_default();
    assert!(
        packed.contains("refs/heads/other"),
        "fixture must leave a stale packed entry, got:\n{packed}"
    );

    let store = RefStore::open(&git_dir, ObjectFormat::Sha1).expect("open ref store");
    let expected = oid(&git(dir, &["rev-parse", "refs/heads/other"]));
    assert_eq!(store.resolve_oid(&name("refs/heads/other")).unwrap(), expected);
    assert_eq!(store.head_branch().unwrap(), Some(name("refs/heads/main")));
}

#[test]
fn detached_and_unborn_head_match_git() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q"]);
    let git_dir = dir.join(".git");

    // Unborn: HEAD names a branch that does not exist yet.
    let store = RefStore::open(&git_dir, ObjectFormat::Sha1).expect("open ref store");
    assert_eq!(
        store.head().unwrap(),
        HeadState::Unborn {
            branch: name("refs/heads/main")
        }
    );
    assert_eq!(store.head_branch().unwrap(), None);

    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "one"]);
    let head = oid(&git(dir, &["rev-parse", "HEAD"]));
    git(dir, &["checkout", "-q", "--detach"]);

    let store = RefStore::open(&git_dir, ObjectFormat::Sha1).expect("reopen ref store");
    assert_eq!(store.head().unwrap(), HeadState::Detached { oid: head });
    assert_eq!(store.head_branch().unwrap(), None);
    assert_eq!(store.resolve_oid(&name("HEAD")).unwrap(), head);
}

#[test]
fn refs_rivet_writes_are_exactly_what_git_reads() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "one"]);
    std::fs::write(dir.join("a.txt"), b"two\n").unwrap();
    git(dir, &["commit", "-q", "-am", "two"]);

    let git_dir = dir.join(".git");
    let store = RefStore::open(&git_dir, ObjectFormat::Sha1).expect("open ref store");
    let first = oid(&git(dir, &["rev-parse", "HEAD"]));
    let second = oid(&git(dir, &["rev-parse", "HEAD^"]));
    let made = name("refs/heads/rivet-made");

    // Create (CAS from absent) and let git confirm the value.
    store.set_direct(&made, first, None).expect("set_direct");
    assert_eq!(git(dir, &["rev-parse", "refs/heads/rivet-made"]), first.to_hex().to_string());

    // Compare-and-swap: moving it with the right expectation succeeds, the wrong one is refused
    // and leaves the ref untouched.
    store
        .update(&made, second, Some(first), None)
        .expect("update with correct expectation");
    assert_eq!(git(dir, &["rev-parse", "refs/heads/rivet-made"]), second.to_hex().to_string());
    assert!(
        store.update(&made, first, Some(second), None).is_ok(),
        "second CAS back to the original value"
    );
    assert!(
        store.update(&made, second, Some(second), None).is_err(),
        "a stale expectation must be refused"
    );
    assert_eq!(git(dir, &["rev-parse", "refs/heads/rivet-made"]), first.to_hex().to_string());

    // Deleting removes the file; git then agrees the ref is gone.
    store.delete(&made).expect("delete");
    let out = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/rivet-made"])
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(!out.status.success(), "deleted ref must not resolve");

    // A symbolic ref written by rivet is followed by git too.
    store
        .set_symbolic(&name("refs/heads/alias"), &name("refs/heads/main"), None)
        .expect("set_symbolic");
    assert_eq!(
        git(dir, &["symbolic-ref", "refs/heads/alias"]),
        "refs/heads/main"
    );
    assert_eq!(
        store.follow(&name("refs/heads/alias")).unwrap(),
        name("refs/heads/main")
    );
}

#[test]
fn reading_through_a_symbolic_ref_and_list_all_agree_with_git() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), b"one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "one"]);
    git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

    let git_dir = dir.join(".git");
    let store = RefStore::open(&git_dir, ObjectFormat::Sha1).expect("open ref store");
    let odb = Odb::open(&git_dir, ObjectFormat::Sha1).expect("open odb");

    let all = store.list_all(&odb).expect("list_all");
    let head = oid(&git(dir, &["rev-parse", "HEAD"]));
    for (n, o) in &all {
        let full = String::from_utf8(n.as_bytes().to_vec()).unwrap();
        assert_eq!(*o, oid(&git(dir, &["rev-parse", &full])), "list_all({full})");
    }
    assert!(
        all.iter().any(|(n, o)| n.as_bytes() == b"refs/heads/main" && *o == head),
        "list_all must include the branch git just committed to"
    );
    assert_eq!(
        rivet_pairs(store.list_remote().unwrap()),
        git_refs(dir, "refs/remotes")
    );

    // HEAD read as a target is symbolic to the branch name git reports.
    assert_eq!(
        store.read(&name("HEAD")).unwrap(),
        RefTarget::Symbolic(name("refs/heads/main"))
    );
}
