//! Phase 10 — the pack reader against packs that `git` wrote.
//!
//! Every assertion here is a comparison with the real binary: enumerate the objects the index
//! describes, read each one through [`PackFile`], and check the bytes against `git cat-file`.
//! A pack produced by `git gc` exercises `OFS_DELTA` chains, the v2 index's fanout and offset
//! tables, and the entry-header varint — all of which a hand-written fixture would encode with
//! the same misunderstanding as the reader.

use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_core::hash::ObjectFormat;
use rivet_core::object::ObjectKind;
use rivet_core::pack::{index_pack, PackFile};

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a repository whose history deltifies: several revisions of a large, partly-changing
/// file, then `gc` so every object lands in one pack.
fn repository_with_pack() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "--quiet", "-b", "main"]);
    git(root, &["config", "user.email", "a@example.com"]);
    git(root, &["config", "user.name", "A"]);
    git(root, &["config", "core.autocrlf", "false"]);

    let base: String = (0..400)
        .map(|i| format!("line {i} of a file that will be revised repeatedly\n"))
        .collect();
    std::fs::write(root.join("big.txt"), &base).unwrap();
    std::fs::write(root.join("small.txt"), "hello\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "one"]);

    for revision in 0..4 {
        let mut text = base.clone();
        text.push_str(&format!("revision {revision}\n"));
        std::fs::write(root.join("big.txt"), text).unwrap();
        std::fs::write(root.join("note.txt"), format!("note {revision}\n")).unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", &format!("revision {revision}")]);
    }

    git(root, &["gc", "--quiet", "--aggressive", "--prune=now"]);

    let pack_dir = root.join(".git/objects/pack");
    let pack = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|x| x == "pack").unwrap_or(false))
        .expect("a pack after gc");
    (dir, pack)
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}

#[test]
fn reads_every_object_git_packed() {
    if !have_git() {
        return;
    }
    let (dir, pack_path) = repository_with_pack();
    let root = dir.path();

    let owned = PackFile::open_path(&pack_path, ObjectFormat::Sha1).expect("open pack");
    let pack = owned.pack().expect("borrow pack");
    pack.verify_checksum().expect("pack checksum");
    assert!(pack.object_count() > 0);

    let index = pack.index().expect("index beside pack");
    assert_eq!(index.len() as u32, pack.object_count());
    assert_eq!(index.pack_checksum(), pack.checksum());

    let mut deltas = 0usize;
    for i in 0..index.len() {
        let entry = index.entry(i).expect("entry");
        let (kind, data) = pack
            .get_at_offset(entry.offset, None)
            .expect("resolve entry by offset");
        let hex = entry.oid.to_hex();
        let expected = git(
            root,
            &["cat-file", kind_name(kind), std::str::from_utf8(&hex).unwrap()],
        );
        assert_eq!(
            data,
            expected,
            "content mismatch for {} {}",
            kind_name(kind),
            hex
        );
        if pack.entry_at(entry.offset).unwrap().kind.is_delta() {
            deltas += 1;
        }
    }
    assert!(deltas > 0, "gc should have produced delta chains");

    // Reading by id goes through the index and re-hashes the result.
    let first = index.entry(0).unwrap();
    let object = pack.get(&first.oid, None).expect("get by id");
    assert_eq!(object.id(ObjectFormat::Sha1).unwrap(), first.oid);
}

#[test]
fn index_pack_reproduces_gits_index() {
    if !have_git() {
        return;
    }
    let (_dir, pack_path) = repository_with_pack();
    let pack_bytes = std::fs::read(&pack_path).unwrap();
    let idx_path = pack_path.with_extension("idx");
    let git_index_bytes = std::fs::read(&idx_path).unwrap();

    let result = index_pack(&pack_bytes, ObjectFormat::Sha1, None).expect("index the pack");
    assert!(!result.thin);
    assert_eq!(result.pack_checksum, {
        let owned = PackFile::open_path(&pack_path, ObjectFormat::Sha1).unwrap();
        owned.pack().unwrap().checksum()
    });
    assert_eq!(
        result.index.as_index().len(),
        result.entries.len(),
        "every entry should appear once"
    );

    let ours = result.index.as_index();
    let theirs = rivet_core::pack::PackIndex::parse(&git_index_bytes, ObjectFormat::Sha1)
        .expect("git index parses");
    assert_eq!(ours.len(), theirs.len());
    for i in 0..ours.len() {
        assert_eq!(ours.entry(i), theirs.entry(i), "entry {i} differs");
    }

    // The bytes we would write must be accepted by git, not merely self-consistent.
    let written = ours.write();
    let dir = tempfile::tempdir().unwrap();
    let stem = dir.path().join("check");
    std::fs::write(stem.with_extension("pack"), &pack_bytes).unwrap();
    std::fs::write(stem.with_extension("idx"), &written).unwrap();
    let output = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(stem.with_extension("idx"))
        .output()
        .expect("git verify-pack");
    assert!(
        output.status.success(),
        "git rejected our index: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
