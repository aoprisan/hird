//! The command line: `hird add`, `ls`, `show`, `cancel`, `reopen`, `mem …`.
//!
//! Everything a human does here is recorded with `actor = "cli"`, so the TUI's
//! history pane shows who did what.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;
use clap::{Args, Parser, Subcommand};

use crate::config::{self, Config};
use crate::db::Db;
use crate::fmt;
use crate::identity::{self, ACTOR_CLI};
use crate::model::{Status, TaskSummary};
use crate::repo::{MemoryQuery, NewAssertion, ProjectScope};

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
    }
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
    writeln!(out, "{}", task.seq)?;
    Ok(())
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
    let now = Utc::now();
    let width = tasks
        .iter()
        .map(|t| t.seq.to_string().len())
        .max()
        .unwrap_or(1);
    for task in &tasks {
        writeln!(out, "{}", ls_line(task, width, scope.is_all(), now))?;
    }
    Ok(())
}

/// One `hird ls` row. Extracted so its layout can be tested directly.
fn ls_line(
    task: &TaskSummary,
    seq_width: usize,
    show_project: bool,
    now: chrono::DateTime<Utc>,
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
    writeln!(out, "created   {} ago", fmt::age(&task.created_at, now))?;
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
                "  {:>5} ago  {:<13} {}{}",
                fmt::age(&event.at, now),
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
                    format!("{} ago", fmt::age(&assertion.created_at, now)),
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
        let line = ls_line(&summary(7, Status::Open), 2, false, now());
        assert_eq!(line, "#7   open         write the parser");
    }

    #[test]
    fn ls_rows_show_the_holder_and_lease_countdown() {
        let mut task = summary(7, Status::Claimed);
        task.claimed_by = Some("codex:9f2c".into());
        task.lease_expires_at = Some(crate::model::fmt_ts(now() + chrono::Duration::minutes(12)));

        let line = ls_line(&task, 1, false, now());
        assert!(line.contains("[codex:9f2c] 12m left"), "{line}");
    }

    #[test]
    fn ls_rows_show_priority_and_project_only_when_relevant() {
        let mut task = summary(7, Status::Open);
        task.priority = 5;
        assert!(ls_line(&task, 1, false, now()).ends_with("  p5"));
        assert!(ls_line(&task, 1, true, now()).ends_with(&format!("({PROJECT})")));
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
