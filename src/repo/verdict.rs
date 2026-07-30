//! The verdict: a review that closes its own loop.
//!
//! Recusal (see `recusal.rs`) got the work in front of a second harness. What
//! it did not do is listen to the answer. A review ended in prose — a `result`
//! line saying, somewhere in its own words, whether the work was any good —
//! and then the loop dangled: a human had to read the review, decide that
//! "the error path drops the lock" means *broken*, find the task it reviewed,
//! and reopen it by hand, carrying the findings across themselves. Every
//! other hand-off in hird files itself, on the observation that something an
//! agent — or a human — has to remember to do is something that does not
//! happen. The one hand-off left by hand was the one carrying the judgment.
//!
//! So a review now ends in a **verdict**, and the verdict is enforced where
//! the review ends: completing a task that is a review requires one, and the
//! two possible verdicts name their own consequences. `upheld` means the work
//! stands — its card can say so, on the word of a harness that recusal
//! guarantees did not write it. `sent_back` means it does not stand, and the
//! queue acts immediately: the work is reopened with the reviewer's findings
//! appended to its brief, so the next agent to claim it — its author included
//! — is handed exactly what must change without having to know to ask. The
//! work was filed with `review` set, so finishing it again files a fresh
//! review, and the loop runs, round after round, until a review upholds. No
//! agent is the last word on its own work; no human is the courier between
//! agents.
//!
//! One invariant bends, knowingly. "Terminal statuses only leave via a human
//! reopen" was written when nothing but a human could be trusted to judge
//! finished work. The recusal edge is what changed that: a `sent_back` comes
//! from a harness the queue *proves* did not do the work, which is precisely
//! the trust the human reopen was standing in for. The human keeps the last
//! word they always had — cancel the task, lift the recusal, ignore the
//! round — but they stop being the loop's transport.
//!
//! And because every verdict is delivered on the record — who judged, whose
//! work, which round — the queue accumulates the one measurement it is
//! uniquely placed to take: whose work survives a reading by a different
//! model. `hird record` is that table. It is a report, not a scheduler;
//! nothing routes work by it. Reading it is the human's job, and deciding
//! what to do about a harness that ships rework is exactly the kind of call
//! hird leaves to people.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, Transaction};

use crate::error::{Error, Result};
use crate::identity::actor_harness;
use crate::model::{now_ts, EventKind, HarnessRecord, Status, Verdict, VerdictRecord};

/// One verdict as it landed on a reviewed task, and what the queue did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// The work task judged.
    pub seq: i64,
    pub verdict: Verdict,
    /// True when `sent_back` found the work `done` and returned it to the
    /// pool. False for `upheld`, and for work a human had already moved.
    pub reopened: bool,
}

impl Delivered {
    /// One sentence, aimed at a model that has to relay it to a human.
    pub fn describe(&self) -> String {
        match (self.verdict, self.reopened) {
            (Verdict::Upheld, _) => format!("task {} upheld — the work stands", self.seq),
            (Verdict::SentBack, true) => format!(
                "task {} sent back — open again with your findings appended to its brief",
                self.seq
            ),
            (Verdict::SentBack, false) => format!(
                "task {} sent back, but it had already been moved; the verdict is on record",
                self.seq
            ),
        }
    }
}

/// Repository over `task_verdicts`.
pub struct Verdicts<'a> {
    conn: &'a Connection,
}

