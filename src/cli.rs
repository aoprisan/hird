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
use crate::fmt;
use crate::glob;
use crate::identity::{self, ACTOR_CLI};
use crate::model::{Status, TaskSummary};
use crate::repo::{dispatch_waves, MemoryQuery, NewAssertion, OnConflict, ProjectScope};

/// Coordinate AI coding agents across harnesses through a shared work queue
/// and a shared assertion memory, all in one local SQLite database.
#[derive(Debug, Parser)]
#[command(name = "hird", version, about, long_about = None)]
pub struct Cli {
    /// Database file to use. Overrides HIRD_DB and the default location.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
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
    /// Add or remove dependencies between tasks.
    #[command(subcommand)]
    Dep(DepCommand),
    /// Print the queue as dispatch waves: what can be worked now, and what
    /// each later wave is waiting for.
    Graph(ScopeFilterArgs),
    /// Show or set the files a task is expected to touch.
    Scope(ScopeArgs),
    /// Show which agent is working what, and where they overlap.
    Agents(ScopeFilterArgs),
    /// Store and search assertions.
    #[command(subcommand)]
    Mem(MemCommand),
    /// Watch and drive the queue in a terminal UI.
    Tui,
    /// Serve the Model Context Protocol on stdio. Harnesses run this.
    Mcp,
    /// Print the database path this invocation would use.
    DbPath,
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
pub struct ScopeFilterArgs {
    /// Span every project rather than just the current one.
    #[arg(long)]
    pub all_projects: bool,
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
    Add {
        /// The assertion, in plain prose.
        content: String,
        /// Comma-separated tags.
        #[arg(long, default_value = "")]
        tags: String,
        /// Link it to the task it was learned on.
        #[arg(long, value_name = "SEQ")]
        task: Option<i64>,
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
}

/// Run a parsed command, writing human-readable output to `out`.
///
/// `Command::Tui` and `Command::Mcp` are handled by the binary, which owns the
/// terminal and the async runtime; they are rejected here.
pub fn run(cli: &Cli, out: &mut impl Write) -> anyhow::Result<()> {
    let db_path = config::resolve_db_path(cli.db.as_deref());

    if let Command::DbPath = cli.command {
        writeln!(out, "{}", db_path.display())?;
        return Ok(());
    }

    let config = Config::load_default()?;
    let db = Db::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = identity::resolve_project(&cwd);

    match &cli.command {
        Command::DbPath => unreachable!("handled above"),
        Command::Tui | Command::Mcp => {
            anyhow::bail!(
                "`hird {}` is served by the binary, not the command dispatcher",
                match cli.command {
                    Command::Tui => "tui",
                    _ => "mcp",
                }
            )
        }
        Command::Add(args) => add(&db, &project, args, out),
        Command::Ls(args) => ls(&db, &project, &config, args, out),
        Command::Show { seq } => show(&db, *seq, out),
        Command::Cancel { seq, reason } => {
            let task = db.tasks().cancel(*seq, ACTOR_CLI, reason)?;
            writeln!(out, "task {} cancelled", task.seq)?;
            Ok(())
        }
        Command::Reopen { seq, reason } => {
            let task = db.tasks().reopen(*seq, ACTOR_CLI, reason)?;
            writeln!(out, "task {} reopened", task.seq)?;
            Ok(())
        }
        Command::Mem(cmd) => mem(&db, &project, &config, cmd, out),
        Command::Dep(cmd) => dep(&db, cmd, out),
        Command::Graph(args) => graph(&db, &scope_of(&project, &config, args.all_projects), out),
        Command::Scope(args) => scope_cmd(&db, args, out),
        Command::Agents(args) => agents(&db, &scope_of(&project, &config, args.all_projects), out),
    }
}

fn scope_of(project: &str, config: &Config, all_projects: bool) -> ProjectScope {
    ProjectScope::resolve(project, config.all_projects(Some(all_projects)))
}

fn add(db: &Db, project: &str, args: &AddArgs, out: &mut impl Write) -> anyhow::Result<()> {
    let body = match (&args.body, &args.body_file) {
        (Some(body), _) => body.clone(),
        (None, Some(path)) => read_body(path)?,
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
    writeln!(out, "{}", task.seq)?;
    Ok(())
}

fn dep(db: &Db, cmd: &DepCommand, out: &mut impl Write) -> anyhow::Result<()> {
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

/// Print the queue as dispatch waves.
///
/// A wave is everything that becomes workable once the previous wave is done,
/// which is the shape a human actually wants to see: not "who points at whom"
/// but "how much of this can run at once, and what is the critical path".
fn graph(db: &Db, scope: &ProjectScope, out: &mut impl Write) -> anyhow::Result<()> {
    let tasks = db.tasks().list(scope, None)?;
    let edges = db.deps().edges(scope)?;
    let waves = dispatch_waves(&tasks, &edges);
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

fn read_body(path: &Path) -> anyhow::Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading task body from stdin")?;
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

    if tasks.is_empty() {
        writeln!(out, "no tasks")?;
        return Ok(());
    }
    let unmet = db.deps().unmet_map(&scope)?;
    let now = Utc::now();
    let width = tasks
        .iter()
        .map(|t| t.seq.to_string().len())
        .max()
        .unwrap_or(1);
    for task in &tasks {
        let blocked = unmet.get(&task.seq).map(Vec::as_slice).unwrap_or(&[]);
        writeln!(
            out,
            "{}",
            ls_line(task, width, scope.is_all(), now, blocked)
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
    if task.priority != 0 {
        line.push_str(&format!("  p{}", task.priority));
    }
    if show_project {
        line.push_str(&format!("  ({})", task.project));
    }
    line
}

fn show(db: &Db, seq: i64, out: &mut impl Write) -> anyhow::Result<()> {
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

    let (blockers, conflicts) = db.tasks().readiness(seq)?;
    if !blockers.is_empty() {
        let listed = blockers
            .iter()
            .map(|b| format!("#{} ({})", b.seq, b.status))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "waits for {listed}")?;
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
    for conflict in &conflicts {
        writeln!(out, "overlap   {}", conflict.describe())?;
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
        writeln!(out, "\nassertions recorded on this task")?;
        for assertion in learned {
            writeln!(out, "  - {}", fmt::truncate(&assertion.content, 88))?;
        }
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
    match cmd {
        MemCommand::Add {
            content,
            tags,
            task,
        } => {
            let assertion = db.memory().store(NewAssertion {
                project,
                content,
                tags,
                actor: ACTOR_CLI,
                task_seq: *task,
            })?;
            writeln!(out, "{}", assertion.id)?;
            Ok(())
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
            for assertion in hits {
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
                if scope.is_all() {
                    meta.push(assertion.project.clone());
                }
                writeln!(out, "{}", assertion.content)?;
                writeln!(out, "    {}  {}", assertion.id, meta.join("  "))?;
            }
            Ok(())
        }
    }
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
        }
    }

    #[test]
    fn the_command_tree_parses() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ls_rows_line_up_and_omit_empty_columns() {
        let line = ls_line(&summary(7, Status::Open), 2, false, now(), &[]);
        assert_eq!(line, "#7   open         write the parser");
    }

    #[test]
    fn ls_rows_show_the_holder_and_lease_countdown() {
        let mut task = summary(7, Status::Claimed);
        task.claimed_by = Some("codex:9f2c".into());
        task.lease_expires_at = Some(crate::model::fmt_ts(now() + chrono::Duration::minutes(12)));

        let line = ls_line(&task, 1, false, now(), &[]);
        assert!(line.contains("[codex:9f2c] 12m left"), "{line}");
    }

    #[test]
    fn ls_rows_show_priority_and_project_only_when_relevant() {
        let mut task = summary(7, Status::Open);
        task.priority = 5;
        assert!(ls_line(&task, 1, false, now(), &[]).ends_with("  p5"));
        assert!(ls_line(&task, 1, true, now(), &[]).ends_with(&format!("({PROJECT})")));
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
            Command::Add(args) => assert_eq!(args.priority, -3),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn list_is_an_alias_for_ls() {
        assert!(matches!(
            Cli::try_parse_from(["hird", "list"]).unwrap().command,
            Command::Ls(_)
        ));
    }

    #[test]
    fn dependencies_can_be_given_as_a_list_or_repeated() {
        let comma = Cli::try_parse_from(["hird", "add", "t", "--needs", "3,4"]).unwrap();
        let repeated =
            Cli::try_parse_from(["hird", "add", "t", "--needs", "3", "--needs", "4"]).unwrap();
        for cli in [comma, repeated] {
            match cli.command {
                Command::Add(args) => assert_eq!(args.needs, vec![3, 4]),
                other => panic!("parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn paths_are_repeatable_and_kept_verbatim_for_normalization() {
        let cli = Cli::try_parse_from(["hird", "add", "t", "--path", "src/**", "--path", "tests/"])
            .unwrap();
        match cli.command {
            Command::Add(args) => assert_eq!(args.paths, vec!["src/**", "tests/"]),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn clearing_a_scope_conflicts_with_setting_one() {
        let err =
            Cli::try_parse_from(["hird", "scope", "1", "--clear", "--path", "src/**"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn ls_rows_mark_the_tasks_that_cannot_start_yet() {
        let line = ls_line(&summary(7, Status::Open), 1, false, now(), &[3, 4]);
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
    fn mem_search_defaults_to_an_empty_query() {
        let cli = Cli::try_parse_from(["hird", "mem", "search"]).unwrap();
        match cli.command {
            Command::Mem(MemCommand::Search { query, limit, .. }) => {
                assert_eq!(query, "");
                assert_eq!(limit, 20);
            }
            other => panic!("parsed as {other:?}"),
        }
    }
}
