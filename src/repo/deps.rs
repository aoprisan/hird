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
use crate::model::{now_ts, Blocker, EventKind, Status, TaskSummary};

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
        let mut stmt = self.conn.prepare(
            "SELECT t.seq, t.title, t.status FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             WHERE d.depends_on_id = ?1
             ORDER BY t.seq",
        )?;
        let rows = stmt.query_map([&task_id], row_to_blocker)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every unfinished task in `scope` with unfinished dependencies, and
    /// which ones.
    ///
    /// One query for the whole board: the TUI and `hird ls` mark blocked tasks
    /// without asking per row. Tasks that have already finished are left out —
    /// a `done` task is not waiting for anything, whatever its edges say.
    pub fn unmet_map(&self, scope: &ProjectScope) -> Result<BTreeMap<i64, Vec<i64>>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let sql = format!(
            "SELECT t.seq, dep.seq FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             JOIN tasks dep ON dep.id = d.depends_on_id
             WHERE {project_clause}
               AND dep.status <> 'done'
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

/// Dependencies of `task_id` that are not `done`, cheapest form for the claim
/// path: empty means the task is ready as far as the graph is concerned.
pub(crate) fn unmet_blockers(conn: &Connection, task_id: &str) -> Result<Vec<Blocker>> {
    Ok(blockers_for_id(conn, task_id)?
        .into_iter()
        .filter(|b| !b.is_cleared())
        .collect())
}

fn blockers_for_id(conn: &Connection, task_id: &str) -> Result<Vec<Blocker>> {
    let mut stmt = conn.prepare(
        "SELECT t.seq, t.title, t.status FROM task_deps d
         JOIN tasks t ON t.id = d.depends_on_id
         WHERE d.task_id = ?1
         ORDER BY t.seq",
    )?;
    let rows = stmt.query_map([task_id], row_to_blocker)?;
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
        assert_eq!(db.deps().unmet_map(&scope).unwrap().get(&b), Some(&vec![a]));

        // Failing the dependency does not release the dependent: the work it
        // was waiting for still has not happened.
        db.tasks().claim(a, "x:1", TTL).unwrap();
        db.tasks().fail(a, "x:1", "nope").unwrap();
        assert_eq!(db.deps().unmet_map(&scope).unwrap().get(&b), Some(&vec![a]));

        db.tasks().reopen(a, "cli", "retry").unwrap();
        finish(&db, a);
        assert!(db.deps().unmet_map(&scope).unwrap().is_empty());
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
            db.deps().unmet_map(&scope).unwrap().is_empty(),
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
            .unmet_map(&ProjectScope::Only(PROJECT.into()))
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
}
