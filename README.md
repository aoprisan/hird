# hird

**A shared work queue and shared memory for AI coding agents, across harnesses.**

You run Claude Code in one terminal, Codex CLI in another, maybe Copilot in your
editor. They can't see each other's work, and nothing either of them learns
survives the session.

`hird` is one local binary that gives them a queue and a memory. You file the
work; you tell any agent in any harness *"pick up task 42"*; it claims the task,
works it, records what it learned, and marks it done. Everything lands in one
SQLite file, and a terminal UI shows you the board while it happens.

No daemon. No server. No accounts.

```
┌────────────┐  ┌────────────┐  ┌────────────┐
│ Claude Code│  │  Codex CLI │  │  Copilot   │
└─────┬──────┘  └─────┬──────┘  └─────┬──────┘
      │ stdio MCP     │ stdio MCP     │ stdio MCP
┌─────▼──────┐  ┌─────▼──────┐  ┌─────▼──────┐
│ hird mcp   │  │ hird mcp   │  │ hird mcp   │   one process per session
└─────┬──────┘  └─────┬──────┘  └─────┬──────┘
      └───────────────┼────────────────┘
                ┌─────▼──────┐        ┌──────────┐
                │  SQLite    │◄───────┤ hird tui │
                │  (WAL)     │◄───────┤ hird CLI │
                └────────────┘        └──────────┘
```

## Install

```sh
cargo install --path .
```

Needs a Rust toolchain. SQLite is compiled in — there is nothing else to
install and nothing to configure before first use.

## Try it in two minutes

```sh
hird add "Port the config loader to serde" \
        --body "Keep the env-var precedence. Tests in tests/config.rs must pass."
# → 1

hird ls
# #1  open         Port the config loader to serde
```

Now register `hird` with a harness (below), open a session there, and say:

> pick up task 1

The agent calls `task_claim`, gets the full body back, works it, and calls
`task_complete`. Meanwhile:

```sh
hird tui     # watch the board live
hird show 1  # or read the history after the fact
```

## Registering with a harness

Each harness runs its own `hird mcp` process. The only thing they need to
differ on is `HIRD_HARNESS`, which is how agents and the board tell each other
apart (`claude-code:af31`, `codex:9f2c`).

**Claude Code**

```sh
claude mcp add hird -e HIRD_HARNESS=claude-code -- hird mcp
```

**Codex CLI** — in `~/.codex/config.toml`:

```toml
[mcp_servers.hird]
command = "hird"
args = ["mcp"]
env = { HIRD_HARNESS = "codex" }
```

**Copilot / VS Code** — in `.vscode/mcp.json`:

```json
{
  "servers": {
    "hird": {
      "type": "stdio",
      "command": "hird",
      "args": ["mcp"],
      "env": { "HIRD_HARNESS": "copilot" }
    }
  }
}
```

Any MCP-capable harness works the same way: run `hird mcp` over stdio and set
`HIRD_HARNESS` to something recognisable.

## How the queue behaves

```
open ──claim──► claimed ──start──► in_progress ──complete──► done
  ▲                │                    │        └─fail────► failed
  │                └── lease expiry ────┘
  └──────── reopen (human) ◄── done|failed|cancelled
open ──cancel (human)──► cancelled
```

**Claiming is atomic.** It is a single compare-and-set, so if two agents reach
for the same task at the same moment exactly one wins. The loser gets a sentence
it can repeat to you verbatim:

```
task 42 is claimed by codex:9f2c until 2026-07-25T14:32:00.000Z
```

**Claims are leases, not locks.** A claim lasts 15 minutes by default and every
`task_update` renews it. If an agent crashes, gets killed, or simply wanders
off, the lease lapses and the task returns to `open` on its own — no stuck
tasks, no manual cleanup, no background process. Expiry is enforced lazily by
whoever reads the table next, which is usually the TUI half a second later.

**Only the lease holder can drive a task.** Another agent cannot complete,
fail, or update work it does not hold. You can always `cancel` or `reopen`
anything from the CLI or the TUI.

## Memory

The queue is for work; memory is for what the work taught you. Agents call
`mem_store` with one plain-prose fact — where something lives, why a decision
went the way it did, which command actually works — and `mem_search` finds it
later, from any harness, in any session.

Every assertion carries provenance: who asserted it, when, and which task they
were working at the time. Facts are never edited or deleted; a fact that stops
being true is *superseded* by a new one, and the old version stays searchable
behind `--include-superseded`.

```sh
hird mem add "Integration tests need HIRD_DB set or they touch the real db" --tags testing
hird mem search testing
```

