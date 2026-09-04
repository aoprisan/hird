//! The digest: what happened on the board while nobody was looking.
//!
//! `hird events` prints the trail and `hird ls` prints the present, and a
//! human who steps away for an afternoon wants neither — they want the
//! difference, folded into sentences: which tasks finished, which were sent
//! back, who is waiting on an answer, whose lease died. That is a fold over
//! the same events the feed already prints, from the cursor they last saw to
//! now, grouped by what it means rather than when it landed.
//!
//! The fold is pure so it can be tested without a database; the bookmark
//! that says "last seen" lives in [`crate::repo::Bookmarks`].

use std::collections::BTreeMap;

use crate::fmt;
use crate::model::EventKind;
use crate::repo::FeedEvent;

/// One task's worth of news, in the order the digest reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskNews {
    pub seq: i64,
    pub title: String,
    pub project: String,
    pub filed: bool,
    /// The result it finished with, if it finished.
    pub finished: Option<String>,
    /// The reason it failed, if it failed.
    pub failed: Option<String>,
    pub cancelled: bool,
    /// The findings it was sent back with, if a review sent it back.
    pub sent_back: Option<String>,
    pub upheld: bool,
    /// A human reopened it, with the reason.
    pub reopened: Option<String>,
    /// The question it stepped out of dispatch to ask, if still the latest
    /// thing it did on that front.
    pub asked: Option<String>,
    pub answered: bool,
    /// Holders who lost it to the clock, in order.
    pub expired: Vec<String>,
    /// Who released it back unfinished.
    pub released: Vec<String>,
    /// Who claimed it, in order.
    pub claimed_by: Vec<String>,
    /// Whether the last thing that happened to its lease was a claim — that
    /// is, whether somebody still holds it as of the end of the window.
    pub held: bool,
    /// Distinct files the witness saw move under it.
    pub changed: Vec<String>,
    pub ground_shifted: bool,
    /// Every event on this task in the window.
    pub events: usize,
}

/// The whole window, folded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Digest {
    /// Tasks with news, in queue order.
    pub tasks: Vec<TaskNews>,
    pub events: usize,
    /// The first and last event times in the window.
    pub from: Option<String>,
    pub to: Option<String>,
    /// The last cursor folded, for the bookmark.
    pub cursor: Option<i64>,
}

/// Fold a run of events, oldest first, into per-task news.
pub fn fold(events: &[FeedEvent]) -> Digest {
    let mut by_task: BTreeMap<i64, TaskNews> = BTreeMap::new();
    for event in events {
        let news = by_task.entry(event.task_seq).or_insert_with(|| TaskNews {
            seq: event.task_seq,
            title: event.task_title.clone(),
            project: event.project.clone(),
            ..TaskNews::default()
        });
        news.events += 1;
        let detail = event.detail.trim().to_string();
        match event.kind {
            EventKind::Created => news.filed = true,
            EventKind::Claimed => {
                news.claimed_by.push(event.actor.clone());
                news.held = true;
                news.asked = None;
            }
            EventKind::Completed => {
                news.held = false;
                news.finished = Some(detail);
                news.failed = None;
                news.sent_back = None;
            }
            EventKind::Failed => {
                news.held = false;
                news.failed = Some(detail);
                news.finished = None;
            }
            EventKind::Cancelled => {
                news.held = false;
                news.cancelled = true;
            }
            EventKind::Reopened => {
                news.held = false;
                news.finished = None;
                news.failed = None;
                news.cancelled = false;
                match detail.strip_prefix("sent back by review ") {
                    Some(rest) => {
                        let findings = rest.split_once(": ").map(|(_, f)| f).unwrap_or(rest);
                        news.sent_back = Some(findings.to_string());
                        news.upheld = false;
                    }
                    None => news.reopened = Some(detail),
                }
            }
            EventKind::Reviewed if detail.starts_with("upheld by review") => {
                news.upheld = true;
                news.sent_back = None;
            }
            EventKind::Asked => {
                news.held = false;
                news.asked = Some(detail);
                news.answered = false;
            }
            EventKind::Answered => {
                news.asked = None;
                news.answered = true;
            }
            EventKind::LeaseExpired => {
                let holder = detail
                    .strip_prefix("lease held by ")
                    .and_then(|r| r.split_once(' ').map(|(h, _)| h))
                    .unwrap_or("unknown");
                news.expired.push(holder.to_string());
                news.held = false;
            }
            EventKind::Released => {
                news.held = false;
                news.released.push(event.actor.clone());
            }
            EventKind::Witnessed => {
                if let Some(list) = detail
                    .strip_prefix("saw ")
                    .and_then(|r| r.strip_suffix(" change"))
                {
                    for path in list.split(", ") {
                        if !news.changed.iter().any(|p| p == path) {
                            news.changed.push(path.to_string());
                        }
                    }
                }
            }
            EventKind::GroundShifted => news.ground_shifted = true,
            _ => {}
        }
    }
    Digest {
        tasks: by_task.into_values().collect(),
        events: events.len(),
        from: events.first().map(|e| e.at.clone()),
        to: events.last().map(|e| e.at.clone()),
        cursor: events.last().map(|e| e.cursor),
    }
}

