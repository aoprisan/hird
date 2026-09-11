//! Who is acting, and on which project.
//!
//! Both answers come from the environment the harness starts `hird` in, so a
//! Claude Code session and a Codex session pointed at the same checkout land on
//! the same project scope while staying distinguishable as actors. The one
//! thing the environment does not always say is the harness's name, and there
//! the client's own name for itself stands in — see [`AgentId`].
//!
//! An actor answers *what* is acting. `HIRD_IDENTITY` adds *who*: an optional
//! principal, prefixed as `<principal>/<harness>:<session>`, for a queue more
//! than one person files into. It is recorded and reported; it steers nothing.
//! Recusal keeps barring the harness, because §15's bar is about models rather
//! than people — two people on one harness are still one model reading its own
//! work. Absent the variable the actor string is exactly what it was.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ulid::Ulid;

/// Environment variable naming the harness, set in each harness's MCP config.
pub const HARNESS_ENV: &str = "HIRD_HARNESS";
/// Environment variable naming the person a session acts for.
pub const IDENTITY_ENV: &str = "HIRD_IDENTITY";
/// Environment variable overriding project detection.
pub const PROJECT_ENV: &str = "HIRD_PROJECT";
/// Environment variable overriding the database path.
pub const DB_ENV: &str = "HIRD_DB";

/// Actor string recorded for CLI actions.
pub const ACTOR_CLI: &str = "cli";
/// Actor string recorded for TUI actions.
pub const ACTOR_TUI: &str = "tui";
/// Actor string recorded for what `hird web` does on the human's behalf —
/// sweeping leases and looking at the tree, never anything a task would call
/// a decision.
pub const ACTOR_WEB: &str = "web";

/// Longest harness name recorded in an actor string. A name is a badge in a
/// TUI column, not a payload, and it arrives over the wire in the MCP case.
const HARNESS_MAX: usize = 32;

/// Longest principal recorded in an actor string, on the same reasoning as
/// [`HARNESS_MAX`]: a name in a column, not a payload.
const PRINCIPAL_MAX: usize = 32;

/// Separates the principal from the harness in an actor string. Stripped by
/// [`sanitize`] from every component, so no name can forge a second one.
const PRINCIPAL_SEP: char = '/';

/// A `[<principal>/]<harness>:<session>` identity for one MCP session.
///
/// The session half is minted once, when the process starts. The harness half
/// is whatever `HIRD_HARNESS` says; when the environment does not say, it is
/// taken from the first client that names itself — MCP 2026-07-28 carries the
/// client's implementation on every request, so a harness configured by hand
/// still arrives with a name instead of being filed as `unknown`.
///
/// Taken once and then latched, not re-read per call. An actor string that
/// changed mid-session would leave this process unable to find its own leases.
///
/// The principal — *who* the session acts for — comes from `HIRD_IDENTITY` and
/// nowhere else. Deliberately not from the client: a harness may name itself,
/// because being wrong about that costs a badge, but a client that could name
/// the person would be a client that could sign another person's work. The
/// human sets it where they set the rest of the registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentId {
    /// Unset until the environment or a client supplies a usable name.
    harness: OnceLock<String>,
    /// Who this session acts for. `None` until somebody says, and no client may.
    principal: Option<String>,
    session: String,
}

impl AgentId {
    /// Mint an identity for a named harness without consulting the
    /// environment, for a process whose environment is not the caller's.
    ///
    /// An empty name leaves the harness unset, so a client that names itself
    /// still supplies one — the fallback §1.6 added, and the one a server has
    /// to rely on because it cannot see the far end's configuration.
    pub fn fresh(harness: impl Into<String>) -> AgentId {
        AgentId::new(harness, short_session_id())
    }

    /// Read the harness and principal from the environment and mint a fresh
    /// session suffix.
    pub fn from_env() -> AgentId {
        AgentId::new(
            std::env::var(HARNESS_ENV).unwrap_or_default(),
            short_session_id(),
        )
        .acting_for(std::env::var(IDENTITY_ENV).unwrap_or_default())
    }

