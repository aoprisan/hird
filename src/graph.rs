//! The graph as data: one snapshot of the board that every picture is drawn
//! from.
//!
//! `hird graph` prints waves, `--dot` and `--mermaid` draw them, and the TUI
//! lists them; each of those read the queue for itself and agreed only
//! because they called the same `dispatch_waves`. `hird graph --json` and
//! `hird web` want the same thing a machine can consume — nodes, edges, waves,
//! who holds what, what each task waits for and feeds — so that reading lives
//! here once, as a [`Snapshot`], and the renderers are readers of it.
//!
//! A snapshot is either *live* — the board as it stands — or *replayed*, the
//! board as the trail says it stood at an earlier instant (§22's `hird
//! replay`, folded into the same shape). Replay reconstructs status, holder
//! and the question gate from events; dependencies and requirements are read
//! from the present-day rows, because the trail records those edits as prose
//! rather than as reversible facts, and a picture that is honest about what
//! it knows says so in [`Snapshot::live`].

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::db::Db;
use crate::error::Result;
use crate::model::{Status, TaskSummary};
use crate::repo::{dispatch_waves, ProjectScope};

/// One node of the graph: a task, as the picture needs it.
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub seq: i64,
    pub title: String,
    pub status: Status,
    pub priority: i64,
    pub project: String,
    /// `harness:session` while claimed or in progress.
    pub holder: Option<String>,
    pub lease_expires_at: Option<String>,
    /// Capabilities a claimant must advertise.
    pub requires: Vec<String>,
    /// The plan this task was filed from, and its name in that plan.
    pub plan: Option<String>,
    pub node: Option<String>,
    /// Which wave an unfinished task sits in, zero-based; finished tasks have
    /// none.
    pub wave: Option<usize>,
    /// Unfinished tasks this one still waits for.
    pub waits_for: Vec<i64>,
    /// Every task this one is a dependency of, finished or not.
    pub feeds: Vec<i64>,
    /// An unanswered question is parking it out of dispatch.
    pub awaits_answer: bool,
    /// The task whose work this task reviews, when it is a review.
    pub review_of: Option<i64>,
    /// The unfinished review of this task's work, when one is open.
    pub under_review: Option<i64>,
    /// The newest verdict delivered on this task's work.
    pub verdict: Option<&'static str>,
}

/// One dependency edge, pointing the way work flows: `from` must finish
/// before `to` can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Edge {
    pub from: i64,
    pub to: i64,
}

/// A recess, as the picture wears it.
#[derive(Debug, Clone, Serialize)]
pub struct RecessNote {
    pub reason: String,
    pub at: String,
}

