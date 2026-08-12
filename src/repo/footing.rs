//! The footing under an assertion: what it was read off, and who else says it.
//!
//! Everything hird stores about a task is answerable — a claim has a holder, a
//! scope has an overlap, a footprint has a diff. Memory is the one part that
//! was not. An assertion recorded in March is served up in July in exactly the
//! same voice, with exactly the same confidence, whether the code it describes
//! has been untouched since or rewritten twice. That is how a shared memory
//! stops being an asset: not by filling up with lies, but by filling up with
//! sentences that *were* true, which is worse, because nothing about reading
//! one tells you which kind you have.
//!
//! Nothing about the sentence can tell you. The code can. So an assertion is
//! stored with its **footing** — the files it was read off and the content hash
//! each of them had at the time — and any later reader can ask the working tree
//! whether the ground has moved. That is the whole idea, and hird can have it
//! for almost nothing because [`crate::witness`] is already fingerprinting
//! files for a different reason.
//!
//! Three rules, and they are the same three the witness lives by:
//!
//! - **It reports, it does not judge.** A changed file does not falsify an
//!   assertion; a rename, a reformat and a rewrite are indistinguishable from
//!   here. It marks it *unverified*, which is the useful thing to know, because
//!   that set is exactly where a re-read pays for itself.
//! - **It never fails a call.** No git, no footing, and memory behaves exactly
//!   as it did before this module existed. [`Standing::Unanchored`] is an
//!   ordinary answer.
//! - **Being told is cheap; being wrong is not.** A false "check this" costs an
//!   agent one file read. A fact that quietly went stale costs a working day.
//!
//! And because the ground can move back: restating an assertion that already
//! exists does not duplicate it, it **re-anchors** it to the tree as it stands
//! and records who said it again — see [`super::Memory::record`]. Memory that
//! gets re-grounded by ordinary use is memory that does not need an owner.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::deps::id_for_seq;
use super::memory::{row_to_assertion, ASSERTION_COLUMNS};
use super::ProjectScope;
use crate::error::Result;
use crate::glob;
use crate::model::{now_ts, Anchor, Assertion, Shift, Standing, Voices};

/// Who chose the files an assertion stands on.
///
/// The distinction only matters at one moment, and it matters a lot there: a
/// finishing task re-anchors what it learned to the tree it is leaving behind,
/// and it must not do that to a fact whose author said, in so many words,
/// *this is about these files*. Deriving a footing is hird being helpful;
/// overruling a stated one would be hird being wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    /// hird worked the files out from the task's scope and footprint.
    Derived,
    /// The author named them.
    Named,
}

/// Repository over `assertion_footing` and `assertion_affirmations`.
pub struct Footings<'a> {
    conn: &'a Connection,
}

