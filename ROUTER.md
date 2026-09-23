# Assessment: a hird router over hird-server

> **Question.** Can we build a *hird router* that sits over `hird-server`,
> accepts registrations of agents and harnesses, and, given a task, dispatches
> the proper agent — perhaps with `jev` (TypeSafe AI) making the decision?
>
> **Short answer.** Yes, and most of the decision logic already exists. It has
> to be a **third program**, a client of the queue. It cannot be a feature of
> `hird` or `hird-server`. It is the herdr relay (`herdr-plugin/dispatch.sh`
> plus `route.sh`) generalized from panes on one machine to agents on many.
> `jev` fits as the *tie-breaker among permitted agents*, exactly as
> [ROUTING.md](ROUTING.md) already uses it. It does not fit as the thing that
> decides who is permitted. The hard part is not the decision. It is
> **addressing and liveness**: knowing that an agent on someone else's machine
> exists, is idle, and can be woken. That is the part hird refused to own
> (§21, §23), and it is the router's whole reason to exist.

*Written against the tree at `a5f27e9` (hird-server §31, routed summons
§23, capabilities §25, narrowed jev routing in ROUTING.md). This is an
assessment, and nothing in it is built.*

## Where it may live, and where it may not

The design is explicit three times over:

- §23 rejected a roster of agents in hird's config: *"the moment hird chooses
  whom to wake, it owns liveness, addressing, and a mapping from harness names
  to panes — the scheduler-daemon the design forbids."*
- §31 ends on *"Nothing routes"* for `hird-server`.
- REMOTE.md: problem C *"should be built as a separate thing that consumes
  hird rather than as a version of hird."*

None of those forbid a router. They forbid a router **inside** the queue. The
split that made `hird-server` a separate binary (§31: `cargo tree -p hird`
names no HTTP dependency) extends naturally to a third workspace member:

```
                    ┌───────────────────────────── hird-router ──────────────────────────────┐
  registrations ───►│ registry (agents, harnesses)   decide (bars → jev → order)   wake adapters│──► webhook / ssh+herdr /
  heartbeats    ───►│        ▲                              ▲                                   │    spawn / CI dispatch
                    └────────┼──────────────────────────────┼──────────────────────────────────┘
                     roster.toml (read-only)       claimable set (hird lib, DB read-only)
                             │                              │
                    ┌────────┴──────────────────────────────┴───┐
  agents ── MCP ───►│ hird-server  (tokens → identity, claim CAS) │── SQLite (WAL)
                    └────────────────────────────────────────────┘
```

The router **never claims**. It wakes an agent and says "work hird task 42".
The agent claims through its own authenticated MCP session, and the queue
re-checks recusal and capabilities atomically, as it does today. A wrong
routing decision therefore costs one wasted summons and can never produce a
wrong claim. That property is what lets the router be opinionated while the
queue stays correct.

## What already exists, and what is missing

| Piece a router needs | Status today |
|---|---|
| Who *may* take a task (hard bars) | **Exists.** `HIRD_RECUSED` + `HIRD_REQUIRES` in every announcement, re-checked at claim. `Deps::claimable(seq, clearance)` computes both. |
| Who is *better* for it (soft preference) | **Exists.** `route.sh`: jev `choice` over harness labels, narrowed to the permitted set, 0/1-label short circuits, confidence floor, every failure = no opinion. |
| Who an agent *is* and what it *has* | **Exists.** `roster.toml` in hird-server: token → identity, pinned harness, capabilities (`[harness.X]` defaults). |
| Knowing work became claimable | **Exists, twice.** `dispatch_hook` (edge, push) and `hird events --follow --json` (log, resumable cursor). |
| Which agents are *alive and idle* right now | **Missing.** herdr answers this for local panes. Nothing answers it across machines. |
| How to *wake* a remote agent | **Missing.** Per-harness, and it is most of the build. |
| Remembering that an offer was made, and re-offering on silence | **Missing.** The relay is fire-and-forget. A remote summons can be lost. |

So the router is roughly one-third port (the bars and jev narrowing already
exist in shell) and two-thirds new (registry, liveness, wake adapters, offer
ledger).

## Registration: two registries, not one

The request says "registration of agents and harnesses". These are different
facts with different authorities, and merging them is the trap ROUTING.md
already names.

**Harness types** are the rubric: `claude-code = ambiguous briefs,
architectural decisions…`, `codex = a clear spec carried out exactly…`. That
is the jev page's label set, and it belongs to the router's operator. It is
**not** a capability table. ROUTING.md is right that `[harness.X]` in the
roster describes what a harness *has* (a ceiling the queue enforces), while
the rubric describes what it is *good at* (prose that reorders a walk).
Register harnesses by adding labels to the router's `route.jev`. The doctor
check from `doctor.sh` ports over as "a label no registered agent carries /
an agent whose harness no label describes".

