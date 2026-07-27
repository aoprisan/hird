//! Declared file scopes, and the collision detector built on them.
//!
//! A queue that hands work to several agents at once has one failure mode the
//! status machine cannot see: two agents editing the same file from two
//! harnesses, each unaware of the other, one of them about to lose their work.
//!
//! So a task may declare the paths it is going to touch — globs, not files,
//! because nothing has been written yet. Whenever a declaration is recorded the
//! queue checks it against every other task currently being worked and reports
//! the overlaps. The check is exact: [`crate::glob::intersects`] asks whether
//! any path at all is described by both patterns, so `src/*.rs` and `src/lib*`
//! collide before either agent has created `src/lib.rs`.
//!
//! Declarations are advisory by default — the queue tells both agents and lets
//! them sort it out — or enforced, when the configuration says overlapping work
//! should simply be refused.

use rusqlite::{params, Connection, Row, Transaction, TransactionBehavior};

use super::deps::id_for_seq;
use super::{new_id, ProjectScope};
use crate::error::{Error, Result};
use crate::glob;
use crate::model::{now_ts, Conflict, EventKind, ScopedTask, Status};

/// What to do when a declaration overlaps live work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Record the declaration and report the overlap.
    Report,
    /// Refuse the declaration and leave nothing behind.
    Refuse,
}

/// Repository over `task_paths`.
pub struct Scopes<'a> {
    conn: &'a Connection,
}

impl<'a> Scopes<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Scopes<'a> {
        Scopes { conn }
    }

    /// Add `patterns` to what task `seq` says it will touch.
    ///
    /// Returns the overlaps with tasks other agents are working right now.
    /// Under [`OnConflict::Refuse`] a non-empty overlap is an error and
    /// nothing is written, so the caller cannot half-declare a scope.
    pub fn declare(
        &self,
        seq: i64,
        patterns: &[String],
        actor: &str,
        on_conflict: OnConflict,
    ) -> Result<Vec<Conflict>> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let conflicts = declare_in_tx(&tx, seq, patterns, actor, on_conflict)?;
        tx.commit()?;
        Ok(conflicts)
    }

    /// Forget everything task `seq` declared.
    pub fn clear(&self, seq: i64, actor: &str) -> Result<usize> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let removed = tx.execute("DELETE FROM task_paths WHERE task_id = ?1", [&task_id])?;
        if removed > 0 {
            super::tasks::insert_event(
                &tx,
                &task_id,
                &now_ts(),
                actor,
                EventKind::Scoped,
                "file scope cleared",
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// The patterns task `seq` has declared, in declaration order.
    pub fn for_task(&self, seq: i64) -> Result<Vec<String>> {
        let task_id = id_for_seq(self.conn, seq)?;
        patterns_for_id(self.conn, &task_id)
    }

    /// Overlaps between task `seq`'s declared scope and live work elsewhere.
    ///
    /// Sweep leases before calling: a task whose lease has run out is not live
    /// work, and its declarations should not hold anyone up.
    pub fn conflicts(&self, seq: i64) -> Result<Vec<Conflict>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let patterns = patterns_for_id(self.conn, &task_id)?;
        conflicts_for(self.conn, &task_id, &patterns)
    }

    /// Every task in `scope` that has declared paths, newest activity first.
    ///
    /// This is the conflict radar's data source: the TUI overlays the live
    /// rows against each other and paints the overlaps.
    pub fn declared(&self, scope: &ProjectScope, active_only: bool) -> Result<Vec<ScopedTask>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let active = if active_only {
            "AND t.status IN ('claimed','in_progress')"
        } else {
            ""
        };
        let sql = format!(
            "SELECT t.seq, t.title, t.status, t.claimed_by, p.pattern
             FROM task_paths p JOIN tasks t ON t.id = p.task_id
             WHERE {project_clause} {active}
             ORDER BY t.seq, p.rowid"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((row_to_scoped(row)?, row.get::<_, String>(4)?))
        })?;

        let mut out: Vec<ScopedTask> = Vec::new();
        for row in rows {
            let (task, pattern) = row?;
            match out.last_mut() {
                Some(last) if last.seq == task.seq => last.patterns.push(pattern),
                _ => out.push(ScopedTask {
                    patterns: vec![pattern],
                    ..task
                }),
            }
        }
        Ok(out)
    }
}

