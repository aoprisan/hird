//! Filing a whole plan: many tasks, their dependencies and their scopes, at once.
//!
//! A plan is filed in one transaction. Either the entire graph lands or none of
//! it does, which is the property a shell script full of `hird add` calls
//! cannot have: one that dies halfway leaves a handful of real tasks behind,
//! missing exactly the dependencies that were going to keep them in order.
//!
//! Applying is idempotent. Each task a plan files is remembered by the name it
//! had in the file, so applying the same plan again recognizes its own work and
//! adds only what is new. That is what makes a plan file something you can edit
//! and re-run rather than something you get one shot at.
//!
//! What it will not do is rewrite tasks that already exist. A plan is how work
//! is filed, not a description the queue is kept in sync with: by the time you
//! edit the file an agent may have claimed a task, worked it, and recorded what
//! it learned. So a task whose title has drifted from the plan keeps the
//! queue's version, and [`Applied::drifted`] says so rather than letting the
//! difference pass unmentioned.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::deps::{dependency_path, id_for_seq};
use super::scope::{declare_in_tx, OnConflict};
use super::tasks::{create_in_tx, insert_dep, insert_event};
use crate::error::{Error, Result};
use crate::model::{now_ts, Conflict, EventKind};
use crate::plan::{Plan, PlanTask};

/// Repository over `task_plan_nodes`, and the one place a plan becomes rows.
pub struct Plans<'a> {
    conn: &'a Connection,
}

/// A task in the queue, under the name the plan gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub name: String,
    pub seq: i64,
    pub title: String,
}

/// A task the plan describes differently from how the queue holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub name: String,
    pub seq: i64,
    /// Which part differs: `"title"`, `"body"` or `"priority"`.
    pub field: &'static str,
}

impl Drift {
    /// One sentence, as `hird plan apply` prints it.
    pub fn describe(&self) -> String {
        format!(
            "#{} ({}) — the plan's {} differs from the queue's, and the queue's \
             was kept; edit it with the task, not the file",
            self.seq, self.name, self.field
        )
    }
}

/// What applying a plan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Tasks this apply filed, in plan order.
    pub created: Vec<Placed>,
    /// Tasks an earlier apply of the same plan had already filed.
    pub reused: Vec<Placed>,
    /// Reused tasks the plan now describes differently.
    pub drifted: Vec<Drift>,
    /// Dependency edges this apply added.
    pub edges_added: usize,
    /// Overlaps between what the plan declares and what agents hold right now.
    pub conflicts: Vec<Conflict>,
}

impl Applied {
    /// Did this apply change anything at all?
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.edges_added == 0
    }
}

