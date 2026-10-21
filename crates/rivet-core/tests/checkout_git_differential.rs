//! Compare `worktree::checkout` against real `git checkout`.
//!
//! The plan is a value and the apply is a sequence of file operations, so the only honest test
//! is the work tree itself: after Rivet moves a repository from one commit to another, the files
//! must be byte-for-byte, mode-for-mode identical to what `git checkout` produces from the same
//! pair of commits. Every expectation below is produced by Git, never written by hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use bstr::BStr;

use rivet_core::hash::ObjectId;
use rivet_core::repo::Repository;
use rivet_core::worktree::checkout::{CheckoutOptions, CheckoutPlan};
use rivet_core::worktree::StatCache;

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

fn write_exec(path: &Path, contents: &str) {
    write(path, contents);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Everything a work tree contains, as path → (executable, bytes or link target).
type Snapshot = BTreeMap<String, (bool, Vec<u8>)>;

fn snapshot(root: &Path) -> Snapshot {
    fn walk(root: &Path, dir: &Path, out: &mut Snapshot) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            if meta.is_dir() {
                walk(root, &path, out);
                continue;
            }
            use std::os::unix::fs::PermissionsExt;
            let executable = meta.permissions().mode() & 0o111 != 0;
            let bytes = if meta.file_type().is_symlink() {
                std::fs::read_link(&path)
                    .expect("readlink")
                    .to_string_lossy()
                    .into_owned()
                    .into_bytes()
            } else {
                std::fs::read(&path).expect("read")
            };
            out.insert(relative, (executable, bytes));
        }
    }
    let mut out = Snapshot::new();
    walk(root, root, &mut out);
    out
}

/// `main` has the old tree, `other` the new one, and `other` exercises every action a checkout
/// can take: create, update, delete, a rename, a mode change, a nested directory that does not
/// exist yet, and a symlink.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "core.autocrlf", "false"]);

    write(&root.join("keep.txt"), "unchanged\n");
    write(&root.join("update.txt"), "before\n");
    write(&root.join("delete.txt"), "gone in other\n");
    write(&root.join("rename-old.txt"), "same bytes, new name\n");
    write(&root.join("mode.txt"), "#!/bin/sh\necho hi\n");
    write(&root.join("dir/nested.txt"), "nested\n");
    std::os::unix::fs::symlink("keep.txt", root.join("link")).expect("symlink");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "base"]);

    git(&root, &["checkout", "-q", "-b", "other"]);
    write(&root.join("update.txt"), "after\n");
    std::fs::remove_file(root.join("delete.txt")).expect("rm");
    std::fs::rename(
        root.join("rename-old.txt"),
        root.join("rename-new.txt"),
    )
    .expect("rename");
    write_exec(&root.join("mode.txt"), "#!/bin/sh\necho hi\n");
    write(&root.join("created.txt"), "brand new\n");
    write(&root.join("deep/a/b/c.txt"), "three levels down\n");
    std::fs::remove_file(root.join("link")).expect("rm link");
    std::os::unix::fs::symlink("update.txt", root.join("link")).expect("symlink");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "other"]);
    git(&root, &["checkout", "-q", "main"]);

    (dir, root)
}

fn tree_id(root: &Path, revision: &str) -> ObjectId {
    let hex = git(root, &["rev-parse", &format!("{revision}^{{tree}}")]);
    let hex = hex.trim();
    ObjectId::from_hex(
        rivet_core::hash::ObjectFormat::Sha1,
        BStr::new(hex.as_bytes()),
    )
    .expect("object id")
}

/// The tree Git itself produces for `other`, in a separate clone so the two checkouts cannot
/// share an index or a work tree.
fn git_checkout_of_other(root: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("clone");
    let output = Command::new("git")
        .args(["clone", "-q", "--no-local"])
        .arg(root)
        .arg(&dest)
        .output()
        .expect("git clone");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    git(&dest, &["checkout", "-q", "other"]);
    (dir, dest)
}

