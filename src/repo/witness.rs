//! What the witness saw, and the contention detector built on it.
//!
//! Two tables. `task_witness` holds the working-tree fingerprint a task is
//! measured against, taken when it was claimed and never moved after that.
//! `task_changes` holds the difference between that fingerprint and the tree as
//! it stands — one row per path, rewritten on every observation, so a file
//! edited and then put back the way it was leaves no row behind.
//!
//! The `hash` on a change row is not simply "the content now". It is the last
//! version this task's own holder was *shown*, and that distinction is the
//! whole contention detector. Anybody may look — another agent's check-in, the
//! human's board — and looking updates the footprint without touching the
//! hashes; only [`Witnessed::confirm`], called on a holder's own check-in after
//! it has been handed the report, moves them. So when two rows for one path
//! disagree, the older one belongs to an agent whose copy of that file is out
//! of date, and a write from it would silently discard the other's work.
//!
//! What the footprint cannot supply on its own is *whose* change it was — a
//! shared checkout has one filesystem and no keyboards — so every live task
//! carries every change made while it was live. The declared scopes are the
//! other half: [`Witnessed::contention`] fires only where two live tasks both
//! said they would write a file and the file then moved, which is the predicted
//! collision and the observed one at the same time.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, Row};

use super::deps::id_for_seq;
use super::{new_id, ProjectScope};
use crate::error::Result;
use crate::glob;
use crate::model::{now_ts, Contention, EventKind, Footprint, Observed, Status, WitnessedTask};
use crate::witness::{Change, Tree};

/// A live task and the tree it is measured against.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub seq: i64,
    pub task_id: String,
    /// The tree as it stood when the task was claimed.
    pub tree: Tree,
}

/// Repository over `task_witness` and `task_changes`.
pub struct Witnessed<'a> {
    conn: &'a Connection,
}

