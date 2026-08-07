//! The exhibit: reading back what the witness kept.
//!
//! The witness records *that* files moved under a task; the exhibit is the
//! content behind those records. Every version the witness fingerprints is
//! kept, content-addressed, in the same database as everything else — so the
//! question "what did this task actually change?" has an answer that is a
//! diff rather than a list of names, and it keeps having one after the tree
//! has moved on, after the task is done, after the file has been written
//! over by somebody else. Nothing here was committed; that is the point. Git
//! remembers what was committed. The exhibit remembers what happened between
//! commits, which is where agents live.
//!
//! Three readers:
//!
//! - `hird diff` — the uncommitted diff of what moved under a task.
//! - The review a completion files, which now carries the diff of the work
//!   under review instead of asking the reviewer to guess it from file names.
//! - `hird salvage` — the last version the witness saw of a file under a
//!   task, which is what turns "your edit discarded theirs" from a diagnosis
//!   into a recovery.
//!
//! The exhibit answers with exactly the confidence the evidence has earned,
//! the way every witness surface does. A version it never kept — too large,
//! pruned, observed before the exhibit existed — is reported as not kept,
//! never guessed at; and "the last version the witness saw" is the honest
//! name for what a salvage returns, because a version that came and went
//! between two observations was never seen at all.

use std::path::Path;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::model::Observed;
use crate::witness::Witness;

/// One side of a file's diff: what the content was, as far as hird can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    /// The content, as kept or as read.
    Bytes(Vec<u8>),
    /// The file did not exist on this side — the empty side of an addition
    /// or a deletion, which is an answer rather than an absence of one.
    Absent,
    /// hird was in no position to keep this version, and says why.
    Unkept(String),
}

impl Side {
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Side::Bytes(b) => Some(b),
            Side::Absent => Some(&[]),
            Side::Unkept(_) => None,
        }
    }
}

/// One changed file, resolved to content on both sides.
#[derive(Debug, Clone)]
pub struct FileExhibit {
    pub path: String,
    /// `added`, `modified` or `deleted` — the change record's word.
    pub kind: String,
    /// As the task found it.
    pub before: Side,
    /// As the record last saw it.
    pub after: Side,
}

/// Everything the witness can show for one task, one entry per changed file.
///
/// Errors only when hird was never watching the task at all — no baseline
/// means no "before" and nothing was kept. A watched task that changed
/// nothing comes back empty, which is the read-only answer in diff form.
pub fn assemble(db: &Db, witness: &Witness, seq: i64) -> Result<Vec<FileExhibit>> {
    let Some(baseline) = db.witnessed().baseline_of(seq)? else {
        return Err(Error::invalid(format!(
            "hird was not watching task {seq} — no baseline was taken, so nothing was kept"
        )));
    };
    let task = db.tasks().get(seq)?;
    let live = task.status.is_active();
    Ok(assemble_from(
        db,
        witness,
        &baseline,
        db.witnessed().touched(seq)?,
        live,
    ))
}

/// The same answer for an archived round of the task — a holding that ended
/// and was replaced by a later claim.
///
/// Never live: an archived tenure is over by definition, so its "after" is
/// the last version the witness saw under it, not the disk as it stands. This
/// is what makes the answer stable — the diff of what round one did reads the
/// same before and after round two rewrites everything.
pub fn assemble_tenure(db: &Db, witness: &Witness, seq: i64, n: i64) -> Result<Vec<FileExhibit>> {
    let Some(baseline) = db.witnessed().tenure_baseline(seq, n)? else {
        let held = db.witnessed().tenures(seq)?;
        return Err(Error::invalid(if held.is_empty() {
            format!("task {seq} has no archived holdings — it has never changed hands under the witness")
        } else {
            format!(
                "task {seq} has no archived holding {n}; it has {} — `hird show {seq}` lists them",
                held.len()
            )
        }));
    };
    let changes = db
        .witnessed()
        .tenures(seq)?
        .into_iter()
        .find(|t| t.n == n)
        .map(|t| t.changes)
        .unwrap_or_default();
    Ok(assemble_from(db, witness, &baseline, changes, false))
}

