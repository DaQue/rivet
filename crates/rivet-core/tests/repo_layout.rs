//! End-to-end tests for [`Repository`]: layout, staging, tree building, committing and discovery.

use rivet_core::{
    find_git_dir, gitfile_target, AnyObject, CommitOptions, EntryMode, HeadState, IndexEntry,
    IndexEntryFlags, InitOptions, Object, ObjectFormat, ObjectId, ObjectKind, Repository,
    RepositoryKind, Signature, Tree,
};

fn entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry {
        ctime_seconds: 0,
        ctime_nanos: 0,
        mtime_seconds: 0,
        mtime_nanos: 0,
        dev: 0,
        ino: 0,
        mode: EntryMode::Blob,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: IndexEntryFlags::empty(),
        extended_flags: 0,
        path: path.into(),
    }
}

fn signature() -> Signature {
    Signature {
        name: "Test".into(),
        email: "test@example.com".into(),
        when: 1_700_000_000,
        tz_offset_minutes: 0,
    }
}

fn tree_bytes(repo: &Repository, oid: &ObjectId) -> Vec<u8> {
    match repo.odb().read_typed(oid, ObjectKind::Tree).unwrap() {
        Object::Tree(bytes) => bytes,
        other => panic!("expected a tree, got {:?}", other.kind()),
    }
}

#[test]
fn init_stage_tree_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path(), &InitOptions::new()).unwrap();

    assert_eq!(repo.kind(), RepositoryKind::Normal);
    assert_eq!(repo.worktree_path(), Some(dir.path()));
    assert!(!repo.is_bare());
    match repo.head().unwrap() {
        HeadState::Unborn { branch } => assert_eq!(branch.as_bytes(), b"refs/heads/main"),
        other => panic!("expected an unborn head, got {other:?}"),
    }
    assert_eq!(repo.head_commit().unwrap(), None);
    assert!(dir.path().join(".git/HEAD").is_file());
    assert!(dir.path().join(".git/objects").is_dir());

    let alpha = repo
        .odb()
        .write_raw(ObjectKind::Blob, b"alpha\n")
        .unwrap();
    let beta = repo.odb().write_raw(ObjectKind::Blob, b"beta\n").unwrap();
    repo.index_mut().insert(entry("a.txt", alpha));
    repo.index_mut().insert(entry("dir/b.txt", beta));
    repo.write_index().unwrap();

    repo.reload_index().unwrap();
    assert_eq!(repo.index().len(), 2);

    let tree_id = repo.write_tree().unwrap();
    let root_bytes = tree_bytes(&repo, &tree_id);
    let tree = Tree::parse(repo.format(), &root_bytes).unwrap();
    let names: Vec<&[u8]> = tree.entries.iter().map(|e| e.name.as_slice()).collect();
    assert_eq!(names, vec![&b"a.txt"[..], &b"dir"[..]]);
    let dir_entry = tree.get(&"dir".into()).unwrap();
    assert_eq!(dir_entry.mode, EntryMode::Tree);
    let subtree_bytes = tree_bytes(&repo, &dir_entry.oid);
    let subtree = Tree::parse(repo.format(), &subtree_bytes).unwrap();
    assert_eq!(subtree.entries.len(), 1);
    assert_eq!(subtree.entries[0].name.as_slice(), b"b.txt");

    let options = CommitOptions {
        author: Some(signature()),
        committer: Some(signature()),
        message: "initial commit\n".into(),
        parents: Vec::new(),
        allow_empty: false,
        allow_empty_message: false,
        amend: false,
    };
    let commit_id = repo.commit(&options).unwrap();

    match repo.head().unwrap() {
        HeadState::Attached { branch, oid } => {
            assert_eq!(branch.as_bytes(), b"refs/heads/main");
            assert_eq!(oid, commit_id);
        }
        other => panic!("expected an attached head, got {other:?}"),
    }
    assert_eq!(repo.head_commit().unwrap(), Some(commit_id));

    match repo.odb().read_typed(&commit_id, ObjectKind::Commit).unwrap().parse().unwrap() {
        AnyObject::Commit(commit) => {
            assert_eq!(commit.tree, tree_id);
            assert!(commit.parents.is_empty());
            assert_eq!(commit.message, b"initial commit\n");
            assert_eq!(commit.author.name.as_slice(), b"Test");
        }
        other => panic!("expected a commit, got {:?}", other.kind()),
    }

    let head_tree = repo.head_tree().unwrap().expect("head tree");
    assert_eq!(head_tree.entries.len(), 2);
}

