//! The witness: what the working tree says actually happened.
//!
//! Everything else in `hird` is what agents *say*. A task's status is what its
//! holder claimed, its file scope is what its holder predicted, and its result
//! is a sentence the holder wrote about itself. That is fine when the board is
//! read by a human who can go and look — and it is exactly the wrong shape when
//! three agents share one checkout, because the failure that costs you work is
//! the one nobody reports: two agents editing the same file, the second write
//! landing on top of the first, both of them completing successfully.
//!
//! So the witness goes and looks. At claim time it records a fingerprint of the
//! files that could plausibly change — one content hash each — and at every
//! check-in it takes another and subtracts. What comes out is not a prediction
//! and not a summary an agent wrote about itself: it is the set of files that
//! moved while the task was held, straight off the disk.
//!
//! It is careful about what that proves. One checkout has one filesystem and no
//! keyboards, so a change that happens while three agents are live belongs to
//! all three footprints and hird will not pretend otherwise. Saying *who* needs
//! the other half of the picture — the file scopes agents declare — and the
//! place the two meet is [`crate::repo::Witnessed::contention`]: a file two
//! agents both said they would write, which has since moved under both of them.
//! That is the predicted collision and the observed one at once, and it is the
//! failure a status machine cannot see.
//!
//! Three rules keep it from being a liability:
//!
//! - **It never fails a call.** No git, no repository, a `git` that hangs — the
//!   witness goes quiet and the queue behaves exactly as it did before it
//!   existed. [`Witness::discover`] returning `None` is a supported outcome.
//! - **It watches what git already tracks.** Candidate paths come from
//!   `git status`, so `.gitignore` decides what is noise, and a clean checkout
//!   costs one process and no hashing at all. No live task in the project, and
//!   [`sweep`] does not run git at all.
//! - **It records facts, not judgements.** Observed paths live in their own
//!   table and are never folded into what an agent declared. Declared scope is
//!   intent; witnessed scope is history; the interesting reports are the places
//!   where the two disagree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::hash::sha256_hex;

/// Files larger than this are fingerprinted by size and mtime rather than by
/// content. A source file is never this big; a checked-in fixture might be, and
/// hashing it on every heartbeat would be the slowest thing hird does.
const MAX_HASH_BYTES: u64 = 4 * 1024 * 1024;

/// Upper bound on how many paths one observation will look at.
///
/// A working tree dirtier than this is a rebase or a `cargo build` into a
/// non-ignored directory, not agent work, and hashing it would stall a claim.
const MAX_WATCHED_PATHS: usize = 4096;

/// How a file differs between two observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        }
    }
}

impl std::str::FromStr for ChangeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<ChangeKind, String> {
        match s {
            "added" => Ok(ChangeKind::Added),
            "modified" => Ok(ChangeKind::Modified),
            "deleted" => Ok(ChangeKind::Deleted),
            other => Err(format!("unknown change kind {other:?}")),
        }
    }
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(self.as_str())
    }
}

/// One file, as it differs between two observations of the same tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Project-relative, `/`-separated.
    pub path: String,
    pub kind: ChangeKind,
    /// The content hash *after* the change; empty for a deletion.
    pub hash: String,
}

/// A fingerprint of the working tree, taken at one instant.
///
/// Only interesting paths are in `entries` — a file the tree has never had
/// reason to look at is absent, which is not the same as "does not exist". Two
/// trees are only ever compared over the union of their key sets, and
/// [`Witness::observe`] is told which paths a comparison will need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// `HEAD` at the time of the observation; empty in a repository with no
    /// commits yet. Recorded so that a later observation can ask git what a
    /// commit made in between touched.
    pub head: String,
    /// Project-relative path to content hash. A path present here with an
    /// empty hash was looked at and found missing.
    pub entries: BTreeMap<String, String>,
    /// Set when the working tree had more dirty paths than the witness will
    /// look at in one go, so `entries` is not the whole story.
    pub truncated: bool,
}

impl Tree {
    /// The hash recorded for `path`, or `""` if the file was absent.
    fn get(&self, path: &str) -> Option<&str> {
        self.entries.get(path).map(String::as_str)
    }