fn assemble_from(
    db: &Db,
    witness: &Witness,
    baseline: &crate::repo::Baseline,
    observed: Vec<Observed>,
    live: bool,
) -> Vec<FileExhibit> {
    observed
        .into_iter()
        .map(|observed| {
            let before = before_side(db, witness, baseline, &observed);
            let after = after_side(db, witness, &observed, live);
            FileExhibit {
                path: observed.path,
                kind: observed.kind,
                before,
                after,
            }
        })
        .collect()
}

/// The content of `observed.path` as task `seq` found it at claim time.
fn before_side(
    db: &Db,
    witness: &Witness,
    baseline: &crate::repo::Baseline,
    observed: &Observed,
) -> Side {
    if observed.kind == "added" {
        return Side::Absent;
    }
    match baseline.tree.entries.get(&observed.path) {
        // Fingerprinted at claim: the file was already dirty, so the version
        // it held then exists nowhere but the exhibit.
        Some(hash) if hash.is_empty() => Side::Absent,
        Some(hash) if hash.starts_with("size:") => {
            Side::Unkept("too large for the witness to keep".to_string())
        }
        Some(hash) => match db.witnessed().blob(hash) {
            Ok(Some(bytes)) => Side::Bytes(bytes),
            _ => Side::Unkept("the version at claim time is no longer kept".to_string()),
        },
        // Clean at claim: git itself holds that version, and asking it is
        // better evidence than any copy would have been.
        None => match witness.content_at(&baseline.tree.head, &observed.path) {
            Some(bytes) => Side::Bytes(bytes),
            None => Side::Unkept(format!(
                "not in the repository at the claim's commit ({})",
                short(&baseline.tree.head)
            )),
        },
    }
}

/// The content of `observed.path` as the record last saw it under this task.
fn after_side(db: &Db, witness: &Witness, observed: &Observed, live: bool) -> Side {
    if observed.kind == "deleted" {
        return Side::Absent;
    }
    if observed.hash.starts_with("size:") {
        return Side::Unkept("too large for the witness to keep".to_string());
    }
    if live {
        // A live task's diff should end at the tree as it stands — the honest
        // now — not at the last heartbeat.
        return match witness.read(&observed.path) {
            Some(bytes) => Side::Bytes(bytes),
            None => Side::Unkept("unreadable in the working tree right now".to_string()),
        };
    }
    match db.witnessed().blob(&observed.hash) {
        Ok(Some(bytes)) => Side::Bytes(bytes),
        // Not kept, but if the tree still holds exactly that version, the
        // disk is as good as the store.
        _ if witness.fingerprint(&observed.path) == observed.hash => {
            match witness.read(&observed.path) {
                Some(bytes) => Side::Bytes(bytes),
                None => Side::Unkept("the final version was not kept".to_string()),
            }
        }
        _ => Side::Unkept("the final version was not kept".to_string()),
    }
}

/// The content of one file as it stood under one task, for `hird salvage`.
///
/// `at_claim` asks for the version the task started from; otherwise the
/// answer is the last version the witness saw on the task's record — which
/// is the version about to be lost, or already lost, when another agent's
/// write lands on the same file. `tenure` reaches an archived round instead
/// of the current record: the version a vanished holder left behind is
/// salvageable after its successor has claimed, worked and overwritten it.
/// Every refusal names what the witness can and cannot say, because a
/// salvage that guessed would be worse than none.
pub fn salvage(
    db: &Db,
    witness: &Witness,
    seq: i64,
    path: &str,
    at_claim: bool,
    tenure: Option<i64>,
) -> Result<Vec<u8>> {
    if let Some(n) = tenure {
        return salvage_from(
            db,
            witness,
            seq,
            path,
            at_claim,
            db.witnessed().tenure_baseline(seq, n)?.ok_or_else(|| {
                Error::invalid(format!(
                    "task {seq} has no archived holding {n} — `hird show {seq}` lists what it has"
                ))
            })?,
            db.witnessed()
                .tenures(seq)?
                .into_iter()
                .find(|t| t.n == n)
                .map(|t| t.changes)
                .unwrap_or_default(),
        );
    }
    let Some(baseline) = db.witnessed().baseline_of(seq)? else {
        return Err(Error::invalid(format!(
            "hird was not watching task {seq} — no baseline was taken, so nothing was kept"
        )));
    };
    let observed = db.witnessed().touched(seq)?;
    salvage_from(db, witness, seq, path, at_claim, baseline, observed)
}

