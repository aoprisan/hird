//! The herald: the queue announcing work that is waiting for hands.
//!
//! Everything else in hird is pull. An agent calls `task_next` when it wants
//! work, and a task that becomes claimable while nobody is asking sits there —
//! correct, visible on the board, and silent. That silence is a design
//! decision (§2: no daemon), but it leaves one seam the human still has to
//! work by hand: noticing that the queue has something ready and going to
//! find an agent to point at it.
//!
//! The herald closes that seam without opening the one the design forbids.
//! It is not a scheduler and not a daemon: it is one configured command
//! (`dispatch_hook` in `config.toml`), run at the moment a task becomes
//! claimable, told which task, why, and whom the queue would refuse it to
//! (`HIRD_RECUSED` — so a summons never wakes the one agent a filed review
//! is barred from), and then left entirely alone. What the
//! command does — prompt an idle agent through a terminal multiplexer such as
//! herdr, post a notification, append to a log, nothing — is the human's
//! business. hird neither waits for it nor reads its answer, so the queue's
//! own operations cannot be slowed down or failed by whatever the hook gets
//! up to.
//!
//! Because hird only runs when somebody calls it, announcements happen when
//! the becoming-claimable is *observed*, which for everything except lease
//! expiry is the same write that causes it: completing a task announces the
//! dependents it released, filing work announces it if nothing blocks it, a
//! `sent_back` verdict announces the reopened work. Lease expiry alone has no
//! write of its own — it is enforced lazily by sweeps — so it is announced by
//! whichever sweep first notices it, with the same latency the expiry itself
//! already has.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::identity;

/// Why a task is being announced. Every cause means the same thing at the
/// moment it fires — this task is claimable now — and names how it got there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Created with nothing blocking it.
    Filed,
    /// Its last unmet dependency finished.
    Unblocked,
    /// Finished work put itself in front of another harness.
    ReviewFiled,
    /// A review's verdict reopened it, findings appended to its brief.
    SentBack,
    /// A human put it back in the pool.
    Reopened,
    /// Its holder handed it back unfinished.
    Released,
    /// Its holder went quiet and the lease ran out.
    LeaseExpired,
}

impl Cause {
    /// The word the hook receives in `HIRD_EVENT`.
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Filed => "filed",
            Cause::Unblocked => "unblocked",
            Cause::ReviewFiled => "review_filed",
            Cause::SentBack => "sent_back",
            Cause::Reopened => "reopened",
            Cause::Released => "released",
            Cause::LeaseExpired => "lease_expired",
        }
    }
}

/// One announcement: task `seq` is claimable, why — and whom it is not for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub cause: Cause,
    pub seq: i64,
    pub title: String,
    pub project: String,
    /// Harnesses the queue will refuse this task to. The claim would tell
    /// them so anyway; telling the hook first means a summons that can pick
    /// between agents never wakes one the queue is about to turn away —
    /// which is every `review_filed` announcement under a hook that always
    /// prompts the same worker.
    pub recused: Vec<String>,
}

/// The configured messenger, or nothing.
///
/// Built once per process from the `dispatch_hook` key; an empty or unset key
/// builds `None` and costs every call site one `is_some` check.
#[derive(Debug, Clone)]
pub struct Herald {
    command: String,
    db: PathBuf,
}

impl Herald {
    /// A herald for `command`, announcing a queue that lives at `db`.
    ///
    /// `None` when the command is empty: no hook configured, nothing to run.
    pub fn new(command: &str, db: &Path) -> Option<Herald> {
        let command = command.trim();
        if command.is_empty() {
            return None;
        }
        Some(Herald {
            command: command.to_string(),
            db: db.to_path_buf(),
        })
    }