impl<'a> Verdicts<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Verdicts<'a> {
        Verdicts { conn }
    }

    /// Every verdict delivered on task `seq`'s work, oldest first.
    ///
    /// More than one means rounds: sent back, redone, judged again.
    pub fn for_task(&self, seq: i64) -> Result<Vec<VerdictRecord>> {
        self.query("WHERE t.seq = ?1 ORDER BY v.at ASC, v.id ASC", params![seq])
    }

    /// The newest verdict on task `seq`'s work, if any has been delivered.
    pub fn latest(&self, seq: i64) -> Result<Option<VerdictRecord>> {
        Ok(self.for_task(seq)?.pop())
    }

    /// The verdicts review `seq` delivered, one per task it reviewed.
    pub fn of_review(&self, seq: i64) -> Result<Vec<VerdictRecord>> {
        self.query("WHERE r.seq = ?1 ORDER BY v.at ASC, v.id ASC", params![seq])
    }

    /// The newest verdict per work task in `scope`, for annotating a board.
    pub fn standing(&self, scope: &super::ProjectScope) -> Result<BTreeMap<i64, Verdict>> {
        let (clause, value) = scope.clause("t.project");
        let sql = format!(
            "SELECT t.seq, v.verdict FROM task_verdicts v
             JOIN tasks t ON t.id = v.task_id
             WHERE {clause} ORDER BY v.at ASC, v.id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (seq, verdict) = row?;
            if let Ok(verdict) = verdict.parse::<Verdict>() {
                // Later rows overwrite earlier ones: the newest verdict stands.
                out.insert(seq, verdict);
            }
        }
        Ok(out)
    }

    /// Each harness's standing in the record: verdicts received on its work,
    /// first-pass rate across distinct tasks, and verdicts it has handed out.
    ///
    /// Aggregated from the delivered verdicts and nothing else, so it holds
    /// still when leases churn and workers hand tasks on: a verdict names the
    /// worker it judged at the moment it landed, and that name does not move.
    pub fn record(&self, scope: &super::ProjectScope) -> Result<Vec<HarnessRecord>> {
        let (clause, value) = scope.clause("t.project");
        let sql = format!(
            "SELECT v.worker, v.reviewer, v.verdict, v.task_id FROM task_verdicts v
             JOIN tasks t ON t.id = v.task_id
             WHERE {clause} ORDER BY v.at ASC, v.id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        fn entry<'m>(
            records: &'m mut BTreeMap<String, HarnessRecord>,
            harness: &str,
        ) -> &'m mut HarnessRecord {
            records
                .entry(harness.to_string())
                .or_insert_with(|| HarnessRecord {
                    harness: harness.to_string(),
                    ..HarnessRecord::default()
                })
        }
        let mut records: BTreeMap<String, HarnessRecord> = BTreeMap::new();
        let mut first_seen: BTreeMap<String, (String, Verdict)> = BTreeMap::new();
        for row in rows {
            let (worker, reviewer, verdict, task_id) = row?;
            let Ok(verdict) = verdict.parse::<Verdict>() else {
                continue;
            };
            if !worker.is_empty() {
                let harness = actor_harness(&worker).to_string();
                let rec = entry(&mut records, &harness);
                rec.judged += 1;
                match verdict {
                    Verdict::Upheld => rec.upheld += 1,
                    Verdict::SentBack => rec.sent_back += 1,
                }
                // The first verdict on a task is the one that measures the
                // work as delivered, before any round of rework.
                first_seen.entry(task_id).or_insert((harness, verdict));
            }
            let rec = entry(&mut records, actor_harness(&reviewer));
            match verdict {
                Verdict::Upheld => rec.upheld_given += 1,
                Verdict::SentBack => rec.sent_back_given += 1,
            }
        }
        for (harness, verdict) in first_seen.into_values() {
            if let Some(rec) = records.get_mut(&harness) {
                rec.tasks_judged += 1;
                if verdict == Verdict::Upheld {
                    rec.first_pass += 1;
                }
            }
        }
        Ok(records.into_values().collect())
    }

    fn query(&self, tail: &str, binds: impl rusqlite::Params) -> Result<Vec<VerdictRecord>> {
        let sql = format!(
            "SELECT r.seq, t.seq, v.verdict, v.worker, v.reviewer, v.at
             FROM task_verdicts v
             JOIN tasks r ON r.id = v.review_id
             JOIN tasks t ON t.id = v.task_id
             {tail}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(binds, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (review_seq, task_seq, verdict, worker, reviewer, at) = row?;
            let Ok(verdict) = verdict.parse::<Verdict>() else {
                continue;
            };
            out.push(VerdictRecord {
                review_seq,
                task_seq,
                verdict,
                worker,
                reviewer,
                at,
            });
        }
        Ok(out)
    }
}

