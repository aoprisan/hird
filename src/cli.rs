//! The command line: `hird add`, `ls`, `show`, `cancel`, `reopen`, `mem …`.
//!
//! Everything a human does here is recorded with `actor = "cli"`, so the TUI's
//! history pane shows who did what.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;
use clap::{Args, Parser, Subcommand};

use crate::config::{self, Config};
use crate::db::Db;
use crate::exhibit;
use crate::fmt;
use crate::footing;
use crate::glob;
// The two announcement helpers live with the herald because the rule they
// encode — a sweep must announce what it collects — binds every reader that
// polls, not just this one.
use crate::herald::{announce_claimable, sweep_announcing, Cause, Herald};
use crate::identity::{self, ACTOR_CLI};
use crate::model::{Footprint, Standing, Status, TaskSummary};
use crate::plan;
use crate::register::{self, Registration};
use crate::repo::{dispatch_waves, Called, MemoryQuery, NewAssertion, OnConflict, ProjectScope};
use crate::witness;

/// Coordinate AI coding agents across harnesses through a shared work queue
/// and a shared assertion memory, all in one local SQLite database.
#[derive(Debug, Parser)]
#[command(
    name = "hird",
    version,
    about,
    long_about = None,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Database file to use. Overrides HIRD_DB and the default location.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Copy this hird binary into ~/.local/bin.
    #[arg(long)]
    pub install: bool,

    /// Install the hird skill for Codex, Claude Code, Copilot, and OpenCode.
    #[arg(long)]
    pub install_skill: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add a task to the queue and print its number.
    Add(AddArgs),
    /// List tasks.
    #[command(visible_alias = "list")]
    Ls(LsArgs),
    /// Show one task in full, with its history.
    Show { seq: i64 },
    /// Explain whether a task is claimable right now, and if not, everything
    /// standing in the way.
    ///
    /// The claim gates, asked out loud in the order dispatch applies them: a
    /// live lease, unfinished dependencies, an unanswered question, recusals,
    /// capability requirements, and file-scope overlap with live work. `hird
    /// show` says everything about a task; this answers the one question a
    /// silent queue raises — why is nobody getting this handed to them?
    Why { seq: i64 },
    /// The diff of what moved under a task, from the versions the witness kept.
    Diff {
        seq: i64,
        /// Only this file, project-relative.
        #[arg(long)]
        path: Option<String>,
        /// An archived holding instead of the current record: what round N
        /// changed, as `hird show` numbers the rounds.
        #[arg(long, value_name = "N")]
        tenure: Option<i64>,
    },
    /// Recover a version of a file as it stood under a task.
    Salvage {
        seq: i64,
        /// Project-relative path, as `hird show` lists it.
        path: String,
        /// The version the task started from, instead of the last one seen.
        #[arg(long)]
        baseline: bool,
        /// Reach an archived holding instead of the current record: the last
        /// version round N saw, as `hird show` numbers the rounds.
        #[arg(long, value_name = "N")]
        tenure: Option<i64>,
        /// Write to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Overwrite an existing --out file.
        #[arg(long)]
        force: bool,
    },
    /// Abandon a task.
    Cancel {
        seq: i64,
        /// Why, for the record.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Put a finished or cancelled task back in the pool.
    Reopen {
        seq: i64,
        /// Why, for the record.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Answer the question keeping an open task out of dispatch.
    Answer {
        seq: i64,
        /// The decision or information the next agent should proceed with.
        answer: String,
    },
    /// Stand this project's queue down: nothing new is handed out until
    /// `hird resume` lifts it.
    ///
    /// Named claims and `task_next` alike are refused, carrying the reason,
    /// and the dispatch hook stays quiet. Work already claimed is untouched —
    /// leases run, check-ins and completions land — because a recess stops
    /// the hand-out, not the work. Calling it again refreshes the reason.
    Recess {
        /// Why, in your words. Carried on every refused claim.
        #[arg(default_value = "")]
        reason: String,
        /// Another project's queue instead of the one this directory is in.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },
    /// Lift the recess: claims resume at once, and the dispatch hook is
    /// woken for everything that became claimable while the queue stood down.
    Resume {
        /// Another project's queue instead of the one this directory is in.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },
    /// Add or remove dependencies between tasks.
    #[command(subcommand)]
    Dep(DepCommand),
    /// File a whole dependency graph from a plan file, or read it first.
    #[command(subcommand)]
    Plan(PlanCommand),
    /// Print the queue as dispatch waves: what can be worked now, and what
    /// each later wave is waiting for.
    Graph(ScopeFilterArgs),
    /// Show or set the files a task is expected to touch.
    Scope(ScopeArgs),
    /// Show or set the capabilities a claimant must advertise.
    Require(RequireArgs),
    /// Show which agent is working what, and where they overlap.
    Agents(ScopeFilterArgs),
    /// Bar whoever worked one task from working another, or lift the bar.
    ///
    /// This is what makes a review a review: the queue refuses the claim from
    /// the harness that did the work, and dispatch routes around it.
    Recuse(RecuseArgs),
    /// Show each harness's track record under review: verdicts received on
    /// its work, its first-pass rate, and the verdicts it has handed out.
    ///
    /// Derived entirely from delivered verdicts, so it measures the one thing
    /// the queue can measure — whose work survives a reading by a different
    /// model. A report, not a scheduler: nothing routes work by it.
    Record(ScopeFilterArgs),
    /// Tail the append-only event trail across every task, oldest first:
    /// claims, check-ins, completions, verdicts, witnessed changes, expiries.
    ///
    /// This is the queue's own log — what the TUI shows, without the terminal
    /// UI, so a swarm can be watched from a second terminal, a tmux pane, or
    /// a script. `--follow` keeps reading as events land, and `--json` makes
    /// each line one machine-readable object for whatever wants to build on
    /// the feed.
    Events(EventsArgs),
    /// The board as it stood at a past moment, folded from the event trail.
    ///
    /// The trail is append-only so that no question about the past becomes
    /// unanswerable; this is the command that asks one. "Show me the queue at
    /// the moment task 23 was sent back" is a fold over events that already
    /// exist — who held what, which wave was live, what was parked on a
    /// question — read-only, from the same trail `hird events` prints.
    /// Statuses are yesterday's; titles are today's, since the trail does not
    /// version them.
    Replay {
        /// When: an RFC3339 UTC instant, prefixes welcome ("2026-08-14",
        /// "2026-08-14T10:00"), or how long ago ("90m", "2h", "3d").
        when: String,
        #[command(flatten)]
        scope: ScopeFilterArgs,
    },
    /// Show what earlier work already learned about a task, and why it is
    /// relevant. This is what an agent is handed when it claims the task.
    Recall {
        seq: i64,
        /// How many assertions at most. Defaults to the configured
        /// `recall_limit`.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Store and search assertions.
    #[command(subcommand)]
    Mem(MemCommand),
    /// Watch and drive the queue in a terminal UI.
    Tui,
    /// Serve the Model Context Protocol on stdio. Harnesses run this.
    Mcp,
    /// Write this binary's MCP registration into a harness's config file.
    Register(RegisterArgs),
    /// Print the database path this invocation would use.
    DbPath,
}

#[derive(Debug, Args)]
pub struct EventsArgs {
    /// Keep watching: print each new event as it lands. On the way it sweeps
    /// expired leases and the working tree, the same way the TUI does, so
    /// expiries and witnessed changes appear even while no agent is calling.
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// One JSON object per line instead of columns, for piping into tools.
    #[arg(long)]
    pub json: bool,
    /// Only these kinds, comma-separated — e.g. claimed,completed,witnessed.
    #[arg(long = "kind", value_name = "KIND", value_delimiter = ',')]
    pub kinds: Vec<crate::model::EventKind>,
    /// Only events on this task.
    #[arg(long, value_name = "SEQ")]
    pub task: Option<i64>,
    /// Only events recorded by this actor, exactly as the board names it —
    /// e.g. codex:9f2c, or cli.
    #[arg(long, value_name = "NAME")]
    pub actor: Option<String>,
    /// How many recent events to print — the backlog, before any following.
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub limit: usize,
    /// Every project in the database, not just this one.
    #[arg(long)]
    pub all_projects: bool,
}

#[derive(Debug, Args)]
pub struct RegisterArgs {
    /// Which harness to register with. Each has one config file, and this
    /// writes to that one.
    pub harness: register::Harness,
    /// Name the server something other than `hird` — for a second
    /// registration alongside the first, usually with its own `--db`.
    #[arg(long, default_value = "hird", value_name = "NAME")]
    pub name: String,
    /// Print what would be written and write nothing.
    #[arg(long)]
    pub print: bool,
    /// Replace an entry of the same name that says something else.
    #[arg(long, conflicts_with = "print")]
    pub force: bool,
    /// Capability this harness can satisfy, e.g. browser or network.
    /// Repeatable, or comma-separated.
    #[arg(long = "capability", value_name = "NAME", value_delimiter = ',')]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Short title, as the human will refer to it.
    pub title: String,
    /// Full instructions for the agent, as markdown.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Read the instructions from a file ("-" for stdin).
    #[arg(long, value_name = "PATH")]
    pub body_file: Option<PathBuf>,
    /// Higher sorts first. Informational only.
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    pub priority: i64,
    /// Project root to file this under. Defaults to the current project.
    #[arg(long, value_name = "PATH")]
    pub project: Option<PathBuf>,
    /// Task numbers this one must wait for. Repeatable, or comma-separated.
    #[arg(long = "needs", value_name = "SEQ", value_delimiter = ',')]
    pub needs: Vec<i64>,
    /// A file or glob this task is expected to touch, relative to the project
    /// root. Repeatable. Declaring these lets the queue keep two agents out of
    /// the same file.
    #[arg(long = "path", value_name = "GLOB")]
    pub paths: Vec<String>,
    /// Capability a claimant must advertise, e.g. browser or network.
    /// Repeatable, or comma-separated.
    #[arg(long = "requires", value_name = "CAPABILITY", value_delimiter = ',')]
    pub requirements: Vec<String>,
    /// When this task finishes, file a review of what it changed — scoped to
    /// the files that actually moved, and barred to the harness that moved
    /// them.
    #[arg(long)]
    pub review: bool,
}

#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// File every task in a plan file, with its dependencies and file scopes.
    ///
    /// The whole plan lands in one transaction, or none of it does. Applying
    /// the same plan again files only what the file has gained since, so a
    /// plan is something to edit and re-run rather than a one-shot script.
    Apply {
        /// The plan file, as TOML ("-" for stdin).
        file: PathBuf,
        /// Work out what would be filed — waves, overlaps and all — and write
        /// nothing.
        #[arg(long)]
        dry_run: bool,
        /// Project root to file under. Defaults to the current project.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },
    /// Read a plan for trouble before filing it, and write nothing.
    ///
    /// Parsing is already the hard gate — cycles, dangling needs, bad globs
    /// and bad capability labels all refuse to parse, here and at apply
    /// alike — so what this adds is the trouble a fileable plan can still
    /// carry: unordered tasks declaring the same files, tasks the conflict
    /// radar cannot see, reviews that will never be filed or that nobody on
    /// this board could claim. Advisories, not refusals: the plan files
    /// either way. The exit code is nonzero only when the plan itself is
    /// unfileable.
    Lint {
        /// The plan file, as TOML ("-" for stdin).
        file: PathBuf,
        /// Project root to lint against. Defaults to the current project.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DepCommand {
    /// Make a task wait for one or more others.
    Add {
        seq: i64,
        /// Task numbers it must wait for.
        #[arg(
            long = "needs",
            value_name = "SEQ",
            required = true,
            value_delimiter = ','
        )]
        needs: Vec<i64>,
    },
    /// Stop a task waiting for one or more others.
    #[command(visible_alias = "remove")]
    Rm {
        seq: i64,
        /// Task numbers it should no longer wait for.
        #[arg(
            long = "needs",
            value_name = "SEQ",
            required = true,
            value_delimiter = ','
        )]
        needs: Vec<i64>,
    },
}

#[derive(Debug, Args)]
pub struct ScopeArgs {
    pub seq: i64,
    /// A file or glob to add to the task's scope. Repeatable.
    #[arg(long = "path", value_name = "GLOB")]
    pub paths: Vec<String>,
    /// Forget everything the task had declared.
    #[arg(long, conflicts_with = "paths")]
    pub clear: bool,
}

#[derive(Debug, Args)]
pub struct RecuseArgs {
    /// The task to put under the bar.
    pub seq: i64,
    /// Task numbers whose worker must not take it. Repeatable, or
    /// comma-separated.
    #[arg(long = "from", value_name = "SEQ", value_delimiter = ',')]
    pub from: Vec<i64>,
    /// Why, for the record and for the refusal message.
    #[arg(long, default_value = "")]
    pub reason: String,
    /// Lift every bar on the task instead.
    #[arg(long, conflicts_with = "from")]
    pub clear: bool,
}

#[derive(Debug, Args)]
pub struct ScopeFilterArgs {
    /// Span every project rather than just the current one.
    #[arg(long)]
    pub all_projects: bool,
}

#[derive(Debug, Args)]
pub struct RequireArgs {
    /// The task whose requirements to show or replace.
    pub seq: i64,
    /// Required capabilities. Repeatable, or comma-separated. Supplying any
    /// replaces the existing set.
    #[arg(long = "capability", value_name = "NAME", value_delimiter = ',')]
    pub capabilities: Vec<String>,
    /// Remove every capability requirement.
    #[arg(long, conflicts_with = "capabilities")]
    pub clear: bool,
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Only tasks in this status.
    #[arg(long, value_name = "STATUS")]
    pub status: Option<String>,
    /// List tasks from every project.
    #[arg(long)]
    pub all_projects: bool,
}

#[derive(Debug, Subcommand)]
pub enum MemCommand {
    /// Record one factual assertion.
    ///
    /// Recording something already on file word for word does not duplicate it:
    /// it re-anchors that assertion to the code as it stands now and records
    /// another voice for it.
    Add {
        /// The assertion, in plain prose.
        content: String,
        /// Comma-separated tags.
        #[arg(long, default_value = "")]
        tags: String,
        /// Link it to the task it was learned on.
        #[arg(long, value_name = "SEQ")]
        task: Option<i64>,
        /// The file this fact is about. Repeatable. hird records what it says
        /// now, so a later reader can be told when it has moved.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Search assertions.
    Search {
        /// Full-text query. Plain words are fine.
        #[arg(default_value = "")]
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Search every project.
        #[arg(long)]
        all_projects: bool,
        /// Include assertions that have been replaced.
        #[arg(long)]
        include_superseded: bool,
    },
    /// Audit what the memory still stands on.
    ///
    /// Every anchored assertion against the files it was learned from, oldest
    /// first: which are still exactly what they were, which have moved, and
    /// which were about code that no longer exists.
    Standing {
        /// Only the assertions worth re-reading — shaky and orphaned.
        #[arg(long)]
        shaky: bool,
        /// Audit every project.
        #[arg(long)]
        all_projects: bool,
    },
    /// Write this project's recorded facts as Markdown, ready to paste into
    /// a CLAUDE.md or AGENTS.md.
    ///
    /// The memory reaches agents through claims and searches, which only
    /// helps a session that can reach the queue. Exporting gives the same
    /// facts a second life on paper: a cloud harness, a fresh clone, a
    /// session with no MCP at all still inherits what the swarm learned. The
    /// export stays footing-aware — a fact whose ground has been rewritten
    /// since it was recorded goes out marked for a re-read, not dressed as
    /// this morning's.
    Export {
        /// Only facts anchored to files this glob names.
        #[arg(long, value_name = "GLOB")]
        path: Option<String>,
        /// Only facts whose anchors still match the tree exactly; the shaky,
        /// the orphaned and the never-anchored stay home.
        #[arg(long)]
        firm: bool,
    },
}

/// Run a parsed command, writing human-readable output to `out`.
///
/// `Command::Tui` and `Command::Mcp` are handled by the binary, which owns the
/// terminal and the async runtime; they are rejected here.
pub fn run(cli: &Cli, out: &mut impl Write) -> anyhow::Result<()> {
    // Registering is the one thing a human does before there is anything to
    // open, so it happens without touching the database.
    if let Some(Command::Register(args)) = &cli.command {
        return register_cmd(args, cli.db.as_deref(), out);
    }

    let db_path = config::resolve_db_path(cli.db.as_deref());

    if let Some(Command::DbPath) = &cli.command {
        writeln!(out, "{}", db_path.display())?;
        return Ok(());
    }

    let config = Config::load_default()?;
    let db = Db::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = identity::resolve_project(&cwd);
    let herald = config.herald(&db_path);
    // Every queue verb below sweeps expired leases anyway; sweeping here
    // first keeps the outcome, so an expiry is announced by whichever
    // invocation notices it — `hird ls` glanced at by a human included.
    sweep_announcing(&db, &config, herald.as_ref());

    match cli
        .command
        .as_ref()
        .context("a command or installer option is required")?
    {
        Command::DbPath | Command::Register(_) => unreachable!("handled above"),
        Command::Tui | Command::Mcp => {
            anyhow::bail!(
                "`hird {}` is served by the binary, not the command dispatcher",
                match &cli.command {
                    Some(Command::Tui) => "tui",
                    _ => "mcp",
                }
            )
        }
        Command::Add(args) => {
            let seq = add(&db, &project, args, out)?;
            // Filed ready to work — unless `--needs` queued it behind
            // something, in which case the finish that clears it will speak.
            announce_claimable(herald.as_ref(), &db, &config, Cause::Filed, seq);
            Ok(())
        }
        Command::Ls(args) => {
            // The listing now says whether each task has changed anything, and
            // a stale answer to that is worse than none: a task that has been
            // writing since the last sweep would be listed as read-only.
            look(&db, &config, &project);
            ls(&db, &project, &config, args, out)
        }
        Command::Show { seq } => {
            look(&db, &config, &project);
            show(&db, *seq, &config, out)
        }
        Command::Why { seq } => why(&db, *seq, &config, out),
        Command::Diff { seq, path, tenure } => {
            diff(&db, *seq, path.as_deref(), *tenure, &config, out)
        }
        Command::Salvage {
            seq,
            path,
            baseline,
            tenure,
            out: to,
            force,
        } => salvage(
            &db,
            *seq,
            path,
            *baseline,
            *tenure,
            to.as_deref(),
            *force,
            &config,
            out,
        ),
        Command::Recall { seq, limit } => recall(
            &db,
            &config,
            &project,
            *seq,
            limit.map(|n| n.min(200)).unwrap_or(config.recall_limit()),
            out,
        ),
        Command::Cancel { seq, reason } => {
            let task = db.tasks().cancel(*seq, ACTOR_CLI, reason)?;
            writeln!(out, "task {} cancelled", task.seq)?;
            Ok(())
        }
        Command::Reopen { seq, reason } => {
            let task = db.tasks().reopen(*seq, ACTOR_CLI, reason)?;
            writeln!(out, "task {} reopened", task.seq)?;
            announce_claimable(herald.as_ref(), &db, &config, Cause::Reopened, task.seq);
            Ok(())
        }
        Command::Answer { seq, answer } => {
            let answered = db.questions().answer(*seq, ACTOR_CLI, answer)?;
            writeln!(out, "task {} answered", seq)?;
            writeln!(out, "  question  {}", answered.question)?;
            writeln!(
                out,
                "  answer    {}",
                answered.answer.as_deref().unwrap_or("")
            )?;
            announce_claimable(herald.as_ref(), &db, &config, Cause::Answered, *seq);
            Ok(())
        }
        Command::Recess {
            reason,
            project: explicit,
        } => recess_cmd(&db, &named_project(explicit, &project), reason, out),
        Command::Resume { project: explicit } => resume_cmd(
            &db,
            &config,
            herald.as_ref(),
            &named_project(explicit, &project),
            out,
        ),
        Command::Mem(cmd) => mem(&db, &project, &config, cmd, out),
        Command::Dep(cmd) => dep(&db, &config, herald.as_ref(), cmd, out),
        Command::Plan(cmd) => plan_cmd(&db, &project, &config, herald.as_ref(), cmd, out),
        Command::Graph(args) => graph(&db, &scope_of(&project, &config, args.all_projects), out),
        Command::Scope(args) => scope_cmd(&db, args, out),
        Command::Require(args) => require_cmd(&db, args, out),
        Command::Agents(args) => {
            look(&db, &config, &project);
            agents(&db, &scope_of(&project, &config, args.all_projects), out)
        }
        Command::Recuse(args) => recuse(&db, args, out),
        Command::Record(args) => record(&db, &scope_of(&project, &config, args.all_projects), out),
        Command::Replay { when, scope } => replay(
            &db,
            &scope_of(&project, &config, scope.all_projects),
            when,
            out,
        ),
        Command::Events(args) => events_cmd(&db, &project, &config, herald.as_ref(), args, out),
    }
}

/// `hird register`: put this binary in a harness's MCP config.
///
/// `--db` is honoured here as the database the *registered session* should use,
/// not the one this command reads — it reads none. Passing it is how a second
/// registration ends up pointed at a scratch board.
fn register_cmd(
    args: &RegisterArgs,
    db: Option<&Path>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let harness = args.harness;
    let registration =
        Registration::new(harness, &args.name, db).with_capabilities(&args.capabilities)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if args.print {
        writeln!(
            out,
            "# {} — {}",
            harness.label(),
            harness.config_path(&cwd).display()
        )?;
        write!(out, "{}", registration.render(harness))?;
        return Ok(());
    }

    let (path, outcome) = register::apply(harness, &registration, &cwd, args.force)?;

    writeln!(out, "{}", outcome.describe(&registration.name, &path))?;
    writeln!(
        out,
        "  command  {} {}",
        registration.command,
        registration.args.join(" ")
    )?;
    let env: Vec<String> = registration
        .env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    writeln!(out, "  env      {}", env.join("  "))?;
    // The registration is only half of it, and the other half is the half
    // people miss — so it is the last line either way.
    writeln!(out, "next: {}", harness.next_step())?;
    Ok(())
}

/// Bring the witness's record of the current project up to date.
///
/// The human is reading the board, not working a task, so this confirms nothing
/// on any agent's behalf: it can only add to what is known, never make an
/// agent's copy of a file look fresher than it is. Failure is silence — an
/// unreadable working tree is not a reason to refuse to print the queue.
fn look(db: &Db, config: &Config, project: &str) {
    let Some(witness) = config.witness(Path::new(project)) else {
        return;
    };
    let _ = witness::sweep(db, &witness, project, ACTOR_CLI);
}

fn scope_of(project: &str, config: &Config, all_projects: bool) -> ProjectScope {
    ProjectScope::resolve(project, config.all_projects(Some(all_projects)))
}

/// The project a recess verb aims at: `--project` when given, else the one
/// this directory resolves to.
fn named_project(explicit: &Option<PathBuf>, resolved: &str) -> String {
    match explicit {
        Some(root) => identity::canonical_project(root),
        None => resolved.to_string(),
    }
}

/// `hird recess`: stand this project's queue down.
fn recess_cmd(db: &Db, project: &str, reason: &str, out: &mut impl Write) -> anyhow::Result<()> {
    match db.recesses().call(project, reason, ACTOR_CLI)? {
        Called::Began(recess) => {
            writeln!(
                out,
                "queue for {project} in recess{}",
                recess.quoted_reason()
            )?;
            writeln!(
                out,
                "  nothing new is handed out; work already claimed continues"
            )?;
            writeln!(out, "  hird resume lifts it")?;
        }
        Called::AlreadyStanding {
            recess,
            reason_changed,
        } => {
            let note = if reason_changed {
                "; reason updated"
            } else {
                ""
            };
            writeln!(
                out,
                "already in recess since {}{note}",
                fmt::age_phrase(&recess.at, Utc::now())
            )?;
        }
    }
    Ok(())
}

/// `hird resume`: lift the recess and summon hands to what waited behind it.
fn resume_cmd(
    db: &Db,
    config: &Config,
    herald: Option<&Herald>,
    project: &str,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    match db.recesses().lift(project)? {
        None => writeln!(
            out,
            "the queue for {project} was not in recess; nothing to lift"
        )?,
        Some(_) => {
            writeln!(out, "recess lifted for {project} — claims resume")?;
            // Everything that became claimable while the queue stood down was
            // deliberately never announced; announce it now, task by task, so
            // the dispatch hook summons hands to the backlog. The claimable
            // check inside `announce_claimable` keeps blocked and parked work
            // quiet, exactly as it does everywhere else.
            let scope = ProjectScope::Only(project.to_string());
            for task in db.tasks().list(&scope, Some(Status::Open))? {
                announce_claimable(herald, db, config, Cause::Resumed, task.seq);
            }
        }
    }
    Ok(())
}

/// The banner `hird ls` and `hird graph` open with while a recess stands.
fn recess_banner(recess: &crate::model::Recess, now: chrono::DateTime<Utc>) -> String {
    format!(
        "in recess{} — called {}; nothing new is handed out, hird resume lifts it",
        recess.quoted_reason(),
        fmt::age_phrase(&recess.at, now)
    )
}

/// `hird events`: the trail across tasks, as a log — and, with `--follow`,
/// as a live one.
///
/// Follow mode makes this command a monitor in the same sense the TUI is one:
/// it polls the same SQLite file at the same cadence, sweeps expired leases so
/// their announcements are not waiting on somebody else to glance at the
/// board, and looks at the working tree so witnessed changes land in the feed
/// while they are still news. There is still no daemon — stopping the tail
/// costs nothing and forgets nothing, because the cursor is the trail itself.
fn events_cmd(
    db: &Db,
    project: &str,
    config: &Config,
    herald: Option<&Herald>,
    args: &EventsArgs,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let scope = scope_of(project, config, args.all_projects);
    let filter = crate::repo::FeedFilter {
        kinds: args.kinds.clone(),
        task_seq: args.task,
        actor: args.actor.clone(),
    };
    // The trail only carries what somebody enforced; the reader is now that
    // somebody, so the backlog it prints is current rather than as-of the
    // last time an agent called.
    look(db, config, project);
    let backlog = db.events().tail(&scope, &filter, args.limit)?;
    let mut cursor = backlog.last().map(|e| e.cursor).unwrap_or(0);
    if backlog.is_empty() && !args.follow && !args.json {
        writeln!(out, "nothing on the record yet")?;
    }
    for event in &backlog {
        write_event(out, event, args.json, scope.is_all())?;
    }
    if !args.follow {
        return Ok(());
    }
    out.flush()?;
    let mut last_look = Utc::now();
    loop {
        std::thread::sleep(crate::tui::app::POLL_INTERVAL);
        // Expiry is enforced lazily by whoever reads the table next, and
        // while a tail is running that is this loop — with the herald told,
        // exactly as if a human had glanced at the board.
        if let Ok(outcome) = db.tasks().sweep_leases() {
            for seq in outcome.expired {
                announce_claimable(herald, db, config, Cause::LeaseExpired, seq);
            }
        }
        let now = Utc::now();
        if now - last_look >= crate::tui::app::WITNESS_INTERVAL {
            last_look = now;
            look(db, config, project);
        }
        for event in db.events().since(&scope, &filter, cursor)? {
            cursor = event.cursor;
            write_event(out, &event, args.json, scope.is_all())?;
        }
        out.flush()?;
    }
}

/// One event, one line: columns for a human, an object for a pipe.
fn write_event(
    out: &mut impl Write,
    event: &crate::repo::FeedEvent,
    json: bool,
    all_projects: bool,
) -> anyhow::Result<()> {
    if json {
        writeln!(
            out,
            "{}",
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
        )?;
        return Ok(());
    }
    let clock = crate::model::parse_ts(&event.at)
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".into());
    // The detail is the news; where a kind carries none, the title says what
    // the event happened to.
    let what = if event.detail.is_empty() {
        fmt::truncate(&event.task_title, 76)
    } else {
        fmt::truncate(&event.detail, 76)
    };
    let task = format!("#{}", event.task_seq);
    if all_projects {
        writeln!(
            out,
            "{clock}  {task:>4}  {:<14} {:<18} {what}  [{}]",
            event.kind, event.actor, event.project
        )?;
    } else {
        writeln!(
            out,
            "{clock}  {task:>4}  {:<14} {:<18} {what}",
            event.kind, event.actor
        )?;
    }
    Ok(())
}

fn add(db: &Db, project: &str, args: &AddArgs, out: &mut impl Write) -> anyhow::Result<i64> {
    let body = match (&args.body, &args.body_file) {
        (Some(body), _) => body.clone(),
        (None, Some(path)) => read_text(path)?,
        (None, None) => String::new(),
    };
    let project = match &args.project {
        Some(explicit) => identity::canonical_project(explicit),
        None => project.to_string(),
    };
    let task = db
        .tasks()
        .create(&project, &args.title, &body, args.priority, ACTOR_CLI)?;
    // Dependencies and scope go on after creation, so a bad `--needs` leaves a
    // real task behind rather than losing the title the human just typed.
    for needed in &args.needs {
        db.deps().add(task.seq, *needed, ACTOR_CLI)?;
    }
    if !args.paths.is_empty() {
        db.scopes()
            .declare(task.seq, &args.paths, ACTOR_CLI, OnConflict::Report)?;
    }
    if !args.requirements.is_empty() {
        db.requirements()
            .set(task.seq, &args.requirements, ACTOR_CLI)?;
    }
    if args.review {
        db.tasks().set_review(task.seq, true, ACTOR_CLI)?;
    }
    writeln!(out, "{}", task.seq)?;
    Ok(task.seq)
}

fn dep(
    db: &Db,
    config: &Config,
    herald: Option<&Herald>,
    cmd: &DepCommand,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    match cmd {
        DepCommand::Add { seq, needs } => {
            for needed in needs {
                if db.deps().add(*seq, *needed, ACTOR_CLI)? {
                    writeln!(out, "task {seq} now waits for task {needed}")?;
                } else {
                    writeln!(out, "task {seq} already waited for task {needed}")?;
                }
            }
        }
        DepCommand::Rm { seq, needs } => {
            for needed in needs {
                if db.deps().remove(*seq, *needed, ACTOR_CLI)? {
                    writeln!(out, "task {seq} no longer waits for task {needed}")?;
                } else {
                    writeln!(out, "task {seq} was not waiting for task {needed}")?;
                }
            }
            // Removing an edge is a human re-plan, and it can be the thing
            // that leaves the task waiting for nothing.
            announce_claimable(herald, db, config, Cause::Unblocked, *seq);
        }
    }
    Ok(())
}

/// File a plan, or work out what filing it would do.
fn plan_cmd(
    db: &Db,
    project: &str,
    config: &Config,
    herald: Option<&Herald>,
    cmd: &PlanCommand,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let (file, explicit) = match cmd {
        PlanCommand::Apply { file, project, .. } | PlanCommand::Lint { file, project } => {
            (file, project)
        }
    };
    let source = read_text(file)?;
    let parsed =
        plan::parse(&source).with_context(|| format!("reading the plan in {}", file.display()))?;
    let project = match explicit {
        Some(root) => identity::canonical_project(root),
        None => project.to_string(),
    };

    let dry_run = match cmd {
        PlanCommand::Lint { .. } => return lint(db, &project, &parsed, out),
        PlanCommand::Apply { dry_run, .. } => *dry_run,
    };
    if dry_run {
        return preview(db, &project, &parsed, out);
    }

    let applied = db.plans().apply(&project, &parsed, ACTOR_CLI)?;
    // The first wave is workable the moment the plan commits; the claimable
    // check inside `announce` is what keeps the later waves quiet.
    for placed in &applied.created {
        announce_claimable(herald, db, config, Cause::Filed, placed.seq);
    }
    if applied.created.is_empty() {
        writeln!(
            out,
            "plan {:?} is already filed in full; nothing to do",
            parsed.plan
        )?;
    } else {
        writeln!(
            out,
            "filed {} from plan {:?}",
            count(applied.created.len(), "task"),
            parsed.plan
        )?;
        for placed in &applied.created {
            writeln!(
                out,
                "  #{:<4} {:<14} {}",
                placed.seq,
                fmt::truncate(&placed.name, 14),
                fmt::truncate(&placed.title, 44)
            )?;
        }
    }
    if !applied.reused.is_empty() && !applied.created.is_empty() {
        // Only worth saying next to what was filed: on a re-apply that files
        // nothing, the line above has already said the whole plan was there.
        let numbers: Vec<String> = applied
            .reused
            .iter()
            .map(|p| format!("#{}", p.seq))
            .collect();
        writeln!(
            out,
            "{} already filed: {}",
            count(applied.reused.len(), "task"),
            numbers.join(", ")
        )?;
    }
    if applied.edges_added > 0 {
        writeln!(out, "{} recorded", count(applied.edges_added, "dependency"))?;
    }
    for drift in &applied.drifted {
        writeln!(out, "note: {}", drift.describe())?;
    }
    for conflict in &applied.conflicts {
        writeln!(out, "warning: {}", conflict.describe())?;
    }
    Ok(())
}

/// What this plan would become, printed and not written.
///
/// The waves come from the same function `hird graph` prints, so what a human
/// reads here is what the board will say once the plan is filed.
fn preview(
    db: &Db,
    project: &str,
    parsed: &plan::Plan,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let preview = parsed.preview();
    let filed: BTreeMap<String, i64> = db
        .plans()
        .nodes(project, &parsed.plan)?
        .into_iter()
        .collect();

    writeln!(
        out,
        "plan {:?} — {}, {}, at most {} at once",
        parsed.plan,
        count(parsed.tasks.len(), "task"),
        count(preview.waves.len(), "wave"),
        preview.widest()
    )?;

    for (index, wave) in preview.waves.iter().enumerate() {
        let label = match index {
            0 => "wave 1  (workable now)".to_string(),
            n => format!("wave {}  (after wave {n})", n + 1),
        };
        writeln!(out, "\n{label}")?;
        for name in wave {
            let Some(task) = parsed.task(name) else {
                continue;
            };
            let number = match filed.get(name) {
                Some(seq) => format!("#{seq}"),
                None => "new".to_string(),
            };
            let mut line = format!(
                "  {:<5} {:<14} {}",
                number,
                fmt::truncate(name, 14),
                fmt::truncate(&task.title, 40)
            );
            if !task.needs.is_empty() {
                line.push_str(&format!("   waits for {}", task.needs.join(", ")));
            }
            writeln!(out, "{line}")?;
            if !task.paths.is_empty() {
                writeln!(out, "      files  {}", task.paths.join(", "))?;
            }
            if !task.requires.is_empty() {
                writeln!(out, "      requires {}", task.requires.join(", "))?;
            }
        }
    }

    if !preview.collisions.is_empty() {
        writeln!(
            out,
            "\nsame files, nothing ordering them — the queue hands these out one \
             at a time, so the waves above are wider than the work really is"
        )?;
        for collision in &preview.collisions {
            writeln!(out, "  {}", collision.describe())?;
        }
    }
    if !preview.unscoped.is_empty() {
        writeln!(
            out,
            "\ndeclaring no files: {}\n  the queue cannot keep another agent out \
             of what these touch, and what earlier work learned reaches them by \
             title alone",
            preview.unscoped.join(", ")
        )?;
    }

    let already = filed.len();
    if already == 0 {
        writeln!(out, "\nnothing was written; drop --dry-run to file it")?;
    } else {
        writeln!(
            out,
            "\nnothing was written; {} of these already filed, so applying would \
             file the other {}",
            already,
            parsed.tasks.len() - already
        )?;
    }
    Ok(())
}

/// `hird mem export`: the project's current facts, as Markdown on stdout.
///
/// One fact per bullet, its files and footing on the line below — enough for
/// a reader with no queue to know what was learned and whether the code it
/// was learned from still stands. Oldest first, so re-exporting after new
/// work appends rather than reshuffles.
fn mem_export(
    db: &Db,
    project: &str,
    witness: Option<&witness::Witness>,
    path: Option<&str>,
    firm_only: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let wanted = match path {
        Some(raw) => Some(glob::normalize(raw).ok_or_else(|| {
            anyhow::anyhow!("--path {raw:?} is not a usable glob; try src/**, or a literal path")
        })?),
        None => None,
    };
    if firm_only && witness.is_none() {
        anyhow::bail!(
            "--firm needs a watched tree to check anchors against; run inside the \
             project's git checkout, or export without it"
        );
    }

    let scope = ProjectScope::Only(project.to_string());
    let mut facts = db
        .memory()
        .search(&MemoryQuery::new("", scope).limit(10_000))?;
    // Recency is search's order; an export wants the stable one.
    facts.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    let ids: Vec<String> = facts.iter().map(|a| a.id.clone()).collect();
    let anchors = db.footings().anchors_for(Some(project), &ids)?;
    let standings = footing::standings(db, witness, project, &ids);

    let kept: Vec<&crate::model::Assertion> = facts
        .iter()
        .filter(|fact| match &wanted {
            Some(glob) => anchors
                .get(&fact.id)
                .is_some_and(|list| list.iter().any(|a| crate::glob::intersects(glob, &a.path))),
            None => true,
        })
        .filter(|fact| !firm_only || matches!(standings.get(&fact.id), Some(Standing::Firm { .. })))
        .collect();

    if kept.is_empty() {
        writeln!(out, "nothing to export; hird mem add is how facts get here")?;
        return Ok(());
    }

    writeln!(out, "## What earlier work here learned")?;
    writeln!(out)?;
    writeln!(
        out,
        "<!-- hird mem export, {}. Facts recorded by agents working this\n\
         project, each anchored to the files it was read off. One marked\n\
         \"re-check\" has had that ground rewritten since it was learned. -->",
        Utc::now().format("%Y-%m-%d")
    )?;
    writeln!(out)?;
    for fact in kept {
        writeln!(out, "- {}", fact.content.trim())?;
        let mut notes: Vec<String> = Vec::new();
        if let Some(list) = anchors.get(&fact.id).filter(|l| !l.is_empty()) {
            let paths: Vec<String> = list.iter().map(|a| format!("`{}`", a.path)).collect();
            notes.push(paths.join(", "));
        }
        match standings.get(&fact.id) {
            Some(Standing::Shaky { .. }) => {
                notes.push("re-check: the ground under this has moved since".to_string());
            }
            Some(Standing::Orphaned { .. }) => {
                notes.push("re-check: the files this was read off are gone".to_string());
            }
            _ => {}
        }
        if let Some(spoken) = footing::corroboration(db, fact) {
            notes.push(spoken);
        }
        if !notes.is_empty() {
            writeln!(out, "  — {}", notes.join("; "))?;
        }
    }
    Ok(())
}

/// `hird replay`: the board as it stood at a past moment.
fn replay(db: &Db, scope: &ProjectScope, when: &str, out: &mut impl Write) -> anyhow::Result<()> {
    let now = Utc::now();
    let cutoff = parse_when(when, now)?;
    let board = db.events().board_at(scope, &cutoff)?;

    // The age is only worth saying when the cutoff is a full instant; a
    // prefix like "2026-08-14" already reads as itself.
    let age = crate::model::parse_ts(&cutoff)
        .map(|_| format!(" — {}", fmt::age_phrase(&cutoff, now)))
        .unwrap_or_default();
    writeln!(out, "the board as of {cutoff}{age}")?;
    if board.is_empty() {
        writeln!(out, "nothing had been filed yet")?;
        return Ok(());
    }
    for task in &board {
        let mut line = format!(
            "#{:<4} {:<11}  {}",
            task.seq,
            task.status,
            fmt::truncate(&task.title, 48)
        );
        if let Some(holder) = &task.holder {
            line.push_str(&format!("  [{holder}]"));
        }
        if task.awaiting_answer {
            line.push_str("  awaits answer");
        }
        if scope.is_all() {
            line.push_str(&format!("  ({})", task.project));
        }
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

/// A moment from the command line: an age like `2h`, or an RFC3339 instant —
/// a prefix of one included, since stored timestamps are fixed-width UTC and
/// compare correctly as strings.
fn parse_when(raw: &str, now: chrono::DateTime<Utc>) -> anyhow::Result<String> {
    let raw = raw.trim();
    if let Some(digits) = raw.strip_suffix(['s', 'm', 'h', 'd']) {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            let n: i64 = digits.parse()?;
            let delta = match raw.chars().last() {
                Some('s') => chrono::Duration::seconds(n),
                Some('m') => chrono::Duration::minutes(n),
                Some('h') => chrono::Duration::hours(n),
                _ => chrono::Duration::days(n),
            };
            return Ok(crate::model::fmt_ts(now - delta));
        }
    }
    let looks_absolute = raw.len() >= 4
        && raw.starts_with(|c: char| c.is_ascii_digit())
        && raw
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | '.' | 'Z' | '+'));
    if looks_absolute {
        return Ok(raw.to_string());
    }
    anyhow::bail!(
        "replay wants a moment: an RFC3339 UTC instant (\"2026-08-14T10:00\", prefixes \
         welcome) or how long ago (\"90m\", \"2h\", \"3d\"), not {raw:?}"
    )
}

/// `hird plan lint`: the trouble a fileable plan can still carry.
///
/// Everything fatal is already `parse`'s job, and re-litigating it here would
/// let the two drift. What lint reads for is the plan that files cleanly and
/// then disappoints: waves wider on paper than the queue will run them,
/// tasks the collision radar cannot see, reviews that will never exist or
/// that nobody present may claim.
fn lint(db: &Db, project: &str, parsed: &plan::Plan, out: &mut impl Write) -> anyhow::Result<()> {
    let preview = parsed.preview();
    let mut advisories = 0usize;

    writeln!(
        out,
        "plan {:?} parses — {}, {}, at most {} at once",
        parsed.plan,
        count(parsed.tasks.len(), "task"),
        count(preview.waves.len(), "wave"),
        preview.widest()
    )?;

    for collision in &preview.collisions {
        advisories += 1;
        writeln!(out, "collision {}", collision.describe())?;
        writeln!(
            out,
            "          nothing orders them, so the queue hands them out one at a time"
        )?;
    }
    if !preview.unscoped.is_empty() {
        advisories += 1;
        writeln!(out, "unscoped  {}", preview.unscoped.join(", "))?;
        writeln!(
            out,
            "          declaring no files, the queue cannot keep another agent out \
             of what these touch"
        )?;
    }

    // The reviews: work marked `review = true` files one on finishing — but
    // only when there is something to read, and only usefully when a second
    // harness exists to read it.
    let reviewed: Vec<&plan::PlanTask> = parsed.tasks.iter().filter(|t| t.review).collect();
    for task in reviewed.iter().filter(|t| t.paths.is_empty()) {
        advisories += 1;
        writeln!(
            out,
            "review    {} declares no files — if the witness sees nothing move, \
             finishing it files no review",
            task.name
        )?;
    }
    if !reviewed.is_empty() {
        let seen = db
            .events()
            .harnesses_seen(&ProjectScope::Only(project.to_string()))?;
        if seen.len() < 2 {
            advisories += 1;
            let present = match seen.as_slice() {
                [] => "none has acted here yet".to_string(),
                only => format!("this board has only seen {}", only.join(", ")),
            };
            writeln!(
                out,
                "review    a review is refused to the harness that did the work, and \
                 {present} — a second harness is what makes these claimable",
            )?;
        }
    }

    match advisories {
        0 => writeln!(out, "\nnothing to flag; hird plan apply files it")?,
        n => writeln!(out, "\n{}; the plan files either way", count(n, "advisory"))?,
    }
    Ok(())
}

/// `1 task` / `3 tasks`, so the sentence around it does not have to agree.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        return format!("{n} {noun}");
    }
    match noun.strip_suffix('y') {
        Some(stem) => format!("{n} {stem}ies"),
        None => format!("{n} {noun}s"),
    }
}

