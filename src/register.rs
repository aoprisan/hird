//! `hird register`: write this binary's MCP registration into a harness's
//! config file.
//!
//! The failure this exists to prevent is a session that has hird's instructions
//! and none of its tools. Two things cause it, and both are invisible from
//! inside the harness: the block went into a file that harness does not read,
//! or `command` was the bare word `hird`, which resolves against the harness's
//! `PATH` and not the shell's. So the registration written here always names
//! the absolute path of the binary doing the writing, and each harness has one
//! file it is written to.
//!
//! Nothing here starts a server or restarts an editor. What it can do is say
//! which step is left, which is why every outcome carries one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::ValueEnum;
use serde_json::{json, Map, Value as Json};

use crate::identity::{self, DB_ENV, HARNESS_ENV};

/// A harness hird knows how to register itself with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Harness {
    /// Claude Code — project-scoped, in `./.mcp.json`.
    ClaudeCode,
    /// The Codex CLI — in `~/.codex/config.toml`.
    Codex,
    /// Copilot in VS Code — project-scoped, in `./.vscode/mcp.json`.
    Copilot,
    /// The Copilot CLI — in `~/.copilot/mcp-config.json`.
    CopilotCli,
}

/// How a harness's config file is shaped.
enum Shape {
    /// JSON, with the servers under one top-level key.
    Json { container: &'static str },
    /// TOML, with the servers under `[mcp_servers.<name>]`.
    Toml,
}

impl Harness {
    /// The value of `HIRD_HARNESS` for this harness — how the board and the
    /// other agents will tell its sessions apart.
    ///
    /// Both Copilots say `copilot`: the bar hird enforces on review is the
    /// harness, not the front end it was driven from, and one person's editor
    /// and terminal are the same reviewer.
    pub fn harness_name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Copilot | Harness::CopilotCli => "copilot",
        }
    }

    /// What to call this harness in a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "Claude Code",
            Harness::Codex => "the Codex CLI",
            Harness::Copilot => "Copilot in VS Code",
            Harness::CopilotCli => "the Copilot CLI",
        }
    }

    /// The one file this harness reads. Project-scoped for the two that have a
    /// documented project scope, so registering does not reach outside the
    /// checkout the human ran it in.
    pub fn config_path(self, cwd: &Path) -> PathBuf {
        match self {
            Harness::ClaudeCode => cwd.join(".mcp.json"),
            Harness::Codex => identity::home().join(".codex").join("config.toml"),
            Harness::Copilot => cwd.join(".vscode").join("mcp.json"),
            Harness::CopilotCli => identity::home().join(".copilot").join("mcp-config.json"),
        }
    }

    fn shape(self) -> Shape {
        match self {
            Harness::ClaudeCode | Harness::CopilotCli => Shape::Json {
                container: "mcpServers",
            },
            Harness::Copilot => Shape::Json {
                container: "servers",
            },
            Harness::Codex => Shape::Toml,
        }
    }

    /// The step this command cannot take for the human.
    pub fn next_step(self) -> &'static str {
        match self {
            Harness::ClaudeCode => {
                "start a session in this directory; `claude mcp list` confirms it"
            }
            Harness::Codex => "restart the Codex CLI; `codex mcp list` confirms it",
            Harness::Copilot => {
                "in VS Code: MCP: List Servers → hird → Start Server, then tick hird in the \
                 agent-mode tools picker"
            }
            Harness::CopilotCli => "restart the Copilot CLI; `/mcp` confirms it",
        }
    }
}