#[test]
fn second_commit_parents_the_first_and_amend_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path(), &InitOptions::new()).unwrap();

    let blob = repo.odb().write_raw(ObjectKind::Blob, b"one\n").unwrap();
    repo.index_mut().insert(entry("f.txt", blob));
    let first = repo
        .commit(&CommitOptions {
            author: Some(signature()),
            committer: Some(signature()),
            message: "one\n".into(),
            parents: Vec::new(),
            allow_empty: false,
            allow_empty_message: false,
            amend: false,
        })
        .unwrap();

    let blob = repo.odb().write_raw(ObjectKind::Blob, b"two\n").unwrap();
    repo.index_mut().insert(entry("f.txt", blob));
    let second = repo
        .commit(&CommitOptions {
            author: Some(signature()),
            committer: Some(signature()),
            message: "two\n".into(),
            parents: Vec::new(),
            allow_empty: false,
            allow_empty_message: false,
            amend: false,
        })
        .unwrap();
    assert_ne!(first, second);

    match repo.odb().read_typed(&second, ObjectKind::Commit).unwrap().parse().unwrap() {
        AnyObject::Commit(commit) => assert_eq!(commit.parents, vec![first]),
        other => panic!("expected a commit, got {:?}", other.kind()),
    }

    let blob = repo.odb().write_raw(ObjectKind::Blob, b"three\n").unwrap();
    repo.index_mut().insert(entry("f.txt", blob));
    let amended = repo
        .commit(&CommitOptions {
            author: Some(signature()),
            committer: Some(signature()),
            message: "two amended\n".into(),
            parents: Vec::new(),
            allow_empty: false,
            allow_empty_message: false,
            amend: true,
        })
        .unwrap();

    match repo.odb().read_typed(&amended, ObjectKind::Commit).unwrap().parse().unwrap() {
        AnyObject::Commit(commit) => {
            assert_eq!(commit.parents, vec![first], "amend keeps the original parents");
            assert_eq!(commit.message, b"two amended\n");
        }
        other => panic!("expected a commit, got {:?}", other.kind()),
    }
    assert_eq!(repo.head_commit().unwrap(), Some(amended));
}

#[test]
fn unchanged_tree_is_refused_without_allow_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path(), &InitOptions::new()).unwrap();
    let blob = repo.odb().write_raw(ObjectKind::Blob, b"same\n").unwrap();
    repo.index_mut().insert(entry("f.txt", blob));

    let options = CommitOptions {
        author: Some(signature()),
        committer: Some(signature()),
        message: "one\n".into(),
        parents: Vec::new(),
        allow_empty: false,
        allow_empty_message: false,
        amend: false,
    };
    repo.commit(&options).unwrap();

    let err = repo.commit(&options).unwrap_err();
    assert!(matches!(err, rivet_core::Error::Message(_)), "got {err:?}");

    let allowed = CommitOptions {
        allow_empty: true,
        allow_empty_message: false,
        ..options
    };
    repo.commit(&allowed).unwrap();
}

#[test]
fn discovery_follows_a_relative_gitfile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = Repository::init(&root.join("repo"), &InitOptions::new()).unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join(".git"), b"gitdir: ../repo/.git\n").unwrap();

    let found = find_git_dir(&work).unwrap();
    assert_eq!(found, root.join("repo/.git"));
    assert_eq!(gitfile_target(&work.join(".git")).unwrap(), found);

    let opened = Repository::discover(&work).unwrap();
    assert_eq!(opened.git_dir(), repo.git_dir());
}

#[test]
fn malformed_gitfile_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let gitfile = dir.path().join(".git");
    std::fs::write(&gitfile, b"# not a gitfile\n").unwrap();
    let err = gitfile_target(&gitfile).unwrap_err();
    assert!(matches!(err, rivet_core::Error::InvalidPath(_)), "got {err:?}");

    std::fs::write(&gitfile, b"gitdir: /does/not/exist\n").unwrap();
    assert!(matches!(
        gitfile_target(&gitfile).unwrap_err(),
        rivet_core::Error::InvalidPath(_)
    ));
}

