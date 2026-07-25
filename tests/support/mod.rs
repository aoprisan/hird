//! Shared helpers for the integration tests, which drive the real binary.
//!
//! Each test binary includes this module, so not every helper is used by
//! every one of them.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// A scratch home + project + database for one test.
pub struct Sandbox {
    pub dir: TempDir,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        Sandbox { dir }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.path().join("hird.db")
    }

    pub fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// A command with the environment pinned so tests never touch the real
    /// user's config or data directories.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(bin());
        cmd.current_dir(self.project())
            .env_remove("HIRD_HARNESS")
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("HIRD_DB", self.db())
            .env("HIRD_PROJECT", self.project());
        cmd
    }

    /// Run a `hird` subcommand and require success.
    pub fn run(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().expect("spawn hird");
        assert!(
            output.status.success(),
            "hird {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 stdout")
    }

    /// Run a `hird` subcommand and require failure, returning stderr.
    pub fn run_failing(&self, args: &[&str]) -> String {
        let output: Output = self.command().args(args).output().expect("spawn hird");
        assert!(
            !output.status.success(),
            "hird {args:?} unexpectedly succeeded"
        );
        String::from_utf8(output.stderr).expect("utf-8 stderr")
    }

    /// Write a config file into the sandbox's XDG config home.
    pub fn write_config(&self, contents: &str) {
        let dir = self.dir.path().join("config").join("hird");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }
}

/// Path to the binary under test, as produced by cargo for this test run.
pub fn bin() -> PathBuf {
    // `current_exe` is target/<profile>/deps/<test>-<hash>; the binary sits two
    // levels up.
    let mut path = std::env::current_exe().expect("current exe");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("hird{}", std::env::consts::EXE_SUFFIX))
}

/// Assert a path exists, with a message naming it.
pub fn assert_exists(path: &Path) {
    assert!(path.exists(), "{} does not exist", path.display());
}