/// One MCP server entry, in whatever shape the harness wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// What the harness will call the server.
    pub name: String,
    /// Absolute path to the hird binary.
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Registration {
    /// The registration this binary should write for `harness`.
    ///
    /// `db` is threaded through as `HIRD_DB` so that `hird register codex --db
    /// /tmp/scratch.db` produces a session pointed at a scratch board rather
    /// than one that silently shares the real one.
    pub fn new(harness: Harness, name: &str, db: Option<&Path>) -> Registration {
        let mut env = BTreeMap::new();
        env.insert(HARNESS_ENV.to_string(), harness.harness_name().to_string());
        if let Some(db) = db {
            env.insert(DB_ENV.to_string(), db.display().to_string());
        }
        Registration {
            name: name.to_string(),
            command: this_binary(),
            args: vec!["mcp".to_string()],
            env,
        }
    }

    /// This entry as JSON, in the dialect `harness` expects.
    fn as_json(&self, harness: Harness) -> Json {
        let mut entry = Map::new();
        // VS Code and the Copilot CLI both discriminate on a `type`, and
        // disagree about what to call a subprocess speaking stdio.
        match harness {
            Harness::Copilot => {
                entry.insert("type".into(), json!("stdio"));
            }
            Harness::CopilotCli => {
                entry.insert("type".into(), json!("local"));
            }
            _ => {}
        }
        entry.insert("command".into(), json!(self.command));
        entry.insert("args".into(), json!(self.args));
        entry.insert("env".into(), json!(self.env));
        if harness == Harness::CopilotCli {
            // Without this the CLI registers the server and exposes none of it.
            entry.insert("tools".into(), json!(["*"]));
        }
        Json::Object(entry)
    }

    /// This entry as TOML, rendered rather than serialized so `env` stays an
    /// inline table. A block appended to somebody's `config.toml` must not
    /// leave a bare `[mcp_servers.hird.env]` header as the last thing in the
    /// file, or the next key they add lands inside it.
    fn as_toml_block(&self) -> String {
        let env: Vec<String> = self
            .env
            .iter()
            .map(|(k, v)| format!("{k} = {}", toml_str(v)))
            .collect();
        let args: Vec<String> = self.args.iter().map(|a| toml_str(a)).collect();
        format!(
            "[mcp_servers.{}]\ncommand = {}\nargs = [{}]\nenv = {{ {} }}\n",
            self.name,
            toml_str(&self.command),
            args.join(", "),
            env.join(", ")
        )
    }

    /// The same entry as a value, for comparing against what is already filed.
    fn as_toml_value(&self) -> toml::Value {
        let mut table = toml::map::Map::new();
        table.insert("command".into(), toml::Value::String(self.command.clone()));
        table.insert(
            "args".into(),
            toml::Value::Array(
                self.args
                    .iter()
                    .map(|a| toml::Value::String(a.clone()))
                    .collect(),
            ),
        );
        let mut env = toml::map::Map::new();
        for (k, v) in &self.env {
            env.insert(k.clone(), toml::Value::String(v.clone()));
        }
        table.insert("env".into(), toml::Value::Table(env));
        toml::Value::Table(table)
    }

    /// What would be written, for `--print`.
    pub fn render(&self, harness: Harness) -> String {
        match harness.shape() {
            Shape::Json { container } => {
                let mut servers = Map::new();
                servers.insert(self.name.clone(), self.as_json(harness));
                let mut doc = Map::new();
                doc.insert(container.to_string(), Json::Object(servers));
                let mut text = serde_json::to_string_pretty(&Json::Object(doc))
                    .unwrap_or_else(|_| "{}".to_string());
                text.push('\n');
                text
            }
            Shape::Toml => self.as_toml_block(),
        }
    }
}

/// A TOML string literal, escaped.
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The absolute path of the running binary, or the bare name if the OS will not
/// say — which is the one case where the human has to fix `command` by hand.
fn this_binary() -> String {
    std::env::current_exe()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "hird".to_string())
}

/// What writing the registration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The config file did not exist and now does.
    Created,
    /// The file existed and gained this server.
    Added,
    /// The file already had this server, differently, and `--force` replaced it.
    Replaced,
    /// The file already said exactly this. Registering twice is not an error.
    Unchanged,
}

impl Outcome {
    /// What happened, named by the server it happened to.
    pub fn describe(self, name: &str, path: &Path) -> String {
        let path = path.display();
        match self {
            Outcome::Created => format!("registered {name} in {path}"),
            Outcome::Added => format!("added {name} to {path}"),
            Outcome::Replaced => format!("replaced {name} in {path}"),
            Outcome::Unchanged => format!("{name} was already registered in {path}"),
        }
    }
}

/// Write `reg` into the file `harness` reads, and say what that did.
///
/// An existing entry that differs is a refusal rather than an overwrite: the
/// most likely reason for one is a registration somebody tuned by hand, and
/// this command knows nothing about why.
pub fn apply(
    harness: Harness,
    reg: &Registration,
    cwd: &Path,
    force: bool,
) -> anyhow::Result<(PathBuf, Outcome)> {
    let path = harness.config_path(cwd);
    let outcome = match harness.shape() {
        Shape::Json { container } => write_json(&path, container, reg, harness, force)?,
        Shape::Toml => write_toml(&path, reg, force)?,
    };
    Ok((path, outcome))
}

