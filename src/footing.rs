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
use crate::repo::footing::{assess, paths_in, Anchoring};
use crate::repo::Recalled;
use crate::witness::Witness;

/// The files an assertion should stand on, and who decided them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ground {
    pub paths: Vec<String>,
    pub by: Anchoring,
}

impl Ground {
    fn nothing() -> Ground {
        Ground {
            paths: Vec::new(),
            by: Anchoring::Derived,
        }
    }
}

/// Fingerprint `ground` as it stands now and record it as what `assertion_id`
/// was read off.
///
/// Returns what was written, which is empty in three ordinary cases: there is
/// no witness, there were no files, or the assertion already stands on a
/// footing its author named and this one was only derived. None of them is an
/// error; all of them leave the assertion exactly as it was.
pub fn anchor(
    db: &Db,
    witness: Option<&Witness>,
    assertion_id: &str,
    ground: &Ground,
) -> Result<Vec<Anchor>> {
    let (Some(witness), false) = (witness, ground.paths.is_empty()) else {
        return Ok(Vec::new());
    };
    let at = now_ts();
    let anchors: Vec<Anchor> = ground
        .paths
        .iter()
        .map(|path| Anchor {
            hash: witness.fingerprint(path),
            path: path.clone(),
            at: at.clone(),
        })
        .collect();
    if !db.footings().anchor(assertion_id, &anchors, ground.by)? {
        return Ok(Vec::new());
    }
    Ok(anchors)
}

/// Where a fact learned while working task `seq` should be anchored.
///
/// Falls back to nothing rather than to something plausible: an assertion with
/// no task and no named paths is a statement about the project in general, and
/// inventing a footing for it would be inventing a reason to distrust it later.
///
/// Globs in `named` are dropped rather than expanded — a glob describes a set
/// nobody has enumerated — and a `named` list with nothing usable left in it
/// falls through to the task, because refusing to anchor a fact over a typo
/// would be a strange way to punish the one agent that tried to be precise.
pub fn ground(db: &Db, task_seq: Option<i64>, named: Option<&[String]>) -> Ground {
    if let Some(paths) = named {
        let usable: Vec<String> = paths
            .iter()
            .filter_map(|p| crate::glob::normalize(p))
            .filter(|p| crate::glob::is_literal(p))
            .collect();
        if !usable.is_empty() {
            return Ground {
                paths: dedup(usable),
                by: Anchoring::Named,
            };
        }
    }
    let Some(seq) = task_seq else {
        return Ground::nothing();
    };
    match db.footings().ground_for_task(seq) {
        Ok(paths) => Ground {
            paths,
            by: Anchoring::Derived,
        },
        Err(_) => Ground::nothing(),
    }
}

