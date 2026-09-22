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
door — and, optionally, one that knocks on the door the task actually
suits.

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

## Routing by fit

The roster is an order you wrote once. It cannot know that this task is a
sprawling refactor and that one a rename, which is the whole of "choose the
best tool for the job" — and hird will not choose for you. It is explicit
about that: the queue knows what a task *requires* and what a caller
*advertises*, but it has no roster and picks nobody. Fit is a judgement, and
judgements live out here in the hook.

So ask a classifier. [`jev`](https://crates.io/crates/jev-repl) sends the
task to TypeSafe AI's System One as a `choice` over your harness names and
reads back one label with a confidence:

```sh
cargo install jev-repl
cp route.jev "$(herdr plugin config-dir hird)/route.jev"   # route.jev ships next to dispatch.sh
```

Then edit the labels in that copy to your own harness names — the ones in the
roster's third column — and write what each agent is actually good at. That
text *is* the rubric; it is all the model reads about your agents.

```text
harness: Which harness is the better tool for this particular task
  claude-code = Ambiguous briefs, architectural decisions, sprawling multi-file changes
  codex = A clear spec carried out exactly: mechanical edits, test writing, a rename
  copilot = Small local fixes in code somebody already has open
```

From then on every announcement asks the page which harness fits, and the
roster is walked twice: that harness first, then everybody. Which is the
point worth being precise about.

**An opinion is a preference, never a permission.** The answer only reorders
the roster. Every worker it reaches still has to clear the same three bars —
not recused, equipped, not busy — and the unfiltered walk follows immediately
after, so a routing answer can move a worker up and can never rule one out.
A review recused from `claude-code` does not go to `claude-code` because a
model liked the idea; it goes to whoever may actually take it. The queue then
checks the hard facts a third time, atomically, when that agent claims.

**The question is narrowed before it is asked.** Two of those bars are facts
about the task rather than guesses about the moment — `HIRD_RECUSED` and
`HIRD_REQUIRES` against the roster's third and fourth columns — so the relay
applies them to the *labels* and not only to the answer. The page that goes
over the wire offers exactly the harnesses this task may go to. A choice
spends its confidence on the options it is given, so leaving a recused
harness in costs an answer that could only ever match nobody: on a review, the
one case where the queue has already said your default order is wrong. Two
consequences worth expecting — a label no roster line carries is cut (it
routed nothing anyway), and when the bars leave fewer than two harnesses there
is nothing to decide, so **no call is made at all**. On a swarm of two, that is
every review.

**Capability or rubric — they are different knobs.** Something an agent's
environment either has or does not — a browser, credentials, an OS, a GPU — is
a capability: hird enforces it, and work that requires it waits until an
equipped worker claims. Something an agent is merely *better at* belongs in
the page's description, where being wrong costs one summons. Writing
`refactoring` as a capability to force routing swaps a preference for a
correctness constraint, and the symptom is a task marked `incompatible`
forever rather than one that went to the second-best agent.

**Every way it can go wrong ends in your roster order.** No page, no `jev` on
`PATH`, no API key, a call that times out, an answer below the confidence bar,
an answer this cannot parse: all of them are silence, and silence is the relay
exactly as it behaved before the page existed. Routing is an upgrade over the
walk, never a gate in front of it.

Four knobs, all optional, all baked into the hook line by `wire`:

| Variable | Default | What it does |
|---|---|---|
| `HIRD_JEV_PAGE` | `<config-dir>/route.jev` | The page. Missing is the off switch — `wire` names the path whether or not the file is there, so putting one in later needs no re-wiring. |
| `HIRD_JEV_CONFIDENCE` | `0.6` | The bar an answer must clear to be heard at all. |
| `HIRD_JEV_TIMEOUT` | `10` | Seconds one live call may take. |
| `HIRD_JEV_BIN` | `jev` | Where the binary is, if not on `PATH`. |

Two things to know before leaning on it:

- **The key has to be where the announcement is made.** hird spawns the hook
  from whichever process announced — an agent's `hird mcp` session, a `hird
  add` in your shell, a `hird events --follow` that swept an expired lease —
  so the hook inherits *that* environment. `TYPESAFE_API_KEY` belongs in a
  shell profile all of them read. Without it `jev` would simulate its
  answers, and a simulated answer is a coin that lands in the roster order
  looking exactly like a judgement, so the call is not made at all.
- **Calibrate before you trust it, and price it first.** Every claimable
  event that leaves two or more harnesses in the running is one call:
  `filed`, `unblocked`, `review_filed`, `sent_back`, `released`, `reopened`,
  `answered`, `lease_expired`. `jev cost route.jev --price <in>/<out>` says
  what that costs per announcement, and `jev eval route.jev --cases
  routed.jsonl` over thirty tasks you have already routed by hand says
  whether `0.6` is the right bar or whether most answers should be falling
  through to the order you wrote. A bar measured on the whole page is a
  conservative one for the narrowed page actually sent: fewer labels is an
  easier question, and its confidences run higher.
- **The startup report says when the two files have drifted.** The page's
  labels and the roster's third column are the same names kept in two places.
  `herdr plugin log list --plugin hird` names a label no roster line carries
  (an answer that routes nothing) and a roster harness the page never
  describes (an agent that can never be preferred).

If you wired the hook before this existed, reopen the `wire` pane once: the
hook line is what carries the page's path.

Why it is shaped this way — what a classifier may and may not be asked, and
what was deliberately left unbuilt — is in
[ROUTING.md](https://github.com/aoprisan/hird/blob/main/ROUTING.md).

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
six short POSIX `sh` entry points over a shared `lib.sh` and `route.sh`, no
build step, no dependencies — so the read before the install is a short one.
Everything the plugin assumes about herdr itself (what a busy worker looks
like, what a prompt's exit status is worth, how simultaneous relays take
turns) is in `lib.sh`, which is the file to read first; everything it assumes
about `jev`, including the one call it can be made to pay for, is in
`route.sh`, which is the file to read before copying a `route.jev`. Its CI-only behavioral check lives
in `.github/scripts/`.