fn write_json(
    path: &Path,
    container: &str,
    reg: &Registration,
    harness: Harness,
    force: bool,
) -> anyhow::Result<Outcome> {
    let entry = reg.as_json(harness);
    let (mut doc, existed) = match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => (Map::new(), true),
        Ok(raw) => {
            let parsed: Json = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!(
                    "{} is not JSON this can edit ({e}). Comments and trailing commas are \
                     legal in some of these files and not in any JSON parser — add the \
                     block by hand, or see `hird register {} --print`",
                    path.display(),
                    harness
                        .to_possible_value()
                        .expect("named variant")
                        .get_name()
                )
            })?;
            match parsed {
                Json::Object(map) => (map, true),
                _ => anyhow::bail!("{} is JSON, but not an object", path.display()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Map::new(), false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let servers = match doc
        .entry(container.to_string())
        .or_insert_with(|| json!({}))
    {
        Json::Object(map) => map,
        _ => anyhow::bail!(
            "{} has a `{container}` that is not an object; leaving it alone",
            path.display()
        ),
    };

    let outcome = match servers.get(&reg.name) {
        Some(current) if *current == entry => return Ok(Outcome::Unchanged),
        Some(current) if !force => {
            anyhow::bail!(
                "{} already registers {:?}, differently:\n\n{}\n\nre-run with --force to \
                 replace it",
                path.display(),
                reg.name,
                serde_json::to_string_pretty(current).unwrap_or_default()
            )
        }
        Some(_) => Outcome::Replaced,
        None if existed => Outcome::Added,
        None => Outcome::Created,
    };
    servers.insert(reg.name.clone(), entry);

    let mut text = serde_json::to_string_pretty(&Json::Object(doc))?;
    text.push('\n');
    write_file(path, &text)?;
    Ok(outcome)
}

fn write_toml(path: &Path, reg: &Registration, force: bool) -> anyhow::Result<Outcome> {
    let (raw, existed) = match std::fs::read_to_string(path) {
        Ok(raw) => (raw, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    // Parsed for the comparison, spliced as text for the write: a round-trip
    // through the serializer would drop every comment in somebody's config.
    let parsed: toml::Table = raw
        .parse()
        .with_context(|| format!("{} is not valid TOML; leaving it alone", path.display()))?;
    let current = parsed
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .and_then(|servers| servers.get(&reg.name));

    let outcome = match current {
        Some(value) if *value == reg.as_toml_value() => return Ok(Outcome::Unchanged),
        Some(value) if !force => anyhow::bail!(
            "{} already registers {:?}, differently:\n\n{}\nre-run with --force to replace it",
            path.display(),
            reg.name,
            toml::to_string(value).unwrap_or_default()
        ),
        Some(_) => Outcome::Replaced,
        None if existed => Outcome::Added,
        None => Outcome::Created,
    };

    let block = reg.as_toml_block();
    let text = match span_of(&raw, &reg.name) {
        Some((from, to)) => {
            let mut out = String::with_capacity(raw.len() + block.len());
            out.push_str(&raw[..from]);
            out.push_str(&block);
            out.push_str(&raw[to..]);
            out
        }
        None => {
            let mut out = raw;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&block);
            out
        }
    };
    write_file(path, &text)?;
    Ok(outcome)
}

/// Byte range of the existing `[mcp_servers.<name>]` block, header to the line
/// before the next top-level header.
fn span_of(raw: &str, name: &str) -> Option<(usize, usize)> {
    let quoted = format!("[mcp_servers.\"{name}\"]");
    let bare = format!("[mcp_servers.{name}]");
    let mut offset = 0usize;
    let mut start = None;
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim();
        match start {
            None => {
                if trimmed == bare || trimmed == quoted {
                    start = Some(offset);
                }
            }
            Some(from) => {
                if trimmed.starts_with('[') {
                    return Some((from, offset));
                }
            }
        }
        offset += line.len();
    }
    start.map(|from| (from, raw.len()))
}

fn write_file(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(name: &str) -> Registration {
        Registration {
            name: name.to_string(),
            command: "/opt/bin/hird".to_string(),
            args: vec!["mcp".to_string()],
            env: BTreeMap::from([(HARNESS_ENV.to_string(), "codex".to_string())]),
        }
    }

    #[test]
    fn every_harness_names_itself_for_the_board() {
        assert_eq!(Harness::ClaudeCode.harness_name(), "claude-code");
        assert_eq!(Harness::Codex.harness_name(), "codex");
        // One person's editor and terminal are one reviewer, so the recusal
        // bar has to see them as one harness.
        assert_eq!(Harness::Copilot.harness_name(), "copilot");
        assert_eq!(Harness::CopilotCli.harness_name(), "copilot");
    }

    #[test]
    fn the_registration_names_an_absolute_command() {
        let reg = Registration::new(Harness::Codex, "hird", None);
        // Whatever the test binary is, it is not the bare word a hand-written
        // config would have used.
        assert!(reg.command.starts_with('/'), "{}", reg.command);
        assert_eq!(reg.args, vec!["mcp".to_string()]);
        assert_eq!(reg.env[HARNESS_ENV], "codex");
        assert!(!reg.env.contains_key(DB_ENV));
    }

    #[test]
    fn a_scratch_database_rides_along_in_the_environment() {
        let reg = Registration::new(Harness::Codex, "hird-scratch", Some(Path::new("/tmp/s.db")));
        assert_eq!(reg.env[DB_ENV], "/tmp/s.db");
    }

    #[test]
    fn each_json_harness_gets_its_own_dialect() {
        let vscode = reg("hird").as_json(Harness::Copilot);
        assert_eq!(vscode["type"], json!("stdio"));
        assert!(vscode.get("tools").is_none());

        let cli = reg("hird").as_json(Harness::CopilotCli);
        assert_eq!(cli["type"], json!("local"));
        // Registered and exposing nothing is the same as not registered.
        assert_eq!(cli["tools"], json!(["*"]));

        // Claude Code discriminates on nothing: a `type` it does not know is
        // a parse error, not a hint.
        let claude = reg("hird").as_json(Harness::ClaudeCode);
        assert!(claude.get("type").is_none());
    }

    #[test]
    fn the_toml_block_keeps_env_inline() {
        let block = reg("hird").as_toml_block();
        assert_eq!(
            block,
            "[mcp_servers.hird]\ncommand = \"/opt/bin/hird\"\nargs = [\"mcp\"]\n\
             env = { HIRD_HARNESS = \"codex\" }\n"
        );
        // Which has to parse back to what the comparison will look for.
        let parsed: toml::Table = block.parse().unwrap();
        assert_eq!(parsed["mcp_servers"]["hird"], reg("hird").as_toml_value());
    }

    #[test]
    fn a_missing_file_is_created_with_only_our_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (path, outcome) = apply(Harness::Copilot, &reg("hird"), dir.path(), false).unwrap();
        assert_eq!(outcome, Outcome::Created);
        assert_eq!(path, dir.path().join(".vscode").join("mcp.json"));
        let written: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["servers"]["hird"]["command"],
            json!("/opt/bin/hird")
        );
    }

    #[test]
    fn registering_twice_changes_nothing_and_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        apply(Harness::ClaudeCode, &reg("hird"), dir.path(), false).unwrap();
        let (_, again) = apply(Harness::ClaudeCode, &reg("hird"), dir.path(), false).unwrap();
        assert_eq!(again, Outcome::Unchanged);
    }

    #[test]
    fn another_server_in_the_file_survives_registration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"thing"}},"unrelated":true}"#,
        )
        .unwrap();

        let (_, outcome) = apply(Harness::ClaudeCode, &reg("hird"), dir.path(), false).unwrap();
        assert_eq!(outcome, Outcome::Added);
        let written: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other"]["command"], json!("thing"));
        assert_eq!(written["unrelated"], json!(true));
        assert!(written["mcpServers"]["hird"].is_object());
    }

    #[test]
    fn a_hand_tuned_entry_is_refused_until_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"hird":{"command":"/somewhere/else/hird"}}}"#,
        )
        .unwrap();

        let err = apply(Harness::ClaudeCode, &reg("hird"), dir.path(), false).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("--force"), "{message}");
        // And the refusal really did leave the file alone.
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("/somewhere/else/hird"));

        let (_, forced) = apply(Harness::ClaudeCode, &reg("hird"), dir.path(), true).unwrap();
        assert_eq!(forced, Outcome::Replaced);
    }

    #[test]
    fn json_that_cannot_be_parsed_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(&path, "{ // a comment VS Code would accept\n}").unwrap();

        let err = apply(Harness::ClaudeCode, &reg("hird"), dir.path(), false).unwrap_err();
        assert!(format!("{err:#}").contains("--print"), "{err:#}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("comment"));
    }

    #[test]
    fn a_toml_block_is_appended_and_the_comments_around_it_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# my settings\nmodel = \"o3\"\n").unwrap();

        let outcome = write_toml(&path, &reg("hird"), false).unwrap();
        assert_eq!(outcome, Outcome::Added);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.starts_with("# my settings\nmodel = \"o3\"\n"),
            "{written}"
        );
        assert!(written.contains("[mcp_servers.hird]"), "{written}");

        // Idempotent through the text path too.
        assert_eq!(
            write_toml(&path, &reg("hird"), false).unwrap(),
            Outcome::Unchanged
        );
    }

    #[test]
    fn forcing_a_toml_entry_replaces_only_that_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[mcp_servers.hird]\ncommand = \"hird\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();

        assert_eq!(
            write_toml(&path, &reg("hird"), true).unwrap(),
            Outcome::Replaced
        );
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"]["hird"]["command"],
            toml::Value::String("/opt/bin/hird".into())
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"],
            toml::Value::String("x".into())
        );
    }

    #[test]
    fn broken_toml_is_left_exactly_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml\n").unwrap();

        assert!(write_toml(&path, &reg("hird"), false).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is not = = toml\n"
        );
    }

    #[test]
    fn the_printed_form_is_what_would_be_written() {
        let printed = reg("hird").render(Harness::Copilot);
        let parsed: Json = serde_json::from_str(&printed).unwrap();
        assert_eq!(
            parsed["servers"]["hird"],
            reg("hird").as_json(Harness::Copilot)
        );
        assert!(printed.ends_with("\n"));
    }
}
