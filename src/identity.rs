//! Who is acting, and on which project.
//!
//! Both answers come from the environment the harness starts `hird` in, so a
//! Claude Code session and a Codex session pointed at the same checkout land on
//! the same project scope while staying distinguishable as actors. The one
//! thing the environment does not always say is the harness's name, and there
//! the client's own name for itself stands in — see [`AgentId`].

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// Longest harness name recorded in an actor string. A name is a badge in a
/// TUI column, not a payload, and it arrives over the wire in the MCP case.
const HARNESS_MAX: usize = 32;

/// A `<harness>:<session>` identity for one MCP session.
///
/// The session half is minted once, when the process starts. The harness half
/// is whatever `HIRD_HARNESS` says; when the environment does not say, it is
/// taken from the first client that names itself — MCP 2026-07-28 carries the
/// client's implementation on every request, so a harness configured by hand
/// still arrives with a name instead of being filed as `unknown`.
///
/// Taken once and then latched, not re-read per call. An actor string that
/// changed mid-session would leave this process unable to find its own leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentId {
    /// Unset until the environment or a client supplies a usable name.
    harness: OnceLock<String>,
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
        let id = AgentId {
            harness: OnceLock::new(),
            session: sanitize(&session.into()),
        };
        id.name_harness(&harness.into());
        id
    }

    /// Offer a name a client gave for itself, and say whether it was taken.
    ///
    /// Ignored once the identity has a name, so `HIRD_HARNESS` — set by
    /// `hird register`, and the only half of this the human controls — is never
    /// overridden by what a client calls itself.
    pub fn name_from_client(&self, client_name: &str) -> bool {
        self.name_harness(client_name)
    }

    fn name_harness(&self, raw: &str) -> bool {
        let mut name = sanitize(raw);
        if name.is_empty() {
            return false;
        }
        name.truncate(
            name.char_indices()
                .nth(HARNESS_MAX)
                .map_or(name.len(), |(i, _)| i),
        );
        self.harness.set(name).is_ok()
    }

    /// The harness name alone, used for colour-coding badges in the TUI.
    pub fn harness(&self) -> &str {
        self.harness.get().map_or("unknown", String::as_str)
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// The full `harness:session` string stored on claims and assertions.
    pub fn as_actor(&self) -> String {
        format!("{}:{}", self.harness(), self.session)
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.harness(), self.session)
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

/// Canonicalize a user-supplied project path the same way detection does, so
/// `--project .` and automatic detection agree on the scope string.
pub fn canonical_project(path: &Path) -> String {
    canonical(path)
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

pub(crate) fn config_dir() -> PathBuf {
    non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

/// The home directory, or the current one when the environment will not say.
pub fn home() -> PathBuf {
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
    fn a_nameless_identity_takes_the_name_the_client_gives_for_itself() {
        let id = AgentId::new("", "af31");
        assert!(id.name_from_client("codex-cli"));
        assert_eq!(id.as_actor(), "codex-cli:af31");
    }

    #[test]
    fn the_environment_outranks_whatever_the_client_calls_itself() {
        let id = AgentId::new("claude-code", "af31");
        assert!(!id.name_from_client("something-else"));
        assert_eq!(id.harness(), "claude-code");
    }

    #[test]
    fn the_first_client_name_is_the_one_that_sticks() {
        let id = AgentId::new("", "af31");
        assert!(id.name_from_client("codex-cli"));
        assert!(!id.name_from_client("copilot"));
        assert_eq!(id.harness(), "codex-cli");
    }

    #[test]
    fn a_client_that_names_itself_nothing_leaves_the_identity_open() {
        let id = AgentId::new("", "af31");
        assert!(!id.name_from_client("  "));
        assert_eq!(id.harness(), "unknown");
        assert!(id.name_from_client("cursor"));
        assert_eq!(id.harness(), "cursor");
    }

    #[test]
    fn a_client_cannot_make_its_name_an_essay() {
        let id = AgentId::new("", "af31");
        id.name_from_client(&"x".repeat(500));
        assert_eq!(id.harness().len(), HARNESS_MAX);
    }

    #[test]
    fn a_client_name_cannot_forge_a_second_field_either() {
        let id = AgentId::new("", "af31");
        id.name_from_client("cla ude:code");
        assert_eq!(id.as_actor(), "claudecode:af31");
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