impl Digest {
    /// The digest as paragraphs, one heading per kind of news. Empty when
    /// there is no news, so the caller can say so in its own words.
    pub fn render(&self, all_projects: bool) -> Vec<String> {
        let name = |t: &TaskNews| {
            let project = if all_projects {
                format!("  [{}]", t.project)
            } else {
                String::new()
            };
            format!("#{} {}{project}", t.seq, fmt::truncate(&t.title, 56))
        };
        let mut sections: Vec<String> = Vec::new();
        let mut section = |heading: &str, lines: Vec<String>| {
            if !lines.is_empty() {
                sections.push(format!("{heading}\n{}", lines.join("\n")));
            }
        };

        section(
            "finished",
            self.tasks
                .iter()
                .filter_map(|t| {
                    let result = t.finished.as_ref()?;
                    let verdict = if t.upheld { "  (upheld)" } else { "" };
                    Some(format!(
                        "  {}{verdict}\n      {}",
                        name(t),
                        fmt::truncate(result, 80)
                    ))
                })
                .collect(),
        );
        section(
            "sent back",
            self.tasks
                .iter()
                .filter_map(|t| {
                    let findings = t.sent_back.as_ref()?;
                    Some(format!(
                        "  {}\n      {}",
                        name(t),
                        fmt::truncate(findings, 80)
                    ))
                })
                .collect(),
        );
        section(
            "failed",
            self.tasks
                .iter()
                .filter_map(|t| {
                    let reason = t.failed.as_ref()?;
                    Some(format!(
                        "  {}\n      {}",
                        name(t),
                        fmt::truncate(reason, 80)
                    ))
                })
                .collect(),
        );
        section(
            "waiting on you",
            self.tasks
                .iter()
                .filter_map(|t| {
                    let question = t.asked.as_ref()?;
                    Some(format!(
                        "  {}\n      {}\n      hird answer {} <ANSWER>",
                        name(t),
                        fmt::truncate(question, 80),
                        t.seq
                    ))
                })
                .collect(),
        );
        section(
            "leases expired",
            self.tasks
                .iter()
                .filter(|t| !t.expired.is_empty())
                .map(|t| format!("  {}   held by {}", name(t), t.expired.join(", ")))
                .collect(),
        );
        section(
            "handed back",
            self.tasks
                .iter()
                .filter(|t| !t.released.is_empty())
                .map(|t| format!("  {}   by {}", name(t), t.released.join(", ")))
                .collect(),
        );
        section(
            "ground shifted",
            self.tasks
                .iter()
                .filter(|t| t.ground_shifted)
                .map(|t| format!("  {}", name(t)))
                .collect(),
        );
        section(
            "reopened",
            self.tasks
                .iter()
                .filter_map(|t| {
                    let reason = t.reopened.as_ref()?;
                    Some(format!("  {}   {}", name(t), fmt::truncate(reason, 60)))
                })
                .collect(),
        );
        section(
            "cancelled",
            self.tasks
                .iter()
                .filter(|t| t.cancelled)
                .map(|t| format!("  {}", name(t)))
                .collect(),
        );
        section(
            "claimed",
            self.tasks
                .iter()
                .filter(|t| t.held)
                .map(|t| {
                    let last = t.claimed_by.last().cloned().unwrap_or_default();
                    let hands = if t.claimed_by.len() > 1 {
                        format!(" ({} hands)", t.claimed_by.len())
                    } else {
                        String::new()
                    };
                    let files = if t.changed.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", {} {}",
                            t.changed.len(),
                            if t.changed.len() == 1 {
                                "file changed"
                            } else {
                                "files changed"
                            }
                        )
                    };
                    format!("  {}   {last}{hands}{files}", name(t))
                })
                .collect(),
        );
        section(
            "filed",
            self.tasks
                .iter()
                .filter(|t| t.filed && t.claimed_by.is_empty())
                .map(|t| format!("  {}", name(t)))
                .collect(),
        );
        sections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(cursor: i64, seq: i64, kind: EventKind, actor: &str, detail: &str) -> FeedEvent {
        FeedEvent {
            cursor,
            at: format!("2026-08-14T10:00:{cursor:02}.000Z"),
            actor: actor.into(),
            kind,
            detail: detail.into(),
            task_seq: seq,
            task_title: format!("task {seq}"),
            project: "/p".into(),
        }
    }

    #[test]
    fn a_finished_task_is_finished_and_a_sent_back_one_is_not() {
        let events = vec![
            ev(1, 1, EventKind::Created, "cli", ""),
            ev(2, 1, EventKind::Claimed, "codex:1", ""),
            ev(
                3,
                1,
                EventKind::Witnessed,
                "codex:1",
                "saw src/a.rs, src/b.rs change",
            ),
            ev(4, 1, EventKind::Completed, "codex:1", "ported it"),
            ev(5, 2, EventKind::Completed, "claude:2", "reviewed"),
            ev(
                6,
                1,
                EventKind::Reopened,
                "claude:2",
                "sent back by review 2: tests missing",
            ),
            ev(7, 3, EventKind::Created, "cli", ""),
        ];
        let digest = fold(&events);
        assert_eq!(digest.events, 7);
        assert_eq!(digest.cursor, Some(7));
        let one = &digest.tasks[0];
        assert_eq!(one.finished, None);
        assert_eq!(one.sent_back.as_deref(), Some("tests missing"));
        assert_eq!(one.changed, vec!["src/a.rs", "src/b.rs"]);
        let rendered = digest.render(false).join("\n\n");
        assert!(
            rendered.contains("sent back\n  #1 task 1\n      tests missing"),
            "{rendered}"
        );
        assert!(rendered.contains("finished\n  #2 task 2"), "{rendered}");
        assert!(rendered.contains("filed\n  #3 task 3"), "{rendered}");
        assert!(!rendered.contains("claimed\n"), "{rendered}");
    }

    #[test]
    fn a_question_waits_until_answered_and_an_expiry_names_the_holder() {
        let events = vec![
            ev(1, 1, EventKind::Asked, "codex:1", "which port?"),
            ev(2, 2, EventKind::Asked, "codex:1", "which db?"),
            ev(3, 2, EventKind::Answered, "cli", "sqlite"),
            ev(
                4,
                3,
                EventKind::LeaseExpired,
                "hird",
                "lease held by copilot:9 expired; task returned to open",
            ),
        ];
        let digest = fold(&events);
        let rendered = digest.render(false).join("\n\n");
        assert!(
            rendered.contains(
                "waiting on you\n  #1 task 1\n      which port?\n      hird answer 1 <ANSWER>"
            ),
            "{rendered}"
        );
        assert!(!rendered.contains("which db?"), "{rendered}");
        assert!(
            rendered.contains("leases expired\n  #3 task 3   held by copilot:9"),
            "{rendered}"
        );
    }

    #[test]
    fn nothing_folds_to_nothing() {
        let digest = fold(&[]);
        assert!(digest.render(false).is_empty());
        assert_eq!(digest.cursor, None);
    }
}