    /// Every path this tree has an opinion about.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// A working tree the witness can observe.
///
/// Construction is the whole feature check: if [`Witness::discover`] hands back
/// a value, git works here and observations will succeed.
#[derive(Debug, Clone)]
pub struct Witness {
    root: PathBuf,
}

impl Witness {
    /// A witness for `root`, or `None` if this is not somewhere git can answer
    /// questions about — no repository, no `git` on `PATH`, a project path that
    /// belongs to another machine.
    ///
    /// Deliberately total: every caller treats `None` as "witnessing is off"
    /// rather than as an error, because a queue that stops working when git is
    /// missing would be a worse queue than one that stops watching.
    pub fn discover(root: &Path) -> Option<Witness> {
        if !root.is_dir() {
            return None;
        }
        let inside = git(root, &["rev-parse", "--is-inside-work-tree"])?;
        if inside.trim() != "true" {
            return None;
        }
        Some(Witness {
            root: root.to_path_buf(),
        })
    }

    /// The tree this witness watches.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Fingerprint the tree now.
    ///
    /// `also` names paths the caller needs an answer for whatever their state —
    /// the paths of an earlier observation, so that the two can be subtracted
    /// even for files that have since been committed and gone clean. `heads`
    /// are the commits earlier observations were taken at: if the tree has moved
    /// on from one of them, whatever that move touched is a candidate too, so a
    /// task that commits its own work is still seen to have done it.
    pub fn observe(&self, also: &[String], heads: &[String]) -> Tree {
        let head = git(&self.root, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let mut candidates: BTreeSet<String> = BTreeSet::new();
        candidates.extend(self.dirty_paths());
        candidates.extend(also.iter().cloned());
        for earlier in heads {
            if earlier.is_empty() || *earlier == head || head.is_empty() {
                continue;
            }
            candidates.extend(self.paths_changed_between(earlier, &head));
        }

        let truncated = candidates.len() > MAX_WATCHED_PATHS;
        let mut entries = BTreeMap::new();
        for path in candidates.into_iter().take(MAX_WATCHED_PATHS) {
            entries.insert(path.clone(), self.fingerprint(&path));
        }
        Tree {
            head,
            entries,
            truncated,
        }
    }

    /// Paths git reports as differing from `HEAD`, including untracked files.
    ///
    /// `.gitignore` therefore decides what counts as noise, which is the right
    /// answer and one the witness does not have to reimplement.
    fn dirty_paths(&self) -> Vec<String> {
        let Some(raw) = git(
            &self.root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ],
        ) else {
            return Vec::new();
        };
        // NUL-separated `XY <path>` records. `--no-renames` means no record
        // ever carries the second path that would otherwise follow.
        raw.split('\0')
            .filter(|record| record.len() > 3)
            .map(|record| record[3..].to_string())
            .filter(|path| !path.is_empty())
            .collect()
    }