/// Move `root` from `main` to `other` with Rivet, then compare the work tree and the index with
/// Git's own checkout of the same commit.
fn rivet_checkout(root: &Path, pathspec: &str) -> Result<(), rivet_core::error::Error> {
    let repo = Repository::open_or_discover(root).expect("open");
    let worktree = repo.worktree().expect("worktree");
    let format = repo.format();
    let from = repo
        .head_tree()
        .expect("head tree")
        .map(|tree| tree.id(format))
        .transpose()
        .expect("tree id");
    let to = tree_id(root, "other");

    let mut options = CheckoutOptions::new();
    if !pathspec.is_empty() {
        options.pathspec(BStr::new(pathspec.as_bytes()));
    }
    let plan = CheckoutPlan::plan(format, from, to, repo.odb(), &options).expect("plan");
    plan.validate(worktree, repo.index(), repo.odb(), &mut StatCache::new(), &options)?;
    plan.apply(worktree, repo.odb())?;
    Ok(())
}

#[test]
fn applies_every_action_kind_like_git() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    let (_expected_dir, expected) = git_checkout_of_other(&root);

    rivet_checkout(&root, "").expect("checkout");

    let ours = snapshot(&root);
    let theirs = snapshot(&expected);
    assert_eq!(
        ours.keys().collect::<Vec<_>>(),
        theirs.keys().collect::<Vec<_>>(),
        "the two checkouts must have the same paths"
    );
    for (path, their) in &theirs {
        assert_eq!(Some(&their.0), ours.get(path).map(|o| &o.0), "exec bit of {path}");
        assert_eq!(
            Some(&their.1),
            ours.get(path).map(|o| &o.1),
            "contents of {path}"
        );
    }
}

#[test]
fn index_entries_match_the_tree_git_checked_out() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    // `git ls-files -s` is the authority on what the index means, and it does not depend on
    // Rivet's own index reader agreeing with Git's extension blocks.
    let git_index = {
        let (_d, expected) = git_checkout_of_other(&root);
        let mut map = BTreeMap::new();
        for line in git(&expected, &["ls-files", "-s"]).lines() {
            let (meta, path) = line.split_once('\t').expect("tab-separated");
            let mut fields = meta.split(' ');
            let mode = fields.next().expect("mode");
            let oid = fields.next().expect("oid");
            map.insert(
                path.to_string(),
                (
                    u32::from_str_radix(mode, 8).expect("octal mode"),
                    ObjectId::from_hex(
                        rivet_core::hash::ObjectFormat::Sha1,
                        BStr::new(oid.as_bytes()),
                    )
                    .expect("oid"),
                ),
            );
        }
        map
    };

    let repo = Repository::open_or_discover(&root).expect("open");
    let format = repo.format();
    let plan = CheckoutPlan::plan(
        format,
        repo.head_tree()
            .expect("head tree")
            .map(|t| t.id(format))
            .transpose()
            .expect("tree id"),
        tree_id(&root, "other"),
        repo.odb(),
        &CheckoutOptions::new(),
    )
    .expect("plan");
    // The plan only names the paths it touches, so the new index is the old one with the plan
    // applied on top — which is exactly how a caller is expected to use `index_entries`.
    let mut ours: BTreeMap<String, (u32, ObjectId)> = repo
        .index()
        .entries()
        .iter()
        .map(|entry| (entry.path.to_string(), (entry.mode.to_u32(), entry.oid)))
        .collect();
    for entry in plan.entries() {
        match entry.action {
            rivet_core::worktree::checkout::CheckoutAction::Remove => {
                ours.remove(&entry.path.to_string());
            }
            rivet_core::worktree::checkout::CheckoutAction::Keep
            | rivet_core::worktree::checkout::CheckoutAction::Create
            | rivet_core::worktree::checkout::CheckoutAction::Update => {
                if let (Some(oid), Some(mode)) = (entry.oid, entry.mode)
                    && mode != rivet_core::object::EntryMode::Commit {
                        ours.insert(entry.path.to_string(), (mode.to_u32(), oid));
                    }
            }
        }
    }
    // And the ids the plan hands back must agree with what the merge just produced.
    for entry in plan.index_entries(repo.odb()).expect("index entries") {
        assert_eq!(
            Some(&(entry.mode.to_u32(), entry.oid)),
            ours.get(&entry.path.to_string()),
            "index entry for {}",
            entry.path
        );
    }

    assert_eq!(ours, git_index);
}

