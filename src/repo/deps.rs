//! The dependency graph: which tasks must finish before which others start.
//!
//! Dependencies are what let a human file a whole plan at once and then walk
//! away: the queue itself knows that task 7 cannot start until 3 and 4 are
//! done, so agents asking for work never pick it up early. The graph is a DAG
//! by construction — [`Deps::add`] refuses any edge that would close a cycle,
//! and names the chain that would have closed it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::ProjectScope;
use crate::error::{Error, Result};
use crate::model::{
    now_ts, Blocker, Clearance, EventKind, Ground, GroundStanding, Shifted, Status, TaskSummary,
};

/// The unfinished review of a task's work, where one exists — the subquery
/// every read in this module shares. Being a review *is* having recusal
/// edges (§15), so "an unfinished review of `t`" is an unfinished task
/// recused from it.
const PENDING_REVIEW: &str = "(SELECT rev.seq FROM task_recusals rec
        JOIN tasks rev ON rev.id = rec.task_id
        WHERE rec.from_task_id = t.id
          AND rev.status IN ('open','claimed','in_progress')
        ORDER BY rev.seq LIMIT 1)";

/// A task in the form the herald announces: claimable now, named well enough
/// for a hook — or the agent it summons — to know what it is being told about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimable {
    pub seq: i64,
    pub title: String,
    pub project: String,
}

/// Repository over `task_deps`.
pub struct Deps<'a> {
    conn: &'a Connection,
}

