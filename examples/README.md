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
| [`task-body.md`](task-body.md) | A task body worth writing, for `--body-file`. |
| [`config.toml`](config.toml) | Every configuration key, annotated, at its default. |
| [`harness/`](harness) | Drop-in MCP registration for Claude Code, Codex CLI and VS Code. |
| [`lib/mcp.sh`](lib/mcp.sh) | The shell helpers the scripts share — a raw MCP session in 20 lines. |

```sh
./examples/manual-dispatch.sh
./examples/swarm-plan.sh
./examples/plan-file.sh
./examples/witness.sh          # needs git; makes its own throwaway repository
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

`witness.sh` needs more than that. To show two agents taking turns in one file
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
