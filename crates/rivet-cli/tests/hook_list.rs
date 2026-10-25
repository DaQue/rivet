//! `hook list` knows only the hooks directory. A repository that defines hooks in its
//! configuration (`hook.<name>.command`) is refused by name, never listed incompletely. (The
//! supported cases are the differential scenario `s_hooks_list`.)

use std::process::Command;

#[test]
fn hook_list_refuses_config_defined_hooks() {
    let dir = std::env::temp_dir().join(format!("rivet-hook-list-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rivet = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rivet"))
            .args(args)
            .current_dir(&dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("spawn")
    };
    assert!(rivet(&["init", "-q"]).status.success());
    assert!(
        rivet(&["config", "hook.x.command", "true"])
            .status
            .success()
    );
    assert!(
        rivet(&["config", "hook.x.event", "pre-commit"])
            .status
            .success()
    );
    let out = rivet(&["hook", "list", "pre-commit"]);
    assert_eq!(out.status.code(), Some(128));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("hooks defined in the configuration"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