impl<'a> Witnessed<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Witnessed<'a> {
        Witnessed { conn }
    }

    /// Start measuring task `seq` against `tree`, held by `holder`.
    ///
    /// Called once, when the task is claimed. Re-claiming after a lease lapse
    /// starts a fresh measurement: the previous holder's footprint is not the
    /// new holder's doing, and keeping it would blame them for it. But the
    /// evidence is not destroyed — it is archived as a finished *tenure*, in
    /// the same transaction that replaces it, because the moment a task
    /// changes hands is exactly the moment its history starts to matter: the
    /// tree the new baseline was just read off may be carrying whatever the
    /// previous holder left uncommitted, and this record is the only account
    /// of what that was.
    pub fn begin(&self, seq: i64, tree: &Tree, holder: &str) -> Result<()> {
        let tx = super::immediate_tx(self.conn)?;
        let task_id = id_for_seq(&tx, seq)?;
        let now = now_ts();
        archive_tenure(&tx, &task_id, &now)?;
        let encoded = encode(tree);
        tx.execute("DELETE FROM task_changes WHERE task_id = ?1", [&task_id])?;
        tx.execute(
            "INSERT INTO task_witness (task_id, head, tree, at, holder)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(task_id) DO UPDATE SET
               head = excluded.head, tree = excluded.tree, at = excluded.at,
               holder = excluded.holder",
            params![task_id, tree.head, encoded, now, holder],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The finished holdings of task `seq`, oldest first, each with what the
    /// witness saw move while it was live.
    pub fn tenures(&self, seq: i64) -> Result<Vec<crate::model::Tenure>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let mut stmt = self.conn.prepare(
            "SELECT id, n, holder, began_at, ended, ended_at FROM task_tenures
             WHERE task_id = ?1 ORDER BY n",
        )?;
        let rows = stmt.query_map([&task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                crate::model::Tenure {
                    n: row.get(1)?,
                    holder: row.get(2)?,
                    began_at: row.get(3)?,
                    ended: row.get(4)?,
                    ended_at: row.get(5)?,
                    changes: Vec::new(),
                },
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (tenure_id, mut tenure) = row?;
            let mut stmt = self.conn.prepare(
                "SELECT path, kind, hash, first_seen, last_seen FROM tenure_changes
                 WHERE tenure_id = ?1 ORDER BY path",
            )?;
            let changes = stmt.query_map([&tenure_id], row_to_observed)?;
            tenure.changes = changes.collect::<rusqlite::Result<Vec<_>>>()?;
            out.push(tenure);
        }
        Ok(out)
    }

    /// The baseline tenure `n` of task `seq` was measured against, if that
    /// round was archived. This is what lets `hird diff --tenure` resolve the
    /// "before" side of a finished holding the same way it does for the
    /// current one.
    pub fn tenure_baseline(&self, seq: i64, n: i64) -> Result<Option<Baseline>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT t.seq, w.task_id, w.head, w.tree
                 FROM task_tenures w JOIN tasks t ON t.id = w.task_id
                 WHERE t.seq = ?1 AND w.n = ?2",
                params![seq, n],
                |row| {
                    Ok(Baseline {
                        seq: row.get(0)?,
                        task_id: row.get(1)?,
                        tree: decode(row.get::<_, String>(2)?, row.get::<_, String>(3)?),
                    })
                },
            )
            .optional()?)
    }

    /// Every task in `project` that currently holds a lease and has a baseline.
    ///
    /// This is what an observation needs before it takes place: the union of
    /// these trees' paths is what has to be looked at, and their commits are
    /// what a `HEAD` that has moved is compared against.
    pub fn baselines(&self, project: &str) -> Result<Vec<Baseline>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.seq, w.task_id, w.head, w.tree
             FROM task_witness w JOIN tasks t ON t.id = w.task_id
             WHERE t.project = ?1 AND t.status IN ('claimed','in_progress')
             ORDER BY t.seq",
        )?;
        let rows = stmt.query_map([project], |row| {
            Ok(Baseline {
                seq: row.get(0)?,
                task_id: row.get(1)?,
                tree: decode(row.get::<_, String>(2)?, row.get::<_, String>(3)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace what task `seq` is on record as having seen change.
    ///
    /// `changes` is the whole difference from the claim fingerprint, so a path
    /// that has dropped out of it has gone back to the way it started and its
    /// row goes with it. Paths appearing for the first time are returned, and
    /// only those, so the caller can log the new ones without writing an event
    /// on every heartbeat.
    ///
    /// Looking never counts as being told. A row already on file keeps the
    /// content it was last *confirmed* at, however many times anybody observes
    /// it in the meantime, and only [`Witnessed::confirm`] moves it — which is
    /// what lets [`Witnessed::contention`] tell a stale copy from a current
    /// one. A path seen here for the first time is recorded at what it says
    /// now, because there is no earlier version to preserve.
    pub fn record(&self, seq: i64, changes: &[Change], actor: &str) -> Result<Vec<String>> {
        let tx = super::immediate_tx(self.conn)?;
        let task_id = id_for_seq(&tx, seq)?;
        let now = now_ts();

        let mut known: Vec<String> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT path FROM task_changes WHERE task_id = ?1")?;
            let rows = stmt.query_map([&task_id], |row| row.get::<_, String>(0))?;
            for path in rows {
                known.push(path?);
            }
        }

        let mut fresh: Vec<String> = Vec::new();
        for change in changes {
            let kind = change.kind.as_str();
            if known.iter().any(|p| p == &change.path) {
                tx.execute(
                    "UPDATE task_changes SET kind = ?3 WHERE task_id = ?1 AND path = ?2",
                    params![task_id, change.path, kind],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO task_changes
                       (id, task_id, path, kind, hash, first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![new_id(), task_id, change.path, kind, change.hash, now],
                )?;
                fresh.push(change.path.clone());
            }
        }

        for gone in known
            .iter()
            .filter(|path| !changes.iter().any(|c| &c.path == *path))
        {
            tx.execute(
                "DELETE FROM task_changes WHERE task_id = ?1 AND path = ?2",
                params![task_id, gone],
            )?;
        }

        if !fresh.is_empty() {
            super::tasks::insert_event(
                &tx,
                &task_id,
                &now,
                actor,
                EventKind::Witnessed,
                // "saw … change", not "changed …": the actor column on an
                // event is who made the call, and the whole point of this one
                // is that hird does not know who typed. A sweep triggered by
                // one agent writes this onto every live task, so the sentence
                // has to stay true when the two are different people.
                &format!("saw {} change", fresh.join(", ")),
            )?;
        }
        tx.commit()?;
        Ok(fresh)
    }

    /// Record that task `seq`'s holder has now been shown these versions.
    ///
    /// Called after the holder has been handed the report and not before: an
    /// agent that is told "this file moved under you" in the same breath as
    /// having its copy marked current has been told nothing at all. So a
    /// check-in reads the evidence first, and confirms second.
    pub fn confirm(&self, seq: i64, changes: &[Change]) -> Result<()> {
        let tx = super::immediate_tx(self.conn)?;
        let task_id = id_for_seq(&tx, seq)?;
        let now = now_ts();
        for change in changes {
            tx.execute(
                "UPDATE task_changes SET hash = ?3, last_seen = ?4
                 WHERE task_id = ?1 AND path = ?2",
                params![task_id, change.path, change.hash, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// What the witness saw change while task `seq` was held.
    pub fn touched(&self, seq: i64) -> Result<Vec<Observed>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let mut stmt = self.conn.prepare(
            "SELECT path, kind, hash, first_seen, last_seen FROM task_changes
             WHERE task_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map([&task_id], row_to_observed)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether task `seq` left a mark on the working tree, or only read it.
    ///
    /// [`Witnessed::touched`] already says *what* moved, and says nothing at
    /// all in the two cases that matter here: a task that wrote nothing and a
    /// task nobody watched both come back with an empty list. The difference
    /// is the whole answer to "did this change anything?", so it is read off
    /// the baseline rather than off the change rows — a baseline exists
    /// exactly when hird was in a position to know.
    pub fn footprint(&self, seq: i64) -> Result<Footprint> {
        let task_id = id_for_seq(self.conn, seq)?;
        let watched: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM task_witness WHERE task_id = ?1)",
            [&task_id],
            |row| row.get(0),
        )?;
        if !watched {
            return Ok(Footprint::Unwatched);
        }
        let paths: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM task_changes WHERE task_id = ?1",
            [&task_id],
            |row| row.get(0),
        )?;
        if paths == 0 {
            return Ok(Footprint::ReadOnly);
        }
        // A path in two footprints is a path that moved while both tasks were
        // live, which is the one thing that stops a count being an account of
        // what this task did.
        let alongside: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM task_changes mine
             JOIN task_changes theirs ON theirs.path = mine.path
                                     AND theirs.task_id <> mine.task_id
             JOIN tasks t ON t.id = theirs.task_id
             WHERE mine.task_id = ?1
               AND t.project = (SELECT project FROM tasks WHERE id = ?1)",
            [&task_id],
            |row| row.get(0),
        )?;
        Ok(Footprint::Modified {
            paths: paths as usize,
            shared: alongside > 0,
        })
    }

    /// The same question for every task in `scope`, in one pass.
    ///
    /// Two queries and a fold rather than the per-task version run in a loop:
    /// the board asks this of every card it paints, twice a second. Tasks the
    /// witness never watched are absent from the map, which reads the same as
    /// [`Footprint::Unwatched`] to every caller.
    pub fn footprints(&self, scope: &ProjectScope) -> Result<BTreeMap<i64, Footprint>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let binds: Vec<&str> = project_value.into_iter().collect();

        let sql = format!(
            "SELECT t.seq FROM task_witness w JOIN tasks t ON t.id = w.task_id
             WHERE {project_clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let watched = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            row.get::<_, i64>(0)
        })?;
        let mut out: BTreeMap<i64, Footprint> = BTreeMap::new();
        for seq in watched {
            out.insert(seq?, Footprint::ReadOnly);
        }

        let sql = format!(
            "SELECT t.seq, t.project, c.path FROM task_changes c JOIN tasks t ON t.id = c.task_id
             WHERE {project_clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let changed = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        // One row per task per path, so counting rows for a path counts the
        // tasks that hold it.
        let mut holders: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for (_, project, path) in &changed {
            *holders
                .entry((project.as_str(), path.as_str()))
                .or_default() += 1;
        }
        let mut tally: BTreeMap<i64, (usize, bool)> = BTreeMap::new();
        for (seq, project, path) in &changed {
            let shared = holders
                .get(&(project.as_str(), path.as_str()))
                .is_some_and(|held| *held > 1);
            let entry = tally.entry(*seq).or_insert((0, false));
            entry.0 += 1;
            entry.1 |= shared;
        }
        for (seq, (paths, shared)) in tally {
            out.insert(seq, Footprint::Modified { paths, shared });
        }
        Ok(out)
    }

    /// Files two live tasks both said they would write, and that have since
    /// moved under both of them.
    ///
    /// This is the intersection of hird's two detectors, and it is deliberately
    /// narrower than either. A shared checkout cannot tell you who typed: when
    /// a file changes, every task that was live at the time has it in its
    /// footprint, so the footprint alone would accuse everybody of everything.
    /// A declaration is what supplies the missing half — an agent that said
    /// "I am going to write `src/config.rs`" has told you it holds a copy and
    /// intends to write from it, and when that file then moves, its copy is the
    /// one about to be written over the other agent's work.
    ///
    /// So: a predicted collision that the filesystem has confirmed. A task that
    /// declared nothing contends with nobody, which is one more reason to
    /// declare early and the only one that costs an agent its own work.
    ///
    /// Sweep leases before calling: a task whose lease has lapsed is not
    /// somebody else's problem any more.
    pub fn contention(&self, seq: i64) -> Result<Vec<Contention>> {
        let task_id = id_for_seq(self.conn, seq)?;
        let mine = super::scope::Scopes::new(self.conn).for_task(seq)?;
        if mine.is_empty() {
            return Ok(Vec::new());
        }
        let project: String = self.conn.query_row(
            "SELECT project FROM tasks WHERE id = ?1",
            [&task_id],
            |row| row.get(0),
        )?;
        let live_scopes =
            super::scope::Scopes::new(self.conn).declared(&ProjectScope::Only(project), true)?;

        let mut stmt = self.conn.prepare(
            "SELECT mine.path, mine.hash, mine.last_seen,
                    t.seq, t.title, t.status, t.claimed_by,
                    theirs.hash, theirs.last_seen
             FROM task_changes mine
             JOIN task_changes theirs ON theirs.path = mine.path
                                     AND theirs.task_id <> mine.task_id
             JOIN tasks t ON t.id = theirs.task_id
             WHERE mine.task_id = ?1
               AND t.status IN ('claimed','in_progress')
               AND t.project = (SELECT project FROM tasks WHERE id = ?1)
             ORDER BY mine.path, t.seq",
        )?;
        let rows = stmt.query_map([&task_id], |row| {
            let raw: String = row.get(5)?;
            let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(Contention {
                path: row.get(0)?,
                hash: row.get(1)?,
                last_seen: row.get(2)?,
                other_seq: row.get(3)?,
                other_title: row.get(4)?,
                other_status: status,
                other_holder: row.get(6)?,
                other_hash: row.get(7)?,
                other_last_seen: row.get(8)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            let found: Contention = row?;
            let theirs = live_scopes
                .iter()
                .find(|s| s.seq == found.other_seq)
                .map(|s| s.patterns.as_slice())
                .unwrap_or_default();
            let staked =
                |patterns: &[String]| patterns.iter().any(|p| glob::matches(p, &found.path));
            // Both must have a stake, and the two must actually disagree about
            // what the file says. Two agents in one file who have both been
            // shown the same version are not in trouble yet, and saying so on
            // every heartbeat would drown out the time they are.
            if found.is_stale() && staked(&mine) && staked(theirs) {
                out.push(found);
            }
        }
        Ok(out)
    }

    /// Paths task `seq` was seen to change that none of its declared patterns
    /// describes.
    ///
    /// Drift, in other words: the gap between what an agent said it would touch
    /// and where it actually went. Worth surfacing rather than papering over,
    /// because every other agent's collision check is reading the declaration.
    pub fn undeclared(&self, seq: i64) -> Result<Vec<String>> {
        let patterns = super::scope::Scopes::new(self.conn).for_task(seq)?;
        if patterns.is_empty() {
            // Nothing was declared, so nothing has drifted from it. An agent
            // that never declared a scope is a different complaint.
            return Ok(Vec::new());
        }
        Ok(self
            .touched(seq)?
            .into_iter()
            .map(|o| o.path)
            .filter(|path| !patterns.iter().any(|p| glob::matches(p, path)))
            .collect())
    }

    // ------------------------------------------------------------ the exhibit

    /// Keep these versions: one row per content hash, refreshed on re-sight.
    ///
    /// Content-addressed, so keeping the same version twice costs one UPDATE
    /// of a timestamp, and two files with identical content cost one row.
    pub fn keep(&self, blobs: &[(String, Vec<u8>)]) -> Result<()> {
        if blobs.is_empty() {
            return Ok(());
        }
        let tx = super::immediate_tx(self.conn)?;
        let now = now_ts();
        for (hash, content) in blobs {
            tx.execute(
                "INSERT INTO witness_blobs (hash, content, size, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(hash) DO UPDATE SET at = excluded.at",
                params![hash, content, content.len() as i64, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The content that hashes to `hash`, if the witness kept it.
    pub fn blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM witness_blobs WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Which of `hashes` have no kept content yet.
    ///
    /// Asked before reading files, so a sweep only re-reads the versions it
    /// has never seen — which after the first sighting is none of them.
    pub fn missing_blobs(&self, hashes: &[String]) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT EXISTS(SELECT 1 FROM witness_blobs WHERE hash = ?1)")?;
        let mut missing = Vec::new();
        for hash in hashes {
            let kept: bool = stmt.query_row([hash], |row| row.get(0))?;
            if !kept {
                missing.push(hash.clone());
            }
        }
        Ok(missing)
    }

    /// Drop kept versions nothing points at any more.
    ///
    /// A hash still on some task's change record — the current holding's or
    /// an archived tenure's — is somebody's evidence and stays whatever its
    /// age; everything else — baseline versions of work long finished,
    /// versions observed and then superseded — goes once it is older than the
    /// cutoff. Returns how many rows went.
    pub fn prune_blobs(&self, before: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM witness_blobs
             WHERE at < ?1
               AND hash NOT IN (SELECT hash FROM task_changes)
               AND hash NOT IN (SELECT hash FROM tenure_changes)",
            [before],
        )?)
    }

    /// The baseline task `seq` was measured against, whatever its status now.
    ///
    /// [`Witnessed::baselines`] answers for live tasks because that is what a
    /// sweep needs; this answers for one task after the fact, which is what
    /// reading its diff needs.
    pub fn baseline_of(&self, seq: i64) -> Result<Option<Baseline>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT t.seq, w.task_id, w.head, w.tree
                 FROM task_witness w JOIN tasks t ON t.id = w.task_id
                 WHERE t.seq = ?1",
                [seq],
                |row| {
                    Ok(Baseline {
                        seq: row.get(0)?,
                        task_id: row.get(1)?,
                        tree: decode(row.get::<_, String>(2)?, row.get::<_, String>(3)?),
                    })
                },
            )
            .optional()?)
    }

    /// Every task in `scope` the witness has something on, newest first.
    ///
    /// The radar's second data source: the TUI paints these next to what the
    /// same tasks declared, and the disagreements are the story.
    pub fn seen(&self, scope: &ProjectScope, active_only: bool) -> Result<Vec<WitnessedTask>> {
        let (project_clause, project_value) = scope.clause("t.project");
        let active = if active_only {
            "AND t.status IN ('claimed','in_progress')"
        } else {
            ""
        };
        let sql = format!(
            "SELECT t.seq, t.title, t.status, t.claimed_by,
                    c.path, c.kind, c.hash, c.first_seen, c.last_seen
             FROM task_changes c JOIN tasks t ON t.id = c.task_id
             WHERE {project_clause} {active}
             ORDER BY t.seq, c.path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let binds: Vec<&str> = project_value.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
            let raw: String = row.get(2)?;
            let status: Status = raw.parse().map_err(|e: crate::model::UnknownStatus| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let task = WitnessedTask {
                seq: row.get(0)?,
                title: row.get(1)?,
                status,
                holder: row.get(3)?,
                changes: Vec::new(),
            };
            let observed = Observed {
                path: row.get(4)?,
                kind: row.get(5)?,
                hash: row.get(6)?,
                first_seen: row.get(7)?,
                last_seen: row.get(8)?,
            };
            Ok((task, observed))
        })?;

        let mut out: Vec<WitnessedTask> = Vec::new();
        for row in rows {
            let (task, observed) = row?;
            match out.last_mut() {
                Some(last) if last.seq == task.seq => last.changes.push(observed),
                _ => out.push(WitnessedTask {
                    changes: vec![observed],
                    ..task
                }),
            }
        }
        Ok(out)
    }
}

// -------------------------------------------------------------------- helpers

/// Archive the baseline and footprint a fresh measurement is about to
/// replace, as one finished tenure. A task with no baseline has nothing to
/// archive, which is the ordinary first claim.
///
/// How the holding ended is read from the event trail rather than stored as
/// it happens: the ending transitions are many and this is the one place the
/// answer is needed. The new claim's own `claimed` event is already on the
/// trail by now, but `claimed` is not an ending, so the newest ending event
/// since the baseline was taken is exactly the one that ended this holding.
fn archive_tenure(tx: &Connection, task_id: &str, now: &str) -> Result<()> {
    use rusqlite::OptionalExtension;
    let Some((head, tree, began_at, holder)) = tx
        .query_row(
            "SELECT head, tree, at, holder FROM task_witness WHERE task_id = ?1",
            [task_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(());
    };
    let (ended, ended_at) = tx
        .query_row(
            "SELECT kind, at FROM task_events
             WHERE task_id = ?1
               AND kind IN ('completed','failed','released','lease_expired','cancelled')
               AND at >= ?2
             ORDER BY at DESC, id DESC LIMIT 1",
            params![task_id, began_at],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    let n: i64 = tx.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM task_tenures WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    let tenure_id = new_id();
    tx.execute(
        "INSERT INTO task_tenures
           (id, task_id, n, holder, began_at, ended, ended_at, head, tree, archived_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![tenure_id, task_id, n, holder, began_at, ended, ended_at, head, tree, now],
    )?;
    tx.execute(
        "INSERT INTO tenure_changes (tenure_id, path, kind, hash, first_seen, last_seen)
         SELECT ?1, path, kind, hash, first_seen, last_seen
         FROM task_changes WHERE task_id = ?2",
        params![tenure_id, task_id],
    )?;
    Ok(())
}

fn row_to_observed(row: &Row<'_>) -> rusqlite::Result<Observed> {
    Ok(Observed {
        path: row.get(0)?,
        kind: row.get(1)?,
        hash: row.get(2)?,
        first_seen: row.get(3)?,
        last_seen: row.get(4)?,
    })
}

/// Trees are stored as one JSON object of path to hash.
///
/// Opaque to SQL on purpose: nothing ever queries inside a fingerprint, it is
/// only ever loaded whole and subtracted from another one. `task_changes` is
/// where the queryable half lives.
fn encode(tree: &Tree) -> String {
    serde_json::to_string(&tree.entries).unwrap_or_else(|_| "{}".to_string())
}

fn decode(head: String, entries: String) -> Tree {
    Tree {
        head,
        entries: serde_json::from_str(&entries).unwrap_or_default(),
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::witness::ChangeKind;
    use std::collections::BTreeMap;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seed(db: &Db, title: &str, holder: &str) -> i64 {
        let seq = db.tasks().create(PROJECT, title, "", 0, "cli").unwrap().seq;
        db.tasks().claim(seq, holder, TTL).unwrap();
        seq
    }

    fn tree(entries: &[(&str, &str)]) -> Tree {
        Tree {
            head: "abc123".to_string(),
            entries: entries
                .iter()
                .map(|(p, h)| (p.to_string(), h.to_string()))
                .collect::<BTreeMap<_, _>>(),
            truncated: false,
        }
    }

    /// Say a task will touch `patterns`, the way an agent does on claiming.
    fn declare(db: &Db, seq: i64, patterns: &[&str]) {
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        db.scopes()
            .declare(seq, &owned, "cli", super::super::OnConflict::Report)
            .unwrap();
    }

    fn change(path: &str, hash: &str) -> Change {
        Change {
            path: path.to_string(),
            kind: ChangeKind::Modified,
            hash: hash.to_string(),
        }
    }

    #[test]
    fn a_baseline_round_trips_through_the_database() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        let snapshot = tree(&[("src/a.rs", "hash-a"), ("src/b.rs", "")]);
        db.witnessed().begin(seq, &snapshot, "codex:9f2c").unwrap();

        let live = db.witnessed().baselines(PROJECT).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].seq, seq);
        assert_eq!(live[0].tree, snapshot);
    }

    #[test]
    fn only_live_tasks_have_baselines_to_measure() {
        let db = db();
        let live = seed(&db, "live", "codex:9f2c");
        let finished = seed(&db, "finished", "codex:9f2c");
        db.witnessed()
            .begin(live, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .begin(finished, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.tasks().complete(finished, "codex:9f2c", "done").unwrap();

        let seqs: Vec<i64> = db
            .witnessed()
            .baselines(PROJECT)
            .unwrap()
            .into_iter()
            .map(|b| b.seq)
            .collect();
        assert_eq!(seqs, vec![live]);
    }

    #[test]
    fn recording_reports_only_paths_it_had_not_seen_before() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();

        let first = db
            .witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert_eq!(first, vec!["src/a.rs"]);

        // Same path, new content: not a new path, so no second event.
        let moved = [change("src/a.rs", "h2")];
        let again = db.witnessed().record(seq, &moved, "codex:9f2c").unwrap();
        assert!(again.is_empty());
        db.witnessed().confirm(seq, &moved).unwrap();

        let wider = db
            .witnessed()
            .record(
                seq,
                &[change("src/a.rs", "h2"), change("src/b.rs", "h3")],
                "codex:9f2c",
            )
            .unwrap();
        assert_eq!(wider, vec!["src/b.rs"]);

        let touched = db.witnessed().touched(seq).unwrap();
        assert_eq!(touched.len(), 2);
        assert_eq!(touched[0].path, "src/a.rs");
        assert_eq!(touched[0].hash, "h2");
        assert!(touched[0].first_seen <= touched[0].last_seen, "{touched:?}");
    }

    /// A file edited and then put back the way it was is not a change, and the
    /// record has to be able to say so — otherwise the contention detector
    /// keeps firing on work that was undone.
    #[test]
    fn a_path_that_goes_back_to_normal_leaves_no_row() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert_eq!(db.witnessed().touched(seq).unwrap().len(), 1);

        db.witnessed().record(seq, &[], "codex:9f2c").unwrap();
        assert!(db.witnessed().touched(seq).unwrap().is_empty());
    }

    #[test]
    fn the_first_sighting_of_a_path_is_written_into_the_history() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();

        let task = db.tasks().get(seq).unwrap();
        let events = db.tasks().events(&task.id, 20).unwrap();
        let witnessed: Vec<&crate::model::TaskEvent> = events
            .iter()
            .filter(|e| e.kind == EventKind::Witnessed)
            .collect();
        assert_eq!(witnessed.len(), 1);
        assert_eq!(witnessed[0].detail, "saw src/a.rs change");
        // The actor is whoever asked for the observation, which on somebody
        // else's sweep is not whoever made the edit. The wording has to hold
        // either way.
        assert_eq!(witnessed[0].actor, "codex:9f2c");
    }

    /// Looking is not telling. However many times the tree is observed, an
    /// agent's recorded version of a file only moves when that agent has
    /// actually been handed the new one.
    #[test]
    fn observing_does_not_confirm_a_version_but_confirming_does() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "mine")], "codex:9f2c")
            .unwrap();
        let first = db.witnessed().touched(seq).unwrap()[0].clone();

        let moved = [change("src/a.rs", "somebody-elses")];
        db.witnessed().record(seq, &moved, "tui").unwrap();
        let after = db.witnessed().touched(seq).unwrap()[0].clone();
        assert_eq!(after.hash, "mine", "looking cannot confirm a version");
        assert_eq!(after.last_seen, first.last_seen);

        db.witnessed().confirm(seq, &moved).unwrap();
        assert_eq!(
            db.witnessed().touched(seq).unwrap()[0].hash,
            "somebody-elses",
            "being shown the new version is what catches an agent up"
        );
    }

    #[test]
    fn two_live_tasks_in_one_file_are_a_contention() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        db.witnessed()
            .begin(mine, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .begin(theirs, &tree(&[]), "codex:9f2c")
            .unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, theirs, &["src/*.rs"]);

        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "claude-code:af31")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "h2")], "codex:9f2c")
            .unwrap();

        let seen = db.witnessed().contention(mine).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "src/shared.rs");
        assert_eq!(seen[0].other_seq, theirs);
        assert_eq!(seen[0].other_holder.as_deref(), Some("codex:9f2c"));
        assert!(seen[0].is_stale(), "the hashes differ, so one side is old");

        let sentence = seen[0].describe();
        assert!(sentence.contains("src/shared.rs"), "{sentence}");
        assert!(sentence.contains("codex:9f2c"), "{sentence}");
        assert!(sentence.contains("re-read"), "{sentence}");
    }

    /// Two agents in one file who have both been shown the same version are
    /// not in trouble, and hearing about it on every heartbeat is how a
    /// warning stops being read.
    #[test]
    fn two_agents_who_agree_on_the_content_are_not_reported() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        db.witnessed()
            .begin(mine, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .begin(theirs, &tree(&[]), "codex:9f2c")
            .unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, theirs, &["src/shared.rs"]);
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "same")], "a")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "same")], "b")
            .unwrap();

        assert!(db.witnessed().contention(mine).unwrap().is_empty());

        // It becomes one the moment one of them is left behind.
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "moved on")], "b")
            .unwrap();
        db.witnessed()
            .confirm(theirs, &[change("src/shared.rs", "moved on")])
            .unwrap();
        let seen = db.witnessed().contention(mine).unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].is_stale());
    }

    #[test]
    fn a_finished_task_stops_contending() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        db.witnessed()
            .begin(mine, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .begin(theirs, &tree(&[]), "codex:9f2c")
            .unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, theirs, &["src/shared.rs"]);
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "a")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "h2")], "b")
            .unwrap();
        assert_eq!(db.witnessed().contention(mine).unwrap().len(), 1);

        db.tasks()
            .complete(theirs, "codex:9f2c", "shipped")
            .unwrap();
        assert!(db.witnessed().contention(mine).unwrap().is_empty());
    }

    #[test]
    fn tasks_in_other_projects_never_contend() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let elsewhere = db
            .tasks()
            .create("/other/project", "theirs", "", 0, "cli")
            .unwrap()
            .seq;
        db.tasks().claim(elsewhere, "codex:9f2c", TTL).unwrap();
        db.witnessed()
            .begin(mine, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .begin(elsewhere, &tree(&[]), "codex:9f2c")
            .unwrap();
        declare(&db, mine, &["src/shared.rs"]);
        declare(&db, elsewhere, &["src/shared.rs"]);
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "a")
            .unwrap();
        db.witnessed()
            .record(elsewhere, &[change("src/shared.rs", "h2")], "b")
            .unwrap();

        assert!(db.witnessed().contention(mine).unwrap().is_empty());
    }

    #[test]
    fn drift_is_the_gap_between_what_was_declared_and_what_moved() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/config.rs".to_string()],
                "codex:9f2c",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[change("src/config.rs", "h1"), change("src/mcp.rs", "h2")],
                "codex:9f2c",
            )
            .unwrap();

        assert_eq!(db.witnessed().undeclared(seq).unwrap(), vec!["src/mcp.rs"]);
    }

    #[test]
    fn a_glob_covers_everything_it_describes() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.scopes()
            .declare(
                seq,
                &["src/**".to_string()],
                "codex:9f2c",
                super::super::OnConflict::Report,
            )
            .unwrap();
        db.witnessed()
            .record(
                seq,
                &[change("src/repo/tasks.rs", "h1"), change("README.md", "h2")],
                "codex:9f2c",
            )
            .unwrap();

        assert_eq!(db.witnessed().undeclared(seq).unwrap(), vec!["README.md"]);
    }

    #[test]
    fn declaring_nothing_cannot_drift() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/anything.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert!(db.witnessed().undeclared(seq).unwrap().is_empty());
    }

    /// Re-claiming after a lease lapse must not hand the new holder the old
    /// holder's footprint — they did not do it, and they cannot answer for it.
    #[test]
    fn a_fresh_claim_starts_a_fresh_measurement() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        db.tasks()
            .release(seq, "codex:9f2c", "handing back")
            .unwrap();

        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[("src/a.rs", "h1")]), "claude-code:af31")
            .unwrap();
        assert!(db.witnessed().touched(seq).unwrap().is_empty());
    }

    /// The fresh measurement does not destroy the old one: the previous
    /// holding is archived — who held it, how it ended, what moved — in the
    /// same transaction that replaces it. This is the record the successor's
    /// claim answer is written from.
    #[test]
    fn a_fresh_claim_archives_the_previous_holding_as_a_tenure() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        assert!(db.witnessed().tenures(seq).unwrap().is_empty());

        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        db.tasks()
            .release(seq, "codex:9f2c", "handing back")
            .unwrap();
        // Nothing is archived until somebody replaces the measurement: the
        // released task's footprint is still the current record.
        assert!(db.witnessed().tenures(seq).unwrap().is_empty());

        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[]), "claude-code:af31")
            .unwrap();

        let held = db.witnessed().tenures(seq).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].n, 1);
        assert_eq!(held[0].holder, "codex:9f2c");
        assert_eq!(held[0].ended, "released");
        assert!(!held[0].ended_at.is_empty());
        assert_eq!(held[0].changes.len(), 1);
        assert_eq!(held[0].changes[0].path, "src/a.rs");
        assert_eq!(held[0].changes[0].hash, "h1");

        let sentence = held[0].describe(seq);
        assert!(sentence.contains("codex:9f2c"), "{sentence}");
        assert!(sentence.contains("src/a.rs"), "{sentence}");
        assert!(sentence.contains("handed it back"), "{sentence}");
        assert!(
            sentence.contains(&format!("hird diff {seq} --tenure 1")),
            "{sentence}"
        );
    }

    /// Each hand-over is its own round, its changes frozen at the moment the
    /// next claim replaced them, and the current holding is never in the list.
    #[test]
    fn rounds_count_up_and_each_keeps_its_own_changes() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        db.tasks().release(seq, "codex:9f2c", "first").unwrap();

        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[]), "claude-code:af31")
            .unwrap();
        db.witnessed()
            .record(seq, &[change("src/b.rs", "h2")], "claude-code:af31")
            .unwrap();
        db.tasks()
            .complete(seq, "claude-code:af31", "done")
            .unwrap();
        db.tasks().reopen(seq, "cli", "look again").unwrap();

        db.tasks().claim(seq, "copilot:11", TTL).unwrap();
        db.witnessed().begin(seq, &tree(&[]), "copilot:11").unwrap();

        let held = db.witnessed().tenures(seq).unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!((held[0].n, held[0].holder.as_str()), (1, "codex:9f2c"));
        assert_eq!(held[0].ended, "released");
        assert_eq!(held[0].changes[0].path, "src/a.rs");
        assert_eq!(
            (held[1].n, held[1].holder.as_str()),
            (2, "claude-code:af31")
        );
        // The holding ended with the completion; the human reopen that
        // followed is the task's business, not the tenure's.
        assert_eq!(held[1].ended, "completed");
        assert_eq!(held[1].changes[0].path, "src/b.rs");

        // The current holder's record is the live tables, not the archive.
        assert!(db.witnessed().touched(seq).unwrap().is_empty());
    }

    /// The tenure's founding scenario: a holder that vanishes. The lease
    /// lapses, the sweep returns the task, and the successor's archive says
    /// the holding ended with an expiry rather than by anybody's decision —
    /// which is exactly the difference between "they handed it back" and
    /// "they may have died mid-edit".
    #[test]
    fn a_vanished_holders_tenure_ends_with_the_lease_expiry() {
        let db = db();
        let seq = seed(&db, "t", "codex:dead");
        db.witnessed().begin(seq, &tree(&[]), "codex:dead").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:dead")
            .unwrap();
        let past = crate::model::fmt_ts(chrono::Utc::now() - chrono::Duration::hours(1));
        db.conn()
            .execute(
                "UPDATE tasks SET lease_expires_at = ?1 WHERE seq = ?2",
                params![past, seq],
            )
            .unwrap();
        db.tasks().sweep_leases().unwrap();

        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[]), "claude-code:af31")
            .unwrap();

        let held = db.witnessed().tenures(seq).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].holder, "codex:dead");
        assert_eq!(held[0].ended, "lease_expired");
        let sentence = held[0].describe(seq);
        assert!(sentence.contains("lease expired"), "{sentence}");
        assert!(sentence.contains("src/a.rs"), "{sentence}");
    }

    /// A holding that wrote nothing archives as read-only rather than not at
    /// all: "the previous attempt verifiably left nothing behind" is exactly
    /// what its successor wants to know.
    #[test]
    fn a_read_only_holding_is_archived_and_says_so() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.tasks().release(seq, "codex:9f2c", "nope").unwrap();
        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[]), "claude-code:af31")
            .unwrap();

        let held = db.witnessed().tenures(seq).unwrap();
        assert_eq!(held.len(), 1);
        assert!(held[0].changes.is_empty());
        let sentence = held[0].describe(seq);
        assert!(sentence.contains("no leftover edits"), "{sentence}");
    }

    /// The archived baseline is still resolvable, which is what `hird diff
    /// --tenure` stands on after the live baseline has been replaced.
    #[test]
    fn an_archived_rounds_baseline_is_still_resolvable() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        let snapshot = tree(&[("src/a.rs", "hash-at-claim")]);
        db.witnessed().begin(seq, &snapshot, "codex:9f2c").unwrap();
        db.tasks().release(seq, "codex:9f2c", "back").unwrap();
        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[]), "claude-code:af31")
            .unwrap();

        let archived = db.witnessed().tenure_baseline(seq, 1).unwrap().unwrap();
        assert_eq!(archived.tree, snapshot);
        assert!(db.witnessed().tenure_baseline(seq, 2).unwrap().is_none());
        assert!(db.witnessed().tenure_baseline(999, 1).unwrap().is_none());
    }

    /// A version named on an archived tenure is somebody's evidence: pruning
    /// must spare it exactly as it spares the live records'.
    #[test]
    fn pruning_spares_versions_on_an_archived_tenure() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h-tenure")], "codex:9f2c")
            .unwrap();
        db.witnessed()
            .keep(&[("h-tenure".into(), b"round one's work".to_vec())])
            .unwrap();
        db.tasks().release(seq, "codex:9f2c", "back").unwrap();
        db.tasks().claim(seq, "claude-code:af31", TTL).unwrap();
        db.witnessed()
            .begin(seq, &tree(&[]), "claude-code:af31")
            .unwrap();

        db.witnessed()
            .prune_blobs("9999-01-01T00:00:00.000Z")
            .unwrap();
        assert!(
            db.witnessed().blob("h-tenure").unwrap().is_some(),
            "an archived round's version is evidence, not housekeeping"
        );
    }

    /// The three answers, and the one that must never be guessed: a task
    /// nobody watched is not a task that changed nothing.
    #[test]
    fn a_task_reports_read_only_only_where_it_was_actually_watched() {
        let db = db();
        let unwatched = seed(&db, "never watched", "codex:9f2c");
        assert_eq!(
            db.witnessed().footprint(unwatched).unwrap(),
            Footprint::Unwatched
        );

        let seq = seed(&db, "read the config", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        assert_eq!(db.witnessed().footprint(seq).unwrap(), Footprint::ReadOnly);

        db.witnessed()
            .record(seq, &[change("src/a.rs", "h1")], "codex:9f2c")
            .unwrap();
        assert_eq!(
            db.witnessed().footprint(seq).unwrap(),
            Footprint::Modified {
                paths: 1,
                shared: false
            }
        );

        // Put back the way it was, so there is nothing left to have done.
        db.witnessed().record(seq, &[], "codex:9f2c").unwrap();
        assert_eq!(db.witnessed().footprint(seq).unwrap(), Footprint::ReadOnly);
    }

    /// One file in two footprints is one file that moved while both tasks were
    /// live. The count may not be read as an account of either of them.
    #[test]
    fn a_file_in_two_footprints_is_reported_as_shared() {
        let db = db();
        let mine = seed(&db, "mine", "claude-code:af31");
        let theirs = seed(&db, "theirs", "codex:9f2c");
        for seq in [mine, theirs] {
            db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        }
        db.witnessed()
            .record(mine, &[change("src/shared.rs", "h1")], "a")
            .unwrap();
        db.witnessed()
            .record(theirs, &[change("src/shared.rs", "h2")], "b")
            .unwrap();

        for seq in [mine, theirs] {
            assert_eq!(
                db.witnessed().footprint(seq).unwrap(),
                Footprint::Modified {
                    paths: 1,
                    shared: true
                }
            );
        }

        // A file only one of them ever saw is still that one's own.
        let solo = seed(&db, "solo", "copilot:11");
        db.witnessed()
            .begin(solo, &tree(&[]), "codex:9f2c")
            .unwrap();
        db.witnessed()
            .record(solo, &[change("docs/index.md", "h3")], "c")
            .unwrap();
        assert_eq!(
            db.witnessed().footprint(solo).unwrap(),
            Footprint::Modified {
                paths: 1,
                shared: false
            }
        );
    }

    /// The board asks this of every card it paints, so it has to come back in
    /// one pass — and it has to agree with the per-task answer exactly.
    #[test]
    fn the_batch_answer_matches_the_one_asked_task_by_task() {
        let db = db();
        let read_only = seed(&db, "read", "codex:9f2c");
        let wrote = seed(&db, "wrote", "claude-code:af31");
        let elsewhere = db
            .tasks()
            .create("/other/project", "not ours", "", 0, "cli")
            .unwrap()
            .seq;
        db.tasks().claim(elsewhere, "copilot:11", TTL).unwrap();
        for seq in [read_only, wrote, elsewhere] {
            db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        }
        db.witnessed()
            .record(
                wrote,
                &[change("src/a.rs", "h1"), change("src/b.rs", "h2")],
                "b",
            )
            .unwrap();
        // The same path in another project is another project's business.
        db.witnessed()
            .record(elsewhere, &[change("src/a.rs", "h9")], "c")
            .unwrap();
        // Finished work keeps its footprint: this is the report a human reads
        // after the fact, not a live gauge.
        db.tasks().complete(wrote, "claude-code:af31", "done").ok();

        let scope = ProjectScope::Only(PROJECT.into());
        let batch = db.witnessed().footprints(&scope).unwrap();
        assert_eq!(batch.get(&read_only), Some(&Footprint::ReadOnly));
        assert_eq!(
            batch.get(&wrote),
            Some(&Footprint::Modified {
                paths: 2,
                shared: false
            })
        );
        assert_eq!(batch.get(&elsewhere), None, "scoped to one project");
        for seq in [read_only, wrote] {
            assert_eq!(batch[&seq], db.witnessed().footprint(seq).unwrap());
        }

        // A task hird never watched is simply absent, which every reader
        // treats as the same "nothing to say" as `Unwatched`.
        let never = seed(&db, "never claimed under a witness", "codex:9f2c");
        assert_eq!(db.witnessed().footprints(&scope).unwrap().get(&never), None);
    }

    #[test]
    fn kept_versions_round_trip_and_deduplicate() {
        let db = db();
        assert_eq!(
            db.witnessed().missing_blobs(&["h1".into()]).unwrap(),
            vec!["h1".to_string()]
        );
        db.witnessed()
            .keep(&[("h1".into(), b"content".to_vec())])
            .unwrap();
        // Keeping the same version again is a refresh, not a duplicate.
        db.witnessed()
            .keep(&[("h1".into(), b"content".to_vec())])
            .unwrap();

        assert!(db
            .witnessed()
            .missing_blobs(&["h1".into()])
            .unwrap()
            .is_empty());
        assert_eq!(
            db.witnessed().blob("h1").unwrap().as_deref(),
            Some(b"content".as_slice())
        );
        assert_eq!(db.witnessed().blob("absent").unwrap(), None);
    }

    /// Pruning is housekeeping, not forgetting: a version still named on some
    /// task's change record is evidence and stays, whatever its age.
    #[test]
    fn pruning_spares_versions_still_on_a_record() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        db.witnessed().begin(seq, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(seq, &[change("src/a.rs", "h-referenced")], "codex:9f2c")
            .unwrap();
        db.witnessed()
            .keep(&[
                ("h-referenced".into(), b"kept".to_vec()),
                ("h-orphan".into(), b"orphan".to_vec()),
            ])
            .unwrap();

        // A cutoff after "now" ages everything, so only the reference saves a row.
        let gone = db
            .witnessed()
            .prune_blobs("9999-01-01T00:00:00.000Z")
            .unwrap();
        assert_eq!(gone, 1);
        assert!(db.witnessed().blob("h-referenced").unwrap().is_some());
        assert!(db.witnessed().blob("h-orphan").unwrap().is_none());
    }

    /// `hird diff` on a finished task needs the fingerprint it was measured
    /// against, which `baselines` — scoped to live tasks — no longer serves.
    #[test]
    fn the_baseline_is_readable_after_the_task_is_done() {
        let db = db();
        let seq = seed(&db, "t", "codex:9f2c");
        let snapshot = tree(&[("src/a.rs", "hash-a")]);
        db.witnessed().begin(seq, &snapshot, "codex:9f2c").unwrap();
        db.tasks().complete(seq, "codex:9f2c", "done").unwrap();

        assert!(db.witnessed().baselines(PROJECT).unwrap().is_empty());
        let found = db.witnessed().baseline_of(seq).unwrap().unwrap();
        assert_eq!(found.tree, snapshot);
        assert!(db.witnessed().baseline_of(999).unwrap().is_none());
    }

    #[test]
    fn the_radar_groups_changes_by_task() {
        let db = db();
        let a = seed(&db, "a", "codex:9f2c");
        let b = seed(&db, "b", "claude-code:af31");
        db.witnessed().begin(a, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed().begin(b, &tree(&[]), "codex:9f2c").unwrap();
        db.witnessed()
            .record(
                a,
                &[change("src/a.rs", "h1"), change("src/b.rs", "h2")],
                "a",
            )
            .unwrap();
        db.witnessed()
            .record(b, &[change("docs/index.html", "h3")], "b")
            .unwrap();
        db.tasks()
            .complete(b, "claude-code:af31", "shipped")
            .unwrap();

        let scope = ProjectScope::Only(PROJECT.into());
        let live = db.witnessed().seen(&scope, true).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].seq, a);
        assert_eq!(live[0].changes.len(), 2);

        let all = db.witnessed().seen(&scope, false).unwrap();
        assert_eq!(all.len(), 2);
    }
}
