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

What the work teaches gets written down, and the next agent to touch those files
is handed it without having to know to ask.

And because a board built entirely out of what agents say about themselves has
one blind spot, `hird` also watches the working tree — so when a file two agents
both declared moves under both of them, they hear about it while there is still
time to re-read it, instead of at merge time.

The same look at the tree is what keeps the memory honest. A fact is recorded
against the files it was read off, so when that code is rewritten the fact
arrives marked *unverified* rather than arriving looking exactly like one
learned this morning. Confirm it and it goes back to standing; the way to
confirm it is to say it again.

And because the point of running three different models is that they are not
the same model, work can be marked for review: finishing it files a review of
exactly what changed, and the queue refuses that review to the harness that did
the changing. No agent gets to be the last word on its own work — and the
review is not the last word either, because it ends in a verdict the queue
acts on: work that is *sent back* reopens carrying the reviewer's findings,
the redo files a fresh review, and the loop runs until one is *upheld*, with
you nowhere in the transport. Every verdict lands on a record, so `hird
record` can tell you whose work survives a reading by a different model.

And when a task outlives a holder — a lease expires mid-work, work is handed
back, a review sends it back — the queue archives what each holding did
instead of forgetting it. The next agent's claim names whose uncommitted
leavings are in its tree, and every earlier round stays diffable after the
redo has written over it.

