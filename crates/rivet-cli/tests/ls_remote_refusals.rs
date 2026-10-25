//! `ls-remote` options that are refused by name rather than ignored.

use std::process::Command;

#[test]
fn unsupported_options_are_refused_by_name() {
    let rivet = env!("CARGO_BIN_EXE_rivet");
    for args in [
        &["ls-remote", "--sort=refname", "."][..],
        &["ls-remote", "--upload-pack=x", "."],
        &["ls-remote", "-o", "x", "."],
    ] {
        let out = Command::new(rivet).args(args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--sort, --upload-pack and --server-option are not implemented"),
            "{args:?}: {stderr}"
        );
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