fn salvage_from(
    db: &Db,
    witness: &Witness,
    seq: i64,
    path: &str,
    at_claim: bool,
    baseline: crate::repo::Baseline,
    observed: Vec<Observed>,
) -> Result<Vec<u8>> {
    if at_claim {
        return match baseline.tree.entries.get(path) {
            Some(hash) if hash.is_empty() => Err(Error::invalid(format!(
                "{path} did not exist when task {seq} was claimed"
            ))),
            Some(hash) if hash.starts_with("size:") => Err(Error::invalid(format!(
                "{path} was too large for the witness to keep"
            ))),
            Some(hash) => db.witnessed().blob(hash)?.ok_or_else(|| {
                Error::invalid(format!(
                    "the version of {path} at task {seq}'s claim is no longer kept"
                ))
            }),
            None => witness
                .content_at(&baseline.tree.head, path)
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "{path} was clean when task {seq} was claimed, and commit {} does not \
                     have it either",
                        short(&baseline.tree.head)
                    ))
                }),
        };
    }
    let Some(row) = observed.iter().find(|o| o.path == path) else {
        let saw: Vec<&str> = observed.iter().map(|o| o.path.as_str()).collect();
        return Err(Error::invalid(if saw.is_empty() {
            format!("the witness saw nothing move under task {seq}")
        } else {
            format!(
                "the witness never saw {path} move under task {seq}; it saw: {}",
                saw.join(", ")
            )
        }));
    };
    if row.hash.is_empty() {
        return Err(Error::invalid(format!(
            "the witness last saw {path} deleted under task {seq}; --baseline has the \
             version from before the claim"
        )));
    }
    if row.hash.starts_with("size:") {
        return Err(Error::invalid(format!(
            "{path} was too large for the witness to keep"
        )));
    }
    db.witnessed().blob(&row.hash)?.ok_or_else(|| {
        Error::invalid(format!(
            "the last version the witness saw of {path} under task {seq} is no longer kept"
        ))
    })
}

/// Render exhibits as one unified diff, the way `git diff` would print it.
///
/// A file whose two sides are both in hand becomes an ordinary hunked diff; a
/// file with an unkept side becomes one line saying which side is missing and
/// why, because a diff that silently skipped it would read as "unchanged".
pub fn render(exhibits: &[FileExhibit]) -> String {
    let mut out = String::new();
    for exhibit in exhibits {
        match (&exhibit.before, &exhibit.after) {
            (Side::Unkept(reason), _) => {
                out.push_str(&format!(
                    "{} ({}) — before-version {reason}; no diff to show\n",
                    exhibit.path, exhibit.kind
                ));
            }
            (_, Side::Unkept(reason)) => {
                out.push_str(&format!(
                    "{} ({}) — after-version {reason}; no diff to show\n",
                    exhibit.path, exhibit.kind
                ));
            }
            (before, after) => {
                let (b, a) = (before.bytes().unwrap_or(&[]), after.bytes().unwrap_or(&[]));
                if b == a {
                    continue;
                }
                match unified(&exhibit.path, b, a) {
                    Some(text) => out.push_str(&text),
                    None => out.push_str(&format!(
                        "{} ({}) — diff could not be rendered\n",
                        exhibit.path, exhibit.kind
                    )),
                }
            }
        }
    }
    out
}

/// The most diff a review brief will carry before pointing at `hird diff`.
///
/// A brief lands in a model's context unasked, so it gets the head of the
/// change and a pointer to the rest, not everything the witness kept.
pub const BRIEF_MAX_BYTES: usize = 24 * 1024;