    /// Announce one claimable task: run the hook with the facts in its
    /// environment and walk away.
    ///
    /// The command runs through `sh -c` with `HIRD_EVENT`, `HIRD_TASK`,
    /// `HIRD_TITLE`, `HIRD_PROJECT`, `HIRD_RECUSED` and `HIRD_DB` set.
    /// `HIRD_RECUSED` is comma-separated harness names — commas cannot appear
    /// in a harness name, so `case ",$HIRD_RECUSED," in *,codex,*)` is a safe
    /// membership test — and empty for the tasks anybody may take. All three
    /// stdio streams are closed: an MCP server's stdout is a JSON-RPC wire,
    /// and a hook that inherited it could corrupt the session that fired it.
    /// Failure to spawn is swallowed — the queue's work is already committed,
    /// and a broken hook must not turn a finished task into an error.
    pub fn announce(&self, a: &Announcement) {
        let child = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .env("HIRD_EVENT", a.cause.as_str())
            .env("HIRD_TASK", a.seq.to_string())
            .env("HIRD_TITLE", &a.title)
            .env("HIRD_PROJECT", &a.project)
            .env("HIRD_RECUSED", a.recused.join(","))
            .env(identity::DB_ENV, &self.db)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            // Reap from a thread so the hook neither blocks this call nor
            // lingers as a zombie under a long-lived MCP server.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    /// Announce a batch, one hook run per task.
    pub fn announce_all(&self, list: &[Announcement]) {
        for a in list {
            self.announce(a);
        }
    }
}

/// Announce `list` through `herald`, if there is one.
///
/// The call sites all hold an `Option<Herald>`; this keeps each of them to a
/// single line that reads as what it does.
pub fn announce(herald: Option<&Herald>, list: &[Announcement]) {
    if let Some(herald) = herald {
        herald.announce_all(list);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if !contents.is_empty() {
                    return contents;
                }
            }
            assert!(
                Instant::now() < deadline,
                "hook never wrote {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn an_empty_command_builds_no_herald() {
        assert!(Herald::new("", Path::new("/tmp/x.db")).is_none());
        assert!(Herald::new("   ", Path::new("/tmp/x.db")).is_none());
        assert!(Herald::new("true", Path::new("/tmp/x.db")).is_some());
    }

    #[test]
    fn the_hook_receives_the_announcement_in_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let herald = Herald::new(
            &format!(
                "printf '%s %s %s %s %s' \"$HIRD_EVENT\" \"$HIRD_TASK\" \"$HIRD_TITLE\" \
                 \"$HIRD_PROJECT\" \"$HIRD_DB\" > {}",
                log.display()
            ),
            Path::new("/tmp/board.db"),
        )
        .unwrap();
        herald.announce(&Announcement {
            cause: Cause::Unblocked,
            seq: 7,
            title: "port the loader".to_string(),
            project: "/repo".to_string(),
            recused: Vec::new(),
        });
        assert_eq!(
            wait_for(&log),
            "unblocked 7 port the loader /repo /tmp/board.db"
        );
    }

    /// A hook that can address more than one agent routes on `HIRD_RECUSED`:
    /// the names arrive comma-joined, and empty when anybody may claim.
    #[test]
    fn the_hook_is_told_whom_the_task_is_not_for() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let herald = Herald::new(
            &format!("printf '[%s]' \"$HIRD_RECUSED\" > {}", log.display()),
            Path::new("/tmp/board.db"),
        )
        .unwrap();
        herald.announce(&Announcement {
            cause: Cause::ReviewFiled,
            seq: 8,
            title: "Review: port the loader".to_string(),
            project: "/repo".to_string(),
            recused: vec!["claude-code".to_string(), "codex".to_string()],
        });
        assert_eq!(wait_for(&log), "[claude-code,codex]");
    }

    /// The hook is fire-and-forget: a command that cannot run does not turn
    /// into an error at the call site.
    #[test]
    fn a_broken_hook_is_swallowed() {
        let herald = Herald::new("/nonexistent/hook", Path::new("/tmp/x.db")).unwrap();
        herald.announce(&Announcement {
            cause: Cause::Filed,
            seq: 1,
            title: "t".to_string(),
            project: "p".to_string(),
            recused: Vec::new(),
        });
    }
}
