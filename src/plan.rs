//! Plan files: a whole dependency graph, written down before it is filed.
//!
//! A plan filed by hand is a shell script — `hird add` calls whose printed
//! numbers get caught in variables and threaded back into `--needs`. That works
//! and it composes, but the plan only ever exists as the act of filing it:
//! there is nothing to read before the swarm starts, nothing to commit next to
//! the code it describes, and nothing to run twice.
//!
//! A plan file is the same graph as data. Tasks carry symbolic names instead of
//! queue numbers, so the file says `needs = ["schema"]` and means it whatever
//! the database happens to number things; [`crate::repo::Plans::apply`] resolves
//! the names once, in one transaction.
//!
//! # What a plan may say
//!
//! Exactly what a row can hold — a title, a body, a priority, the files a task
//! expects to touch, and the other tasks it waits for. There are no
//! conditionals, no loops, no templating and no retries, and that is not an
//! omission to be filled in later: `hird` hands work out because an agent asked
//! for it, and a file that could say *when* to run something would be
//! describing a scheduler this queue deliberately does not have. The rule that
//! keeps it honest is that nothing may appear in a plan that is not already a
//! column: there is no table for a conditional, so there is no syntax for one.
//!
//! ```toml
//! plan = "serde-migration"
//!
//! [[task]]
//! name = "schema"
//! title = "Design the storage schema"
//! priority = 3
//! paths = ["src/db.rs"]
//!
//! [[task]]
//! name = "repos"
//! title = "Port the repository layer"
//! paths = ["src/repo/**"]
//! needs = ["schema"]
//! ```
//!
//! # What the file can be checked for before it is filed
//!
//! Everything the queue would otherwise only discover at dispatch time.
//! [`Plan::preview`] resolves the graph into the same waves `hird graph` prints,
//! and — because a declared scope is a glob and two globs can be intersected
//! without either file existing — it can also name the pairs of tasks that
//! declare overlapping files with no dependency between them. Those pairs are
//! the ones that look parallel on paper and will be handed out one at a time,
//! which is worth knowing before three agents are pointed at the plan rather
//! than after.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::glob;
use crate::model::{Status, TaskSummary};
use crate::repo::{dispatch_waves, normalize_all};

/// A dependency graph as written down, before any of it exists in the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// What this plan is called. Names the set of tasks it files, so applying
    /// the same plan twice recognizes its own work instead of duplicating it.
    pub plan: String,
    /// The tasks, in the order they were written.
    #[serde(default, rename = "task")]
    pub tasks: Vec<PlanTask>,
}

/// One task in a plan: everything `hird add` takes, with names for numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    /// How the rest of the plan refers to this task. Never reaches the queue.
    pub name: String,
    /// Short title, as the human will refer to it.
    pub title: String,
    /// Full instructions for the agent, as markdown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Higher sorts first when the queue chooses what to hand out.
    #[serde(default, skip_serializing_if = "is_default_priority")]
    pub priority: i64,
    /// Files or globs this task expects to touch, relative to the project root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Names of the tasks in this plan that must finish first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// When this task finishes, file a review of what it changed, barred to
    /// the harness that changed it.
    ///
    /// A plan is where this belongs: which work is worth a second pair of eyes
    /// is a judgement about the shape of the job, made once, in the file you
    /// commit next to the code.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review: bool,
}

fn is_default_priority(priority: &i64) -> bool {
    *priority == 0
}

/// Read a plan from TOML, rejecting anything that could not be filed.
///
/// Parsing and validation are one step on purpose: a `Plan` value that exists
/// is one the queue has already agreed to take, so nothing downstream has to
/// re-check the graph.
pub fn parse(source: &str) -> Result<Plan> {
    let plan: Plan = toml::from_str(source).map_err(|e| {
        // TOML's own error points at the offending line and says what was
        // expected there, which is more use than anything paraphrasing it.
        Error::invalid(format!(
            "this plan file cannot be read: it is TOML — a `plan = \"…\"` line, \
             then one [[task]] block per task.\n\n{}",
            e.to_string().trim_end()
        ))
    })?;
    plan.validate()?;
    Ok(plan)
}

