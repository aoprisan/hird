//! The project-wide feed: the append-only trail read across tasks.
//!
//! Every mutation in hird already lands one row in `task_events`; this module
//! is the first reader to look at that trail sideways — across tasks, in the
//! order things happened — instead of replaying one task's history. It is
//! what `hird events` prints and what `--follow` resumes from.

use std::collections::BTreeMap;

use rusqlite::{params_from_iter, Connection};

use super::ProjectScope;
use crate::error::Result;
use crate::model::{EventKind, Status};

/// One entry in the feed, joined to the task it happened on.
#[derive(Debug, Clone)]
pub struct FeedEvent {
    /// Insertion-order cursor. A follower resumes with everything after the
    /// last cursor it saw; ULID ids sort by creation time, but two events in
    /// one transaction share a millisecond, and the rowid never ties.
    pub cursor: i64,
    pub at: String,
    pub actor: String,
    pub kind: EventKind,
    pub detail: String,
    pub task_seq: i64,
    pub task_title: String,
    pub project: String,
}

/// One task's standing at a past moment, folded from the trail.
///
/// Only what the events can testify to: status, holder and the question
/// gate. The title and project ride along from the present-day task row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedTask {
    pub seq: i64,
    pub title: String,
    pub project: String,
    pub status: Status,
    pub holder: Option<String>,
    /// Whether an unanswered question had parked it at that moment.
    pub awaiting_answer: bool,
}

impl ReplayedTask {
    /// Every way a holding ends clears the holder, whatever the status.
    fn finish(&mut self, status: Status) {
        self.status = status;
        self.holder = None;
    }
}

/// What a reader wants to see of the trail. Empty means everything.
#[derive(Debug, Default, Clone)]
pub struct FeedFilter {
    /// Only these kinds. Empty keeps every kind.
    pub kinds: Vec<EventKind>,
    /// Only events on this task.
    pub task_seq: Option<i64>,
    /// Only events by this actor, exactly as recorded.
    pub actor: Option<String>,
}

/// The event trail, read across tasks.
pub struct Events<'a> {
    conn: &'a Connection,
}

