//! A read-modify-write of a config file touches only the lines it changes (what `git config` does).

use bstr::{BStr, ByteSlice};
use rivet_core::config::Config;

const FILE: &[u8] = b"# top\n[splitIndex]\n\t# c\n\tsharedIndexExpire = now   # trailing\n\n[Core]\n\tFoo = 1\n\tbar = 2\n# between\n[remote \"o\"]\n\turl = x\n# tail\n";

fn edit(f: impl FnOnce(&mut Config)) -> String {
    let mut cfg = Config::parse(FILE).unwrap();
    f(&mut cfg);
    cfg.to_bytes().to_str().unwrap().to_owned()
}

#[test]
fn an_unchanged_file_round_trips_byte_for_byte() {
    assert_eq!(Config::parse(FILE).unwrap().to_bytes(), FILE);
}

#[test]
fn replacing_a_value_rewrites_only_its_line() {
    let out = edit(|c| {
        c.set(BStr::new("splitindex.sharedindexexpire"), b"never")
            .unwrap()
    });
    assert_eq!(
        out,
        String::from_utf8_lossy(FILE).replace(
            "sharedIndexExpire = now   # trailing",
            "sharedindexexpire = never"
        )
    );
}

#[test]
fn a_new_key_goes_after_its_sections_last_key_and_a_new_section_after_the_tail() {
    let out = edit(|c| c.set(BStr::new("core.baz"), b"3").unwrap());
    assert!(out.contains("\tbar = 2\n\tbaz = 3\n# between\n"), "{out}");
    let out = edit(|c| c.set(BStr::new("new.k"), b"v").unwrap());
    assert!(out.ends_with("# tail\n[new]\n\tk = v\n"), "{out}");
}

#[test]
fn unset_keeps_headers_and_comments_and_remove_section_takes_the_section_with_its_comments() {
    let mut cfg = Config::parse(FILE).unwrap();
    assert!(cfg.remove(BStr::new("remote.o.url")).unwrap());
    let out = cfg.to_bytes();
    assert!(
        out.ends_with(b"[remote \"o\"]\n# tail\n"),
        "{}",
        out.as_bstr()
    );

    let mut cfg = Config::parse(FILE).unwrap();
    assert!(cfg.remove_section(BStr::new("core"), None));
    assert_eq!(
        cfg.to_bytes().as_bstr(),
        "# top\n[splitIndex]\n\t# c\n\tsharedIndexExpire = now   # trailing\n\n[remote \"o\"]\n\turl = x\n# tail\n"
    );
}

#[test]
fn a_continued_value_joins_its_lines_and_survives_an_edit_elsewhere() {
    let file = b"[a]\n\tk = one\\\n  two\n\tn = a\\\n\tb # c\n\tr = s\\\n\n\tq = 1\n";
    let mut cfg = Config::parse(file).unwrap();
    assert_eq!(cfg.get(BStr::new("a.k")).unwrap().unwrap(), "one  two");
    assert_eq!(cfg.get(BStr::new("a.n")).unwrap().unwrap(), "a\tb");
    assert_eq!(cfg.get(BStr::new("a.r")).unwrap().unwrap(), "s");
    assert_eq!(cfg.get(BStr::new("a.q")).unwrap().unwrap(), "1");
    cfg.set(BStr::new("a.q"), b"5").unwrap();
    let out = cfg.to_bytes();
    assert!(out.starts_with(&file[..file.len() - "\tq = 1\n".len()]));
    assert!(out.ends_with(b"\tq = 5\n"));
}

#[test]
fn an_unquoted_hash_or_semicolon_ends_a_value_where_it_stands() {
    let file = b"[a]\n\tk = x#y\n\tm = x;y\n\tu = https://h/#frag\n\tq = \"x#y\"#z\n\te = #c\n";
    let cfg = Config::parse(file).unwrap();
    for (key, want) in [
        ("a.k", "x"),
        ("a.m", "x"),
        ("a.u", "https://h/"),
        ("a.q", "x#y"),
        ("a.e", ""),
    ] {
        assert_eq!(cfg.get(BStr::new(key)).unwrap().unwrap(), want, "{key}");
    }
}

#[test]
fn an_emptied_section_takes_its_empty_duplicate_headers_with_it() {
    let unset = |file: &[u8]| {
        let mut cfg = Config::parse(file).unwrap();
        assert!(cfg.remove(BStr::new("a.x")).unwrap());
        cfg.to_bytes().to_str().unwrap().to_owned()
    };
    // Values taken from `git config --unset a.x` on the same files.
    assert_eq!(
        unset(b"[a]\n\tx = 1\n[a]\n[c]\n\tz = 3\n"),
        "[c]\n\tz = 3\n"
    );
    assert_eq!(unset(b"[a]\n[a]\n\tx = 1\n[c]\n"), "[c]\n");
    assert_eq!(unset(b"[a]\n\tx = 1\n[a]\n"), "");
    assert_eq!(
        unset(b"[a]\n\tx = 1\n[d]\n[c]\n\tz = 3\n"),
        "[d]\n[c]\n\tz = 3\n"
    );
    // A comment, or another entry, on either side keeps the header.
    assert_eq!(unset(b"[a]\n\tx = 1\n[a]\n# c\n"), "[a]\n[a]\n# c\n");
    assert_eq!(unset(b"# t\n[a]\n\tx = 1\n[b]\n"), "# t\n[a]\n[b]\n");
    assert_eq!(
        unset(b"[a]\n\tx = 1\n[a]\n\tw = 1\n"),
        "[a]\n[a]\n\tw = 1\n"
    );
}
