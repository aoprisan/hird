//! Configuration: `~/.config/hird/config.toml` plus environment overrides.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::identity;
use crate::repo::OnConflict;

/// Default lease TTL when nothing else says otherwise.
pub const DEFAULT_LEASE_TTL_MINUTES: u64 = 15;

/// How many recalled assertions a claim carries by default.
///
/// Small on purpose: this arrives unasked-for in an agent's context, so it has
/// to be the handful most likely to matter rather than everything that touches
/// the same files.
pub const DEFAULT_RECALL_LIMIT: usize = 5;

/// What the queue does when a task's declared file scope overlaps live work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathConflicts {
    /// Record the declaration and tell both sides about the overlap.
    #[default]
    Report,
    /// Refuse the claim or declaration outright.
    Refuse,
}

impl PathConflicts {
    fn policy(self) -> OnConflict {
        match self {
            PathConflicts::Report => OnConflict::Report,
            PathConflicts::Refuse => OnConflict::Refuse,
        }
    }
}

/// On-disk configuration. Every field is optional in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// How long a claim survives without a `task_update`.
    pub lease_ttl_minutes: u64,
    /// Whether list and search default to every project instead of the
    /// current one. Explicit `all_projects` arguments still win.
    pub all_projects_by_default: bool,
    /// How to treat a declared file scope that overlaps live work.
    pub path_conflicts: PathConflicts,
    /// Whether `task_next` passes over tasks whose declared file scope
    /// overlaps what another agent is already working.
    pub dispatch_avoids_conflicts: bool,
    /// How many recalled assertions ride along with a claimed task. Zero
    /// turns recall off.
    pub recall_limit: usize,
    /// Whether the queue watches the working tree to see what claimed tasks
    /// actually change. Needs git; goes quiet by itself where there is none.
    pub witness: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lease_ttl_minutes: DEFAULT_LEASE_TTL_MINUTES,
            all_projects_by_default: false,
            path_conflicts: PathConflicts::Report,
            dispatch_avoids_conflicts: true,
            recall_limit: DEFAULT_RECALL_LIMIT,
            witness: true,
        }
    }
}

impl Config {
    /// Load from `path`, returning defaults when the file does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                Ok(toml::from_str(&raw).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    /// Load from the default location.
    pub fn load_default() -> anyhow::Result<Config> {
        Config::load(&identity::default_config_path())
    }

    /// The lease TTL as a duration, clamped to at least one minute.
    pub fn lease_ttl(&self) -> Duration {
        Duration::from_secs(self.lease_ttl_minutes.max(1) * 60)
    }

    /// Resolve an optional per-call `all_projects` flag against the default.
    pub fn all_projects(&self, requested: Option<bool>) -> bool {
        requested.unwrap_or(self.all_projects_by_default)
    }

    /// The overlap policy to apply to a claim or a scope declaration.
    pub fn on_conflict(&self) -> OnConflict {
        self.path_conflicts.policy()
    }

    /// Resolve an optional per-call collision-avoidance flag for `task_next`.
    pub fn avoid_conflicts(&self, requested: Option<bool>) -> bool {
        requested.unwrap_or(self.dispatch_avoids_conflicts)
    }

    /// How many recalled assertions to attach to a task, clamped to a size a
    /// model can actually read.
    pub fn recall_limit(&self) -> usize {
        self.recall_limit.min(50)
    }

    /// A witness for `root`, unless the configuration or the environment says
    /// there will not be one.
    ///
    /// The two reasons are deliberately the same answer: a project outside git
    /// and a project whose human turned witnessing off both get a queue that
    /// works exactly as it did before the witness existed.
    pub fn witness(&self, root: &std::path::Path) -> Option<crate::witness::Witness> {
        self.witness
            .then(|| crate::witness::Witness::discover(root))
            .flatten()
    }
}

/// Resolve the database path: `--db` beats `HIRD_DB` beats the XDG default.
pub fn resolve_db_path(flag: Option<&Path>) -> PathBuf {
    if let Some(path) = flag {
        return path.to_path_buf();
    }
    if let Ok(env) = std::env::var(identity::DB_ENV) {
        let env = env.trim();
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    identity::default_db_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_design() {
        let cfg = Config::default();
        assert_eq!(cfg.lease_ttl_minutes, 15);
        assert!(!cfg.all_projects_by_default);
        assert_eq!(cfg.lease_ttl(), Duration::from_secs(900));
        // Overlaps are reported rather than refused: the queue's job is to
        // tell agents about each other, not to decide for them.
        assert_eq!(cfg.on_conflict(), OnConflict::Report);
        // Self-dispatch, on the other hand, has a free choice of task, so it
        // takes the one that cannot collide.
        assert!(cfg.avoid_conflicts(None));
        assert_eq!(cfg.recall_limit(), 5);
        // Watching the tree is on by default: it costs nothing where there is
        // no live task, and the failure it catches is silent and expensive.
        assert!(cfg.witness);
    }

    #[test]
    fn witnessing_can_be_turned_off_without_touching_git() {
        let off = Config {
            witness: false,
            ..Config::default()
        };
        assert!(off.witness(Path::new(".")).is_none());
    }

    #[test]
    fn recall_can_be_turned_off_and_cannot_be_turned_up_absurdly() {
        let off = Config {
            recall_limit: 0,
            ..Config::default()
        };
        assert_eq!(off.recall_limit(), 0);
        let greedy = Config {
            recall_limit: 5_000,
            ..Config::default()
        };
        assert_eq!(greedy.recall_limit(), 50);
    }

    #[test]
    fn the_conflict_policy_is_named_in_the_file_the_way_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "path_conflicts = \"refuse\"\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().on_conflict(),
            OnConflict::Refuse
        );

        std::fs::write(&path, "path_conflicts = \"shout\"\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("path_conflicts"), "{err}");
    }

    #[test]
    fn collision_avoidance_is_overridable_per_call() {
        let cfg = Config {
            dispatch_avoids_conflicts: false,
            ..Config::default()
        };
        assert!(!cfg.avoid_conflicts(None));
        assert!(cfg.avoid_conflicts(Some(true)));
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/hird/config.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_files_keep_the_remaining_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "lease_ttl_minutes = 30\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.lease_ttl(), Duration::from_secs(1800));
        assert!(!cfg.all_projects_by_default);
    }

    #[test]
    fn unknown_keys_are_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "leash_ttl_minutes = 30\n").unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("leash_ttl_minutes"), "{err}");
    }

    #[test]
    fn a_zero_ttl_is_clamped_to_a_minute() {
        let cfg = Config {
            lease_ttl_minutes: 0,
            ..Config::default()
        };
        assert_eq!(cfg.lease_ttl(), Duration::from_secs(60));
    }

    #[test]
    fn the_all_projects_default_is_overridable_per_call() {
        let cfg = Config {
            all_projects_by_default: true,
            ..Config::default()
        };
        assert!(cfg.all_projects(None));
        assert!(!cfg.all_projects(Some(false)));
        assert!(Config::default().all_projects(Some(true)));
    }

    /// The shipped example must stay loadable, and must document the actual
    /// defaults rather than drifting away from them.
    #[test]
    fn the_example_config_parses_and_matches_the_defaults() {
        let example = include_str!("../examples/config.toml");
        let parsed: Config = toml::from_str(example).expect("examples/config.toml must parse");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn an_explicit_db_flag_wins() {
        let chosen = resolve_db_path(Some(Path::new("/tmp/explicit.db")));
        assert_eq!(chosen, PathBuf::from("/tmp/explicit.db"));
    }
}