/// `hird recuse`: who must not work a task, and why.
fn recuse(db: &Db, args: &RecuseArgs, out: &mut impl Write) -> anyhow::Result<()> {
    if args.clear {
        let removed = db.recusals().clear(args.seq, ACTOR_CLI)?;
        writeln!(out, "lifted {removed} recusal(s) from task {}", args.seq)?;
        return Ok(());
    }
    for from in &args.from {
        db.recusals()
            .add(args.seq, *from, &args.reason, ACTOR_CLI)?;
    }
    let recusals = db.recusals().for_task(args.seq)?;
    if recusals.is_empty() {
        writeln!(out, "task {} is recused from nothing", args.seq)?;
        return Ok(());
    }
    for recusal in &recusals {
        writeln!(out, "{}", recusal.describe())?;
    }
    Ok(())
}

/// `hird record`: each harness's standing in the verdict record.
fn record(db: &Db, scope: &ProjectScope, out: &mut impl Write) -> anyhow::Result<()> {
    let records = db.verdicts().record(scope)?;
    let workers: Vec<_> = records.iter().filter(|r| r.judged > 0).collect();
    let reviewers: Vec<_> = records
        .iter()
        .filter(|r| r.upheld_given + r.sent_back_given > 0)
        .collect();
    if workers.is_empty() && reviewers.is_empty() {
        writeln!(
            out,
            "no verdicts on record — file work with `hird add --review` and the reviews \
             that finishing it files will deliver them"
        )?;
        return Ok(());
    }
    if !workers.is_empty() {
        writeln!(
            out,
            "{:<14} {:>7} {:>7} {:>10} {:>11}",
            "as worker", "judged", "upheld", "sent back", "first pass"
        )?;
        for r in workers {
            writeln!(
                out,
                "{:<14} {:>7} {:>7} {:>10} {:>11}",
                r.harness,
                r.judged,
                r.upheld,
                r.sent_back,
                format!("{}/{}", r.first_pass, r.tasks_judged)
            )?;
        }
    }
    if !reviewers.is_empty() {
        writeln!(
            out,
            "\n{:<14} {:>7} {:>10}",
            "as reviewer", "upheld", "sent back"
        )?;
        for r in reviewers {
            writeln!(
                out,
                "{:<14} {:>7} {:>10}",
                r.harness, r.upheld_given, r.sent_back_given
            )?;
        }
    }
    Ok(())
}

