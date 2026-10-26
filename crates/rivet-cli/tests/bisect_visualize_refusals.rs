//! `bisect visualize` / `view` run `git log` over the suspect range; the two forms that start another
//! program are refused by name: no arguments with a display and `gitk` on `PATH`, and a first argument
//! that is not an option (git runs it as a command). (The `log` form is the differential scenario
//! `s_bisect_visualize`.)

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, args: &[&str], envs: &[(&str, &std::ffi::OsStr)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rivet"));
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("DISPLAY");
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("spawn")
}

#[test]
fn gitk_and_command_forms_are_refused_by_name() {
    let dir = std::env::temp_dir().join(format!("rivet-bisect-view-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/gitk"), b"#!/bin/sh\n").unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(run(&repo, &["init", "-q"], &[]).status.success());

    let out = run(&repo, &["bisect", "view", "tig"], &[]);
    assert_eq!(out.status.code(), Some(128));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("visualize running 'tig' as a command is not implemented"),
        "{err}"
    );

    let bin = dir.join("bin");
    let envs: [(&str, &std::ffi::OsStr); 2] =
        [("DISPLAY", ":0".as_ref()), ("PATH", bin.as_os_str())];
    let out = run(&repo, &["bisect", "visualize"], &envs);
    assert_eq!(out.status.code(), Some(128));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("visualize with gitk is not implemented"),
        "{err}"
    );

    // With no gitk on PATH git falls back to `git log`; nothing is being bisected, so it is silent.
    let none = dir.join("empty");
    std::fs::create_dir_all(&none).unwrap();
    let envs: [(&str, &std::ffi::OsStr); 2] =
        [("DISPLAY", ":0".as_ref()), ("PATH", none.as_os_str())];
    let out = run(&repo, &["bisect", "visualize"], &envs);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}
