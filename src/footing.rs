//! Reading the working tree on memory's behalf.
//!
//! [`crate::repo::footing`] holds the rows and the rule; this is the half that
//! touches disk. The split is the same one [`crate::witness`] draws for tasks,
//! and for the same reason: everything that decides anything stays testable
//! without a filesystem, and everything that needs a filesystem stays small
//! enough to read in one sitting.
//!
//! Every function here takes `Option<&Witness>` and does something sensible
//! with `None`. A project outside git, or one whose human turned witnessing
//! off, gets memory exactly as it behaved before this existed: assertions with
//! no footing, and no standing to report. That is not a degraded mode, it is
//! the old mode.

use std::collections::BTreeMap;

use crate::db::Db;
use crate::error::Result;
use crate::model::{now_ts, Anchor, Assertion, Standing};
use crate::repo::footing::{assess, paths_in};
use crate::repo::Recalled;
use crate::witness::Witness;

/// Fingerprint `paths` as they stand now and record them as what
/// `assertion_id` was read off.
///
/// Returns what was written, which is empty when there is no witness or the
/// caller named no files — both ordinary outcomes that leave the assertion
/// unanchored rather than failing anything.
pub fn anchor(
    db: &Db,
    witness: Option<&Witness>,
    assertion_id: &str,
    paths: &[String],
) -> Result<Vec<Anchor>> {
    let (Some(witness), false) = (witness, paths.is_empty()) else {
        return Ok(Vec::new());
    };
    let at = now_ts();
    let anchors: Vec<Anchor> = paths
        .iter()
        .map(|path| Anchor {
            hash: witness.fingerprint(path),
            path: path.clone(),
            at: at.clone(),
        })
        .collect();
    db.footings().anchor(assertion_id, &anchors)?;
    Ok(anchors)
}

/// Where a fact learned while working task `seq` should be anchored.
///
/// Falls back to nothing rather than to something plausible: an assertion with
/// no task and no explicit paths is a statement about the project in general,
/// and inventing a footing for it would be inventing a reason to distrust it
/// later.
pub fn ground(db: &Db, task_seq: Option<i64>, explicit: Option<&[String]>) -> Vec<String> {
    if let Some(paths) = explicit {
        let normalized: Vec<String> = paths
            .iter()
            .filter_map(|p| crate::glob::normalize(p))
            .filter(|p| crate::glob::is_literal(p))
            .collect();
        if !normalized.is_empty() {
            return dedup(normalized);
        }
    }
    task_seq
        .and_then(|seq| db.footings().ground_for_task(seq).ok())
        .unwrap_or_default()
}

/// The standing of one assertion.
pub fn standing(db: &Db, witness: Option<&Witness>, assertion_id: &str) -> Standing {
    let ids = vec![assertion_id.to_string()];
    standings(db, witness, &ids)
        .remove(assertion_id)
        .unwrap_or(Standing::Unanchored)
}

/// The standing of many assertions, reading each distinct file exactly once.
///
/// A search of twenty rows whose footings overlap costs one read per file, not
/// one per row per file. Never fails: a standing nobody can compute is
/// [`Standing::Unanchored`], which reports nothing, which is the correct thing
/// to say when you do not know.
pub fn standings(
    db: &Db,
    witness: Option<&Witness>,
    assertion_ids: &[String],
) -> BTreeMap<String, Standing> {
    let Some(witness) = witness else {
        return BTreeMap::new();
    };
    let Ok(footings) = db.footings().anchors_for(assertion_ids) else {
        return BTreeMap::new();
    };
    let now: BTreeMap<String, String> = paths_in(&footings)
        .into_iter()
        .map(|path| {
            let hash = witness.fingerprint(&path);
            (path, hash)
        })
        .collect();
    footings
        .into_iter()
        .map(|(id, anchors)| {
            let standing = assess(&anchors, |path| now.get(path).cloned().unwrap_or_default());
            (id, standing)
        })
        .collect()
}

