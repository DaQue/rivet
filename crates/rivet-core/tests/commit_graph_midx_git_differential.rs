//! Rivet's readers for `commit-graph` and `multi-pack-index` are checked against files real git
//! wrote: every commit's tree, parents, time and generation come out equal to what the commit
//! objects say, a damaged file is ignored instead of trusted, and nothing is ever written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_core::commit_graph::CommitGraph;
use rivet_core::midx::MultiPackIndex;
use rivet_core::{Commit, ObjectFormat, ObjectId, Odb};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rivet-cg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A history with branches, an octopus merge (so the EDGE chunk is used), and a root of its own.
fn build(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    let commit = |name: &str, tick: u32| {
        std::fs::write(dir.join(name), format!("{name}\n")).unwrap();
        git(dir, &["add", name]);
        let date = format!("{} +0000", 1_600_000_000 + tick * 100);
        let out = Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@x")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@x")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["commit", "-q", "-m", name])
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    commit("a", 1);
    commit("b", 2);
    for (i, branch) in ["x", "y", "z"].iter().enumerate() {
        git(dir, &["checkout", "-q", "-b", branch, "main"]);
        commit(&format!("{branch}1"), 10 + i as u32);
        commit(&format!("{branch}2"), 20 + i as u32);
    }
    git(dir, &["checkout", "-q", "main"]);
    commit("c", 30);
    git(
        dir,
        &["merge", "-q", "--no-ff", "-m", "octopus", "x", "y", "z"],
    );
    commit("d", 40);
    git(dir, &["checkout", "-q", "--orphan", "other"]);
    git(dir, &["rm", "-rfq", "."]);
    commit("lone", 50);
    git(dir, &["checkout", "-q", "main"]);
    git(
        dir,
        &[
            "merge",
            "-q",
            "--allow-unrelated-histories",
            "-m",
            "join",
            "other",
        ],
    );
}

fn all_commits(dir: &Path) -> Vec<ObjectId> {
    git(dir, &["rev-list", "--all"])
        .lines()
        .map(|l| ObjectId::from_hex(ObjectFormat::Sha1, l.as_bytes().into()).unwrap())
        .collect()
}

/// Topological levels computed from the commit objects alone.
fn levels(odb: &Odb, commits: &[ObjectId]) -> HashMap<ObjectId, u32> {
    fn level(odb: &Odb, oid: ObjectId, memo: &mut HashMap<ObjectId, u32>) -> u32 {
        if let Some(l) = memo.get(&oid) {
            return *l;
        }
        let object = odb.read(&oid).unwrap();
        let commit = Commit::parse(ObjectFormat::Sha1, object.data()).unwrap();
        let l = 1 + commit
            .parents
            .iter()
            .map(|p| level(odb, *p, memo))
            .max()
            .unwrap_or(0);
        memo.insert(oid, l);
        l
    }
    let mut memo = HashMap::new();
    for oid in commits {
        level(odb, *oid, &mut memo);
    }
    memo
}

fn check_graph(dir: &Path, graph: &CommitGraph) {
    let odb = Odb::open(&dir.join(".git"), ObjectFormat::Sha1).unwrap();
    let commits = all_commits(dir);
    let expected_levels = levels(&odb, &commits);
    assert_eq!(graph.len(), commits.len());
    for oid in &commits {
        let object = odb.read(oid).unwrap();
        let commit = Commit::parse(ObjectFormat::Sha1, object.data()).unwrap();
        let got = graph
            .commit(oid)
            .unwrap_or_else(|| panic!("{oid} missing from the graph"));
        assert_eq!(got.tree, commit.tree, "{oid} tree");
        assert_eq!(got.parents, commit.parents, "{oid} parents");
        assert_eq!(got.commit_time, commit.committer.when, "{oid} time");
        assert_eq!(got.generation, expected_levels[oid], "{oid} generation");
        assert_eq!(graph.generation(oid), Some(expected_levels[oid]));
    }
    // A tree, a blob or a made-up id is not a commit in the graph.
    let tree = ObjectId::from_hex(
        ObjectFormat::Sha1,
        git(dir, &["rev-parse", "HEAD^{tree}"])
            .trim()
            .as_bytes()
            .into(),
    )
    .unwrap();
    assert!(!graph.contains(&tree));
    assert!(graph.commit(&ObjectId::zero(ObjectFormat::Sha1)).is_none());
}