impl<'a> Footings<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Footings<'a> {
        Footings { conn }
    }

    fn immediate_tx(&self) -> Result<Transaction<'_>> {
        super::immediate_tx(self.conn)
    }

    /// Record what `assertion_id` was read off, replacing any earlier footing.
    ///
    /// Replacement rather than accumulation: re-anchoring is how a verified
    /// assertion gets its standing back, and an anchor set that only ever grew
    /// would keep every version it had ever been checked against and never be
    /// firm again.
    ///
    /// The one thing it will not replace is a footing the author *named* with a
    /// footing hird *derived*, and `false` is that refusal. Deriving a footing
    /// is hird being helpful where nobody said anything; overruling a stated one
    /// — on a finishing task, or on somebody else restating the fact — would be
    /// hird deciding it knows better than the agent that wrote the sentence.
    /// The rule lives here rather than at the two call sites so there is one
    /// authority for it.
    pub fn anchor(&self, assertion_id: &str, anchors: &[Anchor], by: Anchoring) -> Result<bool> {
        if by == Anchoring::Derived && self.anchored_by(assertion_id)? == Anchoring::Named {
            return Ok(false);
        }
        let tx = self.immediate_tx()?;
        tx.execute(
            "DELETE FROM assertion_footing WHERE assertion_id = ?1",
            [assertion_id],
        )?;
        for anchor in anchors {
            tx.execute(
                "INSERT INTO assertion_footing (assertion_id, path, hash, named, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    assertion_id,
                    anchor.path,
                    anchor.hash,
                    i64::from(by == Anchoring::Named),
                    anchor.at
                ],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Did the author of this assertion name its files, or did hird work them
    /// out?
    ///
    /// The one thing settling has to ask before it overwrites anything.
    pub fn anchored_by(&self, assertion_id: &str) -> Result<Anchoring> {
        let named: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(named) FROM assertion_footing WHERE assertion_id = ?1",
                [assertion_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(match named {
            Some(1) => Anchoring::Named,
            _ => Anchoring::Derived,
        })
    }

    /// What one assertion was read off, in path order.
    pub fn anchors(&self, assertion_id: &str) -> Result<Vec<Anchor>> {
        Ok(self
            .anchors_for(None, std::slice::from_ref(&assertion_id.to_string()))?
            .remove(assertion_id)
            .unwrap_or_default())
    }

    /// The same, for many assertions at once.
    ///
    /// A search returns twenty rows and their footings overlap heavily; asking
    /// per row would read the same file twenty times. Absent keys mean an
    /// unanchored assertion, which is why the caller gets a map and not a
    /// parallel vector.
    ///
    /// `project` is not a filter for convenience, it is a correctness bound: an
    /// anchor is a path relative to *its own* project root, and whoever is about
    /// to resolve these against a working tree can only do so for one project.
    /// Passing `None` says the caller has no tree in mind and only wants the
    /// rows.
    pub fn anchors_for(
        &self,
        project: Option<&str>,
        assertion_ids: &[String],
    ) -> Result<BTreeMap<String, Vec<Anchor>>> {
        let mut out: BTreeMap<String, Vec<Anchor>> = BTreeMap::new();
        if assertion_ids.is_empty() {
            return Ok(out);
        }
        // Chunked so a very large search cannot overflow SQLite's parameter
        // limit, which is the same reason the witness batches `ls-tree`.
        for batch in assertion_ids.chunks(256) {
            let holes = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let scoped = if project.is_some() {
                "AND a.project = ?"
            } else {
                ""
            };
            let sql = format!(
                "SELECT f.assertion_id, f.path, f.hash, f.at
                 FROM assertion_footing f JOIN assertions a ON a.id = f.assertion_id
                 WHERE f.assertion_id IN ({holes}) {scoped}
                 ORDER BY f.assertion_id, f.path"
            );
            let mut binds: Vec<&str> = batch.iter().map(String::as_str).collect();
            binds.extend(project);
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Anchor {
                        path: row.get(1)?,
                        hash: row.get(2)?,
                        at: row.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (id, anchor) = row?;
                out.entry(id).or_default().push(anchor);
            }
        }
        Ok(out)
    }

    /// The files a fact learned while working task `seq` is a fact *about*.
    ///
    /// Two sources, and the union is deliberate. What the task declared covers
    /// intent — but only its literal paths, because a glob names a set nobody
    /// has enumerated and hird will not guess at its members. What the witness
    /// saw covers reality, including the files an agent in a hurry never
    /// declared, and a realized glob's members are in there already by
    /// construction: they are the files that actually moved.
    pub fn ground_for_task(&self, seq: i64) -> Result<Vec<String>> {
        let mut paths: Vec<String> = Vec::new();
        for pattern in super::scope::Scopes::new(self.conn).for_task(seq)? {
            if glob::is_literal(&pattern) && !paths.contains(&pattern) {
                paths.push(pattern);
            }
        }
        for observed in super::witness::Witnessed::new(self.conn).touched(seq)? {
            if !paths.contains(&observed.path) {
                paths.push(observed.path);
            }
        }
        Ok(paths)
    }

    /// Every current assertion recorded while working task `seq`.
    ///
    /// The settling list: when a task finishes, what it learned is a statement
    /// about the tree it left behind, not the one it found halfway through.
    pub fn learned_on(&self, seq: i64) -> Result<Vec<Assertion>> {
        let task_id = id_for_seq(self.conn, seq)?;
        super::memory::Memory::new(self.conn)
            .for_task(&task_id)
            .map(|assertions: Vec<Assertion>| {
                assertions
                    .into_iter()
                    .filter(|a| a.superseded_by.is_none())
                    .collect()
            })
    }

    /// Record that `actor` has independently stated `assertion_id` too.
    ///
    /// Idempotent per actor: an agent that says the same thing twice in one
    /// session is not two agents, and the count is only worth anything if it
    /// counts voices rather than sentences.
    pub fn affirm(&self, assertion_id: &str, actor: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO assertion_affirmations (assertion_id, actor, at) VALUES (?1, ?2, ?3)
             ON CONFLICT(assertion_id, actor) DO UPDATE SET at = excluded.at",
            params![assertion_id, actor, now_ts()],
        )?;
        Ok(())
    }

    /// Everyone who has stated this assertion, the original author first.
    pub fn voices(&self, assertion: &Assertion) -> Result<Voices> {
        let mut actors = vec![assertion.actor.clone()];
        let mut stmt = self.conn.prepare(
            "SELECT actor FROM assertion_affirmations WHERE assertion_id = ?1 ORDER BY at, actor",
        )?;
        let rows = stmt.query_map([&assertion.id], |row| row.get::<_, String>(0))?;
        for actor in rows {
            let actor = actor?;
            if !actors.contains(&actor) {
                actors.push(actor);
            }
        }
        Ok(Voices { actors })
    }

    /// Every current assertion in `scope` that has a footing, oldest first.
    ///
    /// Oldest first because the audit's job is to surface what has had the most
    /// time to rot, and a reader who stops halfway down should have seen the
    /// worst of it.
    pub fn anchored(&self, scope: &ProjectScope) -> Result<Vec<Assertion>> {
        let (project_clause, project_value) = scope.clause("a.project");
        let sql = format!(
            "SELECT {ASSERTION_COLUMNS} FROM assertions a
             WHERE {project_clause} AND a.superseded_by IS NULL
               AND EXISTS (SELECT 1 FROM assertion_footing f WHERE f.assertion_id = a.id)
             ORDER BY a.created_at ASC, a.rowid ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), row_to_assertion)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Decide an assertion's standing from its anchors and the tree as it is now.
///
/// `current` answers "what does this file say at the moment", with `""` for a
/// file that is not there. Kept as a closure so the decision itself is pure and
/// testable, and so one filesystem read can serve every assertion in a search.
pub fn assess(anchors: &[Anchor], current: impl Fn(&str) -> String) -> Standing {
    if anchors.is_empty() {
        return Standing::Unanchored;
    }
    let mut moved: Vec<Shift> = Vec::new();
    let mut firm: Vec<String> = Vec::new();
    for anchor in anchors {
        let now = current(&anchor.path);
        if now == anchor.hash {
            firm.push(anchor.path.clone());
        } else {
            moved.push(Shift {
                path: anchor.path.clone(),
                gone: now.is_empty(),
            });
        }
    }
    if moved.is_empty() {
        return Standing::Firm { paths: firm };
    }
    // Orphaned is the strongest claim available, so it is held to the strongest
    // condition: not one file gone, but nothing left standing at all. An
    // assertion about three files of which one was deleted is shaky — the other
    // two may still be exactly what it describes.
    if firm.is_empty() && moved.iter().all(|s| s.gone) {
        return Standing::Orphaned {
            paths: moved.into_iter().map(|s| s.path).collect(),
        };
    }
    Standing::Shaky { moved, firm }
}

/// The union of every path in a batch of footings, each named once.
pub fn paths_in(footings: &BTreeMap<String, Vec<Anchor>>) -> Vec<String> {
    footings
        .values()
        .flatten()
        .map(|a| a.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::repo::{NewAssertion, OnConflict};
    use crate::witness::{Change, ChangeKind, Tree};
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn anchor(path: &str, hash: &str) -> Anchor {
        Anchor {
            path: path.to_string(),
            hash: hash.to_string(),
            at: now_ts(),
        }
    }

    fn store(db: &Db, content: &str, task_seq: Option<i64>) -> Assertion {
        db.memory()
            .store(NewAssertion {
                project: PROJECT,
                content,
                tags: "",
                actor: "codex:9f2c",
                task_seq,
            })
            .unwrap()
    }

    /// A tree where every named file has the given content hash and every
    /// other file is missing.
    fn tree_says<'a>(entries: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> String + 'a {
        move |path: &str| {
            entries
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, h)| h.to_string())
                .unwrap_or_default()
        }
    }

    #[test]
    fn an_assertion_with_no_footing_has_nothing_to_check() {
        let standing = assess(&[], tree_says(&[]));
        assert_eq!(standing, Standing::Unanchored);
        assert_eq!(standing.as_str(), "unanchored");
        assert!(standing.describe().is_none());
        assert!(!standing.needs_checking());
    }

    #[test]
    fn a_file_that_still_says_what_it_said_leaves_the_assertion_firm() {
        let standing = assess(
            &[anchor("src/config.rs", "h1")],
            tree_says(&[("src/config.rs", "h1")]),
        );
        assert_eq!(
            standing,
            Standing::Firm {
                paths: vec!["src/config.rs".to_string()]
            }
        );
        assert!(!standing.needs_checking());
        assert_eq!(
            standing.describe().unwrap(),
            "src/config.rs is unchanged since this was recorded"
        );
    }

    #[test]
    fn a_file_that_has_moved_makes_the_assertion_shaky_not_false() {
        let standing = assess(
            &[anchor("src/config.rs", "h1"), anchor("src/db.rs", "h2")],
            tree_says(&[("src/config.rs", "REWRITTEN"), ("src/db.rs", "h2")]),
        );
        assert_eq!(standing.as_str(), "shaky");
        assert!(standing.needs_checking());
        let why = standing.describe().unwrap();
        assert!(why.starts_with("src/config.rs has changed"), "{why}");
        assert!(why.contains("re-read before relying on it"), "{why}");
        // The unmoved half is kept: it is the reason this is not orphaned.
        match standing {
            Standing::Shaky { moved, firm } => {
                assert_eq!(
                    moved,
                    vec![Shift {
                        path: "src/config.rs".into(),
                        gone: false
                    }]
                );
                assert_eq!(firm, vec!["src/db.rs".to_string()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_assertion_about_files_that_have_all_gone_is_orphaned() {
        let standing = assess(
            &[anchor("src/old.rs", "h1"), anchor("src/older.rs", "h2")],
            tree_says(&[]),
        );
        assert_eq!(standing.as_str(), "orphaned");
        assert_eq!(
            standing.describe().unwrap(),
            "all 2 files this was recorded against have been deleted"
        );
    }

    /// One deleted file out of three is not the end of the assertion: the other
    /// two may still be exactly what it describes.
    #[test]
    fn a_single_deletion_among_survivors_is_only_shaky() {
        let standing = assess(
            &[anchor("a.rs", "h1"), anchor("b.rs", "h2")],
            tree_says(&[("b.rs", "h2")]),
        );
        assert_eq!(standing.as_str(), "shaky");
        let why = standing.describe().unwrap();
        assert!(why.starts_with("a.rs is gone"), "{why}");
    }

    #[test]
    fn standing_ranks_a_verified_fact_above_a_suspect_one() {
        let firm = Standing::Firm { paths: vec![] };
        let shaky = Standing::Shaky {
            moved: vec![],
            firm: vec![],
        };
        let orphaned = Standing::Orphaned { paths: vec![] };
        assert!(firm.rank() < Standing::Unanchored.rank());
        assert!(Standing::Unanchored.rank() < shaky.rank());
        assert!(shaky.rank() < orphaned.rank());
    }

    #[test]
    fn a_footing_round_trips_and_is_replaced_rather_than_accumulated() {
        let db = db();
        let a = store(&db, "the loader reads env first", None);
        db.footings()
            .anchor(&a.id, &[anchor("src/config.rs", "h1")], Anchoring::Derived)
            .unwrap();
        assert_eq!(db.footings().anchors(&a.id).unwrap().len(), 1);

        db.footings()
            .anchor(&a.id, &[anchor("src/config.rs", "h2")], Anchoring::Derived)
            .unwrap();
        let anchors = db.footings().anchors(&a.id).unwrap();
        assert_eq!(anchors.len(), 1, "re-anchoring replaces, {anchors:?}");
        assert_eq!(anchors[0].hash, "h2");
    }

    #[test]
    fn footings_are_fetched_in_one_pass_for_a_whole_search() {
        let db = db();
        let a = store(&db, "one", None);
        let b = store(&db, "two", None);
        let unanchored = store(&db, "three", None);
        db.footings()
            .anchor(
                &a.id,
                &[anchor("src/a.rs", "h1"), anchor("shared.rs", "hs")],
                Anchoring::Derived,
            )
            .unwrap();
        db.footings()
            .anchor(&b.id, &[anchor("shared.rs", "hs")], Anchoring::Derived)
            .unwrap();

        let ids = vec![a.id.clone(), b.id.clone(), unanchored.id.clone()];
        let footings = db.footings().anchors_for(Some(PROJECT), &ids).unwrap();
        assert_eq!(footings.len(), 2, "an unanchored assertion has no entry");
        assert_eq!(footings[&a.id].len(), 2);
        // The shared file is named once, so it is read off disk once.
        assert_eq!(paths_in(&footings), vec!["shared.rs", "src/a.rs"]);
    }

    /// The ground under a fact is where the work actually was: the literal
    /// paths it declared, plus everything the witness saw move. A glob is not
    /// expanded — hird will not invent members for a set nobody enumerated —
    /// but the files it described are in there anyway, because they moved.
    #[test]
    fn the_ground_under_a_task_is_its_literals_plus_what_moved() {
        let db = db();
        let seq = db.tasks().create(PROJECT, "t", "", 0, "cli").unwrap().seq;
        db.scopes()
            .declare(
                seq,
                &["src/config.rs".to_string(), "tests/**".to_string()],
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(seq, "codex:9f2c", TTL).unwrap();
        db.witnessed()
            .begin(seq, &Tree::default(), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[
                    Change {
                        path: "tests/config.rs".into(),
                        kind: ChangeKind::Modified,
                        hash: "h1".into(),
                    },
                    Change {
                        path: "src/db.rs".into(),
                        kind: ChangeKind::Modified,
                        hash: "h2".into(),
                    },
                ],
                "codex:9f2c",
            )
            .unwrap();

        assert_eq!(
            db.footings().ground_for_task(seq).unwrap(),
            vec!["src/config.rs", "src/db.rs", "tests/config.rs"]
        );
    }

    #[test]
    fn affirmations_count_voices_and_not_sentences() {
        let db = db();
        let a = store(&db, "the loader reads env first", None);
        assert_eq!(db.footings().voices(&a).unwrap().actors, vec!["codex:9f2c"]);
        assert!(db.footings().voices(&a).unwrap().describe().is_none());

        // The author saying it again adds nothing.
        db.footings().affirm(&a.id, "codex:9f2c").unwrap();
        assert_eq!(db.footings().voices(&a).unwrap().actors.len(), 1);

        db.footings().affirm(&a.id, "claude-code:af31").unwrap();
        db.footings().affirm(&a.id, "claude-code:af31").unwrap();
        let voices = db.footings().voices(&a).unwrap();
        assert_eq!(voices.actors, vec!["codex:9f2c", "claude-code:af31"]);
        assert_eq!(voices.harnesses(), vec!["codex", "claude-code"]);
        assert_eq!(
            voices.describe().unwrap(),
            "also stated by claude-code:af31, independently across 2 harnesses"
        );
    }

    #[test]
    fn the_audit_lists_anchored_current_assertions_oldest_first() {
        let db = db();
        let old = store(&db, "older fact", None);
        let new = store(&db, "newer fact", None);
        let loose = store(&db, "never anchored", None);
        let dead = store(&db, "will be retracted", None);
        for a in [&old, &new, &dead] {
            db.footings()
                .anchor(&a.id, &[anchor("src/a.rs", "h1")], Anchoring::Derived)
                .unwrap();
        }
        db.memory()
            .supersede(&dead.id, "not any more", "cli")
            .unwrap();

        let listed: Vec<String> = db
            .footings()
            .anchored(&ProjectScope::Only(PROJECT.into()))
            .unwrap()
            .into_iter()
            .map(|a| a.content)
            .collect();
        assert_eq!(listed, vec!["older fact", "newer fact"]);
        assert!(!listed.contains(&loose.content));
    }

    #[test]
    fn what_a_task_learned_excludes_what_has_been_retracted() {
        let db = db();
        let seq = db.tasks().create(PROJECT, "t", "", 0, "cli").unwrap().seq;
        store(&db, "still true", Some(seq));
        let wrong = store(&db, "turned out wrong", Some(seq));
        db.memory()
            .supersede(&wrong.id, "the opposite", "cli")
            .unwrap();

        let learned: Vec<String> = db
            .footings()
            .learned_on(seq)
            .unwrap()
            .into_iter()
            .map(|a| a.content)
            .collect();
        assert_eq!(learned, vec!["still true", "the opposite"]);
    }
}
