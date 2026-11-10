//! `ls-remote` options that are refused by name rather than ignored.

use std::process::Command;

#[test]
fn unsupported_options_are_refused_by_name() {
    let rivet = env!("CARGO_BIN_EXE_rivet");
    for args in [
        &["ls-remote", "--upload-pack=x", "."][..],
        &["ls-remote", "-o", "x", "."],
    ] {
        let out = Command::new(rivet).args(args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--upload-pack and --server-option are not implemented"),
            "{args:?}: {stderr}"
        );
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn deref_sort_keys_sort_like_the_plain_key() {
    let rivet = env!("CARGO_BIN_EXE_rivet");
    let dir = std::env::temp_dir().join(format!("rivet-lsr-deref-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        Command::new(rivet)
            .current_dir(&dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@a")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@a")
            .output()
            .unwrap()
    };
    assert!(run(&["init", "-q", "."]).status.success());
    std::fs::write(dir.join("f"), "x").unwrap();
    assert!(run(&["add", "f"]).status.success());
    assert!(run(&["commit", "-q", "-m", "c"]).status.success());
    assert!(run(&["branch", "B"]).status.success());
    assert!(run(&["tag", "Alpha"]).status.success());
    for (star, plain) in [
        ("*refname", "refname"),
        ("-*refname", "-refname"),
        ("v:*refname", "v:refname"),
        ("*refname:short", "refname:short"),
    ] {
        let a = run(&["ls-remote", &format!("--sort={star}"), "."]);
        let b = run(&["ls-remote", &format!("--sort={plain}"), "."]);
        assert!(a.status.success(), "{star}");
        assert_eq!(a.stdout, b.stdout, "{star}");
    }
    for key in ["*refname:lstrip=2", "*refname:rstrip=1"] {
        let refused = run(&["ls-remote", &format!("--sort={key}"), "."]);
        assert!(!refused.status.success(), "{key}");
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains(&format!("--sort={key} is not implemented")),
            "{key}"
        );
    }
    let bad = run(&["ls-remote", "--sort=*refname:bogus", "."]);
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("unrecognized %(*refname) argument: bogus")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
