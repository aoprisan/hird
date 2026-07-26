//! Recall: the memory a task arrives with.
//!
//! `mem_search` only helps an agent that thinks to search, and an agent that
//! has just been handed a task does not yet know what it does not know. The
//! queue does: it knows which files this task expects to touch, and it knows
//! which facts were recorded by agents who worked those same files before.
//!
//! So a claim comes back with the assertions that earlier work in this
//! territory produced, each one carrying the reason it surfaced. Nothing is
//! stored for this — recall is derived at read time from the assertion trail,
//! the declared file scopes and [`crate::glob::intersects`], the same exact
//! pattern intersection the collision detector uses. An assertion recorded on
//! `src/config.rs` reaches a task that declared `src/*.rs` without either side
//! having named the other.
//!
//! Three things make an assertion relevant, strongest first:
//!
//! 1. it was recorded while working *this* task, before it came back around;
//! 2. it was recorded on a task whose declared files overlap this one's;
//! 3. it reads like the title.
//!
//! Superseded assertions never surface: recall is what is true now.

use rusqlite::Connection;

use super::memory::{row_to_assertion, Memory, MemoryQuery, ASSERTION_COLUMNS};
use super::scope::Scopes;
use super::tasks::Tasks;
use super::ProjectScope;
use crate::error::Result;
use crate::glob;
use crate::model::Assertion;

/// How many words of a title are worth searching on.
const MAX_TERMS: usize = 8;

/// Shorter than this and a word carries no signal of its own.
const MIN_TERM_LEN: usize = 3;

/// Words that appear in every other task title.
const STOPWORDS: &[&str] = &[
    "and", "are", "but", "for", "from", "into", "not", "the", "that", "this", "those", "these",
    "with", "when", "why", "how", "its", "our", "out", "over", "under", "onto", "off", "all",
    "any", "can", "has", "have", "was", "were", "will", "should", "must", "then", "than", "there",
    "their", "them", "they", "you", "your",
];

/// Why an assertion was recalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallReason {
    /// Recorded while working this very task, on an earlier pass.
    SameTask,
    /// Recorded on another task that declared an overlapping file.
    SameFiles {
        /// This task's pattern that overlaps.
        pattern: String,
        /// The pattern the earlier task had declared.
        other_pattern: String,
        seq: i64,
        title: String,
    },
    /// The assertion reads like this task's title.
    Wording { terms: Vec<String> },
}

impl RecallReason {
    /// The reason as a sentence, for a model to relay or a human to read.
    pub fn describe(&self) -> String {
        match self {
            RecallReason::SameTask => "recorded while working this task earlier".to_string(),
            RecallReason::SameFiles {
                pattern,
                other_pattern,
                seq,
                title,
            } => {
                let where_ = if pattern == other_pattern {
                    pattern.clone()
                } else {
                    format!("{other_pattern}, which your {pattern} overlaps")
                };
                format!("learned on task {seq} ({title}), working {where_}")
            }
            RecallReason::Wording { terms } if terms.is_empty() => {
                "reads like this task's title".to_string()
            }
            RecallReason::Wording { terms } => {
                let listed = terms
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("mentions {listed}")
            }
        }
    }

    /// Rank: lower is a stronger reason to surface the assertion.
    fn strength(&self) -> u8 {
        match self {
            RecallReason::SameTask => 0,
            RecallReason::SameFiles { .. } => 1,
            RecallReason::Wording { .. } => 2,
        }
    }
}

/// One recalled assertion and the reason it came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recalled {
    pub assertion: Assertion,
    pub reason: RecallReason,
    /// The task the assertion was recorded on, as a human refers to it.
    pub task_seq: Option<i64>,
}

/// Derives the memory relevant to a task. Owns no table of its own.
pub struct Recall<'a> {
    conn: &'a Connection,
}

