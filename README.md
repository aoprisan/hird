# hird

**A shared work queue and shared memory for AI coding agents, across harnesses.**

You run Claude Code in one terminal, Codex CLI in another, maybe Copilot in your
editor. They can't see each other's work, and nothing either of them learns
survives the session.

`hird` is one local binary that gives them a queue and a memory. You file the
work; you tell any agent in any harness *"pick up task 42"*; it claims the task,
works it, records what it learned, and marks it done. Everything lands in one
SQLite file, and a terminal UI shows you the board while it happens.

Or file a whole plan and say *"work the queue"* to all three at once. The queue
knows which tasks are blocked by unfinished dependencies and which ones would
put two agents in the same file, so it hands each agent something it can
actually do, and nothing that collides. Nobody assigns anything.

No daemon. No server. No accounts.

📖 **[Usage guide](https://aoprisan.github.io/hird/)** · 🧪 **[Examples](examples/)**

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
  │                └──── release ───────┘
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

**An agent that cannot finish hands the task back.** `task_release` returns it
to `open` immediately, which is not the same as `failed` — a failure is a
verdict on the task and waits for you to reopen it, a release just says this
agent stopped.

## Working the queue without assigning anything

Naming a task number works, but it does not scale past one agent. Give the
queue a plan instead, and let the agents take it apart themselves.

```sh
hird add "Design the storage schema"     --path 'src/db.rs'
hird add "Port the repository layer"     --path 'src/repo/**' --needs 1
hird add "Rewrite the renderer"          --path 'src/tui/**'
```

Now open three harnesses and tell each one the same thing: **"work the queue"**.

Each agent calls `task_next`, and the queue hands out the most important task
that is *actually workable* — open, with every dependency finished, and whose
files nobody else is inside. Task 1 and task 3 go out immediately, in parallel.
Task 2 waits, because the schema is not done yet. Nobody was assigned anything.

Automatic dispatch was added *alongside* dispatching by hand, not in place of
it. Nothing here pushes work at an agent: `task_next` is a tool an agent chooses
to call, so an agent you never tell to work the queue sits idle until you name a
number — and naming a number still works while a swarm is running, including for
tasks that were filed as part of a plan. The two ways mix, task by task.

```
$ hird graph
wave 1  (workable now)
  #1    in_progress  Design the storage schema
  #3    claimed      Rewrite the renderer
wave 2  (after wave 1)
  #2    open         Port the repository layer      waits for #1
```

### Dependencies are enforced, not annotated

A task whose dependencies are unfinished cannot be claimed at all, and the
refusal says what to do about it:

```
task 2 is blocked by task 1 (Design the storage schema, in_progress);
it becomes claimable once every dependency is done
```

Only `done` clears a blocker. A failed dependency keeps its dependents off the
ready list, because the work they were waiting for did not happen.

### Two agents, one file

The queue also knows which files each task expects to touch, so it can see the
one failure mode a status machine cannot: two agents editing the same file from
two harnesses, one of them about to lose their work.

Declarations are globs, not files, because nothing has been written yet — and
overlap is decided by asking whether *any path at all* is described by both
patterns. `src/*.rs` and `src/lib*` collide before `src/lib.rs` exists.

```
$ hird agents
codex:9f2c        #4 in_progress  Port the config loader        11m left
    files  src/config.rs, tests/config.rs
claude-code:af31  #7 claimed      Rewrite the loader tests      14m left
    files  tests/**
    !!     tests/** also claimed by codex:9f2c on #4
```

The agents see it too, the moment they say what they are about to change:

```json
{ "seq": 7, "paths": ["tests/**"],
  "overlaps": ["tests/** overlaps tests/config.rs on task 4 (Port the config loader), held by codex:9f2c"],
  "advice": "tell the human about the overlap before editing those files, …" }
```

By default `task_next` simply does not hand out work that would collide — with
several tasks to choose from there is no reason to pick the one that clashes.
Set `path_conflicts = "refuse"` to make an overlapping claim fail outright.

### Agents can split work for each other

An agent that finds its task is really three jobs does not have to do them in
sequence, or hand the problem back to you:

```
task_split { seq: 1, subtasks: [
  { title: "Write the migration",   paths: ["src/db.rs"] },
  { title: "Port the repositories", paths: ["src/repo/**"] },
] }
```

The pieces are filed as real tasks. Task 1 starts waiting for both and goes back
in the pool — blocked, so nobody can claim it early — and the agent that split it
is free to pick up something else. The other harnesses take a piece each, in
parallel, and task 1 becomes workable again the moment they are both done.

Pass `sequential: true` when the pieces genuinely have to happen in order.

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
                 [--needs <seq>,…] [--path <glob>]…
hird ls [--status <status>] [--all-projects]
hird show <seq>
hird cancel <seq> [--reason <text>]
hird reopen <seq> [--reason <text>]
hird dep add <seq> --needs <seq>,…
hird dep rm  <seq> --needs <seq>,…
hird graph [--all-projects]
hird scope <seq> [--path <glob>]… [--clear]
hird agents [--all-projects]
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

Three screens, `Tab` between them (`Shift-Tab` goes back). The board polls every
500 ms, so claims from other harnesses appear as they happen.

The **Swarm** screen is the one to watch while several agents are running: every
live agent, the files it has declared, an overlap line in red wherever two of
them are in the same territory, and — on the right — what is workable right now
and how much is queued behind it.

| Key | Anywhere |
|---|---|
| `Tab` | next screen (`Shift-Tab` for the previous) |
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

| Key | Swarm |
|---|---|
| `j` `k` | move between agents |
| `Enter` | open the task that agent is holding |

Cards on the queue board carry a yellow `waits #1 #3` badge when a task looks
open but nobody can actually claim it yet.

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

# What to do when a task declares files another agent is already working:
#   "report"  record it and tell the agent who else is in there  (default)
#   "refuse"  reject the claim outright, rolling back anything it took
path_conflicts = "report"

# Whether task_next passes over tasks whose files overlap live work. Default
# true: when the queue gets to choose, it should not choose a collision.
dispatch_avoids_conflicts = true
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

Twelve, and no more.

| Tool | Purpose |
|---|---|
| `task_list` | What work exists, optionally filtered by status. |
| `task_get` | One task in full: dependencies, file scope, recent history. |
| `task_next` | **Be handed the next workable task, already claimed.** |
| `task_claim` | Take a named task. Atomic; fails if someone else has it. |
| `task_scope` | Say which files you will change; find out who else is in them. |
| `task_update` | Record progress and renew the lease. Holder only. |
| `task_split` | Break a task into pieces the other agents can work. Holder only. |
| `task_complete` | Finish, with a summary. Holder only. |
| `task_fail` | Give up, with a reason. Holder only. |
| `task_release` | Hand the task back unfinished, still claimable. Holder only. |
| `mem_store` | Record one durable fact. |
| `mem_search` | Find facts recorded earlier, by anyone. |

Results are compact JSON. Failures come back as `isError` text rather than
protocol errors, so a model can relay them to you as-is instead of reporting
that a tool broke.

## Examples

[`examples/`](examples/) holds runnable versions of everything above. Each script
points `HIRD_DB` at a throwaway file, so running one cannot disturb your board.

```sh
./examples/manual-dispatch.sh   # file work, hand it out by number
./examples/swarm-plan.sh        # file a plan, three agents pull from it
```

They open real `hird mcp` sessions and send the tool calls a harness would,
because claiming and completing are agent-side operations with no CLI verb —
so the transcript shows exactly what "pick up task 42" looks like on the wire.
[`examples/harness/`](examples/harness) has drop-in MCP registration for Claude
Code, Codex CLI and VS Code.

## Documentation

The usage guide at **<https://aoprisan.github.io/hird/>** is a static site with
no build step: [`docs/`](docs/) is two files, published by
[`.github/workflows/pages.yml`](.github/workflows/pages.yml) on every push to
`main` that touches them. `just site` opens it locally, `just site-check` runs the
dead-link check CI runs, and `just examples` runs both example scripts end to
end.

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
the lease sweep by eight concurrent sweepers, self-dispatch by eight agents
calling `task_next` at the same instant and having to come away with eight
different tasks, and the status machine by a table-driven test asserting no
transition exists outside the diagram above. Pattern intersection is checked
against pattern matching over an exhaustive grid: whenever a concrete path
matches two patterns, those patterns must be reported as overlapping.
`tests/mcp_stdio.rs` spawns the real binary and speaks JSON-RPC to it, including
a test that a cold `hird mcp` is usable within the 50 ms budget a harness
expects — it starts a fresh one for every session.

## Design notes

`DESIGN.md` is the specification this was built from, kept as written.

`DESIGN.md` deliberately left out dependencies and automatic dispatch; both are
here now, along with file-scope collision detection, because a queue that
several agents work at once needs to know what is workable and what is in the
way. Still absent: multi-machine sync and vector search. The append-only event
trail is meant to make sync tractable later.

## Licence

MIT OR Apache-2.0.
