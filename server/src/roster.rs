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

use std::collections::BTreeMap;
use std::path::Path;

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
    /// Capability labels this token may claim against.
    pub capabilities: Vec<String>,
}

/// The file as written.
#[derive(Debug, Deserialize)]
struct RosterFile {
    /// Project for workers that do not name their own.
    project: Option<String>,
    #[serde(default, rename = "worker")]
    workers: Vec<WorkerFile>,
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
    /// work: a duplicated token, a missing project, an empty credential.
    pub fn parse(text: &str) -> anyhow::Result<Roster> {
        let file: RosterFile = toml::from_str(text)?;
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
            let entry = Worker {
                identity: identity.clone(),
                harness: worker.harness.map(|h| h.trim().to_string()),
                project,
                capabilities: worker.capabilities,
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
}