fn scope_cmd(db: &Db, args: &ScopeArgs, out: &mut impl Write) -> anyhow::Result<()> {
    if args.clear {
        let removed = db.scopes().clear(args.seq, ACTOR_CLI)?;
        writeln!(out, "cleared {removed} pattern(s) from task {}", args.seq)?;
        return Ok(());
    }
    if !args.paths.is_empty() {
        let conflicts =
            db.scopes()
                .declare(args.seq, &args.paths, ACTOR_CLI, OnConflict::Report)?;
        for conflict in &conflicts {
            writeln!(out, "warning: {}", conflict.describe())?;
        }
    }
    let patterns = db.scopes().for_task(args.seq)?;
    if patterns.is_empty() {
        writeln!(out, "task {} declares no files", args.seq)?;
    } else {
        for pattern in patterns {
            writeln!(out, "{pattern}")?;
        }
    }
    Ok(())
}

fn require_cmd(db: &Db, args: &RequireArgs, out: &mut impl Write) -> anyhow::Result<()> {
    if args.clear {
        db.requirements().set(args.seq, &[], ACTOR_CLI)?;
    } else if !args.capabilities.is_empty() {
        db.requirements()
            .set(args.seq, &args.capabilities, ACTOR_CLI)?;
    }
    let required = db.requirements().for_task(args.seq)?;
    if required.is_empty() {
        writeln!(out, "task {} requires no capabilities", args.seq)?;
    } else {
        for capability in required {
            writeln!(out, "{capability}")?;
        }
    }
    Ok(())
}

