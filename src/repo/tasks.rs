//! Task queue repository: creation, the atomic claim, leases and the audit trail.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

use super::scope::OnConflict;
use super::{deps, new_id, questions, recusal, requirements, scope, ProjectScope};
use crate::error::{Error, Result};
use crate::model::{
    fmt_ts, now_ts, Clearance, Conflict, EventKind, Ground, Question, Status, Task, TaskEvent,
    TaskSummary, Transition,
};

/// Columns selected for a full [`Task`], in the order [`row_to_task`] expects.
const TASK_COLUMNS: &str = "id, seq, project, title, body, status, priority, \
                            claimed_by, lease_expires_at, result, review, created_at, updated_at";

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

/// A successful claim, with everything the claimant needs before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub task: Task,
    /// Capabilities this task required. The claim proves this session
    /// advertised every one of them.
    pub requirements: Vec<String>,
    /// The task's file scope, as it now stands on record.
    pub paths: Vec<String>,
    /// Overlaps between that scope and work other agents are holding.
    pub conflicts: Vec<Conflict>,
    /// The finished dependencies this task builds on: each one's own summary
    /// of what it did, and whether that answer is still provisional. Handed
    /// over here because the claim is the one moment the claimant is
    /// guaranteed to be listening.
    pub ground: Vec<Ground>,
    /// Questions earlier holders asked and the answers that made this task
    /// workable again. Handed over on the claim so continuation never depends
    /// on the next agent knowing to inspect history.
    pub questions: Vec<Question>,
}

/// The outcome of asking the queue for whatever should be worked next.
///
/// The reasons a request came back empty-handed are part of the answer: an
/// agent that is told "three tasks are ready but every one of them overlaps
/// files another agent is in" can say something useful to the human, where one
/// told merely "no work" cannot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dispatch {
    /// The task handed out, if any.
    pub claim: Option<Claim>,
    /// Ready tasks passed over because their file scope overlapped live work.
    pub deferred: Vec<(i64, Vec<Conflict>)>,
    /// Open tasks still waiting on unfinished dependencies.
    pub blocked: Vec<i64>,
    /// Ready tasks this harness is barred from, and what bars it. Worth saying
    /// out loud: a queue whose only remaining work is a review of your own
    /// code is not an idle queue, it is a queue waiting for another harness.
    pub recused: Vec<(i64, crate::model::Recusal)>,
    /// Ready tasks passed over because this session lacks one or more
    /// capabilities they require.
    pub incompatible: Vec<(i64, Vec<String>)>,
    /// Tasks whose every dependency is finished but for a verdict — held only
    /// under `under_review = "holds"`, each with the review it waits on. Its
    /// own bucket rather than `blocked`, because "the work is done and the
    /// review has not read it yet" points a human at a completely different
    /// fix than "the work has not happened".
    pub held: Vec<(i64, i64)>,
    /// The recess the queried project stands under, when it does. The human
    /// stood the queue down, so nothing was considered at all: every other
    /// bucket is empty on purpose, because enumerating work during a recess
    /// invites acting on it.
    pub recess: Option<crate::model::Recess>,
    /// Open tasks passed over because their own project is in recess — only
    /// under an all-projects scope, where the rest of the queue stays live.
    pub in_recess: Vec<i64>,
    /// Open tasks whose holder released them with a question: work that needs
    /// a human answer before any agent can continue it.
    ///
    /// Not exclusive with the buckets above. A task can be waiting for both an
    /// answer and a dependency, and a human told only about the question would
    /// answer it and watch nothing move.
    pub awaiting_answer: Vec<(i64, Question)>,
}