    /// What changed between two observations of the same working tree.
    ///
    /// A path `before` has an entry for is decided by comparing hashes, which
    /// needs nothing but the two trees. A path it does not is the ordinary
    /// case — the tree was clean when the task started, so nothing was dirty to
    /// fingerprint — and there the question of whether a file is *new* or
    /// *edited* is settled by asking the commit `before` was taken at.
    pub fn diff(&self, before: &Tree, after: &Tree) -> Vec<Change> {
        let unexamined: Vec<&str> = after
            .entries
            .keys()
            .filter(|path| !before.entries.contains_key(*path))
            .map(String::as_str)
            .collect();
        let existed = self.tracked_at(&before.head, &unexamined);

        let mut out = Vec::new();
        for (path, now) in &after.entries {
            let kind = match (before.get(path), now.as_str()) {
                // Fingerprinted before, so the two hashes settle it.
                (Some(""), "") => continue,
                (Some(""), _) => ChangeKind::Added,
                (Some(_), "") => ChangeKind::Deleted,
                (Some(was), is) if was == is => continue,
                (Some(_), _) => ChangeKind::Modified,
                // Not fingerprinted before, so it was clean: it is whatever the
                // snapshot's commit says it was.
                (None, "") if existed.contains(path.as_str()) => ChangeKind::Deleted,
                (None, "") => continue,
                (None, _) if existed.contains(path.as_str()) => ChangeKind::Modified,
                (None, _) => ChangeKind::Added,
            };
            out.push(Change {
                path: path.clone(),
                kind,
                hash: now.clone(),
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Which of `paths` existed in commit `head`.
    ///
    /// One `ls-tree` per batch rather than one per path, and none at all when
    /// there is nothing ambiguous to resolve or no commit to resolve it against.
    fn tracked_at<'p>(&self, head: &str, paths: &[&'p str]) -> BTreeSet<&'p str> {
        let mut found = BTreeSet::new();
        if head.is_empty() || paths.is_empty() {
            return found;
        }
        // Batched so a very dirty tree cannot overflow the argument list.
        for batch in paths.chunks(256) {
            let mut args = vec!["ls-tree", "-r", "-z", "--name-only", head, "--"];
            args.extend_from_slice(batch);
            let Some(raw) = git(&self.root, &args) else {
                continue;
            };
            for listed in raw.split('\0').filter(|p| !p.is_empty()) {
                if let Some(hit) = batch.iter().find(|p| **p == listed) {
                    found.insert(*hit);
                }
            }
        }
        found
    }

    /// Paths that differ between two commits.
    fn paths_changed_between(&self, from: &str, to: &str) -> Vec<String> {
        let Some(raw) = git(&self.root, &["diff", "--name-only", "-z", from, to]) else {
            return Vec::new();
        };
        raw.split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// A hash standing for the current content of `path`, or `""` if it is not
    /// there. Oversized files are fingerprinted by their metadata instead.
    fn fingerprint(&self, path: &str) -> String {
        let full = self.root.join(path);
        let Ok(meta) = std::fs::metadata(&full) else {
            return String::new();
        };
        if meta.is_dir() {
            return String::new();
        }
        if meta.len() > MAX_HASH_BYTES {
            let stamp = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            return format!("size:{}:{stamp}", meta.len());
        }
        match std::fs::read(&full) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(_) => String::new(),
        }
    }
}

// ------------------------------------------------------------------- sweeping

/// What one look at the working tree turned up.
#[derive(Debug, Clone, Default)]
pub struct Sweep {
    /// The tree as it was found. Empty when there was nothing to look for.
    pub tree: Tree,
    /// Everything that has moved under each live task since it was claimed,
    /// in `seq` order.
    pub changes: Vec<(i64, Vec<Change>)>,
    /// Paths seen changing for the first time, per task. The interesting half
    /// of a sweep for anyone reporting on it.
    pub fresh: Vec<(i64, Vec<String>)>,
    /// Tasks that were measured.
    pub live: Vec<i64>,
}

impl Sweep {
    /// Paths this sweep saw change under task `seq` for the first time.
    pub fn fresh_for(&self, seq: i64) -> &[String] {
        self.fresh
            .iter()
            .find(|(s, _)| *s == seq)
            .map(|(_, paths)| paths.as_slice())
            .unwrap_or_default()
    }