/// The standing of one assertion, against `project`'s working tree.
pub fn standing(db: &Db, witness: Option<&Witness>, project: &str, assertion_id: &str) -> Standing {
    let ids = vec![assertion_id.to_string()];
    standings(db, witness, project, &ids)
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
    project: &str,
    assertion_ids: &[String],
) -> BTreeMap<String, Standing> {
    let Some(witness) = witness else {
        return BTreeMap::new();
    };
    // Scoped to one project, because an anchor is a path relative to its own
    // project root and `witness` can only answer for one tree. A cross-project
    // search gets standings for the rows it can vouch for and silence for the
    // rest, which is better than resolving another checkout's paths against
    // this one and calling the answer a fact.
    let Ok(footings) = db.footings().anchors_for(Some(project), assertion_ids) else {
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
    let ground = ground(db, Some(seq), None);
    if ground.paths.is_empty() {
        return 0;
    }
    // Only the footings hird worked out for itself get re-taken. A fact whose
    // author named its files said something specific, and a task tidying up
    // after itself is not entitled to overrule it — the fact may well be about
    // a file this task never went near. [`crate::repo::Footings::anchor`] is
    // the one place that rule is enforced, so an empty answer here means the
    // assertion was left alone and is not counted as settled.
    learned
        .into_iter()
        .filter(|a| {
            anchor(db, Some(witness), &a.id, &ground).is_ok_and(|written| !written.is_empty())
        })
        .count()
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
    // Recall never crosses projects, so every row belongs to the first one's.
    let project = recalled[0].assertion.project.clone();
    let mut standings = standings(db, witness, &project, &ids);
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

    /// A real git checkout with real files, because everything in this module
    /// exists to read them, plus the database that talks about it.
    ///
    /// The project *is* the checkout path throughout: a standing is only ever
    /// computed for assertions filed under the tree being measured, so a test
    /// that filed them anywhere else would be testing nothing.
    struct Sandbox {
        dir: tempfile::TempDir,
        db: Db,
    }

    impl Sandbox {
        /// `None` where git will not run, which is a supported outcome for the
        /// feature and therefore a supported outcome for its tests.
        fn new() -> Option<Sandbox> {
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
            Some(Sandbox {
                dir,
                db: Db::open_in_memory().ok()?,
            })
        }

        fn project(&self) -> String {
            self.dir.path().to_string_lossy().to_string()
        }

        fn witness(&self) -> Witness {
            Witness::discover(self.dir.path()).expect("a fresh git checkout is witnessable")
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

        fn store(&self, content: &str, task_seq: Option<i64>) -> Assertion {
            self.db
                .memory()
                .store(NewAssertion {
                    project: &self.project(),
                    content,
                    tags: "",
                    actor: "codex:9f2c",
                    task_seq,
                })
                .unwrap()
        }

        fn record(&self, content: &str, actor: &str) -> crate::repo::Recorded {
            self.db
                .memory()
                .record(NewAssertion {
                    project: &self.project(),
                    content,
                    tags: "",
                    actor,
                    task_seq: None,
                })
                .unwrap()
        }

        fn standing(&self, id: &str) -> Standing {
            standing(&self.db, Some(&self.witness()), &self.project(), id)
        }

        fn anchor_to(&self, id: &str, paths: &[&str]) -> Vec<Anchor> {
            let named: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            let ground = ground(&self.db, None, Some(&named));
            anchor(&self.db, Some(&self.witness()), id, &ground).unwrap()
        }
    }

    fn named(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn a_fact_anchored_to_a_file_goes_shaky_when_the_file_moves() {
        let Some(s) = Sandbox::new() else { return };
        s.write("src/config.rs", "fn load() { env_first() }");
        let a = s.store("the loader reads env before the file", None);

        let anchors = s.anchor_to(&a.id, &["src/config.rs"]);
        assert_eq!(anchors.len(), 1);
        assert!(!anchors[0].hash.is_empty());
        assert_eq!(
            s.standing(&a.id).as_str(),
            "firm",
            "nothing has happened yet"
        );

        s.write("src/config.rs", "fn load() { file_first() }");
        let moved = s.standing(&a.id);
        assert_eq!(moved.as_str(), "shaky");
        assert!(moved.needs_checking());
        assert!(moved.describe().unwrap().contains("src/config.rs"));

        s.remove("src/config.rs");
        assert_eq!(s.standing(&a.id).as_str(), "orphaned");
    }

    #[test]
    fn restating_a_shaky_fact_re_anchors_it_and_records_the_second_voice() {
        let Some(s) = Sandbox::new() else { return };
        s.write("src/config.rs", "v1");

        let first = s.record("the loader reads env before the file", "codex:9f2c");
        assert!(!first.was_affirmed());
        let id = first.assertion().id.clone();
        s.anchor_to(&id, &["src/config.rs"]);

        s.write("src/config.rs", "v2");
        assert_eq!(s.standing(&id).as_str(), "shaky");

        // Another agent, in another harness, checks and finds it still true.
        let again = s.record(
            "  the loader reads env before the file  ",
            "claude-code:af31",
        );
        assert!(
            again.was_affirmed(),
            "an exact restatement is not a new fact"
        );
        assert_eq!(again.assertion().id, id, "and it keeps its provenance");
        s.anchor_to(&id, &["src/config.rs"]);

        assert_eq!(s.standing(&id).as_str(), "firm");
        let sentence = corroboration(&s.db, again.assertion()).unwrap();
        assert!(
            sentence.contains("independently across 2 harnesses"),
            "{sentence}"
        );
    }

    /// The failure this feature would have had if `settle` did not exist: a
    /// task that records a fact and then keeps editing would leave every fact
    /// it produced marked shaky by its own hand.
    #[test]
    fn a_task_that_keeps_working_after_recording_a_fact_still_leaves_it_firm() {
        let Some(s) = Sandbox::new() else { return };
        s.write("src/config.rs", "v1");
        let project = s.project();

        let seq =
            s.db.tasks()
                .create(&project, "Port the loader", "", 0, "cli")
                .unwrap()
                .seq;
        s.db.scopes()
            .declare(seq, &named(&["src/config.rs"]), "cli", OnConflict::Report)
            .unwrap();
        s.db.tasks()
            .claim(seq, "codex:9f2c", std::time::Duration::from_secs(900))
            .unwrap();

        let learned = s.store("the loader reads env before the file", Some(seq));
        let derived = ground(&s.db, Some(seq), None);
        assert_eq!(derived.by, Anchoring::Derived);
        anchor(&s.db, Some(&s.witness()), &learned.id, &derived).unwrap();

        // The agent carries on working the same file, as agents do.
        s.write("src/config.rs", "v2 — tidied up");
        assert_eq!(
            s.standing(&learned.id).as_str(),
            "shaky",
            "mid-flight, its own edits have moved the ground under it"
        );

        assert_eq!(settle(&s.db, Some(&s.witness()), seq), 1);
        assert_eq!(
            s.standing(&learned.id).as_str(),
            "firm",
            "settling re-reads the tree the task is leaving behind"
        );
    }

    /// A fact whose author named its files said something specific, and a task
    /// tidying up after itself is not entitled to overrule it — the fact may
    /// well be about a file this task never went near.
    #[test]
    fn settling_leaves_a_footing_the_author_named_alone() {
        let Some(s) = Sandbox::new() else { return };
        s.write("src/config.rs", "v1");
        s.write("docs/loader.md", "the loader, explained");
        let project = s.project();

        let seq =
            s.db.tasks()
                .create(&project, "Port the loader", "", 0, "cli")
                .unwrap()
                .seq;
        s.db.scopes()
            .declare(seq, &named(&["src/config.rs"]), "cli", OnConflict::Report)
            .unwrap();
        s.db.tasks()
            .claim(seq, "codex:9f2c", std::time::Duration::from_secs(900))
            .unwrap();

        let about_docs = s.store("the loader's docs are hand-written", Some(seq));
        let stated = ground(&s.db, Some(seq), Some(&named(&["docs/loader.md"])));
        assert_eq!(stated.by, Anchoring::Named);
        anchor(&s.db, Some(&s.witness()), &about_docs.id, &stated).unwrap();

        // The task's own file moves; the fact is not about that file.
        s.write("src/config.rs", "v2");
        assert_eq!(s.standing(&about_docs.id).as_str(), "firm");

        assert_eq!(
            settle(&s.db, Some(&s.witness()), seq),
            0,
            "nothing to settle: the only fact here chose its own ground"
        );
        assert_eq!(
            s.db.footings()
                .anchors(&about_docs.id)
                .unwrap()
                .into_iter()
                .map(|a| a.path)
                .collect::<Vec<_>>(),
            vec!["docs/loader.md"],
            "and it kept it"
        );
    }

    /// An anchor is a path relative to *its own* project root, so a fact filed
    /// under another checkout must never be measured against this one — the
    /// paths would resolve, and the answer would be fiction.
    #[test]
    fn a_fact_from_another_project_is_never_measured_against_this_tree() {
        let Some(s) = Sandbox::new() else { return };
        s.write("src/config.rs", "v1");
        let mine = s.store("this project's fact", None);
        s.anchor_to(&mine.id, &["src/config.rs"]);

        let theirs =
            s.db.memory()
                .store(NewAssertion {
                    project: "/somewhere/else",
                    content: "another project's fact",
                    tags: "",
                    actor: "codex:9f2c",
                    task_seq: None,
                })
                .unwrap();
        // Same relative path, a different checkout entirely.
        s.db.footings()
            .anchor(
                &theirs.id,
                &[Anchor {
                    path: "src/config.rs".into(),
                    hash: "whatever-it-said-over-there".into(),
                    at: now_ts(),
                }],
                Anchoring::Named,
            )
            .unwrap();

        let ids = vec![mine.id.clone(), theirs.id.clone()];
        let seen = standings(&s.db, Some(&s.witness()), &s.project(), &ids);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[&mine.id].as_str(), "firm");
        assert!(!seen.contains_key(&theirs.id));
    }

    #[test]
    fn without_a_witness_nothing_is_anchored_and_nothing_is_claimed() {
        let db = Db::open_in_memory().unwrap();
        let a = db
            .memory()
            .store(NewAssertion {
                project: "/tmp/project",
                content: "a fact",
                tags: "",
                actor: "codex:9f2c",
                task_seq: None,
            })
            .unwrap();
        let ground = Ground {
            paths: named(&["src/config.rs"]),
            by: Anchoring::Named,
        };
        assert!(anchor(&db, None, &a.id, &ground).unwrap().is_empty());
        assert_eq!(
            standing(&db, None, "/tmp/project", &a.id),
            Standing::Unanchored
        );
        assert_eq!(settle(&db, None, 1), 0);
    }

    #[test]
    fn named_paths_win_over_the_task_and_globs_are_dropped() {
        let db = Db::open_in_memory().unwrap();
        let seq = db
            .tasks()
            .create("/tmp/project", "t", "", 0, "cli")
            .unwrap()
            .seq;
        db.scopes()
            .declare(seq, &named(&["src/db.rs"]), "cli", OnConflict::Report)
            .unwrap();

        let chosen = ground(&db, Some(seq), Some(&named(&["./src/config.rs", "src/**"])));
        assert_eq!(
            chosen,
            Ground {
                paths: named(&["src/config.rs"]),
                by: Anchoring::Named,
            },
            "normalized, and the glob is not guessed at"
        );

        // Nothing usable named at all falls back to the task rather than
        // refusing to anchor, because a typo is a poor reason to punish the one
        // agent that tried to be precise.
        let fallback = ground(&db, Some(seq), Some(&named(&["src/**"])));
        assert_eq!(
            fallback,
            Ground {
                paths: named(&["src/db.rs"]),
                by: Anchoring::Derived,
            }
        );
        assert!(ground(&db, None, None).paths.is_empty());
    }
}