**Agents** are live instances, and they are what actually registers:

```
POST /agents            Authorization: Bearer <the agent's hird-server token>
{ "wake": { "kind": "webhook", "url": "https://…/summon", "secret": "…" },
  "max_concurrent": 1, "ttl_s": 120 }
→ 201 { "agent": "ana/codex:01J…", "harness": "codex", "capabilities": ["linux"] }

PUT  /agents/{id}/heartbeat     (idle | busy)      extends the TTL
DELETE /agents/{id}
```

The rule that keeps this safe mirrors §25 and §29: **registration may declare
how to be reached. It may never declare who the agent is or what it has.**
Identity, harness pin and capabilities are read from the same `roster.toml`
hird-server uses, keyed by the token the agent already holds. A registration
that says `"capabilities": ["browser"]` is ignored (or refused), because the
alternative is an agent widening its own correctness constraint through the
side door. Co-locating the router with the server and giving it read-only
access to the roster keeps one authority, not two. The router gets no
tokens of its own, no accounts, and no issuance.

Liveness is a lease, the same idea the queue already uses for claims: a
registration without a heartbeat within `ttl_s` stops being offered work.
Stale registrations are the router's equivalent of herdr reporting a pane
as gone.

## The decision: bars, then jev, then order

Per claimable task:

1. **Permitted set.** Live, idle, registered agents whose harness is not in
   `recused` and whose roster capabilities ⊇ `requirements`. This is the
   same predicate as `worker_permitted` in `dispatch.sh`, written once in
   Rust against the `hird` library, so it cannot drift from what the claim
   checks.
2. **Short circuits.** 0 agents means the task waits (pull still works, and
   `hird why` explains it). All permitted agents on one harness means there
   is no question to ask. The router skips jev and picks by order or
   least-recently-summoned.
3. **jev** only when ≥ 2 *harnesses* survive: `jev run - --json` over the
   router's page with the `harness:` labels cut to the survivors. This is
   `narrowed_page` from `route.sh`, and the ROUTING.md argument for narrowing
   applies unchanged. Accept the answer only above `confidence`. Otherwise,
   or on any failure, fall back to registration order. As today, an opinion
   can move an agent up and can never rule one out.
4. **Offer.** Wake one agent of the chosen harness and record an offer
   `(task, agent, deadline)`. If the task is still unclaimed at the deadline,
   offer it to the next permitted agent, excluding those already tried. The
   ledger lives in the router's memory. Losing it on restart costs one round
   of duplicate summons, which the claim CAS makes harmless.

**Why ask jev about harnesses and not about agents.** Per-agent labels would
make the page change with every registration. The confidence bar measured by
`jev eval` would stop meaning anything, and the classifier would be choosing
among instances that differ only by liveness, which the router already knows
exactly. The rubric describes *kinds* of agent. If two agents of one harness
genuinely differ (different model, different repo checkout), that difference
is either a capability (the roster) or a distinct harness name. It never
belongs in a hidden per-agent label.

**A feedback loop worth having.** Every review ends in a verdict (§16), and
`hird record` already says whose work survives a reading by a different
model. The router knows which harness it chose, and whether that choice came
from jev or the fallback. Joining the two gives `jev eval` a cases file built
from real outcomes rather than thirty hand-labelled tasks. This is
calibration data, and it should feed that purpose only. It is not a runtime
score (ROUTING.md: "no scores, weights or second-choice lists").

## Feeding it: level-triggered, not hook-triggered

There are two ways to learn that work is waiting:

- **The server's `dispatch_hook`** (`curl` a router endpoint). It has low
  latency, but REMOTE.md already flags hooks on a shared server: *a colleague
  filing a task causes shell execution on your host*. It is also
  edge-triggered, so an announcement the router misses (restart, network
  blip) is lost silently.
- **Reconciliation against the database.** The router links the `hird`
  library (as `hird-server` does), opens the DB read-only, and every few
  seconds computes the claimable set with recused/requires per task. It
  diffs that against the offer ledger. Nothing is lost on restart, no
  shell runs, and a lease that expires is noticed by the same loop, with no
  special event needed.

