//! Differential test against real Git: rivet's loose object store must agree with Git's.
//!
//! 1. Git writes a loose blob; rivet reads it and must get identical bytes and id.
//! 2. Rivet writes a loose blob; `git cat-file -p` must print the same bytes and
//!    `git fsck --strict` must accept the repository.

use std::io::Write as _;
use std::process::Command;

use rivet_core::odb::LooseObjectStore;
use rivet_core::ObjectFormat;
use rivet_core::ObjectId;
use rivet_core::ObjectKind;

fn git_in_dir(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_DIR", dir.join(".git").as_os_str())
        .output();
    let out = out.expect("git failed");
    let stdout = String::from_utf8(out.stdout).expect("git stdout not UTF-8");
    assert!(out.status.success(), "git {:?} failed: {}", args, stdout);
    stdout
}

#[test]
fn git_written_blob_rivet_reads() {
    let tmp = tempfile::TempDir::new().unwrap();
    git_in_dir(tmp.path(), &["init", "--quiet"]);

    // git hash-object -w writes the blob to .git/objects/ and prints its id
    let body = b"hello from git\n";
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(tmp.path())
        .env("GIT_DIR", tmp.path().join(".git").as_os_str())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git spawn failed");
    child.stdin.as_mut().unwrap().write_all(body).expect("write to git stdin failed");
    let out = child.wait_with_output().expect("git wait failed");
    let stdout = String::from_utf8(out.stdout).expect("not UTF-8");
    assert!(out.status.success(), "git hash-object failed: {stdout}");
    let id_hex = stdout.trim();

    let store = LooseObjectStore::open(tmp.path().join(".git").join("objects").as_path(), ObjectFormat::Sha1);
    let oid = ObjectId::from_hex(ObjectFormat::Sha1, id_hex.as_bytes().into()).unwrap();
    assert!(store.contains(&oid));

    let obj = store.read(&oid).unwrap();
    assert_eq!(obj.kind(), ObjectKind::Blob);
    assert_eq!(obj.data(), body);
    assert_eq!(obj.id(ObjectFormat::Sha1).unwrap(), oid);
}

#[test]
fn rivet_written_blob_git_reads() {
    let tmp = tempfile::TempDir::new().unwrap();
    git_in_dir(tmp.path(), &["init", "--quiet"]);

    let body = b"hello from rivet\n";
    let store = LooseObjectStore::open(tmp.path().join(".git").join("objects").as_path(), ObjectFormat::Sha1);
    let oid = store.write_raw(ObjectKind::Blob, body).unwrap();

    // git cat-file -p <id> must print the body verbatim
    let hex = oid.to_hex().to_string();
    let out = git_in_dir(tmp.path(), &["cat-file", "-p", &hex]);
    assert_eq!(out.as_bytes(), body, "git cat-file output mismatch");

    // fsck --strict must accept the repository. An unreferenced object is
    // reported as "dangling ..." plus a "notice: No default references" line;
    // both are informational (verified against real git: exit status 0), so
    // only genuine problem lines fail this test.
    let out = git_in_dir(tmp.path(), &["fsck", "--strict"]);
    for line in out.lines() {
        let problem = ["error", "missing", "corrupt", "bad", "fatal"]
            .iter()
            .any(|kw| line.contains(kw));
        assert!(!problem, "git fsck reported a problem: {line}");
    }

    // And git re-derives exactly rivet's id from the same content.
    let mut child = Command::new("git")
        .args(["hash-object", "-t", "blob", "--stdin"])
        .current_dir(tmp.path())
        .env("GIT_DIR", tmp.path().join(".git").as_os_str())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git spawn failed");
    child.stdin.as_mut().unwrap().write_all(body).expect("write to git stdin failed");
    let out = child.wait_with_output().expect("git wait failed");
    let derived = String::from_utf8(out.stdout).expect("not UTF-8");
    assert!(out.status.success(), "git hash-object failed: {derived}");
    assert_eq!(derived.trim(), hex, "git derived a different id than rivet wrote");
}
