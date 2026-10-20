//! Compare the sideband reader/writer and pack storage against real `git`.
//!
//! The only way to know that our demultiplexer understands the real protocol is to talk to the
//! real server: `git upload-pack` is spawned, asked for a pack over protocol v0 with
//! `side-band-64k`, and its bytes are fed to us unchanged. The pack we recover must be the same
//! pack `git index-pack` names, and the objects we store must be readable by `git` afterwards —
//! which is also the check that our `.idx` serialisation is right.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rivet_core::hash::ObjectFormat;
use rivet_core::odb::Odb;
use rivet_core::remote::sideband::{
    ChannelWrite, SidebandOptions, demultiplex, frame, store_pack,
};
use rivet_core::remote::transport::ProtocolVersion;
use rivet_core::remote::{PktLine, SidebandKind, SidebandReader};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, contents).expect("write");
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Rivet Test"]);
    git(dir, &["config", "user.email", "rivet@example.invalid"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

/// A repository with two commits, a nested directory, and a second revision of a file, so the
/// pack the server builds contains both full objects and deltas.
fn source_repo(root: &Path) -> PathBuf {
    let repo = root.join("source");
    init_repo(&repo);
    write(&repo.join("README.md"), "rivet\n");
    write(&repo.join("src/main.rs"), "fn main() {}\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let big = "line\n".repeat(400);
    write(&repo.join("src/main.rs"), &format!("fn main() {{\n{big}}}\n"));
    write(&repo.join("docs/notes.txt"), "notes\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "second"]);
    repo
}

fn git_dir_of(repo: &Path) -> PathBuf {
    let text = git(repo, &["rev-parse", "--absolute-git-dir"]);
    PathBuf::from(text.trim())
}

/// Read the ref advertisement up to the flush, returning the head commit id.
fn read_advertisement<R: Read>(reader: &mut R) -> String {
    let mut head = None;
    loop {
        let line = PktLine::read_from(reader).expect("advertisement packet");
        match line {
            PktLine::Flush => break,
            PktLine::Data(data) => {
                let text = String::from_utf8_lossy(&data).into_owned();
                if head.is_none() {
                    head = Some(text.split(' ').next().unwrap_or_default().to_string());
                }
            }
            _ => {}
        }
    }
    head.expect("advertisement names a ref")
}

/// Run `git upload-pack` over a pipe and return the raw bytes of the response, starting just after
/// the `NAK` that precedes the pack.
fn upload_pack_stream(repo: &Path, want: &str) -> Vec<u8> {
    // Git 2.56's `upload-pack` requires the *git directory* itself: it calls `enter_repo` in strict
    // mode, which does not descend into `.git` of a working tree. Passing the worktree path makes
    // the server exit with "does not appear to be a git repository" before writing any bytes.
    let mut child = Command::new("git")
        .arg("upload-pack")
        .arg("--strict")
        .arg(git_dir_of(repo))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("upload-pack starts");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    let advertised = read_advertisement(&mut stdout);
    assert_eq!(advertised, want, "advertisement head");

    // Every request line is a pkt-line: a four-digit hex length *including* itself, then the
    // payload. Writing the payload bare makes the server read "want" as a length prefix and die
    // with "bad line length character: want".
    let mut request = Vec::new();
    request.extend_from_slice(
        &PktLine::new_data(format!("want {want} side-band-64k ofs-delta\n").as_bytes())
            .encode()
            .expect("encode want"),
    );
    request.extend_from_slice(&PktLine::Flush.encode().expect("encode flush"));
    request.extend_from_slice(&PktLine::new_data(b"done\n").encode().expect("encode done"));
    stdin.write_all(&request).expect("write request");
    stdin.flush().expect("flush request");

    // The server answers `done` with `NAK` as an ordinary pkt-line; everything after it is the
    // sideband-framed pack.
    let line = match PktLine::read_from(&mut stdout) {
        Ok(line) => line,
        Err(e) => {
            let mut err = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut err);
            }
            panic!("nak packet: {e}; server stderr: {err}");
        }
    };
    match &line {
        PktLine::Data(data) => assert_eq!(
            String::from_utf8_lossy(&data).trim(),
            "NAK",
            "server sends NAK before the pack"
        ),
        other => panic!("expected NAK, got {other:?}"),
    }

    let mut rest = Vec::new();
    stdout.read_to_end(&mut rest).expect("read pack stream");
    drop(stdin);
    let status = child.wait().expect("upload-pack exits");
    assert!(status.success(), "upload-pack failed: {status:?}");
    rest
}

fn options() -> SidebandOptions {
    SidebandOptions {
        enabled: true,
        max_payload: 65516,
        want_progress: false,
        version: ProtocolVersion::V0,
    }
}

#[test]
fn demultiplexes_the_pack_upload_pack_sends() {
    if !have_git() {
        return;
    }
    let tmp = TempDir::new();
    let repo = source_repo(tmp.path());
    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let stream = upload_pack_stream(&repo, &head);
    let mut cursor = std::io::Cursor::new(&stream[..]);
    let pack = demultiplex(&mut cursor, &options(), None).expect("demultiplex");

    // The pack's own trailer is the name Git gives the file; `git index-pack` prints the same
    // value for the same bytes, so this compares our reader against Git rather than against
    // itself.
    let pack_file = tmp.join("received.pack");
    std::fs::write(&pack_file, &pack).expect("write pack");
    let in_repo = tmp.join("namer");
    init_repo(&in_repo);
    let pathspec = pack_file.to_string_lossy().into_owned();
    let named = git(&in_repo, &["index-pack", &pathspec]);
    let named = named.trim().to_string();

    // `git index-pack <file>` prints the bare pack hash (no `pack-` prefix); compare hashes so
    // the assertion is about the checksum, not about Git's filename formatting.
    let result = rivet_core::pack::index_pack(&pack, ObjectFormat::Sha1, None).expect("index pack");
    assert_eq!(
        result.pack_checksum.to_hex(),
        named.trim_start_matches("pack-"),
        "our pack checksum names the pack the same way git does"
    );
    assert!(!result.thin, "the server sent a self-contained pack");

    // Every object the pack claims to hold must also be what the source repository holds.
    let count = git(&repo, &["rev-list", "--objects", "--all"]);
    let count = count.lines().count();
    assert_eq!(
        result.entries.len(),
        count,
        "one entry per reachable object: {} entries, {} objects",
        result.entries.len(),
        count
    );
}

#[test]
fn stores_a_fetched_pack_that_git_can_read() {
    if !have_git() {
        return;
    }
    let tmp = TempDir::new();
    let repo = source_repo(tmp.path());
    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let stream = upload_pack_stream(&repo, &head);

    let dest = tmp.join("dest");
    init_repo(&dest);
    let dest_git = git_dir_of(&dest);
    let mut odb = Odb::open(&dest_git, ObjectFormat::Sha1).expect("odb");

    let mut cursor = std::io::Cursor::new(&stream[..]);
    let stored = store_pack(
        &mut cursor,
        &options(),
        &mut odb,
        ObjectFormat::Sha1,
        None,
    )
    .expect("store pack");

    let name = format!("pack-{}", stored.pack_checksum.to_hex());
    let pack_path = dest_git.join("objects/pack").join(format!("{name}.pack"));
    let idx_path = dest_git.join("objects/pack").join(format!("{name}.idx"));
    assert!(pack_path.is_file(), "pack written to {}", pack_path.display());
    assert!(idx_path.is_file(), "index written to {}", idx_path.display());

    // `git verify-pack` parses the idx we wrote and checks it against the pack. A wrong offset or
    // a wrong CRC fails here, which no in-memory test of ours would catch.
    // Absolute: `git verify-pack` resolves a relative path against its cwd, and the index lives in
    // `.git/objects/pack`, not at the top of the worktree.
    let idx_name = idx_path.to_string_lossy().into_owned();
    git(&dest, &["verify-pack", "--stat-only", &idx_name]);

    // And the objects are real: read a blob and a commit back through Git itself.
    let readme = git(&repo, &["rev-parse", "HEAD:README.md"]);
    let readme = readme.trim();
    let stored_readme = git(&dest, &["cat-file", "-p", readme]);
    assert_eq!(stored_readme, "rivet\n", "blob bytes survive the round trip");

    let commit_type = git(&dest, &["cat-file", "-t", &head]);
    assert_eq!(commit_type.trim(), "commit");
    let source_commit = git(&repo, &["cat-file", "-p", &head]);
    let dest_commit = git(&dest, &["cat-file", "-p", &head]);
    assert_eq!(dest_commit, source_commit, "commit object is byte-identical");

    // `git fsck` is the final word on whether the repository we wrote is sound.
    git(&dest, &["fsck", "--strict"]);
}

#[test]
fn frames_a_write_the_reader_can_parse() {
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let params = ChannelWrite {
        channel: SidebandKind::Pack,
        max_payload: rivet_core::remote::MAX_SIDEBAND_DATA,
    };
    let framed = frame(params, &payload).expect("frame");

    // Whole packets: every length field counts the four header bytes and the channel byte, and no
    // packet may claim more data than the capability allows.
    let mut offset = 0usize;
    let mut packets = 0usize;
    while offset < framed.len() {
        let len = usize::from_str_radix(
            std::str::from_utf8(&framed[offset..offset + 4]).expect("hex header"),
            16,
        )
        .expect("hex length");
        assert!(len <= params.max_payload + 5, "packet length {len} in range");
        assert!(len >= 5, "every sideband packet carries a channel byte");
        offset += len;
        packets += 1;
    }
    assert_eq!(offset, framed.len(), "framing has no trailing bytes");
    assert_eq!(packets, payload.len().div_ceil(params.max_payload));

    let mut cursor = std::io::Cursor::new(framed);
    let mut reader = SidebandReader::new(&mut cursor, true);
    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().expect("packet") {
        assert_eq!(packet.channel, SidebandKind::Pack);
        out.extend_from_slice(&packet.data);
    }
    assert_eq!(out, payload, "payload survives framing unchanged");
}

/// A scratch directory under `/tmp`, deliberately not `TMPDIR`.
///
/// `git upload-pack` resolves its repository argument through `enter_repo`, which refuses any path
/// component that begins with a dot. The agent's `TMPDIR` lives under `~/.hermes`, so a fixture
/// there is rejected with "does not appear to be a git repository" even though it is a perfectly
/// good repository. `/tmp` has no such component. The directory deletes itself on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let base = Path::new("/tmp").join(format!(
            "rivet-sideband-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).expect("mkdir");
        TempDir(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