    pub fn new(harness: impl Into<String>, session: impl Into<String>) -> AgentId {
        let id = AgentId {
            harness: OnceLock::new(),
            principal: None,
            session: sanitize(&session.into()),
        };
        id.name_harness(&harness.into());
        id
    }

    /// Name the person this session acts for. An empty or unusable name leaves
    /// the identity as it was, which is the unattributed actor of every queue
    /// with one human on it.
    pub fn acting_for(mut self, principal: impl Into<String>) -> AgentId {
        self.principal = clamp(&principal.into(), PRINCIPAL_MAX);
        self
    }

    /// Who this session acts for, if anybody said.
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
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
        let Some(name) = clamp(raw, HARNESS_MAX) else {
            return false;
        };
        self.harness.set(name).is_ok()
    }

    /// The harness name alone, used for colour-coding badges in the TUI.
    pub fn harness(&self) -> &str {
        self.harness.get().map_or("unknown", String::as_str)
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// The full `[principal/]harness:session` string stored on claims and
    /// assertions.
    pub fn as_actor(&self) -> String {
        match self.principal() {
            Some(who) => format!("{who}{PRINCIPAL_SEP}{}:{}", self.harness(), self.session),
            None => format!("{}:{}", self.harness(), self.session),
        }
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_actor())
    }
}

/// The harness part of an actor string, for badge rendering.
///
/// Non-agent actors (`cli`, `tui`, `hird`) have no colon and are returned
/// whole. A principal prefix is dropped: the badge answers *what* is acting,
/// and every reader of it — the TUI's colours, recusal's bar, the record's
/// rows — is asking about the model rather than the person.
pub fn actor_harness(actor: &str) -> &str {
    let what = actor.split_once(':').map_or(actor, |(what, _)| what);
    what.rsplit_once(PRINCIPAL_SEP)
        .map_or(what, |(_, harness)| harness)
}

/// The person an actor string was recorded for, if it names one.
///
/// `None` for the unattributed actors of a single-human queue, and for the
/// `cli`, `tui` and `web` actors, which are the human at the keyboard already.
pub fn actor_principal(actor: &str) -> Option<&str> {
    let what = actor.split_once(':').map_or(actor, |(what, _)| what);
    what.split_once(PRINCIPAL_SEP)
        .map(|(who, _)| who)
        .filter(|who| !who.is_empty())
}

/// Normalize a principal the way an identity would, for callers writing one
/// down before any session exists — `hird register`, chiefly.
///
/// `None` when nothing usable survives, so an empty `--identity` writes no
/// variable rather than an empty one.
pub fn principal_name(raw: &str) -> Option<String> {
    clamp(raw, PRINCIPAL_MAX)
}

/// Which half of an actor a reading groups by.
///
/// The record measures "whose work survives a reading by a different model"
/// (§16). With one human on a queue those two words mean the same thing and
/// [`Axis::Harness`] answers both. With more than one they come apart, and
/// only the reader knows which was meant — so this is a choice at the point of
/// reading, never a change to what is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Axis {
    /// Group by the model that acted. Every actor has one.
    #[default]
    Harness,
    /// Group by the person it acted for. Unattributed actors have none, and a
    /// reading along this axis leaves them out rather than inventing a name.
    Person,
}

impl Axis {
    /// The key `actor` falls under, or `None` when it carries no such half.
    pub fn key<'a>(&self, actor: &'a str) -> Option<&'a str> {
        match self {
            Axis::Harness => Some(actor_harness(actor)),
            Axis::Person => actor_principal(actor),
        }
    }
}

/// Four lowercase characters of ULID randomness — enough to tell a handful of
/// concurrent sessions apart without making log lines unreadable.
fn short_session_id() -> String {
    let ulid = Ulid::generate().to_string().to_lowercase();
    ulid[ulid.len() - 4..].to_string()
}

/// Strip our separators and whitespace from identity components: the colon
/// splits `harness:session`, the slash splits the principal from the harness,
/// and the comma joins harness names in the herald's `HIRD_RECUSED` list.
///
/// Stripping rather than rejecting keeps a badly set variable a cosmetic
/// problem, and stripping the slash is what stops a harness or session name
/// from claiming to be somebody.
fn sanitize(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(*c, ':' | ',' | PRINCIPAL_SEP))
        .collect()
}