/// Re-anchor everything task `seq` learned to the tree it is leaving behind.
///
/// A fact recorded in the third minute of a task is a statement about the code
/// as it was in the third minute, and by the time the task finishes its own
/// author has usually edited that code — so without this, a task's own work
/// would leave every fact it produced marked shaky the moment it was filed. The
/// settling is what makes the whole thing quiet enough to be worth reading:
/// after it, a shaky assertion means *somebody else* moved the ground, which is
/// the case worth a warning.
///
/// Best-effort in the strongest sense. It runs on the finishing call, and
/// nothing it does may turn a completed task into a failed one.
pub fn settle(db: &Db, witness: Option<&Witness>, seq: i64) -> usize {
    let Some(witness) = witness else {
        return 0;
    };
    let Ok(learned) = db.footings().learned_on(seq) else {
        return 0;
    };
    if learned.is_empty() {
        return 0;
    }
    let paths = db.footings().ground_for_task(seq).unwrap_or_default();
    if paths.is_empty() {
        return 0;
    }
    let mut settled = 0;
    for assertion in learned {
        // Only what this task itself is the footing for. An assertion the task
        // recorded but which was anchored somewhere else on purpose — an
        // explicit `paths` argument — keeps the footing its author chose.
        if anchor(db, Some(witness), &assertion.id, &paths).is_ok() {
            settled += 1;
        }
    }
    settled
}

/// Fill in the standing and corroboration of recalled assertions, and order
/// them so the ones hird can vouch for come first.
///
/// Recall picks *what* to surface from the file graph and the wording; this
/// decides what order to say it in and how much to hedge. The two are kept
/// apart because the first is a question about rows and the second is a
/// question about the disk, and only one of them can fail because git is not
/// installed.
///
/// The reason an assertion surfaced still outranks its standing — a shaky fact
/// learned in this very file is more use than a firm one that merely shares a
/// word with the title — so this is a tie-break, not a re-ranking.
pub fn decorate(db: &Db, witness: Option<&Witness>, mut recalled: Vec<Recalled>) -> Vec<Recalled> {
    if recalled.is_empty() {
        return recalled;
    }
    let ids: Vec<String> = recalled.iter().map(|r| r.assertion.id.clone()).collect();
    let mut standings = standings(db, witness, &ids);
    for row in &mut recalled {
        row.standing = standings.remove(&row.assertion.id);
        row.corroboration = corroboration(db, &row.assertion);
    }
    recalled.sort_by_key(|r| {
        (
            r.reason.strength(),
            r.standing.as_ref().map_or(1, Standing::rank),
        )
    });
    recalled
}

/// Everyone who has stated `assertion`, as a sentence, or nothing.
pub fn corroboration(db: &Db, assertion: &Assertion) -> Option<String> {
    db.footings()
        .voices(assertion)
        .ok()
        .and_then(|v| v.describe())
}

