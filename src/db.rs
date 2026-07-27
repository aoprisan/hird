//! Database handle: connection setup, pragmas and schema migrations.
//!
//! One SQLite file backs every `hird` process on the machine. Concurrency is
//! handled by WAL plus a busy timeout; every write is a short transaction and
//! there is no daemon holding the file open.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::Result;
use crate::repo::{Deps, Footings, Memory, Plans, Recall, Scopes, Tasks, Witnessed};

/// Numbered migrations, applied in order and recorded in `meta.schema_version`.
const MIGRATIONS: &[&str] = &[
    // 1 — initial schema
    r#"
CREATE TABLE tasks (
  id          TEXT PRIMARY KEY,
  seq         INTEGER UNIQUE NOT NULL,
  project     TEXT NOT NULL,
  title       TEXT NOT NULL,
  body        TEXT NOT NULL DEFAULT '',
  status      TEXT NOT NULL DEFAULT 'open'
              CHECK (status IN ('open','claimed','in_progress','done','failed','cancelled')),
  priority    INTEGER NOT NULL DEFAULT 0,
  claimed_by  TEXT,
  lease_expires_at TEXT,
  result      TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

CREATE TABLE task_events (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(id),
  at         TEXT NOT NULL,
  actor      TEXT NOT NULL,
  kind       TEXT NOT NULL,
  detail     TEXT NOT NULL DEFAULT ''
);

CREATE TABLE assertions (
  id         TEXT PRIMARY KEY,
  project    TEXT NOT NULL,
  content    TEXT NOT NULL,
  tags       TEXT NOT NULL DEFAULT '',
  actor      TEXT NOT NULL,
  task_id    TEXT REFERENCES tasks(id),
  superseded_by TEXT REFERENCES assertions(id),
  created_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE assertions_fts USING fts5(
  content, tags, content='assertions', content_rowid='rowid'
);

CREATE TRIGGER assertions_fts_ai AFTER INSERT ON assertions BEGIN
  INSERT INTO assertions_fts(rowid, content, tags)
  VALUES (new.rowid, new.content, new.tags);
END;

CREATE TRIGGER assertions_fts_ad AFTER DELETE ON assertions BEGIN
  INSERT INTO assertions_fts(assertions_fts, rowid, content, tags)
  VALUES ('delete', old.rowid, old.content, old.tags);
END;

CREATE TRIGGER assertions_fts_au AFTER UPDATE ON assertions BEGIN
  INSERT INTO assertions_fts(assertions_fts, rowid, content, tags)
  VALUES ('delete', old.rowid, old.content, old.tags);
  INSERT INTO assertions_fts(rowid, content, tags)
  VALUES (new.rowid, new.content, new.tags);
END;

CREATE INDEX idx_tasks_project_status ON tasks(project, status);
CREATE INDEX idx_tasks_status_lease ON tasks(status, lease_expires_at);
CREATE INDEX idx_task_events_task_at ON task_events(task_id, at);
CREATE INDEX idx_assertions_project_current ON assertions(project, superseded_by);

INSERT INTO meta(key, value) VALUES ('next_seq', '1');
"#,
    // 2 — dependencies between tasks, and the file scopes tasks declare
    r#"
CREATE TABLE task_deps (
  task_id       TEXT NOT NULL REFERENCES tasks(id),
  depends_on_id TEXT NOT NULL REFERENCES tasks(id),
  actor         TEXT NOT NULL,
  created_at    TEXT NOT NULL,
  PRIMARY KEY (task_id, depends_on_id),
  CHECK (task_id <> depends_on_id)
);

CREATE INDEX idx_task_deps_depends_on ON task_deps(depends_on_id);

CREATE TABLE task_paths (
  id          TEXT PRIMARY KEY,
  task_id     TEXT NOT NULL REFERENCES tasks(id),
  pattern     TEXT NOT NULL,
  declared_by TEXT NOT NULL,
  at          TEXT NOT NULL,
  UNIQUE (task_id, pattern)
);

CREATE INDEX idx_task_paths_task ON task_paths(task_id);
"#,
    // 3 — the witness: what the working tree says actually happened
    r#"
CREATE TABLE task_witness (
  task_id    TEXT PRIMARY KEY REFERENCES tasks(id),
  head       TEXT NOT NULL DEFAULT '',
  tree       TEXT NOT NULL DEFAULT '{}',
  at         TEXT NOT NULL
);

CREATE TABLE task_changes (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(id),
  path       TEXT NOT NULL,
  kind       TEXT NOT NULL CHECK (kind IN ('added','modified','deleted')),
  hash       TEXT NOT NULL DEFAULT '',
  first_seen TEXT NOT NULL,
  last_seen  TEXT NOT NULL,
  UNIQUE (task_id, path)
);

CREATE INDEX idx_task_changes_path ON task_changes(path);
CREATE INDEX idx_task_changes_task ON task_changes(task_id);
"#,
    // 4 — the name a plan file gave a task, so a plan can be applied twice
    r#"
CREATE TABLE task_plan_nodes (
  task_id    TEXT PRIMARY KEY REFERENCES tasks(id),
  project    TEXT NOT NULL,
  plan       TEXT NOT NULL,
  node       TEXT NOT NULL,
  at         TEXT NOT NULL,
  UNIQUE (project, plan, node)
);
"#,
    // 5 — the footing under an assertion: the files it was read off, the
    //     versions they were in, and everyone who has said it since
    r#"
CREATE TABLE assertion_footing (
  assertion_id TEXT NOT NULL REFERENCES assertions(id),
  path         TEXT NOT NULL,
  hash         TEXT NOT NULL DEFAULT '',
  at           TEXT NOT NULL,
  PRIMARY KEY (assertion_id, path)
);

CREATE INDEX idx_assertion_footing_path ON assertion_footing(path);

CREATE TABLE assertion_affirmations (
  assertion_id TEXT NOT NULL REFERENCES assertions(id),
  actor        TEXT NOT NULL,
  at           TEXT NOT NULL,
  PRIMARY KEY (assertion_id, actor)
);
"#,
];

/// An open connection to the hird database.
///
/// All SQL lives behind this handle's repositories; the MCP, CLI and TUI
/// layers only ever touch [`Db::tasks`] and [`Db::memory`].
pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl Db {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    crate::error::Error::invalid(format!(
                        "cannot create database directory {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
        let conn = Connection::open(&path)?;
        Self::from_connection(conn, path)
    }

    /// Open a private in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, PathBuf::from(":memory:"))
    }

    fn from_connection(conn: Connection, path: PathBuf) -> Result<Db> {
        // `journal_mode` returns a row, so it needs `query_row` rather than
        // `execute_batch` on some builds; pragma_update_and_check handles both.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn, path };
        db.migrate()?;
        Ok(db)
    }

    /// The path this handle was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Raw connection access, for tests that need to set up states the
    /// repository layer deliberately refuses to create.
    #[cfg(test)]
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Task and task-event repository.
    pub fn tasks(&self) -> Tasks<'_> {
        Tasks::new(&self.conn)
    }

    /// Assertion repository.
    pub fn memory(&self) -> Memory<'_> {
        Memory::new(&self.conn)
    }

    /// Task dependency graph repository.
    pub fn deps(&self) -> Deps<'_> {
        Deps::new(&self.conn)
    }

    /// Declared file scope repository.
    pub fn scopes(&self) -> Scopes<'_> {
        Scopes::new(&self.conn)
    }

    /// Plan filing: a whole graph of tasks in one transaction.
    pub fn plans(&self) -> Plans<'_> {
        Plans::new(&self.conn)
    }

    /// What the working tree was seen to do while tasks were held.
    pub fn witnessed(&self) -> Witnessed<'_> {
        Witnessed::new(&self.conn)
    }

    /// What assertions were learned against, and who else has said them.
    pub fn footings(&self) -> Footings<'_> {
        Footings::new(&self.conn)
    }

    /// The memory relevant to a task, derived from the other three.
    pub fn recall(&self) -> Recall<'_> {
        Recall::new(&self.conn)
    }

    /// The schema version currently applied.
    pub fn schema_version(&self) -> Result<u32> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .ok();
        Ok(raw.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    fn migrate(&self) -> Result<()> {
        // `meta` must exist before we can read the version out of it.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )?;
        let applied = self.schema_version()?;
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let version = idx as u32 + 1;
            if version <= applied {
                continue;
            }
            let tx = self.conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [version.to_string()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_applies_every_migration() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
    }

    #[test]
    fn reopening_an_existing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("hird.db");
        let first = Db::open(&path).unwrap();
        let seq = first.tasks().create("/p", "t", "", 0, "cli").unwrap().seq;
        drop(first);

        let second = Db::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), MIGRATIONS.len() as u32);
        assert_eq!(second.tasks().get(seq).unwrap().title, "t");
    }

    #[test]
    fn fts5_is_available_in_the_bundled_sqlite() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute_batch("CREATE VIRTUAL TABLE probe USING fts5(x)")
            .expect("bundled SQLite must be built with FTS5");
    }

    #[test]
    fn wal_and_foreign_keys_are_on_for_file_databases() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("hird.db")).unwrap();
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let fk: i64 = db
            .conn()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }
}
