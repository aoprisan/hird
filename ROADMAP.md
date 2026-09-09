# Roadmap

This is the direction of travel, not a schedule. `hird` is local-first,
daemon-free and pull-based, and every line below was chosen to keep it that
way. Items move from *later* to *next* when somebody needs them, not when a
date arrives.

## Where it stands

Everything the README describes is built and tested: the queue with atomic
claiming and leases (v1), swarm coordination — dependencies, self-dispatch,
declared file scopes, `task_split` (v1.1), the witness that reads the working
tree instead of taking agents' word for it (v1.2), plan files (v1.3), memory
footing (v1.4), recusal (v1.5), the MCP 2026-07-28 lifecycle (v1.6), review
verdicts and the sent-back loop (v1.7), footprints (v1.8), the ground a task
builds on (v1.9), the exhibit — kept versions, `hird diff`, `hird salvage`
(v2.0), tenures (v2.1), the dispatch hook (v2.2), the event feed (v2.3),
routed summonses (v2.4), human question gates (v2.5), capability-aware
dispatch (v2.6), the readings — `hird why`, `plan lint`, `replay`,
`mem export` and the question hook (v2.7), the recess — `hird recess` /
`hird resume`, the human standing a queue down without killing anything
(v2.8) — and the picture: `hird graph --json`, the TUI's graph screen, and
`hird web`, the board drawn live in a browser with a scrubber over the trail
(v2.9). The herdr plugin packages the pairing.

`DESIGN.md` records each of those decisions as it was made and stays the
specification. This file is only about what is not built yet.

## What every item below must respect

These are the constraints the design has held through twenty-five sections,
and a roadmap item that breaks one is a different project:

- **Twelve MCP tools, six statuses.** Everything an agent is told without
  asking rides along on calls it already makes. A feature that needs a
  thirteenth tool needs a better design first.
- **Pull, not push.** `task_next` is a tool an agent chooses to call. The
  dispatch hook wakes workers; nothing assigns, routes or schedules. hird has
  no roster and chooses nobody.
- **No daemon, no accounts, and no server an agent depends on.** One binary,
  one SQLite file, one process per session. `hird web` (v2.9) listens on a
  socket, and it is worth being exact about why that is not a breach: it is a
  loopback, read-only viewer with the TUI's posture — it dies with the
  terminal and no agent ever talks to it. The line it draws is between a
  *screen* and a *transport*, and a transport agents reach the queue through
  is still the deferral below.
- **Plans are data.** Nothing may appear in a plan file that is not already
  stored task state — no conditionals, loops, retries or schedules, ever.
- **Reports, not verdicts.** The witness says what moved, not who typed;
  footing says *unverified*, never *false*; the record measures and does not
  steer.

## Next

**The first tagged release.** The release engineering is in place — pushing a
`v*` tag builds static Linux and macOS binaries, attaches them with checksums
to a GitHub release that `scripts/get.sh` installs from, and publishes to
crates.io once the `CARGO_REGISTRY_TOKEN` secret is set. What remains is the
human act: set the secret, tag `v0.1.0`, and confirm `get.sh` and
`cargo install hird` both land a working binary.

**More first-class registrations.** `hird register` knows six harnesses — the
Gemini CLI is in, at the project scope `gemini mcp add` itself defaults to —
and `--print` covers the rest by hand. Every MCP-capable CLI that people
actually run beside those six deserves an entry that writes the right file
with the right absolute path, because the absolute path is the thing
hand-written configs get wrong and the reason `register` exists.

## Later

The two items below — sync and a remote transport — are assessed together in
[REMOTE.md](REMOTE.md), which surveys what could be bought instead of built
and reaches a recommendation: dumb object storage with conditional writes for
the queue, a tunnel for the transport, and no hosted orchestrator at any
price. It also prices two costs this file understates — the `seq` collision
and the witness's per-machine blind spot.

**Multi-machine sync (`hird sync`).** The flagship deferral, and the reason
the event trail is append-only: every mutation in hird already lands as an
event, which is the shape that makes sync tractable — ship the trail, replay
it, and let ULIDs and the CAS semantics sort out the races. Still pull-based,
still no daemon: `hird sync` would be a command you (or a hook) run, moving
events through dumb storage such as S3, not a service that stays up. The
design work that remains is real — two machines can hold two working trees,
so the witness's evidence is per-machine even when the queue is shared — and
it is the reason this is *later* rather than *next*.

**Authorization beyond a file.** The transport shipped: v3.1 (§30) gave the
MCP server per-connection session state, and v3.2 (§31) put an HTTP one behind
it in a second binary, `hird-server`, so the local `hird` carries no web stack
at all. A bearer token in a roster file maps to a person, one endpoint is built
per identity, and two people on two machines share a queue. `README.md` has
both recipes — SSH for harnesses that can spawn it, HTTP for those that cannot.

What a file cannot do is scale past a few people you know: no rotation, no
expiry, no revocation short of an edit and a restart, and no delegation. The
MCP authorization spec is the answer — OAuth 2.1 with the server as a resource
server only, RFC 9728 discovery, RFC 8707 tokens bound to this server — and
rmcp ships an `auth` feature for it. `REMOTE.md` sets out the cost. It waits on
a queue with more than a handful of people on it, because that is the first
point at which editing a file stops being the simpler thing.

The witness caveat is unchanged and now load-bearing: a remote harness has a
remote working tree, so a shared queue serves every session with the witness,
footing and exhibit dark, and should say so by turning them off rather than
reporting about the server's own directory.

**Semantic search for memory.** FTS5 finds facts by the words they use;
recall finds them by the files they touch. Neither finds "the loader ignores
the config file" when you search for "precedence". Local embeddings could,
without breaking local-first — the models are small enough now. The bar to
clear: no network calls, no index that has to be rebuilt when the trail is
the source of truth, and degradation to today's behavior where the model is
absent. Until then, FTS5 with the substring fallback is the deliberate
choice, not a stopgap.

**The record, over time.** `hird record` aggregates every verdict ever
delivered into one table. Once a queue has months of history, "whose work
survives a reading by a different model" has a trend, and a trend is more
useful than a total. A `--since` and a per-plan slice keep it a report;
anything that feeds it back into dispatch is on the *never* list below.

## Never

Recorded here so their absence keeps reading as a decision rather than a gap:

- **A scheduler.** Nothing in hird will ever decide *when* work runs. The
  moment a plan can say "retry twice, then at midnight", the queue has become
  a workflow engine, and every workflow language that ever shipped grew
  `needs` first and `on_failure` second.
- **A router.** The queue knows what a task requires and what a caller
  advertises, and it stops there. No roster, no idle-worker tracking, no
  placement scores. The dispatch hook maps labels to workers, and that
  mapping belongs to the user's command, not to hird.
- **Dispatch steered by the record.** Agents graded by a table they can see
  are agents optimizing the table. The record measures; humans decide.
- **A daemon.** Lazy sweeps, one process per session, and a hook that runs
  detached have covered every "surely this needs a background process" so
  far. The bar for the first daemon is a feature that is impossible without
  one, not inconvenient.
- **Merged similar assertions.** Affirmation is word-for-word on purpose. A
  memory that quietly merges what a model thinks are similar sentences is a
  memory that loses facts.
