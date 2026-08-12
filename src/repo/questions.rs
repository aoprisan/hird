//! Questions that park work until a human or external actor answers.
//!
//! A question is deliberately not a task status. The holder releases the task
//! to `open` and this table supplies a derived readiness gate, just as
//! unfinished dependencies do. The status machine stays small while the queue
//! stops handing the same unanswerable work from agent to agent.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::{immediate_tx, new_id, tasks::insert_event, ProjectScope};
use crate::error::{Error, Result};
use crate::model::{now_ts, EventKind, Question, Status};

/// Typed access to a task's question and answer history.
pub struct Questions<'a> {
    conn: &'a Connection,
}

impl<'a> Questions<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Questions<'a> {
        Questions { conn }
    }

    /// Every question on a task, oldest first, including answered ones.
    pub fn for_task(&self, seq: i64) -> Result<Vec<Question>> {
        let task_id = task_id_for_seq(self.conn, seq)?;
        history_for_id(self.conn, &task_id)
    }

    /// The one unresolved question on a task, when it is waiting for input.
    pub fn unanswered(&self, seq: i64) -> Result<Option<Question>> {
        let task_id = task_id_for_seq(self.conn, seq)?;
        unanswered_in(self.conn, &task_id)
    }

    /// Every task currently waiting for an answer in this project scope.
    pub fn unanswered_map(&self, scope: &ProjectScope) -> Result<BTreeMap<i64, Question>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let sql = format!(
            "SELECT q.id, q.task_id, q.n, q.asked_by, q.question, q.asked_at,
                    q.answer, q.answered_by, q.answered_at, t.seq
             FROM task_questions q
             JOIN tasks t ON t.id = q.task_id
             WHERE {project_clause} AND t.status = 'open' AND q.answer IS NULL
             ORDER BY t.seq"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            Ok((row.get(9)?, row_to_question(row)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }

    /// Resolve the current question through the human path.
    ///
    /// The answer and its event land in one transaction. A concurrent second
    /// answer therefore sees no open question and is refused rather than
    /// silently replacing the first human decision.
    pub fn answer(&self, seq: i64, actor: &str, answer: &str) -> Result<Question> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(Error::invalid(
                "answer must not be empty; say what the next agent should proceed with",
            ));
        }
        let now = now_ts();
        let tx = immediate_tx(self.conn)?;
        let (task_id, status): (String, String) = tx
            .query_row(
                "SELECT id, status FROM tasks WHERE seq = ?1",
                [seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(Error::TaskNotFound { seq })?;
        let status: Status = status
            .parse()
            .map_err(|_| Error::invalid(format!("task {seq} has an invalid status")))?;
        if status != Status::Open {
            return Err(Error::invalid(format!(
                "cannot answer task {seq}: it is {status}, not open"
            )));
        }
        let question = unanswered_in(&tx, &task_id)?
            .ok_or_else(|| Error::invalid(format!("task {seq} is not awaiting an answer")))?;
        let changed = tx.execute(
            "UPDATE task_questions
             SET answer = ?1, answered_by = ?2, answered_at = ?3
             WHERE id = ?4 AND answer IS NULL",
            params![answer, actor, now, question.id],
        )?;
        if changed == 0 {
            return Err(Error::invalid(format!(
                "task {seq}'s question was already answered"
            )));
        }
        tx.execute(
            "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;
        insert_event(&tx, &task_id, &now, actor, EventKind::Answered, answer)?;
        let answered = question_by_id(&tx, &question.id)?.expect("answered row exists");
        tx.commit()?;
        Ok(answered)
    }
}

/// Insert a question inside the same transaction that releases its holder.
pub(crate) fn ask_in_tx(
    tx: &Transaction<'_>,
    task_id: &str,
    actor: &str,
    question: &str,
    now: &str,
) -> Result<Question> {
    let question = question.trim();
    if question.is_empty() {
        return Err(Error::invalid(
            "question must not be empty; say what answer the task needs",
        ));
    }
    if unanswered_in(tx, task_id)?.is_some() {
        return Err(Error::invalid(
            "this task is already awaiting an answer; answer it before asking another question",
        ));
    }
    let n: i64 = tx.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM task_questions WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    let id = new_id();
    tx.execute(
        "INSERT INTO task_questions
         (id, task_id, n, asked_by, question, asked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, task_id, n, actor, question, now],
    )?;
    insert_event(tx, task_id, now, actor, EventKind::Asked, question)?;
    Ok(question_by_id(tx, &id)?.expect("question inserted"))
}

pub(crate) fn unanswered_in(conn: &Connection, task_id: &str) -> Result<Option<Question>> {
    conn.query_row(
        "SELECT id, task_id, n, asked_by, question, asked_at,
                answer, answered_by, answered_at
         FROM task_questions WHERE task_id = ?1 AND answer IS NULL",
        [task_id],
        row_to_question,
    )
    .optional()
    .map_err(Error::from)
}

pub(crate) fn history_for_id(conn: &Connection, task_id: &str) -> Result<Vec<Question>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, n, asked_by, question, asked_at,
                answer, answered_by, answered_at
         FROM task_questions WHERE task_id = ?1 ORDER BY n",
    )?;
    let rows = stmt.query_map([task_id], row_to_question)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn question_by_id(conn: &Connection, id: &str) -> Result<Option<Question>> {
    conn.query_row(
        "SELECT id, task_id, n, asked_by, question, asked_at,
                answer, answered_by, answered_at
         FROM task_questions WHERE id = ?1",
        [id],
        row_to_question,
    )
    .optional()
    .map_err(Error::from)
}

fn row_to_question(row: &rusqlite::Row<'_>) -> rusqlite::Result<Question> {
    Ok(Question {
        id: row.get(0)?,
        task_id: row.get(1)?,
        n: row.get(2)?,
        asked_by: row.get(3)?,
        question: row.get(4)?,
        asked_at: row.get(5)?,
        answer: row.get(6)?,
        answered_by: row.get(7)?,
        answered_at: row.get(8)?,
    })
}

fn task_id_for_seq(conn: &Connection, seq: i64) -> Result<String> {
    conn.query_row("SELECT id FROM tasks WHERE seq = ?1", [seq], |row| {
        row.get(0)
    })
    .optional()?
    .ok_or(Error::TaskNotFound { seq })
}

#[cfg(test)]
mod tests {
    use crate::db::Db;
    use std::time::Duration;

    #[test]
    fn one_answer_wins_and_history_remains() {
        let db = Db::open_in_memory().unwrap();
        let seq = db.tasks().create("/p", "choose", "", 0, "cli").unwrap().seq;
        db.tasks()
            .claim(seq, "codex:1", Duration::from_secs(900))
            .unwrap();
        db.tasks()
            .release_asking(seq, "codex:1", "need policy", "Keep compatibility?")
            .unwrap();

        let open = db.questions().unanswered(seq).unwrap().unwrap();
        assert_eq!(open.question, "Keep compatibility?");
        let answered = db.questions().answer(seq, "cli", "Yes").unwrap();
        assert_eq!(answered.answer.as_deref(), Some("Yes"));
        assert!(db.questions().unanswered(seq).unwrap().is_none());
        assert_eq!(db.questions().for_task(seq).unwrap(), vec![answered]);
        assert_eq!(
            db.questions()
                .answer(seq, "cli", "No")
                .unwrap_err()
                .to_string(),
            format!("task {seq} is not awaiting an answer")
        );
    }
}