// -------------------------------------------------------------------- helpers

/// The body of [`Scopes::declare`], reusable inside a caller's transaction so
/// claiming and declaring can be one atomic step.
pub(crate) fn declare_in_tx(
    tx: &Transaction<'_>,
    seq: i64,
    patterns: &[String],
    actor: &str,
    on_conflict: OnConflict,
) -> Result<Vec<Conflict>> {
    let task_id = id_for_seq(tx, seq)?;
    let normalized = normalize_all(patterns)?;
    if normalized.is_empty() {
        return Ok(Vec::new());
    }

    let conflicts = conflicts_for(tx, &task_id, &normalized)?;
    if on_conflict == OnConflict::Refuse && !conflicts.is_empty() {
        return Err(Error::PathConflict { seq, conflicts });
    }

    let now = now_ts();
    let mut added = Vec::new();
    for pattern in &normalized {
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO task_paths (id, task_id, pattern, declared_by, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![new_id(), task_id, pattern, actor, now],
        )?;
        if inserted > 0 {
            added.push(pattern.clone());
        }
    }
    if !added.is_empty() {
        super::tasks::insert_event(
            tx,
            &task_id,
            &now,
            actor,
            EventKind::Scoped,
            &format!("will touch {}", added.join(", ")),
        )?;
    }
    Ok(conflicts)
}

