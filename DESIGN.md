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

- **Agent identity:** every MCP session identifies itself as `<harness>:<session>`, e.g. `claude-code:af31`. The harness name comes from `HIRD_HARNESS` env var set in each harness's MCP registration config (fallback `unknown`); the session suffix is a short random id generated at MCP process start. Stored on claims and assertions.
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
intersect, which is a decidable question about the patterns themselves.


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

stdio transport. Instructions string (returned in MCP initialize) must tell the model: how tasks are referenced (by `seq`), that it must claim before working, must call `task_update` at least every ~10 minutes to keep the lease, and should store assertions for durable facts it learns.

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
| `task_complete` | `seq`, `result` | Holder-only. → `done`, clears lease, stores result. |
| `task_fail` | `seq`, `reason` | Holder-only. → `failed`, clears lease, stores reason. |
| `task_release` | `seq`, `reason` | **(v1.1)** Holder-only. → `open`, clears lease, keeps the task claimable. |
| `mem_store` | `content`, `tags?`, `task_seq?` | Insert assertion with actor + project provenance. |
| `mem_search` | `query`, `limit? (20)`, `all_projects?`, `include_superseded? (false)` | FTS5 `MATCH`; fall back to `LIKE` if the query fails FTS syntax. Results include id, content, tags, actor, created_at. |

**(v1.1)** The `initialize` instructions gain the swarm protocol: ask for work
with `task_next` when no number was named, declare files with `task_scope` as
soon as they are known, split rather than serialize, release rather than fail.

Design rules: every tool result is compact JSON; errors are descriptive strings the model can relay verbatim ("task 42 is claimed by codex:9f2c until 14:32Z"). No tool mutates memory implicitly.

### Harness registration (document in README)
- Claude Code: `claude mcp add hird -e HIRD_HARNESS=claude-code -- hird mcp`
- Codex CLI: entry in `~/.codex/config.toml` `[mcp_servers.hird]` with `command = "hird"`, `args = ["mcp"]`, `env = { HIRD_HARNESS = "codex" }`
- Copilot / VS Code: `.vscode/mcp.json` stdio server, `HIRD_HARNESS=copilot`.

## 7. CLI

```
hird add <title> [--body <md>|--body-file <path>] [--priority N] [--project <path>]   → prints seq
                 [--needs <seq>,…] [--path <glob>]…                                  # (v1.1)
hird ls [--status s] [--all-projects]
hird show <seq>
hird cancel <seq> / hird reopen <seq>
hird dep add|rm <seq> --needs <seq>,…    # (v1.1) edit the graph
hird graph [--all-projects]              # (v1.1) the queue as dispatch waves
hird scope <seq> [--path <glob>]… [--clear]   # (v1.1) a task's file scope
hird agents [--all-projects]             # (v1.1) who is working what, and overlaps
hird mem add <content> [--tags a,b] / hird mem search <query>
hird tui
hird mcp
hird db-path
```
Human CLI actions record events with `actor='cli'`.

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
