//! The terminal UI: `hird tui`.
//!
//! A queue board and a memory browser over the same SQLite file the agents are
//! writing to. There is no daemon and no change feed, so the UI simply re-reads
//! the database twice a second; the queries are indexed and WAL keeps them from
//! blocking the agents.

pub mod app;
mod theme;
mod view;

pub use app::{App, Column, Mode, Screen};

use std::path::PathBuf;

use chrono::Utc;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::config::Config;
use crate::db::Db;
use crate::identity;

/// Run the TUI until the user quits.
pub fn run(db: Db, config: Config) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project = identity::resolve_project(&cwd);
    // The board polls twice a second, which makes it the fastest sweeper in
    // any running swarm and so usually the one that collects a dead agent's
    // lease. Without a herald of its own it would collect that expiry and
    // swallow the summons that should replace the agent.
    let herald = config.herald(db.path());
    let mut app = App::new(db.path().to_path_buf(), project, config).with_herald(herald);
    app.refresh(&db)?;

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &mut app, &db);
    ratatui::restore();
    outcome
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    db: &Db,
) -> anyhow::Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| view::render(frame, app, Utc::now()))?;

        // Waking on either input or the poll deadline keeps the board live
        // without spinning: an idle UI does two wakeups a second.
        if event::poll(app::POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key, db)?,
                _ => {}
            }
        }

        if app.due_for_refresh(Utc::now()) {
            app.refresh(db)?;
        }
    }
    Ok(())
}