impl<'a> Recall<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Recall<'a> {
        Recall { conn }
    }

    /// The assertions worth putting in front of whoever picks up task `seq`,
    /// strongest reason first and at most `limit` of them.
    ///
    /// A limit of zero turns recall off and costs nothing.
    pub fn for_task(&self, seq: i64, limit: usize) -> Result<Vec<Recalled>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let task = Tasks::new(self.conn).get(seq)?;
        let patterns = Scopes::new(self.conn).for_task(seq)?;

        let mut picked: Vec<Recalled> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let take = |candidate: Recalled, picked: &mut Vec<Recalled>, seen: &mut Vec<String>| {
            if seen.contains(&candidate.assertion.id) {
                return;
            }
            seen.push(candidate.assertion.id.clone());
            picked.push(candidate);
        };

        // 1. What this task itself learned, if it has been worked before.
        for assertion in Memory::new(self.conn).for_task(&task.id)? {
            if assertion.superseded_by.is_some() {
                continue;
            }
            take(
                Recalled {
                    assertion,
                    reason: RecallReason::SameTask,
                    task_seq: Some(seq),
                },
                &mut picked,
                &mut seen,
            );
        }

        // 2. What other agents learned in the same files.
        for candidate in self.by_files(&task.id, &task.project, &patterns, limit)? {
            take(candidate, &mut picked, &mut seen);
        }

        // 3. Whatever else reads like the title.
        if picked.len() < limit {
            for candidate in self.by_wording(&task.project, &task.title, limit)? {
                take(candidate, &mut picked, &mut seen);
            }
        }

        picked.sort_by_key(|r| r.reason.strength());
        picked.truncate(limit);
        Ok(picked)
    }

    /// Assertions recorded on tasks whose declared files overlap `patterns`.
    ///
    /// Intersection is decided in Rust rather than in SQL, because SQL cannot
    /// answer "could these two patterns name the same path". Rows arrive newest
    /// first and the walk stops as soon as `limit` distinct assertions match, so
    /// it reads the whole scoped memory only when there is little to find —
    /// which, on a local database of a few thousand rows, is still cheap.
    fn by_files(
        &self,
        task_id: &str,
        project: &str,
        patterns: &[String],
        limit: usize,
    ) -> Result<Vec<Recalled>> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {ASSERTION_COLUMNS}, t.seq, t.title, p.pattern
             FROM assertions a
             JOIN tasks t ON t.id = a.task_id
             JOIN task_paths p ON p.task_id = t.id
             WHERE a.project = ?1 AND a.superseded_by IS NULL AND a.task_id <> ?2
             ORDER BY a.created_at DESC, a.rowid DESC, p.rowid"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params![project, task_id])?;

        let mut out: Vec<Recalled> = Vec::new();
        while let Some(row) = rows.next()? {
            let assertion = row_to_assertion(row)?;
            if out.iter().any(|r| r.assertion.id == assertion.id) {
                continue;
            }
            let other_seq: i64 = row.get(8)?;
            let other_title: String = row.get(9)?;
            let other_pattern: String = row.get(10)?;
            let Some(mine) = patterns
                .iter()
                .find(|mine| glob::intersects(mine, &other_pattern))
            else {
                continue;
            };
            out.push(Recalled {
                assertion,
                reason: RecallReason::SameFiles {
                    pattern: mine.clone(),
                    other_pattern,
                    seq: other_seq,
                    title: other_title,
                },
                task_seq: Some(other_seq),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Assertions that read like the task's title.
    fn by_wording(&self, project: &str, title: &str, limit: usize) -> Result<Vec<Recalled>> {
        let terms = title_terms(title);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let query = fts_any_of(&terms);
        let hits = Memory::new(self.conn).search(
            &MemoryQuery::new(&query, ProjectScope::Only(project.to_string())).limit(limit),
        )?;
        let seqs = Tasks::new(self.conn).seq_index()?;
        Ok(hits
            .into_iter()
            .map(|assertion| {
                let matched = matching_terms(&assertion, &terms);
                let task_seq = assertion
                    .task_id
                    .as_ref()
                    .and_then(|id| seqs.get(id).copied());
                Recalled {
                    assertion,
                    reason: RecallReason::Wording { terms: matched },
                    task_seq,
                }
            })
            .collect())
    }
}

/// The words of a title worth searching on: long enough, not furniture.
fn title_terms(title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in title.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let word = word.to_lowercase();
        if word.len() < MIN_TERM_LEN || STOPWORDS.contains(&word.as_str()) {
            continue;
        }
        if !out.contains(&word) {
            out.push(word);
        }
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

/// An FTS5 query matching any of `terms`.
///
/// Every term is quoted: they are alphanumeric by construction, but quoting
/// keeps a word like `or` from being read as an operator if the stoplist ever
/// loses one.
fn fts_any_of(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Which of `terms` actually appear in the assertion, for the "why" line.
fn matching_terms(assertion: &Assertion, terms: &[String]) -> Vec<String> {
    let haystack = format!("{} {}", assertion.content, assertion.tags).to_lowercase();
    terms
        .iter()
        .filter(|t| haystack.contains(t.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::model::Status;
    use crate::repo::{NewAssertion, OnConflict};
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// File a task with a declared scope, the way `hird add --path` does.
    fn task(db: &Db, title: &str, paths: &[&str]) -> i64 {
        let seq = db.tasks().create(PROJECT, title, "", 0, "cli").unwrap().seq;
        if !paths.is_empty() {
            let owned: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            db.scopes()
                .declare(seq, &owned, "cli", OnConflict::Report)
                .unwrap();
        }
        seq
    }

    fn learn(db: &Db, seq: Option<i64>, content: &str) {
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content,
                tags: "",
                actor: "codex:9f2c",
                task_seq: seq,
            })
            .unwrap();
    }

    fn contents(recalled: &[Recalled]) -> Vec<&str> {
        recalled
            .iter()
            .map(|r| r.assertion.content.as_str())
            .collect()
    }

    #[test]
    fn a_fact_learned_in_the_same_files_reaches_the_next_task() {
        let db = db();
        let first = task(&db, "Port the config loader", &["src/config.rs"]);
        learn(&db, Some(first), "config precedence is env over file");
        let second = task(&db, "Audit the loader", &["src/*.rs"]);

        let recalled = db.recall().for_task(second, 5).unwrap();
        assert_eq!(
            contents(&recalled),
            vec!["config precedence is env over file"]
        );
        assert_eq!(recalled[0].task_seq, Some(first));
        let why = recalled[0].reason.describe();
        assert!(why.contains(&format!("task {first}")), "{why}");
        assert!(why.contains("src/config.rs"), "{why}");
    }

    /// The intersection is exact, not a string comparison: a task declaring
    /// `src/*.rs` must not be handed what was learned about `tests/`.
    #[test]
    fn facts_from_files_this_task_cannot_touch_stay_away() {
        let db = db();
        let other = task(&db, "Rewrite the renderer", &["src/tui/**"]);
        learn(&db, Some(other), "the renderer redraws on every poll");
        let mine = task(&db, "Something else entirely", &["docs/*.md"]);

        assert!(db.recall().for_task(mine, 5).unwrap().is_empty());
    }

    #[test]
    fn what_this_task_learned_before_comes_back_first() {
        let db = db();
        let neighbour = task(&db, "Port the loader", &["src/config.rs"]);
        learn(&db, Some(neighbour), "the loader is in src/config.rs");
        let mine = task(&db, "Finish the loader", &["src/config.rs"]);
        learn(&db, Some(mine), "half of this was already done");

        let recalled = db.recall().for_task(mine, 5).unwrap();
        assert_eq!(
            contents(&recalled),
            vec![
                "half of this was already done",
                "the loader is in src/config.rs"
            ]
        );
        assert_eq!(recalled[0].reason, RecallReason::SameTask);
        assert_eq!(
            recalled[0].reason.describe(),
            "recorded while working this task earlier"
        );
    }

    #[test]
    fn a_fact_that_reads_like_the_title_surfaces_without_any_file_scope() {
        let db = db();
        learn(&db, None, "the renderer redraws on every poll");
        let mine = task(&db, "Fix the renderer", &[]);

        let recalled = db.recall().for_task(mine, 5).unwrap();
        assert_eq!(
            contents(&recalled),
            vec!["the renderer redraws on every poll"]
        );
        assert_eq!(
            recalled[0].reason,
            RecallReason::Wording {
                terms: vec!["renderer".to_string()]
            }
        );
        assert_eq!(recalled[0].reason.describe(), "mentions \"renderer\"");
    }

    #[test]
    fn a_superseded_fact_is_never_recalled() {
        let db = db();
        let first = task(&db, "Port the loader", &["src/config.rs"]);
        let stale = db
            .memory()
            .store(NewAssertion {
                project: PROJECT,
                content: "the loader reads config.json",
                tags: "",
                actor: "a:1",
                task_seq: Some(first),
            })
            .unwrap();
        db.memory()
            .supersede(&stale.id, "the loader reads config.toml", "cli")
            .unwrap();
        let mine = task(&db, "Audit the loader", &["src/config.rs"]);

        let recalled = db.recall().for_task(mine, 5).unwrap();
        assert_eq!(contents(&recalled), vec!["the loader reads config.toml"]);
    }

    #[test]
    fn one_assertion_is_recalled_once_however_many_reasons_it_has() {
        let db = db();
        let first = task(
            &db,
            "Port the config loader",
            &["src/config.rs", "tests/**"],
        );
        // Matches on both declared patterns, and on the title's wording.
        learn(
            &db,
            Some(first),
            "the config loader is tested in tests/config.rs",
        );
        let mine = task(&db, "Port the config loader again", &["src/**", "tests/**"]);

        let recalled = db.recall().for_task(mine, 5).unwrap();
        assert_eq!(recalled.len(), 1);
    }

    #[test]
    fn the_limit_is_honoured_and_zero_turns_recall_off() {
        let db = db();
        let first = task(&db, "Port the loader", &["src/config.rs"]);
        for i in 0..5 {
            learn(&db, Some(first), &format!("fact {i} about the loader"));
        }
        let mine = task(&db, "Audit the loader", &["src/config.rs"]);

        assert_eq!(db.recall().for_task(mine, 2).unwrap().len(), 2);
        assert!(db.recall().for_task(mine, 0).unwrap().is_empty());
    }

    /// Recall reaches into finished work — that is the whole point — but never
    /// across projects, where the files mean something else entirely.
    #[test]
    fn recall_crosses_time_but_not_projects() {
        let db = db();
        let done = task(&db, "Port the loader", &["src/config.rs"]);
        learn(&db, Some(done), "the loader needs HIRD_DB set");
        db.tasks()
            .claim(done, "codex:9f2c", Duration::from_secs(60))
            .unwrap();
        db.tasks().complete(done, "codex:9f2c", "ported").unwrap();
        assert_eq!(db.tasks().get(done).unwrap().status, Status::Done);

        let elsewhere = db
            .tasks()
            .create("/other/project", "Audit the loader", "", 0, "cli")
            .unwrap()
            .seq;
        db.scopes()
            .declare(
                elsewhere,
                &["src/config.rs".to_string()],
                "cli",
                OnConflict::Report,
            )
            .unwrap();

        let mine = task(&db, "Audit the loader", &["src/config.rs"]);
        assert_eq!(
            contents(&db.recall().for_task(mine, 5).unwrap()),
            vec!["the loader needs HIRD_DB set"]
        );
        assert!(db.recall().for_task(elsewhere, 5).unwrap().is_empty());
    }

    #[test]
    fn titles_lose_their_furniture_words() {
        assert_eq!(
            title_terms("Port the config loader to serde"),
            vec!["port", "config", "loader", "serde"]
        );
        // `to` and `a` are too short, `the` is a stopword: nothing is left.
        assert!(title_terms("To a the").is_empty());
    }

    #[test]
    fn terms_are_quoted_into_an_any_of_query() {
        assert_eq!(
            fts_any_of(&["config".to_string(), "loader".to_string()]),
            "\"config\" OR \"loader\""
        );
    }

    /// A title full of punctuation must not produce a query FTS5 rejects.
    #[test]
    fn a_punctuation_heavy_title_still_searches() {
        let db = db();
        learn(&db, None, "handle_event takes the context");
        let mine = task(&db, "Fix handle_event(ctx) — again!", &[]);
        let recalled = db.recall().for_task(mine, 5).unwrap();
        assert_eq!(contents(&recalled), vec!["handle_event takes the context"]);
    }

    #[test]
    fn a_task_with_nothing_around_it_recalls_nothing() {
        let db = db();
        let mine = task(&db, "Xyzzy", &["src/xyzzy.rs"]);
        assert!(db.recall().for_task(mine, 5).unwrap().is_empty());
    }

    #[test]
    fn recalling_a_task_that_does_not_exist_says_so() {
        let db = db();
        let err = db.recall().for_task(404, 5).unwrap_err();
        assert_eq!(err.to_string(), "task 404 not found");
    }
}
