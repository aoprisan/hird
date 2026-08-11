# herdr integration assessment

An assessment of how far hird should go in integrating with
[herdr](https://herdr.dev), written against hird `main` (post-v2.3, the herald
and the feed both landed) and herdr `0.8.0` (read from source at
[herdrdev/herdr](https://github.com/herdrdev/herdr)).

The question is not *whether* to pair with herdr — the dispatch hook was built
for that pairing, and DESIGN.md §21 already settled the shape of it ("why a
command and not an integration"). The question is what, if anything, comes
next: where the current pairing is thin, which of herdr's surfaces are worth
building against, and which are traps.

One ground rule of the original design is treated as negotiable here: the
no-daemon rule. hird's owner has said the project can be flexible on that, so
this assessment evaluates resident options on their merits instead of ruling
them out at the door. The line that is *not* treated as negotiable is
coupling: whatever runs, hird's binary should still not know herdr's name.

**Summary of the recommendation:** keep the binary ignorant of herdr, exactly
as §21 decided — but the pairing as shipped leans on a one-line hook that
targets a fixed agent name, treats every event the same, and fires exactly
once, and that leaves real value on the table. Close the gap in five moves,
none of which puts herdr in hird's dependency graph: (1) a reference dispatch
script in `examples/` that picks an idle agent instead of naming one, routes
reviews away from the recused harness, and falls back to a herdr notification
when nobody is idle; (2) one new environment variable in the announcement,
`HIRD_RECUSED`, so any hook can do that routing without guessing; (3) a
paragraph of skill guidance teaching agents inside herdr to report the task
they hold as pane metadata, so the herdr sidebar shows the board's state per
agent; (4) with the daemon rule bent, an *optional resident herald* — `hird
herald`, a foreground command that tails the board's own feed and re-announces
work that stays unclaimed, which is the piece a fire-and-forget hook can never
be: dispatch that survives the moment everyone was busy; (5) later, and in its
own repository, a herdr *plugin* that bundles the TUI as a pane, an
events-feed bridge, and the herdr-native half of matchmaking (summon-on-idle).
Worktree-per-task orchestration is the one tempting integration to explicitly
defer: it quietly breaks both the witness and project scoping.

---

## 1. Where things stand

hird today touches herdr in exactly three places, all of them one-directional
and none of them code:

- **`dispatch_hook`** (src/herald.rs) runs an arbitrary command when a task
  becomes claimable, with `HIRD_EVENT`, `HIRD_TASK`, `HIRD_TITLE`,
  `HIRD_PROJECT`, `HIRD_DB` in its environment. The documented pairing is
  `dispatch_hook = 'herdr agent prompt worker "hird task $HIRD_TASK is ready;
  work the hird queue."'` — herdr is the motivating example, named in
  `examples/config.toml`, `examples/dispatch-hook.sh`, the FAQ, and the README.
- **The skill** (skills/hird/SKILL.md, "More hands, inside herdr") teaches an
  agent that finds itself with `HERDR_ENV=1` to split a pane, start a sibling
  of a *different* harness, and prompt it to work the queue — on explicit user
  request only.
- **The docs FAQ** answers "can hird start agents itself?" with: no, but the
  hook can tell herdr to.

DESIGN.md §21 records why this is a command and not an integration: herdr is
one answer to "something that can address an agent", and a config key holding a
command line is all of them at once. That reasoning has aged well and nothing
below argues against it. hird's binary should not learn herdr's name.

## 2. What herdr offers (the surface inventory)

Read from herdr 0.8.0's source and docs, the pieces relevant to hird:

**The runtime.** herdr is a background server (Rust, Apache-2.0) that owns the
terminals agents run in. Panes are addressed `w1:p1`; a recognized coding agent
in a pane has a lifecycle state — `working`, `blocked`, `idle`, `done` (idle
but not yet seen by the human), `unknown` — read mostly off the screen, or
reported by per-harness integrations. It survives detach, lid-close, and
reattaches over SSH. Crucially for hird: *it is already the daemon.* Anything
in the hird/herdr pairing that wants to be resident belongs on herdr's side of
the line.

**The CLI / socket API.** One control surface, three layers (agent skill → CLI
wrappers → newline-delimited JSON over a local socket). The socket path
resolves in order: `--session`, `HERDR_SOCKET_PATH`, `HERDR_SESSION`, then the
default `~/.config/herdr/herdr.sock` — so a dispatch hook fired from a process
*outside* any herdr pane still reaches the default session. The calls that
matter here:

| Call | What it gives a hird hook or bridge |
| --- | --- |
| `herdr agent list` | Every live agent with pane id, kind (`claude`, `codex`, `copilot`, …), and lifecycle state — enough to *choose* a target instead of naming one. |
| `herdr agent prompt <target> "…" [--wait]` | Atomic prompt submission, honoring bracketed paste. Can prompt a working agent (the text queues). `agent_prompt_stalled` if nothing observably happens within 5s. |
| `herdr agent wait --until <state>` | Server-owned, event-driven waits on semantic state. |
| `herdr agent start <name> --kind <k> --pane <id>` | Start a supported agent in an existing shell pane; returns when it is ready for input. |
| `herdr pane split / pane run / pane wait-output / pane read` | Raw terminal fan-out for non-agent work. |
| `herdr notification show "…" --body "…" --sound request` | A toast through the human's configured delivery — the natural "nobody is idle" fallback. |
| `herdr pane report-metadata` | Display-only pane metadata: title, display name, per-state labels, and named *tokens* renderable as `$name` in the sidebar. TTL-able, source-scoped, sequence-guarded. |
| `herdr workspace report-metadata` | The same token contract at workspace level. |
| `agent.view.set` (socket) | A declarative filter/sort projection over the Agents sidebar — non-plugin sources are allowed. |
| `events.subscribe` (socket) | Long-lived push stream of workspace/tab/pane/agent/worktree lifecycle events. |
| `worktree.create / open / remove` | Git worktrees as first-class workspaces. |

**The plugin system.** A manifest (`herdr-plugin.toml`) declaring startup
hooks, actions (context-menu / keybinding invocable), event hooks (run on
`worktree.created` etc.), terminal pane entrypoints (`overlay`, `popup`,
`split`, `tab`, `zoomed`), and URL link handlers. Plugins install from GitHub
via a marketplace, persist across restarts, and receive `HERDR_SOCKET_PATH`,
`HERDR_BIN_PATH`, state/config dirs, and event JSON in their environment.

**Per-harness integrations** (`herdr integration install claude|codex|…`) are
herdr's mechanism for *agent harnesses* to report session identity and
lifecycle state. hird is not a harness and has no session to restore; this
surface is not applicable to hird, and nothing below uses it.

## 3. Assessment of the pairing as shipped

What works, first, because it is most of it: the hook fires on every cause
(`filed`, `unblocked`, `review_filed`, `sent_back`, `reopened`, `released`,
`lease_expired`), it is announced off the committed board so it never lies,
herdr's socket resolution means the hook works whether or not the announcing
process is inside a pane, and the skill closes the loop on the receiving end
("summoned by the queue" → claim that task, fall back to `task_next`). The
division of labour in the FAQ — hird decides *what* and *who may*, herdr owns
*where* — is exactly right.

The gaps are all in the one line of config the user is told to write:

**Gap 1 — the fixed name.** `herdr agent prompt worker …` requires a live
agent named `worker`. Agents a user starts by hand are detected but *unnamed*
(addressable only by pane id) until someone runs `agent start` with a name or
`agent rename`. The documented one-liner therefore fails silently — hird
closes the hook's stdio and swallows its exit, by design — for the most common
setup. Nothing tells the user; the task just sits there, which is precisely
the seam the herald was built to close.

**Gap 2 — reviews can summon the recused.** `review_filed` announces a task
that the harness which did the work is barred from claiming (src/repo/recusal.rs:
the bar is the harness, not the session). A hook that always prompts the same
agent will, roughly half the time in a two-harness setup, summon exactly the
agent the queue must refuse. The summoned agent claims nothing, `task_next`
correctly routes around the recusal and finds nothing else, and the
announcement is spent. The review then waits for a human again — the pre-§21
world, restored for the most valuable event on the board. The hook *cannot*
fix this alone: the announcement's environment does not say who is recused,
and mapping herdr's agent kinds to hird's free-form `HIRD_HARNESS` strings by
guesswork is exactly the kind of coupling §21 exists to avoid.

**Gap 3 — nobody idle, nobody told.** When every agent is `working`, a prompt
still queues (harmless — it becomes the agent's next turn), but when there are
*no* live agents the hook fails silently. The polished shape is: pick an idle
agent if one exists, otherwise tell the human through
`herdr notification show` — a one-toast fallback the current example never
mentions.

**Gap 4 — prompt storms.** `plan apply` announces every immediately-workable
task it files; a fixed-target hook turns a ten-task plan into ten queued
prompts at one agent. Self-limiting (the skill says fall back to `task_next`,
and a summoned agent works the queue, not just the named task) but noisy, and
trivially avoided by a hook that spreads prompts across idle agents or
coalesces.

**Gap 5 — the board is invisible from the sidebar.** herdr's sidebar can
render per-pane metadata tokens (`$hird_task`), but nothing reports any. A
human running three agents under herdr sees `working / working / idle` and has
to open the TUI to learn *which task* each agent holds. The data is all in the
claim results already; only the last foot of plumbing is missing.

**Gap 6 — dispatch is edge-triggered on one side only.** This is the
structural one, and the only one a better hook cannot fix. hird announces at
the moment *work becomes ready*; nothing anywhere announces the symmetric
moment *hands become free*. An announcement that fires while every agent is
busy or absent is spent — queued into a busy agent's input at best, a
one-time toast at worst — and when an agent goes idle ten minutes later with
that task still open on the board, silence. The skill papers over it ("call
`task_next` until nothing is workable"), but that discipline ends when the
agent's turn ends. Fire-and-forget can summon hands to work; it cannot bring
work to hands. Closing this requires something resident on one side of the
seam or the other — which is exactly the flexibility now on the table, and
§5 below takes it up.

Gaps 1–5 are not defects in the herald; they are the cost of the example hook
being one line, and all are addressable without touching §21's rule. Gap 6 is
a limit of fire-and-forget itself.

## 4. Options

### O1 — a reference dispatch script: `examples/herdr-dispatch.sh` ★ recommended

A shipped, documented script the user points `dispatch_hook` at, replacing the
one-liner as the *recommended* herdr pairing (the one-liner stays as the
minimal illustration). Behaviour:

1. Resolve herdr (`herdr` on `PATH`, or `HERDR_BIN_PATH` when set); exit 0
   quietly when absent.
2. `herdr agent list`, parse JSON; candidates are agents in state `idle` or
   `done`.
3. For `HIRD_EVENT=review_filed`, drop candidates whose kind maps to the
   recused harness (see O2; without O2 this step is best-effort name matching).
4. Prompt the first candidate with the summons phrasing the skill already
   recognizes; on `agent_prompt_stalled` or error, try the next.
5. No candidates: `herdr notification show "hird: task $HIRD_TASK waiting"
   --body "$HIRD_TITLE" --sound request` — the human hears exactly once per
   announcement, rate-limited by herdr itself.

Optionally, behind an explicit env knob in the script (off by default): when
nobody is idle and fewer than N agents are live, split a pane and `agent
start` a new one. Spawning agents from a hook is a policy decision with real
cost; it must be the user's, made by editing a visible variable, never a
default. This mirrors the skill's own rule ("never on your own initiative").

Effort: one shell script plus docs. Risk: low — it depends only on documented
CLI JSON (`herdr agent list`, `agent prompt`, `notification show`), and herdr
documents protocol-stability checking (`herdr status`) if the script ever
needs to be defensive. Closes gaps 1, 3, 4, and (with O2) 2.

### O2 — announce the recusal: `HIRD_RECUSED` ★ recommended, the one binary change

Add one variable to the `review_filed` announcement's environment: the harness
name barred from the review (empty otherwise). The completing transaction that
files the review already knows it — the recusal edge is written there — so
this is threading an existing string through `Announcement`, not new state.

This is herdr-agnostic in exactly the way §21 demands: hird states a fact
about its own board ("this task exists because finished work needs different
eyes than `claude-code`'s") and any hook — herdr, a notifier, a CI trigger —
decides what to do with it. Without it, no hook anywhere can route reviews
correctly, because the fact lives only inside the database. A dozen lines
including the test, and `examples/dispatch-hook.sh`'s transcript gains one
column.

Worth doing even if nothing else in this document happens.

### O3 — skill guidance: report the held task as pane metadata ★ recommended

One short addition to the skill's herdr section: after a successful
`task_claim` inside herdr (`HERDR_ENV=1`), and again at `task_complete` /
`task_release`, run

```sh
herdr pane report-metadata "$HERDR_PANE_ID" \
  --source user:hird --token hird_task="#42 port the loader"
```

(and clear the token on finish). Tokens are display-only, source-scoped, and
guarded so they cannot disturb lifecycle authority or another integration's
reports — herdr built this surface precisely so tools like hird can decorate
the sidebar without owning it. The human's sidebar then reads
`claude · working · #42 port the loader`, which closes gap 5 for the cost of
a documentation paragraph. Unreliable in the way all skill guidance is
(agents forget), which is acceptable for presentation and would be
unacceptable for anything load-bearing — the reliable version of this belongs
in O4's bridge, which can do it from the events feed without agent
cooperation.

### O4 — a herdr plugin, in its own repository — recommended later

Everything above is fire-and-forget. herdr, by contrast, is a server with a
plugin system whose manifest was built for exactly this shape of guest — so
whatever the daemon rule does or doesn't allow hird itself (§5), the
herdr-native resident pieces belong here. A `hird-herdr` plugin (separate repo,
installable from the herdr marketplace, so neither project carries the other)
could bundle:

- **A pane entrypoint** running `hird tui` (`placement = "overlay"` or
  `popup`) — the board one keybinding away in any herdr session, plus an
  action ("show the hird board") in herdr's context menus.
- **The bridge**: a `[[startup]]`-launched process that tails
  `hird events --follow --json` (the v2.3 feed is NDJSON with a crash-safe
  rowid cursor, built to be followed by exactly this kind of reader — and
  `--follow` sweeps, so the bridge also keeps lease expiry enforced while it
  watches) and turns selected events into `notification.show` toasts, pane
  tokens (the reliable O3), and optionally an `agent.view.set` projection
  ("agents holding hird tasks first"). With the daemon rule bent, the bridge
  is also where herdr-native *matchmaking* lives — see §5.
- **The dispatch wiring**: ship O1's script inside the plugin and let its
  install step be the thing that tells the user what `dispatch_hook` line to
  set (hird's config stays the user's to edit; the plugin should not write it).

The plugin system is v1-early (manifest-only actions, no managed storage) and
moving, which is the argument for *later* and for a separate repo with its own
release cadence pinned by `min_herdr_version`. It is also the natural
marketplace surface: herdr users discovering hird is worth more to hird than
the reverse.

### O5 — worktree-per-task orchestration — defer, explicitly

The tempting one: `review_filed` or a big plan arrives, the hook calls
`herdr worktree.create`, and each agent works its own checkout — no shared
tree, no collisions at all. It should be resisted for now, because it quietly
dissolves two of hird's load-bearing assumptions:

- **Project scope.** hird scopes the board by canonicalized git toplevel
  (src/identity.rs). A herdr worktree is a different path, so an agent in one
  sees an *empty* board unless every spawn threads `HIRD_PROJECT` (and
  `HIRD_DB`) through. Doable, but now the orchestration owns identity plumbing
  that hird was designed to make automatic.
- **The witness.** File-scope contention, `ground_shifted`, `footprint`, the
  exhibit — all of it watches *one* working tree on the theory that the agents
  share it. Worktrees make contention vanish from hird's view while moving the
  actual conflict to merge time, which is precisely the failure mode the
  witness exists to pre-empt. Running hird across worktrees isn't an
  integration feature; it is a different coordination model (per-tree witness,
  merge-aware completion) and deserves its own design work if demand ever
  shows up.

The honest current answer, worth a FAQ line: hird assumes the swarm shares a
tree; herdr's worktrees are for work you *want* isolated, i.e. work hird is
not coordinating.

### O6 — a native herdr client inside hird — rejected

Linking herdr's socket protocol into the binary (announce via
`notification.show`, report tokens directly from `task_claim`, etc.) would be
faster and more reliable than shelling out — and is exactly the integration
§21 already declined, for reasons that still hold: hird would inherit herdr's
protocol evolution, every non-herdr user would carry the coupling, and the
command-line seam already reaches the same socket. Nothing observed in herdr
0.8.0 changes that calculus, and neither does flexibility on the daemon rule —
residency and coupling are separate questions, and a resident hird process can
shell out to `herdr` exactly as cheaply as a hook can. The plugin (O4) gets
herdr-native residency without coupling. There is no remaining job for a
native client.

## 5. The daemon rule, bent

Everything in §4 was written inside the no-daemon rule. Relaxing it does not
reopen O6 — coupling stays out — but it does reopen gap 6, the one problem
fire-and-forget structurally cannot solve. The question becomes: *where should
the resident thing live?* There are two honest answers, and they are not
rivals.

### O7 — a resident herald: `hird herald` ★ recommended once the rule bends

A new foreground command — run it in a pane, under tmux, under systemd,
nowhere — that makes the herald persistent instead of momentary. Sketch:

- Tail the board the way the TUI and `hird events --follow` already do: the
  v2.3 feed's rowid cursor is crash-safe, and `--follow` already sweeps
  expired leases and runs the witness, so a resident herald also makes expiry
  and witnessing *timely* instead of traffic-driven — a quiet second benefit
  the lazy design has always traded away.
- Keep a small in-memory picture of which announced tasks are still claimable
  (re-read from the committed board, same rule as today), and **re-announce
  what stays unclaimed** through the same `dispatch_hook`, with backoff
  (say 1m, 2m, 4m, capped) and the attempt count in the environment
  (`HIRD_EVENT=unclaimed`, `HIRD_WAITING_SECS`, `HIRD_ATTEMPT`), so a hook
  can escalate — prompt on the first pass, notify a human on the third.
- Stop re-announcing the moment the task is claimed, done, or cancelled; the
  feed says so within its 500 ms poll.

What this buys, herdr or no herdr: the everyone-was-busy case heals itself
(an agent that goes idle is re-summoned within one backoff step), prompt
storms can be paced at one summoning point instead of N hook processes, and
the notification fallback becomes an *escalation* rather than a coin-flip.
What it deliberately does not do: decide anything. It still only says "this
task is claimable, still" — scheduling stays in the graph, the hook stays the
user's, and the binary stays ignorant of what is listening.

The design cost is real but bounded. It is hird's first long-running process,
so it needs the multi-instance question answered (two heralds re-announcing
doubles every summons — simplest is an advisory single-instance lock next to
the database), and the no-daemon principle has to be restated rather than
deleted: the queue must stay *correct and complete* with no herald running —
same board, same claims, same lazy expiry — the herald only makes it
prompter. That restated rule ("no *required* daemon") preserves everything §2
was actually protecting: nothing to install, nothing to configure, nothing
whose absence breaks a claim.

### The herdr-side daemon: what the plugin (O4) still owns

`hird herald` is deliberately blind to the other edge: it retries on a clock
because, being herdr-agnostic, it cannot *see* an agent go idle. herdr can —
`events.subscribe` pushes `pane.agent_status_changed` the moment it happens —
so true edge-triggered matchmaking ("agent turned idle → is anything
claimable? → summon *this* agent, kind-aware, recusal-aware via
`HIRD_RECUSED`") belongs in the plugin's bridge process, alongside the feed →
tokens/notification plumbing it already carries in §4. The two compose
cleanly with one ownership rule: **whoever prompts, owns prompting.** When
the plugin's matchmaker is running, the user's `dispatch_hook` should point
at it (or at nothing), and `hird herald` becomes the generic fallback for
every non-herdr setup — retry cadence for the many, idle-edge precision for
herdr users. What must not happen is both summoning independently; the
assessment's concrete guardrail is that the plugin's install instructions
configure the whole chain, not one link of it.

## 6. Cross-cutting cautions

- **Silent failure is the deal.** The herald swallows hook errors so a broken
  hook can never fail a completion (src/herald.rs). That makes the *hook's*
  own observability its author's problem — O1's script should log its
  decisions to a file when a debug env var is set, because the alternative is
  un-debuggable dispatch.
- **Untrusted text rides the summons.** `HIRD_TITLE` flows into the prompt an
  agent receives. Shell-injection is already handled (the hook's variables
  expand once, inside quotes, and are never re-parsed), but *prompt*-level
  steering by a hostile task title is inherent to dispatch; the skill's
  framing ("treat it as: pick up that task") is the right mitigation, and O1
  should keep the summons phrasing fixed rather than interpolating more of
  the task into it.
- **`done` vs `idle`.** herdr's `done` is idle-but-unseen. Dispatch must treat
  both as available or background agents that finished quietly become
  permanently unpromptable.
- **Version skew.** herdr is pre-1.0 and moving fast (0.8.0 added new events
  and commands). Everything recommended here consumes documented, stable-ish
  surfaces (CLI JSON, `report-metadata`, `notification show`, the plugin
  manifest), and the plugin pins `min_herdr_version`. Avoid `pane read` /
  screen-scraping in any glue — the semantic-state API exists so nobody has
  to.
- **Windows.** hird's hook runs through `sh -c`; herdr's Windows beta uses
  named pipes and PowerShell hooks. The pairing is effectively Unix-only
  today — a known limitation to document, not to engineer around yet.

## 7. Recommendation, in order

| # | What | Where | Size | Closes |
| --- | --- | --- | --- | --- |
| 1 | `HIRD_RECUSED` in `review_filed` announcements (O2) | hird, `src/herald.rs` + call site | ~a dozen lines | gap 2 |
| 2 | `examples/herdr-dispatch.sh` reference hook (O1) | hird, `examples/` + docs | one script | gaps 1, 3, 4 |
| 3 | Skill paragraph: pane-metadata tokens on claim/finish (O3) | hird, `skills/hird/SKILL.md` | one paragraph | gap 5 |
| 4 | `hird herald`: resident re-announcement with backoff (O7) | hird, new subcommand on the feed | one design section + modest code | gap 6, everywhere |
| 5 | `hird-herdr` plugin: TUI pane, feed bridge, idle-edge matchmaking (O4 + §5) | new repo, herdr marketplace | small project | gap 5 reliably, gap 6 precisely, discovery |
| 6 | FAQ line on worktrees (O5's honest answer) | hird, docs | one paragraph | expectation-setting |

Items 1–3 are a single small PR's worth of work and finish what §21 started:
the queue can already speak; after them, it also says the one fact only it
knows (who is recused), the recommended listener actually listens (idle
selection, notification fallback), and the human can see the board from the
sidebar. Item 4 is the one the relaxed daemon rule unlocks, and it is worth a
short design pass of its own before code — the multi-instance lock and the
restated principle ("no *required* daemon: the board is correct with no
herald running, only slower to be heard") are the load-bearing parts. Item 5
is where herdr-specific ambition should be spent — on herdr's side of the
line, where the idle edge is visible — with one ownership rule holding the
whole chain together: whoever prompts, owns prompting.