/// The harness name an [`AgentId`] would carry for `raw`, or `None` if nothing
/// usable survives sanitizing.
///
/// Public so that a configuration naming harnesses — the server's roster,
/// which pins one per worker and keys defaults on the same names — matches
/// them the way the identity will record them, rather than by the spelling
/// the operator happened to type.
pub fn harness_name(raw: &str) -> Option<String> {
    clamp(raw, HARNESS_MAX)
}

/// Sanitize `raw` and cut it to `max` characters, or `None` if nothing usable
/// survives. Shared by the harness and the principal, which have the same
/// shape and the same reason for a limit.
fn clamp(raw: &str, max: usize) -> Option<String> {
    let mut name = sanitize(raw);
    if name.is_empty() {
        return None;
    }
    name.truncate(name.char_indices().nth(max).map_or(name.len(), |(i, _)| i));
    Some(name)
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

    /// The comma joins harness names in `HIRD_RECUSED`, so a name carrying
    /// one could smuggle a second entry into the herald's list.
    #[test]
    fn commas_cannot_forge_an_entry_in_the_recused_list() {
        let id = AgentId::new("codex,claude-code", "af31");
        assert_eq!(id.harness(), "codexclaude-code");
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
    fn actor_harness_looks_past_the_principal() {
        // Every reader of a badge asks what model acted, so the answer must not
        // change when a queue starts recording who it acted for.
        assert_eq!(actor_harness("ana/claude-code:af31"), "claude-code");
        assert_eq!(actor_harness("ana/claude-code"), "claude-code");
    }

    #[test]
    fn actor_principal_names_who_acted_or_nobody() {
        assert_eq!(actor_principal("ana/claude-code:af31"), Some("ana"));
        assert_eq!(actor_principal("claude-code:af31"), None);
        assert_eq!(actor_principal("cli"), None);
    }

    #[test]
    fn an_identity_without_a_principal_is_the_actor_it_always_was() {
        let id = AgentId::new("claude-code", "af31");
        assert_eq!(id.as_actor(), "claude-code:af31");
        assert_eq!(id.principal(), None);
        assert_eq!(id.to_string(), id.as_actor());
    }

    #[test]
    fn an_identity_with_a_principal_carries_it_in_the_actor() {
        let id = AgentId::new("claude-code", "af31").acting_for("ana");
        assert_eq!(id.as_actor(), "ana/claude-code:af31");
        assert_eq!(id.principal(), Some("ana"));
        assert_eq!(actor_harness(&id.as_actor()), "claude-code");
        assert_eq!(actor_principal(&id.as_actor()), Some("ana"));
    }

    #[test]
    fn an_empty_principal_leaves_the_identity_unattributed() {
        let id = AgentId::new("claude-code", "af31").acting_for("   ");
        assert_eq!(id.principal(), None);
        assert_eq!(id.as_actor(), "claude-code:af31");
    }

    #[test]
    fn no_component_can_forge_a_second_separator() {
        // A harness that could write a slash could claim to be someone; a
        // principal that could write a colon could claim to be a session.
        let id = AgentId::new("ben/claude-code", "af:31").acting_for("ana/root:x");
        assert_eq!(id.as_actor(), "anarootx/benclaude-code:af31");
        assert_eq!(actor_principal(&id.as_actor()), Some("anarootx"));
        assert_eq!(actor_harness(&id.as_actor()), "benclaude-code");
    }

    #[test]
    fn a_long_principal_is_cut_to_a_column_width() {
        let id = AgentId::new("claude-code", "af31").acting_for("a".repeat(80));
        assert_eq!(id.principal().unwrap().chars().count(), PRINCIPAL_MAX);
    }

    #[test]
    fn a_client_may_name_the_harness_but_never_the_person() {
        let id = AgentId::new("", "af31");
        assert!(id.name_from_client("codex"));
        assert_eq!(id.harness(), "codex");
        // There is no path from a client's words to the principal: naming one
        // takes an owned identity, which only the process's own environment
        // gets to build.
        assert_eq!(id.principal(), None);
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