#[test]
fn single_file_graph_matches_the_commit_objects() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("single");
    build(&dir);
    git(
        &dir,
        &["commit-graph", "write", "--reachable", "--changed-paths"],
    );
    let graph = CommitGraph::open(&dir.join(".git/objects"), ObjectFormat::Sha1).expect("graph");
    check_graph(&dir, &graph);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn split_chain_matches_the_commit_objects() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("split");
    build(&dir);
    git(
        &dir,
        &["commit-graph", "write", "--reachable", "--split=no-merge"],
    );
    std::fs::write(dir.join("later"), "later\n").unwrap();
    git(&dir, &["add", "later"]);
    git(&dir, &["commit", "-q", "-m", "later"]);
    git(
        &dir,
        &["commit-graph", "write", "--reachable", "--split=no-merge"],
    );
    git(&dir, &["checkout", "-q", "-b", "tip"]);
    std::fs::write(dir.join("tip"), "tip\n").unwrap();
    git(&dir, &["add", "tip"]);
    git(&dir, &["commit", "-q", "-m", "tip"]);
    git(
        &dir,
        &["commit-graph", "write", "--reachable", "--split=no-merge"],
    );
    let chain =
        std::fs::read_to_string(dir.join(".git/objects/info/commit-graphs/commit-graph-chain"))
            .unwrap();
    assert!(
        chain.lines().count() >= 3,
        "expected a chain, got {chain:?}"
    );
    let graph = CommitGraph::open(&dir.join(".git/objects"), ObjectFormat::Sha1).expect("graph");
    check_graph(&dir, &graph);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_graphs_are_ignored_not_trusted() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("damaged");
    build(&dir);
    git(&dir, &["commit-graph", "write", "--reachable"]);
    let path = dir.join(".git/objects/info/commit-graph");
    let good = std::fs::read(&path).unwrap();
    let objects = dir.join(".git/objects");
    assert!(CommitGraph::open(&objects, ObjectFormat::Sha1).is_some());

    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    cases.push(("truncated", good[..good.len() / 2].to_vec()));
    cases.push(("header only", good[..8].to_vec()));
    cases.push(("empty", Vec::new()));
    let mut bad_sig = good.clone();
    bad_sig[0] = b'X';
    cases.push(("signature", bad_sig));
    let mut bad_version = good.clone();
    bad_version[4] = 9;
    cases.push(("version", bad_version));
    let mut bad_hash = good.clone();
    bad_hash[5] = 2;
    cases.push(("hash version", bad_hash));
    let mut bad_chunk = good.clone();
    bad_chunk[8 + 4 + 7] = 0xff; // the first chunk's offset runs far past the file
    cases.push(("chunk offset", bad_chunk));
    let mut bad_fanout = good.clone();
    let fanout = u32::from_be_bytes(good[8 + 4 + 4..8 + 4 + 8].try_into().unwrap()) as usize;
    bad_fanout[fanout + 4 * 10..fanout + 4 * 10 + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    cases.push(("fanout not monotonic", bad_fanout));
    for (name, bytes) in cases {
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            CommitGraph::open(&objects, ObjectFormat::Sha1).is_none(),
            "{name}: a damaged graph must be ignored"
        );
    }
    // Opening never rewrites the file.
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, &good).unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let _ = CommitGraph::open(&objects, ObjectFormat::Sha1);
    assert_eq!(std::fs::read(&path).unwrap(), good);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multi_pack_index_finds_every_packed_object() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("midx");
    build(&dir);
    // Two packs so the index has to say which one holds an object.
    git(&dir, &["repack", "-q", "-a", "-d"]);
    std::fs::write(dir.join("extra"), "extra\n").unwrap();
    git(&dir, &["add", "extra"]);
    git(&dir, &["commit", "-q", "-m", "extra"]);
    git(&dir, &["repack", "-q", "-d"]);
    git(&dir, &["prune"]);
    git(&dir, &["multi-pack-index", "write"]);
    let pack_dir = dir.join(".git/objects/pack");
    let midx = MultiPackIndex::open(&pack_dir, ObjectFormat::Sha1).expect("midx");
    assert!(midx.pack_names().len() >= 2);
    let odb = Odb::open(&dir.join(".git"), ObjectFormat::Sha1).unwrap();
    let listed: Vec<ObjectId> = git(
        &dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(|l| ObjectId::from_hex(ObjectFormat::Sha1, l.as_bytes().into()).unwrap())
    .collect();
    assert_eq!(midx.len(), listed.len());
    for oid in &listed {
        let (name, offset) = midx.lookup(oid).unwrap_or_else(|| panic!("{oid} missing"));
        assert!(
            name.ends_with(".idx") && pack_dir.join(name).is_file(),
            "{name}"
        );
        assert!(offset >= 12);
        assert!(odb.contains(oid));
    }
    assert!(!midx.contains(&ObjectId::zero(ObjectFormat::Sha1)));

    let path = pack_dir.join("multi-pack-index");
    let good = std::fs::read(&path).unwrap();
    for bytes in [good[..good.len() - 30].to_vec(), good[..10].to_vec(), {
        let mut v = good.clone();
        v[0] = b'Z';
        v
    }] {
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, bytes).unwrap();
        assert!(MultiPackIndex::open(&pack_dir, ObjectFormat::Sha1).is_none());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_odb_uses_the_files_only_while_they_can_be_trusted() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("odb");
    build(&dir);
    git(&dir, &["repack", "-q", "-a", "-d"]);
    git(&dir, &["prune"]);
    git(&dir, &["commit-graph", "write", "--reachable"]);
    git(&dir, &["multi-pack-index", "write"]);
    let head = ObjectId::from_hex(
        ObjectFormat::Sha1,
        git(&dir, &["rev-parse", "HEAD"]).trim().as_bytes().into(),
    )
    .unwrap();
    let odb = Odb::open(&dir.join(".git"), ObjectFormat::Sha1).unwrap();
    assert!(odb.commit_graph().is_some() && odb.multi_pack_index().is_some());
    let entry = odb.graph_commit(&head).expect("HEAD is in the graph");
    assert_eq!(entry.parents.len(), 2);
    assert!(odb.contains(&head));

    // A shallow boundary hides parents the graph lists: git stops using the graph, and so do we.
    std::fs::write(dir.join(".git/shallow"), format!("{head}\n")).unwrap();
    let odb = Odb::open(&dir.join(".git"), ObjectFormat::Sha1).unwrap();
    assert!(odb.commit_graph().is_none());
    std::fs::remove_file(dir.join(".git/shallow")).unwrap();

    // A multi-pack-index naming a pack that is gone must not vouch for its objects.
    let pack_dir = dir.join(".git/objects/pack");
    let pack = std::fs::read_dir(&pack_dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".idx"))
        .unwrap()
        .path();
    std::fs::remove_file(&pack).unwrap();
    std::fs::remove_file(pack.with_extension("pack")).unwrap();
    let odb = Odb::open(&dir.join(".git"), ObjectFormat::Sha1).unwrap();
    assert!(odb.multi_pack_index().is_none());
    assert!(!odb.contains(&head));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incremental_multi_pack_index_chain_finds_every_object() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("midx-chain");
    build(&dir);
    git(&dir, &["repack", "-q", "-a", "-d"]);
    git(&dir, &["multi-pack-index", "write", "--incremental"]);
    std::fs::write(dir.join("extra"), "extra\n").unwrap();
    git(&dir, &["add", "extra"]);
    git(&dir, &["commit", "-q", "-m", "extra"]);
    git(&dir, &["repack", "-q", "-d"]);
    git(&dir, &["prune"]);
    git(&dir, &["multi-pack-index", "write", "--incremental"]);
    let pack_dir = dir.join(".git/objects/pack");
    let chain = std::fs::read_to_string(pack_dir.join("multi-pack-index.d/multi-pack-index-chain"))
        .unwrap();
    assert_eq!(chain.lines().count(), 2, "two layers");
    assert!(!pack_dir.join("multi-pack-index").exists());

    let midx = MultiPackIndex::open(&pack_dir, ObjectFormat::Sha1).expect("chain");
    assert_eq!(midx.all_pack_names().len(), 2);
    let listed: Vec<ObjectId> = git(
        &dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(|l| ObjectId::from_hex(ObjectFormat::Sha1, l.as_bytes().into()).unwrap())
    .collect();
    assert_eq!(midx.len(), listed.len());
    let mut by_pack = std::collections::HashSet::new();
    for oid in &listed {
        let (name, offset) = midx.lookup(oid).unwrap_or_else(|| panic!("{oid} missing"));
        assert!(pack_dir.join(name).is_file(), "{name}");
        assert!(offset >= 12);
        by_pack.insert(name.to_string());
    }
    assert_eq!(by_pack.len(), 2, "objects come from both layers' packs");
    // The Odb, which uses the chain, still reads every object.
    let odb = Odb::open(&dir.join(".git"), ObjectFormat::Sha1).unwrap();
    assert!(odb.multi_pack_index().is_some());
    for oid in &listed {
        assert!(odb.contains(oid));
    }
    // Damage to any layer drops the whole chain.
    let layer = chain.lines().next().unwrap();
    let path = pack_dir.join(format!("multi-pack-index.d/multi-pack-index-{layer}.midx"));
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, bytes).unwrap();
    assert!(MultiPackIndex::open(&pack_dir, ObjectFormat::Sha1).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_trailer_that_is_not_the_hash_of_the_body_makes_the_files_untrusted() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("trailer");
    build(&dir);
    git(&dir, &["repack", "-q", "-a", "-d"]);
    git(&dir, &["multi-pack-index", "write"]);
    git(&dir, &["commit-graph", "write", "--reachable"]);
    let objects = dir.join(".git/objects");
    assert!(CommitGraph::open(&objects, ObjectFormat::Sha1).is_some());
    assert!(MultiPackIndex::open(&objects.join("pack"), ObjectFormat::Sha1).is_some());
    for path in [
        objects.join("info/commit-graph"),
        objects.join("pack/multi-pack-index"),
    ] {
        // A change in the middle of the file with a trailer that still says the old hash.
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes.len() / 2;
        bytes[at] ^= 0x80;
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, bytes).unwrap();
    }
    assert!(CommitGraph::open(&objects, ObjectFormat::Sha1).is_none());
    assert!(MultiPackIndex::open(&objects.join("pack"), ObjectFormat::Sha1).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_config_turns_the_commit_graph_and_the_multi_pack_index_off() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("config");
    build(&dir);
    git(&dir, &["repack", "-q", "-a", "-d"]);
    git(&dir, &["multi-pack-index", "write"]);
    git(&dir, &["commit-graph", "write", "--reachable"]);
    let open = || rivet_core::Repository::open_or_discover(&dir).unwrap();
    {
        let repository = open();
        assert!(repository.odb().commit_graph().is_some());
        assert!(repository.odb().multi_pack_index().is_some());
    }
    git(&dir, &["config", "core.commitGraph", "false"]);
    {
        let repository = open();
        assert!(repository.odb().commit_graph().is_none());
        assert!(repository.odb().multi_pack_index().is_some());
    }
    git(&dir, &["config", "core.multiPackIndex", "false"]);
    git(&dir, &["config", "core.commitGraph", "true"]);
    {
        let repository = open();
        assert!(repository.odb().commit_graph().is_some());
        assert!(repository.odb().multi_pack_index().is_none());
        // Without the index every pack is searched on its own, and every object is still found.
        let head = git(&dir, &["rev-parse", "HEAD"]);
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, head.trim().as_bytes().into()).unwrap();
        assert!(repository.odb().contains(&oid));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn is_ancestor_with_generation_pruning_agrees_with_git() {
    if !have_git() {
        return;
    }
    let dir = temp_dir("ancestor");
    build(&dir);
    git(&dir, &["commit-graph", "write", "--reachable"]);
    let repository = rivet_core::Repository::open_or_discover(&dir).unwrap();
    assert!(repository.odb().commit_graph().is_some());
    let commits = all_commits(&dir);
    assert!(commits.len() > 10);
    for a in &commits {
        for b in &commits {
            let status = Command::new("git")
                .current_dir(&dir)
                .args([
                    "merge-base",
                    "--is-ancestor",
                    &a.to_string(),
                    &b.to_string(),
                ])
                .status()
                .unwrap();
            assert_eq!(
                repository.is_ancestor(*a, *b).unwrap(),
                status.success(),
                "{a} {b}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
