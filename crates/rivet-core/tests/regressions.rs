//! One test per bug found by the differential harness, named after the behaviour it protects.
//! They need no `git` binary: each asserts values the library itself returns.

use bstr::{BStr, BString};
use rivet_core::object::{CommitBuilder, Object, ObjectKind, Signature};
use rivet_core::revwalk::{SortOrder, WalkOptions, walk_oids};
use rivet_core::{ObjectFormat, ObjectId, Odb, RemoteUrl, parse_int};

#[test]
fn url_display_masks_the_password_but_config_form_keeps_it() {
    let url = RemoteUrl::parse(BStr::new("https://user:secret@example.com:8443/repo.git")).unwrap();
    assert_eq!(url.port, Some(8443));
    assert_eq!(url.host.as_deref().map(|h| h.as_slice()), Some(&b"example.com"[..]));
    assert_eq!(url.user.as_deref().map(|u| u.as_slice()), Some(&b"user"[..]));
    assert_eq!(format!("{url}"), "https://user:***@example.com:8443/repo.git");
    assert_eq!(url.redacted(), "https://user:***@example.com:8443/repo.git");
    assert_eq!(url.to_config_string(), "https://user:secret@example.com:8443/repo.git");
}

#[test]
fn url_parsing_accepts_absolute_paths_scp_syntax_and_ipv6() {
    let local = RemoteUrl::parse(BStr::new("/srv/git/repo.git")).unwrap();
    assert_eq!(local.path, "/srv/git/repo.git");
    let scp = RemoteUrl::parse(BStr::new("git@example.com:team/repo.git")).unwrap();
    assert_eq!(scp.user.as_deref().map(|u| u.as_slice()), Some(&b"git"[..]));
    assert_eq!(scp.path, "team/repo.git");
    let v6 = RemoteUrl::parse(BStr::new("ssh://[::1]:2222/repo")).unwrap();
    assert_eq!(v6.host.as_deref().map(|h| h.as_slice()), Some(&b"::1"[..]));
    assert_eq!(v6.port, Some(2222));
}

#[test]
fn config_integers_reject_suffix_overflow_instead_of_wrapping() {
    assert_eq!(parse_int(BStr::new("-1k")).unwrap(), -1024);
    assert_eq!(parse_int(BStr::new("2g")).unwrap(), 2 * 1024 * 1024 * 1024);
    assert!(parse_int(BStr::new("9223372036854775807g")).is_err());
    assert!(parse_int(BStr::new("99999999999999999999")).is_err());
}

#[test]
fn paths_are_quoted_the_way_git_quotes_them() {
    use rivet_core::ops::quote_path;
    assert_eq!(quote_path(b"plain/name.txt", false), "plain/name.txt");
    assert_eq!(quote_path(b"with space", false), "with space");
    assert_eq!(quote_path(b"with space", true), "\"with space\"");
    assert_eq!(quote_path(b"tab\tname", false), "\"tab\\tname\"");
    assert_eq!(quote_path("é".as_bytes(), false), "\"\\303\\251\"");
    assert_eq!(quote_path(b"quote\"d", false), "\"quote\\\"d\"");
}

#[test]
fn rename_display_factors_out_the_common_prefix_and_suffix() {
    use rivet_core::diff::pprint_rename;
    assert_eq!(pprint_rename(b"a.txt", b"b.txt"), "a.txt => b.txt");
    assert_eq!(pprint_rename(b"src/old/deep.txt", b"src/new/deep.txt"), "src/{old => new}/deep.txt");
    assert_eq!(pprint_rename(b"dir/a.c", b"dir/b.c"), "dir/{a.c => b.c}");
    assert_eq!(pprint_rename(b"a/x.rs", b"b/x.rs"), "{a => b}/x.rs");
}

fn signature(when: i64) -> Signature {
    Signature { name: BString::from("T"), email: BString::from("t@example.com"), when, tz_offset_minutes: 0 }
}

fn commit(odb: &Odb, tree: ObjectId, parents: &[ObjectId], when: i64, format: ObjectFormat) -> ObjectId {
    let mut builder = CommitBuilder::new();
    builder.tree(tree);
    for parent in parents {
        builder.parent(*parent);
    }
    builder.author(signature(when));
    builder.committer(signature(when));
    builder.message(b"m\n");
    let body = builder.build(format).unwrap();
    odb.write_raw(ObjectKind::Commit, &body).unwrap()
}

#[test]
fn objects_parse_in_a_sha256_repository_without_being_told_the_format() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("objects")).unwrap();
    let odb = Odb::open_objects_dir(&dir.path().join("objects"), ObjectFormat::Sha256).unwrap();
    let tree = odb.write(&Object::Tree(Vec::new())).unwrap();
    let first = commit(&odb, tree, &[], 1, ObjectFormat::Sha256);
    let second = commit(&odb, tree, &[first], 2, ObjectFormat::Sha256);
    assert_eq!(second.to_hex().len(), 64);
    let object = odb.read(&second).unwrap();
    match object.parse().unwrap() {
        rivet_core::AnyObject::Commit(c) => assert_eq!(c.parents, vec![first]),
        other => panic!("expected a commit, got {other:?}"),
    }
}

#[test]
fn history_walk_orders_by_date_and_honours_exclusions_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("objects")).unwrap();
    let odb = Odb::open_objects_dir(&dir.path().join("objects"), ObjectFormat::Sha1).unwrap();
    let tree = odb.write(&Object::Tree(Vec::new())).unwrap();
    let base = commit(&odb, tree, &[], 10, ObjectFormat::Sha1);
    let left = commit(&odb, tree, &[base], 30, ObjectFormat::Sha1);
    let right = commit(&odb, tree, &[base], 20, ObjectFormat::Sha1);
    let merge = commit(&odb, tree, &[left, right], 40, ObjectFormat::Sha1);

    let all = walk_oids(&odb, &[merge], &WalkOptions::new()).unwrap();
    assert_eq!(all, vec![merge, left, right, base]);

    let mut excluded = WalkOptions::new();
    excluded.exclude(left);
    assert_eq!(walk_oids(&odb, &[merge], &excluded).unwrap(), vec![merge, right]);

    let mut limited = WalkOptions::new();
    limited.max_count(2).skip(1);
    assert_eq!(walk_oids(&odb, &[merge], &limited).unwrap(), vec![left, right]);

    let mut reversed = WalkOptions::new();
    reversed.sort(SortOrder::Reverse);
    assert_eq!(walk_oids(&odb, &[merge], &reversed).unwrap(), vec![base, right, left, merge]);

    let mut first_parent = WalkOptions::new();
    first_parent.first_parent(true);
    assert_eq!(walk_oids(&odb, &[merge], &first_parent).unwrap(), vec![merge, left, base]);
}
