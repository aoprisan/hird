//! Who may connect, and as whom.
//!
//! The local binary reads its identity from the environment it was started in,
//! because that environment belongs to the human running the harness. A server
//! has no such luxury: the environment is the server's, and the far end of a
//! socket can claim anything. The roster is the operator's answer — a file
//! naming each token that may connect and the session it connects as.
//!
//! It is deliberately a file rather than a table in the queue's own database.
//! The queue records what happened; who is allowed to make things happen is a
//! different kind of fact, and keeping it outside means a compromised queue
//! cannot grant access to itself.
//!
//! Capability labels are per worker, and a worker may inherit a set from the
//! harness it pins: `[harness.opencode]` says what every pinned OpenCode
//! worker starts with, and the worker's own `capabilities` add to it. Only a
//! *pinned* harness reaches that table. The name a client gives itself never
//! does, because a label is something a human grants (§25) and a client's name
//! for itself is the one thing the design lets a client be wrong about (§29).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hird::{capability, identity};
use serde::Deserialize;

/// A parsed roster: every token that may connect, and what it connects as.
#[derive(Debug, Clone)]
pub struct Roster {
    workers: BTreeMap<String, Worker>,
}

/// One entry: a credential, and the session it resolves to.
#[derive(Debug, Clone)]
pub struct Worker {
    /// The person this token acts for. Recorded on everything it does.
    pub identity: String,
    /// The harness, when the operator wants to pin it. Left unset, the client
    /// names itself — which §29 permits for a harness and never for a person.
    pub harness: Option<String>,
    /// The project this token's calls scope to. Required: the server's working
    /// directory is the server's, and a caller's checkout is somewhere else.
    pub project: String,
    /// Capability labels this token may claim against: the pinned harness's
    /// defaults and the worker's own, normalized, sorted and deduplicated.
    pub capabilities: Vec<String>,
}

/// The file as written.
#[derive(Debug, Deserialize)]
struct RosterFile {
    /// Project for workers that do not name their own.
    project: Option<String>,
    /// Defaults per harness type, keyed by the name a worker pins.
    #[serde(default, rename = "harness")]
    harnesses: BTreeMap<String, HarnessFile>,
    #[serde(default, rename = "worker")]
    workers: Vec<WorkerFile>,
}