impl<'a> Deps<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Deps<'a> {
        Deps { conn }
    }

    /// Record that `seq` cannot start until `on_seq` is done.
    ///
    /// Idempotent. Refuses self-edges and anything that would make the graph
    /// cyclic; both refusals name the tasks involved.
    pub fn add(&self, seq: i64, on_seq: i64, actor: &str) -> Result<bool> {
        if seq == on_seq {
            return Err(Error::invalid(format!(
                "task {seq} cannot depend on itself"
            )));
        }
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let on_id = id_for_seq(&tx, on_seq)?;

        // The edge is `seq depends on on_seq`; it closes a cycle exactly when
        // `on_seq` can already reach `seq` by following its own dependencies.
        if let Some(path) = dependency_path(&tx, &on_id, &task_id)? {
            return Err(Error::DependencyCycle {
                seq,
                on: on_seq,
                path,
            });
        }

        let now = now_ts();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO task_deps (task_id, depends_on_id, actor, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, on_id, actor, now],
        )?;
        if inserted > 0 {
            super::tasks::insert_event(
                &tx,
                &task_id,
                &now,
                actor,
                EventKind::DepAdded,
                &format!("now waits for task {on_seq}"),
            )?;
        }
        tx.commit()?;
        Ok(inserted > 0)
    }

    /// Drop the dependency of `seq` on `on_seq`. `false` if there was none.
    pub fn remove(&self, seq: i64, on_seq: i64, actor: &str) -> Result<bool> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let on_id = id_for_seq(&tx, on_seq)?;
        let now = now_ts();
        let removed = tx.execute(
            "DELETE FROM task_deps WHERE task_id = ?1 AND depends_on_id = ?2",
            params![task_id, on_id],
        )?;
        if removed > 0 {
            super::tasks::insert_event(
                &tx,
                &task_id,
                &now,
                actor,
                EventKind::DepRemoved,
                &format!("no longer waits for task {on_seq}"),
            )?;
        }
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Everything `seq` waits for, done or not, in task order.
    pub fn blockers(&self, seq: i64) -> Result<Vec<Blocker>> {
        let task_id = id_for_seq(self.conn, seq)?;
        blockers_for_id(self.conn, &task_id)
    }

    /// Everything that waits for `seq`, in task order.
    pub fn dependents(&self, seq: i64) -> Result<Vec<Blocker>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let sql = format!(
            "SELECT t.seq, t.title, t.status, {PENDING_REVIEW} FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             WHERE d.depends_on_id = ?1
             ORDER BY t.seq"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([&task_id], row_to_blocker)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The open dependents of `seq` that now wait for nothing — what a finish
    /// just released, in the form the herald announces.
    ///
    /// Meant to be read after the finishing transaction commits, against the
    /// board as it then stands: a dependent that another agent has claimed in
    /// the meantime is not waiting for hands and is left out.
    pub fn released_by(&self, seq: i64, clearance: Clearance) -> Result<Vec<Claimable>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.seq, t.title, t.project FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             WHERE d.depends_on_id = ?1 AND t.status = 'open'
             ORDER BY t.seq",
        )?;
        let rows = stmt.query_map([&task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Claimable {
                    seq: row.get(1)?,
                    title: row.get(2)?,
                    project: row.get(3)?,
                },
            ))
        })?;
        let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut released = Vec::new();
        for (id, claimable) in candidates {
            if unmet_blockers(self.conn, &id, clearance)?.is_empty() {
                released.push(claimable);
            }
        }
        Ok(released)
    }

    /// Task `seq` in announceable form, if it is claimable right now: open,
    /// with nothing unmet. `None` otherwise — including when another agent
    /// has already taken it, which is the race this read exists to lose
    /// gracefully.
    pub fn claimable(&self, seq: i64, clearance: Clearance) -> Result<Option<Claimable>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, seq, title, project FROM tasks WHERE seq = ?1 AND status = 'open'",
                [seq],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Claimable {
                            seq: row.get(1)?,
                            title: row.get(2)?,
                            project: row.get(3)?,
                        },
                    ))
                },
            )
            .optional()?;
        match row {
            Some((id, claimable)) if unmet_blockers(self.conn, &id, clearance)?.is_empty() => {
                Ok(Some(claimable))
            }
            _ => Ok(None),
        }
    }

    /// The ground under `seq`: every finished dependency, carrying the result
    /// its finisher wrote and how far that word can currently be trusted.
    pub fn ground(&self, seq: i64) -> Result<Vec<Ground>> {
        let task_id = id_for_seq(self.conn, seq)?;
        ground_for(self.conn, &task_id)
    }

    /// Dependencies of `seq` that have stopped being `done` — the ground that
    /// has moved under a task since it was claimed.
    pub fn shifted(&self, seq: i64) -> Result<Vec<Shifted>> {
        let task_id = id_for_seq(self.conn, seq)?;
        shifted_for(self.conn, &task_id)
    }

    /// Every unfinished task in `scope` with unfinished dependencies, and
    /// which ones.
    ///
    /// One query for the whole board: the TUI and `hird ls` mark blocked tasks
    /// without asking per row. Tasks that have already finished are left out —
    /// a `done` task is not waiting for anything, whatever its edges say.
    /// Under [`Clearance::Reviewed`] a `done` dependency whose review is still
    /// unfinished counts as unmet, so the board and the claim refuse for the
    /// same reasons.
    pub fn unmet_map(
        &self,
        scope: &ProjectScope,
        clearance: Clearance,
    ) -> Result<BTreeMap<i64, Vec<i64>>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let unmet_clause = match clearance {
            Clearance::Done => "dep.status <> 'done'",
            Clearance::Reviewed => {
                "(dep.status <> 'done' OR EXISTS (SELECT 1 FROM task_recusals rec
                    JOIN tasks rev ON rev.id = rec.task_id
                    WHERE rec.from_task_id = dep.id
                      AND rev.status IN ('open','claimed','in_progress')))"
            }
        };
        let sql = format!(
            "SELECT t.seq, dep.seq FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             JOIN tasks dep ON dep.id = d.depends_on_id
             WHERE {project_clause}
               AND {unmet_clause}
               AND t.status IN ('open','claimed','in_progress')
             ORDER BY t.seq, dep.seq"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut map: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for row in rows {
            let (task, blocker) = row?;
            map.entry(task).or_default().push(blocker);
        }
        Ok(map)
    }

    /// Every edge in `scope`, as `(task, depends_on)` pairs, for the graph view.
    pub fn edges(&self, scope: &ProjectScope) -> Result<Vec<(i64, i64)>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let sql = format!(
            "SELECT t.seq, dep.seq FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             JOIN tasks dep ON dep.id = d.depends_on_id
             WHERE {project_clause}
             ORDER BY t.seq, dep.seq"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

// -------------------------------------------------------------------- helpers

/// Dependencies of `task_id` that do not clear it, cheapest form for the claim
/// path: empty means the task is ready as far as the graph is concerned.
pub(crate) fn unmet_blockers(
    conn: &Connection,
    task_id: &str,
    clearance: Clearance,
) -> Result<Vec<Blocker>> {
    Ok(blockers_for_id(conn, task_id)?
        .into_iter()
        .filter(|b| !b.is_cleared(clearance))
        .collect())
}

fn blockers_for_id(conn: &Connection, task_id: &str) -> Result<Vec<Blocker>> {
    let sql = format!(
        "SELECT t.seq, t.title, t.status, {PENDING_REVIEW} FROM task_deps d
         JOIN tasks t ON t.id = d.depends_on_id
         WHERE d.task_id = ?1
         ORDER BY t.seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([task_id], row_to_blocker)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The finished dependencies of `task_id`, each carrying its own account of
/// itself: the `result` its finisher wrote, and whether that answer is upheld,
/// merely done, or provisional under an unfinished review.
///
/// Two subqueries per row instead of two more joins, because the row count is
/// a task's direct dependencies — single digits in any real plan.
pub(crate) fn ground_for(conn: &Connection, task_id: &str) -> Result<Vec<Ground>> {
    let sql = format!(
        "SELECT t.seq, t.title, t.result, {PENDING_REVIEW},
                (SELECT v.verdict FROM task_verdicts v WHERE v.task_id = t.id
                 ORDER BY v.at DESC, v.id DESC LIMIT 1)
         FROM task_deps d
         JOIN tasks t ON t.id = d.depends_on_id
         WHERE d.task_id = ?1 AND t.status = 'done'
         ORDER BY t.seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([task_id], |row| {
        let pending: Option<i64> = row.get(3)?;
        let verdict: Option<String> = row.get(4)?;
        let standing = match (pending, verdict.as_deref()) {
            (Some(review), _) => GroundStanding::UnderReview { review },
            (None, Some("upheld")) => GroundStanding::Upheld,
            _ => GroundStanding::Done,
        };
        Ok(Ground {
            seq: row.get(0)?,
            title: row.get(1)?,
            result: row.get(2)?,
            standing,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Dependencies of `task_id` that are not `done` — for a task that is being
/// worked, the ground that has moved since its claim let it through.
///
/// The attribution is the newest verdict on record: a blocker whose latest
/// verdict is a sent-back was reopened by that review, and the review's
/// findings are sitting in its brief. Anything else — an older verdict, none
/// at all — is a human's move or a failure, and the sentence says only what
/// the status can back.
pub(crate) fn shifted_for(conn: &Connection, task_id: &str) -> Result<Vec<Shifted>> {
    let sql = "SELECT t.seq, t.title, t.status,
                (SELECT CASE WHEN v.verdict = 'sent_back' THEN rev.seq END
                 FROM task_verdicts v JOIN tasks rev ON rev.id = v.review_id
                 WHERE v.task_id = t.id
                 ORDER BY v.at DESC, v.id DESC LIMIT 1)
         FROM task_deps d
         JOIN tasks t ON t.id = d.depends_on_id
         WHERE d.task_id = ?1 AND t.status <> 'done'
         ORDER BY t.seq";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([task_id], |row| {
        let raw: String = row.get(2)?;
        let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(Shifted {
            seq: row.get(0)?,
            title: row.get(1)?,
            status,
            sent_back_by: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The shortest chain of dependencies from `from` to `to`, in task numbers.
///
/// `None` when `to` is unreachable — which, for the edge being added, is the
/// answer that means "no cycle".
pub(crate) fn dependency_path(conn: &Connection, from: &str, to: &str) -> Result<Option<Vec<i64>>> {
    if from == to {
        let seq = seq_for_id(conn, from)?;
        return Ok(Some(vec![seq, seq]));
    }
    let mut stmt = conn.prepare("SELECT depends_on_id FROM task_deps WHERE task_id = ?1")?;
    let mut parents: HashMap<String, String> = HashMap::new();
    let mut queue = VecDeque::from([from.to_string()]);
    let mut found = false;

    while let Some(current) = queue.pop_front() {
        if current == to {
            found = true;
            break;
        }
        let next: Vec<String> = stmt
            .query_map([&current], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in next {
            if id != from && !parents.contains_key(&id) {
                parents.insert(id.clone(), current.clone());
                queue.push_back(id);
            }
        }
    }
    if !found {
        return Ok(None);
    }

    // Walk the parent links back to `from`, then render as task numbers.
    let mut chain = vec![to.to_string()];
    let mut cursor = to.to_string();
    while cursor != from {
        let parent = parents
            .get(&cursor)
            .expect("every visited node has a parent link")
            .clone();
        chain.push(parent.clone());
        cursor = parent;
    }
    chain.reverse();
    let mut seqs = Vec::with_capacity(chain.len());
    for id in chain {
        seqs.push(seq_for_id(conn, &id)?);
    }
    Ok(Some(seqs))
}

fn row_to_blocker(row: &rusqlite::Row<'_>) -> rusqlite::Result<Blocker> {
    let raw: String = row.get(2)?;
    let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Blocker {
        seq: row.get(0)?,
        title: row.get(1)?,
        status,
        pending_review: row.get(3)?,
    })
}

pub(crate) fn id_for_seq(conn: &Connection, seq: i64) -> Result<String> {
    conn.query_row("SELECT id FROM tasks WHERE seq = ?1", [seq], |row| {
        row.get(0)
    })
    .optional()?
    .ok_or(Error::TaskNotFound { seq })
}

fn seq_for_id(conn: &Connection, id: &str) -> Result<i64> {
    Ok(
        conn.query_row("SELECT seq FROM tasks WHERE id = ?1", [id], |row| {
            row.get(0)
        })?,
    )
}

/// Group unfinished tasks into waves by longest dependency depth.
///
/// Depth is the length of the longest chain of unfinished dependencies behind
/// a task, so a task sits one wave after the last thing it waits for — the
/// critical path, not the shortest one. Finished dependencies do not count;
/// they are already out of the way.
pub fn dispatch_waves(tasks: &[TaskSummary], edges: &[(i64, i64)]) -> Vec<Vec<i64>> {
    let pending: BTreeSet<i64> = tasks
        .iter()
        .filter(|t| !t.status.is_terminal())
        .map(|t| t.seq)
        .collect();
    let mut depth: BTreeMap<i64, usize> = pending.iter().map(|s| (*s, 0)).collect();

    // Relax the edges until the depths settle. A cycle is impossible by
    // construction, so this terminates in at most one pass per task.
    for _ in 0..=pending.len() {
        let mut changed = false;
        for (task, needed) in edges {
            let (Some(&behind), true) = (depth.get(needed), pending.contains(task)) else {
                continue;
            };
            let want = behind + 1;
            if depth.get(task).is_some_and(|d| *d < want) {
                depth.insert(*task, want);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut waves: Vec<Vec<i64>> = Vec::new();
    for (seq, level) in depth {
        if waves.len() <= level {
            waves.resize(level + 1, Vec::new());
        }
        waves[level].push(seq);
    }
    waves
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::model::Status;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seed(db: &Db, title: &str) -> i64 {
        db.tasks().create(PROJECT, title, "", 0, "cli").unwrap().seq
    }

    fn finish(db: &Db, seq: i64) {
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        db.tasks().complete(seq, "a:1", "done").unwrap();
    }

    #[test]
    fn a_dependency_is_recorded_once_and_shows_on_both_sides() {
        let db = db();
        let first = seed(&db, "schema");
        let second = seed(&db, "api");

        assert!(db.deps().add(second, first, "cli").unwrap());
        assert!(!db.deps().add(second, first, "cli").unwrap(), "idempotent");

        let blockers = db.deps().blockers(second).unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].seq, first);
        assert_eq!(blockers[0].status, Status::Open);

        let dependents = db.deps().dependents(first).unwrap();
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].seq, second);
    }

    #[test]
    fn adding_a_dependency_records_an_event_on_the_waiting_task() {
        let db = db();
        let first = seed(&db, "schema");
        let second = seed(&db, "api");
        db.deps().add(second, first, "cli").unwrap();

        let task = db.tasks().get(second).unwrap();
        let events = db.tasks().events(&task.id, 20).unwrap();
        let added = events
            .iter()
            .find(|e| e.kind == EventKind::DepAdded)
            .expect("dep_added event");
        assert!(added.detail.contains(&format!("task {first}")), "{added:?}");
    }

    #[test]
    fn self_dependencies_are_refused() {
        let db = db();
        let seq = seed(&db, "t");
        let err = db.deps().add(seq, seq, "cli").unwrap_err();
        assert_eq!(err.to_string(), "task 1 cannot depend on itself");
    }

    #[test]
    fn a_cycle_is_refused_and_the_chain_is_named() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        let c = seed(&db, "c");
        db.deps().add(b, a, "cli").unwrap(); // b waits for a
        db.deps().add(c, b, "cli").unwrap(); // c waits for b

        // a waiting for c would close a -> c -> b -> a.
        let err = db.deps().add(a, c, "cli").unwrap_err();
        assert!(matches!(err, Error::DependencyCycle { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("task 1 cannot depend on task 3"), "{text}");
        assert!(text.contains("3 -> 2 -> 1"), "{text}");
    }

    #[test]
    fn a_diamond_is_not_a_cycle() {
        let db = db();
        let root = seed(&db, "root");
        let left = seed(&db, "left");
        let right = seed(&db, "right");
        let join = seed(&db, "join");
        db.deps().add(left, root, "cli").unwrap();
        db.deps().add(right, root, "cli").unwrap();
        db.deps().add(join, left, "cli").unwrap();
        db.deps().add(join, right, "cli").unwrap();

        assert_eq!(db.deps().blockers(join).unwrap().len(), 2);
    }

    #[test]
    fn removing_a_dependency_reports_whether_there_was_one() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        db.deps().add(b, a, "cli").unwrap();

        assert!(db.deps().remove(b, a, "cli").unwrap());
        assert!(!db.deps().remove(b, a, "cli").unwrap());
        assert!(db.deps().blockers(b).unwrap().is_empty());
    }

    #[test]
    fn dependencies_on_missing_tasks_are_reported_as_such() {
        let db = db();
        let seq = seed(&db, "t");
        assert_eq!(
            db.deps().add(seq, 99, "cli").unwrap_err().to_string(),
            "task 99 not found"
        );
        assert_eq!(
            db.deps().add(99, seq, "cli").unwrap_err().to_string(),
            "task 99 not found"
        );
    }

    #[test]
    fn only_done_dependencies_clear_a_blocker() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        db.deps().add(b, a, "cli").unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        assert_eq!(
            db.deps()
                .unmet_map(&scope, Clearance::Done)
                .unwrap()
                .get(&b),
            Some(&vec![a])
        );

        // Failing the dependency does not release the dependent: the work it
        // was waiting for still has not happened.
        db.tasks().claim(a, "x:1", TTL).unwrap();
        db.tasks().fail(a, "x:1", "nope").unwrap();
        assert_eq!(
            db.deps()
                .unmet_map(&scope, Clearance::Done)
                .unwrap()
                .get(&b),
            Some(&vec![a])
        );

        db.tasks().reopen(a, "cli", "retry").unwrap();
        finish(&db, a);
        assert!(db
            .deps()
            .unmet_map(&scope, Clearance::Done)
            .unwrap()
            .is_empty());
    }

    /// A task that has already finished is not waiting for anything, however
    /// its dependencies end up looking afterwards.
    #[test]
    fn finished_tasks_are_never_reported_as_blocked() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        db.deps().add(b, a, "cli").unwrap();
        finish(&db, a);
        finish(&db, b);
        db.tasks().reopen(a, "cli", "found a bug").unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        assert!(
            db.deps()
                .unmet_map(&scope, Clearance::Done)
                .unwrap()
                .is_empty(),
            "b is done; reopening its dependency does not un-finish it"
        );
    }

    #[test]
    fn the_unmet_map_covers_the_whole_board_in_one_query() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        let c = seed(&db, "c");
        db.deps().add(c, a, "cli").unwrap();
        db.deps().add(c, b, "cli").unwrap();
        finish(&db, a);

        let map = db
            .deps()
            .unmet_map(&ProjectScope::Only(PROJECT.into()), Clearance::Done)
            .unwrap();
        assert_eq!(map.get(&c), Some(&vec![b]));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn edges_are_listed_for_the_graph_view() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        db.deps().add(b, a, "cli").unwrap();
        finish(&db, a);

        // Unlike the unmet map, satisfied edges stay visible.
        let edges = db
            .deps()
            .edges(&ProjectScope::Only(PROJECT.into()))
            .unwrap();
        assert_eq!(edges, vec![(b, a)]);
    }

    #[test]
    fn a_long_chain_still_detects_the_closing_edge() {
        let db = db();
        let seqs: Vec<i64> = (0..25).map(|i| seed(&db, &format!("t{i}"))).collect();
        for pair in seqs.windows(2) {
            db.deps().add(pair[1], pair[0], "cli").unwrap();
        }
        let err = db.deps().add(seqs[0], seqs[24], "cli").unwrap_err();
        assert!(matches!(err, Error::DependencyCycle { .. }), "{err:?}");
        // The reported chain is the real one, all 25 hops of it.
        let text = err.to_string();
        assert!(text.contains("25 -> 24 -> 23"), "{text}");
        assert!(text.contains("3 -> 2 -> 1"), "{text}");
    }

    // ------------------------------------------------------------ the ground

    /// File `title` marked for review with a declared scope, work it, finish
    /// it, and return the review that filed itself.
    fn reviewed_done(db: &Db, seq: i64, result: &str) -> i64 {
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/loader.rs".to_string()],
                "cli",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        let finished = db.tasks().complete(seq, "codex:9f2c", result).unwrap();
        finished.review.expect("a review was filed")
    }

    #[test]
    fn the_claim_ground_carries_each_finished_dependencys_own_result() {
        let db = db();
        let schema = seed(&db, "schema");
        let api = seed(&db, "api");
        db.deps().add(api, schema, "cli").unwrap();
        db.tasks().claim(schema, "codex:9f2c", TTL).unwrap();
        db.tasks()
            .complete(schema, "codex:9f2c", "the schema lives in db.rs")
            .unwrap();

        let ground = db.deps().ground(api).unwrap();
        assert_eq!(ground.len(), 1);
        assert_eq!(ground[0].seq, schema);
        assert_eq!(
            ground[0].result.as_deref(),
            Some("the schema lives in db.rs")
        );
        assert_eq!(ground[0].standing, GroundStanding::Done);
        assert_eq!(ground[0].label(), format!("#{schema} done"));
    }

    #[test]
    fn ground_under_an_unfinished_review_says_it_is_provisional() {
        let db = db();
        let work = seed(&db, "port the loader");
        let dependent = seed(&db, "use the loader");
        db.deps().add(dependent, work, "cli").unwrap();
        let review = reviewed_done(&db, work, "ported it");

        let ground = db.deps().ground(dependent).unwrap();
        assert_eq!(ground[0].standing, GroundStanding::UnderReview { review });
        assert!(ground[0].label().contains("provisional"), "{ground:?}");

        // The verdict lands, the hedge comes off, and the word gets stronger:
        // this `done` has been read by a harness that provably did not do it.
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "read it, it holds",
                Some(crate::model::Verdict::Upheld),
            )
            .unwrap();
        let ground = db.deps().ground(dependent).unwrap();
        assert_eq!(ground[0].standing, GroundStanding::Upheld);
    }

    #[test]
    fn holds_keeps_a_dependent_waiting_until_the_verdict() {
        let db = db();
        let work = seed(&db, "port the loader");
        let dependent = seed(&db, "use the loader");
        db.deps().add(dependent, work, "cli").unwrap();
        let review = reviewed_done(&db, work, "ported it");

        // Under the default clearance the dependent is workable at once.
        let scope = ProjectScope::Only(PROJECT.into());
        assert!(db
            .deps()
            .unmet_map(&scope, Clearance::Done)
            .unwrap()
            .is_empty());

        // Under `holds`, the same board says the dependent is still waiting —
        // and the claim refusal names the review rather than the work.
        assert_eq!(
            db.deps()
                .unmet_map(&scope, Clearance::Reviewed)
                .unwrap()
                .get(&dependent),
            Some(&vec![work])
        );
        let err = db
            .tasks()
            .claim_scoped(
                dependent,
                "claude-code:af31",
                TTL,
                &[],
                super::super::OnConflict::Report,
                Clearance::Reviewed,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("under review {review}")), "{err}");
        assert!(err.contains("verdict"), "{err}");

        // The verdict clears it, with nothing else changing.
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "holds",
                Some(crate::model::Verdict::Upheld),
            )
            .unwrap();
        assert!(db
            .deps()
            .unmet_map(&scope, Clearance::Reviewed)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn shifted_ground_names_the_review_that_sent_the_work_back() {
        let db = db();
        let work = seed(&db, "port the loader");
        let dependent = seed(&db, "use the loader");
        db.deps().add(dependent, work, "cli").unwrap();
        let review = reviewed_done(&db, work, "ported it");

        // The dependent starts under the default clearance, on provisional
        // ground; then the verdict takes the ground away.
        db.tasks().claim(dependent, "copilot:11", TTL).unwrap();
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "the error path drops the lock",
                Some(crate::model::Verdict::SentBack),
            )
            .unwrap();

        let shifted = db.deps().shifted(dependent).unwrap();
        assert_eq!(shifted.len(), 1);
        assert_eq!(shifted[0].seq, work);
        assert_eq!(shifted[0].sent_back_by, Some(review));
        let said = shifted[0].describe();
        assert!(
            said.contains(&format!("sent back by review {review}")),
            "{said}"
        );
        assert!(said.contains("re-read"), "{said}");
    }

    #[test]
    fn ground_reopened_by_a_human_is_shifted_without_blaming_a_review() {
        let db = db();
        let a = seed(&db, "a");
        let b = seed(&db, "b");
        db.deps().add(b, a, "cli").unwrap();
        finish(&db, a);
        db.tasks().claim(b, "copilot:11", TTL).unwrap();
        db.tasks().reopen(a, "cli", "found a hole").unwrap();

        let shifted = db.deps().shifted(b).unwrap();
        assert_eq!(shifted[0].sent_back_by, None);
        assert!(
            shifted[0].describe().contains("was reopened"),
            "{shifted:?}"
        );
    }

    // ------------------------------------------------------------- the graph

    fn summary(seq: i64, status: Status) -> TaskSummary {
        TaskSummary {
            seq,
            project: PROJECT.into(),
            title: format!("task {seq}"),
            status,
            priority: 0,
            claimed_by: None,
            lease_expires_at: None,
            updated_at: crate::model::now_ts(),
        }
    }

    fn waves_of(tasks: &[(i64, Status)], edges: &[(i64, i64)]) -> Vec<Vec<i64>> {
        let tasks: Vec<TaskSummary> = tasks
            .iter()
            .map(|(seq, status)| TaskSummary {
                status: *status,
                ..summary(*seq, *status)
            })
            .collect();
        dispatch_waves(&tasks, edges)
    }

    #[test]
    fn independent_tasks_all_land_in_the_first_wave() {
        let waves = waves_of(
            &[(1, Status::Open), (2, Status::Open), (3, Status::Open)],
            &[],
        );
        assert_eq!(waves, vec![vec![1, 2, 3]]);
    }

    #[test]
    fn a_chain_becomes_one_task_per_wave() {
        let waves = waves_of(
            &[(1, Status::Open), (2, Status::Open), (3, Status::Open)],
            &[(2, 1), (3, 2)],
        );
        assert_eq!(waves, vec![vec![1], vec![2], vec![3]]);
    }

    /// A task waits for the *last* of its dependencies, so the layering has to
    /// follow the longest path rather than the first one it happens to find.
    #[test]
    fn a_diamond_puts_the_join_after_its_deepest_branch() {
        let waves = waves_of(
            &[
                (1, Status::Open),
                (2, Status::Open),
                (3, Status::Open),
                (4, Status::Open),
            ],
            // 4 waits for 2 and 3; 3 waits for 2; 2 waits for 1.
            &[(2, 1), (3, 2), (4, 2), (4, 3)],
        );
        assert_eq!(waves, vec![vec![1], vec![2], vec![3], vec![4]]);
    }

    #[test]
    fn finished_tasks_leave_the_graph_and_their_dependents_move_up() {
        let waves = waves_of(
            &[(1, Status::Done), (2, Status::Open), (3, Status::Open)],
            &[(2, 1), (3, 2)],
        );
        // Task 1 is done, so 2 is workable now and 3 is one wave behind it.
        assert_eq!(waves, vec![vec![2], vec![3]]);
    }

    /// What the herald asks after a finish: which dependents now wait for
    /// nothing — and only those.
    #[test]
    fn a_finish_releases_exactly_the_dependents_left_waiting_for_nothing() {
        let db = db();
        let gate = seed(&db, "gate");
        let freed = seed(&db, "freed");
        let still_waiting = seed(&db, "still waiting");
        let other_gate = seed(&db, "other gate");
        db.deps().add(freed, gate, "cli").unwrap();
        db.deps().add(still_waiting, gate, "cli").unwrap();
        db.deps().add(still_waiting, other_gate, "cli").unwrap();
        finish(&db, gate);

        let released = db.deps().released_by(gate, Clearance::Done).unwrap();
        let seqs: Vec<i64> = released.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, vec![freed]);
        assert_eq!(released[0].title, "freed");
        assert_eq!(released[0].project, PROJECT);
    }

    /// The read happens after the finish commits, and loses gracefully: a
    /// dependent somebody claimed in the gap is no longer waiting for hands.
    #[test]
    fn a_dependent_already_claimed_is_not_released() {
        let db = db();
        let gate = seed(&db, "gate");
        let freed = seed(&db, "freed");
        db.deps().add(freed, gate, "cli").unwrap();
        finish(&db, gate);
        db.tasks().claim(freed, "codex:9f2c", TTL).unwrap();
        assert!(db
            .deps()
            .released_by(gate, Clearance::Done)
            .unwrap()
            .is_empty());
    }

    /// Under `holds`, `done` under an open review releases nothing; the
    /// upholding verdict is the finish that does.
    #[test]
    fn under_holds_the_release_waits_for_the_verdict() {
        let db = db();
        let work = seed(&db, "port the loader");
        let dependent = seed(&db, "use the loader");
        db.deps().add(dependent, work, "cli").unwrap();
        let review = reviewed_done(&db, work, "ported it");

        assert!(db
            .deps()
            .released_by(work, Clearance::Reviewed)
            .unwrap()
            .is_empty());

        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "holds",
                Some(crate::model::Verdict::Upheld),
            )
            .unwrap();
        let released = db.deps().released_by(work, Clearance::Reviewed).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].seq, dependent);
    }

    /// `claimable` is the announcement's gate: open with nothing unmet, or
    /// nothing to say.
    #[test]
    fn claimable_answers_for_exactly_the_tasks_waiting_for_hands() {
        let db = db();
        let gate = seed(&db, "gate");
        let blocked = seed(&db, "blocked");
        let ready = seed(&db, "ready");
        db.deps().add(blocked, gate, "cli").unwrap();

        let claimable = db
            .deps()
            .claimable(ready, Clearance::Done)
            .unwrap()
            .unwrap();
        assert_eq!(claimable.seq, ready);
        assert_eq!(claimable.title, "ready");
        assert_eq!(claimable.project, PROJECT);

        assert!(db
            .deps()
            .claimable(blocked, Clearance::Done)
            .unwrap()
            .is_none());
        db.tasks().claim(ready, "codex:9f2c", TTL).unwrap();
        assert!(db
            .deps()
            .claimable(ready, Clearance::Done)
            .unwrap()
            .is_none());
        assert!(db
            .deps()
            .claimable(9_999, Clearance::Done)
            .unwrap()
            .is_none());
    }
}