/// Normalize and validate a batch of patterns, rejecting the whole batch if
/// any one of them cannot name a path inside the project.
pub(crate) fn normalize_all(patterns: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for raw in patterns {
        if raw.trim().is_empty() {
            continue;
        }
        let normalized = glob::normalize(raw).ok_or_else(|| {
            Error::invalid(format!(
                "{raw:?} is not a usable path pattern; give a path relative to the \
                 project root, like \"src/config.rs\" or \"tests/**\""
            ))
        })?;
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    Ok(out)
}

/// Overlaps between `patterns` and the declarations of every *other* task that
/// is currently being worked in the same project.
pub(crate) fn conflicts_for(
    conn: &Connection,
    task_id: &str,
    patterns: &[String],
) -> Result<Vec<Conflict>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT t.seq, t.title, t.status, t.claimed_by, p.pattern
         FROM task_paths p
         JOIN tasks t ON t.id = p.task_id
         WHERE p.task_id <> ?1
           AND t.status IN ('claimed','in_progress')
           AND t.project = (SELECT project FROM tasks WHERE id = ?1)
         ORDER BY t.seq, p.rowid",
    )?;
    let others = stmt
        .query_map([task_id], |row| {
            Ok((row_to_scoped(row)?, row.get::<_, String>(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut conflicts = Vec::new();
    for pattern in patterns {
        for (other, other_pattern) in &others {
            if glob::intersects(pattern, other_pattern) {
                conflicts.push(Conflict {
                    pattern: pattern.clone(),
                    other_seq: other.seq,
                    other_title: other.title.clone(),
                    other_pattern: other_pattern.clone(),
                    other_status: other.status,
                    other_holder: other.holder.clone(),
                });
            }
        }
    }
    Ok(conflicts)
}

pub(crate) fn patterns_for_id(conn: &Connection, task_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT pattern FROM task_paths WHERE task_id = ?1 ORDER BY rowid")?;
    let rows = stmt.query_map([task_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn row_to_scoped(row: &Row<'_>) -> rusqlite::Result<ScopedTask> {
    let raw: String = row.get(2)?;
    let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(ScopedTask {
        seq: row.get(0)?,
        title: row.get(1)?,
        status,
        holder: row.get(3)?,
        patterns: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seed(db: &Db, title: &str) -> i64 {
        db.tasks().create(PROJECT, title, "", 0, "cli").unwrap().seq
    }

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn declarations_are_normalized_and_deduplicated() {
        let db = db();
        let seq = seed(&db, "t");
        db.scopes()
            .declare(
                seq,
                &patterns(&["./src/lib.rs", "src/lib.rs", "tests/"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        assert_eq!(
            db.scopes().for_task(seq).unwrap(),
            vec!["src/lib.rs", "tests/**"]
        );
    }

    #[test]
    fn declaring_the_same_pattern_twice_is_a_no_op() {
        let db = db();
        let seq = seed(&db, "t");
        for _ in 0..3 {
            db.scopes()
                .declare(seq, &patterns(&["src/**"]), "cli", OnConflict::Report)
                .unwrap();
        }
        assert_eq!(db.scopes().for_task(seq).unwrap(), vec!["src/**"]);
    }

    #[test]
    fn unusable_patterns_are_refused_with_advice() {
        let db = db();
        let seq = seed(&db, "t");
        let err = db
            .scopes()
            .declare(
                seq,
                &patterns(&["../etc/passwd"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a usable path pattern"), "{err}");
        assert!(db.scopes().for_task(seq).unwrap().is_empty());
    }

    #[test]
    fn overlapping_scopes_are_reported_only_while_the_other_task_is_live() {
        let db = db();
        let mine = seed(&db, "mine");
        let theirs = seed(&db, "theirs");
        db.scopes()
            .declare(theirs, &patterns(&["src/**"]), "cli", OnConflict::Report)
            .unwrap();

        // Their task is only open, so there is nothing to collide with yet.
        let quiet = db
            .scopes()
            .declare(mine, &patterns(&["src/db.rs"]), "cli", OnConflict::Report)
            .unwrap();
        assert!(quiet.is_empty());

        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();
        let conflicts = db.scopes().conflicts(mine).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].other_seq, theirs);
        assert_eq!(conflicts[0].other_pattern, "src/**");
        assert_eq!(conflicts[0].other_holder.as_deref(), Some("codex:9f2c"));
        assert!(
            conflicts[0].describe().contains("codex:9f2c"),
            "{:?}",
            conflicts[0]
        );
    }

    #[test]
    fn disjoint_scopes_never_collide() {
        let db = db();
        let mine = seed(&db, "mine");
        let theirs = seed(&db, "theirs");
        db.scopes()
            .declare(
                theirs,
                &patterns(&["src/tui/**"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();

        let conflicts = db
            .scopes()
            .declare(
                mine,
                &patterns(&["src/repo/**", "README.md"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    #[test]
    fn refusing_a_conflict_writes_nothing() {
        let db = db();
        let mine = seed(&db, "mine");
        let theirs = seed(&db, "theirs");
        db.scopes()
            .declare(theirs, &patterns(&["src/**"]), "cli", OnConflict::Report)
            .unwrap();
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();

        let err = db
            .scopes()
            .declare(mine, &patterns(&["src/db.rs"]), "cli", OnConflict::Refuse)
            .unwrap_err();
        assert!(matches!(err, Error::PathConflict { .. }), "{err:?}");
        assert!(
            err.to_string().contains("src/db.rs overlaps src/**"),
            "{err}"
        );
        assert!(
            db.scopes().for_task(mine).unwrap().is_empty(),
            "a refused declaration must leave no trace"
        );
    }

    #[test]
    fn finishing_a_task_takes_its_scope_out_of_the_way() {
        let db = db();
        let mine = seed(&db, "mine");
        let theirs = seed(&db, "theirs");
        db.scopes()
            .declare(theirs, &patterns(&["src/**"]), "cli", OnConflict::Report)
            .unwrap();
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();
        db.scopes()
            .declare(mine, &patterns(&["src/db.rs"]), "cli", OnConflict::Report)
            .unwrap();
        assert_eq!(db.scopes().conflicts(mine).unwrap().len(), 1);

        db.tasks()
            .complete(theirs, "codex:9f2c", "shipped")
            .unwrap();
        assert!(db.scopes().conflicts(mine).unwrap().is_empty());
    }

    #[test]
    fn scopes_in_other_projects_are_invisible() {
        let db = db();
        let mine = seed(&db, "mine");
        let elsewhere = db
            .tasks()
            .create("/other/project", "theirs", "", 0, "cli")
            .unwrap()
            .seq;
        db.scopes()
            .declare(elsewhere, &patterns(&["src/**"]), "cli", OnConflict::Report)
            .unwrap();
        db.tasks().claim(elsewhere, "codex:9f2c", TTL).unwrap();

        let conflicts = db
            .scopes()
            .declare(mine, &patterns(&["src/db.rs"]), "cli", OnConflict::Report)
            .unwrap();
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    #[test]
    fn declaring_everything_collides_with_every_live_scope() {
        let db = db();
        let mine = seed(&db, "mine");
        let theirs = seed(&db, "theirs");
        db.scopes()
            .declare(
                theirs,
                &patterns(&["docs/design.md"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();

        let conflicts = db
            .scopes()
            .declare(mine, &patterns(&["**"]), "cli", OnConflict::Report)
            .unwrap();
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn the_radar_groups_patterns_by_task() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        db.scopes()
            .declare(
                a,
                &patterns(&["src/a.rs", "src/b.rs"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        db.scopes()
            .declare(b, &patterns(&["docs/**"]), "cli", OnConflict::Report)
            .unwrap();
        db.tasks().claim(a, "codex:9f2c", TTL).unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        let live = db.scopes().declared(&scope, true).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].seq, a);
        assert_eq!(live[0].patterns, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(live[0].holder.as_deref(), Some("codex:9f2c"));

        let all = db.scopes().declared(&scope, false).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn clearing_removes_every_pattern_and_logs_it() {
        let db = db();
        let seq = seed(&db, "t");
        db.scopes()
            .declare(
                seq,
                &patterns(&["src/**", "docs/**"]),
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        assert_eq!(db.scopes().clear(seq, "cli").unwrap(), 2);
        assert!(db.scopes().for_task(seq).unwrap().is_empty());
        assert_eq!(db.scopes().clear(seq, "cli").unwrap(), 0);

        let task = db.tasks().get(seq).unwrap();
        let events = db.tasks().events(&task.id, 20).unwrap();
        assert!(events
            .iter()
            .any(|e| e.kind == EventKind::Scoped && e.detail.contains("cleared")));
    }

    /// Two agents declaring overlapping scopes at the same instant must not
    /// both come away thinking they are alone in the file.
    #[test]
    fn concurrent_declarations_cannot_both_miss_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope.db");
        let seqs: Vec<i64> = {
            let db = Db::open(&path).unwrap();
            (0..8)
                .map(|i| {
                    let seq = seed(&db, &format!("t{i}"));
                    db.tasks()
                        .claim(seq, &format!("harness:{i:02}"), TTL)
                        .unwrap();
                    seq
                })
                .collect()
        };

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(seqs.len()));
        let handles: Vec<_> = seqs
            .iter()
            .map(|seq| {
                let (path, seq, barrier) = (path.clone(), *seq, barrier.clone());
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    barrier.wait();
                    db.scopes()
                        .declare(seq, &patterns(&["src/shared.rs"]), "a", OnConflict::Report)
                        .unwrap()
                        .len()
                })
            })
            .collect();

        let seen: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Declarations are serialized, so the k-th writer sees the k-1 before
        // it: exactly one agent can legitimately report no conflict.
        assert_eq!(
            seen.iter().filter(|n| **n == 0).count(),
            1,
            "conflict counts were {seen:?}"
        );
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..seqs.len()).collect::<Vec<_>>());
    }
}
