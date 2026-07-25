//! Assertion memory repository: provenance-carrying facts with FTS5 search.

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use super::{new_id, ProjectScope};
use crate::error::{Error, Result};
use crate::model::{normalize_tags, now_ts, Assertion};

const ASSERTION_COLUMNS: &str =
    "a.id, a.project, a.content, a.tags, a.actor, a.task_id, a.superseded_by, a.created_at";

/// A new assertion, before it is given an id and a timestamp.
#[derive(Debug, Clone)]
pub struct NewAssertion<'a> {
    pub project: &'a str,
    pub content: &'a str,
    pub tags: &'a str,
    pub actor: &'a str,
    /// Human-facing `seq` of the task this was learned while working, if any.
    pub task_seq: Option<i64>,
}

/// Search parameters for [`Memory::search`].
#[derive(Debug, Clone)]
pub struct MemoryQuery<'a> {
    /// FTS5 query text. Empty means "everything, newest first".
    pub query: &'a str,
    pub scope: ProjectScope,
    pub limit: usize,
    pub include_superseded: bool,
}

impl<'a> MemoryQuery<'a> {
    pub fn new(query: &'a str, scope: ProjectScope) -> Self {
        MemoryQuery {
            query,
            scope,
            limit: 20,
            include_superseded: false,
        }
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn include_superseded(mut self, include: bool) -> Self {
        self.include_superseded = include;
        self
    }
}

/// Repository over `assertions` and its FTS5 index.
pub struct Memory<'a> {
    conn: &'a Connection,
}

