//! Differential tests: parse and re-serialise real Git index entries and compare against the bytes
//! Git itself wrote.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use rivet_core::index::{Index, IndexEntry, IndexVersion, Stage, compare_entries};
use rivet_core::object::{ObjectKind, hash_object};
use rivet_core::ObjectFormat;

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

fn git(args: &[&str], dir: &Path) -> Output {
    let out = Command::new("git")
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example")
        .env("HOME", dir.as_os_str())
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write_file(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    git(&["add", name], dir);
}

fn parse_entries(data: &[u8], format: ObjectFormat) -> Vec<IndexEntry> {
    let version = IndexVersion::from_u32(u32::from_be_bytes([
        data[4], data[5], data[6], data[7],
    ]))
    .unwrap();
    let mut entries = Vec::new();
    let mut pos = 12usize;
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    for _ in 0..count {
        let (e, consumed) = IndexEntry::parse(format, version, &data[pos..]).unwrap();
        entries.push(e);
        pos += consumed;
    }
    entries
}

fn entry_slice(data: &[u8], pos: usize, len: usize) -> &[u8] {
    &data[pos..pos + len]
}

#[test]
fn v2_entry_bytes_match_git_exactly() {
    let dir = tempfile_dir("v2");
    write_file(&dir, "a", "hi");
    write_file(&dir, "a.txt", "hello\n");

    let data = std::fs::read(dir.join(".git/index")).unwrap();
    assert!(data.len() > 12);
    let entries = parse_entries(&data, ObjectFormat::Sha1);
    assert_eq!(entries.len(), 2);

    // Entry 1: path "a", mode 100644, size 2.
    let e0 = &entries[0];
    assert_eq!(e0.path.as_slice(), b"a");
    assert_eq!(e0.mode.to_u32(), 0o100644);
    assert_eq!(e0.size, 2);
    // The stored oid is what `git add` computed for "hi".
    let (expected_oid, _) = hash_object(ObjectFormat::Sha1, ObjectKind::Blob, b"hi").unwrap();
    assert_eq!(e0.oid, expected_oid);
    // Flags: stage 0, name length 1, no extended word.
    assert_eq!(e0.flags.bits(), 0x0001);
    assert_eq!(e0.extended_flags, 0);

    // Entry 2: path "a.txt", size 6.
    let e1 = &entries[1];
    assert_eq!(e1.path.as_slice(), b"a.txt");
    assert_eq!(e1.size, 6);
    let (expected_oid, _) = hash_object(ObjectFormat::Sha1, ObjectKind::Blob, b"hello\n").unwrap();
    assert_eq!(e1.oid, expected_oid);
    assert_eq!(e1.flags.bits(), 0x0005);

    // Re-serialisation must reproduce each entry slice exactly — this catches any
    // endianness slip, the NUL-exclusion or the padding rule.
    let pos0 = 12usize;
    let (e, consumed) =
        IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &data[pos0..]).unwrap();
    let slice = entry_slice(&data, pos0, consumed);
    assert_eq!(e.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap(), slice);

    let pos1 = pos0 + consumed;
    let (e, consumed) =
        IndexEntry::parse(ObjectFormat::Sha1, IndexVersion::V2, &data[pos1..]).unwrap();
    let slice = entry_slice(&data, pos1, consumed);
    assert_eq!(e.to_bytes(ObjectFormat::Sha1, IndexVersion::V2).unwrap(), slice);
}