#[test]
fn bare_init_has_no_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = InitOptions::new();
    options.bare(true);
    let repo = Repository::init(dir.path(), &options).unwrap();

    assert_eq!(repo.kind(), RepositoryKind::Bare);
    assert!(repo.is_bare());
    assert_eq!(repo.worktree_path(), None);
    assert!(matches!(
        repo.worktree().unwrap_err(),
        rivet_core::Error::BareRepository { .. }
    ));
    assert!(dir.path().join("HEAD").is_file());
    assert_eq!(repo.format(), ObjectFormat::Sha1);
}

#[test]
fn opening_a_directory_that_is_not_a_repository_fails_explicitly() {
    let dir = tempfile::tempdir().unwrap();

    // A directory that exists but holds no `HEAD` is the mistyped-path case the constructor
    // promises to name, rather than the one that fails later as a missing ref.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    match Repository::open_with(&empty, None, None).unwrap_err() {
        rivet_core::Error::NotARepository(path) => {
            assert!(format!("{path:?}").contains("empty"), "error should name the path, got {path:?}")
        }
        other => panic!("expected NotARepository, got {other:?}"),
    }

    // A path that does not exist at all fails the same way, not with a panic.
    match Repository::open_with(&dir.path().join("missing"), None, None).unwrap_err() {
        rivet_core::Error::NotARepository(_) => {}
        other => panic!("expected NotARepository, got {other:?}"),
    }
}

#[test]
fn a_separate_work_tree_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join("gitdir");
    let work_tree = dir.path().join("wt");
    std::fs::create_dir_all(&work_tree).unwrap();

    let mut options = InitOptions::new();
    options.work_tree = Some(work_tree.clone());
    let repo = Repository::init(&git_dir, &options).unwrap();
    assert_eq!(repo.worktree_path(), Some(work_tree.as_path()));

    // The work tree is outside the repository root, so it can only come back from the config
    // that `init` wrote; a constructor that guessed `<root>` would report the git dir here.
    let reopened = Repository::open_with(&git_dir.join(".git"), None, None).unwrap();
    assert_eq!(reopened.worktree_path(), Some(work_tree.as_path()));
}

#[test]
fn an_empty_commit_message_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path(), &InitOptions::new()).unwrap();
    let blob = repo.odb().write_raw(ObjectKind::Blob, b"x\n").unwrap();
    repo.index_mut().insert(entry("f.txt", blob));

    for message in ["", "   ", "\n\t\n"] {
        let err = repo
            .commit(&CommitOptions {
                author: Some(signature()),
                committer: Some(signature()),
                message: message.into(),
                parents: Vec::new(),
                allow_empty: false,
                allow_empty_message: false,
                amend: false,
            })
            .unwrap_err();
        assert!(matches!(err, rivet_core::Error::Message(_)), "got {err:?}");
    }
    assert_eq!(repo.head_commit().unwrap(), None, "no commit was created");
}

#[test]
fn amend_with_explicit_parents_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(dir.path(), &InitOptions::new()).unwrap();
    let blob = repo.odb().write_raw(ObjectKind::Blob, b"x\n").unwrap();
    repo.index_mut().insert(entry("f.txt", blob));

    let root = repo
        .commit(&CommitOptions {
            author: Some(signature()),
            committer: Some(signature()),
            message: "root\n".into(),
            parents: Vec::new(),
            allow_empty: false,
            allow_empty_message: false,
            amend: false,
        })
        .unwrap();

    let err = repo
        .commit(&CommitOptions {
            author: Some(signature()),
            committer: Some(signature()),
            message: "amended\n".into(),
            parents: vec![root],
            allow_empty: true,
            allow_empty_message: false,
            amend: true,
        })
        .unwrap_err();
    assert!(matches!(err, rivet_core::Error::Message(_)), "got {err:?}");
    assert_eq!(repo.head_commit().unwrap(), Some(root), "HEAD is unchanged");
}
