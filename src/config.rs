//! Configuration: `~/.config/hird/config.toml` plus environment overrides.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::identity;
use crate::model::Clearance;
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

/// What a finished dependency under an unfinished review does to its
/// dependents.
///
/// v1.7 made `done` revocable — a review can send finished work back — which
/// left readiness resting on a word that is no longer final. Whether that
/// possibility should stall a pipeline is a judgement about the project's
/// tolerance for rework, so it is a key rather than a rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnderReview {
    /// `done` clears the dependency at once; the claimant is told the ground
    /// it builds on is provisional, and hears about it if the verdict then
    /// takes that ground away.
    #[default]
    Clears,
    /// Dependents stay unclaimable until the review delivers its verdict.
    /// Slower, and immune to building on work that gets sent back.
    Holds,
}

impl UnderReview {
    fn policy(self) -> Clearance {
        match self {
            UnderReview::Clears => Clearance::Done,
            UnderReview::Holds => Clearance::Reviewed,
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
    /// Whether a `done` dependency whose review has not delivered a verdict
    /// clears its dependents or holds them.
    pub under_review: UnderReview,
    /// Whether `task_next` passes over tasks whose declared file scope
    /// overlaps what another agent is already working.
    pub dispatch_avoids_conflicts: bool,
    /// How many recalled assertions ride along with a claimed task. Zero
    /// turns recall off.
    pub recall_limit: usize,
    /// Whether the queue watches the working tree to see what claimed tasks
    /// actually change. Needs git; goes quiet by itself where there is none.
    pub witness: bool,
    /// Whether an assertion records the files it was learned against, so a
    /// later reader can be told the code has moved under it. Rides on the same
    /// working-tree access as `witness` and is off wherever that is.
    pub memory_footing: bool,
    /// Whether the witness keeps the content of the versions it fingerprints,
    /// so `hird diff` can show what a task changed, reviews carry the diff of
    /// the work under judgement, and `hird salvage` can recover a version an
    /// overlapping write discarded. Rides on `witness` and is off wherever
    /// that is.
    pub exhibit: bool,
    /// A command to run whenever a task becomes claimable — filed unblocked,
    /// released by a finished dependency, reopened by a verdict or a human,
    /// or dropped by an expired lease. Runs detached through `sh -c` with
    /// `HIRD_EVENT`, `HIRD_TASK`, `HIRD_TITLE`, `HIRD_PROJECT` and `HIRD_DB`
    /// in its environment; empty means no hook. See [`crate::herald`].
    pub dispatch_hook: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lease_ttl_minutes: DEFAULT_LEASE_TTL_MINUTES,
            all_projects_by_default: false,
            path_conflicts: PathConflicts::Report,
            under_review: UnderReview::Clears,
            dispatch_avoids_conflicts: true,
            recall_limit: DEFAULT_RECALL_LIMIT,
            witness: true,
            memory_footing: true,
            exhibit: true,
            dispatch_hook: String::new(),
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

    /// What it takes for a dependency to clear its dependents.
    pub fn clearance(&self) -> Clearance {
        self.under_review.policy()
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
            // Whether observations also keep the content they hash rides
            // along on the witness itself, so every sweep — MCP, CLI, TUI —
            // obeys the same answer without each caller asking.
            .map(|w| w.keeping(self.exhibit))
    }

    /// The herald that announces claimable work, if a dispatch hook is
    /// configured. `db` is the board the hook's own `hird` invocations should
    /// read, handed over as `HIRD_DB`.
    pub fn herald(&self, db: &Path) -> Option<crate::herald::Herald> {
        crate::herald::Herald::new(&self.dispatch_hook, db)
    }

    /// The witness memory may read the tree through, if it may.
    ///
    /// Narrower than [`Config::witness`] by one flag: a human who wants the
    /// queue to watch what tasks change but does not want assertions carrying
    /// file fingerprints can have exactly that, and gets memory as it behaved
    /// before footing existed rather than a half-populated version of it.
    pub fn footing<'w>(
        &self,
        witness: Option<&'w crate::witness::Witness>,
    ) -> Option<&'w crate::witness::Witness> {
        if self.memory_footing {
            witness
        } else {
            None
        }
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
        // Likewise the footing under memory: an assertion that cannot say what
        // it was learned against can never be checked, and the whole cost is a
        // hash of files somebody already named.
        assert!(cfg.memory_footing);
        // No hook runs unless a human wrote one down: the herald is a way to
        // summon agents, and summoning is opt-in.
        assert!(cfg.dispatch_hook.is_empty());
        assert!(cfg.herald(Path::new("/tmp/x.db")).is_none());
    }

    #[test]
    fn a_configured_dispatch_hook_builds_a_herald() {
        let cfg = Config {
            dispatch_hook: "true".to_string(),
            ..Config::default()
        };
        assert!(cfg.herald(Path::new("/tmp/x.db")).is_some());
    }

    #[test]
    fn witnessing_can_be_turned_off_without_touching_git() {
        let off = Config {
            witness: false,
            ..Config::default()
        };
        assert!(off.witness(Path::new(".")).is_none());
    }

    /// Footing rides on the witness but can be given up on its own.
    #[test]
    fn memory_footing_is_a_second_switch_over_the_same_access() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A stand-in for "there is a witness here", without needing git: the
        // question under test is the flag, not the discovery.
        let witness = crate::witness::Witness::discover(root);
        let both_off = Config {
            memory_footing: false,
            ..Config::default()
        };
        assert!(both_off.footing(witness.as_ref()).is_none());
        assert_eq!(
            Config::default().footing(witness.as_ref()).is_some(),
            witness.is_some()
        );
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