/// What every worker pinned to one harness type starts with.
#[derive(Debug, Deserialize)]
struct HarnessFile {
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WorkerFile {
    token: String,
    identity: String,
    harness: Option<String>,
    project: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

impl Roster {
    /// Read and validate a roster file.
    pub fn load(path: &Path) -> anyhow::Result<Roster> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read roster {}: {e}", path.display()))?;
        Roster::parse(&text)
    }

    /// Parse a roster, refusing the mistakes that would quietly mis-attribute
    /// work: a duplicated token, a missing project, an empty credential, a
    /// capability label the session would refuse, a harness table nobody pins.
    pub fn parse(text: &str) -> anyhow::Result<Roster> {
        let file: RosterFile = toml::from_str(text)?;
        let mut defaults = BTreeMap::new();
        for (raw, harness) in file.harnesses {
            let Some(name) = identity::harness_name(&raw) else {
                anyhow::bail!("a [harness] table is named {raw:?}, which no worker could pin");
            };
            let capabilities = capability::normalize_all(&harness.capabilities)
                .map_err(|e| anyhow::anyhow!("[harness.{name}]: {e}"))?;
            if defaults.insert(name.clone(), capabilities).is_some() {
                anyhow::bail!(
                    "two [harness] tables both name {name:?} once the name is normalized;                      keep one"
                );
            }
        }
        let mut pinned = BTreeSet::new();
        let mut workers = BTreeMap::new();
        for worker in file.workers {
            let token = worker.token.trim().to_string();
            if token.is_empty() {
                anyhow::bail!("worker {:?} has an empty token", worker.identity);
            }
            if token.len() < MIN_TOKEN {
                anyhow::bail!(
                    "the token for {:?} is {} characters; use at least {MIN_TOKEN} \
                     — this is the only thing standing between a stranger and the queue",
                    worker.identity,
                    token.len()
                );
            }
            let identity = worker.identity.trim().to_string();
            if identity.is_empty() {
                anyhow::bail!("a worker entry has no identity");
            }
            let project = worker
                .project
                .or_else(|| file.project.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "worker {identity:?} has no project, and the roster sets no default; \
                         a server cannot guess one from its own working directory"
                    )
                })?;
            let harness = match worker.harness {
                Some(raw) => match identity::harness_name(&raw) {
                    Some(name) => Some(name),
                    None => anyhow::bail!(
                        "worker {identity:?} pins harness {raw:?}, which is not a usable name"
                    ),
                },
                None => None,
            };
            let inherited = harness
                .as_ref()
                .and_then(|name| defaults.get(name))
                .map(Vec::as_slice)
                .unwrap_or_default();
            if let Some(name) = &harness {
                pinned.insert(name.clone());
            }
            let capabilities =
                capability::normalize_all(inherited.iter().chain(worker.capabilities.iter()))
                    .map_err(|e| anyhow::anyhow!("worker {identity:?}: {e}"))?;
            let entry = Worker {
                identity: identity.clone(),
                harness,
                project,
                capabilities,
            };
            if workers.insert(token, entry).is_some() {
                anyhow::bail!(
                    "two workers share one token; the second would act as {identity:?} \
                     and the queue could not tell them apart"
                );
            }
        }
        if workers.is_empty() {
            anyhow::bail!("the roster names no workers, so nobody could connect");
        }
        if let Some(unused) = defaults.keys().find(|name| !pinned.contains(*name)) {
            anyhow::bail!(
                "[harness.{unused}] grants capabilities, but no worker pins harness {unused:?}; \
                 a client naming itself {unused:?} would not inherit them, so either pin a \
                 worker to it or remove the table"
            );
        }
        Ok(Roster { workers })
    }

    /// The worker a bearer token resolves to, if any.
    ///
    /// The server does not call this at request time — it builds one endpoint
    /// per token up front and dispatches by map lookup — but resolution is
    /// what a roster *is*, and it is the thing worth asserting about.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn worker(&self, token: &str) -> Option<&Worker> {
        self.workers.get(token)
    }

    /// Every (token, worker) pair, for building one service per identity.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Worker)> {
        self.workers.iter()
    }

    /// How many workers may connect. Never zero: [`Roster::parse`] refuses a
    /// roster nobody could use.
    pub fn len(&self) -> usize {
        self.workers.len()
    }
}