impl Plan {
    /// Render back to TOML, as [`parse`] would read it.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("a validated plan always serializes")
    }

    /// The task with this name, if the plan defines one.
    pub fn task(&self, name: &str) -> Option<&PlanTask> {
        self.tasks.iter().find(|t| t.name == name)
    }

    /// Check everything that can be checked without touching the database.
    ///
    /// Each refusal names the task it is about, because a plan is a file
    /// someone is about to edit and "invalid plan" does not say where to look.
    fn validate(&self) -> Result<()> {
        if self.plan.trim().is_empty() {
            return Err(Error::invalid(
                "a plan needs a name: put `plan = \"…\"` at the top of the file. \
                 It is what lets the same plan be applied twice without filing \
                 everything a second time",
            ));
        }
        if self.tasks.is_empty() {
            return Err(Error::invalid(format!(
                "plan {:?} defines no tasks; add a [[task]] block with a name and \
                 a title",
                self.plan
            )));
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for task in &self.tasks {
            check_name(&task.name)?;
            if !seen.insert(task.name.as_str()) {
                return Err(Error::invalid(format!(
                    "two tasks in plan {:?} are both named {:?}; names are how \
                     `needs` refers to a task, so they have to be distinct",
                    self.plan, task.name
                )));
            }
            if task.title.trim().is_empty() {
                return Err(Error::invalid(format!(
                    "task {:?} has no title; the title is what a human sees on the \
                     board",
                    task.name
                )));
            }
            // Reject unusable globs here rather than halfway through filing.
            normalize_all(&task.paths)?;
        }

        for task in &self.tasks {
            for need in &task.needs {
                if need == &task.name {
                    return Err(Error::invalid(format!(
                        "task {:?} waits for itself; a task cannot be its own \
                         dependency",
                        task.name
                    )));
                }
                if !seen.contains(need.as_str()) {
                    return Err(Error::invalid(format!(
                        "task {:?} waits for {:?}, which no task in plan {:?} \
                         defines{}",
                        task.name,
                        need,
                        self.plan,
                        suggestion(need, &seen)
                    )));
                }
            }
        }

        if let Some(cycle) = self.find_cycle() {
            return Err(Error::invalid(format!(
                "the tasks in plan {:?} wait for each other in a circle: {}. \
                 Nothing in that ring could ever start, so the queue will not \
                 take it",
                self.plan,
                cycle.join(" → ")
            )));
        }
        Ok(())
    }

    /// A dependency ring, as the chain of names that closes it.
    fn find_cycle(&self) -> Option<Vec<String>> {
        // Iterative depth-first search, colouring nodes as it goes: grey is on
        // the current stack, black is finished. An edge back into grey is the
        // ring, and the stack below it is the chain to print.
        let mut colour: BTreeMap<&str, u8> = BTreeMap::new();
        for root in &self.tasks {
            if colour.get(root.name.as_str()).copied().unwrap_or(0) != 0 {
                continue;
            }
            let mut stack: Vec<(&str, usize)> = vec![(root.name.as_str(), 0)];
            colour.insert(root.name.as_str(), 1);
            while let Some((name, index)) = stack.pop() {
                let needs = self.task(name).map(|t| t.needs.as_slice()).unwrap_or(&[]);
                let Some(next) = needs.get(index) else {
                    colour.insert(name, 2);
                    continue;
                };
                stack.push((name, index + 1));
                match colour.get(next.as_str()).copied().unwrap_or(0) {
                    0 => {
                        colour.insert(next.as_str(), 1);
                        stack.push((next.as_str(), 0));
                    }
                    1 => {
                        let from = stack
                            .iter()
                            .position(|(n, _)| *n == next.as_str())
                            .unwrap_or(0);
                        let mut chain: Vec<String> =
                            stack[from..].iter().map(|(n, _)| n.to_string()).collect();
                        chain.push(next.clone());
                        return Some(chain);
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// What the queue would look like if this plan were filed right now.
    pub fn preview(&self) -> Preview {
        // Numbered in file order, so the waves come back in the order someone
        // reading the file expects — and computed by the very same function
        // `hird graph` uses, so the preview cannot drift from the real board.
        let summaries: Vec<TaskSummary> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, task)| TaskSummary {
                seq: i as i64 + 1,
                project: String::new(),
                title: task.title.clone(),
                status: Status::Open,
                priority: task.priority,
                claimed_by: None,
                lease_expires_at: None,
                updated_at: String::new(),
            })
            .collect();
        let index: BTreeMap<&str, i64> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.as_str(), i as i64 + 1))
            .collect();
        let edges: Vec<(i64, i64)> = self
            .tasks
            .iter()
            .flat_map(|task| {
                task.needs.iter().filter_map(|need| {
                    Some((*index.get(task.name.as_str())?, *index.get(need.as_str())?))
                })
            })
            .collect();

        let waves = dispatch_waves(&summaries, &edges)
            .into_iter()
            .map(|wave| {
                wave.into_iter()
                    .filter_map(|seq| self.tasks.get(seq as usize - 1))
                    .map(|t| t.name.clone())
                    .collect()
            })
            .collect();

        Preview {
            waves,
            collisions: self.collisions(),
            unscoped: self
                .tasks
                .iter()
                .filter(|t| t.paths.is_empty())
                .map(|t| t.name.clone())
                .collect(),
        }
    }

    /// Pairs of tasks that declare the same files and are not ordered.
    ///
    /// This is the reading a plan file buys that the queue cannot give you
    /// until the work is live. Two tasks whose globs intersect can never be
    /// worked at the same time — `dispatch_avoids_conflicts` will pass over the
    /// second one — so a plan that looks four-wide is really three-wide plus a
    /// queue. Ordered pairs are left out: a task that waits for the other was
    /// never going to run beside it, and saying so would be noise.
    fn collisions(&self) -> Vec<Collision> {
        let reach = self.reachability();
        let mut out = Vec::new();
        for (i, a) in self.tasks.iter().enumerate() {
            for b in self.tasks.iter().skip(i + 1) {
                let ordered = reach
                    .get(a.name.as_str())
                    .is_some_and(|s| s.contains(b.name.as_str()))
                    || reach
                        .get(b.name.as_str())
                        .is_some_and(|s| s.contains(a.name.as_str()));
                if ordered {
                    continue;
                }
                let overlap = a.paths.iter().find_map(|pa| {
                    b.paths
                        .iter()
                        .find(|pb| glob::intersects(pa, pb))
                        .map(|pb| (pa.clone(), pb.clone()))
                });
                if let Some((pattern, other)) = overlap {
                    out.push(Collision {
                        a: a.name.clone(),
                        b: b.name.clone(),
                        pattern,
                        other_pattern: other,
                    });
                }
            }
        }
        out
    }

    /// For each task, every task it transitively waits for.
    fn reachability(&self) -> BTreeMap<&str, BTreeSet<&str>> {
        let mut reach: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for task in &self.tasks {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut queue: Vec<&str> = task.needs.iter().map(String::as_str).collect();
            while let Some(name) = queue.pop() {
                if !seen.insert(name) {
                    continue;
                }
                if let Some(next) = self.task(name) {
                    queue.extend(next.needs.iter().map(String::as_str));
                }
            }
            reach.insert(task.name.as_str(), seen);
        }
        reach
    }
}

/// What a plan would become, worked out without writing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// Task names by dispatch wave: everything in wave *n* becomes workable
    /// once wave *n-1* is done.
    pub waves: Vec<Vec<String>>,
    /// Tasks that declare overlapping files with nothing ordering them.
    pub collisions: Vec<Collision>,
    /// Tasks that declare no files at all.
    pub unscoped: Vec<String>,
}