impl<'a> Memory<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Memory<'a> {
        Memory { conn }
    }

    fn immediate_tx(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    /// Record one factual assertion.
    pub fn store(&self, new: NewAssertion<'_>) -> Result<Assertion> {
        let content = new.content.trim();
        if content.is_empty() {
            return Err(Error::invalid("assertion content must not be empty"));
        }
        let tx = self.immediate_tx()?;
        let task_id = match new.task_seq {
            Some(seq) => Some(
                tx.query_row("SELECT id FROM tasks WHERE seq = ?1", [seq], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
                .ok_or(Error::TaskNotFound { seq })?,
            ),
            None => None,
        };
        let assertion = Assertion {
            id: new_id(),
            project: new.project.to_string(),
            content: content.to_string(),
            tags: normalize_tags(new.tags),
            actor: new.actor.to_string(),
            task_id,
            superseded_by: None,
            created_at: now_ts(),
        };
        tx.execute(
            "INSERT INTO assertions (id, project, content, tags, actor, task_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                assertion.id,
                assertion.project,
                assertion.content,
                assertion.tags,
                assertion.actor,
                assertion.task_id,
                assertion.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(assertion)
    }

    /// Fetch one assertion by id.
    pub fn get(&self, id: &str) -> Result<Assertion> {
        self.conn
            .query_row(
                &format!("SELECT {ASSERTION_COLUMNS} FROM assertions a WHERE a.id = ?1"),
                [id],
                row_to_assertion,
            )
            .optional()?
            .ok_or_else(|| Error::AssertionNotFound { id: id.to_string() })
    }

    /// Search assertions.
    ///
    /// Tries FTS5 `MATCH` first and falls back to a `LIKE` scan when the query
    /// is not valid FTS5 syntax — an agent typing `foo(bar)` should still get
    /// results rather than a parse error.
    pub fn search(&self, q: &MemoryQuery<'_>) -> Result<Vec<Assertion>> {
        let query = q.query.trim();
        if query.is_empty() {
            return self.recent(q);
        }
        match self.search_fts(q, query) {
            Ok(hits) => Ok(hits),
            Err(Error::Sqlite(_)) => self.search_like(q, query),
            Err(other) => Err(other),
        }
    }

    fn search_fts(&self, q: &MemoryQuery<'_>, query: &str) -> Result<Vec<Assertion>> {
        let (project_clause, project_value) = q.scope.clause("a.project");
        let sql = format!(
            "SELECT {ASSERTION_COLUMNS} FROM assertions_fts f
             JOIN assertions a ON a.rowid = f.rowid
             WHERE f MATCH ? AND {project_clause} {superseded}
             ORDER BY f.rank, a.created_at DESC, a.rowid DESC
             LIMIT ?",
            superseded = superseded_clause(q.include_superseded),
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.to_string())];
        if let Some(p) = project_value {
            binds.push(Box::new(p.to_string()));
        }
        binds.push(Box::new(q.limit as i64));
        self.run(&sql, binds)
    }

    /// Degraded search for queries FTS5 refuses to parse.
    ///
    /// The raw query is unlikely to appear verbatim in any assertion — it is
    /// malformed, after all — so it is broken into word-ish terms and every
    /// term is required to appear somewhere in the content or tags.
    fn search_like(&self, q: &MemoryQuery<'_>, query: &str) -> Result<Vec<Assertion>> {
        let terms = like_terms(query);
        if terms.is_empty() {
            return self.recent(q);
        }
        let (project_clause, project_value) = q.scope.clause("a.project");
        let term_clause = terms
            .iter()
            .map(|_| "(a.content LIKE ? ESCAPE '\\' OR a.tags LIKE ? ESCAPE '\\')")
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!(
            "SELECT {ASSERTION_COLUMNS} FROM assertions a
             WHERE {term_clause} AND {project_clause} {superseded}
             ORDER BY a.created_at DESC, a.rowid DESC
             LIMIT ?",
            superseded = superseded_clause(q.include_superseded),
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for term in &terms {
            let pattern = format!("%{}%", escape_like(term));
            // Bound twice: once for `content`, once for `tags`.
            binds.push(Box::new(pattern.clone()));
            binds.push(Box::new(pattern));
        }
        if let Some(p) = project_value {
            binds.push(Box::new(p.to_string()));
        }
        binds.push(Box::new(q.limit as i64));
        self.run(&sql, binds)
    }

    fn recent(&self, q: &MemoryQuery<'_>) -> Result<Vec<Assertion>> {
        let (project_clause, project_value) = q.scope.clause("a.project");
        let sql = format!(
            "SELECT {ASSERTION_COLUMNS} FROM assertions a
             WHERE {project_clause} {superseded}
             ORDER BY a.created_at DESC, a.rowid DESC
             LIMIT ?",
            superseded = superseded_clause(q.include_superseded),
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(p) = project_value {
            binds.push(Box::new(p.to_string()));
        }
        binds.push(Box::new(q.limit as i64));
        self.run(&sql, binds)
    }

    fn run(&self, sql: &str, binds: Vec<Box<dyn rusqlite::ToSql>>) -> Result<Vec<Assertion>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), row_to_assertion)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark `id` as superseded by a new assertion recording why.
    ///
    /// The replacement is a real assertion in the same project, so provenance
    /// stays intact and the retraction is itself searchable.
    pub fn supersede(&self, id: &str, reason: &str, actor: &str) -> Result<Assertion> {
        let existing = self.get(id)?;
        if let Some(by) = &existing.superseded_by {
            return Err(Error::invalid(format!(
                "assertion {id} is already superseded by {by}"
            )));
        }
        let reason = reason.trim();
        let content = if reason.is_empty() {
            format!("Retracted: {}", existing.content)
        } else {
            reason.to_string()
        };

        let tx = self.immediate_tx()?;
        let replacement = Assertion {
            id: new_id(),
            project: existing.project.clone(),
            content,
            tags: existing.tags.clone(),
            actor: actor.to_string(),
            task_id: existing.task_id.clone(),
            superseded_by: None,
            created_at: now_ts(),
        };
        tx.execute(
            "INSERT INTO assertions (id, project, content, tags, actor, task_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                replacement.id,
                replacement.project,
                replacement.content,
                replacement.tags,
                replacement.actor,
                replacement.task_id,
                replacement.created_at,
            ],
        )?;
        tx.execute(
            "UPDATE assertions SET superseded_by = ?1 WHERE id = ?2 AND superseded_by IS NULL",
            params![replacement.id, existing.id],
        )?;
        tx.commit()?;
        Ok(replacement)
    }

    /// Assertions recorded while working a given task.
    pub fn for_task(&self, task_id: &str) -> Result<Vec<Assertion>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ASSERTION_COLUMNS} FROM assertions a
             WHERE a.task_id = ?1 ORDER BY a.created_at ASC"
        ))?;
        let rows = stmt.query_map([task_id], row_to_assertion)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every distinct project that has at least one assertion.
    pub fn projects(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT project FROM assertions ORDER BY project")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Number of current (non-superseded) assertions in `scope`.
    pub fn count_current(&self, scope: &ProjectScope) -> Result<i64> {
        let (project_clause, project_value) = scope.clause("a.project");
        let sql = format!(
            "SELECT COUNT(*) FROM assertions a
             WHERE {project_clause} AND a.superseded_by IS NULL"
        );
        let binds: Vec<&str> = project_value.into_iter().collect();
        Ok(self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(binds), |row| row.get(0))?)
    }
}

fn superseded_clause(include: bool) -> &'static str {
    if include {
        ""
    } else {
        "AND a.superseded_by IS NULL"
    }
}