impl<'a> Events<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Events<'a> {
        Events { conn }
    }

    /// The most recent `limit` matching events, oldest first.
    pub fn tail(
        &self,
        scope: &ProjectScope,
        filter: &FeedFilter,
        limit: usize,
    ) -> Result<Vec<FeedEvent>> {
        let (clauses, binds) = where_parts(scope, filter);
        let sql = format!(
            "SELECT * FROM (
               SELECT e.rowid, e.at, e.actor, e.kind, e.detail, t.seq, t.title, t.project
               FROM task_events e JOIN tasks t ON t.id = e.task_id
               WHERE {clauses}
               ORDER BY e.rowid DESC LIMIT {limit})
             ORDER BY 1 ASC",
            limit = limit as i64
        );
        self.query(&sql, &binds)
    }

    /// Every matching event after `cursor`, oldest first.
    pub fn since(
        &self,
        scope: &ProjectScope,
        filter: &FeedFilter,
        cursor: i64,
    ) -> Result<Vec<FeedEvent>> {
        let (clauses, binds) = where_parts(scope, filter);
        let sql = format!(
            "SELECT e.rowid, e.at, e.actor, e.kind, e.detail, t.seq, t.title, t.project
             FROM task_events e JOIN tasks t ON t.id = e.task_id
             WHERE {clauses} AND e.rowid > {cursor}
             ORDER BY e.rowid ASC"
        );
        self.query(&sql, &binds)
    }

    /// The board as it stood at `cutoff`, folded from the trail.
    ///
    /// The trail is append-only precisely so this question stays answerable:
    /// every status a task has ever held got there through an event, so
    /// replaying the events up to a moment recovers who held what and which
    /// wave was live, without the board keeping any state it does not already
    /// keep. Titles and projects are read from the present — they are not
    /// versioned in the trail — so a replay shows yesterday's statuses under
    /// today's names.
    ///
    /// `cutoff` is compared as a string, which is exact: timestamps are
    /// stored fixed-width so lexicographic and chronological order agree, and
    /// a prefix like `2026-08-14T10:00` reads as the start of that instant.
    /// Ties inside the same millisecond fold in insertion order, the same
    /// order the writes committed.
    pub fn board_at(&self, scope: &ProjectScope, cutoff: &str) -> Result<Vec<ReplayedTask>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let sql = format!(
            "SELECT e.kind, e.actor, t.seq, t.title, t.project
             FROM task_events e JOIN tasks t ON t.id = e.task_id
             WHERE {project_clause} AND e.at <= ?
             ORDER BY e.rowid ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut binds: Vec<&str> = project_value.into_iter().collect();
        binds.push(cutoff);
        let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut board: BTreeMap<i64, ReplayedTask> = BTreeMap::new();
        for row in rows {
            let (kind, actor, seq, title, project) = row?;
            let Ok(kind) = kind.parse::<EventKind>() else {
                continue;
            };
            let task = board.entry(seq).or_insert_with(|| ReplayedTask {
                seq,
                title,
                project,
                status: Status::Open,
                holder: None,
                awaiting_answer: false,
            });
            match kind {
                EventKind::Created => {}
                EventKind::Claimed => {
                    task.status = Status::Claimed;
                    task.holder = Some(actor);
                }
                // The one kind that spells its transition out; `claimed ->
                // in_progress` is the only one a claim-holding fold needs,
                // but reading the detail would couple this to prose. The
                // target status is recoverable from the words that exist.
                EventKind::Status => {
                    if task.status == Status::Claimed {
                        task.status = Status::InProgress;
                    }
                }
                EventKind::Completed => task.finish(Status::Done),
                EventKind::Failed => task.finish(Status::Failed),
                EventKind::Cancelled => task.finish(Status::Cancelled),
                EventKind::Reopened => task.finish(Status::Open),
                EventKind::Released | EventKind::LeaseExpired => task.finish(Status::Open),
                EventKind::Asked => task.awaiting_answer = true,
                EventKind::Answered => task.awaiting_answer = false,
                _ => {}
            }
        }
        Ok(board.into_values().collect())
    }

    /// Every harness that has ever acted on this board, in name order.
    ///
    /// Read from the actors the trail records: agents act as
    /// `harness:session`, while human surfaces (`cli`, `tui`) carry no colon
    /// and are not harnesses. This is as close to a roster as hird gets — it
    /// keeps none, by design — so the answer is who has shown up, not who
    /// could.
    pub fn harnesses_seen(&self, scope: &ProjectScope) -> Result<Vec<String>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let sql = format!(
            "SELECT DISTINCT e.actor
             FROM task_events e JOIN tasks t ON t.id = e.task_id
             WHERE {project_clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            row.get::<_, String>(0)
        })?;
        let mut harnesses: Vec<String> = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|actor| actor.contains(':'))
            .map(|actor| crate::identity::actor_harness(&actor).to_string())
            .collect();
        harnesses.sort();
        harnesses.dedup();
        Ok(harnesses)
    }

    fn query(&self, sql: &str, binds: &[&str]) -> Result<Vec<FeedEvent>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
            Ok(FeedEvent {
                cursor: row.get(0)?,
                at: row.get(1)?,
                actor: row.get(2)?,
                kind: row.get::<_, String>(3)?.parse().unwrap_or(EventKind::Note),
                detail: row.get(4)?,
                task_seq: row.get(5)?,
                task_title: row.get(6)?,
                project: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// The shared `WHERE` clause and its bound values.
///
/// Kind names come from [`EventKind::as_str`] and numbers are integers, so
/// both are formatted in directly; the project and actor are caller strings
/// and stay bound.
fn where_parts<'f>(scope: &'f ProjectScope, filter: &'f FeedFilter) -> (String, Vec<&'f str>) {
    let (project_clause, project_value) = scope.clause("t.project");
    let mut clauses = vec![project_clause];
    let mut binds: Vec<&str> = project_value.into_iter().collect();
    if !filter.kinds.is_empty() {
        let kinds: Vec<String> = filter
            .kinds
            .iter()
            .map(|k| format!("'{}'", k.as_str()))
            .collect();
        clauses.push(format!("e.kind IN ({})", kinds.join(", ")));
    }
    if let Some(seq) = filter.task_seq {
        clauses.push(format!("t.seq = {seq}"));
    }
    if let Some(actor) = &filter.actor {
        clauses.push("e.actor = ?".to_string());
        binds.push(actor.as_str());
    }
    (clauses.join(" AND "), binds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::time::Duration;

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

    fn scope() -> ProjectScope {
        ProjectScope::Only(PROJECT.into())
    }

    #[test]
    fn the_feed_spans_tasks_oldest_first_with_climbing_cursors() {
        let db = db();
        seed(&db, "first");
        seed(&db, "second");
        db.tasks().claim(1, "codex:1", TTL).unwrap();

        let feed = db
            .events()
            .tail(&scope(), &FeedFilter::default(), 50)
            .unwrap();
        let kinds: Vec<EventKind> = feed.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![EventKind::Created, EventKind::Created, EventKind::Claimed]
        );
        assert!(feed.windows(2).all(|w| w[0].cursor < w[1].cursor));
        let claimed = feed.last().unwrap();
        assert_eq!(claimed.task_seq, 1);
        assert_eq!(claimed.task_title, "first");
        assert_eq!(claimed.actor, "codex:1");
        assert_eq!(claimed.project, PROJECT);
    }

    #[test]
    fn tail_keeps_the_most_recent_events_when_it_clips() {
        let db = db();
        seed(&db, "first");
        seed(&db, "second");
        seed(&db, "third");

        let feed = db
            .events()
            .tail(&scope(), &FeedFilter::default(), 2)
            .unwrap();
        let titles: Vec<&str> = feed.iter().map(|e| e.task_title.as_str()).collect();
        assert_eq!(titles, vec!["second", "third"]);
    }

    #[test]
    fn a_cursor_resumes_where_the_reader_left_off() {
        let db = db();
        seed(&db, "watched");
        let feed = db
            .events()
            .tail(&scope(), &FeedFilter::default(), 50)
            .unwrap();
        let cursor = feed.last().unwrap().cursor;

        assert!(db
            .events()
            .since(&scope(), &FeedFilter::default(), cursor)
            .unwrap()
            .is_empty());

        db.tasks().cancel(1, "cli", "not needed").unwrap();
        let fresh = db
            .events()
            .since(&scope(), &FeedFilter::default(), cursor)
            .unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].kind, EventKind::Cancelled);
        assert!(fresh[0].cursor > cursor);
    }

    #[test]
    fn the_feed_filters_by_kind_task_and_actor() {
        let db = db();
        seed(&db, "one");
        seed(&db, "two");
        db.tasks().claim(1, "codex:1", TTL).unwrap();
        db.tasks().claim(2, "claude:2", TTL).unwrap();

        let by_kind = FeedFilter {
            kinds: vec![EventKind::Claimed],
            ..Default::default()
        };
        let feed = db.events().tail(&scope(), &by_kind, 50).unwrap();
        assert_eq!(feed.len(), 2);
        assert!(feed.iter().all(|e| e.kind == EventKind::Claimed));

        let by_task = FeedFilter {
            task_seq: Some(2),
            ..Default::default()
        };
        let feed = db.events().tail(&scope(), &by_task, 50).unwrap();
        assert!(!feed.is_empty());
        assert!(feed.iter().all(|e| e.task_seq == 2));

        let by_actor = FeedFilter {
            actor: Some("codex:1".into()),
            ..Default::default()
        };
        let feed = db.events().tail(&scope(), &by_actor, 50).unwrap();
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].kind, EventKind::Claimed);
        assert_eq!(feed[0].task_seq, 1);
    }

    /// Between two moments the same trail answers differently: the fold
    /// recovers each status a task passed through, and who held it there.
    #[test]
    fn the_board_can_be_replayed_at_any_moment_in_the_trail() {
        let db = db();
        let seq = seed(&db, "port the loader");
        let filed_at = crate::model::now_ts();
        // Real timestamps carry the ordering, so give the clock room to tick
        // past the cutoff before the next write.
        std::thread::sleep(Duration::from_millis(5));

        db.tasks().claim(seq, "codex:1", TTL).unwrap();
        let claimed_at = crate::model::now_ts();
        std::thread::sleep(Duration::from_millis(5));

        db.tasks().complete(seq, "codex:1", "ported").unwrap();
        let done_at = crate::model::now_ts();

        let at = |cutoff: &str| {
            let board = db.events().board_at(&scope(), cutoff).unwrap();
            board.into_iter().find(|t| t.seq == seq).unwrap()
        };
        let filed = at(&filed_at);
        assert_eq!(filed.status, Status::Open);
        assert_eq!(filed.holder, None);

        let held = at(&claimed_at);
        assert_eq!(held.status, Status::Claimed);
        assert_eq!(held.holder.as_deref(), Some("codex:1"));

        let done = at(&done_at);
        assert_eq!(done.status, Status::Done);
        assert_eq!(done.holder, None, "finishing clears the holder");

        assert!(
            db.events()
                .board_at(&scope(), "2000-01-01")
                .unwrap()
                .is_empty(),
            "before the first event there is no board"
        );
    }

    #[test]
    fn a_replay_knows_which_tasks_were_parked_on_a_question() {
        let db = db();
        let seq = seed(&db, "choose");
        db.tasks().claim(seq, "codex:1", TTL).unwrap();
        db.tasks()
            .release_asking(seq, "codex:1", "needs a call", "Which way?")
            .unwrap();
        let asked_at = crate::model::now_ts();

        let board = db.events().board_at(&scope(), &asked_at).unwrap();
        assert_eq!(board[0].status, Status::Open);
        assert!(board[0].awaiting_answer);

        std::thread::sleep(Duration::from_millis(5));
        db.questions().answer(seq, "cli", "left").unwrap();
        let board = db
            .events()
            .board_at(&scope(), &crate::model::now_ts())
            .unwrap();
        assert!(!board[0].awaiting_answer);
    }

    #[test]
    fn the_trail_knows_which_harnesses_have_acted_here() {
        let db = db();
        let seq = seed(&db, "shared");
        assert!(
            db.events().harnesses_seen(&scope()).unwrap().is_empty(),
            "the CLI is not a harness"
        );
        db.tasks().claim(seq, "codex:1", TTL).unwrap();
        db.tasks().release(seq, "codex:1", "handing off").unwrap();
        db.tasks().claim(seq, "claude-code:2", TTL).unwrap();
        assert_eq!(
            db.events().harnesses_seen(&scope()).unwrap(),
            vec!["claude-code", "codex"]
        );
    }

    #[test]
    fn the_feed_scopes_to_the_project_unless_asked_for_everything() {
        let db = db();
        seed(&db, "here");
        db.tasks()
            .create("/tmp/elsewhere", "there", "", 0, "cli")
            .unwrap();

        let near = db
            .events()
            .tail(&scope(), &FeedFilter::default(), 50)
            .unwrap();
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].task_title, "here");

        let all = db
            .events()
            .tail(&ProjectScope::All, &FeedFilter::default(), 50)
            .unwrap();
        assert_eq!(all.len(), 2);
    }
}