/// Deliver the verdict a completing review carries, or refuse the completion.
///
/// Runs inside the same transaction as the completion itself, so a refusal
/// leaves no half-finished review and a delivered verdict cannot land without
/// its review going `done`. Being a review is a fact about the task — it has
/// recusal edges — not about the caller: a task with edges must carry a
/// verdict, a task without must not, and both refusals say exactly what to do
/// instead, because the agent reading them is mid-completion and has nobody
/// to ask.
pub(super) fn deliver_in_tx(
    tx: &Transaction<'_>,
    review: &crate::model::Task,
    verdict: Option<Verdict>,
    actor: &str,
    result: &str,
) -> Result<Vec<Delivered>> {
    let reviewed = reviewed_tasks(tx, &review.id)?;
    let Some(verdict) = verdict else {
        if reviewed.is_empty() {
            return Ok(Vec::new());
        }
        let seqs = reviewed
            .iter()
            .map(|(seq, ..)| format!("{seq}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::invalid(format!(
            "task {} is a review of task {seqs}: complete it with a verdict — \"upheld\" if \
             the work stands, or \"sent_back\" to return it to the pool carrying your findings",
            review.seq
        )));
    };
    if reviewed.is_empty() {
        return Err(Error::invalid(format!(
            "task {} is not a review of anything; complete it without a verdict",
            review.seq
        )));
    }

    let now = now_ts();
    let mut delivered = Vec::with_capacity(reviewed.len());
    for (work_seq, work_id, work_status) in reviewed {
        // The name on the verdict is whoever held the work when it landed,
        // read from the trail the way recusal reads it — so the record stays
        // true even after the task is reopened and picked up by someone else.
        let worker = super::recusal::worker_of(tx, &work_id)?.unwrap_or_default();
        tx.execute(
            "INSERT INTO task_verdicts (id, review_id, task_id, verdict, worker, reviewer, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                super::new_id(),
                review.id,
                work_id,
                verdict.as_str(),
                worker,
                actor,
                now
            ],
        )?;
        super::tasks::insert_event(
            tx,
            &review.id,
            &now,
            actor,
            EventKind::Reviewed,
            &format!("verdict on task {work_seq}: {verdict}"),
        )?;

        let reopened = match verdict {
            Verdict::Upheld => {
                super::tasks::insert_event(
                    tx,
                    &work_id,
                    &now,
                    actor,
                    EventKind::Reviewed,
                    &format!("upheld by review {}", review.seq),
                )?;
                false
            }
            Verdict::SentBack if work_status == Status::Done => {
                // The findings travel in the brief, not in a comment thread:
                // whoever claims the reopened task is handed them the same way
                // it is handed everything else, without knowing to ask.
                let findings = format!(
                    "\n\n---\n\nSent back by review {} ({}):\n\n{}\n",
                    review.seq,
                    actor_harness(actor),
                    result.trim()
                );
                tx.execute(
                    "UPDATE tasks
                     SET status = 'open', claimed_by = NULL, lease_expires_at = NULL,
                         result = NULL, body = body || ?2, updated_at = ?3
                     WHERE id = ?1",
                    params![work_id, findings, now],
                )?;
                super::tasks::insert_event(
                    tx,
                    &work_id,
                    &now,
                    actor,
                    EventKind::Reopened,
                    &format!(
                        "sent back by review {}: {}",
                        review.seq,
                        crate::fmt::truncate(result.trim(), 120)
                    ),
                )?;
                // The work's dependents were let through on the strength of a
                // `done` that has just been taken back. Any of them being
                // worked right now is building on ground that moved, and its
                // holder is the one participant with no way to notice — so
                // the fallout goes on each of their trails here, in the same
                // transaction as the reopen, and their next check-in relays
                // it (`Deps::shifted`).
                notify_live_dependents(tx, &work_id, work_seq, review.seq, actor, &now)?;
                true
            }
            Verdict::SentBack => {
                // A human already moved the work — reopened it themselves,
                // cancelled it, or it failed since. The verdict still lands on
                // the record; the queue does not overrule a human's move.
                super::tasks::insert_event(
                    tx,
                    &work_id,
                    &now,
                    actor,
                    EventKind::Reviewed,
                    &format!(
                        "sent back by review {}, but the work is {work_status}; left as it is",
                        review.seq
                    ),
                )?;
                false
            }
        };
        delivered.push(Delivered {
            seq: work_seq,
            verdict,
            reopened,
        });
    }
    Ok(delivered)
}

