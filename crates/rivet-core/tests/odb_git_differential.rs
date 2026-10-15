//! Differential test: `Odb::read` / `read_typed` / `read_header` against real `git`.
//!
//! git is the oracle: when a test and git disagree, git is right. The test skips gracefully
//! when `git` is not on `PATH`.

use rivet_core::ObjectFormat;
use rivet_core::ObjectId;
use rivet_core::ObjectKind;
use rivet_core::odb::Odb;
use std::env;
use std::io::Write as _;
use std::process::{Command, Stdio};

fn git_path() -> Option<String> {
    env::var_os("PATH")
        .and_then(|path| {
            for dir in env::split_paths(&path) {
                let candidate = dir.join("git");
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
            None
        })
}

fn git(git: &str, args: &[&str], input: Option<&[u8]>) -> Result<String, String> {
    let mut cmd = Command::new(git);
    cmd.args(args);
    let mut child = cmd
        // All three streams must be piped: with stdout/stderr left inherited the child writes
        // into the test harness's own output, `wait_with_output` hands back empty strings, and a
        // command that succeeded looks like it printed nothing (and an error message loses its
        // stderr).
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(data) = input {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(data)
            .map_err(|e| e.to_string())?;
        child.stdin.take().unwrap().flush().map_err(|e| e.to_string())?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_fail_on_error_lines(git: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(git)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stdout.lines().chain(stderr.lines()) {
        let lower = line.to_lowercase();
        if lower.contains("error")
            || lower.contains("missing")
            || lower.contains("corrupt")
            || lower.contains("bad")
            || lower.contains("fatal")
        {
            return Err(format!("git fsck reported a bad line: {line}"));
        }
    }
    Ok(())
}

#[test]
fn odb_read_agrees_with_git_on_blob_and_tree() {
    let Some(git_exe) = git_path() else {
        eprintln!("git not on PATH; skipping");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    let repo_str = repo.to_str().expect("repo path is valid utf-8");

    // git init a temp repo so the objects dir exists and `git cat-file` works.
    git(&git_exe, &["init", repo_str], None).expect("git init");

    let git_dir = repo.join(".git");

    // Write a blob and a small tree via `git hash-object -w --stdin`.
    let blob_data = b"hello from rivet\n";
    // `-C <repo>` pins every git command to the temp repo: the test binary's cwd is a crate dir
    // inside rivet's own repository, so an unpinned `cat-file` resolves against *rivet* instead.
    let blob_hex = git(
        &git_exe,
        &["-C", repo_str, "hash-object", "-t", "blob", "-w", "--stdin"],
        Some(blob_data),
    )
    .expect("git hash-object blob")
    .trim()
    .to_string();

    // A minimal tree, byte-for-byte as git stores one: "<mode> <name>\0" followed by the 20
    // *raw* bytes of the blob id. Hex text here would make a tree git calls broken.
    let mut tree_data = Vec::with_capacity(32);
    tree_data.extend_from_slice(b"100644 hello.txt\0");
    for i in 0..20 {
        tree_data.push(u8::from_str_radix(&blob_hex[2 * i..2 * i + 2], 16).expect("hex byte"));
    }

    let tree_hex = git(
        &git_exe,
        &["-C", repo_str, "hash-object", "-t", "tree", "-w", "--stdin"],
        Some(&tree_data),
    )
    .expect("git hash-object tree")
    .trim()
    .to_string();

    let odb = Odb::open(&git_dir, ObjectFormat::Sha1).expect("open odb");

    let blob_oid = ObjectId::from_hex(ObjectFormat::Sha1, bstr::BStr::new(blob_hex.as_bytes()))
        .expect("parse blob id");
    let tree_oid = ObjectId::from_hex(ObjectFormat::Sha1, bstr::BStr::new(tree_hex.as_bytes()))
        .expect("parse tree id");

    // Blob: bytes must equal `git cat-file -p`, and the id must round-trip.
    let object = odb.read(&blob_oid).expect("read blob");
    assert_eq!(object.kind(), ObjectKind::Blob);
    assert_eq!(object.data(), blob_data);
    // The id round-trips: the bytes hash to the id we asked for.
    let recomputed = odb
        .loose()
        .write_raw(ObjectKind::Blob, blob_data)
        .expect("re-hash blob");
    assert_eq!(recomputed, blob_oid);

    // Tree bytes: `cat-file -p` pretty-prints (mode, type, hex id, `\t`, name) and the helper
    // decodes git's stdout as UTF-8, so a raw 20-byte object id cannot survive a byte-for-byte
    // comparison through stdout. Re-hash the exact bytes instead — identical bytes must hash to
    // the identical id — and let the pretty form prove git parses the entry we wrote.
    let rehashed = git(
        &git_exe,
        &["-C", repo_str, "hash-object", "-t", "tree", "--stdin"],
        Some(&tree_data),
    )
    .expect("re-hash tree");
    assert_eq!(rehashed.trim(), tree_hex.as_str());

    let pretty = git(&git_exe, &["-C", repo_str, "cat-file", "-p", tree_hex.as_str()], None)
        .expect("git cat-file -p tree");
    assert_eq!(pretty, format!("100644 blob {blob_hex}\thello.txt\n"));
    let object = odb.read(&tree_oid).expect("read tree");
    assert_eq!(object.kind(), ObjectKind::Tree);
    assert_eq!(object.data(), tree_data.as_slice());

    // read_typed: right kind passes, wrong kind is WrongObjectKind.
    let _ = odb.read_typed(&blob_oid, ObjectKind::Blob).expect("read_typed blob");
    let err = odb.read_typed(&blob_oid, ObjectKind::Tree).expect_err("wrong kind");
    assert!(matches!(err, rivet_core::error::Error::WrongObjectKind { .. }));

    // read_header agrees with read on kind and size.
    let (kind, size) = odb.read_header(&blob_oid).expect("read_header blob");
    assert_eq!(kind, ObjectKind::Blob);
    assert_eq!(size, blob_data.len() as u64);
    let (kind, size) = odb.read_header(&tree_oid).expect("read_header tree");
    assert_eq!(kind, ObjectKind::Tree);
    assert_eq!(size, tree_data.len() as u64);

    // git fsck --strict afterwards: fail only on error/missing/corrupt/bad/fatal.
    git_fail_on_error_lines(&git_exe, &["-C", repo_str, "fsck", "--strict"])
        .expect("git fsck must report no errors");
}

/// rivet writes a blob; real git reads it back and recomputes the same id.
/// `git cat-file -t`/`-p` and `git hash-object -t blob --stdin` must agree with rivet's
/// reported id, and `git fsck --strict` must not report anything but dangling/notice.
#[test]
fn odb_write_is_read_by_git() {
    let Some(git_exe) = git_path() else {
        eprintln!("git not on PATH; skipping");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    let repo_str = repo.to_str().expect("repo path is valid utf-8");
    git(&git_exe, &["init", repo_str], None).expect("git init");

    let git_dir = repo.join(".git");
    let odb = Odb::open(&git_dir, ObjectFormat::Sha1).expect("open odb");

    let blob_data = b"written by rivet\n";
    let blob_oid = odb
        .write_raw(ObjectKind::Blob, blob_data)
        .expect("rivet write_raw");
    let blob_hex = blob_oid.to_hex();

    // git cat-file -t must say "blob".
    let kind = git(&git_exe, &["-C", repo_str, "cat-file", "-t", &blob_hex.to_string()], None)
        .expect("git cat-file -t")
        .trim()
        .to_string();
    assert_eq!(kind, "blob");

    // git cat-file -p must return exactly the body rivet wrote.
    let cat = git(&git_exe, &["-C", repo_str, "cat-file", "-p", &blob_hex.to_string()], None)
        .expect("git cat-file -p");
    assert_eq!(cat.as_bytes(), blob_data);

    // git hash-object -t blob --stdin over the same body must reproduce rivet's id.
    let recomputed = git(
        &git_exe,
        &["-C", repo_str, "hash-object", "-t", "blob", "--stdin"],
        Some(blob_data),
    )
    .expect("git hash-object")
    .trim()
    .to_string();
    assert_eq!(recomputed, blob_hex);

    // fsck --strict: only dangling/notice are informational; any error line fails.
    git_fail_on_error_lines(&git_exe, &["-C", repo_str, "fsck", "--strict"])
        .expect("git fsck must report no errors");
}