/// `render`, cut down to what a task brief can carry.
///
/// A review brief lands in a model's context unasked, so the diff it carries
/// is bounded — and a bound that goes unmentioned reads exactly like a diff
/// that found nothing more, so the cut names itself and says where the rest
/// lives.
pub fn clipped(text: &str, max_bytes: usize, seq: i64) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = 0;
    for line in text.split_inclusive('\n') {
        if cut + line.len() > max_bytes {
            break;
        }
        cut += line.len();
    }
    format!(
        "{}… clipped — `hird diff {seq}` has the whole change\n",
        &text[..cut]
    )
}

/// A unified diff between two versions of one file, via `git diff --no-index`.
///
/// Git is already the witness's prerequisite, and its diff is the one every
/// reader of this output already knows how to read — hunk headers, `a/` and
/// `b/` prefixes, "Binary files differ" — so hird borrows it rather than
/// approximating it. The two versions are laid out in a scratch directory
/// git is pointed at, and the temp paths are rewritten back to `a/<path>`
/// and `b/<path>` so the output never mentions where it was staged.
fn unified(path: &str, before: &[u8], after: &[u8]) -> Option<String> {
    let stage = std::env::temp_dir().join(format!("hird-exhibit-{}", ulid::Ulid::generate()));
    let result = unified_in(&stage, path, before, after);
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn unified_in(stage: &Path, path: &str, before: &[u8], after: &[u8]) -> Option<String> {
    for (side, content) in [("before", before), ("after", after)] {
        let full = stage.join(side).join(path);
        std::fs::create_dir_all(full.parent()?).ok()?;
        std::fs::write(&full, content).ok()?;
    }
    let before_rel = format!("before/{path}");
    let after_rel = format!("after/{path}");
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-index",
            "--no-color",
            "--",
            &before_rel,
            &after_rel,
        ])
        .current_dir(stage)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    // `--no-index` answers 0 for identical and 1 for different; anything else
    // is git refusing, which for the exhibit is "cannot render".
    if !matches!(output.status.code(), Some(0 | 1)) {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Git's own `a/` and `b/` prefixes wrap the staged paths, so strip the
    // staging layer out of the prefixed forms first and the bare forms after.
    Some(
        text.replace(&format!("a/{before_rel}"), &format!("a/{path}"))
            .replace(&format!("b/{after_rel}"), &format!("b/{path}"))
            .replace(&before_rel, &format!("a/{path}"))
            .replace(&after_rel, &format!("b/{path}")),
    )
}

/// The seven-character commit people are used to reading.
fn short(head: &str) -> &str {
    if head.len() >= 7 {
        &head[..7]
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unified_diff_reads_like_gits_own() {
        let text = unified("src/config.rs", b"a\nb\nc\n", b"a\nB\nc\n").unwrap();
        assert!(text.contains("a/src/config.rs"), "{text}");
        assert!(text.contains("b/src/config.rs"), "{text}");
        assert!(text.contains("-b"), "{text}");
        assert!(text.contains("+B"), "{text}");
        assert!(
            !text.contains("before/"),
            "temp layout must not leak: {text}"
        );
        assert!(
            !text.contains("a/a/"),
            "git's own prefix must not stack: {text}"
        );
    }

    #[test]
    fn an_addition_diffs_from_nothing() {
        let text = unified("new.rs", b"", b"fn new() {}\n").unwrap();
        assert!(text.contains("+fn new() {}"), "{text}");
    }

    #[test]
    fn binary_content_is_declared_not_dumped() {
        let text = unified("blob.bin", &[0u8, 1, 2, 3], &[0u8, 9, 9, 9]).unwrap();
        assert!(text.contains("differ"), "{text}");
        assert!(!text.contains("before/"), "{text}");
    }

    #[test]
    fn clipping_names_itself_and_points_home() {
        let long = "line one\nline two\nline three\n";
        let clipped = clipped(long, 10, 42);
        assert!(clipped.starts_with("line one\n"), "{clipped}");
        assert!(clipped.contains("clipped"), "{clipped}");
        assert!(clipped.contains("hird diff 42"), "{clipped}");
        // And a short diff is left exactly alone.
        assert_eq!(super::clipped(long, 1000, 42), long);
    }
}