/// Put the sent-back on the trail of every live dependent of the reopened
/// work, so the shift is on the record the moment it happens rather than
/// whenever somebody thinks to look.
fn notify_live_dependents(
    tx: &Transaction<'_>,
    work_id: &str,
    work_seq: i64,
    review_seq: i64,
    actor: &str,
    now: &str,
) -> Result<()> {
    let dependents: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT t.id FROM task_deps d
             JOIN tasks t ON t.id = d.task_id
             WHERE d.depends_on_id = ?1 AND t.status IN ('claimed','in_progress')
             ORDER BY t.seq",
        )?;
        let rows = stmt.query_map([work_id], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for dependent in dependents {
        super::tasks::insert_event(
            tx,
            &dependent,
            now,
            actor,
            EventKind::GroundShifted,
            &format!(
                "task {work_seq}, which this task builds on, was sent back by \
                 review {review_seq} and reopened"
            ),
        )?;
    }
    Ok(())
}

/// The tasks a review judges: everything it is recused from.
fn reviewed_tasks(conn: &Connection, review_id: &str) -> Result<Vec<(i64, String, Status)>> {
    let mut stmt = conn.prepare(
        "SELECT t.seq, t.id, t.status FROM task_recusals r
         JOIN tasks t ON t.id = r.from_task_id
         WHERE r.task_id = ?1 ORDER BY t.seq",
    )?;
    let rows = stmt.query_map([review_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, id, status) = row?;
        let status = status
            .parse::<Status>()
            .map_err(|e| Error::invalid(e.to_string()))?;
        out.push((seq, id, status));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::ProjectScope;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// File work marked for review, complete it as `worker`, and return
    /// `(work, review)` seqs.
    fn reviewed_work(db: &Db, worker: &str) -> (i64, i64) {
        let seq = db
            .tasks()
            .create(PROJECT, "Port the loader", "keep the precedence", 0, "cli")
            .unwrap()
            .seq;
        db.tasks().set_review(seq, true, "cli").unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/loader.rs".to_string()],
                "cli",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(seq, worker, TTL).unwrap();
        let finished = db.tasks().complete(seq, worker, "ported it").unwrap();
        (seq, finished.review.expect("a review was filed"))
    }

    #[test]
    fn completing_a_review_without_a_verdict_is_refused_and_the_refusal_teaches() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();

        let err = db
            .tasks()
            .complete(review, "claude-code:af31", "looks wrong")
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("review of task {work}")), "{err}");
        assert!(err.contains("upheld"), "{err}");
        assert!(err.contains("sent_back"), "{err}");
        // The refusal left the review unfinished, holdable and completable.
        assert_eq!(
            db.tasks().get(review).unwrap().status,
            crate::model::Status::Claimed
        );
    }

    #[test]
    fn a_verdict_on_a_task_that_is_not_a_review_is_refused() {
        let db = db();
        let seq = db.tasks().create(PROJECT, "t", "", 0, "cli").unwrap().seq;
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        let err = db
            .tasks()
            .complete_with(seq, "codex:9f2c", "done", Some(Verdict::Upheld))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a review"), "{err}");
    }

    #[test]
    fn upheld_marks_the_work_without_moving_it() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        let finished = db
            .tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "read it, it holds",
                Some(Verdict::Upheld),
            )
            .unwrap();

        assert_eq!(finished.verdicts.len(), 1);
        assert_eq!(finished.verdicts[0].verdict, Verdict::Upheld);
        assert!(!finished.verdicts[0].reopened);
        assert_eq!(
            db.tasks().get(work).unwrap().status,
            crate::model::Status::Done
        );

        let latest = db.verdicts().latest(work).unwrap().unwrap();
        assert_eq!(latest.review_seq, review);
        assert_eq!(latest.worker, "codex:9f2c");
        assert_eq!(latest.reviewer, "claude-code:af31");
        assert!(latest.describe().contains("upheld by claude-code"));

        let standing = db
            .verdicts()
            .standing(&ProjectScope::Only(PROJECT.into()))
            .unwrap();
        assert_eq!(standing.get(&work), Some(&Verdict::Upheld));
    }

    #[test]
    fn sent_back_reopens_the_work_carrying_the_findings() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        let finished = db
            .tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "the error path drops the lock; re-take it before returning",
                Some(Verdict::SentBack),
            )
            .unwrap();
        assert!(finished.verdicts[0].reopened);

        let reopened = db.tasks().get(work).unwrap();
        assert_eq!(reopened.status, crate::model::Status::Open);
        assert_eq!(reopened.result, None);
        // The original brief survives, and the findings arrive appended, named.
        assert!(
            reopened.body.contains("keep the precedence"),
            "{}",
            reopened.body
        );
        assert!(
            reopened
                .body
                .contains(&format!("Sent back by review {review} (claude-code)")),
            "{}",
            reopened.body
        );
        assert!(
            reopened.body.contains("drops the lock"),
            "{}",
            reopened.body
        );

        // The author may take their own work back up — the bar was on the
        // review, never on the fix.
        db.tasks().claim(work, "codex:9f2c", TTL).unwrap();
    }

    /// The reopen's fallout: an agent mid-task on top of the sent-back work
    /// gets the shift on its trail in the same transaction, and an agent whose
    /// task is merely open gets nothing — readiness re-derives for it anyway.
    #[test]
    fn sending_work_back_lands_on_the_trail_of_whoever_builds_on_it() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        let live = db
            .tasks()
            .create(PROJECT, "use the loader", "", 0, "cli")
            .unwrap()
            .seq;
        let idle = db
            .tasks()
            .create(PROJECT, "document the loader", "", 0, "cli")
            .unwrap()
            .seq;
        db.deps().add(live, work, "cli").unwrap();
        db.deps().add(idle, work, "cli").unwrap();
        db.tasks().claim(live, "copilot:11", TTL).unwrap();

        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "the error path drops the lock",
                Some(Verdict::SentBack),
            )
            .unwrap();

        let events_of = |seq: i64| {
            let id = db.tasks().get(seq).unwrap().id;
            db.tasks().events(&id, 40).unwrap()
        };
        let shifted: Vec<_> = events_of(live)
            .into_iter()
            .filter(|e| e.kind == crate::model::EventKind::GroundShifted)
            .collect();
        assert_eq!(shifted.len(), 1);
        assert!(
            shifted[0].detail.contains(&format!("task {work}")),
            "{shifted:?}"
        );
        assert!(
            shifted[0].detail.contains(&format!("review {review}")),
            "{shifted:?}"
        );
        assert!(
            !events_of(idle)
                .iter()
                .any(|e| e.kind == crate::model::EventKind::GroundShifted),
            "an open dependent is re-gated by readiness; writing to its trail would be noise"
        );
    }

    /// The whole loop, unattended: sent back, redone, re-reviewed, upheld —
    /// with the human nowhere in the transport.
    #[test]
    fn the_loop_runs_to_an_upheld_verdict_without_a_human() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "misses the empty case",
                Some(Verdict::SentBack),
            )
            .unwrap();

        // Round two: the author fixes their own work; finishing files a fresh
        // review, because the first one is finished business.
        db.tasks().claim(work, "codex:9f2c", TTL).unwrap();
        let redone = db
            .tasks()
            .complete(work, "codex:9f2c", "empty case handled")
            .unwrap();
        let second = redone.review.expect("a fresh review for the redo");
        assert_ne!(second, review);

        db.tasks().claim(second, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(
                second,
                "claude-code:af31",
                "holds now",
                Some(Verdict::Upheld),
            )
            .unwrap();

        assert_eq!(
            db.tasks().get(work).unwrap().status,
            crate::model::Status::Done
        );
        let rounds = db.verdicts().for_task(work).unwrap();
        assert_eq!(rounds.len(), 2, "one verdict per round");
        assert_eq!(rounds[0].verdict, Verdict::SentBack);
        assert_eq!(rounds[1].verdict, Verdict::Upheld);
        assert_eq!(
            db.verdicts()
                .standing(&ProjectScope::Only(PROJECT.into()))
                .unwrap()
                .get(&work),
            Some(&Verdict::Upheld)
        );
    }

    /// The queue does not overrule a human's move: work already reopened (or
    /// cancelled) when the verdict lands stays exactly where the human put it.
    #[test]
    fn sent_back_does_not_overrule_a_human_who_already_moved_the_work() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks()
            .reopen(work, "cli", "redoing this myself")
            .unwrap();
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        let finished = db
            .tasks()
            .complete_with(
                review,
                "claude-code:af31",
                "broken anyway",
                Some(Verdict::SentBack),
            )
            .unwrap();

        assert!(!finished.verdicts[0].reopened);
        let work_task = db.tasks().get(work).unwrap();
        assert_eq!(work_task.status, crate::model::Status::Open);
        assert!(
            !work_task.body.contains("Sent back"),
            "no findings are appended when nothing was reopened: {}",
            work_task.body
        );
        // But the verdict is on the record, and the trail says what happened.
        assert_eq!(db.verdicts().for_task(work).unwrap().len(), 1);
    }

    #[test]
    fn the_record_tallies_both_sides_of_every_verdict() {
        let db = db();
        // codex ships one task that is sent back once and then upheld…
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(review, "claude-code:af31", "no", Some(Verdict::SentBack))
            .unwrap();
        db.tasks().claim(work, "codex:9f2c", TTL).unwrap();
        let second = db
            .tasks()
            .complete(work, "codex:9f2c", "fixed")
            .unwrap()
            .review
            .unwrap();
        db.tasks().claim(second, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .complete_with(second, "claude-code:af31", "yes", Some(Verdict::Upheld))
            .unwrap();
        // …and claude-code ships one that is upheld first pass, by codex.
        let (_, other_review) = reviewed_work(&db, "claude-code:af31");
        db.tasks().claim(other_review, "codex:9f2c", TTL).unwrap();
        db.tasks()
            .complete_with(other_review, "codex:9f2c", "clean", Some(Verdict::Upheld))
            .unwrap();

        let record = db
            .verdicts()
            .record(&ProjectScope::Only(PROJECT.into()))
            .unwrap();
        let of = |harness: &str| record.iter().find(|r| r.harness == harness).unwrap();

        let codex = of("codex");
        assert_eq!(codex.judged, 2);
        assert_eq!(codex.upheld, 1);
        assert_eq!(codex.sent_back, 1);
        assert_eq!(codex.tasks_judged, 1);
        assert_eq!(codex.first_pass, 0, "sent back on the first round");
        assert_eq!(codex.upheld_given, 1);
        assert_eq!(codex.sent_back_given, 0);

        let claude = of("claude-code");
        assert_eq!(claude.judged, 1);
        assert_eq!(claude.tasks_judged, 1);
        assert_eq!(claude.first_pass, 1);
        assert_eq!(claude.upheld_given, 1);
        assert_eq!(claude.sent_back_given, 1);
    }

    /// Failing a review is not delivering a verdict: the reviewer could not do
    /// the reading, and the work stays exactly as it was.
    #[test]
    fn failing_a_review_delivers_nothing() {
        let db = db();
        let (work, review) = reviewed_work(&db, "codex:9f2c");
        db.tasks().claim(review, "claude-code:af31", TTL).unwrap();
        db.tasks()
            .fail(review, "claude-code:af31", "cannot build this branch")
            .unwrap();
        assert!(db.verdicts().for_task(work).unwrap().is_empty());
        assert_eq!(
            db.tasks().get(work).unwrap().status,
            crate::model::Status::Done
        );
    }
}