/// Short enough to type, long enough not to guess. A roster is the whole
/// authorization story, so a token that fits in a sticky note is refused.
const MIN_TOKEN: usize = 24;

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
project = "/srv/acme"
[[worker]]
token = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
identity = "ana"
capabilities = ["browser"]
[[worker]]
token = "bbbbbbbbbbbbbbbbbbbbbbbbbbbb"
identity = "ben"
harness = "codex"
project = "/srv/other"
"#;

    #[test]
    fn a_roster_resolves_a_token_to_a_person() {
        let roster = Roster::parse(OK).unwrap();
        assert_eq!(roster.len(), 2);
        let ana = roster.worker("aaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(ana.identity, "ana");
        assert_eq!(ana.project, "/srv/acme", "the default project applies");
        assert_eq!(ana.capabilities, ["browser"]);
        assert_eq!(ana.harness, None, "unpinned, so the client names itself");
        let ben = roster.worker("bbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_eq!(ben.project, "/srv/other", "a worker may override it");
        assert_eq!(ben.harness.as_deref(), Some("codex"));
    }

    #[test]
    fn an_unknown_token_resolves_to_nobody() {
        assert!(Roster::parse(OK).unwrap().worker("nope").is_none());
    }

    #[test]
    fn two_workers_may_not_share_a_token() {
        let text = OK.replace(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let err = Roster::parse(&text).unwrap_err().to_string();
        assert!(err.contains("share one token"), "{err}");
    }

    #[test]
    fn a_short_token_is_refused_rather_than_accepted_quietly() {
        let text = OK.replace("aaaaaaaaaaaaaaaaaaaaaaaaaaaa", "short");
        let err = Roster::parse(&text).unwrap_err().to_string();
        assert!(err.contains("at least"), "{err}");
    }

    #[test]
    fn a_worker_without_a_project_is_refused() {
        let text = r#"
[[worker]]
token = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
identity = "ana"
"#;
        let err = Roster::parse(text).unwrap_err().to_string();
        assert!(err.contains("no project"), "{err}");
    }

    #[test]
    fn an_empty_roster_is_refused() {
        let err = Roster::parse("").unwrap_err().to_string();
        assert!(err.contains("names no workers"), "{err}");
    }

    #[test]
    fn capability_labels_are_normalized_at_parse_time() {
        let text = OK.replace(r#"["browser"]"#, r#"[" Browser ", "linux", "browser"]"#);
        let roster = Roster::parse(&text).unwrap();
        let ana = roster.worker("aaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(ana.capabilities, ["browser", "linux"]);
    }

    /// A label the session would refuse is refused when the server starts,
    /// not when that one worker happens to connect.
    #[test]
    fn a_bad_capability_label_is_refused_at_parse_time() {
        let text = OK.replace(r#"["browser"]"#, r#"["has space"]"#);
        let err = Roster::parse(&text).unwrap_err().to_string();
        assert!(err.contains("\"ana\""), "{err}");
        assert!(err.contains("unsupported character"), "{err}");
    }

    const WITH_DEFAULTS: &str = r#"
project = "/srv/acme"

[harness.opencode]
capabilities = ["browser", "linux"]

[harness.pi]
capabilities = ["linux"]

[[worker]]
token = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa"
identity = "ana"
harness = "opencode"
capabilities = ["macos"]

[[worker]]
token = "bbbbbbbbbbbbbbbbbbbbbbbbbbbb"
identity = "ana"
harness = "pi"

[[worker]]
token = "cccccccccccccccccccccccccccc"
identity = "ben"
capabilities = ["gpu"]
"#;

    #[test]
    fn a_pinned_worker_inherits_its_harness_defaults_and_keeps_its_own() {
        let roster = Roster::parse(WITH_DEFAULTS).unwrap();
        let opencode = roster.worker("aaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(opencode.harness.as_deref(), Some("opencode"));
        assert_eq!(opencode.capabilities, ["browser", "linux", "macos"]);
        let pi = roster.worker("bbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_eq!(pi.capabilities, ["linux"], "defaults alone, nothing added");
    }

    /// The whole safety argument in one rule: the table is reached by the
    /// harness the operator pinned, never by the name a client gives itself.
    #[test]
    fn an_unpinned_worker_inherits_nothing_whatever_a_client_says() {
        let roster = Roster::parse(WITH_DEFAULTS).unwrap();
        let ben = roster.worker("cccccccccccccccccccccccccccc").unwrap();
        assert_eq!(ben.harness, None);
        assert_eq!(ben.capabilities, ["gpu"]);
    }

    /// The pin and the table key are matched the way the identity records
    /// them, so a stray space or separator does not make the two miss.
    #[test]
    fn a_pin_and_a_table_match_after_the_identity_normalization() {
        let text = WITH_DEFAULTS.replace(r#"harness = "opencode""#, r#"harness = " open:code ""#);
        let roster = Roster::parse(&text).unwrap();
        let ana = roster.worker("aaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert_eq!(ana.harness.as_deref(), Some("opencode"));
        assert_eq!(ana.capabilities, ["browser", "linux", "macos"]);
    }

    #[test]
    fn a_harness_table_nobody_pins_is_refused_rather_than_ignored() {
        let text = WITH_DEFAULTS.replace(r#"harness = "pi""#, "");
        let err = Roster::parse(&text).unwrap_err().to_string();
        assert!(err.contains("[harness.pi]"), "{err}");
        assert!(err.contains("no worker pins"), "{err}");
    }

    #[test]
    fn a_bad_label_in_a_harness_table_names_the_table() {
        let text = WITH_DEFAULTS.replace(r#"["linux"]"#, r#"["no,commas"]"#);
        let err = Roster::parse(&text).unwrap_err().to_string();
        assert!(err.contains("[harness.pi]"), "{err}");
    }

    /// The example every operator starts from must be one the parser accepts.
    #[test]
    fn the_shipped_example_roster_parses() {
        let text = include_str!("../../examples/roster.toml");
        let roster = Roster::parse(text).unwrap();
        let ana_opencode = roster
            .worker("REPLACE-ME-with-a-third-32-random-bytes")
            .unwrap();
        assert_eq!(ana_opencode.harness.as_deref(), Some("opencode"));
        assert_eq!(ana_opencode.capabilities, ["browser", "linux"]);
    }

    #[test]
    fn a_pin_that_sanitizes_to_nothing_is_refused() {
        let text = OK.replace(r#"harness = "codex""#, r#"harness = " : ""#);
        let err = Roster::parse(&text).unwrap_err().to_string();
        assert!(err.contains("not a usable name"), "{err}");
    }
}
