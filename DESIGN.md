# hird — cross-harness agent work queue & shared memory

> Working name: `hird` (Norse: a lord's retinue). Rename freely; the binary name is referenced throughout as `hird`.

## 1. Overview

`hird` is a single local-first Rust binary that lets multiple AI coding agents — Claude Code, Codex CLI, GitHub Copilot, or any MCP-capable harness — coordinate through a shared **work queue** and a shared **assertion-based memory store**, with a **ratatui TUI** for the human to observe and drive both.

The human creates tasks (CLI or TUI), then tells any agent in any harness "pick up task 42". The agent claims the task via MCP, works it, records assertions it learns along the way, and completes it. All state lives in one SQLite database. No daemon.

### Goals (v1)
- One binary, three modes: `hird mcp` (stdio MCP server), `hird tui`, `hird <cli-subcommand>`.
- Work queue with atomic claiming, lease-based liveness, and full status history.
- Assertion-based memory with provenance and FTS5 search.
- TUI: live queue board + memory browser.
- Works simultaneously from 3–4 concurrent harness sessions on one machine.

### Non-goals (v1)
- No multi-machine sync (design must not preclude it; see §9).
- No daemon / HTTP server mode.
- No task dependencies/DAGs, priorities beyond a simple integer, or scheduling.
- No automatic task dispatch — the human assigns tasks by telling an agent the id.
- No embeddings/vector search — FTS5 only.

> **v1.1 — swarm coordination.** Two of those non-goals were wrong, and §11
> below records why. Once several agents work one queue at the same time, "the
> human assigns tasks by telling an agent the id" is the bottleneck the rest of
> the design exists to remove, and dependencies are what make self-dispatch
> safe. Both are now built, along with a third thing v1 did not anticipate:
> declared file scopes, so the queue can see two agents heading for the same
> file before either of them writes to it. Sections marked **(v1.1)** are
> additions; nothing in v1 was removed.
>
> **v1.3 — plan files.** §13 adds a way to write a dependency graph down and
> file it in one transaction. It is a notation for the rows §11 already
> defined, not a workflow language: "no scheduling" above stays true, and §13
> gives the rule that keeps it true.

## 2. Architecture

```
┌────────────┐  ┌────────────┐  ┌────────────┐
│ Claude Code│  │  Codex CLI │  │  Copilot   │
│  session   │  │  session   │  │  session   │
└─────┬──────┘  └─────┬──────┘  └─────┬──────┘
      │ stdio MCP     │ stdio MCP     │ stdio MCP
┌─────▼──────┐  ┌─────▼──────┐  ┌─────▼──────┐
│ hird mcp   │  │ hird mcp   │  │ hird mcp   │   (one process per session)
└─────┬──────┘  └─────┬──────┘  └─────┬──────┘
      └───────────────┼────────────────┘
                ┌─────▼──────┐        ┌──────────┐
                │  SQLite    │◄───────┤ hird tui │ (poll, 500ms)
                │  (WAL)     │◄───────┤ hird CLI │
                └────────────┘        └──────────┘
```

- **DB location:** `${XDG_DATA_HOME:-~/.local/share}/hird/hird.db`. Overridable via `--db` flag and `HIRD_DB` env var.
- **Concurrency:** `journal_mode=WAL`, `busy_timeout=5000`, `synchronous=NORMAL`, `foreign_keys=ON`. All writes are short transactions.
- **Crate suggestions:** `rusqlite` (bundled), `rmcp` (official Rust MCP SDK) for the MCP server, `ratatui` + `crossterm` for the TUI, `clap` for CLI, `serde`/`serde_json`, `time` or `chrono`, `ulid` for ids.

## 3. Identity & scoping

- **Agent identity:** every MCP session identifies itself as `<harness>:<session>`, e.g. `claude-code:af31`. The harness name comes from the `HIRD_HARNESS` env var set in each harness's MCP registration config; failing that **(v1.6)** from the name the client gives for itself, latched from the first request that carries one; failing that `unknown`. The session suffix is a short random id generated at MCP process start. Stored on claims and assertions.
- **Project scoping:** every task and assertion carries a `project` string — the canonicalized project root path. The MCP server resolves it from `HIRD_PROJECT` env var if set, else the git toplevel of the server's CWD, else the CWD itself. One global DB, filterable by project. All MCP list/search tools default to the current project scope with an explicit `all_projects: true` escape hatch.

## 4. Data model

Use `ulid` strings as primary keys but ALSO give tasks a short monotonic integer `seq` for human reference ("task 42"). All timestamps are UTC ISO 8601 strings.

```sql
CREATE TABLE tasks (
  id          TEXT PRIMARY KEY,            -- ulid
  seq         INTEGER UNIQUE NOT NULL,     -- human-facing number, AUTOINCREMENT-like via meta table
  project     TEXT NOT NULL,
  title       TEXT NOT NULL,
  body        TEXT NOT NULL DEFAULT '',    -- markdown; full instructions for the agent
  status      TEXT NOT NULL DEFAULT 'open'
              CHECK (status IN ('open','claimed','in_progress','done','failed','cancelled')),
  priority    INTEGER NOT NULL DEFAULT 0,  -- higher = more important; informational only in v1
  claimed_by  TEXT,                        -- harness:session, NULL unless claimed/in_progress
  lease_expires_at TEXT,                   -- NULL unless claimed/in_progress
  result      TEXT,                        -- final summary written on done/failed
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

CREATE TABLE task_events (                 -- append-only audit trail; drives TUI history pane
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(id),
  at         TEXT NOT NULL,
  actor      TEXT NOT NULL,                -- harness:session, 'cli', or 'tui'
  kind       TEXT NOT NULL,                -- created|claimed|status|note|lease_renewed|lease_expired|completed|failed|cancelled|reopened
                                           -- (v1.1) |released|dep_added|dep_removed|scoped
  detail     TEXT NOT NULL DEFAULT ''
);

CREATE TABLE assertions (
  id         TEXT PRIMARY KEY,
  project    TEXT NOT NULL,
  content    TEXT NOT NULL,                -- one factual assertion, plain text
  tags       TEXT NOT NULL DEFAULT '',     -- comma-separated, freeform
  actor      TEXT NOT NULL,                -- who asserted it
  task_id    TEXT REFERENCES tasks(id),    -- optional: learned while working this task
  superseded_by TEXT REFERENCES assertions(id),  -- NULL = current
  created_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE assertions_fts USING fts5(
  content, tags, content='assertions', content_rowid='rowid'
);
-- keep in sync with INSERT/UPDATE triggers on assertions

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); -- schema_version, next_seq
```

Indexes: `tasks(project, status)`, `tasks(status, lease_expires_at)`, `task_events(task_id, at)`, `assertions(project, superseded_by)`.

**Schema migrations:** embed numbered SQL migrations, apply on open, track in `meta.schema_version`.

**(v1.1) Migration 2 — the dependency graph and declared file scopes.**

```sql
CREATE TABLE task_deps (
  task_id       TEXT NOT NULL REFERENCES tasks(id),
  depends_on_id TEXT NOT NULL REFERENCES tasks(id),
  actor         TEXT NOT NULL,
  created_at    TEXT NOT NULL,
  PRIMARY KEY (task_id, depends_on_id),
  CHECK (task_id <> depends_on_id)
);

CREATE TABLE task_paths (
  id          TEXT PRIMARY KEY,
  task_id     TEXT NOT NULL REFERENCES tasks(id),
  pattern     TEXT NOT NULL,   -- normalized glob, relative to the project root
  declared_by TEXT NOT NULL,
  at          TEXT NOT NULL,
  UNIQUE (task_id, pattern)
);
```

Indexes: `task_deps(depends_on_id)`, `task_paths(task_id)`.

Two properties are deliberate. **Blocked is derived, not stored**: a task with
unfinished dependencies is still `open`, so the status machine and its `CHECK`
constraint are untouched and there is no seventh state to keep consistent.
**Scopes are patterns, not files**: the work has not been done yet, so there is
nothing to observe; two agents collide when the sets their patterns describe
intersect, which is a decidable question about the patterns themselves. That
second property is what §13's plan preview is built on: two rows that do not
exist yet can still be asked whether they would collide.

**(v1.3) Migration 4 — the name a plan file gave a task.**

```sql
CREATE TABLE task_plan_nodes (
  task_id TEXT PRIMARY KEY REFERENCES tasks(id),
  project TEXT NOT NULL,
  plan    TEXT NOT NULL,
  node    TEXT NOT NULL,
  at      TEXT NOT NULL,
  UNIQUE (project, plan, node)
);
```

Sparse by design — a task filed by hand has no row here — and it is what lets
the same plan be applied twice without filing everything a second time. See §13.


## 5. Queue semantics

### Status machine
```
open ──claim──► claimed ──start──► in_progress ──complete──► done
  ▲                │                    │        └─fail────► failed
  │                └── lease expiry ────┘
  │                └──── release ───────┘   (v1.1)
  └──────── reopen (human) ◄── done|failed|cancelled
open ──cancel (human)──► cancelled
```
- **(v1.1)** `release` is the agent-side counterpart of a lease expiry: the
  holder hands an unfinished task straight back rather than making the queue
  wait out the TTL, and without `failed` — which is a verdict on the task and
  needs a human to clear it.
- `claim` is only valid from `open`. `complete`/`fail` only from `claimed`/`in_progress` **by the lease holder**.
- Humans (CLI/TUI) may `cancel` from any non-terminal state and `reopen` from any terminal state (clears `claimed_by`, `lease_expires_at`, `result`).

### Claiming (atomic CAS)
```sql
UPDATE tasks
SET status='claimed', claimed_by=?1,
    lease_expires_at=?2, updated_at=?3
WHERE seq=?4 AND status='open';
```
Zero rows updated ⇒ claim failed; return the task's current status and holder in the MCP error so the agent can report it to the user.

### Leases
- Default TTL: **15 minutes** (config: `lease_ttl_minutes`).
- Every successful `task_update` / `task_note` by the holder renews the lease.
- **Expiry is enforced lazily**: any reader that touches the tasks table first runs a sweep in the same connection:
  ```sql
  UPDATE tasks SET status='open', claimed_by=NULL, lease_expires_at=NULL, updated_at=?
  WHERE status IN ('claimed','in_progress') AND lease_expires_at < ?now;
  ```
  and writes a `lease_expired` event per affected row. No background thread needed; the TUI's 500 ms poll makes expiry visible promptly.
- A claim by a dead agent therefore self-heals back to `open` within TTL.

### (v1.1) Readiness

A task is **ready** when it is `open` and every task it depends on is `done`.
Only `done` clears a dependency: a `failed` or `cancelled` blocker means the
work its dependents were waiting for did not happen. `claim` refuses a task that
is not ready and names what it is waiting for; a dependency the queue does not
enforce is only a comment.

`task_deps` is kept acyclic by construction — an edge is refused when the
proposed dependency can already reach the dependent through its own
dependencies, and the refusal prints the chain that would have closed.

### (v1.1) Collision detection

Whenever a task's file scope is written — at creation, at claim time, or by
`task_scope` mid-flight — it is checked against every *other* task in the same
project that is currently `claimed` or `in_progress`. Two patterns conflict when
some path exists that both describe. That is an emptiness check on the
intersection of two regular languages, answered exactly by walking the product
of the two patterns' automata (`src/glob.rs`), not approximated by string
comparison: `src/*.rs` and `src/lib*` conflict, `a/**/x.rs` and `a/**/y.rs` do
not.

The check and the write share one `IMMEDIATE` transaction, so two agents
declaring overlapping scopes at the same instant are serialized and at most one
of them can come away believing it is alone in the file.

Policy is configuration, not code: `path_conflicts = "report"` records the
declaration and tells both sides, `"refuse"` rejects the claim and rolls back
everything it had taken.

### (v1.1) Dispatch

`task_next` claims the best *workable* task: `open`, ready, and — unless
`avoid_conflicts` is off — with a file scope that does not overlap live work.
Candidates are considered by descending priority then ascending `seq`, so the
queue is FIFO within a priority band. The whole scan and the claim happen in one
`IMMEDIATE` transaction, which is what lets N agents in N harnesses run the same
loop and come away with N different tasks.

An empty-handed answer carries its reason — how many tasks are blocked, and
which ready ones were passed over for which overlap — because an agent that can
say "three tasks are ready but another agent is in all of their files" is useful
to the human, and one that says "no work" is not.

## 6. MCP server (`hird mcp`)

stdio transport. Instructions string (returned from `server/discover`, and from `initialize` for clients still handshaking) must tell the model: how tasks are referenced (by `seq`), that it must claim before working, must call `task_update` at least every ~10 minutes to keep the lease, and should store assertions for durable facts it learns.

### Tools (exactly these twelve — eight from v1, four from v1.1)

| Tool | Input | Behavior |
|---|---|---|
| `task_list` | `status?`, `all_projects?` | List tasks (seq, title, status, holder, priority, updated_at, blocked_by). Runs lease sweep first. |
| `task_get` | `seq` | Full task incl. body, result, dependencies, file scope, overlaps, last 20 events. |
| `task_claim` | `seq`, `paths?` | Atomic claim as above. Refused while blocked. Returns full task body, scope and overlaps. |
| `task_next` | `all_projects?`, `avoid_conflicts?` | **(v1.1)** Claim the best workable task, or explain why there wasn't one. |
| `task_scope` | `seq`, `paths` | **(v1.1)** Holder-only. Declare files; answer with overlaps and what to do. |
| `task_update` | `seq`, `status?` (`in_progress` only), `note` | Holder-only. Appends `note`/`status` event, renews lease. |
| `task_split` | `seq`, `subtasks`, `sequential?` | **(v1.1)** Holder-only. File the pieces, make this task wait for them, release it. |
| `task_complete` | `seq`, `result`, `verdict?` **(v1.7)** | Holder-only. → `done`, clears lease, stores result. On a review, `verdict` is required and acted on (§16). |
| `task_fail` | `seq`, `reason` | Holder-only. → `failed`, clears lease, stores reason. |
| `task_release` | `seq`, `reason` | **(v1.1)** Holder-only. → `open`, clears lease, keeps the task claimable. |
| `mem_store` | `content`, `tags?`, `task_seq?` | Insert assertion with actor + project provenance. |
| `mem_search` | `query`, `limit? (20)`, `all_projects?`, `include_superseded? (false)` | FTS5 `MATCH`; fall back to `LIKE` if the query fails FTS syntax. Results include id, content, tags, actor, created_at. |

**(v1.1)** The server's instructions gain the swarm protocol: ask for work
with `task_next` when no number was named, declare files with `task_scope` as
soon as they are known, split rather than serialize, release rather than fail.

Design rules: every tool result is compact JSON; errors are descriptive strings the model can relay verbatim ("task 42 is claimed by codex:9f2c until 14:32Z"). No tool mutates memory implicitly.

### (v1.6) Lifecycle — MCP 2026-07-28

The server answers every protocol revision the Rust SDK implements, 2024-11-05
through 2026-07-28, and negotiates rather than requiring one. A revision it does
not implement is refused by name, with the list it does implement attached.

2026-07-28 deletes the `initialize` handshake and the protocol-level session:
a client opens with `server/discover` or with the request it actually wanted,
and each request carries the protocol version, the client's implementation and
its capabilities in `params._meta`. Nothing in this design leaned on that
handshake — the transport is stdio, one process per session, and the identity
and project scope are both read from the environment at process start — so the
two lifecycles coexist on one queue with nothing to reconcile.

Three consequences, and only three:

1. **Discovery is not cacheable.** The instructions string names the current
   project and is shaped by this machine's config file, so `server/discover`
   answers `cacheScope: private` and `ttlMs: 0`. It is nobody else's to reuse.
2. **A client names itself on every call**, which is the one thing worth
   taking. `call_tool` offers that name to the session identity, which keeps it
   only if `HIRD_HARNESS` left the identity unnamed — the env var is the half a
   human controls, and `hird register` writes it — and only once. An actor
   string that changed mid-session would leave the process unable to find its
   own leases, so the first name wins and is held.
3. **An opening request that cannot open a connection is answered**, not
   dropped. The SDK's response to one is to close the transport, which reaches
   a harness as "the hird server crashed"; it did not, and a request with an id
   is owed an error saying which `_meta` keys were missing.

Nothing is adopted from the revision beyond that. The three features it
deprecates — roots, sampling, protocol logging — are three this server never
used, so the deprecation window is not a migration for hird.

### Harness registration (document in README)
- Claude Code: `claude mcp add hird -e HIRD_HARNESS=claude-code -- hird mcp`
- Codex CLI: entry in `~/.codex/config.toml` `[mcp_servers.hird]` with `command = "hird"`, `args = ["mcp"]`, `env = { HIRD_HARNESS = "codex" }`
- Copilot / VS Code: `.vscode/mcp.json` stdio server, `HIRD_HARNESS=copilot`.
- OpenCode: `${XDG_CONFIG_HOME:-~/.config}/opencode/opencode.json` local server
  under `mcp.hird`, `HIRD_HARNESS=opencode`.

## 7. CLI

```
hird add <title> [--body <md>|--body-file <path>] [--priority N] [--project <path>]   → prints seq
                 [--needs <seq>,…] [--path <glob>]…                                  # (v1.1)
hird ls [--status s] [--all-projects]
hird show <seq>
hird cancel <seq> / hird reopen <seq>
hird dep add|rm <seq> --needs <seq>,…    # (v1.1) edit the graph
hird plan apply <file> [--dry-run] [--project <path>]  # (v1.3) file a whole graph
hird graph [--all-projects]              # (v1.1) the queue as dispatch waves
hird scope <seq> [--path <glob>]… [--clear]   # (v1.1) a task's file scope
hird agents [--all-projects]             # (v1.1) who is working what, and overlaps
hird mem add <content> [--tags a,b] / hird mem search <query>
hird tui
hird mcp
hird register <claude-code|codex|copilot|copilot-cli|opencode>
hird db-path
hird --install                         # copy this binary to ~/.local/bin/hird
hird --install-skill                   # install the skill for Codex, Claude, Copilot and OpenCode
```
Human CLI actions record events with `actor='cli'`.

`scripts/install.sh [--install-skill]` builds the locked release, runs the
installer, and cleans the release profile on exit.

## 8. TUI (`hird tui`)

ratatui + crossterm. Poll the DB every 500 ms (cheap queries; WAL makes this safe). Two screens, `Tab` to switch, `q` quits, `?` help overlay.

**Screen 1 — Queue board.** Kanban columns: Open | Claimed/In-progress | Done | Failed/Cancelled. Cards show `#seq title`, holder badge (`claude-code`, `codex`, `copilot` with distinct colors), lease countdown for claimed tasks, priority marker. Keys: `j/k` move, `h/l` columns, `Enter` detail pane (body + event history), `a` add task (title prompt; body optional), `c` cancel, `r` reopen, `/` filter by text, `p` toggle project filter (current/all).

**(v1.1) Cards** carry a `waits #1 #3` badge when a task is `open` but blocked,
because a board that shows unclaimable work as available is lying to the human.

**Screen 2 — Memory browser.** Top: search input (FTS query, live). Below: assertion list — content, tags, actor badge, age, linked task seq. `Enter` shows full assertion + provenance. `d` marks superseded (sets `superseded_by` to a tombstone assertion authored by `tui`). `p` project filter toggle.

**(v1.1) Screen 3 — Swarm.** Left: one row per live agent — harness badge, task,
lease countdown, declared files, and a red line for every overlap with another
agent, resolved by pattern intersection rather than string equality. Right: the
tasks that could be dispatched right now, and how many are queued behind them in
how many waves. `j/k` moves between agents, `Enter` opens the task one holds.

Status bar on every screen: DB path, project filter, counts by status, how many
tasks are ready to dispatch, last-poll age.

## 9. Future (do not build, do not preclude)

- Multi-machine: the append-only `task_events` + assertion model is CRDT-friendly; a later `hird sync` could ship via S3 like ccsync. Keep all mutations expressible as events.
- ~~Task dependencies (`blocked_by`), auto-dispatch~~ — built in v1.1, §11.
- **(v1.3) Not built, deliberately:** a plan file describes tasks and edges and
  nothing else — no conditionals, loops, retries or schedules. See §13 for the
  rule that keeps it that way, and why a queue agents pull from must not grow a
  scheduler.
- Embeddings for memory, HTTP mode for remote harnesses.
- **(v1.1) Not built, deliberately:** scopes are declared, never observed. Reading
  the working tree to check what an agent *actually* touched would make the queue
  depend on the checkout's state, and would report collisions only after the edit
  that caused them. Declaration is earlier and cheaper, and being advisory is the
  point — the queue tells agents about each other and lets them coordinate.

## 10. Implementation plan (milestones)

1. **M1 — Core:** crate skeleton (`clap` multi-mode binary), DB open/migrations, tasks + events + assertions modules with unit tests for the claim CAS and lease sweep (spawn N threads claiming the same task; exactly one wins).
2. **M2 — MCP:** `hird mcp` with all eight tools via `rmcp`; integration test driving the server over stdio JSON-RPC.
3. **M3 — CLI:** all subcommands.
4. **M4 — TUI:** queue board, then memory browser.
5. **M5 — Polish:** README with per-harness setup, `justfile` (build, test, lint, install), config file `~/.config/hird/config.toml` (`lease_ttl_minutes`, default project behavior).

## 11. Quality bar

- `cargo clippy -- -D warnings`, `cargo fmt --check` clean.
- Every DB mutation goes through a typed repository layer; no raw SQL in MCP/TUI/CLI layers.
- Property-style test for the status machine: no transition outside §5's diagram is reachable.
- The binary must start `mcp` mode in <50 ms (harnesses spawn it per session).

## 11. (v1.1) Swarm coordination — what changed and why

v1's queue works beautifully for one agent at a time. The moment three
harnesses are live it develops three problems, and they are the same problem:
the queue knows what work exists but not what work is *workable*.

1. **The human is the dispatcher.** Every task needs a person to say a number
   out loud. Three idle agents and twelve open tasks is twelve interruptions.
2. **Order is a matter of trust.** "Do the schema before the API" lives in a
   task body, and an agent that skips it fails in a way nobody notices until
   later.
3. **Nobody can see anyone else's hands.** Two agents open the same file from
   two harnesses and the second write silently wins.

Each is fixed by giving the queue one more thing to know:

| Problem | What the queue learns | Tool |
|---|---|---|
| dispatch | which task is most important *and* workable | `task_next` |
| order | which tasks wait for which | `task_deps`, enforced in `claim` |
| collisions | which files each task expects to touch | `task_paths`, `task_scope` |

They compose into something none of them is alone. Dependencies make
self-dispatch safe (an agent cannot start work whose prerequisites are missing);
declared scopes make *parallel* self-dispatch safe (two agents are never handed
work in the same files); and `task_split` closes the loop by letting an agent
turn one task into a fan-out the other agents can pick up — the queue's own
contents become the plan, written by the agents working it.

The invariant that keeps this honest: **every one of these decisions is made
inside the same `IMMEDIATE` transaction that performs the claim.** Readiness is
not advice checked beforehand; the collision check is not a race with the write
that follows it. Whatever the queue told an agent was true, was true at the
instant it handed the task over.

## 12. (v1.2) The witness — reading the working tree

§11 gave the queue three new things to know, and every one of them is something
an agent *said*. A task's status is its holder's claim about itself; its file
scope is its holder's prediction; its result is its holder's summary. The board
is an accurate record of what a set of cooperating agents reported, which is a
different thing from a record of what happened, and the difference is where the
expensive failure lives.

Consider the case the collision detector was built for, played out to the end.
Two agents declare `src/config.rs`. Both are told about the overlap. Both decide
their part is small. Both edit. The second write lands on top of the first,
`task_complete` succeeds for both, the board goes green, and git has nothing to
show because nothing was committed — there is one working tree and one version
of the file, and it is the second agent's. Nobody involved is in a position to
notice: not the first agent, whose session ended believing it had finished; not
the second, which never saw the version it replaced; not the human, who has two
completed tasks and a plausible pair of summaries.

So v1.2 adds the one participant that is not taking anybody's word for it.

**Migration 3.** `task_witness` holds one working-tree fingerprint per task —
`HEAD`, plus a content hash for every path that could plausibly move — taken
when the task is claimed and never moved afterwards. `task_changes` holds the
difference between that fingerprint and the tree as it stands, one row per path,
rewritten on every observation, so a file edited and then put back the way it
was leaves no row behind.

**Candidate paths come from git.** `git status --porcelain -z
--untracked-files=all` decides what is worth hashing, which means `.gitignore`
decides what counts as noise and hird reimplements nothing. A clean checkout
costs one process and no hashing at all. When a task's recorded `HEAD` has moved
— an agent that commits its own work — whatever the commits touched is a
candidate too, so committing does not erase the record of having done it.

### What it can and cannot prove

One checkout has one filesystem and no keyboards. When a file changes while
three agents hold leases, all three have it in their footprint, and no amount of
hashing will say which of them typed. hird does not pretend otherwise: the
footprint is described everywhere as *what moved while the task was held*, never
as what the task did.

Saying *who* needs the other half — and the other half already exists. A
declared scope is an agent stating that it holds a copy of a file and intends to
write from it. So:

> **A contention is a path that two live tasks both declared, that has since
> moved under both of them, and that the two of them disagree about the content
> of.**

Each clause carries weight. Both declaring is what supplies attribution the
filesystem cannot. Having moved is what makes it an event rather than a
prediction — the plain declared overlap is already reported, and repeating it
adds nothing. Disagreeing is what makes it *actionable*: two agents who have
both been shown the current version are not in trouble, and warning them anyway
is how a warning stops being read.

That is the predicted collision and the observed one at the same time, and what
it tells the agent holding the stale copy is exactly the thing that saves the
work: **re-read the file before you write it.**

### Looking is not telling

The `hash` on a change row is not "the content now". It is the last version this
task's own holder was *shown*. Anybody may look — another agent's check-in, the
human's TUI polling twice a second — and looking brings footprints up to date
without touching a single hash. Only `confirm`, called on a holder's own
check-in *after* it has been handed the report, moves them.

The ordering is load-bearing and easy to get wrong. Confirming first would mark
an agent's copy current in the same breath as telling it the file had moved,
which is to say it would never tell it anything. Every MCP tool that witnesses
therefore does the same three steps: sweep, read, confirm.

### Off the critical path by construction

The witness is a courtesy on top of the real answer, and the failure mode it
must never have is costing an agent a call it did make in order to deliver a
report it did not ask for.

- `Witness::discover` returning `None` is an ordinary state, not an error: no
  git, no repository, no `git` on `PATH`, `witness = false`. Every tool then
  answers exactly as it did before v1.2, and the handshake omits the paragraph
  about the witness entirely rather than promising a model a field nothing will
  ever populate.
- Every sweep is wrapped and discarded on failure. There is no configuration in
  which a witness error becomes a tool error.
- No live task in the project and git is not invoked at all. The cost is only
  paid while somebody is working.
- Measured on this repository: ~8 ms for a claim, ~6 ms for a check-in, against
  0.5 ms for a tool that never looks. Three git subprocesses, against a
  heartbeat measured in minutes.

### What it feeds

Beyond the contention itself, the footprint answers two questions the queue
could not:

- **Drift.** Paths that moved under a task that none of its declared patterns
  covers come back as `undeclared` — worth saying, because every other agent's
  collision check is reading the declaration and not the edits. The advice is
  phrased conditionally when another agent is live, because in that case hird
  genuinely does not know whose edit it was.
- **Recall.** §11's memory reaches a task through *declared* files, which makes
  it depend on the previous agent having declared any — exactly the step an
  agent in a hurry skips. Witnessed paths count too, on both sides. An agent
  that said nothing and edited `src/config.rs` still leaves the file behind it,
  and what it learned still reaches whoever comes next.

Still twelve MCP tools. Like recall, the witness needed no new one: it rides
along on the calls agents already make, which is the only way a fact reaches an
agent that does not know to ask for it.

## 13. (v1.3) Plan files — the graph as data

§11 gave the queue dependencies and file scopes, which is what makes a plan
worth filing at all. It did not give anyone a way to *write one down*. In
practice a plan was a shell script:

```sh
schema=$(hird add "Design the storage schema" --path 'src/db.rs')
repos=$(hird add "Port the repository layer" --path 'src/repo/**' --needs "$schema")
```

That composes, and it stays supported. But the plan only ever exists as the act
of filing it. There is nothing to review before three agents are pointed at it,
nothing to commit next to the code it describes, no way to run it twice, and a
script that dies between two lines leaves real tasks behind missing exactly the
dependencies that were going to keep them in order.

A plan file is the same graph as data, in TOML, with symbolic names in place of
queue numbers.

### The rule that keeps it a format and not a language

**Nothing may appear in a plan that is not already a column.** A plan may carry
a title, a body, a priority, declared paths and `needs`, because `tasks`,
`task_paths` and `task_deps` hold exactly those. It may not carry a
conditional, a loop, a retry policy or a schedule, because there is no table
for one.

This is not fastidiousness about scope. The whole of §11 rests on agents
pulling: `task_next` is a tool an agent chooses to call, and nothing in hird
pushes work at anybody. A file that could say *when* to run something would be
describing a scheduler this queue does not have and does not want, and every
workflow language that has ever shipped grew `needs` first and `on_failure`
second. Tying the format to the schema makes the boundary structural rather
than a matter of resolve.

Two consequences follow, and both are load-bearing:

- The format is a *serialization of the queue*, so it round-trips in principle:
  a plan file and a set of rows say the same thing.
- The likely author is a model, not a person — "plan this migration" produces a
  file the human reviews. That argues for a syntax models already emit
  perfectly (TOML) over any bespoke grammar, and for spending the effort on
  validation messages rather than on notation. `deny_unknown_fields` is part of
  that: `need = [...]` is a typo worth refusing, not a field worth ignoring.

### Identity, so a plan can be applied twice

```sql
CREATE TABLE task_plan_nodes (
  task_id TEXT PRIMARY KEY REFERENCES tasks(id),
  project TEXT NOT NULL,
  plan    TEXT NOT NULL,
  node    TEXT NOT NULL,
  at      TEXT NOT NULL,
  UNIQUE (project, plan, node)
);
```

A task remembers the name the plan filed it under. Applying the same plan again
recognizes its own work and files only what the file has gained, which is what
makes a plan something to edit rather than something you get one shot at. The
name is scoped by project, so one plan file serves every checkout that uses it.

Existing tasks are never rewritten. By the time a plan is edited an agent may
have claimed a task, worked it, and recorded what it learned against it; a
title that has drifted from the file is reported (`Applied::drifted`) and the
queue's version is kept. A plan is how work is *filed*, not a description the
queue is reconciled against — hird has no mechanism for un-filing work an agent
is holding, and inventing one to serve a file would be the tail wagging the dog.

Applying is one `IMMEDIATE` transaction: every task, every edge, every
declaration, or nothing. The only refusal left at that point is a dependency
that would close a cycle *through an edge added outside the plan* — the plan's
own graph is checked for rings before anything is written — and it is the same
check and the same message `Deps::add` produces.

### Why the dry run is the point

Filing is the smaller half. `--dry-run` resolves the plan through the very same
`dispatch_waves` the board uses, and then reports the thing the queue could not
have reported until the work was already live: **pairs of tasks that declare
intersecting globs with no dependency ordering them.**

The collision detector in §11 compares a declaration against *live* claims,
which means the earliest it can speak is the moment an agent claims the second
task. But glob intersection is a decidable question about the patterns alone —
that is why §11 chose patterns over files — so the same question can be asked
of two rows that do not exist yet. A plan whose first wave lists four tasks, two
of which declare `src/tui/**` and `src/tui/view.rs`, is a three-wide plan with a
queue in it, and that is worth knowing while it is still a file.

The preview also names tasks that declare nothing, since a task with no scope is
invisible to both the collision check and recall's strongest signal.

### Still twelve MCP tools

There is no `plan_apply` tool, and this is a boundary rather than an omission.
The review step is the entire reason the file exists; an agent that could write
a plan and apply it unread would have removed it. An agent that discovers its
task is really three jobs already has `task_split`, which puts the pieces in
front of the *other* agents and keeps the human on the board rather than in the
loop.

## 14. (v1.4) Footing — memory that knows when its evidence moved

Everything hird stores about a *task* is answerable. A claim has a holder. A
declared scope has an overlap. A footprint has a diff. §12 exists because the
one thing agents could not be trusted to report was what they had actually
done, so the queue went and looked.

Memory was never held to that standard, and it is the half where it matters
most. An assertion recorded in March is served up in July in the same voice,
with the same confidence, whether the code it describes has been untouched
since or rewritten twice. That is how a shared memory stops being an asset —
not by filling up with lies, which a human would notice, but by filling up with
sentences that *were* true, which nobody notices, because reading one tells you
nothing about which kind you have.

The sentence cannot tell you. The code can.

### Migration 5

```sql
CREATE TABLE assertion_footing (
  assertion_id TEXT NOT NULL REFERENCES assertions(id),
  path         TEXT NOT NULL,
  hash         TEXT NOT NULL DEFAULT '',
  at           TEXT NOT NULL,
  PRIMARY KEY (assertion_id, path)
);

CREATE TABLE assertion_affirmations (
  assertion_id TEXT NOT NULL REFERENCES assertions(id),
  actor        TEXT NOT NULL,
  at           TEXT NOT NULL,
  PRIMARY KEY (assertion_id, actor)
);
```

An assertion's **footing** is the files it was read off and the content hash
each of them had at the time. Any later reader asks the working tree whether
that ground has moved. hird can afford this for almost nothing because §12 is
already fingerprinting files for a different reason — the whole feature is one
table, one hash per anchored file per read, and the module that decides what it
means.

### It reports, it does not judge

`Standing` has four values and none of them is `false`:

| | |
|---|---|
| `unanchored` | No files were recorded. hird has nothing to say, and says nothing. |
| `firm` | Every file it was read off still hashes to what it hashed to. |
| `shaky` | At least one does not. |
| `orphaned` | All of them are gone. |

A changed file does not falsify an assertion. A rename, a `cargo fmt` and a
total rewrite are indistinguishable from here, and a queue that announced
"this fact is now wrong" on the strength of a whitespace change would be
training every reader to ignore it. `shaky` means **unverified**, which is a
weaker claim and a far more useful one, because that set is exactly where
opening the file pays for itself.

`orphaned` is held to the strongest condition available — not one file gone,
but nothing left standing — for the same reason. An assertion about three files
of which one was deleted is shaky; the other two may still be exactly what it
describes.

### Where the footing comes from

Two sources, unioned, and the asymmetry is deliberate:

- **What the task declared**, but only its *literal* paths. A glob names a set
  nobody has enumerated, and hird will not guess at its members.
- **What the witness saw move.** This covers the agent that never declared
  anything — most of them, in practice — and a realized glob's members are in
  here already by construction: they are the files that actually changed.

An agent may also name files outright (`mem_store` gained a `paths` argument),
which is the escape hatch for a fact not tied to a task. An assertion with
neither stays unanchored, because inventing a footing for a general statement
about the project would be inventing a reason to distrust it later.

The two are not interchangeable, and `assertion_footing.named` records which
happened. **A derived footing never overwrites a named one.** Deriving is hird
being helpful where nobody said anything; overruling a stated footing — on a
finishing task, or when somebody else restates the fact — would be hird
deciding it knows better than the agent that wrote the sentence, about a file
that agent may have chosen precisely because the task never went near it. The
rule lives in `Footings::anchor` rather than at its call sites, so there is one
authority for it and no way to route around it.

### One project at a time

An anchor is a path relative to *its own* project root, and a `Witness` can
only answer for one tree. `Footings::anchors_for` therefore takes the project
it is being resolved for, and a cross-project search gets standings for the
rows hird can vouch for and silence for the rest. `hird mem standing
--all-projects` does the other thing available — discovers a witness per
project — because there it is a report rather than a hot path. Resolving
another checkout's relative paths against this one would produce answers that
look exactly like facts.

### Settling, which is what makes it quiet enough to read

A fact recorded in the third minute of a task is a statement about the code as
it was in the third minute, and by the time that task finishes its own author
has usually edited that code. Without a correction, every task would mark its
own facts shaky by its own hand, and `shaky` would mean nothing at all.

So finishing a task **settles** what it learned: re-anchors every assertion
recorded on it to the tree it is leaving behind. After that, `shaky` means
*somebody else moved this*, which is the only reading worth a warning. Like the
sweep it rides beside, it is best-effort in the strongest sense — nothing it
does may turn a completed task into a failed one.

### The way back, without a thirteenth tool

An agent that checks a shaky fact and finds it still true has exactly one way to
say so: say the fact again. It should not have to know anything else. So that is
what hird made it mean — `mem_store` with content that matches an existing
current assertion **word for word** does not duplicate it. It records this actor
as another voice for it and re-anchors it to today's code, and the answer says
`affirmed: true`.

Word for word is the deliberate bar. Two sentences that mean the same thing are
a judgement call, and a memory store that quietly merged what a model thought
were similar would be a memory store that loses facts.

Two things fall out of it. Duplicate assertions stop accumulating, which is the
oldest complaint about assertion memories. And the affirmation table counts
*voices* rather than sentences, so hird can say the thing no single harness is
positioned to say: **two agents in two different harnesses, unable to see each
other's sessions, arrived at the same fact independently.** That is a trust
signal only the process both of them talk to can produce.

### Still twelve MCP tools

`mem_store` gained an argument and `mem_search` and recall gained a field. There
is no `mem_verify`, no `mem_reaffirm` and no `mem_audit`: something an agent has
to know to ask for is something it will not ask for, and the confirmation an
agent already had a reason to perform is the one that should carry the
re-grounding. The human's audit — `hird mem standing`, and `f` in the memory
browser — is a CLI and TUI concern, because deciding what to do about a memory
that has drifted is a human's job and always was.

### Off is off

`memory_footing = false`, or a project outside git, and memory behaves exactly
as it did before any of this existed: no anchors written, no standing computed,
no `standing` field in any payload, and the server's instructions do not
mention it — a model told to read a field nothing will ever populate has been
handed a rule it cannot use and a reason to doubt the ones it can. That is the
same rule §12 lives by, for the same reason.

## 15. (v1.5) Recusal — no agent reviews its own work

Every result line in the queue was written by the agent that produced the work
it describes. `task_complete` takes a summary; the summary is the last word;
nobody else ever looks. §12 exists because that same asymmetry was intolerable
for *what an agent did*, and went and read the disk instead. It was left in
place for *whether the work was any good*.

For one agent that is not a choice — there is nobody else to ask. For a swarm
it is, and a strange one, because the single most valuable property of running
three different models on one codebase is precisely that they are not the same
model, and the cheapest way to spend that is to have them read each other.

Every harness can review code. What none of them can do is know *whose* code it
is looking at: a harness cannot see another harness's session, and an agent
asked "review this" has no way to tell whether it is being handed its own work
from an hour ago. hird can. It is the process all of them talk to, and it wrote
down who held the lease.

### Migration 6

```sql
CREATE TABLE task_recusals (
  task_id      TEXT NOT NULL REFERENCES tasks(id),
  from_task_id TEXT NOT NULL REFERENCES tasks(id),
  reason       TEXT NOT NULL DEFAULT '',
  actor        TEXT NOT NULL,
  at           TEXT NOT NULL,
  PRIMARY KEY (task_id, from_task_id),
  CHECK (task_id <> from_task_id)
);

ALTER TABLE tasks ADD COLUMN review INTEGER NOT NULL DEFAULT 0;
```

A **recusal** is one edge: *whoever worked task N must not work this one*. It is
the second kind of edge in the graph and the mirror of the first — `task_deps`
constrains *when* a task may be claimed, `task_recusals` constrains *who* may
claim it — and like dependencies it is enforced rather than annotated.

### Three decisions

**The bar is the harness, not the session.** Two Claude Code windows are one
model reading its own homework. A recusal that excluded only the exact session
would be satisfied by opening a new tab, which is to say satisfied by nothing.
`actor_harness` already exists for the badge colours; here it decides an
outcome.

**It is enforced where the claim is decided.** `claim_in_tx` checks it in the
same `IMMEDIATE` transaction as the compare-and-set, between the dependency
check and the `UPDATE`, so there is no race to win and a refused claim leaves no
trace. `claim_next` checks it too, but as a *filter* — dispatch routes around a
recused task rather than handing out something it would then refuse, because an
agent that asked for "whatever is workable" and got an error would reasonably
conclude the queue was empty.

**Who worked a task is read from the trail, not the row.** Completing clears
`claimed_by`, so `worker_of` takes the latest of the `claimed`, `completed` and
`failed` events. That gets the awkward cases right for free: work released and
picked up by somebody else is credited to whoever finished it, and a recusal
filed before anybody has worked the task bars nobody rather than locking the
queue.

### Why the review files itself

A review a human has to remember to file is a review that does not happen. This
is the same observation recall and the witness are built on — *something an
agent has to know to ask for is something it will not ask for* — pointed at the
human for once.

So `tasks.review` is a flag set when the work is filed (`hird add --review`,
`review = true` in a plan), and completing a task that carries it files the
review as an ordinary task: titled after the work, at the same priority,
**scoped to the files the witness actually saw move** rather than the ones
anybody declared, carrying the author's own summary marked as the thing under
review rather than as the brief, and recused from the task it reviews.

It declines to file in three cases, and each of them is a case where filing
would put noise on a human's board: work the witness saw no trace of, work that
`failed` — there is nothing to check, and whether the attempt is worth reading
is the human's call — and work whose previous review is still unfinished, so
reopening and re-completing cannot stack them up.

### It is a constraint, not a scheduler

A recusal says who may **not** take a task. It never says who must, nothing here
pushes work at anybody, and §13's rule holds unchanged: a plan file may carry
`review = true` because `tasks.review` is a column, and may not carry anything
that would decide *when* work runs.

The consequence is that a queue with one harness on it and a recusal in it has
one task nobody can claim. That is reported rather than papered over — a
`task_next` whose only remaining candidates are recused says so in as many
words, because "nothing is open" would send the human away from the one thing
that needs them, and the fix is to open a different tool rather than to wait.

## 16. (v1.7) The verdict — the review closes its own loop

§15 got the work in front of a second harness. It did not listen to the answer.
A review ended in prose — a `result` line saying, somewhere in its own words,
whether the work was any good — and then the loop dangled: a human had to read
the review, decide that "the error path drops the lock" means *broken*, find
the task it reviewed, reopen it, and carry the findings across by hand. Every
other hand-off in hird files itself, on the observation that something an agent
has to remember to do is something that does not happen. The one hand-off
carrying the judgment was the one left manual.

So a review ends in a **verdict**, enforced where the review ends:
`task_complete` on a task that has recusal edges requires one, and refuses one
everywhere else. Both refusals teach, because the agent reading them is
mid-completion with nobody to ask. The two verdicts name their own
consequences:

- **`upheld`** — the work stands. The judged task stays `done`, an event lands
  in its trail, and every surface (`task_list`, `task_get`, `hird show`, the
  TUI) can now distinguish *done* from *done, and seen to be done* by a harness
  that provably did not do it.
- **`sent_back`** — the work does not stand. In the same transaction as the
  review's completion, the judged task reopens with the reviewer's findings
  appended to its brief, so the next claimant — its author included — is handed
  exactly what must change without knowing to ask. The task still carries
  `review = 1`, so finishing it again files a fresh review (§15's "previous
  review still unfinished" guard has just cleared), and the loop runs round
  after round until a review upholds.

### Migration 7

```sql
CREATE TABLE task_verdicts (
  id        TEXT PRIMARY KEY,
  review_id TEXT NOT NULL REFERENCES tasks(id),
  task_id   TEXT NOT NULL REFERENCES tasks(id),
  verdict   TEXT NOT NULL CHECK (verdict IN ('upheld','sent_back')),
  worker    TEXT NOT NULL DEFAULT '',   -- whose work was judged, at that moment
  reviewer  TEXT NOT NULL,
  at        TEXT NOT NULL,
  CHECK (review_id <> task_id)
);
```

Append-only, like the event trail: a task sent back and redone accumulates one
row per round, which is what keeps the record honest about how many rounds
there were. `worker` is resolved at delivery time by the same trail-reading
`worker_of` recusal uses, so the name on a verdict does not move when the
reopened task is later picked up by somebody else.

### One invariant bends, knowingly

"Terminal statuses only leave via a human reopen" was written when nothing but
a human could be trusted to judge finished work. The recusal edge is what
changed that: a `sent_back` comes from a harness the queue *proves* did not do
the work, which is precisely the trust the human reopen was standing in for.
The status machine itself is untouched — `done → open` via `Reopen` was always
an edge; what changed is who may drive it. The human keeps the last word they
always had, and the queue never overrules them: a verdict that lands on work a
human already reopened, cancelled, or that failed since, goes on the record and
does nothing else. Only work sitting exactly where the completion left it —
`done` — is moved.

### The record

Because every verdict is delivered on the record — who judged, whose work,
which round — the queue accumulates the one measurement it is uniquely placed
to take: whose work survives a reading by a different model. `hird record`
aggregates delivered verdicts per harness, both sides: verdicts received on its
work (with a first-pass count over distinct tasks — the verdict that measures
the work as delivered, before any rework), and verdicts handed out as a
reviewer.

It is a report, not a scheduler. Nothing routes work by it — dispatch does not
read it, `task_next` does not prefer a harness by it — because the moment the
queue starts steering work toward whoever scores well, agents are being graded
by a table they can see and the table stops measuring anything. Reading it is
the human's job, and what to do about a harness that ships rework is exactly
the kind of call hird leaves to people.

### Still twelve tools

The verdict is a parameter on `task_complete`, not a thirteenth tool; the
record is a CLI report off a table that writes itself. The MCP surface §6
froze stays frozen.

## 17. (v1.8) The footprint — did this task change anything?

§12 gave every task a list of the files that moved while it was held. The list
answers "what changed?" precisely, and it answers the blunter question — "did
this change *anything*?" — ambiguously, because it comes back empty in two
opposite situations:

- the task read the code and wrote nothing, and
- hird was never watching, so it has nothing to report either way.

A front end that prints nothing in both cases invites the reader to take the
second for the first. That is the mistake this section exists to make
impossible: a finished investigation and a finished refactor arrive on the board
as the same green card, and the difference decides whether there is anything to
review, anything to test, or anything to undo.

### Three answers, never two

`Footprint` is the type, and it is deliberately three-valued:

| value | means | said as |
| --- | --- | --- |
| `Unwatched` | no baseline was ever taken: never claimed under a witness, or not a git checkout | nothing at all |
| `ReadOnly` | a baseline was taken and the tree still matches it | `read-only` |
| `Modified { paths, shared }` | the tree differs from the baseline | `modified 3 files` |

The predicate for "hird was in a position to know" is the existence of the
task's row in `task_witness` — the fingerprint taken at claim time. That is what
makes `ReadOnly` an observation rather than an absence of one, and it is why the
question is not answered off `task_changes` alone.

**No migration.** Both tables already hold everything the answer needs; v1.8 is
a reading of v1.2's evidence, not new evidence. Nothing new is written on any
call, and turning the witness off turns this off with it, in the only way it
can be turned off: hird stops having an opinion.

### What a count may not be read as

The limit from §12 carries over unchanged — one checkout has one filesystem and
no keyboards — so `Modified` says the tree moved while the task was held, not
that this task's agent moved it. Where another task in the same project holds
one of those paths in its own footprint, both were live when the file moved, and
`shared` says so: *modified 2 files, though another agent was live in some of
them*. The count is never allowed to speak with more confidence than the
evidence has earned, which is the same rule the drift advice follows.

### A running total is not a verdict

A task still being worked has not finished not writing anything, so `ReadOnly`
is rendered `read-only so far` while a lease is held and `read-only` once it is
over. `Modified` needs no such hedge: a file that moved has moved.

This is also why `FinishResult` re-says its own footprint. The last look has to
be taken while the task is still live, or the sweep would miss whatever the
agent did on its way out — so by the time the reply is assembled, the hedge in
the sentence is one call out of date. `Evidence::settled` restates it for a task
that has stopped, from the same typed answer rather than by editing a string.

### Where it shows

- `hird ls` badges each row, and now sweeps the tree as it renders — a stale
  answer here is worse than none, because a task that has been writing since the
  last sweep would be listed as read-only.
- `hird show` heads the `changed` block with the one-line answer, above the
  paths it is drawn from.
- `hird agents` says `moved  nothing yet` where an agent has been in the code
  and written none of it. An agent twenty minutes into a task with an empty
  footprint is reading, or it is stuck; a blank space says neither.
- The TUI badges every card on the queue board, writes the sentence into the
  task detail overlay, and marks a live agent's row on the Swarm screen.
- MCP reports it as `footprint`, one string, in the same flattened evidence
  block as `changed` and `contended` — absent where there is nothing to say, the
  way every other witness field is.

The board asks the question of every card it paints, twice a second, so the
repository offers it both ways: `footprint(seq)` for one task and
`footprints(scope)` for a whole board in two queries and a fold.

### Still twelve tools

`footprint` is a field on evidence agents already receive, not a thirteenth
tool. Like recall and the witness before it, it reaches an agent that did not
know to ask.

## 18. (v1.9) The ground — what a task builds on

Every dependency edge in this queue was drawn because one task needs what
another produces. That is what "the API waits for the schema" *means*: not
merely that the schema must exist first, but that the API's author needs to
know what the schema turned out to be. And until now the queue read that edge
exactly once, as a gate. The blocker goes `done`, the gate opens, the
blocker's `result` — the one sentence its finisher wrote for exactly this
moment — is dropped on the floor, and the dependent's agent starts blind
unless recall's file overlap happens to carry something across. The causal
edge, the strongest signal of relevance hird holds, was the one channel
delivering nothing.

And v1.7 quietly made the gate itself unsound. Readiness still says "only
`done` clears a dependency", but §16 made `done` revocable: a review can send
finished work back, reopening it in the same transaction as the verdict. A
dependent claimed in the window between the completion and the verdict is
building on ground that may be pulled out from under it — and when it is,
*nobody tells it*. Its readiness was checked at the claim and never again.
Its holder cannot see the verdict land. The board shows a reopened task and a
live dependent, and the one participant positioned to connect them is the
queue, which until now said nothing.

So v1.9 teaches the queue to treat the ground under a task — the finished
work it builds on — as a first-class thing: handed over, qualified, and
watched.

### The handover

`task_claim` and `task_next` answer with `built_on`: one row per finished
dependency, carrying its `seq`, title, its **own `result`** — the summary its
finisher wrote — and a `standing`. The claim is the one moment the claimant
is guaranteed to be listening, which is where every rider in hird lives, and
the read happens inside the same transaction as the compare-and-set, so the
handover describes the instant the task changed hands. `task_get` and `hird
show` carry the same block (`built on  #3 done — the schema lives in db.rs`),
and the TUI's task detail does too.

`standing` is a deliberate echo of memory's §14: both answer *"this was true
when it was written — is it still?"*, one for assertions, one for work.

| | |
|---|---|
| `done` | finished, with nothing further on record |
| `upheld` | finished and seen to be finished, by a harness that provably did not do it |
| `under review N, provisional` | finished, but the review has not delivered its verdict |

`provisional` is the honest word for the third state and the reason it must
be said: the claimant is about to spend real work on that answer, and the
difference between "settled" and "could be sent back while you build" is
exactly the difference it cannot discover for itself.

### The policy

Whether provisional ground should *hold* a dependent back is not a fact, it
is a judgement about a project's tolerance for rework — so like
`path_conflicts` before it, it is a key rather than a rule:

```toml
under_review = "clears"   # or "holds"
```

`"clears"` is the default and the old behavior with the silence removed:
`done` clears the dependency at once, and the claimant is told what it is
standing on. `"holds"` keeps dependents unclaimable until the pending review
finishes — upheld releases them; sent back reopens the work, and readiness,
being derived, re-gates them behind it automatically; a review a human
cancels abandons the hold, because the human keeps the last word they always
had. The refusal teaches, the way every refusal here does: *"task 7 is
blocked by task 3 (port the loader, done but under review 5); the work is
finished, but this queue holds dependents until the review delivers its
verdict."* And `task_next` routes around held tasks the way it routes around
recused ones, in their own bucket — `held`, with the review each waits on —
because "the work is done and the review has not been read" points a human at
a completely different fix than "the work has not happened". A pending review
is discovered from rows that already exist — an unfinished task recused from
this one is what *being* its review means (§15) — so there is no migration
and nothing new to keep consistent.

### The shift

When a sent-back verdict reopens work, its live dependents — tasks
`claimed` or `in_progress`, let through while the ground was still standing —
are the casualty the verdict cannot see. Delivery now writes a
`ground_shifted` event on each of their trails in the same transaction as the
reopen, so the record exists the moment the fact does. Open dependents get
nothing: readiness is derived, so they are simply blocked again, and their
eventual claim hands them the reopened brief anyway.

The holder itself hears at its next check-in. `task_update` answers with
`ground_shifted` — one sentence per dependency that has stopped being `done`,
computed from the graph as it stands rather than replayed from events, which
buys three things: it cannot be stale, it costs one indexed query only when
the task has edges, and it catches every way ground gives way, not just the
verdict — a blocker a human reopened, cancelled, or that failed since is
reported in the same breath, attributed no further than its status can back.
The sent-back case names its review and points at the findings: *"task 3,
which this task builds on, was sent back by review 5 and reopened; re-read it
— the findings are in its brief — before building further on its work."*
That sentence outranks the witness's advice in the reply, because a contended
file costs an edit and a sent-back foundation costs the task.

The human gets the same three sights without asking: `hird agents` and the
Swarm screen mark a live agent whose ground has shifted, and a `done` card
whose review is still open carries `under review #5` on the board — the
difference between *done* and *done, so far as anyone has checked*, visible
from across the room.

### What it deliberately does not do

The queue does not interrupt. An agent mid-edit is not stopped, its lease is
not revoked, its task is not released — hird tells, at the next moment the
agent is listening, and lets it decide, exactly as the witness does with a
contention. And nothing here re-litigates §16's restraint: a verdict still
moves only work sitting exactly where the completion left it, and the shift
report attributes a reopen to a review only when the newest verdict on
record actually is that sent-back.

### Still twelve tools

`built_on`, `held` and `ground_shifted` are fields on answers agents already
receive; the policy is a configuration key; the event is a row in a table
that existed in v1. Nothing new to call, because an agent that had to ask
"has my ground moved?" is an agent that would not ask.