#[test]
fn sha256_object_format_entry_width() {
    let dir = tempfile_dir("sha256");
    let out = Command::new("git")
        .args(["init", "-q", "--object-format=sha256"])
        .env("HOME", dir.as_os_str())
        .current_dir(&dir)
        .output()
        .unwrap();
    if !out.status.success() {
        eprintln!(
            "skipping: git refused --object-format=sha256: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    write_file(&dir, "a", "hi");

    let data = std::fs::read(dir.join(".git/index")).unwrap();
    let version = IndexVersion::from_u32(u32::from_be_bytes([
        data[4], data[5], data[6], data[7],
    ]))
    .unwrap();
    let (e, consumed) = IndexEntry::parse(ObjectFormat::Sha256, version, &data[12..]).unwrap();
    // A 32-byte oid: fixed 40 + 32 + 2 flags = 74, path "a" + NUL = 1, total 76, padded to 80.
    assert_eq!(e.oid.as_bytes().len(), 32);
    assert_eq!(e.path.as_slice(), b"a");
    assert_eq!(e.mode.to_u32(), 0o100644);
    let (expected_oid, _) = hash_object(ObjectFormat::Sha256, ObjectKind::Blob, b"hi").unwrap();
    assert_eq!(e.oid, expected_oid);
    let slice = entry_slice(&data, 12, consumed);
    assert_eq!(e.to_bytes(ObjectFormat::Sha256, version).unwrap(), slice);
}

#[test]
fn extended_word_survives_round_trip() {
    let dir = tempfile_dir("extended");
    write_file(&dir, "one", "1");
    write_file(&dir, "two", "2");
    std::fs::write(dir.join("three"), b"").unwrap();
    git(&["update-index", "--skip-worktree", "one"], &dir);
    git(&["add", "-N", "three"], &dir);

    let data = std::fs::read(dir.join(".git/index")).unwrap();
    let version = IndexVersion::from_u32(u32::from_be_bytes([
        data[4], data[5], data[6], data[7],
    ]))
    .unwrap();
    assert_eq!(version, IndexVersion::V3);
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    assert_eq!(count, 3);

    let mut pos = 12usize;
    let mut skip = None;
    let mut intent = None;
    for _ in 0..count {
        let (e, consumed) = IndexEntry::parse(ObjectFormat::Sha1, version, &data[pos..]).unwrap();
        let slice = entry_slice(&data, pos, consumed);
        if e.path.as_slice() == b"one" {
            skip = Some((e, slice));
        } else if e.path.as_slice() == b"three" {
            intent = Some((e, slice));
        }
        pos += consumed;
    }

    let (e, slice) = skip.unwrap();
    // skip-worktree: flags bit 14 + extended word with bit 14 set.
    assert_eq!(e.flags.bits(), 0x4003);
    assert_eq!(e.extended_flags & 0x4000, 0x4000);
    assert_eq!(e.to_bytes(ObjectFormat::Sha1, version).unwrap(), slice);

    let (e, slice) = intent.unwrap();
    // intent-to-add: flags bit 14, extended word with bit 13 (0x2000) set. Git records the empty
    // blob's oid for an intent-to-add entry of an empty file (older Gits wrote the zero oid).
    assert_eq!(e.flags.bits(), 0x4005);
    assert_eq!(e.extended_flags & 0x2000, 0x2000);
    let (empty_blob, _) = hash_object(ObjectFormat::Sha1, ObjectKind::Blob, b"").unwrap();
    let zero = format!("{}", "0".repeat(40));
    assert!(
        e.oid == empty_blob || e.oid.to_string() == zero,
        "unexpected intent-to-add oid {}",
        e.oid
    );
    assert_eq!(e.to_bytes(ObjectFormat::Sha1, version).unwrap(), slice);

    // The entries are still in sorted order and parse the whole file back.
    let entries = parse_entries(&data, ObjectFormat::Sha1);
    assert_eq!(entries.len(), 3);
    assert_eq!(
        compare_entries(&entries[0], &entries[1]),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        compare_entries(&entries[1], &entries[2]),
        std::cmp::Ordering::Less
    );
    assert!(entries[0].flags.is_extended());
    assert!(entries[0].stage() == Stage::Normal);
}

/// Create a fresh, empty repository directory. Unique per call within the process so tests running
/// in parallel cannot see each other's index.
fn tempfile_dir(tag: &str) -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "rivet-idx-test-{}-{}-{}",
        std::process::id(),
        tag,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&["init", "-q"], &dir);
    dir
}

/// A committed repository's index carries Git's `TREE` extension, which sits between the entries
/// and the trailing checksum. Reading it as another entry header made every real index fail to
/// parse with "extension extends past end", so this test pins the boundary.
#[test]
fn parses_a_git_index_that_has_extensions() {
    let dir = tempfile_dir("extensions");
    write_file(&dir, "a.txt", "hello\n");
    std::fs::create_dir_all(dir.join("dir")).unwrap();
    write_file(&dir, "dir/b.txt", "world\n");
    git(&["commit", "-qm", "one"], &dir);

    let data = std::fs::read(dir.join(".git/index")).unwrap();
    let index = Index::parse(ObjectFormat::Sha1, &data).expect("a Git index must parse");
    assert_eq!(index.entries().len(), 2);
    assert!(
        !index.extensions().is_empty(),
        "this index is expected to carry the TREE extension"
    );
    // Re-serialising keeps the extension, so a Rivet `add` does not silently strip Git's cache.
    let round_tripped = index.to_bytes(ObjectFormat::Sha1);
    let again = Index::parse(ObjectFormat::Sha1, &round_tripped).expect("round trip parses");
    assert_eq!(again.extensions().len(), index.extensions().len());
}
