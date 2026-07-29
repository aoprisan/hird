//! Self-installation helpers for the hird binary and agent skill.
//!
//! The binary is copied rather than symlinked, following dyad's installer: the
//! installed command remains usable if the checkout or target directory moves.
//! Re-running `--install` refreshes that standalone snapshot.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;

const SKILL: &str = include_str!("../skills/hird/SKILL.md");

/// Copy this exact binary into `~/.local/bin`.
pub fn install_binary(out: &mut impl Write) -> anyhow::Result<()> {
    let source = std::env::current_exe()
        .context("locating the current hird binary")?
        .canonicalize()
        .context("canonicalizing the current binary path")?;
    let home = home_dir()?;
    install_binary_from(&source, &home, std::env::var_os("PATH").as_deref(), out)
}

/// Install the bundled skill globally for the current user.
pub fn install_skill(out: &mut impl Write) -> anyhow::Result<()> {
    let home = home_dir()?;
    for (platform, path) in skill_paths(&home) {
        install_skill_at(platform, &path, out)?;
    }
    Ok(())
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn install_binary_from(
    source: &Path,
    home: &Path,
    path_var: Option<&std::ffi::OsStr>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let target_dir = home.join(".local/bin");
    let target = target_dir.join("hird");
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating {}", target_dir.display()))?;

    if let Ok(metadata) = std::fs::symlink_metadata(&target) {
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            anyhow::bail!(
                "refusing to overwrite {}: it is a directory",
                target.display()
            );
        }
        if !file_type.is_symlink() && std::fs::canonicalize(&target).ok() == Some(source.into()) {
            writeln!(out, "hird is already installed at {}", target.display())?;
            return Ok(());
        }
        std::fs::remove_file(&target)
            .with_context(|| format!("removing existing file at {}", target.display()))?;
    }

    std::fs::copy(source, &target)
        .with_context(|| format!("copying binary to {}", target.display()))?;
    writeln!(out, "Installed hird to {}", target.display())?;
    writeln!(out, "  copied from {}", source.display())?;

    if !path_contains(&target_dir, path_var) {
        writeln!(out)?;
        writeln!(out, "Note: {} is not on your PATH.", target_dir.display())?;
        writeln!(out, "Add this to your shell profile:")?;
        writeln!(out, "  export PATH=\"$HOME/.local/bin:$PATH\"")?;
    }
    Ok(())
}

fn path_contains(dir: &Path, path_var: Option<&std::ffi::OsStr>) -> bool {
    path_var
        .map(std::env::split_paths)
        .is_some_and(|mut paths| paths.any(|path| path == dir))
}

fn skill_paths(home: &Path) -> [(&'static str, PathBuf); 3] {
    [
        (
            "Codex and OpenCode",
            home.join(".agents/skills/hird/SKILL.md"),
        ),
        ("Claude Code", home.join(".claude/skills/hird/SKILL.md")),
        ("GitHub Copilot", home.join(".copilot/skills/hird/SKILL.md")),
    ]
}

fn install_skill_at(platform: &str, path: &Path, out: &mut impl Write) -> anyhow::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(SKILL) {
        writeln!(
            out,
            "hird skill for {platform} is already installed at {}",
            path.display()
        )?;
        return Ok(());
    }

    let parent = path
        .parent()
        .context("the hird skill destination has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    std::fs::write(path, SKILL).with_context(|| format!("writing {}", path.display()))?;
    writeln!(
        out,
        "installed hird skill for {platform} at {}",
        path.display()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_install_copies_and_refreshes_a_standalone_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("target/release/hird");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "first build").unwrap();
        let home = dir.path().join("home");
        let target = home.join(".local/bin/hird");
        let mut out = Vec::new();

        install_binary_from(&source, &home, None, &mut out).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first build");
        assert!(String::from_utf8_lossy(&out).contains("not on your PATH"));

        std::fs::write(&source, "second build").unwrap();
        out.clear();
        let path = std::env::join_paths([home.join(".local/bin")]).unwrap();
        install_binary_from(&source, &home, Some(&path), &mut out).unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "second build");
        assert!(!String::from_utf8_lossy(&out).contains("not on your PATH"));
    }

    #[test]
    fn binary_install_refuses_to_overwrite_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, "binary").unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".local/bin/hird")).unwrap();

        let err = install_binary_from(&source, &home, None, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("it is a directory"), "{err:#}");
    }

    #[test]
    fn binary_install_is_a_no_op_when_run_from_the_installed_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join(".local/bin/hird");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "binary").unwrap();
        let source = source.canonicalize().unwrap();
        let mut out = Vec::new();

        install_binary_from(&source, dir.path(), None, &mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("already installed"));
    }

    #[test]
    fn skill_install_is_idempotent_and_refreshes_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills/hird/SKILL.md");
        let mut out = Vec::new();

        install_skill_at("test agent", &path, &mut out).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);
        assert!(String::from_utf8_lossy(&out).contains("installed hird skill"));

        out.clear();
        install_skill_at("test agent", &path, &mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("already installed"));

        std::fs::write(&path, "an older bundled skill").unwrap();
        install_skill_at("test agent", &path, &mut Vec::new()).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), SKILL);
    }

    #[test]
    fn skill_targets_cover_codex_claude_code_copilot_and_opencode() {
        let home = Path::new("/home/developer");
        assert_eq!(
            skill_paths(home),
            [
                (
                    "Codex and OpenCode",
                    home.join(".agents/skills/hird/SKILL.md"),
                ),
                ("Claude Code", home.join(".claude/skills/hird/SKILL.md"),),
                ("GitHub Copilot", home.join(".copilot/skills/hird/SKILL.md"),),
            ]
        );
    }
}