/// The board, as one value.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// The project root, or `None` when the scope spans every project.
    pub project: Option<String>,
    /// The instant this snapshot describes.
    pub as_of: String,
    /// `true` when this is the board as it stands, `false` for a replay —
    /// in which case dependencies and requirements are present-day.
    pub live: bool,
    /// The plan the snapshot was narrowed to, when it was.
    pub plan: Option<String>,
    /// Every plan name that has filed a task in scope.
    pub plans: Vec<String>,
    pub recess: Option<RecessNote>,
    /// The newest event cursor in scope, for a follower to resume from.
    pub cursor: i64,
    /// When the trail in scope begins — the earliest instant a replay can show.
    pub first_at: Option<String>,
    /// Unfinished task numbers by dispatch wave; wave 0 is workable now.
    pub waves: Vec<Vec<i64>>,
    pub tasks: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Snapshot {
    /// The board as it stands.
    pub fn live(db: &Db, scope: &ProjectScope, plan: Option<&str>) -> Result<Snapshot> {
        let tasks = db.tasks().list(scope, None)?;
        let questions: BTreeSet<i64> = db.questions().unanswered_map(scope)?.into_keys().collect();
        let reviews = db.recusals().reviews(scope)?;
        let verdicts = db.verdicts().standing(scope)?;
        let mut snapshot = assemble(db, scope, tasks, &questions, &reviews, plan)?;
        // Which `done` is provisional: judged tasks whose review is still open.
        let open: BTreeSet<i64> = snapshot
            .tasks
            .iter()
            .filter(|n| !n.status.is_terminal())
            .map(|n| n.seq)
            .collect();
        let under_review: BTreeMap<i64, i64> = reviews
            .iter()
            .filter(|(review, _)| open.contains(review))
            .map(|(review, judged)| (*judged, *review))
            .collect();
        for node in &mut snapshot.tasks {
            node.under_review = under_review.get(&node.seq).copied();
            node.verdict = verdicts.get(&node.seq).map(|v| v.as_str());
        }
        snapshot.as_of = crate::model::now_ts();
        snapshot.live = true;
        Ok(snapshot)
    }

    /// The board as the trail says it stood at `cutoff`, an RFC3339 UTC
    /// instant or a prefix of one.
    pub fn at(db: &Db, scope: &ProjectScope, cutoff: &str, plan: Option<&str>) -> Result<Snapshot> {
        let present: BTreeMap<i64, TaskSummary> = db
            .tasks()
            .list(scope, None)?
            .into_iter()
            .map(|t| (t.seq, t))
            .collect();
        let mut questions = BTreeSet::new();
        let mut tasks = Vec::new();
        for replayed in db.events().board_at(scope, cutoff)? {
            let Some(now) = present.get(&replayed.seq) else {
                continue;
            };
            if replayed.awaiting_answer {
                questions.insert(replayed.seq);
            }
            tasks.push(TaskSummary {
                seq: replayed.seq,
                project: replayed.project,
                title: replayed.title,
                status: replayed.status,
                priority: now.priority,
                claimed_by: replayed.holder,
                lease_expires_at: None,
                updated_at: String::new(),
                requirements: now.requirements.clone(),
            });
        }
        let reviews = db.recusals().reviews(scope)?;
        let mut snapshot = assemble(db, scope, tasks, &questions, &reviews, plan)?;
        snapshot.as_of = cutoff.to_string();
        snapshot.live = false;
        Ok(snapshot)
    }
}