**Recommend the second, with the hook optional as a nudge** ("reconcile
now"). This needs one small library addition: a public "list claimable tasks
with clearance" alongside the existing per-task `Deps::claimable`. Keep it in
`src/repo/` per AGENTS.md. It is read-only and adds no tool, no schema change
and no HTTP dependency to `hird`.

## Waking: the adapters are the product

A summons must travel to a process that is not listening to MCP. An idle
harness is not holding an open stream, so the MCP spec's server-initiated
messages do not help. Each harness needs an adapter. In rough order of
value:

| Adapter | Reaches | Notes |
|---|---|---|
| `webhook` | Anything the agent's owner fronts with a URL | Generic, HMAC-signed, fixed body. Build first: it makes every other adapter someone else's problem. |
| `ssh+herdr` | A herdr pane on a machine you can ssh to | `herdr agent prompt <pane> …`. This is today's relay, one hop further out. |
| `spawn` | Headless one-shot runs (`claude -p`, `codex exec`) | The agent exists only while working. Registration then describes a *capacity*, not an instance. |
| `ci` | GitHub Actions `workflow_dispatch` or a cloud-session trigger | For agents that live in CI or hosted sessions. |

**The summons body is fixed and carries no task text:** `hird task 42 is
ready; work the hird queue`. The agent reads title and body through its
authenticated MCP session. Forwarding `HIRD_TITLE` would give anyone who can
file a task a way to put words into a colleague's agent's prompt through a
channel that bypasses the queue's identity. REMOTE.md's hook warning,
applied to prompts, is the same rule as `route.sh` refusing anything that is
not a token.

## What it costs, and the risks

- **It is a scheduler, knowingly.** Liveness, addressing, retries. That is
  fine outside hird. It does mean the router needs the operational care hird
  avoided: a process that stays up, a port, logs. If it is down, the system
  degrades to exactly today, because agents can still `task_next` and humans
  can still say "pick up 42".
- **The identity axis.** `HIRD_RECUSED` and recusal bar by *harness*
  (REMOTE.md §"hird has no concept of a person"). With two people both on
  `codex`, the router will happily route ben's review to ana's codex, and the
  queue will allow it. The router cannot fix that. It inherits whatever §29
  decides.
- **jev cost and latency.** One call per multi-harness decision, made
  *before* anything is locked (as in `dispatch.sh`). Price it with `jev
  cost`. The short circuits remove the review-loop case entirely on a
  swarm of two.
- **Remote agents run with the lights off** (REMOTE.md cost 3). Routing more
  work to them makes the unwitnessed share of the board larger. The router
  cannot fix this, but it can report it: "routed to an unwitnessed agent".
- **Drift between two registries.** Roster (authority) and router registry
  (reachability) name the same people. Refusing registrations whose token is
  not in the roster keeps drift one-directional and loud.

## What not to build

- **A router inside `hird` or `hird-server`.** It is §23 and §31 again, and it
  would put an HTTP client and a liveness model into the binaries whose
  claim is that they have neither.
- **Assignment.** The router claiming on an agent's behalf, or the server
  reserving a task for the routed agent. The claim is the only decision,
  and it belongs to the session that does the work. Offer-and-re-offer gets
  the same effect without a second source of truth about who holds what.
- **Capabilities from registration, or from jev.** A jev question "which
  capabilities does this task need?" is ROUTING.md's rejected `requires`
  question. The registry equivalent is letting an agent declare its own
  labels. Both widen a correctness constraint that §25 keeps in human hands.
- **Per-agent jev labels, scores, or ranked fallbacks.** See above.
- **Forwarding task text in the summons.**

## Recommendation

1. **Build it as `router/` → `hird-router`**, a third workspace member that
   depends on `hird` as a library, reads `roster.toml` and the DB read-only,
   and owns its own HTTP stack. The existing CI guard (`cargo tree -p hird`
   names no HTTP dependency) keeps holding unchanged, because the router's
   dependencies land on its own side of the line.
2. **MVP:** agent registration + heartbeat authenticated by the roster
   token; the reconcile loop; the `webhook` adapter; the permitted-set
   predicate; the offer ledger; registration-order fallback. No jev yet.
   This alone delivers "a task is filed on the server and an idle, permitted
   agent somewhere is woken".
3. **Then jev**, ported from `route.sh`: the narrowed page, the 0/1
   short circuits, the confidence floor, and a doctor check for page/registry
   drift. Shell out to `jev run - --json` as the plugin does, until jev
   offers a library API.
4. **Then** the `ssh+herdr` and `spawn` adapters, and the verdict-to-`jev
   eval` cases export.
5. **Before exposing it beyond two people:** the OAuth and tenancy steps
   REMOTE.md already lists for hird-server apply here unchanged, because the
   router's registration endpoint is as much a front door as the MCP one.

The one library change `hird` needs is a read-only "list claimable with
clearance" in `src/repo/deps.rs`. Everything else lives outside the queue,
which is where the design has always said routing belongs.
