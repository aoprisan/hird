//! Who is acting, and on which project.
//!
//! Both answers come from the environment the harness starts `hird` in, so a
//! Claude Code session and a Codex session pointed at the same checkout land on
//! the same project scope while staying distinguishable as actors.

use std::path::{Path, PathBuf};

use ulid::Ulid;

/// Environment variable naming the harness, set in each harness's MCP config.
pub const HARNESS_ENV: &str = "HIRD_HARNESS";
/// Environment variable overriding project detection.
pub const PROJECT_ENV: &str = "HIRD_PROJECT";
/// Environment variable overriding the database path.
pub const DB_ENV: &str = "HIRD_DB";

/// Actor string recorded for CLI actions.
pub const ACTOR_CLI: &str = "cli";
/// Actor string recorded for TUI actions.
pub const ACTOR_TUI: &str = "tui";

/// A `<harness>:<session>` identity for one MCP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentId {
    harness: String,
    session: String,
}

impl AgentId {
    /// Read the harness from the environment and mint a fresh session suffix.
    pub fn from_env() -> AgentId {
        AgentId::new(
            std::env::var(HARNESS_ENV).unwrap_or_default(),
            short_session_id(),
        )
    }

    pub fn new(harness: impl Into<String>, session: impl Into<String>) -> AgentId {
        let harness = sanitize(&harness.into());
        let harness = if harness.is_empty() {
            "unknown".to_string()
        } else {
            harness
        };
        AgentId {
            harness,
            session: sanitize(&session.into()),
        }
    }

    /// The harness name alone, used for colour-coding badges in the TUI.
    pub fn harness(&self) -> &str {
        &self.harness
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// The full `harness:session` string stored on claims and assertions.
    pub fn as_actor(&self) -> String {
        format!("{}:{}", self.harness, self.session)
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.harness, self.session)
    }
}

/// The harness part of an actor string, for badge rendering.
///
/// Non-agent actors (`cli`, `tui`, `hird`) have no colon and are returned whole.
pub fn actor_harness(actor: &str) -> &str {
    actor.split_once(':').map_or(actor, |(h, _)| h)
}

/// Four lowercase characters of ULID randomness — enough to tell a handful of
/// concurrent sessions apart without making log lines unreadable.
fn short_session_id() -> String {
    let ulid = Ulid::generate().to_string().to_lowercase();
    ulid[ulid.len() - 4..].to_string()
}

/// Strip the colon (our separator) and whitespace from identity components.
fn sanitize(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect()
}

/// Resolve the project root a process is working in.
///
/// `HIRD_PROJECT` wins; otherwise the nearest enclosing directory containing a
/// `.git` entry; otherwise the working directory itself. The result is
/// canonicalized so two harnesses reaching the checkout by different symlinks
/// agree on the scope.
pub fn resolve_project(cwd: &Path) -> String {
    if let Ok(explicit) = std::env::var(PROJECT_ENV) {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return canonical(Path::new(explicit));
        }
    }
    match git_toplevel(cwd) {
        Some(root) => canonical(&root),
        None => canonical(cwd),
    }
}

/// Walk up from `start` looking for a `.git` directory or worktree file.
///
/// Done by directory walk rather than by shelling out to `git`, because MCP
/// servers are spawned per session and must start fast.
fn git_toplevel(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

fn canonical(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved.to_string_lossy().into_owned()
}

/// The default database path: `${XDG_DATA_HOME:-~/.local/share}/hird/hird.db`.
pub fn default_db_path() -> PathBuf {
    data_dir().join("hird").join("hird.db")
}

/// The default config path: `${XDG_CONFIG_HOME:-~/.config}/hird/config.toml`.
pub fn default_config_path() -> PathBuf {
    config_dir().join("hird").join("config.toml")
}

fn data_dir() -> PathBuf {
    non_empty_env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("share"))
}

fn config_dir() -> PathBuf {
    non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

fn home() -> PathBuf {
    non_empty_env("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_ids_render_as_harness_colon_session() {
        let id = AgentId::new("claude-code", "af31");
        assert_eq!(id.as_actor(), "claude-code:af31");
        assert_eq!(id.to_string(), "claude-code:af31");
        assert_eq!(id.harness(), "claude-code");
        assert_eq!(id.session(), "af31");
    }

    #[test]
    fn a_missing_harness_name_becomes_unknown() {
        assert_eq!(AgentId::new("", "af31").harness(), "unknown");
        assert_eq!(AgentId::new("   ", "af31").harness(), "unknown");
    }

    #[test]
    fn colons_and_whitespace_cannot_forge_a_second_field() {
        let id = AgentId::new("cla ude:code", "af:31");
        assert_eq!(id.as_actor(), "claudecode:af31");
    }

    #[test]
    fn session_ids_are_four_characters_and_differ() {
        let a = short_session_id();
        let b = short_session_id();
        assert_eq!(a.len(), 4);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        // ULID randomness makes a collision here a 1-in-a-million event, but
        // the point of the assertion is that ids are not constant.
        assert!(a != b || short_session_id() != a);
    }

    #[test]
    fn actor_harness_splits_agents_and_passes_humans_through() {
        assert_eq!(actor_harness("claude-code:af31"), "claude-code");
        assert_eq!(actor_harness("cli"), "cli");
        assert_eq!(actor_harness("tui"), "tui");
    }

    #[test]
    fn project_detection_finds_the_enclosing_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        let nested = root.join("crates").join("inner");
        std::fs::create_dir_all(&nested).unwrap();

        let found = git_toplevel(&nested).unwrap();
        assert_eq!(
            std::fs::canonicalize(found).unwrap(),
            std::fs::canonicalize(root).unwrap()
        );
    }

    #[test]
    fn project_detection_falls_back_to_the_directory_itself() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git_toplevel(dir.path()).is_none());
    }

    #[test]
    fn worktree_git_files_count_as_a_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere").unwrap();
        assert!(git_toplevel(dir.path()).is_some());
    }

    #[test]
    fn default_paths_sit_under_the_hird_directory() {
        assert!(default_db_path().ends_with("hird/hird.db"));
        assert!(default_config_path().ends_with("hird/config.toml"));
    }
}