impl Preview {
    /// How many tasks can be worked at once at the plan's widest point.
    pub fn widest(&self) -> usize {
        self.waves.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// Two tasks that declare the same files and are not ordered by a dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub a: String,
    pub b: String,
    /// The pattern `a` declared.
    pub pattern: String,
    /// The pattern `b` declared, which describes some of the same paths.
    pub other_pattern: String,
}

impl Collision {
    /// The pair and the overlap, as the preview lists it.
    ///
    /// Terse on purpose: what it means for the plan is said once, above the
    /// list, rather than repeated against every pair.
    pub fn describe(&self) -> String {
        let overlap = if self.pattern == self.other_pattern {
            format!("both declare {}", self.pattern)
        } else {
            format!("{} overlaps {}", self.pattern, self.other_pattern)
        };
        format!("{} and {} — {}", self.a, self.b, overlap)
    }
}

// ------------------------------------------------------------------ helpers

fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::invalid(
            "a task in this plan has no name; every [[task]] needs a `name`, which \
             is how the rest of the plan refers to it",
        ));
    }
    if name.chars().any(char::is_whitespace) {
        return Err(Error::invalid(format!(
            "task name {name:?} contains whitespace; names are identifiers used in \
             `needs`, so keep them to something like \"port-repos\""
        )));
    }
    Ok(())
}