#[test]
fn a_pathspec_leaves_the_rest_of_the_work_tree_alone() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    let before = snapshot(&root);

    rivet_checkout(&root, "deep").expect("checkout");
    let after = snapshot(&root);

    // Only `deep/…` may differ; every other path keeps its old bytes.
    for (path, (exec, bytes)) in &before {
        if path.starts_with("deep/") {
            continue;
        }
        assert_eq!(
            Some(&(*exec, bytes.clone())),
            after.get(path),
            "{path} must not be touched by a pathspec checkout"
        );
    }
    assert_eq!(
        Some(&(false, b"three levels down\n".to_vec())),
        after.get("deep/a/b/c.txt"),
        "the pathspec's own path is written"
    );
}

#[test]
fn a_modified_file_is_refused_unless_forced() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("update.txt"), "local edit\n");

    let repo = Repository::open_or_discover(&root).expect("open");
    let worktree = repo.worktree().expect("worktree");
    let format = repo.format();
    let plan = CheckoutPlan::plan(
        format,
        repo.head_tree()
            .expect("head tree")
            .map(|t| t.id(format))
            .transpose()
            .expect("tree id"),
        tree_id(&root, "other"),
        repo.odb(),
        &CheckoutOptions::new(),
    )
    .expect("plan");

    let error = plan
        .validate(
            worktree,
            repo.index(),
            repo.odb(),
            &mut StatCache::new(),
            &CheckoutOptions::new(),
        )
        .expect_err("a modified file must stop the checkout");
    match error {
        rivet_core::error::Error::DirtyWorktree { paths } => {
            assert!(paths.iter().any(|p| p == "update.txt"), "{paths:?}");
        }
        other => panic!("unexpected error: {other}"),
    }

    // `--force` skips the safety check entirely, which is the only way this checkout may run.
    let mut force = CheckoutOptions::new();
    force.force(true);
    plan.validate(worktree, repo.index(), repo.odb(), &mut StatCache::new(), &force)
        .expect("force skips the dirty check");
}

#[test]
fn an_untracked_file_in_the_way_is_refused() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("created.txt"), "user data that is not tracked\n");

    let repo = Repository::open_or_discover(&root).expect("open");
    let worktree = repo.worktree().expect("worktree");
    let format = repo.format();
    let plan = CheckoutPlan::plan(
        format,
        repo.head_tree()
            .expect("head tree")
            .map(|t| t.id(format))
            .transpose()
            .expect("tree id"),
        tree_id(&root, "other"),
        repo.odb(),
        &CheckoutOptions::new(),
    )
    .expect("plan");

    let obstructed = plan
        .untracked_obstructions(worktree, repo.index())
        .expect("obstructions");
    assert!(
        obstructed.iter().any(|p| p == "created.txt"),
        "an untracked file at a path the plan creates must be reported: {obstructed:?}"
    );

    let error = plan
        .validate(
            worktree,
            repo.index(),
            repo.odb(),
            &mut StatCache::new(),
            &CheckoutOptions::new(),
        )
        .expect_err("an untracked file in the way must stop the checkout");
    assert!(matches!(
        error,
        rivet_core::error::Error::UntrackedOverwrite(_)
    ));
}

#[test]
fn a_staged_change_is_refused() {
    if !have_git() {
        return;
    }
    let (_dir, root) = repository();
    write(&root.join("update.txt"), "staged edit\n");
    git(&root, &["add", "update.txt"]);

    let repo = Repository::open_or_discover(&root).expect("open");
    let worktree = repo.worktree().expect("worktree");
    let format = repo.format();
    let plan = CheckoutPlan::plan(
        format,
        repo.head_tree()
            .expect("head tree")
            .map(|t| t.id(format))
            .transpose()
            .expect("tree id"),
        tree_id(&root, "other"),
        repo.odb(),
        &CheckoutOptions::new(),
    )
    .expect("plan");

    let error = plan
        .validate(
            worktree,
            repo.index(),
            repo.odb(),
            &mut StatCache::new(),
            &CheckoutOptions::new(),
        )
        .expect_err("a staged change must stop the checkout");
    assert!(matches!(
        error,
        rivet_core::error::Error::DirtyWorktree { .. }
    ));
}
