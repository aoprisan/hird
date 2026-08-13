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
missing agent, and a summons that never knocks on a recused or unequipped
door.

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

Needs `hird` on `PATH` ([install](https://github.com/aoprisan/hird#install)),
Herdr 0.7.5 or newer, and works on Linux and macOS.

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
the roster in order and prompts, via `herdr agent prompt`, the first idle worker
that clears three bars:

- **Not recused.** `HIRD_RECUSED` names the harnesses the queue will refuse
  this task to — a filed review names whoever did the work under judgement.
  The relay skips those workers, so the review loop runs on a swarm of two
  without ever summoning the author to judge their own work.
- **Equipped.** `HIRD_REQUIRES` names the capabilities the task needs. The
  optional fourth roster column names what each worker advertises through
  `HIRD_CAPABILITIES`; the relay skips any worker missing even one label. The
  queue repeats that check atomically when the worker claims.
- **Free and actually there.** A worker `herdr agent get` reports working or
  blocked is skipped, and a prompt that fails — no such agent, agent gone —
  falls through to the next worker instead of dying with the summons
  undelivered. Relays are serialized just through the prompt's transition to
  `working`, so a burst of announcements spreads across free workers instead
  of all observing the same preferred worker before its state changes.

The status check is an optimization, and it is written to fail in the cheap
direction. Only the states herdr names as occupied count as busy: if
`agent get` cannot be run or its answer cannot be read, the relay prompts
anyway and lets the prompt be the judge, exactly as it did before it learned
to read status. A wrong guess about herdr's output then costs one redundant
prompt rather than every summons the plugin would ever send. For the same
reason a prompt that times out waiting for `working` is not treated as a
refusal without checking: the summons may well have landed on an agent that
was slow to start, and walking on would put two agents on one task.

If every worker is barred or unreachable the relay exits quietly: the task
is still on the board, and the `summon` action or the next announcement
tries again.

## The roster

`wire` seeds `dispatch.conf` in the plugin's config directory
(`herdr plugin config-dir hird` prints it); after that the file is yours.
One line per worker, preference order top to bottom:

```
worker <herdr agent name> <hird harness[,harness...]> [capability[,capability...]]
```

The agent name is what `herdr agent list` shows. The harness column is how
hird knows the same agent — what `hird agents` and `hird record` print —
and is what recusal is matched against. The optional fourth column lists the
capabilities the worker registers with `hird register --capability`; it is
what `HIRD_REQUIRES` is matched against. List names comma-separated, with no
spaces:

```
worker claude claude-code browser,network
worker codex codex,codex-cli filesystem,shell
```

Omit the fourth column for a worker with no special capabilities. Without a
roster the relay falls back to the same two workers with no special
capabilities, so ordinary tasks still route while capability-bound work waits
for an explicit roster entry.

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
applies: this is ordinary code running as your user. It is small on purpose —
six short POSIX `sh` entry points over one shared `lib.sh`, no build step, no
dependencies — so the read before the install is a short one. Everything the
plugin assumes about herdr itself (what a busy worker looks like, what a
prompt's exit status is worth, how simultaneous relays take turns) is in
`lib.sh`, which is the file to read first. Its CI-only behavioral check lives
in `.github/scripts/`.
