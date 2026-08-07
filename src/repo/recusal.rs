//! Recusal: work that must not be checked by whoever did it.
//!
//! Everything else in `hird` is about getting work *done* — claimed by
//! somebody, kept out of somebody else's files, finished with a summary. The
//! summary is written by the same agent that did the work, and it is the last
//! word. Nobody else ever looks.
//!
//! For a single agent that is simply how it is; there is nobody else to ask.
//! For a swarm it is a choice, and a strange one, because the most valuable
//! property of running three different models on one codebase is precisely that
//! they are not the same model, and the cheapest way to spend that is to have
//! them read each other. Every harness can already review code. What none of
//! them can do is *know* whose code it is looking at, because a harness cannot
//! see another harness's session. hird can: it is the process all of them talk
//! to, and it recorded who held the lease.
//!
//! So a task may carry a **recusal**: an edge saying *whoever worked task N
//! must not work this one*. It is enforced at the claim, in the same
//! transaction as the compare-and-set, and dispatch routes around it rather
//! than handing out something it will then refuse.
//!
//! Two decisions are load-bearing.
//!
//! **The bar is the harness, not the session.** Two Claude Code windows are one
//! model reading its own homework, and the point of a second pair of eyes is
//! that they belong to a second pair. A recusal that only excluded the exact
//! session would be satisfied by opening a new tab, which is to say it would be
//! satisfied by nothing.
//!
//! **It is a constraint, not a scheduler.** A recusal edge says who may *not*
//! take a task; it never says who must. Nothing here pushes work at anybody,
//! nothing assigns, and a queue with one harness on it and a recusal in it
//! simply has one task nobody can claim — which the board says plainly, because
//! a review nobody can do is a fact about your setup and not a bug to paper
//! over.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::deps::id_for_seq;
use crate::error::{Error, Result};
use crate::identity::actor_harness;
use crate::model::{now_ts, EventKind, Recusal};

/// Repository over `task_recusals`.
pub struct Recusals<'a> {
    conn: &'a Connection,
}