Search is SQLite FTS5. If a query isn't valid FTS syntax — an agent typing
`handle_event(ctx)` — it degrades to a term-wise substring match instead of
erroring.

## Command line

```
hird add <title> [--body <md>|--body-file <path>] [--priority N] [--project <path>]
hird ls [--status <status>] [--all-projects]
hird show <seq>
hird cancel <seq> [--reason <text>]
hird reopen <seq> [--reason <text>]
hird mem add <content> [--tags a,b] [--task <seq>]
hird mem search [query] [--limit N] [--all-projects] [--include-superseded]
hird tui
hird mcp
hird db-path
```

`--body-file -` reads the task body from stdin, which is handy for piping a
plan straight into the queue.

## The TUI

```sh
hird tui
```

Two screens, `Tab` between them. The board polls every 500 ms, so claims from
other harnesses appear as they happen.

| Key | Anywhere |
|---|---|
| `Tab` | switch between the queue and memory |
| `p` | toggle project filter (current / all) |
| `/` | filter or search |
| `?` | help |
| `q` | quit |

| Key | Queue board |
|---|---|
| `j` `k` | move down / up |
| `h` `l` | previous / next column |
| `g` `G` | first / last card |
| `Enter` | open the task, its history and what was learned |
| `a` | add a task (`Tab` for the body, `Enter` to save) |
| `c` `r` | cancel / reopen the selected task |

| Key | Memory browser |
|---|---|
| `j` `k` | move down / up |
| `Enter` | show the assertion and its provenance |
| `d` | supersede it with something truer |
| `s` | show or hide superseded assertions |

## Projects

Every task and assertion is filed under a project — the canonical path of your
repository root, found by walking up to the nearest `.git`. Two harnesses opened
in the same checkout share a queue; a different checkout gets its own. One
database holds them all, and `--all-projects` (or `p` in the TUI) looks across
everything.

Override detection with `HIRD_PROJECT` when you need to.

## Configuration

The database lives at `${XDG_DATA_HOME:-~/.local/share}/hird/hird.db`. Point
`hird` somewhere else with `--db` (highest precedence) or `HIRD_DB`.

Optional config at `${XDG_CONFIG_HOME:-~/.config}/hird/config.toml`:

```toml
# How long a claim survives without a task_update. Default 15.
lease_ttl_minutes = 15

# Whether list and search span every project by default. Default false.
# Explicit --all-projects / all_projects arguments still win.
all_projects_by_default = false
```

Agents are told the configured TTL in the MCP handshake and asked to check in at
half that interval, so raising it here is all that's needed to give slow tasks
more room.

| Variable | Meaning |
|---|---|
| `HIRD_HARNESS` | This session's harness name. Set it in the MCP registration. |
| `HIRD_PROJECT` | Override project detection. |
| `HIRD_DB` | Override the database path. |

## MCP tools

Eight, and no more.

| Tool | Purpose |
|---|---|
| `task_list` | What work exists, optionally filtered by status. |
| `task_get` | One task in full, with its recent history. |
| `task_claim` | Take an open task. Atomic; fails if someone else has it. |
| `task_update` | Record progress and renew the lease. Holder only. |
| `task_complete` | Finish, with a summary. Holder only. |
| `task_fail` | Give up, with a reason. Holder only. |
| `mem_store` | Record one durable fact. |
| `mem_search` | Find facts recorded earlier, by anyone. |

Results are compact JSON. Failures come back as `isError` text rather than
protocol errors, so a model can relay them to you as-is instead of reporting
that a tool broke.

## Development

```sh
just          # list recipes
just check    # fmt --check, clippy -D warnings, and the full test suite
just test
just install
```

Layering, from the bottom up:

| Module | Responsibility |
|---|---|
| `model` | Domain types and the status machine |
| `db` | Connection setup and schema migrations |
| `repo` | **The only place SQL is written** |
| `mcp` `cli` `tui` | The three front ends, which call `repo` |

The rules that matter are pinned by tests rather than convention: the claim
compare-and-set is exercised by sixteen threads racing on separate connections,
the lease sweep by eight concurrent sweepers, and the status machine by a
table-driven test asserting no transition exists outside the diagram above.
`tests/mcp_stdio.rs` spawns the real binary and speaks JSON-RPC to it, including
a test that a cold `hird mcp` is usable within the 50 ms budget a harness
expects — it starts a fresh one for every session.

## Design notes

`DESIGN.md` is the specification this was built from, kept as written.

Deliberately absent in v1: multi-machine sync, task dependencies, automatic
dispatch, and vector search. The append-only event trail is meant to make sync
tractable later; the rest can wait until the basics have earned their keep.

## Licence

MIT OR Apache-2.0.