/// A "did you mean" for a `needs` entry that matches nothing.
///
/// Only offered when one candidate is clearly closer than the rest, because a
/// wrong guess costs more than no guess.
fn suggestion(wanted: &str, defined: &BTreeSet<&str>) -> String {
    let mut ranked: Vec<(usize, &str)> = defined
        .iter()
        .map(|name| (distance(wanted, name), *name))
        .collect();
    ranked.sort();
    match ranked.first() {
        Some((d, name)) if *d <= wanted.len().div_ceil(3).max(1) => {
            format!(". Did you mean {name:?}?")
        }
        _ => {
            let names: Vec<String> = defined.iter().map(|n| format!("{n:?}")).collect();
            format!(". This plan defines {}", names.join(", "))
        }
    }
}

/// Levenshtein distance, for the suggestion above and nothing else.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let next = (row[j] + 1).min(row[j + 1] + 1).min(previous + cost);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_plan_reads_back_the_way_it_was_written() {
        let plan = parse(SAMPLE).unwrap();
        assert_eq!(plan.plan, "serde-migration");
        assert_eq!(plan.tasks.len(), 3);
        // File order, not alphabetical: the numbers follow the reading order.
        assert_eq!(plan.tasks[0].name, "schema");
        assert_eq!(plan.tasks[2].name, "renderer");
        assert_eq!(plan.tasks[0].priority, 3);
        assert_eq!(plan.tasks[1].needs, vec!["schema"]);
        assert_eq!(plan.tasks[2].body, "");
    }

    #[test]
    fn a_plan_round_trips_through_toml() {
        let plan = parse(SAMPLE).unwrap();
        let reparsed = parse(&plan.to_toml()).unwrap();
        assert_eq!(plan, reparsed);
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
need = ["b"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("need"), "{err}");
    }

    #[test]
    fn a_dependency_on_nothing_names_the_closest_task() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "schema"
title = "S"
[[task]]
name = "repos"
title = "R"
needs = ["schemas"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"schemas\""), "{err}");
        assert!(err.contains("Did you mean \"schema\""), "{err}");
    }

    #[test]
    fn a_dependency_on_nothing_close_lists_what_exists() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "schema"
title = "S"
[[task]]
name = "repos"
title = "R"
needs = ["renderer"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("defines \"repos\", \"schema\""), "{err}");
    }

    #[test]
    fn a_ring_of_dependencies_is_printed_as_the_ring() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
needs = ["c"]
[[task]]
name = "b"
title = "B"
needs = ["a"]
[[task]]
name = "c"
title = "C"
needs = ["b"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("in a circle"), "{err}");
        for name in ["a", "b", "c"] {
            assert!(err.contains(name), "{err}");
        }
        assert!(err.contains('→'), "{err}");
    }

    #[test]
    fn a_task_cannot_wait_for_itself() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
needs = ["a"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("waits for itself"), "{err}");
    }

    #[test]
    fn duplicate_names_are_refused() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
