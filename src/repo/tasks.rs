//! Task queue repository: creation, the atomic claim, leases and the audit trail.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use super::{new_id, ProjectScope};
use crate::error::{Error, Result};
use crate::model::{fmt_ts, now_ts, EventKind, Status, Task, TaskEvent, TaskSummary, Transition};

/// Columns selected for a full [`Task`], in the order [`row_to_task`] expects.
const TASK_COLUMNS: &str = "id, seq, project, title, body, status, priority, \
                            claimed_by, lease_expires_at, result, created_at, updated_at";

/// What a lease sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// `seq` of every task whose lease expired and which returned to `open`.
    pub expired: Vec<i64>,
}

impl SweepOutcome {
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty()
    }
}

/// Repository over `tasks` and `task_events`.
pub struct Tasks<'a> {
    conn: &'a Connection,
}

impl<'a> Tasks<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Tasks<'a> {
        Tasks { conn }
    }

    fn immediate_tx(&self) -> Result<Transaction<'_>> {
        // IMMEDIATE takes the write lock up front, so two concurrent writers
        // queue on `busy_timeout` instead of deadlocking on a deferred
        // read-to-write upgrade (which SQLite fails without retrying).
        Ok(Transaction::new_unchecked(
            self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    // ---------------------------------------------------------------- create

    /// Create an `open` task and record its `created` event.
    pub fn create(
        &self,
        project: &str,
        title: &str,
        body: &str,
        priority: i64,
        actor: &str,
    ) -> Result<Task> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::invalid("task title must not be empty"));
        }
        let tx = self.immediate_tx()?;
        let seq = next_seq(&tx)?;
        let id = new_id();
        let now = now_ts();
        tx.execute(
            "INSERT INTO tasks (id, seq, project, title, body, status, priority, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?7)",
            params![id, seq, project, title, body, priority, now],
        )?;
        insert_event(
            &tx,
            &id,
            &now,
            actor,
            EventKind::Created,
            &format!("{title:?} in {project}"),
        )?;
        let task = fetch_task_by_seq(&tx, seq)?.expect("task inserted in this transaction");
        tx.commit()?;
        Ok(task)
    }

    // ----------------------------------------------------------------- reads

    /// Sweep expired leases, then list tasks in `scope`.
    ///
    /// Results are ordered so the most interesting work floats to the top:
    /// active tasks first, then by descending priority, then by recency.
    pub fn list(&self, scope: &ProjectScope, status: Option<Status>) -> Result<Vec<TaskSummary>> {
        self.sweep_leases()?;
        let (project_clause, project_value) = scope.clause("project");
        let mut sql = format!(
            "SELECT seq, project, title, status, priority, claimed_by, lease_expires_at, updated_at
             FROM tasks WHERE {project_clause}"
        );
        if status.is_some() {
            sql.push_str(" AND status = ?");
        }
        sql.push_str(
            " ORDER BY CASE status
                         WHEN 'in_progress' THEN 0
                         WHEN 'claimed' THEN 1
                         WHEN 'open' THEN 2
                         ELSE 3
                       END,
                       priority DESC, updated_at DESC, seq DESC",
        );

        let mut binds: Vec<String> = Vec::new();
        if let Some(p) = project_value {
            binds.push(p.to_string());
        }
        if let Some(s) = status {
            binds.push(s.as_str().to_string());
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            Ok(TaskSummary {
                seq: row.get(0)?,
                project: row.get(1)?,
                title: row.get(2)?,
                status: status_from_row(row, 3)?,
                priority: row.get(4)?,
                claimed_by: row.get(5)?,
                lease_expires_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Sweep expired leases, then fetch one task by its human-facing `seq`.
    pub fn get(&self, seq: i64) -> Result<Task> {
        self.get_opt(seq)?.ok_or(Error::TaskNotFound { seq })
    }

    /// Like [`Tasks::get`] but `None` instead of an error when absent.
    pub fn get_opt(&self, seq: i64) -> Result<Option<Task>> {
        self.sweep_leases()?;
        fetch_task_by_seq(self.conn, seq).map_err(Error::from)
    }

    /// The most recent `limit` events for a task, oldest first.
    pub fn events(&self, task_id: &str, limit: usize) -> Result<Vec<TaskEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, at, actor, kind, detail
             FROM (SELECT id, task_id, at, actor, kind, detail FROM task_events
                   WHERE task_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2)
             ORDER BY at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![task_id, limit as i64], |row| {
            Ok(TaskEvent {
                id: row.get(0)?,
                task_id: row.get(1)?,
                at: row.get(2)?,
                actor: row.get(3)?,
                kind: row.get::<_, String>(4)?.parse().unwrap_or(EventKind::Note),
                detail: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Task counts by status within `scope`, for the TUI status bar.
    pub fn counts(&self, scope: &ProjectScope) -> Result<BTreeMap<Status, i64>> {
        self.sweep_leases()?;
        let (project_clause, project_value) = scope.clause("project");
        let sql =
            format!("SELECT status, COUNT(*) FROM tasks WHERE {project_clause} GROUP BY status");
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((status_from_row(row, 0)?, row.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }

    /// Every distinct project that has at least one task.
    pub fn projects(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT project FROM tasks ORDER BY project")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------------------------------------------------------------- leases

    /// Return every task whose lease has run out to `open`.
    ///
    /// Expiry is enforced lazily: readers call this before touching the table,
    /// so a claim held by a dead agent self-heals within the lease TTL without
    /// any background thread.
    pub fn sweep_leases(&self) -> Result<SweepOutcome> {
        let now = now_ts();
        // Cheap read first: the common case is nothing to do, and this avoids
        // taking a write lock on every single read.
        let due: Vec<(String, i64, Option<String>)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, seq, claimed_by FROM tasks
                 WHERE status IN ('claimed','in_progress') AND lease_expires_at < ?1",
            )?;
            let rows = stmt.query_map([&now], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if due.is_empty() {
            return Ok(SweepOutcome::default());
        }

        let tx = self.immediate_tx()?;
        let mut expired = Vec::new();
        for (id, seq, holder) in due {
            // Re-check under the write lock: another process may have swept
            // this row, or the holder may have renewed, since the read above.
            let changed = tx.execute(
                "UPDATE tasks
                 SET status = 'open', claimed_by = NULL, lease_expires_at = NULL, updated_at = ?1
                 WHERE id = ?2 AND status IN ('claimed','in_progress') AND lease_expires_at < ?1",
                params![now, id],
            )?;
            if changed == 0 {
                continue;
            }
            let holder = holder.unwrap_or_else(|| "unknown".to_string());
            insert_event(
                &tx,
                &id,
                &now,
                "hird",
                EventKind::LeaseExpired,
                &format!("lease held by {holder} expired; task returned to open"),
            )?;
            expired.push(seq);
        }
        tx.commit()?;
        expired.sort_unstable();
        Ok(SweepOutcome { expired })
    }

    // ------------------------------------------------------------ agent path

    /// Atomically claim an `open` task.
    ///
    /// The compare-and-set is a single `UPDATE ... WHERE seq = ? AND status =
    /// 'open'`, so exactly one of any number of racing claimants wins. Losers
    /// get an [`Error::ClaimConflict`] naming the current holder.
    pub fn claim(&self, seq: i64, actor: &str, lease_ttl: Duration) -> Result<Task> {
        self.sweep_leases()?;
        let now = Utc::now();
        let now_s = fmt_ts(now);
        let expires = fmt_ts(now + chrono::Duration::from_std(lease_ttl).unwrap_or_default());

        let tx = self.immediate_tx()?;
        let changed = tx.execute(
            "UPDATE tasks
             SET status = 'claimed', claimed_by = ?1, lease_expires_at = ?2, updated_at = ?3
             WHERE seq = ?4 AND status = 'open'",
            params![actor, expires, now_s, seq],
        )?;
        if changed == 0 {
            drop(tx);
            let task = fetch_task_by_seq(self.conn, seq)?.ok_or(Error::TaskNotFound { seq })?;
            return Err(Error::ClaimConflict {
                seq,
                status: task.status,
                holder: task.claimed_by,
                lease_expires_at: task.lease_expires_at,
            });
        }
        insert_event(
            &tx,
            &task_id_for_seq(&tx, seq)?,
            &now_s,
            actor,
            EventKind::Claimed,
            &format!("lease until {expires}"),
        )?;
        let task = fetch_task_by_seq(&tx, seq)?.expect("claimed row exists");
        tx.commit()?;
        Ok(task)
    }

    /// Record progress on a held task and renew its lease.
    ///
    /// `start` moves `claimed` to `in_progress`; passing `false` leaves the
    /// status alone. Either way the lease is pushed out by a full TTL.
    pub fn update(
        &self,
        seq: i64,
        actor: &str,
        start: bool,
        note: &str,
        lease_ttl: Duration,
    ) -> Result<Task> {
        let note = note.trim();
        if note.is_empty() {
            return Err(Error::invalid(
                "note must not be empty; say what you did or what you found",
            ));
        }
        self.sweep_leases()?;
        let now = Utc::now();
        let now_s = fmt_ts(now);
        let expires = fmt_ts(now + chrono::Duration::from_std(lease_ttl).unwrap_or_default());

        let tx = self.immediate_tx()?;
        let task = require_holder(&tx, seq, actor)?;
        let next = if start {
            task.status
                .apply(Transition::Start)
                .ok_or(Error::InvalidTransition {
                    seq,
                    status: task.status,
                    transition: "start",
                })?
        } else {
            task.status
        };

        tx.execute(
            "UPDATE tasks SET status = ?1, lease_expires_at = ?2, updated_at = ?3 WHERE id = ?4",
            params![next.as_str(), expires, now_s, task.id],
        )?;
        if next != task.status {
            insert_event(
                &tx,
                &task.id,
                &now_s,
                actor,
                EventKind::Status,
                &format!("{} -> {}", task.status, next),
            )?;
        }
        insert_event(&tx, &task.id, &now_s, actor, EventKind::Note, note)?;
        insert_event(
            &tx,
            &task.id,
            &now_s,
            actor,
            EventKind::LeaseRenewed,
            &format!("lease until {expires}"),
        )?;
        let task = fetch_task_by_seq(&tx, seq)?.expect("updated row exists");
        tx.commit()?;
        Ok(task)
    }

    /// Finish a held task successfully.
    pub fn complete(&self, seq: i64, actor: &str, result: &str) -> Result<Task> {
        self.finish(seq, actor, Transition::Complete, result)
    }

    /// Give up on a held task.
    pub fn fail(&self, seq: i64, actor: &str, reason: &str) -> Result<Task> {
        self.finish(seq, actor, Transition::Fail, reason)
    }

    fn finish(&self, seq: i64, actor: &str, transition: Transition, result: &str) -> Result<Task> {
        let result = result.trim();
        if result.is_empty() {
            return Err(Error::invalid(match transition {
                Transition::Complete => "result must not be empty; summarise what you did",
                _ => "reason must not be empty; say why the task failed",
            }));
        }
        self.sweep_leases()?;
        let now = now_ts();
        let tx = self.immediate_tx()?;
        let task = require_holder(&tx, seq, actor)?;
        let next = task
            .status
            .apply(transition)
            .ok_or(Error::InvalidTransition {
                seq,
                status: task.status,
                transition: transition.as_str(),
            })?;
        tx.execute(
            "UPDATE tasks
             SET status = ?1, claimed_by = NULL, lease_expires_at = NULL,
                 result = ?2, updated_at = ?3
             WHERE id = ?4",
            params![next.as_str(), result, now, task.id],
        )?;
        let kind = match transition {
            Transition::Complete => EventKind::Completed,
            _ => EventKind::Failed,
        };
        insert_event(&tx, &task.id, &now, actor, kind, result)?;
        let task = fetch_task_by_seq(&tx, seq)?.expect("finished row exists");
        tx.commit()?;
        Ok(task)
    }

    // ------------------------------------------------------------ human path

    /// Abandon a task. Valid from any non-terminal status; humans only.
    pub fn cancel(&self, seq: i64, actor: &str, reason: &str) -> Result<Task> {
        self.human_move(seq, actor, Transition::Cancel, reason)
    }

    /// Put a terminal task back in the pool, clearing holder, lease and result.
    pub fn reopen(&self, seq: i64, actor: &str, reason: &str) -> Result<Task> {
        self.human_move(seq, actor, Transition::Reopen, reason)
    }

    fn human_move(
        &self,
        seq: i64,
        actor: &str,
        transition: Transition,
        detail: &str,
    ) -> Result<Task> {
        self.sweep_leases()?;
        let now = now_ts();
        let tx = self.immediate_tx()?;
        let task = fetch_task_by_seq(&tx, seq)?.ok_or(Error::TaskNotFound { seq })?;
        let next = task
            .status
            .apply(transition)
            .ok_or(Error::InvalidTransition {
                seq,
                status: task.status,
                transition: transition.as_str(),
            })?;
        match transition {
            Transition::Reopen => {
                tx.execute(
                    "UPDATE tasks
                     SET status = ?1, claimed_by = NULL, lease_expires_at = NULL,
                         result = NULL, updated_at = ?2
                     WHERE id = ?3",
                    params![next.as_str(), now, task.id],
                )?;
            }
            _ => {
                tx.execute(
                    "UPDATE tasks
                     SET status = ?1, claimed_by = NULL, lease_expires_at = NULL, updated_at = ?2
                     WHERE id = ?3",
                    params![next.as_str(), now, task.id],
                )?;
            }
        }
        let kind = match transition {
            Transition::Reopen => EventKind::Reopened,
            _ => EventKind::Cancelled,
        };
        let detail = if detail.trim().is_empty() {
            format!("{} -> {} by {}", task.status, next, actor)
        } else {
            detail.trim().to_string()
        };
        insert_event(&tx, &task.id, &now, actor, kind, &detail)?;
        let task = fetch_task_by_seq(&tx, seq)?.expect("moved row exists");
        tx.commit()?;
        Ok(task)
    }
}

// -------------------------------------------------------------------- helpers

/// Allocate the next human-facing task number.
fn next_seq(tx: &Transaction<'_>) -> Result<i64> {
    let current: i64 = tx
        .query_row("SELECT value FROM meta WHERE key = 'next_seq'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('next_seq', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [(current + 1).to_string()],
    )?;
    Ok(current)
}

fn insert_event(
    tx: &Transaction<'_>,
    task_id: &str,
    at: &str,
    actor: &str,
    kind: EventKind,
    detail: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO task_events (id, task_id, at, actor, kind, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![new_id(), task_id, at, actor, kind.as_str(), detail],
    )?;
    Ok(())
}

fn task_id_for_seq(conn: &Connection, seq: i64) -> Result<String> {
    conn.query_row("SELECT id FROM tasks WHERE seq = ?1", [seq], |row| {
        row.get(0)
    })
    .optional()?
    .ok_or(Error::TaskNotFound { seq })
}

/// Load a task and confirm `actor` currently holds its lease.
fn require_holder(conn: &Connection, seq: i64, actor: &str) -> Result<Task> {
    let task = fetch_task_by_seq(conn, seq)?.ok_or(Error::TaskNotFound { seq })?;
    if !task.status.is_active() || task.claimed_by.as_deref() != Some(actor) {
        return Err(Error::NotHolder {
            seq,
            status: task.status,
            holder: task.claimed_by.clone(),
            actor: actor.to_string(),
        });
    }
    Ok(task)
}

fn fetch_task_by_seq(conn: &Connection, seq: i64) -> rusqlite::Result<Option<Task>> {
    conn.query_row(
        &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE seq = ?1"),
        [seq],
        row_to_task,
    )
    .optional()
}

fn status_from_row(row: &Row<'_>, idx: usize) -> rusqlite::Result<Status> {
    let raw: String = row.get(idx)?;
    raw.parse().map_err(|e: crate::model::UnknownStatus| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        seq: row.get(1)?,
        project: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        status: status_from_row(row, 5)?,
        priority: row.get(6)?,
        claimed_by: row.get(7)?,
        lease_expires_at: row.get(8)?,
        result: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const TTL: Duration = Duration::from_secs(900);
    const PROJECT: &str = "/tmp/project";

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seed(db: &Db, title: &str) -> i64 {
        db.tasks()
            .create(PROJECT, title, "body", 0, "cli")
            .unwrap()
            .seq
    }

    /// Back-date a held lease so the next sweep is guaranteed to collect it.
    ///
    /// Claiming with a zero TTL would land in the same millisecond as the
    /// sweep's `now` and make these tests flaky, so the deadline is moved into
    /// the past directly.
    fn expire_lease(db: &Db, seq: i64) {
        let past = fmt_ts(Utc::now() - chrono::Duration::hours(1));
        let changed = db
            .conn()
            .execute(
                "UPDATE tasks SET lease_expires_at = ?1 WHERE seq = ?2",
                params![past, seq],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    #[test]
    fn seq_starts_at_one_and_increments() {
        let db = db();
        assert_eq!(seed(&db, "first"), 1);
        assert_eq!(seed(&db, "second"), 2);
        assert_eq!(seed(&db, "third"), 3);
    }

    #[test]
    fn creating_a_task_records_a_created_event() {
        let db = db();
        let task = db
            .tasks()
            .create(PROJECT, "build it", "", 0, "cli")
            .unwrap();
        let events = db.tasks().events(&task.id, 20).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Created);
        assert_eq!(events[0].actor, "cli");
    }

    #[test]
    fn empty_titles_are_rejected() {
        let db = db();
        let err = db.tasks().create(PROJECT, "   ", "", 0, "cli").unwrap_err();
        assert!(err.to_string().contains("title must not be empty"));
    }

    #[test]
    fn claim_moves_open_to_claimed_and_sets_a_lease() {
        let db = db();
        let seq = seed(&db, "t");
        let task = db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        assert_eq!(task.status, Status::Claimed);
        assert_eq!(task.claimed_by.as_deref(), Some("codex:9f2c"));
        let remaining = task.lease_remaining_secs(Utc::now()).unwrap();
        assert!((880..=900).contains(&remaining), "remaining = {remaining}");
    }

    #[test]
    fn a_second_claim_reports_the_current_holder() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        let err = db
            .tasks()
            .claim(seq, "claude-code:af31", TTL)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("task 1 is claimed by codex:9f2c until "),
            "{err}"
        );
    }

    #[test]
    fn claiming_a_missing_task_says_so() {
        let db = db();
        let err = db.tasks().claim(99, "a:1", TTL).unwrap_err();
        assert_eq!(err.to_string(), "task 99 not found");
    }

    #[test]
    fn only_the_holder_can_update_complete_or_fail() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();

        for err in [
            db.tasks()
                .update(seq, "other:1", true, "n", TTL)
                .unwrap_err(),
            db.tasks().complete(seq, "other:1", "r").unwrap_err(),
            db.tasks().fail(seq, "other:1", "r").unwrap_err(),
        ] {
            assert!(
                err.to_string().contains("held by codex:9f2c, not other:1"),
                "{err}"
            );
        }
    }

    #[test]
    fn unclaimed_tasks_tell_the_agent_to_claim_first() {
        let db = db();
        let seq = seed(&db, "t");
        let err = db.tasks().complete(seq, "a:1", "done").unwrap_err();
        assert_eq!(
            err.to_string(),
            "task 1 is open and unclaimed; a:1 must claim it first"
        );
    }

    #[test]
    fn update_starts_work_renews_the_lease_and_logs_a_note() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks()
            .claim(seq, "a:1", Duration::from_secs(60))
            .unwrap();
        let task = db
            .tasks()
            .update(seq, "a:1", true, "  looking at it  ", TTL)
            .unwrap();

        assert_eq!(task.status, Status::InProgress);
        assert!(task.lease_remaining_secs(Utc::now()).unwrap() > 800);

        let kinds: Vec<_> = db
            .tasks()
            .events(&task.id, 20)
            .unwrap()
            .into_iter()
            .map(|e| (e.kind, e.detail))
            .collect();
        assert!(kinds.contains(&(EventKind::Status, "claimed -> in_progress".into())));
        assert!(kinds.contains(&(EventKind::Note, "looking at it".into())));
        assert!(kinds.iter().any(|(k, _)| *k == EventKind::LeaseRenewed));
    }

    #[test]
    fn repeated_updates_stay_in_progress() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        db.tasks().update(seq, "a:1", true, "one", TTL).unwrap();
        let task = db.tasks().update(seq, "a:1", true, "two", TTL).unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn empty_notes_and_results_are_rejected() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        assert!(db
            .tasks()
            .update(seq, "a:1", false, "  ", TTL)
            .unwrap_err()
            .to_string()
            .contains("note must not be empty"));
        assert!(db
            .tasks()
            .complete(seq, "a:1", "")
            .unwrap_err()
            .to_string()
            .contains("result must not be empty"));
        assert!(db
            .tasks()
            .fail(seq, "a:1", "")
            .unwrap_err()
            .to_string()
            .contains("reason must not be empty"));
    }

    #[test]
    fn completing_clears_the_lease_and_stores_the_result() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        let task = db.tasks().complete(seq, "a:1", "shipped").unwrap();
        assert_eq!(task.status, Status::Done);
        assert_eq!(task.result.as_deref(), Some("shipped"));
        assert!(task.claimed_by.is_none());
        assert!(task.lease_expires_at.is_none());
    }

    #[test]
    fn failing_stores_the_reason() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        let task = db.tasks().fail(seq, "a:1", "upstream is down").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.result.as_deref(), Some("upstream is down"));
    }

    #[test]
    fn humans_cancel_from_any_non_terminal_status() {
        for pre in [None, Some(false), Some(true)] {
            let db = db();
            let seq = seed(&db, "t");
            if let Some(start) = pre {
                db.tasks().claim(seq, "a:1", TTL).unwrap();
                if start {
                    db.tasks().update(seq, "a:1", true, "n", TTL).unwrap();
                }
            }
            let task = db.tasks().cancel(seq, "tui", "not needed").unwrap();
            assert_eq!(task.status, Status::Cancelled);
            assert!(task.claimed_by.is_none());
        }
    }

    #[test]
    fn cancelling_a_terminal_task_is_refused() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().cancel(seq, "tui", "").unwrap();
        let err = db.tasks().cancel(seq, "tui", "").unwrap_err();
        assert_eq!(err.to_string(), "cannot cancel task 1: it is cancelled");
    }

    #[test]
    fn reopen_clears_holder_lease_and_result() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        db.tasks().fail(seq, "a:1", "nope").unwrap();
        let task = db.tasks().reopen(seq, "cli", "trying again").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.result.is_none());
        assert!(task.claimed_by.is_none());
        assert!(task.lease_expires_at.is_none());
    }

    #[test]
    fn reopening_an_open_task_is_refused() {
        let db = db();
        let seq = seed(&db, "t");
        let err = db.tasks().reopen(seq, "cli", "").unwrap_err();
        assert_eq!(err.to_string(), "cannot reopen task 1: it is open");
    }

    #[test]
    fn an_expired_lease_returns_the_task_to_open() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "codex:dead", TTL).unwrap();
        expire_lease(&db, seq);

        let outcome = db.tasks().sweep_leases().unwrap();
        assert_eq!(outcome.expired, vec![seq]);

        let task = db.tasks().get(seq).unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.claimed_by.is_none());

        let events = db.tasks().events(&task.id, 20).unwrap();
        let expiry = events
            .iter()
            .find(|e| e.kind == EventKind::LeaseExpired)
            .expect("lease_expired event");
        assert!(expiry.detail.contains("codex:dead"));

        // Sweeping again finds nothing new.
        assert!(db.tasks().sweep_leases().unwrap().is_empty());
    }

    #[test]
    fn a_swept_task_can_be_claimed_by_someone_else() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "codex:dead", TTL).unwrap();
        expire_lease(&db, seq);
        let task = db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("claude-code:af31"));
    }

    #[test]
    fn the_previous_holder_cannot_act_after_expiry() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "codex:dead", TTL).unwrap();
        expire_lease(&db, seq);
        let err = db
            .tasks()
            .complete(seq, "codex:dead", "too late")
            .unwrap_err();
        assert!(err.to_string().contains("must claim it first"), "{err}");
    }

    #[test]
    fn listing_filters_by_status_and_project() {
        let db = db();
        let open = seed(&db, "open one");
        let claimed = seed(&db, "claimed one");
        db.tasks().claim(claimed, "a:1", TTL).unwrap();
        db.tasks()
            .create("/other", "elsewhere", "", 0, "cli")
            .unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        let all = db.tasks().list(&scope, None).unwrap();
        assert_eq!(all.len(), 2);

        let only_open = db.tasks().list(&scope, Some(Status::Open)).unwrap();
        assert_eq!(only_open.len(), 1);
        assert_eq!(only_open[0].seq, open);

        let everywhere = db.tasks().list(&ProjectScope::All, None).unwrap();
        assert_eq!(everywhere.len(), 3);
    }

    #[test]
    fn listing_puts_active_work_first_then_priority() {
        let db = db();
        db.tasks().create(PROJECT, "low", "", 0, "cli").unwrap();
        let high = db
            .tasks()
            .create(PROJECT, "high", "", 5, "cli")
            .unwrap()
            .seq;
        let busy = db
            .tasks()
            .create(PROJECT, "busy", "", -1, "cli")
            .unwrap()
            .seq;
        db.tasks().claim(busy, "a:1", TTL).unwrap();
        db.tasks().update(busy, "a:1", true, "n", TTL).unwrap();

        let listed = db
            .tasks()
            .list(&ProjectScope::Only(PROJECT.into()), None)
            .unwrap();
        assert_eq!(listed[0].seq, busy);
        assert_eq!(listed[1].seq, high);
    }

    #[test]
    fn counts_group_by_status() {
        let db = db();
        seed(&db, "a");
        let b = seed(&db, "b");
        db.tasks().claim(b, "a:1", TTL).unwrap();

        let counts = db
            .tasks()
            .counts(&ProjectScope::Only(PROJECT.into()))
            .unwrap();
        assert_eq!(counts.get(&Status::Open), Some(&1));
        assert_eq!(counts.get(&Status::Claimed), Some(&1));
        assert_eq!(counts.get(&Status::Done), None);
    }

    #[test]
    fn projects_lists_each_distinct_root_once() {
        let db = db();
        seed(&db, "a");
        seed(&db, "b");
        db.tasks().create("/other", "c", "", 0, "cli").unwrap();
        assert_eq!(db.tasks().projects().unwrap(), vec!["/other", PROJECT]);
    }

    #[test]
    fn events_are_capped_and_returned_oldest_first() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        for i in 0..10 {
            db.tasks()
                .update(seq, "a:1", false, &format!("note {i}"), TTL)
                .unwrap();
        }
        let task = db.tasks().get(seq).unwrap();
        let events = db.tasks().events(&task.id, 5).unwrap();
        assert_eq!(events.len(), 5);
        assert!(events.windows(2).all(|w| w[0].at <= w[1].at));
    }

    /// The claim CAS is the one place where correctness under concurrency
    /// actually matters: N processes race for one task and exactly one wins.
    #[test]
    fn exactly_one_of_many_racing_claimants_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        let seq = {
            let db = Db::open(&path).unwrap();
            seed(&db, "contended")
        };

        const CLAIMANTS: usize = 16;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(CLAIMANTS));
        let handles: Vec<_> = (0..CLAIMANTS)
            .map(|i| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    barrier.wait();
                    db.tasks()
                        .claim(seq, &format!("harness:{i:02}"), TTL)
                        .map(|t| t.claimed_by.unwrap())
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(
            winners.len(),
            1,
            "expected exactly one winner, got {winners:?}"
        );

        let db = Db::open(&path).unwrap();
        let task = db.tasks().get(seq).unwrap();
        assert_eq!(task.status, Status::Claimed);
        assert_eq!(task.claimed_by.as_ref(), Some(winners[0]));

        // Every loser got a conflict naming the winner, not a database error.
        for err in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                matches!(err, Error::ClaimConflict { .. }),
                "loser saw {err:?}"
            );
            assert!(err.to_string().contains(winners[0]), "{err}");
        }
    }

    /// Concurrent sweeps must not double-report an expiry or corrupt state.
    #[test]
    fn concurrent_sweeps_expire_each_lease_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sweep.db");
        let seqs: Vec<i64> = {
            let db = Db::open(&path).unwrap();
            let seqs: Vec<i64> = (0..5)
                .map(|i| {
                    let seq = db
                        .tasks()
                        .create(PROJECT, &format!("t{i}"), "", 0, "cli")
                        .unwrap()
                        .seq;
                    db.tasks().claim(seq, "codex:dead", TTL).unwrap();
                    seq
                })
                .collect();
            // Back-date only after every claim, so no claim's own sweep
            // collects a lease this test still wants outstanding.
            for seq in &seqs {
                expire_lease(&db, *seq);
            }
            seqs
        };

        const SWEEPERS: usize = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(SWEEPERS));
        let handles: Vec<_> = (0..SWEEPERS)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    barrier.wait();
                    db.tasks().sweep_leases().unwrap().expired
                })
            })
            .collect();

        let mut reported: Vec<i64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        reported.sort_unstable();
        assert_eq!(
            reported, seqs,
            "each expiry must be reported by exactly one sweeper"
        );

        let db = Db::open(&path).unwrap();
        for seq in seqs {
            let task = db.tasks().get(seq).unwrap();
            assert_eq!(task.status, Status::Open);
            let expiries = db
                .tasks()
                .events(&task.id, 50)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EventKind::LeaseExpired)
                .count();
            assert_eq!(expiries, 1, "task {seq} logged {expiries} expiry events");
        }
    }

    /// Tasks are numbered by a counter in `meta`, which concurrent creators
    /// must not hand out twice.
    #[test]
    fn concurrent_creates_get_distinct_sequence_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seq.db");
        Db::open(&path).unwrap();

        const WRITERS: usize = 8;
        const EACH: usize = 5;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    barrier.wait();
                    (0..EACH)
                        .map(|i| {
                            db.tasks()
                                .create(PROJECT, &format!("w{w}-{i}"), "", 0, "cli")
                                .unwrap()
                                .seq
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let mut seqs: Vec<i64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=(WRITERS * EACH) as i64).collect::<Vec<_>>());
    }
}