    /// Everything this sweep found changed under task `seq`.
    pub fn changes_for(&self, seq: i64) -> &[Change] {
        self.changes
            .iter()
            .find(|(s, _)| *s == seq)
            .map(|(_, changes)| changes.as_slice())
            .unwrap_or_default()
    }
}

/// Look at the working tree once and bring every live task's footprint up to
/// date.
///
/// Looking is not the same as telling, and this only looks: no agent's recorded
/// version of a file moves here, whoever asked. A holder's own check-in
/// confirms afterwards, with [`crate::repo::Witnessed::confirm`] and the
/// changes this returns, once it has actually been handed the report — which is
/// what keeps "the file moved under you" from being swallowed by the same call
/// that would have said it.
///
/// Costs nothing at all when no task in the project holds a lease: there is
/// nobody to measure, so git is never invoked.
pub fn sweep(
    db: &crate::db::Db,
    witness: &Witness,
    project: &str,
    actor: &str,
) -> crate::error::Result<Sweep> {
    let baselines = db.witnessed().baselines(project)?;
    if baselines.is_empty() {
        return Ok(Sweep::default());
    }
    let tree = observe_against(witness, &baselines);
    let mut sweep = Sweep {
        live: baselines.iter().map(|b| b.seq).collect(),
        tree,
        ..Sweep::default()
    };
    for baseline in &baselines {
        let changes = witness.diff(&baseline.tree, &sweep.tree);
        let fresh = db.witnessed().record(baseline.seq, &changes, actor)?;
        if !fresh.is_empty() {
            sweep.fresh.push((baseline.seq, fresh));
        }
        sweep.changes.push((baseline.seq, changes));
    }
    Ok(sweep)
}

/// Sweep, then start measuring task `seq` against the tree as it stands.
///
/// One observation does both jobs: it is the last word on what happened under
/// the tasks already running, and the first word on the one just claimed.
pub fn begin(
    db: &crate::db::Db,
    witness: &Witness,
    project: &str,
    seq: i64,
    actor: &str,
) -> crate::error::Result<()> {
    let baselines: Vec<_> = db
        .witnessed()
        .baselines(project)?
        .into_iter()
        // A task being re-claimed after a lapse still has the previous
        // holder's baseline. Measuring against it now would credit this
        // sweep's changes to a claim that is over.
        .filter(|b| b.seq != seq)
        .collect();
    let tree = observe_against(witness, &baselines);
    for baseline in &baselines {
        let changes = witness.diff(&baseline.tree, &tree);
        db.witnessed().record(baseline.seq, &changes, actor)?;
    }
    db.witnessed().begin(seq, &tree)
}

/// Observe the tree with everything the given baselines will need to compare
/// against: their paths, and the commits they were taken at.
fn observe_against(witness: &Witness, baselines: &[crate::repo::Baseline]) -> Tree {
    let mut also: BTreeSet<String> = BTreeSet::new();
    let mut heads: BTreeSet<String> = BTreeSet::new();
    for baseline in baselines {
        also.extend(baseline.tree.paths().map(str::to_string));
        heads.insert(baseline.tree.head.clone());
    }
    witness.observe(
        &also.into_iter().collect::<Vec<_>>(),
        &heads.into_iter().collect::<Vec<_>>(),
    )
}

/// Run git in `root`, returning its stdout, or `None` if it could not be run or
/// said no.
///
/// Every git failure is the same failure here — the witness cannot see — so
/// there is nothing to distinguish and nothing to report.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        // A pager or a credential prompt inheriting this process's stdio would
        // deadlock an MCP server talking JSON-RPC down the same pipe.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Repo {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repo { dir };
            repo.git(&["init", "-q", "-b", "main"]);
            repo.git(&["config", "user.email", "t@example.com"]);
            repo.git(&["config", "user.name", "t"]);
            repo
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(self.path())
                .output()
                .expect("git must be runnable in tests");
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).to_string()
        }

        fn write(&self, rel: &str, body: &str) {
            let full = self.path().join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }

        fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
        }

        fn witness(&self) -> Witness {
            Witness::discover(self.path()).expect("a fresh repo must be witnessable")
        }
    }

    fn changed(w: &Witness, before: &Tree, after: &Tree) -> Vec<(String, ChangeKind)> {
        w.diff(before, after)
            .into_iter()
            .map(|c| (c.path, c.kind))
            .collect()
    }

    #[test]
    fn a_directory_without_git_is_simply_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Witness::discover(dir.path()).is_none());
        assert!(Witness::discover(Path::new("/nonexistent/elsewhere")).is_none());
    }

    #[test]
    fn an_untouched_tree_shows_no_changes() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("initial");
        let w = repo.witness();

        let before = w.observe(&[], &[]);
        let after = w.observe(&before.paths().map(str::to_string).collect::<Vec<_>>(), &[]);
        assert!(changed(&w, &before, &after).is_empty());
    }

    #[test]
    fn editing_a_tracked_file_is_seen_as_a_modification() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("initial");
        let w = repo.witness();
        let before = w.observe(&[], &[]);

        repo.write("src/lib.rs", "fn main() { work() }\n");
        let after = w.observe(
            &["src/lib.rs".to_string()],
            std::slice::from_ref(&before.head),
        );

        assert_eq!(
            changed(&w, &before, &after),
            vec![("src/lib.rs".to_string(), ChangeKind::Modified)]
        );
    }

    #[test]
    fn a_new_untracked_file_is_seen_as_an_addition() {
        let repo = Repo::new();
        repo.write("README.md", "hi\n");
        repo.commit("initial");
        let w = repo.witness();
        let before = w.observe(&[], &[]);

        repo.write("src/new.rs", "// new\n");
        let after = w.observe(&[], &[]);

        assert_eq!(
            changed(&w, &before, &after),
            vec![("src/new.rs".to_string(), ChangeKind::Added)]
        );
    }

    #[test]
    fn a_deleted_file_is_seen_as_a_deletion() {
        let repo = Repo::new();
        repo.write("src/gone.rs", "// here\n");
        repo.write("src/stays.rs", "// here\n");
        repo.commit("initial");
        let w = repo.witness();
        let before = w.observe(&["src/gone.rs".to_string()], &[]);

        std::fs::remove_file(repo.path().join("src/gone.rs")).unwrap();
        let after = w.observe(&["src/gone.rs".to_string()], &[]);

        assert_eq!(
            changed(&w, &before, &after),
            vec![("src/gone.rs".to_string(), ChangeKind::Deleted)]
        );
    }

    /// An agent that commits its own work has still done the work. The
    /// committed file goes clean, so `git status` stops mentioning it — the
    /// recorded HEAD is what keeps it visible.
    #[test]
    fn work_that_gets_committed_is_still_attributed() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("initial");
        let w = repo.witness();
        let before = w.observe(&[], &[]);
        assert!(before.entries.is_empty(), "a clean tree has nothing dirty");

        repo.write("src/lib.rs", "fn main() { work() }\n");
        repo.write("src/added.rs", "// added\n");
        repo.commit("the agent's own commit");

        let after = w.observe(&[], std::slice::from_ref(&before.head));
        assert_eq!(
            changed(&w, &before, &after),
            vec![
                ("src/added.rs".to_string(), ChangeKind::Added),
                ("src/lib.rs".to_string(), ChangeKind::Modified),
            ]
        );
    }

    /// A file that was already dirty when the task started, and that the task
    /// never touched, is not the task's doing.
    #[test]
    fn edits_that_predate_the_snapshot_are_not_attributed() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "fn main() {}\n");
        repo.commit("initial");
        repo.write("src/lib.rs", "// the human was here\n");

        let w = repo.witness();
        let before = w.observe(&[], &[]);
        assert_eq!(before.entries.len(), 1, "the dirty file is fingerprinted");

        let after = w.observe(&[], &[]);
        assert!(
            changed(&w, &before, &after).is_empty(),
            "an untouched dirty file is nobody's change"
        );
    }

    /// Reverting a file to its committed content is a change like any other:
    /// it is exactly the case where a second agent has undone the first.
    #[test]
    fn reverting_a_dirty_file_is_seen() {
        let repo = Repo::new();
        repo.write("src/lib.rs", "committed\n");
        repo.commit("initial");
        repo.write("src/lib.rs", "an edit in flight\n");
        let w = repo.witness();
        let before = w.observe(&[], &[]);

        repo.write("src/lib.rs", "committed\n");
        let after = w.observe(&["src/lib.rs".to_string()], &[]);
        assert_eq!(
            changed(&w, &before, &after),
            vec![("src/lib.rs".to_string(), ChangeKind::Modified)]
        );
    }

    #[test]
    fn ignored_files_are_never_candidates() {
        let repo = Repo::new();
        repo.write(".gitignore", "target/\n");
        repo.commit("initial");
        let w = repo.witness();
        let before = w.observe(&[], &[]);

        repo.write("target/debug/huge.bin", "artifacts\n");
        let after = w.observe(&[], &[]);
        assert!(changed(&w, &before, &after).is_empty(), "{after:?}");
    }

    #[test]
    fn paths_with_spaces_and_unicode_survive_the_porcelain() {
        let repo = Repo::new();
        repo.write("README.md", "hi\n");
        repo.commit("initial");
        let w = repo.witness();
        let before = w.observe(&[], &[]);

        repo.write("docs/a note — draft.md", "text\n");
        let after = w.observe(&[], &[]);
        assert_eq!(
            changed(&w, &before, &after),
            vec![("docs/a note — draft.md".to_string(), ChangeKind::Added)]
        );
    }

    /// A repository with no commits at all still works: HEAD is empty and every
    /// file is untracked, which is exactly what the diff needs.
    #[test]
    fn a_repository_with_no_commits_still_witnesses() {
        let repo = Repo::new();
        let w = repo.witness();
        let before = w.observe(&[], &[]);
        assert_eq!(before.head, "");

        repo.write("src/first.rs", "// first\n");
        let after = w.observe(&[], &[]);
        assert_eq!(
            changed(&w, &before, &after),
            vec![("src/first.rs".to_string(), ChangeKind::Added)]
        );
    }

    // ------------------------------------------------------- sweeping, in situ

    use crate::db::Db;
    use std::time::Duration;

    const TTL: Duration = Duration::from_secs(900);

    /// A project whose queue and working tree are the same place, which is the
    /// only configuration the witness is ever in.
    struct Project {
        repo: Repo,
        db: Db,
    }

    impl Project {
        fn new() -> Project {
            let repo = Repo::new();
            repo.write("README.md", "hi\n");
            repo.write("src/lib.rs", "fn main() {}\n");
            repo.write("src/config.rs", "// config\n");
            repo.commit("initial");
            Project {
                repo,
                db: Db::open_in_memory().unwrap(),
            }
        }

        fn root(&self) -> String {
            self.repo.path().to_string_lossy().to_string()
        }

        /// File a task, claim it for `holder`, declare `paths`, and start
        /// witnessing it — the whole opening move of working a task.
        fn claim(&self, title: &str, holder: &str, paths: &[&str]) -> i64 {
            let seq = self
                .db
                .tasks()
                .create(&self.root(), title, "", 0, "cli")
                .unwrap()
                .seq;
            self.db.tasks().claim(seq, holder, TTL).unwrap();
            if !paths.is_empty() {
                let owned: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
                self.db
                    .scopes()
                    .declare(seq, &owned, holder, crate::repo::OnConflict::Report)
                    .unwrap();
            }
            begin(&self.db, &self.repo.witness(), &self.root(), seq, holder).unwrap();
            seq
        }

        /// A check-in by `holder`: look, then record that this holder has been
        /// shown what was found — the order every MCP tool uses.
        fn check_in(&self, seq: i64, holder: &str) -> Sweep {
            let swept = sweep(&self.db, &self.repo.witness(), &self.root(), holder).unwrap();
            self.db
                .witnessed()
                .confirm(seq, swept.changes_for(seq))
                .unwrap();
            swept
        }

        /// The human's TUI looking, which confirms nothing for anybody.
        fn look(&self) -> Sweep {
            sweep(&self.db, &self.repo.witness(), &self.root(), "tui").unwrap()
        }

        fn touched(&self, seq: i64) -> Vec<String> {
            self.db
                .witnessed()
                .touched(seq)
                .unwrap()
                .into_iter()
                .map(|o| o.describe())
                .collect()
        }
    }

    #[test]
    fn a_sweep_with_nobody_working_never_runs_git() {
        let project = Project::new();
        let swept = project.look();
        assert!(swept.live.is_empty());
        assert_eq!(swept.tree, Tree::default());
    }

    #[test]
    fn a_task_is_credited_with_what_moved_while_it_was_held() {
        let project = Project::new();
        let seq = project.claim("port the loader", "codex:9f2c", &[]);
        assert!(project.touched(seq).is_empty(), "nothing has happened yet");

        project.repo.write("src/config.rs", "// ported\n");
        project.repo.write("src/new.rs", "// new\n");
        let swept = project.check_in(seq, "codex:9f2c");

        assert_eq!(
            project.touched(seq),
            vec!["src/config.rs (modified)", "src/new.rs (added)"]
        );
        assert_eq!(
            swept.fresh_for(seq),
            ["src/config.rs".to_string(), "src/new.rs".to_string()]
        );
        // Seen once, reported once: a second check-in with nothing new to say
        // must not write the same event again.
        assert!(project.check_in(seq, "codex:9f2c").fresh.is_empty());
    }

    /// The whole reason the witness exists. Two agents, one checkout, one file,
    /// and no commit between them — the case git itself cannot see.
    #[test]
    fn one_file_and_two_live_agents_is_a_contention_with_a_stale_side() {
        let project = Project::new();
        let first = project.claim("rewrite the loader", "codex:9f2c", &["src/config.rs"]);

        project.repo.write("src/config.rs", "// codex was here\n");
        project.check_in(first, "codex:9f2c");

        // A second agent starts after the first has already been in the file,
        // so the first's edit is part of its baseline and not its doing.
        let second = project.claim("audit the loader", "claude-code:af31", &["src/*.rs"]);
        assert!(project.touched(second).is_empty());

        project
            .repo
            .write("src/config.rs", "// claude wrote over it\n");
        project.check_in(second, "claude-code:af31");

        assert_eq!(project.touched(second), vec!["src/config.rs (modified)"]);

        let for_first = project.db.witnessed().contention(first).unwrap();
        assert_eq!(for_first.len(), 1, "{for_first:?}");
        assert_eq!(for_first[0].other_seq, second);
        assert!(
            for_first[0].is_stale(),
            "the file is not what codex last confirmed"
        );
        let sentence = for_first[0].describe();
        assert!(sentence.contains("src/config.rs"), "{sentence}");
        assert!(sentence.contains("claude-code:af31"), "{sentence}");
        assert!(sentence.contains("re-read"), "{sentence}");

        // And symmetrically, from the agent that is actually up to date.
        let for_second = project.db.witnessed().contention(second).unwrap();
        assert_eq!(for_second.len(), 1);
        assert!(for_second[0]
            .describe()
            .contains("check you did not write over their edit"));
    }

    /// Two agents working different files must never hear about each other,
    /// or the warning stops meaning anything.
    #[test]
    fn agents_in_different_files_do_not_contend() {
        let project = Project::new();
        let a = project.claim("the loader", "codex:9f2c", &["src/config.rs"]);
        let b = project.claim("the docs", "claude-code:af31", &["README.md"]);

        project.repo.write("src/config.rs", "// loader work\n");
        project.check_in(a, "codex:9f2c");
        project.repo.write("README.md", "# docs work\n");
        project.check_in(b, "claude-code:af31");

        // Both were live throughout, so both footprints hold both files: the
        // witness watched the tree, not the two keyboards. What keeps this
        // quiet is that neither agent declared the other's file.
        for seq in [a, b] {
            assert_eq!(
                project.touched(seq),
                vec!["README.md (modified)", "src/config.rs (modified)"]
            );
            assert!(
                project.db.witnessed().contention(seq).unwrap().is_empty(),
                "two agents in separate lanes must never hear about each other"
            );
        }
    }

    /// A file that changes while two agents are live lands in both footprints
    /// — hird watched the file, not the keyboard. The report says as much, and
    /// the human looking at the TUI must not be able to confirm it away.
    #[test]
    fn an_onlooker_sees_the_change_without_settling_anybodys_version() {
        let project = Project::new();
        let a = project.claim("the loader", "codex:9f2c", &["src/config.rs"]);
        let b = project.claim("the tests", "claude-code:af31", &["src/**"]);
        project.repo.write("src/config.rs", "// somebody's edit\n");

        let swept = project.look();
        assert_eq!(swept.live, vec![a, b]);
        assert_eq!(project.touched(a), vec!["src/config.rs (modified)"]);
        assert_eq!(project.touched(b), vec!["src/config.rs (modified)"]);

        // Neither agent has checked in, so neither is on record as having seen
        // this content, and both are told to re-read.
        for seq in [a, b] {
            let seen = project.db.witnessed().contention(seq).unwrap();
            assert_eq!(seen.len(), 1, "{seen:?}");
            assert!(!seen[0].is_stale(), "nobody has confirmed anything else");
        }
    }

    #[test]
    fn undoing_an_edit_takes_it_off_the_record() {
        let project = Project::new();
        let seq = project.claim("try something", "codex:9f2c", &[]);
        project.repo.write("src/lib.rs", "fn main() { oops() }\n");
        project.check_in(seq, "codex:9f2c");
        assert_eq!(project.touched(seq), vec!["src/lib.rs (modified)"]);

        project.repo.write("src/lib.rs", "fn main() {}\n");
        project.check_in(seq, "codex:9f2c");
        assert!(project.touched(seq).is_empty(), "reverted is not changed");
    }

    #[test]
    fn a_task_that_commits_its_work_is_still_credited_with_it() {
        let project = Project::new();
        let seq = project.claim("ship it", "codex:9f2c", &[]);
        project.repo.write("src/config.rs", "// done properly\n");
        project.repo.commit("port the loader");

        project.check_in(seq, "codex:9f2c");
        assert_eq!(project.touched(seq), vec!["src/config.rs (modified)"]);
    }

    /// A repository the witness cannot read is not an error anywhere: the
    /// queue behaves exactly as it did before the witness existed.
    #[test]
    fn a_project_without_git_simply_has_no_witness() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Witness::discover(dir.path()).is_none());
    }

    #[test]
    fn large_files_are_fingerprinted_without_being_read_twice() {
        let repo = Repo::new();
        repo.write("README.md", "hi\n");
        repo.commit("initial");
        let w = repo.witness();

        let big = vec![b'x'; (MAX_HASH_BYTES + 1) as usize];
        std::fs::write(repo.path().join("fixture.bin"), &big).unwrap();
        let tree = w.observe(&[], &[]);
        let hash = tree.get("fixture.bin").unwrap();
        assert!(hash.starts_with("size:"), "{hash}");
    }
}