fn dedup(paths: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{NewAssertion, OnConflict};
    use std::path::Path;

    const PROJECT: &str = "/tmp/project";

    /// A real git checkout with real files, because everything in this module
    /// exists to read them.
    struct Checkout {
        dir: tempfile::TempDir,
    }

    impl Checkout {
        fn new() -> Option<Checkout> {
            let dir = tempfile::tempdir().ok()?;
            for args in [
                vec!["init", "-q"],
                vec!["config", "user.email", "t@example.com"],
                vec!["config", "user.name", "t"],
            ] {
                let ok = std::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir.path())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .ok()?
                    .success();
                if !ok {
                    return None;
                }
            }
            Some(Checkout { dir })
        }

        fn write(&self, path: &str, contents: &str) {
            let full = self.dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, contents).unwrap();
        }

        fn remove(&self, path: &str) {
            std::fs::remove_file(self.dir.path().join(path)).unwrap();
        }

        fn witness(&self) -> Witness {
            Witness::discover(self.dir.path()).expect("a fresh git checkout is witnessable")
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    fn db() -> Db {
        Db::open_in_memory().unwrap()
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

    #[test]
    fn a_fact_anchored_to_a_file_goes_shaky_when_the_file_moves() {
        let Some(checkout) = Checkout::new() else {
            return;
        };
        checkout.write("src/config.rs", "fn load() { env_first() }");
        let db = db();
        let witness = checkout.witness();
        let a = store(&db, "the loader reads env before the file", None);

        let anchors = anchor(&db, Some(&witness), &a.id, &["src/config.rs".to_string()]).unwrap();
        assert_eq!(anchors.len(), 1);
        assert!(!anchors[0].hash.is_empty());
        assert_eq!(
            standing(&db, Some(&witness), &a.id).as_str(),
            "firm",
            "nothing has happened yet"
        );

        checkout.write("src/config.rs", "fn load() { file_first() }");
        let moved = standing(&db, Some(&witness), &a.id);
        assert_eq!(moved.as_str(), "shaky");
        assert!(moved.needs_checking());
        assert!(moved.describe().unwrap().contains("src/config.rs"));

        checkout.remove("src/config.rs");
        assert_eq!(standing(&db, Some(&witness), &a.id).as_str(), "orphaned");
    }

    #[test]
    fn restating_a_shaky_fact_re_anchors_it_and_records_the_second_voice() {
        let Some(checkout) = Checkout::new() else {
            return;
        };
        checkout.write("src/config.rs", "v1");
        let db = db();
        let witness = checkout.witness();

        let first = db
            .memory()
            .record(NewAssertion {
                project: PROJECT,
                content: "the loader reads env before the file",
                tags: "",
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap();
        assert!(!first.was_affirmed());
        let id = first.assertion().id.clone();
        anchor(&db, Some(&witness), &id, &["src/config.rs".to_string()]).unwrap();

        checkout.write("src/config.rs", "v2");
        assert_eq!(standing(&db, Some(&witness), &id).as_str(), "shaky");

        // Another agent, in another harness, checks and finds it still true.
        let again = db
            .memory()
            .record(NewAssertion {
                project: PROJECT,
                content: "  the loader reads env before the file  ",
                tags: "",
                actor: "claude-code:af31",
                task_seq: None,
            })
            .unwrap();
        assert!(
            again.was_affirmed(),
            "an exact restatement is not a new fact"
        );
        assert_eq!(again.assertion().id, id, "and it keeps its provenance");
        anchor(&db, Some(&witness), &id, &["src/config.rs".to_string()]).unwrap();

        assert_eq!(standing(&db, Some(&witness), &id).as_str(), "firm");
        let sentence = corroboration(&db, again.assertion()).unwrap();
        assert!(sentence.contains("2 harnesses"), "{sentence}");
    }

    /// The failure this feature would have had if `settle` did not exist: a
    /// task that records a fact and then keeps editing would leave every fact
    /// it produced marked shaky by its own hand.
    #[test]
    fn a_task_that_keeps_working_after_recording_a_fact_still_leaves_it_firm() {
        let Some(checkout) = Checkout::new() else {
            return;
        };
        checkout.write("src/config.rs", "v1");
        let db = db();
        let witness = checkout.witness();
        let project = checkout.path().to_string_lossy().to_string();

        let seq = db
            .tasks()
            .create(&project, "Port the loader", "", 0, "cli")
            .unwrap()
            .seq;
        db.scopes()
            .declare(
                seq,
                &["src/config.rs".to_string()],
                "cli",
                OnConflict::Report,
            )
            .unwrap();
        db.tasks()
            .claim(seq, "codex:9f2c", std::time::Duration::from_secs(900))
            .unwrap();

        let learned = db
            .memory()
            .store(NewAssertion {
                project: &project,
                content: "the loader reads env before the file",
                tags: "",
                actor: "codex:9f2c",
                task_seq: Some(seq),
            })
            .unwrap();
        anchor(
            &db,
            Some(&witness),
            &learned.id,
            &ground(&db, Some(seq), None),
        )
        .unwrap();

        // The agent carries on working the same file, as agents do.
        checkout.write("src/config.rs", "v2 — tidied up");
        assert_eq!(
            standing(&db, Some(&witness), &learned.id).as_str(),
            "shaky",
            "mid-flight, its own edits have moved the ground under it"
        );

        assert_eq!(settle(&db, Some(&witness), seq), 1);
        assert_eq!(
            standing(&db, Some(&witness), &learned.id).as_str(),
            "firm",
            "settling re-reads the tree the task is leaving behind"
        );
    }

    #[test]
    fn without_a_witness_nothing_is_anchored_and_nothing_is_claimed() {
        let db = db();
        let a = store(&db, "a fact", None);
        assert!(anchor(&db, None, &a.id, &["src/config.rs".into()])
            .unwrap()
            .is_empty());
        assert_eq!(standing(&db, None, &a.id), Standing::Unanchored);
        assert_eq!(settle(&db, None, 1), 0);
    }

    #[test]
    fn explicit_paths_win_over_the_task_and_globs_are_dropped() {
        let db = db();
        let seq = db.tasks().create(PROJECT, "t", "", 0, "cli").unwrap().seq;
        db.scopes()
            .declare(seq, &["src/db.rs".to_string()], "cli", OnConflict::Report)
            .unwrap();

        let explicit = ["./src/config.rs".to_string(), "src/**".to_string()];
        assert_eq!(
            ground(&db, Some(seq), Some(&explicit)),
            vec!["src/config.rs"],
            "normalized, and the glob is not guessed at"
        );
        // Nothing usable in the explicit list falls back to the task's ground.
        let all_globs = ["src/**".to_string()];
        assert_eq!(ground(&db, Some(seq), Some(&all_globs)), vec!["src/db.rs"]);
        assert!(ground(&db, None, None).is_empty());
    }
}
