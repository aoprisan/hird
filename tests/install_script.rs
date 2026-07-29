//! Behavior of the release-build/install/clean shell wrapper.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_script(install_exit: i32) -> (std::process::Output, String) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let target = dir.path().join("target");
    let log = dir.path().join("calls");
    std::fs::create_dir_all(target.join("release")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    executable(
        &bin.join("cargo"),
        "#!/bin/sh\nprintf 'cargo:%s\\n' \"$*\" >> \"$HIRD_INSTALL_TEST_LOG\"\n",
    );
    executable(
        &target.join("release/hird"),
        &format!(
            "#!/bin/sh\nprintf 'hird:%s\\n' \"$*\" >> \"$HIRD_INSTALL_TEST_LOG\"\nexit {install_exit}\n"
        ),
    );

    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let output = Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
        .arg("--install-skill")
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("CARGO_TARGET_DIR", &target)
        .env("HIRD_INSTALL_TEST_LOG", &log)
        .output()
        .unwrap();
    let calls = std::fs::read_to_string(log).unwrap();
    (output, calls)
}

#[test]
fn install_script_builds_installs_then_cleans() {
    let (output, calls) = run_script(0);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        calls,
        "cargo:build --release --locked\n\
         hird:--install --install-skill\n\
         cargo:clean --release --locked\n"
    );
}

#[test]
fn install_script_cleans_when_installation_fails() {
    let (output, calls) = run_script(7);
    assert_eq!(output.status.code(), Some(7));
    assert!(
        calls.ends_with("cargo:clean --release --locked\n"),
        "{calls}"
    );
}
