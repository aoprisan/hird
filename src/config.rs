//! Configuration: `~/.config/hird/config.toml` plus environment overrides.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::identity;

/// Default lease TTL when nothing else says otherwise.
pub const DEFAULT_LEASE_TTL_MINUTES: u64 = 15;

/// On-disk configuration. Every field is optional in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// How long a claim survives without a `task_update`.
    pub lease_ttl_minutes: u64,
    /// Whether list and search default to every project instead of the
    /// current one. Explicit `all_projects` arguments still win.
    pub all_projects_by_default: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lease_ttl_minutes: DEFAULT_LEASE_TTL_MINUTES,
            all_projects_by_default: false,
        }
    }
}

impl Config {
    /// Load from `path`, returning defaults when the file does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(toml::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("{}: {e}", path.display())
            })?),
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

    #[test]
    fn an_explicit_db_flag_wins() {
        let chosen = resolve_db_path(Some(Path::new("/tmp/explicit.db")));
        assert_eq!(chosen, PathBuf::from("/tmp/explicit.db"));
    }
}