/// Everything a live and a replayed snapshot compute the same way.
fn assemble(
    db: &Db,
    scope: &ProjectScope,
    mut tasks: Vec<TaskSummary>,
    questions: &BTreeSet<i64>,
    reviews: &BTreeMap<i64, i64>,
    plan: Option<&str>,
) -> Result<Snapshot> {
    let origins: BTreeMap<i64, (String, String)> = match scope {
        ProjectScope::Only(project) => db.plans().origins(project)?,
        ProjectScope::All => {
            let mut all = BTreeMap::new();
            for project in tasks
                .iter()
                .map(|t| t.project.clone())
                .collect::<BTreeSet<_>>()
            {
                all.extend(db.plans().origins(&project)?);
            }
            all
        }
    };
    let plans: Vec<String> = origins
        .values()
        .map(|(plan, _)| plan.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    // Waves are computed over the whole scope before any narrowing, so a
    // plan's task that waits for work outside the plan still sits in the
    // wave the queue will actually hand it out in.
    let mut edges = db.deps().edges(scope)?;
    let waves = dispatch_waves(&tasks, &edges);
    let wave_of: BTreeMap<i64, usize> = waves
        .iter()
        .enumerate()
        .flat_map(|(i, wave)| wave.iter().map(move |seq| (*seq, i)))
        .collect();

    if let Some(name) = plan {
        let members: BTreeSet<i64> = origins
            .iter()
            .filter(|(_, (p, _))| p == name)
            .map(|(seq, _)| *seq)
            .collect();
        tasks.retain(|t| members.contains(&t.seq));
        edges.retain(|(t, on)| members.contains(t) && members.contains(on));
    }
    let shown: BTreeSet<i64> = tasks.iter().map(|t| t.seq).collect();
    edges.retain(|(t, on)| shown.contains(t) && shown.contains(on));

    let status_of: BTreeMap<i64, Status> = tasks.iter().map(|t| (t.seq, t.status)).collect();
    let mut waits: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut feeds: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (task, on) in &edges {
        feeds.entry(*on).or_default().push(*task);
        if status_of.get(on).is_some_and(|s| !s.is_terminal()) {
            waits.entry(*task).or_default().push(*on);
        }
    }
    let review_of: BTreeMap<i64, i64> = reviews
        .iter()
        .filter(|(review, judged)| shown.contains(review) && shown.contains(judged))
        .map(|(review, judged)| (*review, *judged))
        .collect();

    // Numbered order, whatever order the queue listed them in: a picture is
    // easier to diff when its nodes do not move.
    tasks.sort_by_key(|t| t.seq);
    let nodes = tasks
        .into_iter()
        .map(|t| {
            let (plan, node) = match origins.get(&t.seq) {
                Some((plan, node)) => (Some(plan.clone()), Some(node.clone())),
                None => (None, None),
            };
            Node {
                seq: t.seq,
                title: t.title,
                status: t.status,
                priority: t.priority,
                project: t.project,
                holder: t.claimed_by,
                lease_expires_at: t.lease_expires_at,
                requires: t.requirements,
                plan,
                node,
                wave: wave_of.get(&t.seq).copied(),
                waits_for: waits.remove(&t.seq).unwrap_or_default(),
                feeds: feeds.remove(&t.seq).unwrap_or_default(),
                awaits_answer: questions.contains(&t.seq),
                review_of: review_of.get(&t.seq).copied(),
                under_review: None,
                verdict: None,
            }
        })
        .collect();

    let recess = match scope {
        ProjectScope::Only(project) => db.recesses().current(project)?.map(|r| RecessNote {
            reason: r.reason,
            at: r.at,
        }),
        ProjectScope::All => None,
    };
    let (first_at, cursor) = match db.events().bounds(scope)? {
        Some((first, cursor)) => (Some(first), cursor),
        None => (None, 0),
    };

    Ok(Snapshot {
        project: match scope {
            ProjectScope::Only(p) => Some(p.clone()),
            ProjectScope::All => None,
        },
        as_of: String::new(),
        live: true,
        plan: plan.map(str::to_string),
        plans,
        recess,
        cursor,
        first_at,
        waves: waves
            .into_iter()
            .map(|wave| wave.into_iter().filter(|s| shown.contains(s)).collect())
            .filter(|wave: &Vec<i64>| !wave.is_empty())
            .collect(),
        tasks: nodes,
        edges: edges
            .into_iter()
            .map(|(to, from)| Edge { from, to })
            .collect(),
    })
}

/// One feed event as the JSON `hird events --json` and the web feed share.
pub fn event_json(event: &crate::repo::FeedEvent) -> serde_json::Value {
    serde_json::json!({
        "cursor": event.cursor,
        "at": event.at,
        "project": event.project,
        "task": event.task_seq,
        "title": event.task_title,
        "actor": event.actor,
        "kind": event.kind.as_str(),
        "detail": event.detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan;
    use std::time::Duration;

    const PROJECT: &str = "/tmp/project";
    const TTL: Duration = Duration::from_secs(900);

    fn filed() -> (Db, Vec<i64>) {
        let db = Db::open_in_memory().unwrap();
        let plan = plan::parse(
            r#"
plan = "p"
[[task]]
name = "schema"
title = "Design the schema"
[[task]]
name = "repos"
title = "Port the repos"
needs = ["schema"]
[[task]]
name = "notes"
title = "Write the notes"
needs = ["repos"]
"#,
        )
        .unwrap();
        db.plans().apply(PROJECT, &plan, "cli").unwrap();
        let loose = db
            .tasks()
            .create(PROJECT, "Loose end", "", 0, "cli")
            .unwrap();
        (db, vec![1, 2, 3, loose.seq])
    }

    fn scope() -> ProjectScope {
        ProjectScope::Only(PROJECT.to_string())
    }

    #[test]
    fn a_live_snapshot_carries_waves_edges_and_plan_names() {
        let (db, _) = filed();
        let snap = Snapshot::live(&db, &scope(), None).unwrap();
        assert!(snap.live);
        assert_eq!(snap.plans, vec!["p"]);
        assert_eq!(snap.waves, vec![vec![1, 4], vec![2], vec![3]]);
        assert_eq!(
            snap.edges,
            vec![Edge { from: 1, to: 2 }, Edge { from: 2, to: 3 }]
        );
        let repos = snap.tasks.iter().find(|n| n.seq == 2).unwrap();
        assert_eq!(repos.plan.as_deref(), Some("p"));
        assert_eq!(repos.node.as_deref(), Some("repos"));
        assert_eq!(repos.wave, Some(1));
        assert_eq!(repos.waits_for, vec![1]);
        assert_eq!(repos.feeds, vec![3]);
        let loose = snap.tasks.iter().find(|n| n.seq == 4).unwrap();
        assert!(loose.plan.is_none());
        assert_eq!(loose.wave, Some(0));
        assert!(snap.first_at.is_some());
        assert!(snap.cursor > 0);
    }

    #[test]
    fn narrowing_to_a_plan_keeps_its_tasks_and_the_edges_between_them() {
        let (db, _) = filed();
        let snap = Snapshot::live(&db, &scope(), Some("p")).unwrap();
        assert_eq!(snap.plan.as_deref(), Some("p"));
        let seqs: Vec<i64> = snap.tasks.iter().map(|n| n.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(snap.waves, vec![vec![1], vec![2], vec![3]]);
        assert_eq!(snap.edges.len(), 2);
    }

    #[test]
    fn a_finished_dependency_no_longer_counts_as_waiting_but_still_feeds() {
        let (db, _) = filed();
        db.tasks().claim(1, "codex:1", TTL).unwrap();
        db.tasks().complete(1, "codex:1", "done").unwrap();
        let snap = Snapshot::live(&db, &scope(), None).unwrap();
        let schema = snap.tasks.iter().find(|n| n.seq == 1).unwrap();
        assert_eq!(schema.status, Status::Done);
        assert!(schema.wave.is_none());
        assert_eq!(schema.feeds, vec![2]);
        let repos = snap.tasks.iter().find(|n| n.seq == 2).unwrap();
        assert!(repos.waits_for.is_empty());
        assert_eq!(repos.wave, Some(0));
    }

    #[test]
    fn a_replayed_snapshot_folds_the_trail_and_says_it_is_not_live() {
        let (db, _) = filed();
        db.tasks().claim(1, "codex:1", TTL).unwrap();
        let claimed_at = crate::model::now_ts();
        std::thread::sleep(Duration::from_millis(5));
        db.tasks().complete(1, "codex:1", "done").unwrap();

        let then = Snapshot::at(&db, &scope(), &claimed_at, None).unwrap();
        assert!(!then.live);
        assert_eq!(then.as_of, claimed_at);
        let schema = then.tasks.iter().find(|n| n.seq == 1).unwrap();
        assert_eq!(schema.status, Status::Claimed);
        assert_eq!(schema.holder.as_deref(), Some("codex:1"));
        let repos = then.tasks.iter().find(|n| n.seq == 2).unwrap();
        assert_eq!(repos.waits_for, vec![1]);
        assert_eq!(then.waves[0], vec![1, 4]);

        let now = Snapshot::live(&db, &scope(), None).unwrap();
        assert_eq!(
            now.tasks.iter().find(|n| n.seq == 1).unwrap().status,
            Status::Done
        );
    }

    #[test]
    fn a_replay_before_anything_was_filed_is_empty() {
        let (db, _) = filed();
        let snap = Snapshot::at(&db, &scope(), "2000-01-01", None).unwrap();
        assert!(snap.tasks.is_empty());
        assert!(snap.waves.is_empty());
    }

    #[test]
    fn the_snapshot_serializes_with_snake_case_statuses() {
        let (db, _) = filed();
        let snap = Snapshot::live(&db, &scope(), None).unwrap();
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["tasks"][0]["status"], "open");
        assert_eq!(json["edges"][0]["from"], 1);
        assert_eq!(json["edges"][0]["to"], 2);
        assert_eq!(json["project"], PROJECT);
    }
}
