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

## 5. Queue semantics

### Status machine
```
open ──claim──► claimed ──start──► in_progress ──complete──► done
  ▲                │                    │        └─fail────► failed
  │                └── lease expiry ────┘
  └──────── reopen (human) ◄── done|failed|cancelled
open ──cancel (human)──► cancelled
```
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

## 6. MCP server (`hird mcp`)

stdio transport. Instructions string (returned in MCP initialize) must tell the model: how tasks are referenced (by `seq`), that it must claim before working, must call `task_update` at least every ~10 minutes to keep the lease, and should store assertions for durable facts it learns.

### Tools (exactly these eight)

| Tool | Input | Behavior |
|---|---|---|
| `task_list` | `status?`, `all_projects?` | List tasks (seq, title, status, holder, priority, updated_at). Runs lease sweep first. |
| `task_get` | `seq` | Full task incl. body, result, last 20 events. |
| `task_claim` | `seq` | Atomic claim as above. Returns full task body on success. |
| `task_update` | `seq`, `status?` (`in_progress` only), `note` | Holder-only. Appends `note`/`status` event, renews lease. |
| `task_complete` | `seq`, `result` | Holder-only. → `done`, clears lease, stores result. |
| `task_fail` | `seq`, `reason` | Holder-only. → `failed`, clears lease, stores reason. |
| `mem_store` | `content`, `tags?`, `task_seq?` | Insert assertion with actor + project provenance. |
| `mem_search` | `query`, `limit? (20)`, `all_projects?`, `include_superseded? (false)` | FTS5 `MATCH`; fall back to `LIKE` if the query fails FTS syntax. Results include id, content, tags, actor, created_at. |

Design rules: every tool result is compact JSON; errors are descriptive strings the model can relay verbatim ("task 42 is claimed by codex:9f2c until 14:32Z"). No tool mutates memory implicitly.

### Harness registration (document in README)
- Claude Code: `claude mcp add hird -e HIRD_HARNESS=claude-code -- hird mcp`
- Codex CLI: entry in `~/.codex/config.toml` `[mcp_servers.hird]` with `command = "hird"`, `args = ["mcp"]`, `env = { HIRD_HARNESS = "codex" }`
- Copilot / VS Code: `.vscode/mcp.json` stdio server, `HIRD_HARNESS=copilot`.

## 7. CLI

```
hird add <title> [--body <md>|--body-file <path>] [--priority N] [--project <path>]   → prints seq
hird ls [--status s] [--all-projects]
hird show <seq>
hird cancel <seq> / hird reopen <seq>
hird mem add <content> [--tags a,b] / hird mem search <query>
hird tui
hird mcp
hird db-path
```
Human CLI actions record events with `actor='cli'`.

## 8. TUI (`hird tui`)

ratatui + crossterm. Poll the DB every 500 ms (cheap queries; WAL makes this safe). Two screens, `Tab` to switch, `q` quits, `?` help overlay.

**Screen 1 — Queue board.** Kanban columns: Open | Claimed/In-progress | Done | Failed/Cancelled. Cards show `#seq title`, holder badge (`claude-code`, `codex`, `copilot` with distinct colors), lease countdown for claimed tasks, priority marker. Keys: `j/k` move, `h/l` columns, `Enter` detail pane (body + event history), `a` add task (title prompt; body optional), `c` cancel, `r` reopen, `/` filter by text, `p` toggle project filter (current/all).

**Screen 2 — Memory browser.** Top: search input (FTS query, live). Below: assertion list — content, tags, actor badge, age, linked task seq. `Enter` shows full assertion + provenance. `d` marks superseded (sets `superseded_by` to a tombstone assertion authored by `tui`). `p` project filter toggle.

Status bar on both screens: DB path, project filter, counts by status, last-poll age.

## 9. Future (do not build, do not preclude)

- Multi-machine: the append-only `task_events` + assertion model is CRDT-friendly; a later `hird sync` could ship via S3 like ccsync. Keep all mutations expressible as events.
- Task dependencies (`blocked_by`), auto-dispatch, embeddings for memory, HTTP mode for remote harnesses.

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