impl<'a> Plans<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Plans<'a> {
        Plans { conn }
    }

    /// File every task in `plan` that is not filed already, with its
    /// dependencies and declared scopes, in one transaction.
    ///
    /// The plan has already been validated by [`crate::plan::parse`], so the
    /// only refusals left are ones the database knows about: a dependency that
    /// would close a cycle through an edge somebody added by hand.
    pub fn apply(&self, project: &str, plan: &Plan, actor: &str) -> Result<Applied> {
        let tx = Transaction::new_unchecked(self.conn, TransactionBehavior::Immediate)?;
        let now = now_ts();
        let mut applied = Applied::default();
        // Name → (task id, seq), for the dependency pass below. Built as tasks
        // are created or found, so both passes see the same resolution.
        let mut placed: Vec<(&PlanTask, String, i64)> = Vec::new();

        for task in &plan.tasks {
            match find_node(&tx, project, &plan.plan, &task.name)? {
                Some(existing) => {
                    if let Some(field) = drifted_field(&existing, task) {
                        applied.drifted.push(Drift {
                            name: task.name.clone(),
                            seq: existing.seq,
                            field,
                        });
                    }
                    applied.reused.push(Placed {
                        name: task.name.clone(),
                        seq: existing.seq,
                        title: existing.title.clone(),
                    });
                    placed.push((task, existing.id, existing.seq));
                }
                None => {
                    let created =
                        create_in_tx(&tx, project, &task.title, &task.body, task.priority, actor)?;
                    tx.execute(
                        "INSERT INTO task_plan_nodes (task_id, project, plan, node, at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![created.id, project, plan.plan, task.name, now],
                    )?;
                    insert_event(
                        &tx,
                        &created.id,
                        &now,
                        actor,
                        EventKind::Note,
                        &format!("filed from plan {:?} as {:?}", plan.plan, task.name),
                    )?;
                    applied.created.push(Placed {
                        name: task.name.clone(),
                        seq: created.seq,
                        title: created.title.clone(),
                    });
                    placed.push((task, created.id, created.seq));
                }
            }
        }

        // Scopes and dependencies go on after every task exists, so a plan may
        // name its tasks in any order — `needs` points backwards or forwards.
        for (task, _, seq) in &placed {
            if !task.paths.is_empty() {
                let conflicts = declare_in_tx(&tx, *seq, &task.paths, actor, OnConflict::Report)?;
                applied.conflicts.extend(conflicts);
            }
        }
        for (task, id, seq) in &placed {
            for need in &task.needs {
                let Some((_, needed_id, needed_seq)) =
                    placed.iter().find(|(t, _, _)| &t.name == need)
                else {
                    // Unreachable: validation resolved every name in the plan.
                    continue;
                };
                applied.edges_added += usize::from(add_edge(
                    &tx,
                    id,
                    needed_id,
                    *seq,
                    *needed_seq,
                    actor,
                    &now,
                )?);
            }
        }

        tx.commit()?;
        Ok(applied)
    }

    /// Every task filed by `plan_name` in this project, in queue order.
    pub fn nodes(&self, project: &str, plan_name: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.node, t.seq FROM task_plan_nodes n
             JOIN tasks t ON t.id = n.task_id
             WHERE n.project = ?1 AND n.plan = ?2
             ORDER BY t.seq",
        )?;
        let rows = stmt.query_map(params![project, plan_name], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The plans that have filed work in this project, with how many tasks each.
    pub fn list(&self, project: &str) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT plan, COUNT(*) FROM task_plan_nodes
             WHERE project = ?1 GROUP BY plan ORDER BY plan",
        )?;
        let rows = stmt.query_map([project], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The plan and node name a task was filed under, if it came from a plan.
    pub fn origin_of(&self, seq: i64) -> Result<Option<(String, String)>> {
        let task_id = id_for_seq(self.conn, seq)?;
        Ok(self
            .conn
            .query_row(
                "SELECT plan, node FROM task_plan_nodes WHERE task_id = ?1",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }
}

// ------------------------------------------------------------------ helpers

/// A task an earlier apply of this plan filed.
struct Existing {
    id: String,
    seq: i64,
    title: String,
    body: String,
    priority: i64,
}

fn find_node(
    tx: &Transaction<'_>,
    project: &str,
    plan: &str,
    node: &str,
) -> Result<Option<Existing>> {
    Ok(tx
        .query_row(
            "SELECT t.id, t.seq, t.title, t.body, t.priority
             FROM task_plan_nodes n JOIN tasks t ON t.id = n.task_id
             WHERE n.project = ?1 AND n.plan = ?2 AND n.node = ?3",
            params![project, plan, node],
            |row| {
                Ok(Existing {
                    id: row.get(0)?,
                    seq: row.get(1)?,
                    title: row.get(2)?,
                    body: row.get(3)?,
                    priority: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// The first field the plan and the queue disagree about, if any.
fn drifted_field(existing: &Existing, task: &PlanTask) -> Option<&'static str> {
    if existing.title != task.title.trim() {
        return Some("title");
    }
    if existing.body != task.body {
        return Some("body");
    }
    if existing.priority != task.priority {
        return Some("priority");
    }
    None
}

/// Add one dependency edge, refusing anything that would close a cycle.
///
/// The plan itself is acyclic — validation saw to that — but a task an earlier
/// apply filed may have picked up edges since, from `hird dep add` or from an
/// agent splitting it. So the check is the same one [`super::Deps::add`] makes,
/// and the refusal reads the same way.
fn add_edge(
    tx: &Transaction<'_>,
    task_id: &str,
    needed_id: &str,
    seq: i64,
    needed_seq: i64,
    actor: &str,
    now: &str,
) -> Result<bool> {
    if let Some(path) = dependency_path(tx, needed_id, task_id)? {
        return Err(Error::DependencyCycle {
            seq,
            on: needed_seq,
            path,
        });
    }
    let existing: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM task_deps WHERE task_id = ?1 AND depends_on_id = ?2",
            params![task_id, needed_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(false);
    }
    insert_dep(tx, task_id, needed_id, actor, now)?;
    insert_event(
        tx,
        task_id,
        now,
        actor,
        EventKind::DepAdded,
        &format!("now waits for task {needed_seq}"),
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::model::Status;
    use crate::plan;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const ACTOR: &str = "cli";
    const TTL: Duration = Duration::from_secs(900);

    const SAMPLE: &str = r#"
plan = "serde-migration"

[[task]]
name = "schema"
title = "Design the storage schema"
priority = 3
paths = ["src/db.rs"]

[[task]]
name = "repos"
title = "Port the repository layer"
paths = ["src/repo/**"]
needs = ["schema"]

[[task]]
name = "renderer"
title = "Rewrite the renderer"
paths = ["src/tui/**"]
"#;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn apply(db: &Db, source: &str) -> Applied {
        let plan = plan::parse(source).unwrap();
        db.plans().apply(PROJECT, &plan, ACTOR).unwrap()
    }

    #[test]
    fn a_plan_files_every_task_with_its_edges_and_scopes() {
        let db = db();
        let applied = apply(&db, SAMPLE);

        assert_eq!(applied.created.len(), 3);
        assert!(applied.reused.is_empty());
        assert_eq!(applied.edges_added, 1);
        // Numbered in plan order.
        assert_eq!(applied.created[0].name, "schema");
        assert_eq!(applied.created[0].seq, 1);
        assert_eq!(applied.created[2].seq, 3);

        assert_eq!(db.scopes().for_task(1).unwrap(), vec!["src/db.rs"]);
        let blockers = db.deps().blockers(2).unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].seq, 1);
    }

    #[test]
    fn applying_the_same_plan_again_files_nothing_new() {
        let db = db();
        apply(&db, SAMPLE);
        let again = apply(&db, SAMPLE);

        assert!(again.created.is_empty());
        assert_eq!(again.reused.len(), 3);
        assert_eq!(again.edges_added, 0);
        assert!(again.is_empty());
        assert_eq!(
            db.tasks()
                .list(&super::super::ProjectScope::All, None)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn an_edited_plan_files_only_what_is_new() {
        let db = db();
        apply(&db, SAMPLE);
        let grown = apply(
            &db,
            &format!(
                "{SAMPLE}
[[task]]
name = \"notes\"
title = \"Write the release notes\"
needs = [\"repos\", \"renderer\"]
"
            ),
        );

        assert_eq!(grown.created.len(), 1);
        assert_eq!(grown.created[0].name, "notes");
        assert_eq!(grown.created[0].seq, 4);
        assert_eq!(grown.reused.len(), 3);
        assert_eq!(grown.edges_added, 2);
        assert_eq!(db.deps().blockers(4).unwrap().len(), 2);
    }

    #[test]
    fn a_task_the_plan_now_describes_differently_keeps_the_queues_version() {
        let db = db();
        apply(&db, SAMPLE);
        let edited = apply(
            &db,
            &SAMPLE.replace("Design the storage schema", "Design the schema"),
        );

        assert!(edited.created.is_empty());
        assert_eq!(edited.drifted.len(), 1);
        assert_eq!(edited.drifted[0].field, "title");
        assert_eq!(edited.drifted[0].seq, 1);
        assert!(edited.drifted[0]
            .describe()
            .contains("the queue's was kept"));
        assert_eq!(
            db.tasks().get(1).unwrap().title,
            "Design the storage schema"
        );
    }

    #[test]
    fn a_plan_applied_to_two_projects_files_each_separately() {
        let db = db();
        let plan = plan::parse(SAMPLE).unwrap();
        db.plans().apply(PROJECT, &plan, ACTOR).unwrap();
        let elsewhere = db.plans().apply("/tmp/other", &plan, ACTOR).unwrap();

        assert_eq!(elsewhere.created.len(), 3);
        assert_eq!(elsewhere.created[0].seq, 4);
    }

    #[test]
    fn nothing_is_written_when_an_edge_would_close_a_cycle() {
        // The plan itself is acyclic — validation saw to that. The ring can
        // only come from an edge added outside the plan, between two tasks an
        // earlier apply filed.
        let db = db();
        let first = "
plan = \"p\"
[[task]]
name = \"schema\"
title = \"Design the storage schema\"
[[task]]
name = \"repos\"
title = \"Port the repository layer\"
";
        apply(&db, first);
        // A human decides the schema should follow the port, and says so.
        db.deps().add(1, 2, ACTOR).unwrap();

        // The plan now claims the opposite, and brings a new task with it.
        let edited = plan::parse(
            "
plan = \"p\"
[[task]]
name = \"schema\"
title = \"Design the storage schema\"
[[task]]
name = \"repos\"
title = \"Port the repository layer\"
needs = [\"schema\"]
[[task]]
name = \"notes\"
title = \"Write the release notes\"
",
        )
        .unwrap();

        let err = db.plans().apply(PROJECT, &edited, ACTOR).unwrap_err();
        assert!(matches!(err, Error::DependencyCycle { .. }), "{err}");
        // The new task in the same plan was rolled back along with the edge.
        assert_eq!(
            db.tasks()
                .list(&super::super::ProjectScope::All, None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn filing_a_plan_over_live_work_reports_the_overlap() {
        let db = db();
        let held = db
            .tasks()
            .create(PROJECT, "Rework the renderer", "", 0, ACTOR)
            .unwrap();
        db.scopes()
            .declare(
                held.seq,
                &["src/tui/view.rs".into()],
                ACTOR,
                OnConflict::Report,
            )
            .unwrap();
        db.tasks().claim(held.seq, "codex:9f2c", TTL).unwrap();

        let applied = apply(&db, SAMPLE);
        assert_eq!(applied.created.len(), 3);
        assert_eq!(applied.conflicts.len(), 1);
        assert_eq!(applied.conflicts[0].other_seq, held.seq);
        assert_eq!(applied.conflicts[0].pattern, "src/tui/**");
    }

    #[test]
    fn a_filed_task_remembers_the_plan_it_came_from() {
        let db = db();
        apply(&db, SAMPLE);

        assert_eq!(
            db.plans().origin_of(2).unwrap(),
            Some(("serde-migration".into(), "repos".into()))
        );
        assert_eq!(
            db.plans().nodes(PROJECT, "serde-migration").unwrap(),
            vec![
                ("schema".to_string(), 1),
                ("repos".to_string(), 2),
                ("renderer".to_string(), 3)
            ]
        );
        assert_eq!(
            db.plans().list(PROJECT).unwrap(),
            vec![("serde-migration".to_string(), 3)]
        );
    }

    #[test]
    fn a_task_filed_by_hand_has_no_plan_behind_it() {
        let db = db();
        let task = db.tasks().create(PROJECT, "By hand", "", 0, ACTOR).unwrap();
        assert_eq!(db.plans().origin_of(task.seq).unwrap(), None);
    }

    #[test]
    fn tasks_a_plan_filed_are_ordinary_tasks() {
        // Nothing about a planned task is special: it claims, works and
        // completes like any other, and its dependency still gates it.
        let db = db();
        apply(&db, SAMPLE);

        let err = db.tasks().claim(2, "codex:9f2c", TTL).unwrap_err();
        assert!(matches!(err, Error::Blocked { .. }), "{err}");

        db.tasks().claim(1, "codex:9f2c", TTL).unwrap();
        db.tasks().complete(1, "codex:9f2c", "schema done").unwrap();
        let claimed = db.tasks().claim(2, "codex:9f2c", TTL).unwrap();
        assert_eq!(claimed.status, Status::Claimed);
        assert_eq!(db.scopes().for_task(2).unwrap(), vec!["src/repo/**"]);
    }
}