impl<'a> Recusals<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Recusals<'a> {
        Recusals { conn }
    }

    /// Bar whoever worked `from_seq` from working `seq`.
    pub fn add(&self, seq: i64, from_seq: i64, reason: &str, actor: &str) -> Result<()> {
        if seq == from_seq {
            return Err(Error::invalid(format!(
                "task {seq} cannot be recused from itself"
            )));
        }
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        add_in_tx(&tx, seq, from_seq, reason, actor)?;
        tx.commit()?;
        Ok(())
    }

    /// Lift every recusal on `seq`. Returns how many were removed.
    pub fn clear(&self, seq: i64, actor: &str) -> Result<usize> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let task_id = id_for_seq(&tx, seq)?;
        let removed = tx.execute("DELETE FROM task_recusals WHERE task_id = ?1", [&task_id])?;
        if removed > 0 {
            super::tasks::insert_event(
                &tx,
                &task_id,
                &now_ts(),
                actor,
                EventKind::Recused,
                "recusals lifted",
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// What task `seq` is recused from, and who that bars.
    pub fn for_task(&self, seq: i64) -> Result<Vec<Recusal>> {
        let task_id = id_for_seq(self.conn, seq)?;
        recusals_in(self.conn, &task_id)
    }

    /// Every task in `scope` that is a review, and the task it reviews.
    ///
    /// One query for the whole board: a card that says `reviews #4` is telling
    /// the human something the title alone cannot, namely that this work is
    /// waiting for a harness rather than for time.
    pub fn reviews(
        &self,
        scope: &super::ProjectScope,
    ) -> Result<std::collections::BTreeMap<i64, i64>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let sql = format!(
            "SELECT t.seq, MIN(f.seq) FROM task_recusals r
             JOIN tasks t ON t.id = r.task_id
             JOIN tasks f ON f.id = r.from_task_id
             WHERE {project_clause}
             GROUP BY t.seq"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether `actor` may claim task `seq`, and why not.
    ///
    /// `None` means yes. Sweep leases before calling: the answer depends on who
    /// last held the tasks this one is recused from.
    pub fn bars(&self, seq: i64, actor: &str) -> Result<Option<Recusal>> {
        let task_id = id_for_seq(self.conn, seq)?;
        bars_in(self.conn, &task_id, actor)
    }
}

/// The tasks `task_id` is recused from, each with whoever worked it.
pub(super) fn recusals_in(conn: &Connection, task_id: &str) -> Result<Vec<Recusal>> {
    let mut stmt = conn.prepare(
        "SELECT t.seq, t.title, r.reason FROM task_recusals r
         JOIN tasks t ON t.id = r.from_task_id
         WHERE r.task_id = ?1 ORDER BY t.seq",
    )?;
    let rows = stmt.query_map([task_id], |row| {
        Ok(Recusal {
            from_seq: row.get(0)?,
            from_title: row.get(1)?,
            reason: row.get(2)?,
            worker: None,
        })
    })?;
    let mut out: Vec<Recusal> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    for recusal in &mut out {
        let from_id = id_for_seq(conn, recusal.from_seq)?;
        recusal.worker = worker_of(conn, &from_id)?;
    }
    Ok(out)
}

/// The first recusal that bars `actor` from `task_id`, if any.
pub(super) fn bars_in(conn: &Connection, task_id: &str, actor: &str) -> Result<Option<Recusal>> {
    let mine = actor_harness(actor);
    Ok(recusals_in(conn, task_id)?.into_iter().find(|recusal| {
        recusal
            .worker
            .as_deref()
            .is_some_and(|worker| actor_harness(worker) == mine)
    }))
}

/// Who worked a task, as far as the record goes.
///
/// Completing clears `claimed_by`, so the answer lives in the append-only
/// trail: the most recent claim or finish, whichever is later. That also gets
/// the awkward cases right for free — a task reopened and redone by somebody
/// else is credited to the second agent, and a task whose lease lapsed and was
/// taken up by a third names the one actually holding it.
pub(super) fn worker_of(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT actor FROM task_events
             WHERE task_id = ?1 AND kind IN ('claimed','completed','failed')
             ORDER BY at DESC, id DESC LIMIT 1",
            [task_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// File a review of `task`'s work, if it asked for one and left something to
/// look at.
///
/// The review is an ordinary task — claimable, dispatchable, cancellable, with
/// a scope and a history like any other. The only thing that makes it a review
/// is the recusal edge, which is also the only thing hird is uniquely able to
/// enforce.
///
/// Three things it refuses to do. It will not file a review of work the witness
/// saw no trace of, because a review with nothing to read is busywork on
/// somebody's board. It will not file one while some unfinished task is already
/// recused from this work — which is exactly what being its review means, so
/// this covers both a review still open and one a human filed by hand, and it
/// is what keeps a reopened task from stacking them up. And it never fails the
/// completion: a task that could not have a review filed for it is still done.
pub(super) fn file_review(
    tx: &Transaction<'_>,
    task: &crate::model::Task,
    actor: &str,
    result: &str,
    exhibit: Option<&str>,
) -> Result<Option<i64>> {
    if !task.review || already_under_review(tx, &task.id)? {
        return Ok(None);
    }
    // What to read. The witness's account first, because it is the one nobody
    // wrote about themselves; the declared scope where there is no witness.
    let observed: Vec<String> = super::witness::Witnessed::new(tx)
        .touched(task.seq)?
        .into_iter()
        .map(|o| o.describe())
        .collect();
    let declared = super::scope::Scopes::new(tx).for_task(task.seq)?;
    if observed.is_empty() && declared.is_empty() {
        return Ok(None);
    }

    let body = review_body(task, result, &observed, &declared, exhibit);
    let review = super::tasks::create_in_tx(
        tx,
        &task.project,
        &format!("Review: {}", task.title),
        &body,
        task.priority,
        actor,
    )?;
    // Scoped to what actually moved, so the collision detector and recall both
    // treat the review as work in those files — which it is.
    let paths: Vec<String> = super::witness::Witnessed::new(tx)
        .touched(task.seq)?
        .into_iter()
        .map(|o| o.path)
        .collect();
    let scope = if paths.is_empty() { declared } else { paths };
    super::scope::declare_in_tx(tx, review.seq, &scope, actor, super::OnConflict::Report)?;
    add_in_tx(
        tx,
        review.seq,
        task.seq,
        "no agent reviews its own work",
        actor,
    )?;

    let now = now_ts();
    super::tasks::insert_event(
        tx,
        &task.id,
        &now,
        actor,
        EventKind::Reviewed,
        &format!("review filed as task {}", review.seq),
    )?;
    Ok(Some(review.seq))
}

/// Is there already an unfinished review of this task?
fn already_under_review(tx: &Transaction<'_>, task_id: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT COUNT(*) FROM task_recusals r JOIN tasks t ON t.id = r.task_id
         WHERE r.from_task_id = ?1 AND t.status NOT IN ('done','failed','cancelled')",
        [task_id],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

/// What the reviewing agent is handed: the claim, and how to check it.
///
/// Written for a model that has never seen this work. It says what the author
/// says it did, what the disk says moved, and — the part that matters — that
/// the summary is the thing under review rather than the brief.
fn review_body(
    task: &crate::model::Task,
    result: &str,
    observed: &[String],
    declared: &[String],
    exhibit: Option<&str>,
) -> String {
    let mut out = format!(
        "Review the work done on task {} ({}).\n\nWhat its agent said it did:\n\n> {}\n",
        task.seq,
        task.title,
        result.replace('\n', "\n> "),
    );
    if !observed.is_empty() {
        out.push_str(&format!(
            "\nWhat the working tree was seen to do while it was held:\n\n- {}\n",
            observed.join("\n- ")
        ));
    } else if !declared.is_empty() {
        out.push_str(&format!(
            "\nThe files it said it would touch (nothing was witnessed):\n\n- {}\n",
            declared.join("\n- ")
        ));
    }
    // The change itself, as the witness kept it — not as anybody described
    // it. This is what makes the review a reading of the work rather than a
    // reading of the summary.
    if let Some(diff) = exhibit.map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(&format!(
            "\nThe change under review, as the witness kept it:\n\n```diff\n{diff}\n```\n"
        ));
    }
    if !task.body.trim().is_empty() {
        out.push_str(&format!("\nThe original brief:\n\n{}\n", task.body.trim()));
    }
    out.push_str(
        "\nRead the code, not the summary — the summary is what is under review. \
         Complete this task with what you found and a verdict: `upheld` if the work \
         stands, `sent_back` if it does not. Sending it back reopens the work with \
         your findings appended to its brief, so write the result as instructions \
         for whoever redoes it — they will not see your session. Do not fix the \
         work here either way, and `mem_store` anything durable you learn.\n",
    );
    out
}

pub(super) fn add_in_tx(
    tx: &Transaction<'_>,
    seq: i64,
    from_seq: i64,
    reason: &str,
    actor: &str,
) -> Result<()> {
    let task_id = id_for_seq(tx, seq)?;
    let from_id = id_for_seq(tx, from_seq)?;
    let now = now_ts();
    let changed = tx.execute(
        "INSERT INTO task_recusals (task_id, from_task_id, reason, actor, at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(task_id, from_task_id) DO NOTHING",
        params![task_id, from_id, reason.trim(), actor, now],
    )?;
    if changed > 0 {
        super::tasks::insert_event(
            tx,
            &task_id,
            &now,
            actor,
            EventKind::Recused,
            &format!("recused from task {from_seq}"),
        )?;
    }
    Ok(())
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

    fn task(db: &Db, title: &str) -> i64 {
        db.tasks().create(PROJECT, title, "", 0, "cli").unwrap().seq
    }

    /// Work a task from claim to completion as `actor`.
    fn worked(db: &Db, seq: i64, actor: &str) {
        db.tasks().claim(seq, actor, TTL).unwrap();
        db.tasks().complete(seq, actor, "done").unwrap();
    }

    #[test]
    fn the_agent_that_did_the_work_cannot_claim_the_review_of_it() {
        let db = db();
        let work = task(&db, "Port the loader");
        let review = task(&db, "Review the loader port");
        worked(&db, work, "codex:9f2c");
        db.recusals()
            .add(review, work, "reviews its own work", "cli")
            .unwrap();

        let barred = db.recusals().bars(review, "codex:9f2c").unwrap().unwrap();
        assert_eq!(barred.from_seq, work);
        assert_eq!(barred.worker.as_deref(), Some("codex:9f2c"));

        let err = db
            .tasks()
            .claim(review, "codex:9f2c", TTL)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("task {work}")), "{err}");
        assert!(err.contains("codex:9f2c worked"), "{err}");
        assert!(err.contains("a different harness"), "{err}");
    }

    /// The bar is the harness. A second window of the same tool is the same
    /// model reading its own homework.
    #[test]
    fn another_session_of_the_same_harness_is_barred_too() {
        let db = db();
        let work = task(&db, "Port the loader");
        let review = task(&db, "Review the loader port");
        worked(&db, work, "codex:9f2c");
        db.recusals().add(review, work, "", "cli").unwrap();

        assert!(db.recusals().bars(review, "codex:1a2b").unwrap().is_some());
        assert!(db
            .recusals()
            .bars(review, "claude-code:af31")
            .unwrap()
            .is_none());
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
    }

    /// Nobody has worked the task yet, so nobody is barred — a recusal filed
    /// ahead of the work it refers to must not lock the queue.
    #[test]
    fn a_recusal_on_work_nobody_has_done_bars_nobody() {
        let db = db();
        let work = task(&db, "Port the loader");
        let review = task(&db, "Review the loader port");
        db.recusals().add(review, work, "", "cli").unwrap();

        assert!(db.recusals().bars(review, "codex:9f2c").unwrap().is_none());
        db.tasks().claim(review, "codex:9f2c", TTL).unwrap();
    }

    /// Work handed on is credited to whoever actually finished it, because that
    /// is the agent whose reading a second pair of eyes is meant to be second
    /// to.
    #[test]
    fn the_worker_is_whoever_last_held_the_task() {
        let db = db();
        let work = task(&db, "Port the loader");
        db.tasks().claim(work, "codex:9f2c", TTL).unwrap();
        db.tasks()
            .release(work, "codex:9f2c", "out of my depth")
            .unwrap();
        worked(&db, work, "claude-code:af31");

        let review = task(&db, "Review the loader port");
        db.recusals().add(review, work, "", "cli").unwrap();
        assert!(db
            .recusals()
            .bars(review, "claude-code:af31")
            .unwrap()
            .is_some());
        // Codex touched it and gave it back; it did not write what is there.
        assert!(db.recusals().bars(review, "codex:9f2c").unwrap().is_none());
    }

    #[test]
    fn recusals_are_listed_with_who_they_bar_and_can_be_lifted() {
        let db = db();
        let a = task(&db, "Port the loader");
        let b = task(&db, "Port the renderer");
        let review = task(&db, "Review both");
        worked(&db, a, "codex:9f2c");
        worked(&db, b, "claude-code:af31");
        db.recusals().add(review, a, "wrote it", "cli").unwrap();
        db.recusals().add(review, b, "", "cli").unwrap();

        let listed = db.recusals().for_task(review).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].from_seq, a);
        assert_eq!(listed[0].worker.as_deref(), Some("codex:9f2c"));
        assert_eq!(listed[0].reason, "wrote it");
        assert!(listed[0].describe().contains("codex:9f2c"), "{listed:?}");

        assert_eq!(db.recusals().clear(review, "cli").unwrap(), 2);
        assert!(db.recusals().for_task(review).unwrap().is_empty());
        db.tasks().claim(review, "codex:9f2c", TTL).unwrap();
    }

    #[test]
    fn adding_the_same_recusal_twice_is_a_no_op() {
        let db = db();
        let work = task(&db, "Port the loader");
        let review = task(&db, "Review it");
        db.recusals().add(review, work, "", "cli").unwrap();
        db.recusals().add(review, work, "", "cli").unwrap();
        assert_eq!(db.recusals().for_task(review).unwrap().len(), 1);
    }

    #[test]
    fn a_task_cannot_be_recused_from_itself_or_from_nothing() {
        let db = db();
        let seq = task(&db, "Port the loader");
        assert!(db
            .recusals()
            .add(seq, seq, "", "cli")
            .unwrap_err()
            .to_string()
            .contains("itself"));
        assert_eq!(
            db.recusals()
                .add(seq, 404, "", "cli")
                .unwrap_err()
                .to_string(),
            "task 404 not found"
        );
    }

    /// The whole loop, in one place: work marked for review finishes, files
    /// its own review scoped to what actually moved, and hands it to somebody
    /// else — with nobody having remembered to do any of it.
    #[test]
    fn finishing_reviewed_work_files_the_review_and_hands_it_on() {
        let db = db();
        let seq = db
            .tasks()
            .create(
                PROJECT,
                "Port the config loader",
                "keep the precedence",
                3,
                "cli",
            )
            .unwrap()
            .seq;
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        // The witness saw two files move under it.
        db.witnessed()
            .begin(seq, &crate::witness::Tree::default())
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[
                    crate::witness::Change {
                        path: "src/config.rs".into(),
                        kind: crate::witness::ChangeKind::Modified,
                        hash: "h1".into(),
                    },
                    crate::witness::Change {
                        path: "tests/config.rs".into(),
                        kind: crate::witness::ChangeKind::Added,
                        hash: "h2".into(),
                    },
                ],
                "codex:9f2c",
            )
            .unwrap();

        let finished = db
            .tasks()
            .complete(seq, "codex:9f2c", "ported it, env still wins")
            .unwrap();
        let review = finished.review.expect("a review was filed");

        let filed = db.tasks().get(review).unwrap();
        assert_eq!(filed.title, "Review: Port the config loader");
        assert_eq!(filed.priority, 3, "it matters as much as the work did");
        assert!(
            filed.body.contains("ported it, env still wins"),
            "{}",
            filed.body
        );
        assert!(
            filed.body.contains("src/config.rs (modified)"),
            "{}",
            filed.body
        );
        assert!(filed.body.contains("keep the precedence"), "{}", filed.body);
        assert!(
            filed.body.contains("not the summary"),
            "the reviewer has to be told what is under review: {}",
            filed.body
        );
        // Scoped to what moved, so the collision detector and recall treat it
        // as work in those files — which it is.
        assert_eq!(
            db.scopes().for_task(review).unwrap(),
            vec!["src/config.rs", "tests/config.rs"]
        );

        // And the author cannot take it.
        assert!(db.tasks().claim(review, "codex:1a2b", TTL).is_err());
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
    }

    /// A completion that carries the diff of its work hands it to the review:
    /// the reviewer reads the change itself, not a list of file names.
    #[test]
    fn the_review_brief_carries_the_diff_when_the_completion_brought_one() {
        let db = db();
        let seq = task(&db, "Port the config loader");
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        db.witnessed()
            .begin(seq, &crate::witness::Tree::default())
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[crate::witness::Change {
                    path: "src/config.rs".into(),
                    kind: crate::witness::ChangeKind::Modified,
                    hash: "h1".into(),
                }],
                "codex:9f2c",
            )
            .unwrap();

        let diff = "--- a/src/config.rs\n+++ b/src/config.rs\n-old\n+new\n";
        let finished = db
            .tasks()
            .complete_showing(seq, "codex:9f2c", "ported", None, Some(diff))
            .unwrap();
        let filed = db
            .tasks()
            .get(finished.review.expect("a review was filed"))
            .unwrap();
        assert!(
            filed.body.contains("as the witness kept it"),
            "{}",
            filed.body
        );
        assert!(filed.body.contains("```diff"), "{}", filed.body);
        assert!(filed.body.contains("+new"), "{}", filed.body);
        // And a completion with nothing to show adds no empty section.
        let quiet = task(&db, "Port it again");
        db.tasks().set_review(quiet, true, "cli").unwrap();
        db.tasks().claim(quiet, "codex:9f2c", TTL).unwrap();
        db.witnessed()
            .begin(quiet, &crate::witness::Tree::default())
            .unwrap();
        db.witnessed()
            .record(
                quiet,
                &[crate::witness::Change {
                    path: "src/config.rs".into(),
                    kind: crate::witness::ChangeKind::Modified,
                    hash: "h2".into(),
                }],
                "codex:9f2c",
            )
            .unwrap();
        let finished = db
            .tasks()
            .complete_showing(quiet, "codex:9f2c", "ported", None, Some("  "))
            .unwrap();
        let filed = db
            .tasks()
            .get(finished.review.expect("a review was filed"))
            .unwrap();
        assert!(!filed.body.contains("```diff"), "{}", filed.body);
    }

    /// A review with nothing to read is busywork on somebody's board.
    #[test]
    fn work_that_left_no_trace_files_no_review() {
        let db = db();
        let seq = task(&db, "Think about the problem");
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();

        let finished = db.tasks().complete(seq, "codex:9f2c", "thought").unwrap();
        assert_eq!(finished.review, None);
    }

    /// Failing is not finishing. There is nothing to check, and whether the
    /// attempt is worth reading is the human's call.
    #[test]
    fn failed_work_files_no_review() {
        let db = db();
        let seq = task(&db, "Port the loader");
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/config.rs".to_string()],
                "cli",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();

        let finished = db
            .tasks()
            .fail(seq, "codex:9f2c", "no credentials")
            .unwrap();
        assert_eq!(finished.review, None);
    }

    /// Reopening finished work and completing it again must not stack reviews
    /// up on the board.
    #[test]
    fn a_second_completion_does_not_file_a_second_review() {
        let db = db();
        let seq = task(&db, "Port the loader");
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/config.rs".to_string()],
                "cli",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        let first = db.tasks().complete(seq, "codex:9f2c", "ported").unwrap();
        let review = first.review.expect("filed once");

        db.tasks().reopen(seq, "cli", "not quite").unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        let again = db
            .tasks()
            .complete(seq, "codex:9f2c", "ported again")
            .unwrap();
        assert_eq!(again.review, None, "the first review is still open");

        // Once it has been dealt with, the next completion files a fresh one.
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "looks right",
                Some(crate::model::Verdict::Upheld),
            )
            .unwrap();
        db.tasks().reopen(seq, "cli", "one more thing").unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        let third = db.tasks().complete(seq, "codex:9f2c", "and again").unwrap();
        assert!(third.review.is_some());
    }

    /// Lifting a recusal is a change to the task worth finding in its history,
    /// the same as a dependency or a scope.
    #[test]
    fn recusals_are_written_into_the_task_history() {
        let db = db();
        let work = task(&db, "Port the loader");
        let review = task(&db, "Review it");
        db.recusals().add(review, work, "", "cli").unwrap();

        let task = db.tasks().get(review).unwrap();
        let events = db.tasks().events(&task.id, 20).unwrap();
        let recorded: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == EventKind::Recused)
            .map(|e| e.detail.as_str())
            .collect();
        assert_eq!(recorded, vec![format!("recused from task {work}")]);
    }
}
