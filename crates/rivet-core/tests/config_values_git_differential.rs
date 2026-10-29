//! How a config value is read — escapes outside quotes, quotes anywhere in the value, comments —
//! compared with `git config -f <file> --get`.

use std::process::Command;

use bstr::BStr;
use rivet_core::Config;

#[test]
fn values_read_like_git() {
    let text = "[x]\n\
        \ta = foo \"bar baz\" qux\n\
        \tb = \"quoted # not comment\"\n\
        \tc = plain # comment\n\
        \td = back\\\\slash\n\
        \te = \"a\\\"b\"\n\
        \tf = \"ab\"cd\"ef gh\"\n\
        \tg = a\\tb\\nc\n\
        \th = trailing   \n\
        \ti = \"  padded  \"\n\
        [diff \"sect\"]\n\
        \txfuncname = ^[a-z]+\\n!^beta\n";
    let dir = std::env::temp_dir().join(format!("rivet-config-values-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("config");
    std::fs::write(&file, text).unwrap();
    let config = Config::parse(text.as_bytes()).unwrap();
    for key in [
        "x.a",
        "x.b",
        "x.c",
        "x.d",
        "x.e",
        "x.f",
        "x.g",
        "x.h",
        "x.i",
        "diff.sect.xfuncname",
    ] {
        let out = Command::new("/usr/bin/git")
            .args(["config", "-f"])
            .arg(&file)
            .args(["--get", key])
            .output()
            .unwrap();
        assert!(out.status.success(), "{key}");
        let mut expected = out.stdout;
        assert_eq!(expected.pop(), Some(b'\n'));
        let actual = config.get(BStr::new(key)).unwrap().unwrap();
        assert_eq!(
            String::from_utf8_lossy(actual),
            String::from_utf8_lossy(&expected),
            "{key}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