/// Print the queue as dispatch waves.
///
/// A wave is everything that becomes workable once the previous wave is done,
/// which is the shape a human actually wants to see: not "who points at whom"
/// but "how much of this can run at once, and what is the critical path".
fn graph(db: &Db, scope: &ProjectScope, out: &mut impl Write) -> anyhow::Result<()> {
    let tasks = db.tasks().list(scope, None)?;
    let edges = db.deps().edges(scope)?;
    let waves = dispatch_waves(&tasks, &edges);
    let questions = db.questions().unanswered_map(scope)?;
    // "Workable now" is not true while a recess stands, and this is the view
    // that says it — so the recess speaks first.
    if let ProjectScope::Only(project) = scope {
        if let Some(recess) = db.recesses().current(project)? {
            writeln!(out, "{}", recess_banner(&recess, Utc::now()))?;
        }
    }
    if waves.is_empty() {
        writeln!(out, "no unfinished tasks")?;
        return Ok(());
    }

    let waiting: BTreeMap<i64, Vec<i64>> = edges.iter().fold(BTreeMap::new(), |mut acc, (t, d)| {
        acc.entry(*t).or_default().push(*d);
        acc
    });
    let by_seq: BTreeMap<i64, &TaskSummary> = tasks.iter().map(|t| (t.seq, t)).collect();

    for (index, wave) in waves.iter().enumerate() {
        let label = match index {
            0 => "wave 1  (workable now)".to_string(),
            n => format!("wave {}  (after wave {n})", n + 1),
        };
        writeln!(out, "{label}")?;
        for seq in wave {
            let Some(task) = by_seq.get(seq) else {
                continue;
            };
            let mut line = format!(
                "  #{:<4} {:<12} {}",
                task.seq,
                task.status,
                fmt::truncate(&task.title, 44)
            );
            if let Some(needs) = waiting.get(seq) {
                let listed = needs
                    .iter()
                    .map(|s| format!("#{s}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                line.push_str(&format!("   waits for {listed}"));
            }
            if questions.contains_key(seq) {
                line.push_str("   awaits answer");
            }
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Who is working what, and where two of them are in the same files.
fn agents(db: &Db, scope: &ProjectScope, out: &mut impl Write) -> anyhow::Result<()> {
    db.tasks().sweep_leases()?;
    let tasks = db.tasks().list(scope, None)?;
    let live: Vec<&TaskSummary> = tasks.iter().filter(|t| t.status.is_active()).collect();
    if live.is_empty() {
        writeln!(out, "no agent is working anything right now")?;
        return Ok(());
    }

    let declared = db.scopes().declared(scope, true)?;
    let scopes: BTreeMap<i64, Vec<String>> =
        declared.into_iter().map(|s| (s.seq, s.patterns)).collect();
    let now = Utc::now();

    for task in &live {
        let holder = task.claimed_by.as_deref().unwrap_or("unknown");
        let lease = task
            .lease_expires_at
            .as_deref()
            .map(|e| format!("  {}", fmt::lease_remaining(e, now)))
            .unwrap_or_default();
        writeln!(
            out,
            "{holder}  #{} {}  {}{lease}",
            task.seq,
            task.status,
            fmt::truncate(&task.title, 40)
        )?;
        match scopes.get(&task.seq) {
            Some(patterns) => writeln!(out, "    files  {}", patterns.join(", "))?,
            None => writeln!(out, "    files  (undeclared)")?,
        }
        // What was said, then what happened. Kept on separate lines because
        // the gap between them is the most useful thing on this screen.
        let seen = db.witnessed().touched(task.seq).unwrap_or_default();
        if !seen.is_empty() {
            let listed: Vec<String> = seen.iter().map(|o| o.path.clone()).collect();
            writeln!(out, "    moved  {}", listed.join(", "))?;
        } else if db.witnessed().footprint(task.seq).unwrap_or_default() == Footprint::ReadOnly {
            // An agent that has been in the code for twenty minutes and moved
            // nothing is reading, or is stuck. Either way it is worth a line,
            // and a blank space where the line would be says neither.
            writeln!(out, "    moved  nothing yet")?;
        }
        for other in &live {
            // Both directions: an agent reading this wants to see who is in
            // its way, not only who it is in the way of.
            if other.seq == task.seq {
                continue;
            }
            for overlap in overlapping(&scopes, task.seq, other.seq) {
                writeln!(
                    out,
                    "    !!     {overlap} also claimed by {} on #{}",
                    other.claimed_by.as_deref().unwrap_or("unknown"),
                    other.seq
                )?;
            }
        }
        for found in db.witnessed().contention(task.seq).unwrap_or_default() {
            writeln!(out, "    !!!    {}", found.describe())?;
        }
        // Ground that moved while the agent was on it: a dependency that
        // stopped being done — sent back, reopened, cancelled — after this
        // task was let through. The agent hears at its next check-in; the
        // human deserves the same line without waiting for one.
        for shifted in db.deps().shifted(task.seq).unwrap_or_default() {
            writeln!(out, "    !!!    {}", shifted.describe())?;
        }
    }
    Ok(())
}

/// Patterns from `a` that can name the same path as something in `b`.
fn overlapping(scopes: &BTreeMap<i64, Vec<String>>, a: i64, b: i64) -> Vec<String> {
    let (Some(ours), Some(theirs)) = (scopes.get(&a), scopes.get(&b)) else {
        return Vec::new();
    };
    ours.iter()
        .filter(|pattern| theirs.iter().any(|other| glob::intersects(pattern, other)))
        .cloned()
        .collect()
}

/// Read a file argument, or standard input when it is "-".
fn read_text(path: &Path) -> anyhow::Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading from stdin")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn ls(
    db: &Db,
    project: &str,
    config: &Config,
    args: &LsArgs,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let status = match args
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(raw.parse::<Status>()?),
        None => None,
    };
    let scope = ProjectScope::resolve(project, config.all_projects(Some(args.all_projects)));
    let tasks = db.tasks().list(&scope, status)?;

    // The recess leads the listing, because every row below it is a row
    // nobody is being handed. Only in the single-project view: a recess is
    // per project, and the all-projects listing has no one queue to speak for.
    if !scope.is_all() {
        if let Some(recess) = db.recesses().current(project)? {
            writeln!(out, "{}", recess_banner(&recess, Utc::now()))?;
        }
    }
    if tasks.is_empty() {
        writeln!(out, "no tasks")?;
        return Ok(());
    }
    let unmet = db.deps().unmet_map(&scope, config.clearance())?;
    let questions = db.questions().unanswered_map(&scope)?;
    // What each task did to the tree, which the row prints as one word. A
    // failure here costs the listing a column, not the listing.
    let footprints = db.witnessed().footprints(&scope).unwrap_or_default();
    let now = Utc::now();
    let width = tasks
        .iter()
        .map(|t| t.seq.to_string().len())
        .max()
        .unwrap_or(1);
    for task in &tasks {
        let blocked = unmet.get(&task.seq).map(Vec::as_slice).unwrap_or(&[]);
        let question = questions.get(&task.seq).map(|q| q.question.as_str());
        let footprint = footprints.get(&task.seq).copied().unwrap_or_default();
        writeln!(
            out,
            "{}",
            ls_line(
                task,
                width,
                scope.is_all(),
                now,
                blocked,
                question,
                footprint,
            )
        )?;
    }
    Ok(())
}

/// One `hird ls` row. Extracted so its layout can be tested directly.
fn ls_line(
    task: &TaskSummary,
    seq_width: usize,
    show_project: bool,
    now: chrono::DateTime<Utc>,
    blocked_by: &[i64],
    awaiting_answer: Option<&str>,
    footprint: Footprint,
) -> String {
    let mut line = format!(
        "#{seq:<width$}  {status:<11}  {title}",
        seq = task.seq,
        width = seq_width,
        status = task.status,
        title = fmt::truncate(&task.title, 48),
    );
    if let Some(holder) = &task.claimed_by {
        line.push_str(&format!("  [{holder}]"));
        if let Some(expires) = &task.lease_expires_at {
            line.push_str(&format!(" {}", fmt::lease_remaining(expires, now)));
        }
    }
    if !blocked_by.is_empty() {
        let listed = blocked_by
            .iter()
            .map(|s| format!("#{s}"))
            .collect::<Vec<_>>()
            .join(",");
        line.push_str(&format!("  waits {listed}"));
    }
    if awaiting_answer.is_some() {
        line.push_str("  awaits answer");
    }
    if task.priority != 0 {
        line.push_str(&format!("  p{}", task.priority));
    }
    if !task.requirements.is_empty() {
        line.push_str(&format!("  requires {}", task.requirements.join(",")));
    }
    // Whether the work left a mark. A task nobody watched says nothing here,
    // because "read-only" and "hird was not looking" are opposite claims and
    // only one of them is this column's to make.
    if let Some(label) = footprint.label(task.status.is_active()) {
        line.push_str(&format!("  {label}"));
    }
    if show_project {
        line.push_str(&format!("  ({})", task.project));
    }
    line
}

/// What the queue would tell an agent about this task before it starts.
///
/// The same view an agent gets on `task_claim`, so a human can see what their
/// swarm is being told — and notice when it is telling them something stale.
fn recall(
    db: &Db,
    config: &Config,
    project: &str,
    seq: i64,
    limit: usize,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let discovered = config.witness(Path::new(project));
    let recalled = footing::decorate(
        db,
        config.footing(discovered.as_ref()),
        db.recall().for_task(seq, limit)?,
    );
    if recalled.is_empty() {
        writeln!(out, "nothing recorded so far touches task {seq}")?;
        return Ok(());
    }
    let now = Utc::now();
    for item in recalled {
        writeln!(out, "{}", item.assertion.content)?;
        writeln!(
            out,
            "    {}  ({}, {})",
            item.reason.describe(),
            item.assertion.actor,
            fmt::age_phrase(&item.assertion.created_at, now)
        )?;
        // This is exactly what the agent will be handed on claiming, so the
        // human reading it should see the same hedge the agent will.
        if let Some(why) = item
            .standing
            .as_ref()
            .filter(|s| s.needs_checking())
            .and_then(Standing::describe)
        {
            writeln!(out, "    {why}")?;
        }
        if let Some(voices) = &item.corroboration {
            writeln!(out, "    {voices}")?;
        }
    }
    Ok(())
}

/// The uncommitted diff of what moved under a task, off the kept versions.
fn diff(
    db: &Db,
    seq: i64,
    only: Option<&str>,
    tenure: Option<i64>,
    config: &Config,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let task = db.tasks().get(seq)?;
    let Some(witness) = config.witness(Path::new(&task.project)) else {
        anyhow::bail!(
            "hird is not watching {} — no git repository there, or the witness is off — \
             so there is no diff to show",
            task.project
        );
    };
    // Bring the record up to the tree as it stands first: a diff that stops
    // at the last heartbeat shows less than the disk already knows. An
    // archived round is already over, so there is nothing to bring up to date.
    let mut shown = match tenure {
        Some(n) => exhibit::assemble_tenure(db, &witness, seq, n)?,
        None => {
            let _ = witness::sweep(db, &witness, &task.project, ACTOR_CLI);
            exhibit::assemble(db, &witness, seq)?
        }
    };
    let round = tenure
        .map(|n| format!(" in holding {n}"))
        .unwrap_or_default();
    if let Some(only) = only {
        shown.retain(|e| e.path == only);
        if shown.is_empty() {
            writeln!(
                out,
                "the witness saw nothing move at {only} under task {seq}{round}"
            )?;
            return Ok(());
        }
    }
    let text = exhibit::render(&shown);
    if text.trim().is_empty() {
        writeln!(
            out,
            "nothing moved under task {seq}{round} while the witness was watching"
        )?;
        return Ok(());
    }
    write!(out, "{text}")?;
    Ok(())
}

/// Recover a version of a file as it stood under a task.
#[allow(clippy::too_many_arguments)]
fn salvage(
    db: &Db,
    seq: i64,
    path: &str,
    baseline: bool,
    tenure: Option<i64>,
    to: Option<&Path>,
    force: bool,
    config: &Config,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let task = db.tasks().get(seq)?;
    let Some(witness) = config.witness(Path::new(&task.project)) else {
        anyhow::bail!(
            "hird is not watching {} — no git repository there, or the witness is off — \
             so nothing was kept to salvage",
            task.project
        );
    };
    if tenure.is_none() {
        let _ = witness::sweep(db, &witness, &task.project, ACTOR_CLI);
    }
    let bytes = exhibit::salvage(db, &witness, seq, path, baseline, tenure)?;
    let which = match (baseline, tenure) {
        (true, Some(n)) => format!("as it stood when holding {n} began"),
        (false, Some(n)) => format!("as the witness last saw it under holding {n}"),
        (true, None) => "as it stood when the task was claimed".to_string(),
        (false, None) => "as the witness last saw it under the task".to_string(),
    };
    match to {
        Some(dest) => {
            if dest.exists() && !force {
                anyhow::bail!(
                    "{} already exists; --force to say you meant to overwrite it",
                    dest.display()
                );
            }
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
            writeln!(
                out,
                "salvaged {path} {which} — {} bytes into {}",
                bytes.len(),
                dest.display()
            )?;
        }
        // To stdout raw, so it can be redirected or piped into a pager —
        // salvage is recovery, and recovery wants the bytes, not a report.
        None => out.write_all(&bytes)?,
    }
    Ok(())
}

fn show(db: &Db, seq: i64, config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    let task = db.tasks().get(seq)?;
    let now = Utc::now();

    writeln!(out, "#{} {}", task.seq, task.title)?;
    writeln!(out, "status    {}", task.status)?;
    writeln!(out, "project   {}", task.project)?;
    if task.priority != 0 {
        writeln!(out, "priority  {}", task.priority)?;
    }
    if let Some(holder) = &task.claimed_by {
        let lease = task
            .lease_expires_at
            .as_deref()
            .map(|e| format!(" ({})", fmt::lease_remaining(e, now)))
            .unwrap_or_default();
        writeln!(out, "held by   {holder}{lease}")?;
    }
    writeln!(out, "created   {}", fmt::age_phrase(&task.created_at, now))?;

    let (blockers, conflicts) = db.tasks().readiness(seq, config.clearance())?;
    if !blockers.is_empty() {
        let listed = blockers
            .iter()
            .map(|b| match b.pending_review {
                // Only reachable under `under_review = "holds"`: the work is
                // done, and what the dependent waits on is the verdict.
                Some(review) if b.status == Status::Done => {
                    format!("#{} (done, under review {review})", b.seq)
                }
                _ => format!("#{} ({})", b.seq, b.status),
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "waits for {listed}")?;
    }
    for question in db.questions().for_task(seq)? {
        writeln!(out, "question  {}", question.question)?;
        match question.answer {
            Some(answer) => writeln!(out, "answer    {answer}")?,
            None => writeln!(out, "awaits    hird answer {seq} <ANSWER>")?,
        }
    }
    // The ground under the task: what the work it builds on says for itself.
    // One line per finished dependency — standing first, then as much of its
    // result as a line can hold.
    for ground in db.deps().ground(seq)? {
        let result = ground
            .result
            .as_deref()
            .map(|r| format!(" — {}", fmt::truncate(r, 60)))
            .unwrap_or_default();
        writeln!(out, "built on  {}{result}", ground.label())?;
    }
    let dependents = db.deps().dependents(seq)?;
    if !dependents.is_empty() {
        let listed = dependents
            .iter()
            .map(|b| format!("#{}", b.seq))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "blocks    {listed}")?;
    }
    let patterns = db.scopes().for_task(seq)?;
    if !patterns.is_empty() {
        writeln!(out, "files     {}", patterns.join(", "))?;
    }
    let required = db.requirements().for_task(seq)?;
    if !required.is_empty() {
        writeln!(out, "requires  {}", required.join(", "))?;
    }
    if task.review {
        writeln!(out, "review    on finishing, by another harness")?;
    }
    for recusal in db.recusals().for_task(seq)? {
        writeln!(out, "recused   {}", recusal.describe())?;
    }
    // Two sides of the same table: what reviews concluded about this work,
    // and — when this task is itself a review — what it concluded.
    let judged = db.verdicts().for_task(seq)?;
    if let Some(latest) = judged.last() {
        let rounds = if judged.len() > 1 {
            format!(", verdict {} on this work", judged.len())
        } else {
            String::new()
        };
        writeln!(out, "verdict   {}{rounds}", latest.describe())?;
    }
    for delivered in db.verdicts().of_review(seq)? {
        writeln!(
            out,
            "verdict   {} on task {}, delivered by this review",
            delivered.verdict, delivered.task_seq
        )?;
    }
    for conflict in &conflicts {
        writeln!(out, "overlap   {}", conflict.describe())?;
    }
    // `files` is what somebody said would happen; `changed` is what the
    // working tree says did. On a finished task this is the evidence behind
    // the result line, and it was not written by the agent that wrote it.
    //
    // The headline goes first because it is the part a list of paths cannot
    // say: no paths means the task read and did not write, and it means that
    // only where hird was watching closely enough to know.
    let footprint = db.witnessed().footprint(seq).unwrap_or_default();
    if let Some(sentence) = footprint.describe(task.status.is_active()) {
        writeln!(out, "changed   {sentence}")?;
    }
    for observed in db.witnessed().touched(seq).unwrap_or_default() {
        writeln!(out, "          {}", observed.describe())?;
    }
    for found in db.witnessed().contention(seq).unwrap_or_default() {
        writeln!(out, "contended {}", found.describe())?;
    }
    // Earlier holdings, where the task has changed hands: who had it, how
    // that ended, what moved under them. `changed` above is the current
    // record; these are the rounds it replaced, still readable with
    // `hird diff <seq> --tenure <n>`.
    for held in db.witnessed().tenures(seq).unwrap_or_default() {
        writeln!(out, "held      {}", held.label())?;
    }

    if !task.body.trim().is_empty() {
        writeln!(out, "\n{}", task.body.trim_end())?;
    }
    if let Some(result) = &task.result {
        writeln!(out, "\nresult: {result}")?;
    }

    let events = db.tasks().events(&task.id, 20)?;
    if !events.is_empty() {
        writeln!(out, "\nhistory")?;
        for event in events {
            let detail = if event.detail.is_empty() {
                String::new()
            } else {
                format!("  {}", fmt::truncate(&event.detail, 72))
            };
            writeln!(
                out,
                "  {:>9}  {:<13} {}{}",
                fmt::age_phrase(&event.at, now),
                event.kind,
                event.actor,
                detail
            )?;
        }
    }

    let learned = db.memory().for_task(&task.id)?;
    if !learned.is_empty() {
        let discovered = config.witness(Path::new(&task.project));
        let ids: Vec<String> = learned.iter().map(|a| a.id.clone()).collect();
        let standings =
            footing::standings(db, config.footing(discovered.as_ref()), &task.project, &ids);
        writeln!(out, "\nassertions recorded on this task")?;
        for assertion in learned {
            writeln!(out, "  - {}", fmt::truncate(&assertion.content, 88))?;
            if let Some(why) = standings
                .get(&assertion.id)
                .filter(|s| s.needs_checking())
                .and_then(Standing::describe)
            {
                writeln!(out, "    {why}")?;
            }
        }
    }

    // Only what came from elsewhere: this task's own assertions are listed
    // above, and printing them twice would say nothing new.
    let elsewhere: Vec<_> = db
        .recall()
        .for_task(seq, config.recall_limit())?
        .into_iter()
        .filter(|r| r.reason != crate::repo::RecallReason::SameTask)
        .collect();
    if !elsewhere.is_empty() {
        writeln!(out, "\nrecalled from earlier work")?;
        for item in elsewhere {
            writeln!(out, "  - {}", fmt::truncate(&item.assertion.content, 88))?;
            writeln!(out, "    {}", item.reason.describe())?;
        }
    }
    Ok(())
}

/// `hird why`: the claim gates, run against one task and answered out loud.
///
/// Every check `task_claim` and `task_next` apply, in the order they apply
/// them, each reported rather than silently folded into an empty hand. The
/// verdict comes first — a human asking *why* wants the answer before the
/// evidence — and every line after it is one gate with something to say.
fn why(db: &Db, seq: i64, config: &Config, out: &mut impl Write) -> anyhow::Result<()> {
    // Readiness sweeps leases first, exactly as a claim would: a task whose
    // holder went quiet is open again by the time this answers.
    let (blockers, conflicts) = db.tasks().readiness(seq, config.clearance())?;
    let task = db.tasks().get(seq)?;
    let now = Utc::now();
    let question = db.questions().unanswered(seq)?;
    let recusals: Vec<_> = db
        .recusals()
        .for_task(seq)?
        .into_iter()
        // A recusal on work nobody has done bars nobody yet; `hird show`
        // lists those, but they are not part of why a claim would fail now.
        .filter(|r| r.worker.is_some())
        .collect();
    let required = db.requirements().for_task(seq)?;
    let refuses = config.on_conflict() == crate::repo::OnConflict::Refuse;

    writeln!(out, "#{} {}", task.seq, task.title)?;
    writeln!(out, "status    {}", task.status)?;

    // A lease or a terminal status answers the question by itself.
    if task.status.is_active() {
        let holder = task.claimed_by.as_deref().unwrap_or("?");
        let lease = task
            .lease_expires_at
            .as_deref()
            .map(|e| format!(", {}", fmt::lease_remaining(e, now)))
            .unwrap_or_default();
        writeln!(out, "claimable no — held by {holder}{lease}")?;
        return Ok(());
    }
    if task.status.is_terminal() {
        writeln!(
            out,
            "claimable no — {}; hird reopen {seq} returns it to the pool",
            task.status
        )?;
        return Ok(());
    }

    // What refuses the claim outright, in gate order.
    let mut stoppers: Vec<String> = Vec::new();
    let recess = db.recesses().current(&task.project)?;
    if recess.is_some() {
        stoppers.push("hird resume — the queue is in recess".to_string());
    }
    if question.is_some() {
        stoppers.push("an answer".to_string());
    }
    if !blockers.is_empty() {
        // The one distinction dispatch makes inside "blocked": work that is
        // all done but for a verdict is held, not waiting on hands.
        let verdicts: Vec<String> = blockers
            .iter()
            .filter(|b| b.status == Status::Done)
            .filter_map(|b| b.pending_review.map(|r| format!("#{r}")))
            .collect();
        let unfinished: Vec<String> = blockers
            .iter()
            .filter(|b| !(b.status == Status::Done && b.pending_review.is_some()))
            .map(|b| format!("#{}", b.seq))
            .collect();
        if !unfinished.is_empty() {
            stoppers.push(unfinished.join(", "));
        }
        if !verdicts.is_empty() {
            stoppers.push(format!("the verdict of review {}", verdicts.join(", ")));
        }
    }
    if refuses && !conflicts.is_empty() {
        stoppers.push("the overlap below to clear".to_string());
    }

    // What narrows who may claim without stopping everyone.
    let mut caveats: Vec<String> = Vec::new();
    if !recusals.is_empty() {
        let barred: Vec<&str> = recusals
            .iter()
            .filter_map(|r| r.worker.as_deref())
            .map(crate::identity::actor_harness)
            .collect();
        caveats.push(format!("not by {}", barred.join(" or ")));
    }
    if !required.is_empty() {
        caveats.push(format!(
            "only by a harness advertising {}",
            required.join(", ")
        ));
    }
    if !refuses && !conflicts.is_empty() && config.avoid_conflicts(None) {
        caveats.push(
            "task_next passes it over while the overlap lives; claiming it by number still works"
                .to_string(),
        );
    }

    if stoppers.is_empty() {
        let trailing = if caveats.is_empty() {
            String::new()
        } else {
            format!(" — {}", caveats.join("; "))
        };
        writeln!(out, "claimable yes{trailing}")?;
    } else {
        writeln!(out, "claimable no — waiting on {}", stoppers.join(" and "))?;
        for caveat in &caveats {
            writeln!(out, "and then   {caveat}")?;
        }
    }

    // The evidence behind the verdict, one gate per line.
    if let Some(recess) = &recess {
        writeln!(
            out,
            "recess    the human stood this queue down{}, {}",
            recess.quoted_reason(),
            fmt::age_phrase(&recess.at, now)
        )?;
    }
    if let Some(question) = question {
        writeln!(out, "awaits    {}", question.question)?;
        writeln!(
            out,
            "          asked by {} {} — hird answer {seq} <ANSWER>",
            question.asked_by,
            fmt::age_phrase(&question.asked_at, now)
        )?;
    }
    if !blockers.is_empty() {
        let listed = blockers
            .iter()
            .map(|b| match b.pending_review {
                Some(review) if b.status == Status::Done => {
                    format!("#{} (done, under review {review})", b.seq)
                }
                _ => format!("#{} ({})", b.seq, b.status),
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "waits for {listed}")?;
    }
    for recusal in &recusals {
        writeln!(out, "recused   {}", recusal.describe())?;
    }
    if !required.is_empty() {
        writeln!(out, "requires  {}", required.join(", "))?;
    }
    for conflict in &conflicts {
        writeln!(out, "overlap   {}", conflict.describe())?;
    }
    if refuses && !conflicts.is_empty() {
        writeln!(
            out,
            "          path_conflicts = \"refuse\": a claim is rejected while the overlap lives"
        )?;
    }
    Ok(())
}

fn mem(
    db: &Db,
    project: &str,
    config: &Config,
    cmd: &MemCommand,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let discovered = config.witness(Path::new(project));
    let witness = config.footing(discovered.as_ref());
    match cmd {
        MemCommand::Add {
            content,
            tags,
            task,
            paths,
        } => {
            let recorded = db.memory().record(NewAssertion {
                project,
                content,
                tags,
                actor: ACTOR_CLI,
                task_seq: *task,
            })?;
            let ground =
                footing::ground(db, *task, Some(paths.as_slice()).filter(|p| !p.is_empty()));
            let anchored = footing::anchor(db, witness, &recorded.assertion().id, &ground)?;
            writeln!(out, "{}", recorded.assertion().id)?;
            if recorded.was_affirmed() {
                writeln!(
                    out,
                    "  already on record — affirmed, not duplicated{}",
                    if anchored.is_empty() {
                        String::new()
                    } else {
                        " and re-anchored".to_string()
                    }
                )?;
            }
            if !anchored.is_empty() {
                let paths: Vec<&str> = anchored.iter().map(|a| a.path.as_str()).collect();
                writeln!(out, "  anchored to {}", paths.join(", "))?;
            }
            Ok(())
        }
        MemCommand::Standing {
            shaky,
            all_projects,
        } => {
            let scope = ProjectScope::resolve(project, config.all_projects(Some(*all_projects)));
            standing(db, config, witness, &scope, *shaky, out)
        }
        MemCommand::Export { path, firm } => {
            mem_export(db, project, witness, path.as_deref(), *firm, out)
        }
        MemCommand::Search {
            query,
            limit,
            all_projects,
            include_superseded,
        } => {
            let scope = ProjectScope::resolve(project, config.all_projects(Some(*all_projects)));
            let hits = db.memory().search(
                &MemoryQuery::new(query, scope.clone())
                    .limit((*limit).clamp(1, 500))
                    .include_superseded(*include_superseded),
            )?;
            if hits.is_empty() {
                writeln!(out, "no assertions")?;
                return Ok(());
            }
            let now = Utc::now();
            let ids: Vec<String> = hits.iter().map(|a| a.id.clone()).collect();
            let standings = footing::standings(db, witness, project, &ids);
            for assertion in hits {
                let standing = standings.get(&assertion.id);
                let mut meta = vec![
                    assertion.actor.clone(),
                    fmt::age_phrase(&assertion.created_at, now),
                ];
                if !assertion.tags.is_empty() {
                    meta.push(format!("#{}", assertion.tags.replace(',', " #")));
                }
                if assertion.superseded_by.is_some() {
                    meta.push("superseded".to_string());
                }
                if let Some(standing) = standing.filter(|s| **s != Standing::Unanchored) {
                    meta.push(standing.as_str().to_string());
                }
                if scope.is_all() {
                    meta.push(assertion.project.clone());
                }
                writeln!(out, "{}", assertion.content)?;
                writeln!(out, "    {}  {}", assertion.id, meta.join("  "))?;
                // Only the ones worth acting on explain themselves. A line
                // saying "unchanged" under every row is how a reader learns to
                // skip the line under the row that says otherwise.
                if let Some(why) = standing
                    .filter(|s| s.needs_checking())
                    .and_then(|s| s.describe())
                {
                    writeln!(out, "    {why}")?;
                }
            }
            Ok(())
        }
    }
}

/// `hird mem standing`: what the memory still stands on.
///
/// Deciding a standing means resolving paths against a working tree, and each
/// project has its own — so with `--all-projects` this discovers a witness per
/// project rather than measuring everybody's files against the checkout the
/// command happened to be run from.
fn standing(
    db: &Db,
    config: &Config,
    witness: Option<&witness::Witness>,
    scope: &ProjectScope,
    shaky_only: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    if witness.is_none() && !scope.is_all() {
        writeln!(
            out,
            "no footing here: assertions are only anchored to files in a git checkout \
             with `memory_footing` on"
        )?;
        return Ok(());
    }
    let anchored = db.footings().anchored(scope)?;
    if anchored.is_empty() {
        writeln!(out, "no anchored assertions")?;
        return Ok(());
    }

    let now = Utc::now();
    let mut by_project: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for assertion in &anchored {
        by_project
            .entry(assertion.project.as_str())
            .or_default()
            .push(assertion.id.clone());
    }
    let mut standings: BTreeMap<String, Standing> = BTreeMap::new();
    for (project, ids) in by_project {
        let discovered = config.witness(Path::new(project));
        standings.extend(footing::standings(
            db,
            config.footing(discovered.as_ref()),
            project,
            &ids,
        ));
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut shown = 0usize;

    for assertion in &anchored {
        let standing = standings
            .get(&assertion.id)
            .cloned()
            .unwrap_or(Standing::Unanchored);
        *counts.entry(standing.as_str()).or_default() += 1;
        if shaky_only && !standing.needs_checking() {
            continue;
        }
        shown += 1;
        writeln!(
            out,
            "{:<9} {}",
            standing.as_str(),
            fmt::truncate(&assertion.content, 78)
        )?;
        let mut meta = vec![
            assertion.actor.clone(),
            fmt::age_phrase(&assertion.created_at, now),
        ];
        if scope.is_all() {
            meta.push(assertion.project.clone());
        }
        writeln!(out, "    {}  {}", assertion.id, meta.join("  "))?;
        writeln!(out, "    {}", standing.paths().join(", "))?;
        if let Some(why) = standing.describe().filter(|_| standing.needs_checking()) {
            writeln!(out, "    {why}")?;
        }
        if let Some(voices) = footing::corroboration(db, assertion) {
            writeln!(out, "    {voices}")?;
        }
    }

    if shown == 0 {
        writeln!(
            out,
            "nothing shaky: every anchored assertion still checks out"
        )?;
    }
    let summary: Vec<String> = counts
        .iter()
        .map(|(label, n)| format!("{n} {label}"))
        .collect();
    writeln!(out, "\n{} anchored: {}", anchored.len(), summary.join(", "))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    const PROJECT: &str = "/tmp/project";

    fn now() -> chrono::DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }
    use chrono::DateTime;

    fn summary(seq: i64, status: Status) -> TaskSummary {
        TaskSummary {
            seq,
            project: PROJECT.into(),
            title: "write the parser".into(),
            status,
            priority: 0,
            claimed_by: None,
            lease_expires_at: None,
            updated_at: crate::model::fmt_ts(now()),
            requirements: Vec::new(),
        }
    }

    #[test]
    fn the_command_tree_parses() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ls_rows_line_up_and_omit_empty_columns() {
        let line = ls_line(
            &summary(7, Status::Open),
            2,
            false,
            now(),
            &[],
            None,
            Footprint::Unwatched,
        );
        assert_eq!(line, "#7   open         write the parser");
    }

    #[test]
    fn ls_rows_show_the_holder_and_lease_countdown() {
        let mut task = summary(7, Status::Claimed);
        task.claimed_by = Some("codex:9f2c".into());
        task.lease_expires_at = Some(crate::model::fmt_ts(now() + chrono::Duration::minutes(12)));

        let line = ls_line(&task, 1, false, now(), &[], None, Footprint::Unwatched);
        assert!(line.contains("[codex:9f2c] 12m left"), "{line}");
    }

    #[test]
    fn ls_rows_show_priority_and_project_only_when_relevant() {
        let mut task = summary(7, Status::Open);
        task.priority = 5;
        assert!(ls_line(&task, 1, false, now(), &[], None, Footprint::Unwatched).ends_with("  p5"));
        assert!(
            ls_line(&task, 1, true, now(), &[], None, Footprint::Unwatched)
                .ends_with(&format!("({PROJECT})"))
        );
    }

    #[test]
    fn add_body_flags_are_mutually_exclusive() {
        let err = Cli::try_parse_from(["hird", "add", "t", "--body", "x", "--body-file", "y"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn negative_priorities_parse() {
        let cli = Cli::try_parse_from(["hird", "add", "t", "--priority", "-3"]).unwrap();
        match cli.command {
            Some(Command::Add(args)) => assert_eq!(args.priority, -3),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn list_is_an_alias_for_ls() {
        assert!(matches!(
            Cli::try_parse_from(["hird", "list"]).unwrap().command,
            Some(Command::Ls(_))
        ));
    }

    #[test]
    fn dependencies_can_be_given_as_a_list_or_repeated() {
        let comma = Cli::try_parse_from(["hird", "add", "t", "--needs", "3,4"]).unwrap();
        let repeated =
            Cli::try_parse_from(["hird", "add", "t", "--needs", "3", "--needs", "4"]).unwrap();
        for cli in [comma, repeated] {
            match cli.command {
                Some(Command::Add(args)) => assert_eq!(args.needs, vec![3, 4]),
                other => panic!("parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn paths_are_repeatable_and_kept_verbatim_for_normalization() {
        let cli = Cli::try_parse_from(["hird", "add", "t", "--path", "src/**", "--path", "tests/"])
            .unwrap();
        match cli.command {
            Some(Command::Add(args)) => assert_eq!(args.paths, vec!["src/**", "tests/"]),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_plan_is_applied_from_a_file_and_dry_run_is_opt_in() {
        let cli = Cli::try_parse_from(["hird", "plan", "apply", "plan.toml"]).unwrap();
        match cli.command {
            Some(Command::Plan(PlanCommand::Apply { file, dry_run, .. })) => {
                assert_eq!(file, PathBuf::from("plan.toml"));
                assert!(!dry_run);
            }
            other => panic!("parsed as {other:?}"),
        }
        let dry = Cli::try_parse_from(["hird", "plan", "apply", "-", "--dry-run"]).unwrap();
        match dry.command {
            Some(Command::Plan(PlanCommand::Apply { file, dry_run, .. })) => {
                assert_eq!(file, PathBuf::from("-"));
                assert!(dry_run);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn counted_nouns_agree_with_their_number() {
        assert_eq!(count(1, "task"), "1 task");
        assert_eq!(count(0, "task"), "0 tasks");
        assert_eq!(count(3, "wave"), "3 waves");
        assert_eq!(count(1, "dependency"), "1 dependency");
        assert_eq!(count(2, "dependency"), "2 dependencies");
    }

    #[test]
    fn clearing_a_scope_conflicts_with_setting_one() {
        let err =
            Cli::try_parse_from(["hird", "scope", "1", "--clear", "--path", "src/**"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn ls_rows_mark_the_tasks_that_cannot_start_yet() {
        let line = ls_line(
            &summary(7, Status::Open),
            1,
            false,
            now(),
            &[3, 4],
            None,
            Footprint::Unwatched,
        );
        assert!(line.contains("waits #3,#4"), "{line}");
    }

    #[test]
    fn overlaps_are_reported_both_ways_or_not_at_all() {
        let mut scopes = BTreeMap::new();
        scopes.insert(1, vec!["src/**".to_string()]);
        scopes.insert(2, vec!["src/db.rs".to_string()]);
        scopes.insert(3, vec!["docs/**".to_string()]);

        assert_eq!(overlapping(&scopes, 1, 2), vec!["src/**".to_string()]);
        assert_eq!(overlapping(&scopes, 2, 1), vec!["src/db.rs".to_string()]);
        assert!(overlapping(&scopes, 1, 3).is_empty());
        assert!(overlapping(&scopes, 1, 99).is_empty());
    }

    #[test]
    fn every_harness_is_registrable_by_name() {
        for (word, expected) in [
            ("claude-code", register::Harness::ClaudeCode),
            ("codex", register::Harness::Codex),
            ("copilot", register::Harness::Copilot),
            ("copilot-cli", register::Harness::CopilotCli),
            ("opencode", register::Harness::OpenCode),
        ] {
            let cli = Cli::try_parse_from(["hird", "register", word]).unwrap();
            match cli.command {
                Some(Command::Register(args)) => {
                    assert_eq!(args.harness, expected);
                    assert_eq!(args.name, "hird");
                    assert!(!args.print && !args.force);
                }
                other => panic!("parsed as {other:?}"),
            }
        }
        assert!(Cli::try_parse_from(["hird", "register", "emacs"]).is_err());
    }

    #[test]
    fn printing_a_registration_cannot_also_force_one() {
        let err =
            Cli::try_parse_from(["hird", "register", "codex", "--print", "--force"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn mem_search_defaults_to_an_empty_query() {
        let cli = Cli::try_parse_from(["hird", "mem", "search"]).unwrap();
        match cli.command {
            Some(Command::Mem(MemCommand::Search { query, limit, .. })) => {
                assert_eq!(query, "");
                assert_eq!(limit, 20);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn installer_options_parse_without_a_subcommand_and_may_be_combined() {
        let mcp = Cli::try_parse_from(["hird", "--install"]).unwrap();
        assert!(mcp.install);
        assert!(mcp.command.is_none());

        let both = Cli::try_parse_from(["hird", "--install", "--install-skill"]).unwrap();
        assert!(both.install);
        assert!(both.install_skill);
        assert!(both.command.is_none());
    }

    #[test]
    fn the_install_typo_is_not_an_option() {
        let err = Cli::try_parse_from(["hird", "--insytall"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn installer_options_and_normal_commands_are_detectably_distinct() {
        let cli = Cli::try_parse_from(["hird", "--install-skill", "ls"]).unwrap();
        assert!(cli.install_skill);
        assert!(matches!(cli.command, Some(Command::Ls(_))));
    }
}
