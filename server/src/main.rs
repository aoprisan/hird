//! `hird-server` — the central queue.
//!
//! The `hird` binary is a local queue: one process per harness session, stdio,
//! no network, and a working tree it can read. This one is the other thing —
//! a process on a host two people reach from two machines, speaking MCP over
//! HTTP, with the queue's SQLite file underneath it.
//!
//! It is a separate binary rather than a flag on the first because a server is
//! a different kind of program. It carries a web stack the local binary must
//! not: `cargo tree -p hird` names no HTTP dependency at all, and that stays
//! true however large this one grows.
//!
//! ## How a connection becomes a session
//!
//! [`hird::mcp::Session`] holds who a connection is; the question a server has
//! to answer is who to believe. The far end of a socket can claim anything, so
//! nothing it says about its identity is taken: a request carries a bearer
//! token, the [`roster`] maps that token to a person, and the session is built
//! from the roster's entry. A client may still name its own harness — §29
//! permits that and never permits the other.
//!
//! Each roster entry gets its own MCP service, so identity is fixed when the
//! service is built rather than re-derived per call. Dispatch is a map lookup
//! on the token, and a request without a known one is refused before any of
//! the queue is touched.

mod roster;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use clap::Parser;
use hird::config::{self, Config};
use hird::identity::AgentId;
use hird::mcp::{HirdMcp, Session, Shared};
use hird::Db;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use tower_service::Service;

use crate::roster::Roster;

/// One MCP endpoint, already bound to one person.
type Endpoint = StreamableHttpService<HirdMcp, LocalSessionManager>;

#[derive(Debug, Parser)]
#[command(
    name = "hird-server",
    version,
    about = "Serve a hird queue over HTTP to harnesses on other machines.",
    long_about = "The central queue. Harnesses on other machines reach it over MCP \
                  streamable HTTP, each authenticated by a bearer token that the roster \
                  maps to a person. Every claim is still atomic, because there is still \
                  one SQLite file.\n\n\
                  Terminate TLS in front of this: it speaks plain HTTP, and a bearer \
                  token on an unencrypted connection is a token you have given away."
)]
struct Cli {
    /// The roster file: which tokens may connect, and as whom.
    #[arg(long, value_name = "PATH")]
    roster: PathBuf,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1", value_name = "ADDR")]
    bind: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 7474)]
    port: u16,
    /// Database file to use. Overrides HIRD_DB and the default location.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,
}

/// Everything the HTTP layer needs: one endpoint per token.
#[derive(Clone)]
struct Fleet {
    endpoints: Arc<BTreeMap<String, Endpoint>>,
}

impl Fleet {
    /// Build one MCP endpoint per roster entry, all sharing one open database.
    ///
    /// The identity is closed over here rather than read per request, so a
    /// session cannot change who it is halfway through — the same invariant
    /// the local binary gets from having one process per session.
    fn assemble(roster: &Roster, shared: Arc<Shared>, config: &Config) -> anyhow::Result<Fleet> {
        let mut endpoints = BTreeMap::new();
        for (token, worker) in roster.entries() {
            let shared = shared.clone();
            let config = config.clone();
            let worker = worker.clone();
            let service = StreamableHttpService::new(
                move || {
                    let agent = AgentId::fresh(worker.harness.clone().unwrap_or_default())
                        .acting_for(worker.identity.clone());
                    let session = Session::new(
                        agent,
                        worker.project.clone(),
                        worker.capabilities.clone(),
                        &config,
                    )
                    .map_err(std::io::Error::other)?;
                    Ok(HirdMcp::connect(shared.clone(), session))
                },
                Arc::new(LocalSessionManager::default()),
                Default::default(),
            );
            endpoints.insert(token.clone(), service);
        }
        Ok(Fleet {
            endpoints: Arc::new(endpoints),
        })
    }
}

/// The bearer token a request presents, if it presents one properly.
fn bearer(request: &Request<Body>) -> Option<&str> {
    request
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// Route a request to the endpoint its token names.
///
/// A request with no token, or one the roster does not know, is refused here —
/// before the database is opened, before a session exists, and with the same
/// answer either way, so the response does not say which tokens are real.
async fn route(State(fleet): State<Fleet>, request: Request<Body>) -> Response {
    let Some(endpoint) = bearer(&request).and_then(|token| fleet.endpoints.get(token)) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                "Bearer realm=\"hird\"",
            )],
            "this queue is not open to unnamed callers\n",
        )
            .into_response();
    };
    let mut endpoint = endpoint.clone();
    match endpoint.call(request).await {
        Ok(response) => response.into_response(),
        Err(never) => match never {},
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let roster = Roster::load(&cli.roster)?;
    let config = Config::load_default()?;
    let db_path = config::resolve_db_path(cli.db.as_deref());
    let shared = Arc::new(Shared::new(Db::open(&db_path)?, config.clone()));
    let fleet = Fleet::assemble(&roster, shared, &config)?;

    let app = axum::Router::new()
        .fallback(axum::routing::any(route))
        .with_state(fleet);
    let listener = tokio::net::TcpListener::bind((cli.bind.as_str(), cli.port)).await?;
    let addr = listener.local_addr()?;
    println!(
        "hird-server on http://{addr}  ({} in the roster)",
        roster.len()
    );
    println!("database: {}", db_path.display());
    if config.witness {
        println!(
            "note: the witness is on, and it reads this host's working tree — \
             nobody's checkout. Set `witness = false` for a shared queue."
        );
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
