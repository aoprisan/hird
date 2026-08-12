# Examples

Runnable versions of everything in the [usage guide](https://aoprisan.github.io/hird/).

Each script points `HIRD_DB` at a fresh temporary file, so running one cannot
disturb your real queue. They use an installed `hird` if there is one on `PATH`
and otherwise build this checkout (`HIRD_BIN=/path/to/hird` overrides both).
Output is much easier to read with [`jq`](https://jqlang.github.io/jq/)
installed, but nothing requires it.

| File | What it shows |
|---|---|
| [`manual-dispatch.sh`](manual-dispatch.sh) | **Handing work out by number** — "pick up task 42", start to finish, ending with the next agent in those files being handed what the first one learned. |
| [`swarm-plan.sh`](swarm-plan.sh) | **Letting agents pull** — a dependency graph, three agents, no assignment. |
| [`plan-file.sh`](plan-file.sh) | **The plan as a file** — read it before it is filed, file the whole graph in one transaction, edit it and file it again. |
| [`plan.toml`](plan.toml) | The plan format, annotated, every field at work. |
| [`witness.sh`](witness.sh) | **What actually happened** — two agents, one checkout, one file, and the warning that arrives while there is still time to act on it. |
| [`exhibit.sh`](exhibit.sh) | **The witness keeps what it saw** — a finished task's uncommitted diff shown after the tree has moved on, and a written-over version brought back with one command. |
| [`tenure.sh`](tenure.sh) | **A task remembers every hand that held it** — an agent leaves uncommitted edits and vanishes, and the successor's own claim says whose leavings it is standing in, with round one still diffable after round two writes over it. |
| [`question.sh`](question.sh) | **Work that knows why it is waiting** — an agent reaches a decision it must not guess, the task stays out of dispatch until the human answers, and the answer rides in the next claim. |
| [`footing.sh`](footing.sh) | **Memory that knows when it went stale** — a fact recorded against a file, that file rewritten, and every later reader told so without anybody curating anything. |
| [`review.sh`](review.sh) | **No agent reviews its own work** — finishing files the review, scoped to what actually moved, and the queue refuses it to the harness that did it. |
| [`verdict.sh`](verdict.sh) | **The review closes its own loop** — a `sent_back` verdict reopens the work carrying the findings, the redo files a fresh review, and `hird record` keeps score per harness. |
| [`dispatch-hook.sh`](dispatch-hook.sh) | **The one push in a pull design** — a configured command hears about every task that becomes claimable: filed, unblocked, review filed, sent back. Point it at a multiplexer like [herdr](https://herdr.dev) and the lines become summonses for idle agents — with `HIRD_RECUSED` naming whom a review must not wake, so the summons routes itself to hands the queue will accept. |
| [`events.sh`](events.sh) | **The board as a log** — a follower tails the trail while two harnesses work, then the same record is read after the fact, filtered by kind, and emitted as JSON: monitoring without the TUI, and the feed other tooling builds on. |
| [`protocol.sh`](protocol.sh) | **MCP 2026-07-28 on the wire** — `server/discover`, a task worked without ever calling `initialize`, and a harness that never set `HIRD_HARNESS` named by its own client. |
| [`task-body.md`](task-body.md) | A task body worth writing, for `--body-file`. |
| [`config.toml`](config.toml) | Every configuration key, annotated, at its default. |
| [`harness/`](harness) | MCP registration for Claude Code, Codex CLI, Copilot in VS Code, the Copilot CLI and OpenCode — what `hird register <harness>` writes, for reading or for pasting somewhere it does not reach. |
| [`lib/mcp.sh`](lib/mcp.sh) | The shell helpers the scripts share — a raw MCP session in 20 lines. |

```sh
./examples/manual-dispatch.sh
./examples/swarm-plan.sh
./examples/plan-file.sh
./examples/witness.sh          # needs git; makes its own throwaway repository
./examples/exhibit.sh          # likewise
./examples/tenure.sh           # likewise
./examples/question.sh         # needs nothing but hird
./examples/footing.sh          # likewise
./examples/review.sh           # likewise
./examples/verdict.sh          # likewise
./examples/dispatch-hook.sh    # needs neither
./examples/events.sh           # needs git; makes its own throwaway repository
./examples/protocol.sh         # needs neither
```

## Why the scripts speak JSON-RPC

Claiming, scoping, completing and failing are agent-side operations — only the
lease holder may do them — so they have no CLI verb. Rather than pretend, the
scripts open a real `hird mcp` session and send the same tool calls a harness
would, which is also the clearest way to see what "pick up task 42" costs on the
wire: one `task_claim`, naming the number you said.

Each `mcp <harness>` call in a script is one agent session with its own identity
(`codex:9f2c`), and its lease outlives the process — which is exactly why the
next session in the script is handed something else.

`protocol.sh` is the one script where the wire itself is the subject: it opens
a session on MCP 2026-07-28, which has no handshake at all, and every request
carries in `_meta` what `initialize` used to say once.

`witness.sh`, `exhibit.sh`, `tenure.sh`, `footing.sh`, `review.sh` and `verdict.sh` need more than that. To show two agents taking turns in one file
it has to keep both sessions open and edit the tree *between* their calls, so it
uses `session_open` / `session_call` instead: a pair of fifos per session, and
every call waiting for its own answer. A heredoc will not do it — a script
feeding requests down a pipe runs far ahead of the server reading them, so
nothing written between two lines lands between them.

## Manual or automatic

Both, at once, on the same queue:

- **Manual.** `hird add`, then tell one agent *"pick up task 42"*. It calls
  `task_claim` on the number you named. This is the whole of
  `manual-dispatch.sh`, and it needs no dependencies and no file scopes.
- **Automatic.** File a plan, then tell every agent *"work the queue"*. Each
  calls `task_next` and is handed the most important task that is actually
  workable. That is `swarm-plan.sh`, or `plan-file.sh` for the same graph
  written down and applied from a file.

Nothing in hird pushes work at an agent: `task_next` is a tool the agent chooses
to call, so an agent you never point at the queue stays idle until you name a
number.

The one exception is opt-in and points the other way — at you, or at whatever
you delegate the pointing to. Set `dispatch_hook` in the config and hird runs
that command whenever a task becomes claimable, with the task's number, title
and cause in its environment. Under a terminal multiplexer that can address
agents, such as [herdr](https://herdr.dev), the hook can `herdr agent prompt`
an idle agent to work the queue — self-dispatch with nobody watching the
board. `dispatch-hook.sh` shows the announcements themselves.