/// Break a malformed FTS query into plain search terms.
///
/// Everything FTS5 treats as syntax (quotes, parentheses, `*`, `^`, `:`) is a
/// separator; `_` survives because it is part of so many identifiers.
fn like_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Escape the wildcards `LIKE` would otherwise interpret.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn row_to_assertion(row: &Row<'_>) -> rusqlite::Result<Assertion> {
    Ok(Assertion {
        id: row.get(0)?,
        project: row.get(1)?,
        content: row.get(2)?,
        tags: row.get(3)?,
        actor: row.get(4)?,
        task_id: row.get(5)?,
        superseded_by: row.get(6)?,
        created_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const PROJECT: &str = "/tmp/project";

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn store(db: &Db, content: &str, tags: &str) -> Assertion {
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content,
                tags,
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap()
    }

    fn contents(hits: &[Assertion]) -> Vec<&str> {
        hits.iter().map(|a| a.content.as_str()).collect()
    }

    #[test]
    fn storing_normalizes_tags_and_trims_content() {
        let db = db();
        let a = store(&db, "  the build uses just  ", " build , , tooling ");
        assert_eq!(a.content, "the build uses just");
        assert_eq!(a.tags, "build,tooling");
        assert_eq!(a.tag_list(), vec!["build", "tooling"]);
        assert!(a.superseded_by.is_none());
    }

    #[test]
    fn empty_content_is_rejected() {
        let db = db();
        let err = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "   ",
                tags: "",
                actor: "a:1",
                task_seq: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("content must not be empty"));
    }

    #[test]
    fn an_assertion_can_be_linked_to_a_task() {
        let db = db();
        let task = db.tasks().create(PROJECT, "t", "", 0, "cli").unwrap();
        let a = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "migrations live in src/db.rs",
                tags: "",
                actor: "a:1",
                task_seq: Some(task.seq),
            })
            .unwrap();
        assert_eq!(a.task_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(db.memory().for_task(&task.id).unwrap().len(), 1);
    }

    #[test]
    fn linking_to_a_missing_task_is_an_error() {
        let db = db();
        let err = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "x",
                tags: "",
                actor: "a:1",
                task_seq: Some(404),
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "task 404 not found");
    }

    #[test]
    fn fts_matches_content_and_tags() {
        let db = db();
        store(&db, "the parser lives in src/parse.rs", "parser");
        store(&db, "the renderer lives in src/render.rs", "ui");

        let scope = ProjectScope::Only(PROJECT.into());
        let hits = db
            .memory()
            .search(&MemoryQuery::new("parser", scope.clone()))
            .unwrap();
        assert_eq!(contents(&hits), vec!["the parser lives in src/parse.rs"]);

        let by_tag = db.memory().search(&MemoryQuery::new("ui", scope)).unwrap();
        assert_eq!(
            contents(&by_tag),
            vec!["the renderer lives in src/render.rs"]
        );
    }

    #[test]
    fn malformed_fts_queries_fall_back_to_like() {
        let db = db();
        store(&db, "call handle_event(ctx) before draw", "");
        // Unbalanced quote: invalid FTS5 syntax.
        let hits = db
            .memory()
            .search(&MemoryQuery::new(
                "handle_event(ctx\"",
                ProjectScope::Only(PROJECT.into()),
            ))
            .unwrap();
        assert_eq!(contents(&hits), vec!["call handle_event(ctx) before draw"]);
    }

    #[test]
    fn the_like_fallback_requires_every_term_to_match() {
        let db = db();
        store(&db, "the parser calls draw", "");
        store(&db, "the parser calls nothing", "");
        // Unbalanced quote forces the fallback; both terms must appear.
        let hits = db
            .memory()
            .search(&MemoryQuery::new(
                "\"parser draw",
                ProjectScope::Only(PROJECT.into()),
            ))
            .unwrap();
        assert_eq!(contents(&hits), vec!["the parser calls draw"]);
    }

    #[test]
    fn the_like_fallback_treats_underscores_literally() {
        let db = db();
        store(&db, "call handle_event now", "");
        store(&db, "call handleXevent now", "");
        let hits = db
            .memory()
            .search(&MemoryQuery::new(
                "handle_event\"",
                ProjectScope::Only(PROJECT.into()),
            ))
            .unwrap();
        assert_eq!(contents(&hits), vec!["call handle_event now"]);
    }

    #[test]
    fn a_query_of_pure_punctuation_degrades_to_recent() {
        let db = db();
        store(&db, "older", "");
        store(&db, "newer", "");
        let hits = db
            .memory()
            .search(&MemoryQuery::new(
                "\"((",
                ProjectScope::Only(PROJECT.into()),
            ))
            .unwrap();
        assert_eq!(contents(&hits), vec!["newer", "older"]);
    }

    #[test]
    fn like_terms_split_on_fts_syntax() {
        assert_eq!(
            like_terms("handle_event(ctx\""),
            vec!["handle_event", "ctx"]
        );
        assert!(like_terms("\"((*^").is_empty());
    }

    #[test]
    fn escaping_neutralizes_like_wildcards() {
        assert_eq!(escape_like("a_b%c\\d"), "a\\_b\\%c\\\\d");
    }

    #[test]
    fn an_empty_query_returns_the_most_recent_first() {
        let db = db();
        store(&db, "older", "");
        store(&db, "newer", "");
        let hits = db
            .memory()
            .search(&MemoryQuery::new("  ", ProjectScope::Only(PROJECT.into())))
            .unwrap();
        assert_eq!(contents(&hits), vec!["newer", "older"]);
    }

    #[test]
    fn search_is_project_scoped_unless_told_otherwise() {
        let db = db();
        store(&db, "shared word here", "");
        db.memory()
            .store(NewAssertion {
                project: "/other",
                content: "shared word elsewhere",
                tags: "",
                actor: "a:1",
                task_seq: None,
            })
            .unwrap();

        let scoped = db
            .memory()
            .search(&MemoryQuery::new(
                "shared",
                ProjectScope::Only(PROJECT.into()),
            ))
            .unwrap();
        assert_eq!(scoped.len(), 1);

        let global = db
            .memory()
            .search(&MemoryQuery::new("shared", ProjectScope::All))
            .unwrap();
        assert_eq!(global.len(), 2);
    }

    #[test]
    fn the_limit_is_honoured() {
        let db = db();
        for i in 0..10 {
            store(&db, &format!("fact number {i} about widgets"), "");
        }
        let hits = db
            .memory()
            .search(&MemoryQuery::new("widgets", ProjectScope::Only(PROJECT.into())).limit(3))
            .unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn superseding_hides_the_original_and_records_a_replacement() {
        let db = db();
        let original = store(&db, "the api listens on port 8080", "api");
        let replacement = db
            .memory()
            .supersede(&original.id, "the api listens on port 9090", "tui")
            .unwrap();

        assert_eq!(replacement.actor, "tui");
        assert_eq!(replacement.tags, "api");
        assert_eq!(
            db.memory()
                .get(&original.id)
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(replacement.id.as_str())
        );

        let scope = ProjectScope::Only(PROJECT.into());
        let current = db
            .memory()
            .search(&MemoryQuery::new("api", scope.clone()))
            .unwrap();
        assert_eq!(contents(&current), vec!["the api listens on port 9090"]);

        let with_history = db
            .memory()
            .search(&MemoryQuery::new("api", scope).include_superseded(true))
            .unwrap();
        assert_eq!(with_history.len(), 2);
    }

    #[test]
    fn superseding_without_a_reason_writes_a_retraction() {
        let db = db();
        let original = store(&db, "cache is write-through", "");
        let replacement = db.memory().supersede(&original.id, "  ", "tui").unwrap();
        assert_eq!(replacement.content, "Retracted: cache is write-through");
    }

    #[test]
    fn an_assertion_cannot_be_superseded_twice() {
        let db = db();
        let original = store(&db, "x is true", "");
        db.memory()
            .supersede(&original.id, "x is false", "tui")
            .unwrap();
        let err = db
            .memory()
            .supersede(&original.id, "again", "tui")
            .unwrap_err();
        assert!(err.to_string().contains("already superseded"), "{err}");
    }

    #[test]
    fn superseding_a_missing_assertion_says_so() {
        let db = db();
        let err = db.memory().supersede("nope", "r", "tui").unwrap_err();
        assert_eq!(err.to_string(), "assertion nope not found");
    }

    #[test]
    fn counts_and_projects_reflect_current_assertions() {
        let db = db();
        let a = store(&db, "one", "");
        store(&db, "two", "");
        db.memory()
            .store(NewAssertion {
                project: "/other",
                content: "three",
                tags: "",
                actor: "a:1",
                task_seq: None,
            })
            .unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        assert_eq!(db.memory().count_current(&scope).unwrap(), 2);
        db.memory().supersede(&a.id, "one prime", "tui").unwrap();
        // The retraction itself is current, so the count holds steady.
        assert_eq!(db.memory().count_current(&scope).unwrap(), 2);
        assert_eq!(db.memory().count_current(&ProjectScope::All).unwrap(), 3);
        assert_eq!(db.memory().projects().unwrap(), vec!["/other", PROJECT]);
    }

    #[test]
    fn the_fts_index_tracks_updates_to_the_base_table() {
        let db = db();
        let a = store(&db, "findable phrase", "");
        // supersede() updates the base row, which the FTS triggers must mirror.
        db.memory()
            .supersede(&a.id, "replacement phrase", "tui")
            .unwrap();
        let hits = db
            .memory()
            .search(
                &MemoryQuery::new("phrase", ProjectScope::Only(PROJECT.into()))
                    .include_superseded(true),
            )
            .unwrap();
        assert_eq!(hits.len(), 2);
    }
}
