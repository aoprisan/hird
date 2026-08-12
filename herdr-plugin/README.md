# The hird plugin for herdr

**The [hird](https://github.com/aoprisan/hird) queue under
[herdr](https://herdr.dev): watch the board, follow the feed, wire the
dispatch hook, summon idle agents to claimable work.**

hird is pull: agents ask for work, and a task that becomes ready while every
agent is idle waits on the board in silence. hird's one push is the
`dispatch_hook` — a command it runs, detached, the moment a task becomes
claimable. herdr is a thing that can address an idle agent. The pairing is
already in hird's docs as two lines of shell; this plugin is that pairing
packaged, with the part the two lines leave out: routing that survives a
missing agent, and a summons that never knocks on a recused door.

## Install

```sh
herdr plugin install aoprisan/hird/herdr-plugin
herdr plugin pane open --plugin hird --entrypoint wire
```

The `wire` popup does the setup while you watch: it seeds a worker roster in
the plugin's config directory and points hird's `dispatch_hook` at the
plugin's relay. It replaces a hook it wrote before, treats hird's shipped
`dispatch_hook = ""` as unset, and refuses to clobber a hook of your own —
showing it next to the line you would add by hand instead.

Needs `hird` on `PATH` ([install](https://github.com/aoprisan/hird#install))
and works on Linux and macOS.

## What you get

| Entrypoint | Kind | What it does |
|---|---|---|
| `board` | pane (overlay) | `hird tui` in the focused project, over whatever you were looking at. `herdr plugin pane open --plugin hird --entrypoint board` |
| `feed` | pane (overlay) | `hird events --follow` in the focused project: the trail as it lands — and the process that keeps announcing while the agents are quiet. `herdr plugin pane open --plugin hird --entrypoint feed` |
| `wire` | pane (popup) | The setup, narrated: seed the roster, write the hook, say what landed where. |
| `summon` | action | Wake the first reachable roster worker to work the queue — for work that became ready while the hook was unwired. `herdr plugin action invoke hird.summon` |
| startup | hook | One posture report per server start — hird present? hook wired? roster there? — in `herdr plugin log list --plugin hird`. |

Bind the board to a key:

```toml
# ~/.config/herdr/config.toml — see the herdr keybinding docs for the file
[[keys.command]]
key = "prefix+h"
type = "shell"
command = "\"$HERDR_BIN_PATH\" plugin pane open --plugin hird --entrypoint board"
description = "hird board"
```

## The feed, and why one should be open

hird has no daemon, which means a lease that runs out is not enforced by a
timer — it is enforced by whichever process reads the queue next, and
announced by that same process. Every announcement but this one rides on a
write some agent was making anyway. This one has no write behind it: when a
worker dies, the fact that its task is claimable again is only ever noticed
by somebody reading.

In a swarm that is working, somebody always is. In a swarm that has gone
quiet — the last agent died holding the last task — nobody is, and the relay
stays silent about exactly the case it exists for. `feed` is a reader that
does not stop: `hird events --follow` sweeps every poll, announces what it
collects through the hook, and prints the trail while it goes. Leave one open
and a dead worker is replaced without you; close it and the queue is correct,
current, and quiet again until something calls.

## The relay, and why it routes

Once wired, every announcement hird makes — a task filed with nothing
blocking it, unblocked by a finished dependency, reopened by a `sent_back`
verdict, handed back, filed as a review, dropped by an expired lease —
runs `dispatch.sh` with the announcement in its environment. The relay walks
the roster in order and prompts, via `herdr agent prompt`, the first worker
that clears two bars:

- **Not recused.** `HIRD_RECUSED` names the harnesses the queue will refuse
  this task to — a filed review names whoever did the work under judgement.
  The relay skips those workers, so the review loop runs on a swarm of two
  without ever summoning the author to judge their own work.
- **Actually there.** A prompt that fails — no such agent, agent gone —
  falls through to the next worker instead of dying with the summons
  undelivered.

If every worker is barred or unreachable the relay exits quietly: the task
is still on the board, and the `summon` action or the next announcement
tries again.

## The roster

`wire` seeds `dispatch.conf` in the plugin's config directory
(`herdr plugin config-dir hird` prints it); after that the file is yours.
One line per worker, preference order top to bottom:

```
worker <herdr agent name> <hird harness[,harness...]>
```

The agent name is what `herdr agent list` shows. The harness column is how
hird knows the same agent — what `hird agents` and `hird record` print —
and is what recusal is matched against. List every name the harness may
report, comma-separated, no spaces:

```
worker claude claude-code
worker codex codex,codex-cli
```

Without a roster the relay falls back to exactly those two lines.

## Undo

```sh
herdr plugin uninstall hird
```

removes the plugin and its managed checkout. The hook line in
`~/.config/hird/config.toml` (marked `# wired by the hird herdr plugin`) is
hird's config, not the plugin's, so delete that line yourself — or leave it;
a relay whose script is gone announces to nobody, exactly like the empty
default.

## Trust

The usual [herdr plugin guidance](https://herdr.dev/docs/plugins/#trust-and-security)
applies: this is ordinary code running as your user. It is small on
purpose — five short POSIX `sh` scripts, no build step, no dependencies —
so the read before the install is a short one.