/// A finished task, and everything its completion set in motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub task: Task,
    /// The `seq` of the review task filed for this work.
    pub review: Option<i64>,
    /// When the finished task was itself a review: the verdicts it delivered,
    /// one per task it judged, each saying what the queue did about it.
    pub verdicts: Vec<super::verdict::Delivered>,
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
        super::immediate_tx(self.conn)
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
        let tx = self.immediate_tx()?;
        let task = create_in_tx(&tx, project, title, body, priority, actor)?;
        tx.commit()?;
        Ok(task)
    }

    /// Break a held task into pieces, and make it wait for them.
    ///
    /// This is how one agent puts work in front of the others. The pieces are
    /// filed as real tasks; the original starts depending on every one of
    /// them and goes back into the pool, where — being blocked — nobody can
    /// claim it until the pieces are done. The agent that called this is free
    /// immediately, and the pieces are available to every harness at once.
    ///
    /// With `sequential`, each piece also waits for the one before it, for
    /// work that genuinely has to happen in order.
    pub fn split(
        &self,
        seq: i64,
        actor: &str,
        subtasks: &[Subtask<'_>],
        sequential: bool,
    ) -> Result<(Task, Vec<Task>)> {
        if subtasks.is_empty() {
            return Err(Error::invalid(
                "a split needs at least one subtask; describe the pieces the work \
                 breaks into",
            ));
        }
        // Reject bad patterns before anything is written.
        let scopes: Vec<Vec<String>> = subtasks
            .iter()
            .map(|s| scope::normalize_all(s.paths))
            .collect::<Result<_>>()?;
        let requirements: Vec<Vec<String>> = subtasks
            .iter()
            .map(|s| crate::capability::normalize_all(s.requirements))
            .collect::<Result<_>>()?;

        self.sweep_leases()?;
        let now = now_ts();
        let tx = self.immediate_tx()?;
        let parent = require_holder(&tx, seq, actor)?;

        let mut children = Vec::with_capacity(subtasks.len());
        for ((sub, paths), required) in subtasks.iter().zip(scopes).zip(requirements) {
            let child = create_in_tx(
                &tx,
                &parent.project,
                sub.title,
                sub.body,
                sub.priority.unwrap_or(parent.priority),
                actor,
            )?;
            requirements::set_in_tx(&tx, &child.id, &required, actor, &now_ts())?;
            if !paths.is_empty() {
                scope::declare_in_tx(&tx, child.seq, &paths, actor, OnConflict::Report)?;
            }
            insert_event(
                &tx,
                &child.id,
                &now,
                actor,
                EventKind::Created,
                &format!("split out of task {seq}"),
            )?;
            // The parent waits for every piece; pieces optionally queue up
            // behind each other. Neither edge can close a cycle: these tasks
            // did not exist a moment ago.
            insert_dep(&tx, &parent.id, &child.id, actor, &now)?;
            insert_event(
                &tx,
                &parent.id,
                &now,
                actor,
                EventKind::DepAdded,
                &format!("now waits for task {} ({})", child.seq, child.title),
            )?;
            if sequential {
                if let Some(previous) = children.last() {
                    let previous: &Task = previous;
                    insert_dep(&tx, &child.id, &previous.id, actor, &now)?;
                    insert_event(
                        &tx,
                        &child.id,
                        &now,
                        actor,
                        EventKind::DepAdded,
                        &format!("now waits for task {}", previous.seq),
                    )?;
                }
            }
            children.push(child);
        }

        let numbers: Vec<String> = children.iter().map(|c| c.seq.to_string()).collect();
        let parent = release_in_tx(
            &tx,
            &parent,
            actor,
            &now,
            &format!(
                "split into task{} {}",
                plural(children.len()),
                numbers.join(", ")
            ),
        )?;
        tx.commit()?;
        Ok((parent, children))
    }

    /// Hand a held task back to the pool, unfinished but not failed.
    ///
    /// The distinction matters to whoever reads the board: `failed` is a
    /// verdict on the task and needs a human to reopen it, while a release
    /// says only that this agent stopped, and leaves the task claimable.
    pub fn release(&self, seq: i64, actor: &str, reason: &str) -> Result<Task> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::invalid(
                "reason must not be empty; say why you are handing the task back",
            ));
        }
        self.sweep_leases()?;
        let now = now_ts();
        let tx = self.immediate_tx()?;
        let task = require_holder(&tx, seq, actor)?;
        let task = release_in_tx(&tx, &task, actor, &now, reason)?;
        tx.commit()?;
        Ok(task)
    }

    /// Release a held task and park it behind a question, atomically.
    ///
    /// The task becomes `open` but the unresolved question is a readiness
    /// gate, so no agent can churn through it while the needed decision is
    /// absent. Answering is deliberately a human-side repository operation.
    pub fn release_asking(
        &self,
        seq: i64,
        actor: &str,
        reason: &str,
        question: &str,
    ) -> Result<(Task, Question)> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::invalid(
                "reason must not be empty; say what you finished before asking",
            ));
        }
        let question = question.trim();
        if question.is_empty() {
            return Err(Error::invalid(
                "question must not be empty; say what answer the task needs",
            ));
        }
        self.sweep_leases()?;
        let now = now_ts();
        let tx = self.immediate_tx()?;
        let task = require_holder(&tx, seq, actor)?;
        let asked = questions::ask_in_tx(&tx, &task.id, actor, question, &now)?;
        let task = release_in_tx(&tx, &task, actor, &now, reason)?;
        tx.commit()?;
        Ok((task, asked))
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
            "SELECT seq, project, title, status, priority, claimed_by, lease_expires_at, updated_at,
                    COALESCE((SELECT group_concat(capability, ',') FROM
                        (SELECT capability FROM task_requirements r
                         WHERE r.task_id = tasks.id ORDER BY capability)), '')
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
            let raw_requirements: String = row.get(8)?;
            Ok(TaskSummary {
                seq: row.get(0)?,
                project: row.get(1)?,
                title: row.get(2)?,
                status: status_from_row(row, 3)?,
                priority: row.get(4)?,
                claimed_by: row.get(5)?,
                lease_expires_at: row.get(6)?,
                updated_at: row.get(7)?,
                requirements: raw_requirements
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
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

    /// Map every task's internal id to its human-facing number.
    ///
    /// Assertions reference tasks by id; the TUI shows `#seq`, and one small
    /// lookup table beats a query per row.
    pub fn seq_index(&self) -> Result<BTreeMap<String, i64>> {
        let mut stmt = self.conn.prepare("SELECT id, seq FROM tasks")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
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
        self.claim_scoped(
            seq,
            actor,
            lease_ttl,
            &[],
            OnConflict::Report,
            Clearance::Done,
        )
        .map(|claim| claim.task)
    }

    /// Claim a task and declare the files it will touch, in one transaction.
    ///
    /// Doing both at once matters: a claim that succeeds and a declaration
    /// that is then refused would leave the task held by an agent that has
    /// been told not to work it. Here the refusal rolls the claim back too.
    ///
    /// Claiming is refused outright while any dependency is unfinished — a
    /// dependency the queue does not enforce is only a comment. `clearance`
    /// decides whether a dependency that is done but under an unfinished
    /// review counts as finished.
    pub fn claim_scoped(
        &self,
        seq: i64,
        actor: &str,
        lease_ttl: Duration,
        paths: &[String],
        on_conflict: OnConflict,
        clearance: Clearance,
    ) -> Result<Claim> {
        self.claim_scoped_with_capabilities(
            seq,
            actor,
            lease_ttl,
            paths,
            on_conflict,
            clearance,
            &[],
        )
    }

    /// Claim and declare scope, after checking this session's capabilities.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_scoped_with_capabilities(
        &self,
        seq: i64,
        actor: &str,
        lease_ttl: Duration,
        paths: &[String],
        on_conflict: OnConflict,
        clearance: Clearance,
        capabilities: &[String],
    ) -> Result<Claim> {
        self.sweep_leases()?;
        // Validate the patterns before anything is claimed, so a typo cannot
        // cost the caller a lease it has to hand back.
        let paths = scope::normalize_all(paths)?;
        let capabilities = crate::capability::normalize_all(capabilities)?;

        let now = Utc::now();
        let now_s = fmt_ts(now);
        let expires = fmt_ts(now + chrono::Duration::from_std(lease_ttl).unwrap_or_default());

        let tx = self.immediate_tx()?;
        let claim = claim_in_tx(
            &tx,
            seq,
            actor,
            &now_s,
            &expires,
            &paths,
            on_conflict,
            clearance,
            &capabilities,
        )?;
        tx.commit()?;
        Ok(claim)
    }

    /// Hand out the next task that is actually workable, and claim it.
    ///
    /// "Workable" is the whole point: `open`, every dependency `done`, and —
    /// when `avoid_conflicts` is set — a declared file scope that does not
    /// overlap what another agent is already inside. Several agents in several
    /// harnesses can therefore run the same loop and spread themselves across
    /// the queue without a dispatcher, without stepping on each other, and
    /// without the human assigning anything by hand.
    ///
    /// Candidates are considered by descending priority and then by age, so
    /// the queue stays first-in-first-out within a priority band.
    pub fn claim_next(
        &self,
        actor: &str,
        lease_ttl: Duration,
        scope: &ProjectScope,
        avoid_conflicts: bool,
        clearance: Clearance,
    ) -> Result<Dispatch> {
        self.claim_next_with_capabilities(actor, lease_ttl, scope, avoid_conflicts, clearance, &[])
    }

    /// Claim the next workable task this session is equipped to perform.
    pub fn claim_next_with_capabilities(
        &self,
        actor: &str,
        lease_ttl: Duration,
        scope: &ProjectScope,
        avoid_conflicts: bool,
        clearance: Clearance,
        capabilities: &[String],
    ) -> Result<Dispatch> {
        self.sweep_leases()?;
        let capabilities = crate::capability::normalize_all(capabilities)?;
        let now = Utc::now();
        let now_s = fmt_ts(now);
        let expires = fmt_ts(now + chrono::Duration::from_std(lease_ttl).unwrap_or_default());

        let tx = self.immediate_tx()?;
        let mut dispatch = Dispatch::default();
        // A recess covers the whole project, so when the asked-for scope is
        // one project the answer is one sentence rather than a bucket per
        // task: nothing below is even enumerated, because a list of work the
        // human stood the queue down over is an invitation to work it.
        if let ProjectScope::Only(project) = scope {
            if let Some(standing) = super::recess::current_in(&tx, project)? {
                dispatch.recess = Some(standing);
                tx.commit()?;
                return Ok(dispatch);
            }
        }
        let (project_clause, project_value) = scope.clause("project");
        let candidates: Vec<(i64, String, String)> = {
            let sql = format!(
                "SELECT seq, id, project FROM tasks
                 WHERE {project_clause} AND status = 'open'
                 ORDER BY priority DESC, seq ASC"
            );
            let mut stmt = tx.prepare(&sql)?;
            let binds: Vec<&str> = project_value.into_iter().collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (seq, id, project) in candidates {
            // Under an all-projects scope a recess still covers its own
            // project's tasks; they go into their own bucket rather than
            // vanishing, so the caller can say why the queue looks smaller
            // than the board.
            if scope.is_all() && super::recess::current_in(&tx, &project)?.is_some() {
                dispatch.in_recess.push(seq);
                continue;
            }
            // Recorded, but not instead of the rest of why this task is not
            // workable. A task that is both blocked and awaiting an answer
            // would otherwise reach the human as "answer this and it moves",
            // which is only half true — answering it releases nothing until
            // the work it waits on is done.
            let question = questions::unanswered_in(&tx, &id)?;
            let awaiting = question.is_some();
            if let Some(question) = question {
                dispatch.awaiting_answer.push((seq, question));
            }
            let unmet = deps::unmet_blockers(&tx, &id, clearance)?;
            if !unmet.is_empty() {
                // A task whose every blocker is `done` is not waiting for work
                // to happen — it is waiting for a verdict, and the two deserve
                // different sentences.
                let awaiting_verdict = unmet
                    .iter()
                    .all(|b| b.status == Status::Done)
                    .then(|| unmet.iter().find_map(|b| b.pending_review))
                    .flatten();
                match awaiting_verdict {
                    Some(review) => dispatch.held.push((seq, review)),
                    None => dispatch.blocked.push(seq),
                }
                continue;
            }
            if awaiting {
                continue;
            }
            // Routed around rather than handed out and then refused: an agent
            // that asked for "whatever is workable" and got an error would
            // reasonably conclude the queue was empty.
            if let Some(recusal) = recusal::bars_in(&tx, &id, actor)? {
                dispatch.recused.push((seq, recusal));
                continue;
            }
            let missing = requirements::missing_for(&tx, &id, &capabilities)?;
            if !missing.is_empty() {
                dispatch.incompatible.push((seq, missing));
                continue;
            }
            if avoid_conflicts {
                let patterns = declared_patterns(&tx, &id)?;
                let conflicts = scope::conflicts_for(&tx, &id, &patterns)?;
                if !conflicts.is_empty() {
                    dispatch.deferred.push((seq, conflicts));
                    continue;
                }
            }
            dispatch.claim = Some(claim_in_tx(
                &tx,
                seq,
                actor,
                &now_s,
                &expires,
                &[],
                OnConflict::Report,
                clearance,
                &capabilities,
            )?);
            break;
        }
        tx.commit()?;
        Ok(dispatch)
    }

    /// Everything standing between an `open` task and an agent: unfinished
    /// dependencies first, then overlaps with live work.
    pub fn readiness(
        &self,
        seq: i64,
        clearance: Clearance,
    ) -> Result<(Vec<crate::model::Blocker>, Vec<Conflict>)> {
        self.sweep_leases()?;
        let id = task_id_for_seq(self.conn, seq)?;
        let blockers = deps::unmet_blockers(self.conn, &id, clearance)?;
        let patterns = declared_patterns(self.conn, &id)?;
        let conflicts = scope::conflicts_for(self.conn, &id, &patterns)?;
        Ok((blockers, conflicts))
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
    ///
    /// Returns the task and, when it was marked for review and left something
    /// behind to look at, the review task that was filed for it. Refused when
    /// the task is a review — a review must end in a verdict, via
    /// [`Tasks::complete_with`].
    pub fn complete(&self, seq: i64, actor: &str, result: &str) -> Result<Finished> {
        self.complete_with(seq, actor, result, None)
    }

    /// Finish a held task, delivering a verdict when the task is a review.
    ///
    /// A task with recusal edges is a review of the tasks it is recused from,
    /// and completing it requires a verdict; a task without any must not
    /// carry one. `sent_back` reopens the judged work with `result` — the
    /// findings — appended to its brief, all in this same transaction.
    pub fn complete_with(
        &self,
        seq: i64,
        actor: &str,
        result: &str,
        verdict: Option<crate::model::Verdict>,
    ) -> Result<Finished> {
        self.complete_showing(seq, actor, result, verdict, None)
    }

    /// [`Tasks::complete_with`], carrying the diff of the work being finished.
    ///
    /// `exhibit` is the rendered diff of what the witness saw this task
    /// change, computed by the caller while the task was still live. If the
    /// completion files a review, the brief carries it — the reviewer is
    /// handed the change itself rather than a list of file names to go and
    /// guess it from.
    pub fn complete_showing(
        &self,
        seq: i64,
        actor: &str,
        result: &str,
        verdict: Option<crate::model::Verdict>,
        exhibit: Option<&str>,
    ) -> Result<Finished> {
        self.finish(seq, actor, Transition::Complete, result, verdict, exhibit)
    }

    /// Give up on a held task.
    pub fn fail(&self, seq: i64, actor: &str, reason: &str) -> Result<Finished> {
        self.finish(seq, actor, Transition::Fail, reason, None, None)
    }

    /// Whether finishing this task should file a review of its work.
    pub fn set_review(&self, seq: i64, review: bool, actor: &str) -> Result<()> {
        let tx = self.immediate_tx()?;
        set_review_in_tx(&tx, seq, review, actor)?;
        tx.commit()?;
        Ok(())
    }

    fn finish(
        &self,
        seq: i64,
        actor: &str,
        transition: Transition,
        result: &str,
        verdict: Option<crate::model::Verdict>,
        exhibit: Option<&str>,
    ) -> Result<Finished> {
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
        // A completing review delivers its verdict — or is refused for not
        // carrying one — inside this same transaction, so a verdict cannot
        // land without its review going done, nor the reverse. Only on
        // `complete`: failing a review means the reading did not happen, and
        // work nobody has judged stays exactly as it was.
        let verdicts = if transition == Transition::Complete {
            super::verdict::deliver_in_tx(&tx, &task, verdict, actor, result)?
        } else {
            Vec::new()
        };
        // Work marked for review puts itself in front of somebody else the
        // moment it is finished, because a review a human has to remember to
        // file is a review that does not happen. Only on `complete`: failed
        // work has nothing to check, and a review of it would be the human's
        // call, not the queue's.
        let review = if transition == Transition::Complete {
            recusal::file_review(&tx, &task, actor, result, exhibit)?
        } else {
            None
        };
        let task = fetch_task_by_seq(&tx, seq)?.expect("finished row exists");
        tx.commit()?;
        Ok(Finished {
            task,
            review,
            verdicts,
        })
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

/// One piece of a [`Tasks::split`].
#[derive(Debug, Clone)]
pub struct Subtask<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// Defaults to the parent task's priority.
    pub priority: Option<i64>,
    pub paths: &'a [String],
    /// Capabilities a claimant must advertise for this piece.
    pub requirements: &'a [String],
}

pub(crate) fn create_in_tx(
    tx: &Transaction<'_>,
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
    let seq = next_seq(tx)?;
    let id = new_id();
    let now = now_ts();
    tx.execute(
        "INSERT INTO tasks (id, seq, project, title, body, status, priority, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?7)",
        params![id, seq, project, title, body, priority, now],
    )?;
    insert_event(
        tx,
        &id,
        &now,
        actor,
        EventKind::Created,
        &format!("{title:?} in {project}"),
    )?;
    Ok(fetch_task_by_seq(tx, seq)?.expect("task inserted in this transaction"))
}

/// Move a held task back to `open`, keeping any result field untouched.
fn release_in_tx(
    tx: &Transaction<'_>,
    task: &Task,
    actor: &str,
    now: &str,
    detail: &str,
) -> Result<Task> {
    let next = task
        .status
        .apply(Transition::Release)
        .ok_or(Error::InvalidTransition {
            seq: task.seq,
            status: task.status,
            transition: "release",
        })?;
    tx.execute(
        "UPDATE tasks
         SET status = ?1, claimed_by = NULL, lease_expires_at = NULL, updated_at = ?2
         WHERE id = ?3",
        params![next.as_str(), now, task.id],
    )?;
    insert_event(tx, &task.id, now, actor, EventKind::Released, detail)?;
    Ok(fetch_task_by_seq(tx, task.seq)?.expect("released row exists"))
}

/// Mark or unmark a task for review, recording the change in its history.
///
/// Idempotent, and quiet when nothing changes: a plan applied twice must not
/// write a second event saying the same thing.
pub(crate) fn set_review_in_tx(
    tx: &Transaction<'_>,
    seq: i64,
    review: bool,
    actor: &str,
) -> Result<()> {
    let id = task_id_for_seq(tx, seq)?;
    let changed = tx.execute(
        "UPDATE tasks SET review = ?1 WHERE id = ?2 AND review <> ?1",
        params![i64::from(review), id],
    )?;
    if changed > 0 {
        insert_event(
            tx,
            &id,
            &now_ts(),
            actor,
            EventKind::Reviewed,
            if review {
                "marked for review by another harness"
            } else {
                "no longer marked for review"
            },
        )?;
    }
    Ok(())
}

pub(crate) fn insert_dep(
    tx: &Transaction<'_>,
    task_id: &str,
    depends_on_id: &str,
    actor: &str,
    now: &str,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO task_deps (task_id, depends_on_id, actor, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![task_id, depends_on_id, actor, now],
    )?;
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Claim `seq` inside a caller-owned transaction.
///
/// Every refusal — missing, blocked, already held, overlapping — happens here,
/// before the caller commits, so a rejected claim leaves no trace at all.
#[allow(clippy::too_many_arguments)]
fn claim_in_tx(
    tx: &Transaction<'_>,
    seq: i64,
    actor: &str,
    now: &str,
    expires: &str,
    paths: &[String],
    on_conflict: OnConflict,
    clearance: Clearance,
    capabilities: &[String],
) -> Result<Claim> {
    let id = task_id_for_seq(tx, seq)?;

    let (status, project): (String, String) = tx.query_row(
        "SELECT status, project FROM tasks WHERE id = ?1",
        [&id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if status == Status::Open.as_str() {
        // The recess is the first gate, and only on open tasks: a claim is a
        // hand-out and the recess stops hand-outs, while a task already held
        // is work in flight, which a recess deliberately leaves alone.
        if let Some(recess) = super::recess::current_in(tx, &project)? {
            return Err(Error::InRecess { seq, recess });
        }
        if let Some(question) = questions::unanswered_in(tx, &id)? {
            return Err(Error::AwaitingAnswer {
                seq,
                question: Box::new(question),
            });
        }
    }

    let blockers = deps::unmet_blockers(tx, &id, clearance)?;
    if !blockers.is_empty() {
        return Err(Error::Blocked { seq, blockers });
    }
    // Checked in the same transaction as the compare-and-set, so a review
    // cannot be taken by its own author through a race. Nothing is written on
    // the way to this refusal, so a rejected claim leaves no trace.
    if let Some(recusal) = recusal::bars_in(tx, &id, actor)? {
        return Err(Error::Recused {
            seq,
            recusal,
            actor: actor.to_string(),
        });
    }
    let required = requirements::for_id(tx, &id)?;
    let missing = crate::capability::missing(&required, capabilities);
    if !missing.is_empty() {
        return Err(Error::MissingCapabilities {
            seq,
            required: missing,
            available: capabilities.to_vec(),
        });
    }

    let changed = tx.execute(
        "UPDATE tasks
         SET status = 'claimed', claimed_by = ?1, lease_expires_at = ?2, updated_at = ?3
         WHERE seq = ?4 AND status = 'open'",
        params![actor, expires, now, seq],
    )?;
    if changed == 0 {
        let task = fetch_task_by_seq(tx, seq)?.ok_or(Error::TaskNotFound { seq })?;
        return Err(Error::ClaimConflict {
            seq,
            status: task.status,
            holder: task.claimed_by,
            lease_expires_at: task.lease_expires_at,
        });
    }
    insert_event(
        tx,
        &id,
        now,
        actor,
        EventKind::Claimed,
        &format!("lease until {expires}"),
    )?;

    // Declared after the claim lands, so the overlap check sees this task as
    // live and any other claimant racing it is serialized behind us.
    let conflicts = scope::declare_in_tx(tx, seq, paths, actor, on_conflict)?;
    let declared = declared_patterns(tx, &id)?;
    // A task can carry a scope from its plan even when the claimant declared
    // nothing, and that scope can still be in someone's way.
    let conflicts = if paths.is_empty() {
        scope::conflicts_for(tx, &id, &declared)?
    } else {
        conflicts
    };

    // The gate has just read these edges to let the claim through; read them
    // once more as what they always were underneath — a context channel. The
    // claimant is handed each finished dependency's own summary here, at the
    // one moment it is guaranteed to be listening.
    let ground = deps::ground_for(tx, &id)?;
    let questions = questions::history_for_id(tx, &id)?;

    let task = fetch_task_by_seq(tx, seq)?.expect("claimed row exists");
    Ok(Claim {
        task,
        requirements: required,
        paths: declared,
        conflicts,
        ground,
        questions,
    })
}

fn declared_patterns(conn: &Connection, task_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT pattern FROM task_paths WHERE task_id = ?1 ORDER BY rowid")?;
    let rows = stmt.query_map([task_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn insert_event(
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
        review: row.get::<_, i64>(10)? == 1,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
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
        let finished = db.tasks().complete(seq, "a:1", "shipped").unwrap();
        assert_eq!(finished.task.status, Status::Done);
        assert_eq!(finished.task.result.as_deref(), Some("shipped"));
        assert!(finished.task.claimed_by.is_none());
        assert!(finished.task.lease_expires_at.is_none());
        assert_eq!(finished.review, None, "nothing asked for one");
    }

    #[test]
    fn failing_stores_the_reason() {
        let db = db();
        let seq = seed(&db, "t");
        db.tasks().claim(seq, "a:1", TTL).unwrap();
        let finished = db.tasks().fail(seq, "a:1", "upstream is down").unwrap();
        assert_eq!(finished.task.status, Status::Failed);
        assert_eq!(finished.task.result.as_deref(), Some("upstream is down"));
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

    // -------------------------------------------------- dependencies & scope

    fn declare(db: &Db, seq: i64, patterns: &[&str]) {
        let patterns: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        db.scopes()
            .declare(seq, &patterns, "cli", OnConflict::Report)
            .unwrap();
    }

    fn finish(db: &Db, seq: i64) {
        db.tasks().claim(seq, "finisher:1", TTL).unwrap();
        db.tasks().complete(seq, "finisher:1", "done").unwrap();
    }

    #[test]
    fn a_blocked_task_cannot_be_claimed_and_the_refusal_names_the_blockers() {
        let db = db();
        let first = seed(&db, "schema");
        let second = seed(&db, "api");
        db.deps().add(second, first, "cli").unwrap();

        let err = db.tasks().claim(second, "a:1", TTL).unwrap_err();
        assert!(matches!(err, Error::Blocked { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("task 2 is blocked by task 1"), "{text}");
        assert!(text.contains("schema"), "{text}");

        // And the refusal really did leave the task open, not half-claimed.
        let task = db.tasks().get(second).unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.claimed_by.is_none());
    }

    #[test]
    fn finishing_the_dependency_releases_the_dependent() {
        let db = db();
        let first = seed(&db, "schema");
        let second = seed(&db, "api");
        db.deps().add(second, first, "cli").unwrap();
        finish(&db, first);

        let task = db.tasks().claim(second, "a:1", TTL).unwrap();
        assert_eq!(task.status, Status::Claimed);
    }

    #[test]
    fn claiming_with_a_scope_records_it_and_reports_overlaps() {
        let db = db();
        let theirs = seed(&db, "theirs");
        let mine = seed(&db, "mine");
        declare(&db, theirs, &["src/**"]);
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();

        let claim = db
            .tasks()
            .claim_scoped(
                mine,
                "claude-code:af31",
                TTL,
                &["src/db.rs".to_string()],
                OnConflict::Report,
                Clearance::Done,
            )
            .unwrap();
        assert_eq!(claim.task.status, Status::Claimed);
        assert_eq!(claim.paths, vec!["src/db.rs"]);
        assert_eq!(claim.conflicts.len(), 1);
        assert_eq!(claim.conflicts[0].other_seq, theirs);
    }

    #[test]
    fn a_scope_from_the_plan_is_checked_even_when_the_claimant_declares_nothing() {
        let db = db();
        let theirs = seed(&db, "theirs");
        let mine = seed(&db, "mine");
        declare(&db, theirs, &["src/**"]);
        declare(&db, mine, &["src/db.rs"]);
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();

        let claim = db
            .tasks()
            .claim_scoped(mine, "a:1", TTL, &[], OnConflict::Report, Clearance::Done)
            .unwrap();
        assert_eq!(claim.paths, vec!["src/db.rs"]);
        assert_eq!(claim.conflicts.len(), 1);
    }

    #[test]
    fn a_refused_overlap_rolls_the_claim_back_with_it() {
        let db = db();
        let theirs = seed(&db, "theirs");
        let mine = seed(&db, "mine");
        declare(&db, theirs, &["src/**"]);
        db.tasks().claim(theirs, "codex:9f2c", TTL).unwrap();

        let err = db
            .tasks()
            .claim_scoped(
                mine,
                "a:1",
                TTL,
                &["src/db.rs".to_string()],
                OnConflict::Refuse,
                Clearance::Done,
            )
            .unwrap_err();
        assert!(matches!(err, Error::PathConflict { .. }), "{err:?}");

        let task = db.tasks().get(mine).unwrap();
        assert_eq!(task.status, Status::Open, "the claim must not survive");
        assert!(db.scopes().for_task(mine).unwrap().is_empty());
    }

    #[test]
    fn an_unusable_pattern_costs_no_lease() {
        let db = db();
        let seq = seed(&db, "t");
        let err = db
            .tasks()
            .claim_scoped(
                seq,
                "a:1",
                TTL,
                &["../etc".to_string()],
                OnConflict::Report,
                Clearance::Done,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("not a usable path pattern"),
            "{err}"
        );
        assert_eq!(db.tasks().get(seq).unwrap().status, Status::Open);
    }

    // ------------------------------------------------------------- dispatch

    fn scope() -> ProjectScope {
        ProjectScope::Only(PROJECT.into())
    }

    #[test]
    fn a_claim_hands_over_the_ground_it_stands_on() {
        let db = db();
        let schema = seed(&db, "schema");
        let api = seed(&db, "api");
        db.deps().add(api, schema, "cli").unwrap();
        db.tasks().claim(schema, "codex:9f2c", TTL).unwrap();
        db.tasks()
            .complete(schema, "codex:9f2c", "the schema lives in db.rs")
            .unwrap();

        let claim = db
            .tasks()
            .claim_scoped(
                api,
                "claude-code:af31",
                TTL,
                &[],
                OnConflict::Report,
                Clearance::Done,
            )
            .unwrap();
        assert_eq!(claim.ground.len(), 1);
        assert_eq!(claim.ground[0].seq, schema);
        assert_eq!(
            claim.ground[0].result.as_deref(),
            Some("the schema lives in db.rs")
        );
    }

    #[test]
    fn next_routes_around_a_task_held_for_a_verdict_and_says_which_review() {
        let db = db();
        let work = seed(&db, "port the loader");
        let dependent = seed(&db, "use the loader");
        db.deps().add(dependent, work, "cli").unwrap();
        db.tasks().set_review(work, true, "cli").unwrap();
        declare(&db, work, &["src/loader.rs"]);
        db.tasks().claim(work, "codex:9f2c", TTL).unwrap();
        let review = db
            .tasks()
            .complete(work, "codex:9f2c", "ported it")
            .unwrap()
            .review
            .expect("a review was filed");

        // The author's harness asks for work under `holds`. The review is
        // recused from it and the dependent waits on the verdict, so the
        // dispatch comes back empty — but each refusal keeps its own reason.
        let dispatch = db
            .tasks()
            .claim_next("codex:1a2b", TTL, &scope(), true, Clearance::Reviewed)
            .unwrap();
        assert!(dispatch.claim.is_none(), "{dispatch:?}");
        assert_eq!(dispatch.held, vec![(dependent, review)]);
        assert_eq!(dispatch.recused.len(), 1);
        assert_eq!(dispatch.recused[0].0, review);

        // A harness that may take the review is handed it ahead of waiting.
        let other = db
            .tasks()
            .claim_next("claude-code:af31", TTL, &scope(), true, Clearance::Reviewed)
            .unwrap();
        assert_eq!(other.claim.unwrap().task.seq, review);
    }

    #[test]
    fn next_hands_out_the_highest_priority_task_and_claims_it() {
        let db = db();
        db.tasks().create(PROJECT, "low", "", 0, "cli").unwrap();
        let high = db
            .tasks()
            .create(PROJECT, "high", "", 5, "cli")
            .unwrap()
            .seq;

        let dispatch = db
            .tasks()
            .claim_next("codex:9f2c", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        let claim = dispatch.claim.expect("a task was available");
        assert_eq!(claim.task.seq, high);
        assert_eq!(claim.task.claimed_by.as_deref(), Some("codex:9f2c"));
    }

    #[test]
    fn next_is_first_in_first_out_within_a_priority_band() {
        let db = db();
        let first = seed(&db, "first");
        let second = seed(&db, "second");

        let a = db
            .tasks()
            .claim_next("a:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        let b = db
            .tasks()
            .claim_next("b:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert_eq!(a.claim.unwrap().task.seq, first);
        assert_eq!(b.claim.unwrap().task.seq, second);
    }

    #[test]
    fn next_walks_past_blocked_tasks_and_says_which() {
        let db = db();
        let gate = seed(&db, "gate");
        let waiting = db
            .tasks()
            .create(PROJECT, "waiting", "", 9, "cli")
            .unwrap()
            .seq;
        db.deps().add(waiting, gate, "cli").unwrap();

        // `waiting` outranks `gate` on priority but is not workable yet.
        let dispatch = db
            .tasks()
            .claim_next("a:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert_eq!(dispatch.claim.unwrap().task.seq, gate);
        assert_eq!(dispatch.blocked, vec![waiting]);
    }

    #[test]
    fn next_defers_a_task_that_would_collide_and_explains_why() {
        let db = db();
        let held = seed(&db, "held");
        let overlapping = seed(&db, "overlapping");
        declare(&db, held, &["src/**"]);
        declare(&db, overlapping, &["src/db.rs"]);
        db.tasks().claim(held, "codex:9f2c", TTL).unwrap();

        let dispatch = db
            .tasks()
            .claim_next("claude-code:af31", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert!(dispatch.claim.is_none(), "{dispatch:?}");
        assert_eq!(dispatch.deferred.len(), 1);
        let (seq, conflicts) = &dispatch.deferred[0];
        assert_eq!(*seq, overlapping);
        assert_eq!(conflicts[0].other_seq, held);
        assert_eq!(conflicts[0].other_holder.as_deref(), Some("codex:9f2c"));

        // The same request, with collision avoidance off, hands it over.
        let forced = db
            .tasks()
            .claim_next("claude-code:af31", TTL, &scope(), false, Clearance::Done)
            .unwrap();
        assert_eq!(forced.claim.unwrap().task.seq, overlapping);
    }

    #[test]
    fn next_prefers_the_task_that_does_not_collide() {
        let db = db();
        let held = seed(&db, "held");
        let overlapping = db
            .tasks()
            .create(PROJECT, "overlapping", "", 9, "cli")
            .unwrap()
            .seq;
        let free = seed(&db, "free");
        declare(&db, held, &["src/**"]);
        declare(&db, overlapping, &["src/db.rs"]);
        declare(&db, free, &["docs/**"]);
        db.tasks().claim(held, "codex:9f2c", TTL).unwrap();

        let dispatch = db
            .tasks()
            .claim_next("a:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert_eq!(dispatch.claim.unwrap().task.seq, free);
        assert_eq!(dispatch.deferred.len(), 1);
    }

    #[test]
    fn next_on_an_empty_queue_returns_nothing_and_blames_nobody() {
        let db = db();
        let dispatch = db
            .tasks()
            .claim_next("a:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert_eq!(dispatch, Dispatch::default());
    }

    #[test]
    fn a_named_claim_requires_every_capability_on_the_task() {
        let db = db();
        let seq = seed(&db, "visual QA");
        db.requirements()
            .set(seq, &["browser".into(), "network".into()], "cli")
            .unwrap();

        let err = db
            .tasks()
            .claim_scoped_with_capabilities(
                seq,
                "codex:1",
                TTL,
                &[],
                OnConflict::Report,
                Clearance::Done,
                &["browser".into()],
            )
            .unwrap_err();
        assert!(matches!(err, Error::MissingCapabilities { .. }), "{err}");
        assert!(err.to_string().contains("network"), "{err}");
        assert_eq!(db.tasks().get(seq).unwrap().status, Status::Open);

        let claim = db
            .tasks()
            .claim_scoped_with_capabilities(
                seq,
                "codex:1",
                TTL,
                &[],
                OnConflict::Report,
                Clearance::Done,
                &["network".into(), "browser".into()],
            )
            .unwrap();
        assert_eq!(claim.requirements, vec!["browser", "network"]);
    }

    #[test]
    fn next_routes_around_incompatible_work_and_explains_what_is_missing() {
        let db = db();
        let browser = db
            .tasks()
            .create(PROJECT, "browser work", "", 5, "cli")
            .unwrap()
            .seq;
        let ordinary = seed(&db, "ordinary work");
        db.requirements()
            .set(browser, &["browser".into()], "cli")
            .unwrap();

        let dispatch = db
            .tasks()
            .claim_next_with_capabilities(
                "shell:1",
                TTL,
                &scope(),
                true,
                Clearance::Done,
                &["network".into()],
            )
            .unwrap();
        assert_eq!(dispatch.claim.unwrap().task.seq, ordinary);
        assert_eq!(
            dispatch.incompatible,
            vec![(browser, vec!["browser".into()])]
        );
    }

    /// The recess stops the hand-out, not the work: a named claim on an open
    /// task is refused in the human's words, while the task already held is
    /// driven to done exactly as if nothing stood.
    #[test]
    fn a_recess_refuses_new_claims_but_leaves_work_in_flight_alone() {
        let db = db();
        let held = seed(&db, "already claimed");
        let open = seed(&db, "still open");
        db.tasks().claim(held, "codex:1", TTL).unwrap();

        db.recesses().call(PROJECT, "rebasing main", "cli").unwrap();

        let err = db
            .tasks()
            .claim(open, "claude-code:2", TTL)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(&format!(
                "task {open} is in recess: the human stood this queue down (\"rebasing main\")"
            )),
            "{err}"
        );
        assert!(err.contains("hird resume"), "{err}");
        assert_eq!(db.tasks().get(open).unwrap().status, Status::Open);

        db.tasks()
            .update(held, "codex:1", true, "still going", TTL)
            .unwrap();
        let finished = db
            .tasks()
            .complete(held, "codex:1", "done during the recess")
            .unwrap();
        assert_eq!(finished.task.status, Status::Done);

        db.recesses().lift(PROJECT).unwrap();
        db.tasks().claim(open, "claude-code:2", TTL).unwrap();
    }

    /// `task_next` during a recess answers with the recess and nothing else:
    /// no claim, and no buckets, because enumerating work the human stood the
    /// queue down over invites acting on it.
    #[test]
    fn dispatch_during_a_recess_names_the_recess_and_enumerates_nothing() {
        let db = db();
        seed(&db, "ready");
        db.recesses()
            .call(PROJECT, "merging the PR", "cli")
            .unwrap();

        let dispatch = db
            .tasks()
            .claim_next("codex:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert!(dispatch.claim.is_none());
        assert_eq!(
            dispatch.recess.as_ref().map(|r| r.reason.as_str()),
            Some("merging the PR")
        );
        assert!(dispatch.blocked.is_empty());
        assert!(dispatch.deferred.is_empty());
        assert!(dispatch.awaiting_answer.is_empty());

        db.recesses().lift(PROJECT).unwrap();
        let dispatch = db
            .tasks()
            .claim_next("codex:1", TTL, &scope(), true, Clearance::Done)
            .unwrap();
        assert!(dispatch.recess.is_none());
        assert_eq!(dispatch.claim.unwrap().task.seq, 1);
    }

    /// A recess is per project: under an all-projects scope the stood-down
    /// project's tasks are skipped into their own bucket while every other
    /// queue stays live.
    #[test]
    fn an_all_projects_dispatch_routes_around_a_recessed_project() {
        let db = db();
        let here = seed(&db, "here");
        db.tasks()
            .create("/tmp/elsewhere", "there", "", 0, "cli")
            .unwrap();
        db.recesses().call(PROJECT, "", "cli").unwrap();

        let dispatch = db
            .tasks()
            .claim_next("codex:1", TTL, &ProjectScope::All, true, Clearance::Done)
            .unwrap();
        assert_eq!(dispatch.claim.unwrap().task.project, "/tmp/elsewhere");
        assert_eq!(dispatch.in_recess, vec![here]);
        assert!(dispatch.recess.is_none(), "no single queue was asked for");
    }

    #[test]
    fn readiness_reports_both_kinds_of_obstacle() {
        let db = db();
        let gate = seed(&db, "gate");
        let held = seed(&db, "held");
        let mine = seed(&db, "mine");
        db.deps().add(mine, gate, "cli").unwrap();
        declare(&db, held, &["src/**"]);
        declare(&db, mine, &["src/db.rs"]);
        db.tasks().claim(held, "codex:9f2c", TTL).unwrap();

        let (blockers, conflicts) = db.tasks().readiness(mine, Clearance::Done).unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].seq, gate);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].other_seq, held);
    }

    /// The property the whole self-dispatch idea rests on: agents in different
    /// harnesses all asking for work at the same instant spread out over the
    /// queue instead of piling onto one task or colliding.
    #[test]
    fn concurrent_dispatch_gives_every_agent_a_different_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispatch.db");
        const AGENTS: usize = 8;
        const TASKS: usize = 12;
        {
            let db = Db::open(&path).unwrap();
            for i in 0..TASKS {
                db.tasks()
                    .create(PROJECT, &format!("t{i}"), "", 0, "cli")
                    .unwrap();
            }
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(AGENTS));
        let handles: Vec<_> = (0..AGENTS)
            .map(|i| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    barrier.wait();
                    db.tasks()
                        .claim_next(
                            &format!("harness:{i:02}"),
                            TTL,
                            &scope(),
                            true,
                            Clearance::Done,
                        )
                        .unwrap()
                        .claim
                        .map(|c| c.task.seq)
                })
            })
            .collect();

        let mut handed: Vec<i64> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(handed.len(), AGENTS, "every agent should get work");
        handed.sort_unstable();
        handed.dedup();
        assert_eq!(handed.len(), AGENTS, "two agents were handed the same task");
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
