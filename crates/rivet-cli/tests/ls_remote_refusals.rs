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
fn sort_keys_beyond_refname_are_refused_by_name() {
    let rivet = env!("CARGO_BIN_EXE_rivet");
    for key in [
        "version:refname",
        "-v:refname",
        "*refname",
        "refname:short",
        "refname:dirname",
        "refname:basename",
    ] {
        let out = Command::new(rivet)
            .args(["ls-remote", &format!("--sort={key}"), "."])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{key}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("--sort={key} is not implemented")),
            "{key}: {stderr}"
        );
        assert!(out.stdout.is_empty(), "{key}");
    }
}