All of that is pull: agents ask, hird answers, and a task that becomes ready
while nobody is asking waits in silence. One config key closes that seam
without a daemon. Set `dispatch_hook` to a command and hird runs it, detached,
the moment a task becomes claimable — which task, why, whom the queue would
refuse it to, and any capabilities its worker must have, in its environment.
Point it at anything that can wake an
agent — under [herdr](https://herdr.dev), `herdr agent prompt worker "work
the hird queue"` — and the plan's next wave, the freshly filed review, the
work a verdict sent back, each arrives at an idle agent with nobody carrying
it there. And because a filed review is announced with its author's harness
in `HIRD_RECUSED`, one `case` in the hook routes the summons to a different
agent than the one the claim would turn away — the review loop runs on a
swarm of two without ever knocking on the wrong door. That pairing also
ships packaged, as a herdr plugin — `herdr plugin install
aoprisan/hird/herdr-plugin` wires the hook, keeps a worker roster, and puts
the board and the live feed a keypress away; see
[herdr-plugin/](herdr-plugin/).

No daemon. No server. No accounts.

- 📖 **Documentation: [aoprisan.github.io/hird](https://aoprisan.github.io/hird/)**
  — install, dispatching, file scope,
  [recall](#recall-the-task-arrives-knowing-things), the TUI, every
  configuration key and an FAQ.
- 🧪 **[Examples](examples/)** — runnable versions of everything in the guide.

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
curl -fsSL https://raw.githubusercontent.com/aoprisan/hird/main/scripts/get.sh | sh
```

No Rust toolchain: this downloads the [release](https://github.com/aoprisan/hird/releases)
binary for your platform (Linux x86_64/aarch64, macOS Intel/Apple silicon —
the Linux ones are static musl builds that run on any distribution), verifies
it against the release's checksum, and installs it as `~/.local/bin/hird`.
Append `-s -- --install-skill` to the `sh` to also install the agent skill,
and set `HIRD_VERSION=v0.1.0` to pin a release instead of taking the latest.

With a Rust toolchain, `cargo install hird --locked` gets the same binary
from crates.io, and from a checkout:

```sh
./scripts/install.sh --install-skill
```

builds the release binary, copies that standalone snapshot to
`~/.local/bin/hird`, then removes the release build artifacts. Re-run the
script after upgrading the source.

Whichever way it arrived, `--install-skill` installs the bundled,
agent-portable skill for Codex and OpenCode (`~/.agents/skills/hird`), Claude
Code (`~/.claude/skills/hird`), and GitHub Copilot (`~/.copilot/skills/hird`).
It is optional, can be run separately (`hird --install-skill`), and new agent
sessions should be started after installing it.

SQLite is compiled in — there is nothing else to install and nothing to
configure before first use.

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

`hird register` writes that for you, into the one file the harness in question
actually reads:

```sh
hird register claude-code    # ./.mcp.json
hird register codex          # ~/.codex/config.toml
hird register copilot        # ./.vscode/mcp.json — Copilot in VS Code
hird register copilot-cli    # ~/.copilot/mcp-config.json
hird register opencode       # ~/.config/opencode/opencode.json
```

```
registered hird in /home/you/project/.vscode/mcp.json
  command  /home/you/.cargo/bin/hird mcp
  env      HIRD_HARNESS=copilot
next: in VS Code: MCP: List Servers → hird → Start Server, then tick hird in
      the agent-mode tools picker
```

The `command` it writes is the absolute path of the binary doing the writing,
because that is the thing hand-written configs get wrong: a bare `hird`
resolves against the harness's `PATH`, and a GUI editor's is not your shell's.

Register as many harnesses as you run — that is the point — and run it again
whenever you move the binary. Registering twice changes nothing and says so.
An entry you have since edited by hand is refused rather than overwritten,
with `--force` to say you meant it. `--print` writes nothing and shows what it
would have written, for a config this cannot safely edit — one with comments
in it, or a harness hird has never heard of.

A second registration on a scratch database, alongside the real one:

```sh
hird register codex --name hird-scratch --db /tmp/scratch/hird.db
```

### What it writes

**Claude Code** — `.mcp.json`, project-scoped, so it belongs to the checkout
rather than to you. `claude mcp add hird -e HIRD_HARNESS=claude-code -- hird
mcp` is the user-wide equivalent.

**Codex CLI** — appended to `~/.codex/config.toml`, comments and all left
alone:

```toml
[mcp_servers.hird]
command = "/home/you/.cargo/bin/hird"
args = ["mcp"]
env = { HIRD_HARNESS = "codex" }
```

**Copilot in VS Code** — `.vscode/mcp.json`:

```json
{
  "servers": {
    "hird": {
      "type": "stdio",
      "command": "/home/you/.cargo/bin/hird",
      "args": ["mcp"],
      "env": { "HIRD_HARNESS": "copilot" }
    }
  }
}
```

Writing this does not start it. **MCP: List Servers** → `hird` → **Start
Server**, and tick `hird` in the tools picker of the agent-mode chat box: VS
Code does not launch a newly written server on its own, and a stopped or
unticked server looks exactly like one that was never registered.

**Copilot CLI** — `~/.copilot/mcp-config.json`, which `/mcp add` inside a
session also writes:

```json
{
  "mcpServers": {
    "hird": {
      "type": "local",
      "command": "/home/you/.cargo/bin/hird",
      "args": ["mcp"],
      "env": { "HIRD_HARNESS": "copilot" },
      "tools": ["*"]
    }
  }
}
```

**OpenCode** — `${XDG_CONFIG_HOME:-~/.config}/opencode/opencode.json`:

```json
{
  "mcp": {
    "hird": {
      "type": "local",
      "command": ["/home/you/.cargo/bin/hird", "mcp"],
      "environment": { "HIRD_HARNESS": "opencode" }
    }
  }
}
```

If `opencode.jsonc` already exists and `opencode.json` does not, hird uses the
JSONC path. A file containing comments is left untouched; `--print` renders
the block to merge by hand. Restart OpenCode after registering, and
`opencode mcp list` confirms the connection.

Any other MCP-capable harness works the same way: run `hird mcp` over stdio and
set `HIRD_HARNESS` to something recognisable. `hird register copilot --print`
is a reasonable starting point for one hird has no entry for.

### The protocol

`hird mcp` speaks MCP **2026-07-28**, and every earlier revision back to
2024-11-05. Which one a session uses is the harness's choice, not something to
configure — hird answers whichever it is asked for, and says so by name when
asked for one it does not have.

2026-07-28 removes the `initialize` handshake: a client opens with
`server/discover` or simply with the request it wanted to make, and carries the
protocol version, its own name and its capabilities in `_meta` on each one.
Nothing hird does depended on that handshake — it is one process per session
over a pipe, which is about as stateless as a session gets — so a harness on
the new lifecycle and one still handshaking share a queue without noticing each
other's era. The one visible difference is that a request that claims
2026-07-28 without carrying what 2026-07-28 requires is refused as a bad
request, in a sentence naming what was missing, rather than served on a guess
about who sent it.

The revision also means a client now names itself on every call, which hird
uses for exactly one thing: **a harness that never set `HIRD_HARNESS` is filed
under the name its client gives, instead of `unknown`.** `HIRD_HARNESS` still
wins wherever it is set — it is the half of the identity you control, and
`hird register` writes it — and the name is taken once and then held for the
life of the session, because an actor that changed its name halfway through
would lose track of its own claims. What hird has never used, and does not
start using now, are the three features this revision deprecates: no roots, no
sampling, no protocol logging.

### When the agent says it has no hird tools

A skill, a prompt file or a `copilot-instructions.md` that talks about
`task_claim` is not a connection. It tells an agent what to do with the tools;
it cannot hand it any. Registering the server is a separate step, and it is the
one above. If the agent reports no `task_*` tools, work down this list.

**Is the server registered where that harness reads?** Each registration above
belongs to exactly one harness. Copilot in VS Code does not read
`~/.copilot/mcp-config.json`, and the Copilot CLI does not read
`.vscode/mcp.json`. `hird register <harness>` picks the file for you; run it
again and it will say `already registered` against the path it went to.

**Can the harness spawn `hird`?** `command: "hird"` resolves against the
harness's `PATH`, not your shell's. A GUI VS Code launched from Finder or a
dock has neither `~/.cargo/bin` nor anything else your shell profile adds, so
the spawn fails and the server dies before it is ever spoken to. This is what `hird
register` exists to get right; in a config written by hand, paste the absolute
path instead:

```sh
which hird     # → /Users/you/.cargo/bin/hird
```

**What did the server say?** Every harness keeps the stdio server's output.
In VS Code it is **MCP: List Servers** → `hird` → **Show Output**; in Claude
Code, `claude mcp list`. A `hird` that starts prints nothing and waits, so an
empty log is the healthy case and `command not found` is the usual one.

**Is it the cloud coding agent?** The Copilot coding agent on github.com runs
in an ephemeral container with no access to your machine. hird is a local
queue in a local SQLite file, so there is nothing there for it to connect to —
register hird in an editor or CLI that runs on the same machine as the
database (`hird db-path`).

Confirm the binary works at all before blaming the wiring: `hird ls` from a
terminal exercises the same database over the same code the server does.

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

**An agent that needs your answer stops the churn.** The same call may carry a
`question`. The task returns to `open`, but it is not claimable and
`task_next` routes around it instead of handing an unanswerable task from
agent to agent:

```json
task_release { "seq": 42,
  "reason": "the compatibility branch is isolated",
  "question": "Must the old config format remain readable?" }
```

The board marks it `awaits answer`. Answer from the CLI (`hird answer 42
"Yes; keep it for one release"`) or press `A` on its TUI card. The answer makes
the task claimable, wakes the dispatch hook with `HIRD_EVENT=answered`, and
rides in the next claim beside the question, so the next agent cannot miss the
decision that resumed its work. An ordinary release without `question` remains
immediately claimable.

Nothing summons you, because a question is exactly the stall no agent can
clear — so it is the one thing on the board that waits on you noticing.
`hird ls` marks the row, `hird show 42` prints the question and the command
that answers it, and the TUI counts them in its status bar (`· 2 awaiting
you`) so a card in a column you are not looking at still says so.

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

The edge is also a context channel: when the blockers are done and the
dependent is claimed, the claim arrives carrying each blocker's own result —
see [the ground a task builds on](#the-ground-a-task-builds-on).

### Work knows what its worker must have

Harnesses are not interchangeable. One may have a browser, another network
access or deployment credentials, and a third may be the only session running
on macOS. Put those hard constraints on the work instead of discovering them
after the wrong agent takes a lease:

```sh
hird add "Verify the responsive UI" --path 'frontend/**' --requires browser
hird register codex --capability network
hird register claude-code --capability browser --capability network
```

The registration writes `HIRD_CAPABILITIES`; labels are lowercase free-form
tokens such as `browser`, `network`, `macos` or `deploy.staging`. A named claim
from an unequipped session is refused before it writes anything:

```
task 7 requires browser, but this session advertises network; start a capable
harness or update HIRD_CAPABILITIES
```

`task_next` routes around the mismatch and reports skipped tasks under
`incompatible`, alongside blocked, contended and recused work. This does not
make hird an agent router: the queue knows what the task requires and what this
caller advertises, but it has no roster and chooses nobody. The external
dispatch hook receives the same requirements as `HIRD_REQUIRES` and remains
the place that maps labels to workers. Reviews inherit the work's requirements,
because checking browser-only work generally requires a browser too; use
`hird require <seq> --clear` when a particular review only needs its diff.

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

### The plan can be a file

Three `hird add` calls are fine. A plan worth reviewing before you point three
agents at it is worth writing down, and a plan you will edit is worth being able
to run twice:

```toml
# plan.toml
plan = "serde-migration"

[[task]]
name = "schema"
title = "Design the storage schema"
priority = 3
paths = ["src/db.rs"]

[[task]]
name = "repos"
title = "Port the repository layer"
paths = ["src/repo/**"]
needs = ["schema"]
```

```sh
hird plan apply plan.toml --dry-run   # read it first
hird plan apply plan.toml             # file the whole graph in one transaction
```

Tasks carry names instead of numbers, so the file means the same thing in a
fresh database as in one holding two hundred tasks, and it can live in the
repository beside the code it describes. Each task is remembered under the name
it was filed with, so applying an edited plan files only what the file has
gained — the tasks already in flight keep their claims, their history and their
numbers.

What a plan may say is exactly what the queue stores about a task: a title, a
body, priority, files, dependencies, required capabilities and whether to
review it. There are no
conditionals, no loops and no retries, and that is not an omission to be filled
in later — `hird` hands work out because an agent asked for it, and a file that
could say *when* to run something would be describing a scheduler this queue
deliberately does not have.

**`--dry-run` is the reason the file earns its place.** It lays the plan out in
the waves `hird graph` will print, and then tells you the thing the queue could
not have told you until the work was already live:

```
$ hird plan apply plan.toml --dry-run
plan "serde-migration" — 5 tasks, 3 waves, at most 3 at once

wave 1  (workable now)
  new   schema         Design the storage schema
      files  src/db.rs
  new   renderer       Rewrite the renderer
      files  src/tui/**
  new   audit          Audit the renderer tests
      files  src/tui/view.rs
…

same files, nothing ordering them — the queue hands these out one at a time,
so the waves above are wider than the work really is
  renderer and audit — src/tui/** overlaps src/tui/view.rs

declaring no files: notes
  the queue cannot keep another agent out of what these touch, and what
  earlier work learned reaches them by title alone

nothing was written; drop --dry-run to file it
```

Two globs can be intersected before either file exists, so a pair that *looks*
parallel and will in fact be handed out one at a time is knowable while the plan
is still a file. Wave 1 above says three; the work says two and a queue.

Filing a plan is a human act, which is why there is no MCP tool for it: the
review is the point, and an agent that could apply its own plan unread would
have removed the only step this feature exists for. An agent that finds its
task is really three jobs has `task_split` instead.

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

## What actually happened

Everything above is what an agent *said*. A task's status is its holder's claim
about itself, its file scope is its holder's prediction, and its result is a
sentence the holder wrote about its own work. That is fine right up until three
agents share one checkout, where the failure that costs you work is the one
nobody reports.

Play the collision out to the end. Two agents declare `src/config.rs`. Both are
told about the overlap. Both decide their part is small. Both edit. The second
write lands on the first, both tasks complete, the board goes green — and git
has nothing to show you, because nothing was committed and there is only ever
one version of the file on disk. Not one of the three participants is in a
position to notice.

So `hird` goes and looks. A claim fingerprints the repository, every check-in
takes another fingerprint and subtracts, and the results say what moved:

```json
{ "seq": 1, "status": "in_progress", "note_recorded": "loader ported",
  "changed": ["src/config.rs (modified)", "src/mcp.rs (added)"] }
```

That list did not come from the agent. It came off the disk.

### The overlap that has stopped being hypothetical

What the working tree cannot tell you is *who* — one filesystem, no keyboards —
and `hird` does not pretend it can. What it has instead is the other half of the
picture, which it was already collecting: a declared scope is an agent saying it
holds a copy of a file and means to write from it.

> A **contention** is a file two live agents both declared, which has since
> moved under both of them, and which the two of them disagree about.

All three clauses earn their place. Both declaring is the attribution the
filesystem cannot supply. Having moved is what makes it an event rather than a
forecast — the plain overlap is already on the board. Disagreeing is what makes
it worth interrupting an agent over: two agents who have both been shown the
current file are not in trouble yet, and warning them anyway is how a warning
stops being read.

When all three hold, both sides hear about it on their next call, by name:

```json
{ "contended": [
    "src/config.rs changed under task 2 (Audit the config loader), held by
     claude-code:af31 at 14:36 UTC, at or after the version hird last confirmed
     for you — re-read it before you write, or your edit will discard theirs"],
  "advice": "a file you declared has changed under another agent that declared
             it too — re-read it before your next write, and tell the human" }
```

Re-reading the file is the whole fix, and it is the one thing neither agent
would have thought to do.

### Drift

An agent that wanders outside what it declared is told so, because everyone
else's collision check is reading the declaration and not the edits:

```json
{ "undeclared": ["src/mcp.rs"],
  "advice": "src/mcp.rs moved while you held this task and nobody has declared
             it. If the edit was yours, call task_scope so the other agents can
             see it; if it was not, another agent is working outside what it
             declared" }
```

The conditional is deliberate. Alone in a project, a file that moved moved
because of you. With another agent live, `hird` watched the file and not the
keyboard, and it says so rather than guessing.

### Did it change anything, or did it only read?

The same fingerprints answer a blunter question, and it is the one you ask first
when a task comes back done: did this work touch the code at all?

```sh
$ hird ls
#1  done         Port the config loader   modified 2 files
#2  done         Audit the config loader  read-only
#3  in_progress  Write the release notes  [codex:9f2c] 11m left  read-only so far
```

An investigation that changed nothing and a refactor that rewrote half the
module are the same green card otherwise, and the difference decides whether
there is anything to review, anything to test, or anything to undo.

Three answers, and the third one matters as much as the other two:

- **`modified N files`** — the tree differs from the fingerprint taken when the
  task was claimed. Where another agent was live in one of those files, the
  line says so, because one checkout has no keyboards.
- **`read-only`** — hird was watching, and nothing moved. On a task still being
  worked it reads `read-only so far`: a running total, not a verdict.
- **nothing at all** — hird was not watching, so it has no opinion. Saying
  "read-only" here would be inventing evidence, and the whole point of the
  witness is that it does not.

Agents are told the same thing in the same words, on every check-in and on the
call that finishes the work:

```json
{ "seq": 2, "status": "done", "result": "read it; the precedence is already right",
  "footprint": "read-only — nothing in the working tree moved while it was held" }
```

### On the board

```
$ hird agents
codex:9f2c        #1 in_progress  Port the config loader        11m left
    files  src/config.rs
    moved  src/config.rs, src/mcp.rs
    !!     src/config.rs also claimed by claude-code:af31 on #2
    !!!    src/config.rs changed under task 2 (Audit the config loader), held by
           claude-code:af31 at 14:36 UTC … re-read it before you write
```

`files` is what was announced; `moved` is what happened — or `moved  nothing
yet` where an agent has been in the code and written none of it — and the gap
between the two lines is the point. `hird show` carries the same record, headed
by the one-line answer:

```
$ hird show 1
…
changed   modified 2 files, though another agent was live in some of them
          src/config.rs (modified)
          src/mcp.rs (added)
```

That is the evidence behind a finished task's result line, and it was not
written by the agent that wrote it. The TUI's Swarm screen shows both, and
titles the pane with how many agents are standing in a file that is moving
under them; the queue board badges every card with what it did to the tree.

### The witness keeps what it saw

Detection is half an answer. The witness could tell you `src/config.rs` moved
under task 1; it could not show you the change, and when a second agent's
write landed on the first's uncommitted work it could name the loss but not
undo it. So every file version the witness fingerprints is also **kept** —
content-addressed, deduplicated, in the same SQLite file as everything else —
and three things read it back.

```
$ hird diff 1
diff --git a/src/config.rs b/src/config.rs
--- a/src/config.rs
+++ b/src/config.rs
@@ -1 +1 @@
-fn load() {}
+fn load() { ported() }
```

That is the uncommitted diff of what actually moved under task 1 — while the
work is live, after it is done, and after the tree has moved on to something
else. Git remembers what was committed; the exhibit remembers what happened
between commits, which is where agents live. `--path` narrows it to one file.

The review a completion files is the second reader: its brief carries the
diff of the work under judgement, clipped to a size a context window can
afford and pointing at `hird diff` for the rest — so the reviewing agent
reads the change itself instead of reconstructing it from file names.

```
$ hird salvage 1 src/config.rs --out recovered.rs
salvaged src/config.rs as the witness last saw it under the task — 24 bytes into recovered.rs
```

And `salvage` is the third: the last version the witness saw of a file under
a task, which is exactly the version another agent's write landed on in the
collision the witness was built to catch. `--baseline` asks instead for the
version the task started from. The name is the honest limit — a version that
came and went between two observations was never seen, and hird will say a
version is not kept rather than guess at one.

Kept versions are pruned once nothing references them and they have aged out,
so the store grows with what actually changed, not with time.
`exhibit = false` keeps the watching and gives up the keeping.

### A task remembers every hand that held it

An agent claims a task, edits two files, and dies — session killed, laptop
closed, context burned out. The lease lapses and the task goes back in the
pool. Whoever picks it up next starts on a working tree carrying a vanished
agent's uncommitted edits, looking exactly like code that was always there —
and the record of that first attempt used to be destroyed by the successor's
own claim, at the moment it mattered most.

Now every holding of a task is archived as a **tenure** — who held it, how
the holding ended, what moved while they had it — and the successor's claim
answer says so, unasked:

```json
{ "claimed": 7,
  "previously": "codex:9f2c went quiet until the lease expired, and these files moved
    while they held it: src/config.rs (modified), src/loader.rs (added). Whatever state
    they left is part of the tree this claim starts from — `hird diff 7 --tenure 1`
    shows that round's changes before you build on or over them" }
```

`hird show` numbers the rounds (`held  round 1: codex:9f2c — lease expired;
saw src/config.rs (modified)`), and `hird diff --tenure` and `hird salvage
--tenure` keep every earlier round readable and recoverable after the redo
has written over all of it. A review that sends work back (see the verdict)
gets the same benefit for free: the redo no longer erases the evidence of the
round the reviewer judged.

### It is never in the way

Watching needs git, and a project without it is not a degraded project — every
tool answers exactly as it did before, and agents are not told about a field
nothing will ever fill. A sweep that fails costs a report nobody asked for,
never the call that was actually made. `.gitignore` decides what counts as
noise, so build output is free. And with no task in the project holding a lease
there is nobody to measure, so git is not run at all.

Measured on this repository: about 8 ms for a claim and 6 ms for a check-in,
against 0.5 ms for a call that never looks — three git subprocesses, against a
heartbeat measured in minutes. `witness = false` turns it off.

## No agent reviews its own work

Every result line in the queue was written by the agent that did the work. It is
the last word, and nobody else ever looks. With one agent that is not a choice.
With three it is a waste of the only thing that makes running three worth doing.

Every harness can review code already. None of them can tell *whose* code it is
looking at — a harness cannot see another harness's session, and an agent handed
"review this" has no way to know it wrote it an hour ago. hird can: it is the
process all of them talk to, and it wrote down who held the lease.

```sh
hird add "Port the config loader" --review --path src/config.rs
```

That is the whole opt-in, and it says nothing about who or when. Codex claims
it, works it, and finishes the way it always does:

```
task_complete { seq: 1, result: "ported; env still wins over the file" }

{ "seq": 1, "status": "done", "changed": ["src/config.rs (modified)"],
  "review_filed": 2,
  "advice": "this work was marked for review, so task 2 is now open for an agent
             in another harness — you cannot take it yourself. Tell the human." }
```

Task 2 was filed by the completion. It is titled after the work, scoped to the
file the **witness saw move** rather than the one anybody declared, and its body
carries codex's own summary marked as the thing under review rather than as the
brief — along with [the diff of the change itself](#the-witness-keeps-what-it-saw),
so the reviewer reads the work and not a reconstruction of it. Then:

```
task_next {}                       # asked by codex

{ "idle": "1 task is ready, but every one of them is a review of work this
           harness did — they need an agent in another harness. Tell the human;
           waiting will not change it",
  "recused": [{ "seq": 2, "why": "not whoever worked task 1 (Port the config
                loader): that was codex:43em, so this needs another harness" }] }
```

Not "nothing to do". The queue is not idle — it is waiting for a different tool,
and an agent told the queue was empty would send you away from the one thing
that needs you. Claiming it by name is refused too, in the same transaction as
the compare-and-set, so there is no race to win.

**The bar is the harness, not the session.** Two Claude Code windows are one
model reading its own homework; a bar you could get around by opening a tab
would be no bar at all.

**It is a constraint, not a scheduler.** A recusal says who may *not* take a
task and never who must. Run one harness and the review simply sits there
unclaimable — which the board says plainly, because that is a fact about your
setup rather than something to paper over.

You can set the bar by hand, and lift it:

```sh
hird recuse 7 --from 4 --reason "wrote the thing being checked"
hird recuse 7 --clear
```

And a plan file is where the judgement belongs, since which work deserves a
second pair of eyes is a property of the job:

```toml
[[task]]
name = "renderer"
title = "Rewrite the renderer"
paths = ["src/tui/**"]
review = true
```

Nothing is filed for work the witness saw no trace of, for work that `failed` —
there is nothing to check, and whether the attempt is worth reading is your
call — or while an earlier review of the same work is still open.

## The review closes its own loop

Getting the work in front of a second harness is half the job. The other half
used to be yours: read the review, decide "the error path drops the lock"
means *broken*, find the task it reviewed, reopen it, paste the findings
somewhere. Every other hand-off in hird files itself; the one carrying the
judgment was left by hand.

So a review ends in a **verdict**, and completing one without it is refused:

```
task_complete { seq: 2, result: "the file path still wins when both are set;
                                 invert the precedence in load()",
                verdict: "sent_back" }

{ "seq": 2, "status": "done",
  "verdicts": ["task 1 sent back — open again with your findings appended to
                its brief"],
  "advice": "the work you sent back is open again carrying your findings; any
             agent — its author included — may pick it up, and completing it
             will file a fresh review. Tell the human the verdict." }
```

`sent_back` acts in the same transaction as the completion: the work reopens
with the findings appended to its brief, so the next agent to claim it — its
author included — is handed exactly what must change without knowing to ask.
The work still carries its `review` flag, so finishing it again files a fresh
review, recused the same way, and the loop runs round after round until a
review says `upheld`. Upholding is a signature: the card now reads **done,
and seen to be done** by a harness that provably did not do the work.

The human keeps the last word they always had — a `sent_back` that lands on
work you already reopened or cancelled changes nothing, it just goes on the
record — but they stop being the loop's courier.

And because every verdict is delivered on the record — who judged, whose
work, which round — the queue accumulates the one measurement it is uniquely
placed to take:

```
$ hird record
as worker       judged  upheld  sent back  first pass
claude-code          3       3          0        2/2
codex                2       1          1        0/1

as reviewer     upheld  sent back
claude-code          1          1
codex                3          0
```

That is whose work survives a reading by a different model, per harness, off
delivered verdicts and nothing else. It is a report, not a scheduler: nothing
routes work by it, and what to do about a harness that ships rework is a call
hird leaves to you — now made over a table instead of a hunch.

## The ground a task builds on

A dependency edge means one task needs what another produces — "the API waits
for the schema" is about *knowing the schema*, not just outliving it. So the
edge is a context channel, not only a gate: claiming a task hands over each
finished dependency's own result, written by whoever finished it, without the
claimant knowing to ask.

```
task_claim { seq: 2 }

{ "claimed": 2, ...
  "built_on": [{ "seq": 1, "title": "Design the storage schema",
                 "result": "schema in src/db.rs; migrations are an array,
                            version = index + 1",
                 "standing": "done" }] }
```

`standing` says how far that word can be trusted, the same move memory's
footing makes for facts: `done`, `upheld` — read and signed for by a harness
that provably did not write it — or `under review 5, provisional`, which
means the review has not delivered its verdict and a `sent_back` could still
reopen that work.

Provisional matters because verdicts made `done` revocable. A dependent
claimed while its blocker's review is still open is building on an answer
that can be taken back — and when that happens, the queue is the only
participant positioned to notice. Now it does something about it, twice
over. The reopened work's live dependents get a `ground_shifted` event on
their trails in the same transaction as the verdict. And the holder hears at
its next check-in, ahead of everything else the reply could say:

```
task_update { seq: 2, note: "wiring the loader in" }

{ ...,
  "ground_shifted": ["task 1 (Design the storage schema), which this task
      builds on, was sent back by review 5 and reopened; re-read it — the
      findings are in its brief — before building further on its work"] }
```

The same sentence catches ground lost any other way — a blocker a human
reopened or cancelled, or one that failed since — attributed no further than
its status can back. Nothing interrupts the agent mid-edit: hird tells, at
the next moment it is listening, and lets it decide — the same manners the
witness has.

Whether provisional ground should *hold* dependents back is a policy, not a
rule:

```toml
under_review = "clears"   # default: workable at once, told what it stands on
under_review = "holds"    # unclaimable until the review delivers its verdict
```

Under `holds`, the refusal and the board both say what the wait actually is —
`waits for #1 (done, under review 5)` — and `task_next` reports held tasks in
their own bucket, because "finished but for a verdict" points you at a review
to chase, not at work to do. On every board, a `done` card with an open
review wears `under review #5` until the verdict lands: the difference
between *done* and *done, so far as anyone has checked*.

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

### Recall: the task arrives knowing things

`mem_search` only helps an agent that thinks to search, and an agent that has
just been handed a task does not yet know what it does not know. The queue does:
it knows which files this task expects to touch, and it knows what earlier agents
recorded while working those same files. So a claim comes back with those facts
already attached.

```
task_claim { seq: 7, paths: ["src/*.rs"] }

{ "claimed": 7, "title": "Audit the loader",
  "recalled": [
    { "content": "the loader reads HIRD_DB before the config file",
      "why": "learned on task 4 (Port the config loader), working src/config.rs",
      "actor": "codex:9f2c", "task_seq": 4 } ],
  "reminder": "read `recalled` first — earlier agents left those notes …" }
```

Nobody searched, and neither task mentions the other. Overlap is decided by the
same exact pattern intersection the collision detector uses, so a fact recorded
on `src/config.rs` reaches a task that declared `src/*.rs`. Declaring your paths
early is what makes this work — which is the second reason to do it.

Territory is not only what was declared, though. The witness knows which files
actually moved under a task, and those count on both sides, which matters
because declaring a scope is exactly the step an agent in a hurry skips. An
agent that said nothing and went ahead and edited `src/config.rs` still leaves
the file behind it, and what it learned still reaches whoever comes next.

Three things make a fact relevant, strongest first:

| Because | The `why` line reads |
|---|---|
| it was recorded on this task, on an earlier pass | `recorded while working this task earlier` |
| it was recorded on a task whose files overlap yours | `learned on task 4 (…), working src/config.rs` |
| it reads like your title | `mentions "config", "loader"` |

Every recalled fact says where it came from, because an assertion is a claim and
not gospel: an agent that finds one is wrong calls `mem_store` with the truth,
which supersedes the old fact and stops it being recalled again. Nothing is
stored for recall — it is derived at read time from the assertion trail and the
declared scopes, so there is no index to rebuild and no migration to run.

`task_get` carries the same list without claiming anything, and `hird recall`
shows a human exactly what their agents are being told:

```
$ hird recall 7
the loader reads HIRD_DB before the config file
    learned on task 4 (Port the config loader), working src/config.rs  (codex:9f2c, 2h ago)
```

Five facts ride along by default; `recall_limit = 0` switches it off.

### Footing: a fact knows what it was learned against

A memory that only grows is a memory that eventually lies to you. Not by
filling up with falsehoods — you would notice those — but by filling up with
sentences that *were* true. A fact recorded in March about a file that has been
rewritten twice since arrives in July in exactly the same voice as one recorded
this morning, and nothing about reading it tells you which is which.

The sentence cannot tell you. The code can. So hird records what an assertion
was read off — the files behind it, and the content hash each one had at the
time — and every later reader is told whether that ground has moved.

```
$ hird mem standing
firm      the loader reads HIRD_DB before the config file
    01J…  codex:9f2c  2h ago
    src/config.rs
shaky     the renderer redraws on every poll
    01J…  claude-code:af31  3d ago
    src/tui/view.rs
    src/tui/view.rs has changed since this was recorded — re-read before relying on it

2 anchored: 1 firm, 1 shaky
```

Nobody curated that. Nobody notified hird that `src/tui/view.rs` had changed.
The fact remembers which file it came from and what that file said, and the
file no longer says it.

The same word rides along everywhere a fact is served — `mem_search` results,
the memory browser, and the `recalled` list a claim arrives with, which is the
one that matters most because it lands in an agent's context unasked:

```
"recalled": [
  { "content": "the renderer redraws on every poll",
    "why": "learned on task 4 (Fix the renderer), working src/tui/view.rs",
    "standing": "shaky",
    "caution": "src/tui/view.rs has changed since this was recorded — re-read before relying on it" } ]
```

**It never says a fact is false.** A rename, a formatting pass and a total
rewrite look identical from here. `shaky` means *unverified*, which is a
weaker claim and a much more useful one: it is exactly the set of facts where
opening the file pays for itself. `orphaned` — every file it was about is gone —
is the strongest thing hird will say, and it still stops short of a verdict.

Where the files come from needs nothing from the agent. A fact stored with
`task_seq` is anchored to the literal paths that task declared plus everything
the witness saw it move; `mem_store { paths: [...] }` names them outright for a
fact that belongs to no task. A fact with neither stays unanchored, and hird
says nothing about it rather than guessing.

#### Saying it again is how you say you checked

An agent that re-reads a shaky fact and finds it still true has one way to say
so, and it should not need to know any more than that: say the fact again.

```sh
hird mem add "the renderer redraws on every poll" --path src/tui/view.rs
# 01J…
#   already on record — affirmed, not duplicated and re-anchored
```

No second row, no lost provenance. The original is re-anchored to today's code
and you are recorded as another voice for it. That kills duplicate assertions —
the oldest complaint about memory stores — and buys something else on the way
past: hird counts *voices*, not sentences, so it can say the thing no single
harness is positioned to say.

```
also stated by codex:9f2c, independently across 2 harnesses
```

Two agents that cannot see each other's sessions arrived at the same fact. Only
the process both of them talk to can know that.

#### Why it stays quiet

A task that records a fact and then keeps editing the same file would, left
alone, mark its own fact shaky by its own hand. Finishing a task **settles**
what it learned against the tree it is leaving behind, so `shaky` keeps meaning
*somebody else moved this* — which is the only reading worth a warning, and a
warning that fires on everything is a warning nobody reads.

It rides on the same working-tree access as the witness, and is off wherever
that is: no git, or `memory_footing = false`, and memory behaves exactly as it
did before any of this existed — no anchors, no `standing` field, and the
server's instructions do not mention it.

## Command line

```
hird add <title> [--body <md>|--body-file <path>] [--priority N] [--project <path>]
                 [--needs <seq>,…] [--path <glob>]… [--requires <capability>]… [--review]
hird ls [--status <status>] [--all-projects]
hird show <seq>
hird diff <seq> [--path <file>]
hird salvage <seq> <path> [--baseline] [--out <file> [--force]]
hird cancel <seq> [--reason <text>]
hird reopen <seq> [--reason <text>]
hird answer <seq> <answer>
hird dep add <seq> --needs <seq>,…
hird dep rm  <seq> --needs <seq>,…
hird plan apply <file> [--dry-run] [--project <path>]
hird graph [--all-projects]
hird scope <seq> [--path <glob>]… [--clear]
hird require <seq> [--capability <name>]… [--clear]
hird agents [--all-projects]
hird recuse <seq> --from <seq>,… [--reason <text>] | --clear
hird record [--all-projects]
hird events [--follow] [--json] [--kind <kind>,…] [--task <seq>] [--actor <name>]
            [--limit N] [--all-projects]
hird recall <seq> [--limit N]
hird mem add <content> [--tags a,b] [--task <seq>] [--path <file>]…
hird mem search [query] [--limit N] [--all-projects] [--include-superseded]
hird mem standing [--shaky] [--all-projects]
hird tui
hird mcp
hird register <claude-code|codex|copilot|copilot-cli|opencode> [--name <name>]
              [--capability <name>]… [--print] [--force]
hird db-path
```

`--body-file -` reads the task body from stdin, and `hird plan apply -` reads a
whole plan from it, so either can be piped straight into the queue.

`hird ls`, `hird show`, `hird agents` and `hird events` sweep the working tree
as they render, so all four report what has actually moved alongside what was
declared — and say plainly when the answer is "nothing".

## The TUI

```sh
hird tui
```

Three screens, `Tab` between them (`Shift-Tab` goes back). The board polls every
500 ms, so claims from other harnesses appear as they happen.

The **Swarm** screen is the one to watch while several agents are running: every
live agent, the files it has declared, the files that have actually moved under
it, an overlap line in red wherever two of them are in the same territory, and —
on the right — what is workable right now and how much is queued behind it. When
one of those overlaps stops being hypothetical the line goes from `!!` to a
blinking `⚠`, and the pane title counts the agents standing in a file that is
moving under them.

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
| `A` | answer the selected task's question |
| `c` `r` | cancel / reopen the selected task |

| Key | Memory browser |
|---|---|
| `j` `k` | move down / up |
| `Enter` | show the assertion and its provenance |
| `d` | supersede it with something truer |
| `s` | show or hide superseded assertions |
| `f` | only the facts whose files have moved since |

| Key | Swarm |
|---|---|
| `j` `k` | move between agents |
| `Enter` | open the task that agent is holding |

Cards on the queue board carry a yellow `waits #1 #3` badge when a task looks
open but nobody can actually claim it yet, `awaits answer` when its last holder
needs a human decision, and a magenta `reviews #4` badge when a task is
somebody's review — which decides who can take it, and is exactly the thing you
cannot tell from the title. Verdicts mark the cards they judged: a green
`upheld` on done work that a review signed off, a yellow `sent back` on work
that is open again because one did not.

## The board as a log

The TUI is a screen you sit at. `hird events` is the same view for everything
that cannot sit at one — a second terminal, a tmux pane, a CI job, a script,
or another agent:

```
$ hird events --follow
14:31:52    #1  claimed        codex:9f2c        Port the config loader
14:32:07    #1  witnessed      codex:9f2c        src/config.rs (modified)
14:33:41    #2  claimed        claude-code:af31  Audit the config loader
14:36:09    #1  completed      codex:9f2c        ported; env still wins over the file
```

Nothing new is recorded to serve it. Every mutation in hird already lands one
row in an append-only trail — `hird show` replays one task's slice of it as
history — and this is the same trail read sideways: across tasks, in the order
things happened. One shot prints the last 30 and stops. `--follow` keeps
reading at the TUI's cadence, and sweeps expired leases and the working tree
on the way, so an expiry or a witnessed change lands in the feed even while no
agent is calling and nobody has the board open. Stopping the tail costs
nothing and forgets nothing, because the cursor is the trail itself.

`--kind claimed,completed` narrows to the events you are waiting on,
`--task 7` to one piece of work, `--actor codex:9f2c` to one agent's doings,
and `--all-projects` widens to everything in the database. And because a feed
for machines should not make them parse columns, `--json` makes each line one
object:

```
$ hird events --json --kind ground_shifted,contended --follow
{"cursor":41,"at":"2026-08-11T14:36:09.120Z","project":"/home/you/app","task":2,
 "title":"Port the repository layer","actor":"codex:9f2c","kind":"ground_shifted",
 "detail":"task 1 (Design the storage schema) sent back by review 5"}
```

That line is hird's whole integration surface as an emitter: pipe it into
`jq`, a log file, a dashboard, or the thing that pages you. The `dispatch_hook`
answers *wake somebody when work appears*; the feed answers everything after
that — what the swarm did, as it does it, in a form both halves of your
tooling can read.

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

# What a finished dependency under an unfinished review does to its dependents:
#   "clears"  workable at once, told the ground they build on is provisional
#             (default)
#   "holds"   unclaimable until the review delivers its verdict
under_review = "clears"

# Whether task_next passes over tasks whose files overlap live work. Default
# true: when the queue gets to choose, it should not choose a collision.
dispatch_avoids_conflicts = true

# How many recalled assertions a claimed task arrives with. Default 5, and 0
# turns recall off. Keep it small: this lands in an agent's context unasked.
recall_limit = 5

# Whether the queue watches the working tree to see what tasks actually change.
# Needs git, goes quiet by itself where there is none, and costs nothing while
# no task in the project holds a lease. Default true.
witness = true

# Whether an assertion remembers the files it was learned against, so a later
# reader is told when that code has moved. Rides on the same working-tree access
# as `witness` and is off wherever that is. Default true.
memory_footing = true

# Whether the witness keeps the content of the versions it fingerprints, so
# `hird diff` can show what a task changed, reviews carry the diff of the work
# under judgement, and `hird salvage` can recover a version an overlapping
# write discarded. Rides on `witness` and is off wherever that is. Default true.
exhibit = true
```

Agents are told the configured TTL in the MCP handshake and asked to check in at
half that interval, so raising it here is all that's needed to give slow tasks
more room.

| Variable | Meaning |
|---|---|
| `HIRD_HARNESS` | This session's harness name. Set it in the MCP registration. |
| `HIRD_CAPABILITIES` | Comma-separated capabilities this MCP session can satisfy. |
| `HIRD_PROJECT` | Override project detection. |
| `HIRD_DB` | Override the database path. |

## MCP tools

Twelve, and no more.

| Tool | Purpose |
|---|---|
| `task_list` | What work exists, optionally filtered by status — marking the ones your harness cannot claim. |
| `task_get` | One task in full: dependencies, file scope, recent history. |
| `task_next` | **Be handed the next compatible, workable task, already claimed.** |
| `task_claim` | Take a named task. Atomic; fails if held, blocked, recused or capability-incompatible. |
| `task_scope` | Say which files you will change; find out who else is in them. |
| `task_update` | Record progress and renew the lease. Holder only. |
| `task_split` | Break a task into pieces the other agents can work. Holder only. |
| `task_complete` | Finish, with a summary. Holder only. |
| `task_fail` | Give up, with a reason. Holder only. |
| `task_release` | Hand the task back unfinished; with `question`, park it until `hird answer`. Holder only. |
| `mem_store` | Record one durable fact — or, said again word for word, confirm one. |
| `mem_search` | Find facts recorded earlier, by anyone, each marked with whether its code has moved since. |

Results are compact JSON. Failures come back as `isError` text rather than
protocol errors, so a model can relay them to you as-is instead of reporting
that a tool broke.

None of the things an agent is told without asking needed a thirteenth tool.
Recall, `built_on`, and answered `questions` ride along with the claim; `footprint`, `changed`,
`contended` and `undeclared` ride along with every check-in and every
finishing call; `ground_shifted` rides along with the heartbeat; `standing`
rides along with every fact hird serves. Something an agent has to know to
ask for is something it will not ask for.

## Examples

[`examples/`](examples/) holds runnable versions of everything above. Each script
points `HIRD_DB` at a throwaway file, so running one cannot disturb your board.

```sh
./examples/manual-dispatch.sh   # file work, hand it out by number
./examples/swarm-plan.sh        # file a plan, three agents pull from it
./examples/plan-file.sh         # the same plan as a file: read it, file it, edit it
./examples/witness.sh           # two agents in one file, caught in the act —
                                #   and a third task that finishes read-only
./examples/exhibit.sh           # a finished task's uncommitted diff, and a
                                #   written-over version brought back
./examples/question.sh          # wait for a human answer, then hand it to the next claim
./examples/footing.sh           # a fact, the file it came from, and that file rewritten
./examples/review.sh            # work that files its own review, barred to whoever did it
./examples/verdict.sh           # the sent-back loop, and the per-harness record it leaves
./examples/events.sh            # the board as a log: a follower tails the trail
                                #   while two harnesses work, then reads it as JSON
./examples/protocol.sh          # MCP 2026-07-28 on the wire: no handshake, and who the client says it is
```

They open real `hird mcp` sessions and send the tool calls a harness would,
because claiming and completing are agent-side operations with no CLI verb —
so the transcript shows exactly what "pick up task 42" looks like on the wire.
[`examples/harness/`](examples/harness) has drop-in MCP registration for Claude
Code, Codex CLI, Copilot in VS Code and the Copilot CLI.

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
| `repo` | **The only place SQL is written** — tasks, deps, scopes, memory and recall |
| `witness` | **The only place the working tree is read** — git, hashing, sweeps |
| `mcp` `cli` `tui` | The three front ends, which call `repo` |

The rules that matter are pinned by tests rather than convention: the claim
compare-and-set is exercised by sixteen threads racing on separate connections,
the lease sweep by eight concurrent sweepers, self-dispatch by eight agents
calling `task_next` at the same instant and having to come away with eight
different tasks, and the status machine by a table-driven test asserting no
transition exists outside the diagram above. Pattern intersection is checked
against pattern matching over an exhaustive grid: whenever a concrete path
matches two patterns, those patterns must be reported as overlapping.
The witness is exercised against real git repositories rather than a mock, down
to a test that runs two `hird mcp` processes over one checkout, edits a file
from underneath both of them and asserts that the agent holding the stale copy
is told before it writes.
`tests/mcp_stdio.rs` spawns the real binary and speaks JSON-RPC to it, including
a test that a cold `hird mcp` is usable within the 50 ms budget a harness
expects — it starts a fresh one for every session.

## Design notes

`DESIGN.md` is the specification this was built from, kept as written.

`DESIGN.md` deliberately left out dependencies and automatic dispatch; both are
here now, along with file-scope collision detection, because a queue that
several agents work at once needs to know what is workable and what is in the
way. Recall came out of the same observation applied to the other half: the
queue already knew which memory was relevant to a task, and was making agents
guess at it. The witness (§12) came from noticing that every one of those
answers was still assembled out of things agents had said about themselves, and
that the one failure worth catching is the one nobody reports. Footing (§14) is
that same look at the working tree turned on the memory: an assertion is a
statement about code, code changes, and until now nothing anywhere noticed.
Recusal (§15) is the third application of the same idea: the one thing an agent
cannot honestly report is whether its own work is any good, and hird is the only
process in the room that knows whose work it is. The exhibit (§19) is the
witness given a memory of its own: every version it fingerprints is kept, so
"a file moved" becomes "this is the change", a review is handed the diff it is
judging, and the write that lands on uncommitted work stops being a loss.
Still absent: multi-machine sync and vector search. The append-only event trail
is meant to make sync tractable later. `ROADMAP.md` lays out what comes next,
what comes later, and what stays out on purpose.

## Licence

MIT OR Apache-2.0.
