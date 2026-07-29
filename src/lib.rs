//! `hird` — a cross-harness agent work queue and shared assertion memory.
//!
//! Several AI coding agents, each in its own harness (Claude Code, Codex CLI,
//! Copilot, …), coordinate through one local SQLite database: a work queue with
//! atomic claiming and lease-based liveness, and an assertion store carrying
//! provenance. A human drives both from a `ratatui` TUI or the CLI.
//!
//! Layering:
//!
//! - [`model`] — domain types and the task status machine
//! - [`db`] — connection setup and schema migrations
//! - [`repo`] — the only place SQL is written
//! - [`witness`] — the only place the working tree is read
//! - [`plan`] — a dependency graph as a file, which [`repo`] turns into rows
//! - [`mcp`], [`cli`], [`tui`] — the three front ends, which call [`repo`]

pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod fmt;
pub mod glob;
pub mod hash;
pub mod identity;
pub mod install;
pub mod mcp;
pub mod model;
pub mod plan;
pub mod repo;
pub mod tui;
pub mod witness;

pub use config::Config;
pub use db::Db;
pub use error::{Error, Result};
