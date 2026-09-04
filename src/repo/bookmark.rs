//! Bookmarks: where a human reader last looked.
//!
//! The event trail is append-only and the board is live, which between them
//! answer every question except the one a person coming back to a terminal
//! actually asks — *what happened while I was away?* That needs one number
//! per reader: the cursor of the last event they saw. It lives in `meta`,
//! next to the schema version, because it is bookkeeping about this database
//! rather than a fact about any task, and it is per project because a person
//! comes back to a project, not to a file.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// Reader-side cursors into the trail.
pub struct Bookmarks<'a> {
    conn: &'a Connection,
}

impl<'a> Bookmarks<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Bookmarks<'a> {
        Bookmarks { conn }
    }

    fn key(name: &str, project: &str) -> String {
        format!("bookmark:{name}:{project}")
    }

    /// The cursor last recorded under `name` for `project`; `None` if nobody
    /// has looked yet.
    pub fn get(&self, name: &str, project: &str) -> Result<Option<i64>> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [Self::key(name, project)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|v| v.parse().ok()))
    }

    /// Record `cursor` under `name` for `project`, replacing what was there.
    pub fn set(&self, name: &str, project: &str, cursor: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![Self::key(name, project), cursor.to_string()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    #[test]
    fn a_bookmark_is_absent_until_set_and_then_replaces_itself() {
        let db = Db::open_in_memory().unwrap();
        let marks = db.bookmarks();
        assert_eq!(marks.get("digest", "/p").unwrap(), None);
        marks.set("digest", "/p", 7).unwrap();
        assert_eq!(marks.get("digest", "/p").unwrap(), Some(7));
        marks.set("digest", "/p", 12).unwrap();
        assert_eq!(marks.get("digest", "/p").unwrap(), Some(12));
        // Projects do not share a bookmark.
        assert_eq!(marks.get("digest", "/q").unwrap(), None);
    }
}
