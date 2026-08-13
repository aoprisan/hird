//! Task capability requirements: what a worker must bring before claiming.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::capability;
use crate::error::{Error, Result};
use crate::model::{now_ts, EventKind};

/// Repository over `task_requirements`.
pub struct Requirements<'a> {
    conn: &'a Connection,
}

impl<'a> Requirements<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Requirements<'a> {
        Requirements { conn }
    }

    /// Replace a task's requirements with `capabilities`.
    pub fn set(&self, seq: i64, capabilities: &[String], actor: &str) -> Result<Vec<String>> {
        let normalized = capability::normalize_all(capabilities)?;
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let id = id_for_seq(&tx, seq)?;
        set_in_tx(&tx, &id, &normalized, actor, &now_ts())?;
        tx.commit()?;
        Ok(normalized)
    }

    /// Requirements on one task, sorted for stable output.
    pub fn for_task(&self, seq: i64) -> Result<Vec<String>> {
        let id = id_for_seq(self.conn, seq)?;
        for_id(self.conn, &id)
    }
}

pub(crate) fn set_in_tx(
    tx: &Transaction<'_>,
    task_id: &str,
    capabilities: &[String],
    actor: &str,
    now: &str,
) -> Result<()> {
    let current = for_id(tx, task_id)?;
    if current == capabilities {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM task_requirements WHERE task_id = ?1",
        [task_id],
    )?;
    for capability in capabilities {
        tx.execute(
            "INSERT INTO task_requirements (task_id, capability) VALUES (?1, ?2)",
            params![task_id, capability],
        )?;
    }
    let detail = if capabilities.is_empty() {
        "cleared capability requirements".to_string()
    } else {
        format!("requires {}", capabilities.join(", "))
    };
    super::tasks::insert_event(tx, task_id, now, actor, EventKind::Required, &detail)?;
    Ok(())
}

pub(crate) fn for_id(conn: &Connection, task_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT capability FROM task_requirements WHERE task_id = ?1 ORDER BY capability",
    )?;
    let rows = stmt.query_map([task_id], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn missing_for(
    conn: &Connection,
    task_id: &str,
    available: &[String],
) -> Result<Vec<String>> {
    Ok(capability::missing(&for_id(conn, task_id)?, available))
}

fn id_for_seq(conn: &Connection, seq: i64) -> Result<String> {
    conn.query_row("SELECT id FROM tasks WHERE seq = ?1", [seq], |row| {
        row.get(0)
    })
    .optional()?
    .ok_or(Error::TaskNotFound { seq })
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    #[test]
    fn requirements_are_normalized_and_replaceable() {
        let db = Db::open_in_memory().unwrap();
        let seq = db.tasks().create("/tmp/p", "t", "", 0, "cli").unwrap().seq;
        let required = db
            .requirements()
            .set(seq, &[" Browser ".into(), "network".into()], "cli")
            .unwrap();
        assert_eq!(required, vec!["browser", "network"]);
        assert_eq!(db.requirements().for_task(seq).unwrap(), required);

        db.requirements().set(seq, &[], "cli").unwrap();
        assert!(db.requirements().for_task(seq).unwrap().is_empty());
    }
}
