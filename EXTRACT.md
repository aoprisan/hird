# Assessment: extracting `hird-server` into its own repository

*Written against `main` at `8eddb81` (`hird` 0.2.2 + the v3.0–v3.2 work of #31).
This is an assessment, not a change — nothing here is done. `REMOTE.md` decided
what a remote hird should be; this decides where its code should live.*

## The question

> Is it possible to extract the hird changes for running as a remote server
> into the `hird-server` repo?

**Yes for the binary, no for the changes — and the distinction is the whole
answer.** The `hird-server` crate moves out cleanly and was verified doing so.
The library work that makes a remote server *possible* is not part of it and
cannot follow: it is the `hird` crate.

## What "the remote server changes" actually are

The remote server arrived in one commit, `33d2b7e` (#31), which is four
distinct pieces of work wearing one coat. Splitting them is the first thing an
extraction has to do, because they do not all live in the same place.

| | The work | Where it lives | Can it move? |
|---|---|---|---|
| **§29** | The principal — `HIRD_IDENTITY`, `<principal>/<harness>:<session>`, `record --by person` | `src/identity.rs`, `model.rs`, `repo/verdict.rs`, `cli.rs`, `register.rs` | **No** — it is the actor string every one of the twelve tools writes |
| **§30** | The connection — `HirdMcp` split into `Shared` + `Session`, `Attributes`, `hird mcp --identity/--harness/--project/--capability` | `src/mcp.rs` (+306), `cli.rs` | **No** — it is the MCP server's own shape |
| **§31** | The second binary — `hird-server`, the roster, bearer dispatch | `server/` (437 lines, 3 files) | **Yes** — cleanly |
| — | Docs, `examples/roster.toml`, the CI dependency guard | `REMOTE.md`, `DESIGN.md`, `README.md`, `ROADMAP.md`, `docs/server.html`, `AGENTS.md`, `ci.yml` | **Split** — see below |

§29 and §30 are the load-bearing half. The commit message for §30 says it
plainly: one process per session was *"the single assumption that makes a shared
queue impossible."* Removing it is a change to `hird`, not an addition beside
it. A `hird-server` repository that held §29 and §30 would not be a server
repository; it would be a fork of `hird`.

So the honest form of the question is: **can §31 live in a separate repository
and depend on §29 and §30 as a library?**

## The finding: §31 extracts with zero source changes

Verified, not estimated. `server/` was copied to a scratch directory, made
standalone, and its `hird = { path = ".." }` swapped for a git dependency
pinned to `main`. Nothing else was touched — not one line of Rust.

```
cargo build                                  ✅ Finished in 56s
cargo test                                   ✅ 7 passed; 0 failed
cargo clippy --all-targets -- -D warnings    ✅ clean
cargo fmt --check                            ✅ clean
```

And end to end against the extracted binary, with a two-worker roster:

```
$ hird-server --roster roster.toml --port 7788 --db /tmp/smoke.db
hird-server on http://127.0.0.1:7788  (2 in the roster)

no token       → 401  www-authenticate: Bearer realm="hird"
unknown token  → 401  (same body — it does not say which tokens are real)
rostered token → MCP initialize, protocolVersion 2026-07-28, tools listed
```

It works because the coupling is genuinely thin. The server reaches into
`hird` for exactly **eight items across four modules**, all already `pub`:

```rust
use hird::config::{self, Config};   // Config, load_default, resolve_db_path
use hird::identity::AgentId;        // fresh, acting_for
use hird::mcp::{HirdMcp, Session, Shared};
use hird::Db;                       // open
```

No private items, no `pub(crate)` reach-arounds, no test-only paths. §30 did
the separation work already; the crate boundary just makes it visible.

## The one real blocker: the published crate predates the work

`hird` is on crates.io at 0.1.0 and 0.2.2. The published 0.2.2 tarball carries
`.cargo_vcs_info.json` naming commit `1a03124` — **the commit before `33d2b7e`.**
It contains no `Shared`, no `Session`, no `Attributes`, no `IDENTITY_ENV`.

Building the extracted crate against `hird = "0.2.2"` from crates.io fails,
as it must:

```
error[E0432]: unresolved imports `hird::mcp::Session`, `hird::mcp::Shared`
error[E0599]: no function or associated item named `fresh` found for `AgentId`
error[E0599]: no function or associated item named `connect` found for `HirdMcp`
```

This is a release-ordering problem, not a design problem, and there are two
ways past it.

**A. Git dependency, pinned to a revision.** Works today — this is what was
verified. `hird = { git = "https://github.com/aoprisan/hird", rev = "…" }`.
No release needed. The cost is that a git dependency cannot itself be
published to crates.io, so `hird-server` would stay `publish = false`, which
it already is.

**B. Release `hird` 0.3.0 first, then depend on the version.** The tidy
answer, and the only one that lets `hird-server` be installable with
`cargo install hird-server`. It requires cutting a release that includes
§29–§31, which is owed anyway: `ROADMAP.md` already describes v3.0–v3.2 as
shipped, while the crate version still says 0.2.2 and the published crate is
older still.

**B is the right one**, with A as the bridge while 0.3.0 is prepared.

## What extraction costs

Nothing above is the interesting part. These are:

**1. `hird` acquires a public API contract it does not have today.** Right now
`Shared`, `Session` and `HirdMcp::connect` are `pub` because `lib.rs` makes
every module `pub`, not because anybody promised them. In one workspace, a
breaking change to `Session::new` is caught by the compiler in the same commit
that causes it — that is exactly how §30 was able to reshape `HirdMcp` freely.
Across a repository boundary the same change compiles green in `hird`, ships,
and breaks `hird-server` later, in a different pull request, for whoever
notices first. **This is the strongest argument against extraction**, and it
is not paid for by any of the benefits below.

**2. Coordinated two-repo changes.** `server/Cargo.toml` and `Cargo.toml` both
say `version = "0.2.2"`; the two crates move in lockstep today by construction.
After extraction, every §30-adjacent change becomes: land in `hird`, tag or
push, bump the pin in `hird-server`, land there. Three steps where there was one.

**3. Docs split down a seam that does not exist.** `DESIGN.md` §29 and §30
document the library; §31 documents the server. `README.md` carries one
two-machine recipe that spans both binaries. `REMOTE.md` assesses the whole
question. `docs/server.html` simulates the server but its point is *"there is
one SQLite file underneath all of it"* — a claim about both. Either the docs
duplicate or the reader follows a link between repositories to understand one
mechanism.

**4. The new repository needs its own scaffolding**: `Cargo.lock`, a CI
workflow, a README, and license files — note that `hird` declares
`MIT OR Apache-2.0` in `Cargo.toml` but ships no `LICENSE` files, so that gap
would be inherited rather than found.

## What extraction buys

**1. A release path the server does not have today.** `release.yml` builds and
attaches exactly one binary — `tar -czf … hird`. `hird-server` is never built
for a target, never attached to a release, and is marked `publish = false`.
It is, today, a binary you can only get by cloning the repository and building
the workspace. Its own repository with its own release workflow would fix that
— but so would ten lines in the existing one.

**2. A harder wall than the CI guard.** `ci.yml` fails if `cargo tree -p hird`
names `axum`, `hyper` or `tower-http`. That guard is real and currently passes
(0 matches). Separate repositories make it structural rather than asserted —
though the guard would stay in `hird` and keep working either way, so this is
a smaller win than it sounds.

**3. A smaller `Cargo.lock` for `hird`.** #31 added 24 packages and 281 lines
of lock file for dependencies the local binary never links.

## The recommendation

**Possible: yes, and demonstrably so. Advisable: not yet.**

The extraction is mechanically trivial — 437 lines, eight public items, zero
source edits, verified building and serving. If the goal is to prove it can be
done, it is done; the branch this document is on can carry the copy whenever
wanted.

But the reason to do it is weak and the reason not to is strong. The two things
extraction actually buys — a release path and a harder dependency wall — are
both obtainable inside the workspace, one by extending `release.yml` and the
other already asserted by CI. What it costs is the freedom to reshape
`mcp::Shared` and `mcp::Session` in a single commit, and that is not a
theoretical freedom: §30 reshaped exactly those types in the same commit that
added the server, and §31 exists because it could. Both landed in `33d2b7e`,
which is unreleased — the interface `hird-server` consumes has never survived
a release, because it has never seen one. Freezing it across a repository
boundary now prices in a stability nobody has yet had reason to want.

**Do this instead, in order:**

1. **Release `hird` 0.3.0** to crates.io with §29–§31. The version is owed —
   `ROADMAP.md` describes this work as shipped and the crate still says 0.2.2.
2. **Ship `hird-server` binaries** from `release.yml`, alongside `hird`. This
   is the one concrete gap extraction would close, and it closes without one.
3. **Revisit extraction when the API stops moving** — concretely, when a
   release goes by that changes nothing in `mcp::Shared`, `mcp::Session` or
   `AgentId`. Then the boundary costs nothing, and step 1 will already have made
   `hird = "0.3"` a real dependency `hird-server` can name.

If extraction is wanted regardless, take route A: git dependency pinned to a
revision, `publish = false` retained, `server/` deleted from the workspace and
`Cargo.toml`'s `members` line with it. It builds, tests and serves — that part
is not in question.

## Verification appendix

Every claim above was checked rather than reasoned about:

- Baseline: `cargo build --workspace` on `8eddb81` → `hird` and `hird-server` compile.
- Extraction: `server/` copied standalone, dependency swapped to git at `8eddb81`, no source edits → builds, 7/7 tests, clippy `-D warnings` clean, `fmt --check` clean.
- Runtime: extracted binary served MCP over HTTP; 401 for absent and unknown tokens with an identical body, successful `initialize` for a rostered one.
- crates.io: `hird` 0.2.2 tarball's `.cargo_vcs_info.json` → `1a03124`; building against it fails with three unresolved-item errors.
- Coupling: `hird::config`, `hird::identity`, `hird::mcp`, `hird::Db` — eight items, all `pub`.
- Isolation: `cargo tree -p hird -e normal | grep -E 'axum|hyper|tower-http'` → 0 matches.
