//! `hird` — one binary, three modes.
//!
//! `hird mcp` speaks MCP on stdio for a harness, `hird tui` draws the board for
//! a human, and everything else is a one-shot CLI command. Only `mcp` needs an
//! async runtime, and it builds a single-threaded one so process startup stays
//! well inside the budget harnesses expect.

use std::io::Write;

use clap::Parser;
use hird::cli::{Cli, Command};
use hird::config::{self, Config};
use hird::Db;

fn main() {
    if let Err(err) = run() {
        // The MCP transport owns stdout, so diagnostics always go to stderr.
        let _ = writeln!(std::io::stderr(), "hird: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if (cli.install || cli.install_skill) && cli.command.is_some() {
        anyhow::bail!("installer options cannot be used with a subcommand");
    }
    if cli.install {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        hird::install::install_binary(&mut out)?;
        out.flush()?;
    }
    if cli.install_skill {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        hird::install::install_skill(&mut out)?;
        out.flush()?;
    }
    if cli.install || cli.install_skill {
        return Ok(());
    }

    match cli.command.as_ref().expect("clap requires a command") {
        Command::Mcp => serve_mcp(&cli),
        Command::Tui => {
            let config = Config::load_default()?;
            let db_path = config::resolve_db_path(cli.db.as_deref());
            let db = Db::open(&db_path)?;
            hird::tui::run(db, config)
        }
        _ => {
            let stdout = std::io::stdout();
            let mut out = std::io::BufWriter::new(stdout.lock());
            let result = hird::cli::run(&cli, &mut out);
            out.flush()?;
            result
        }
    }
}

fn serve_mcp(cli: &Cli) -> anyhow::Result<()> {
    let config = Config::load_default()?;
    let db_path = config::resolve_db_path(cli.db.as_deref());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(hird::mcp::serve(&db_path, config))
}