[[task]]
name = "a"
title = "Also A"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("both named \"a\""), "{err}");
    }

    #[test]
    fn an_unusable_glob_is_refused_before_anything_is_filed() {
        let err = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
paths = ["../outside"]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a usable path pattern"), "{err}");
    }

    #[test]
    fn an_empty_plan_says_what_is_missing() {
        assert!(parse("plan = \"p\"")
            .unwrap_err()
            .to_string()
            .contains("defines no tasks"));
        // No `plan = "…"` line at all: serde says which key is missing, and the
        // message carries the shape of the file around it.
        let err = parse("[[task]]\nname = \"a\"\ntitle = \"A\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing field `plan`"), "{err}");
        assert!(err.contains("[[task]] block per task"), "{err}");
        assert!(
            parse("plan = \"  \"\n[[task]]\nname = \"a\"\ntitle = \"A\"")
                .unwrap_err()
                .to_string()
                .contains("a plan needs a name")
        );
    }

    #[test]
    fn the_preview_lays_the_plan_out_in_waves() {
        let preview = parse(SAMPLE).unwrap().preview();
        assert_eq!(preview.waves.len(), 2);
        assert_eq!(preview.waves[0], vec!["schema", "renderer"]);
        assert_eq!(preview.waves[1], vec!["repos"]);
        assert_eq!(preview.widest(), 2);
    }

    #[test]
    fn a_chain_is_one_task_per_wave() {
        let plan = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
[[task]]
name = "b"
title = "B"
needs = ["a"]
[[task]]
name = "c"
title = "C"
needs = ["b"]
"#,
        )
        .unwrap();
        let preview = plan.preview();
        assert_eq!(preview.waves, vec![vec!["a"], vec!["b"], vec!["c"]]);
        assert_eq!(preview.widest(), 1);
    }

    #[test]
    fn a_task_sits_behind_the_longest_chain_it_waits_on() {
        let plan = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
[[task]]
name = "b"
title = "B"
needs = ["a"]
[[task]]
name = "c"
title = "C"
needs = ["a", "b"]
"#,
        )
        .unwrap();
        // Not wave 2 by way of `a`: the critical path runs through `b`.
        assert_eq!(plan.preview().waves[2], vec!["c"]);
    }

    #[test]
    fn overlapping_unordered_tasks_are_reported() {
        let plan = parse(
            r#"
plan = "p"
[[task]]
name = "renderer"
title = "Rewrite the renderer"
paths = ["src/tui/**"]
[[task]]
name = "audit"
title = "Audit the renderer tests"
paths = ["src/tui/view.rs"]
"#,
        )
        .unwrap();
        let preview = plan.preview();
        assert_eq!(preview.collisions.len(), 1);
        let collision = &preview.collisions[0];
        assert_eq!(collision.a, "renderer");
        assert_eq!(collision.b, "audit");
        assert_eq!(
            collision.describe(),
            "renderer and audit — src/tui/** overlaps src/tui/view.rs"
        );
    }

    #[test]
    fn overlapping_tasks_that_are_ordered_are_not_reported() {
        // Direct, and transitive: neither pair was ever going to run at once.
        let plan = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
paths = ["src/**"]
[[task]]
name = "b"
title = "B"
needs = ["a"]
[[task]]
name = "c"
title = "C"
paths = ["src/db.rs"]
needs = ["b"]
"#,
        )
        .unwrap();
        assert!(plan.preview().collisions.is_empty());
    }

    #[test]
    fn disjoint_patterns_do_not_collide() {
        let plan = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
paths = ["src/*.rs"]
[[task]]
name = "b"
title = "B"
paths = ["src/*.toml"]
"#,
        )
        .unwrap();
        assert!(plan.preview().collisions.is_empty());
    }

    #[test]
    fn tasks_declaring_nothing_are_listed() {
        let preview = parse(
            r#"
plan = "p"
[[task]]
name = "a"
title = "A"
paths = ["src/db.rs"]
[[task]]
name = "notes"
title = "Write the release notes"
"#,
        )
        .unwrap()
        .preview();
        assert_eq!(preview.unscoped, vec!["notes"]);
        assert!(preview.collisions.is_empty());
    }
}
