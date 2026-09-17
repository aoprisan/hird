# Assessment: TypeSafe AI in hird

> **Question.** A Rust client for TypeSafe AI's System One API now exists —
> [`typesafe-sdk`](https://github.com/aoprisan/typesafe-ai-rust-sdk), merged
> today. Is there a useful place for it in hird, and where is the opportunity?
>
> **Short answer.** Not inside the `hird` binary, on three grounds that are
> each sufficient alone. Not inside `hird-server` either, for the third of
> those grounds. **Yes at the two seams the design already built for outside
> judgment** — the hooks and the JSON readings — where it needs no change to
> hird at all. The opportunity is a small companion program that answers
> System One questions at those seams, not a dependency. The most useful one
> is a *router* for the dispatch hook, which is the one place hird's own
> roadmap says routing belongs.

*Written against hird v0.2.2 and `typesafe-sdk` 0.1.0 (commit `b6e6a42`).
An assessment, not a specification; nothing here is built.*

## What System One is, and what it is not

The SDK's own description is exact: *send a `state` plus named, typed
questions and get typed answers back*. Three question kinds, and only three:

| Question | Answer |
|---|---|
| `Noul` | probability of "yes" (0–1) |
| `Choice` | one label out of N, per-label probabilities, a confidence |
| `Score` | a probability-weighted level on an ordered scale, a confidence |

That is a **classifier**, not a generator. It does not write a task body,
summarise a review or draft a plan; it decides among labels the caller named
and says how sure it is. The `state` is anything `Serialize`, so hird's own
JSON drops in unchanged. The client is `reqwest` + `rustls`, one call is one
HTTP attempt with a 10 s timeout and a 30 s retry budget, `TYPESAFE_API_KEY`
comes from the environment, and a `blocking` feature gives a synchronous
client for short-lived processes. MSRV 1.88, the same as hird.

The shape matters because *deciding among labels* is precisely the family of
decisions hird's design has spent thirty-one sections assigning to somebody
who is not hird.

## Three gates inside `hird`, each sufficient alone

**1. The HTTP gate.** `AGENTS.md` is categorical — *"`hird` must never gain an
HTTP dependency, and CI fails if `cargo tree -p hird` names one"* — and
`.github/workflows/ci.yml` enforces it by grepping the normal-dependency tree
for `axum`, `hyper` and `tower-http`. `typesafe-sdk` depends on `reqwest`,
and `reqwest` is built on `hyper`; the SDK's `reqwest-client` feature only
exposes the type, it does not remove the dependency. The rule held even for
`hird web`, which is a hand-rolled loopback listener on `std::net` for
exactly this reason. Adding the SDK to the root crate fails CI on the first
push, and rightly.

**2. Local-first, deterministically.** Every claim gate — lease, dependencies,
question, recusal, requirements, overlap — is a check on stored state, and
§26 makes that a stated property: `hird why` runs *"the identical repo
checks"* a claim runs, so *"disagreement between `why` and a real claim would
be a bug in one of them."* A probabilistic gate cannot keep that promise. And
a gate that needs an API up makes claiming depend on a vendor, which the
roadmap's first constraint — *no server an agent depends on* — forbids; an
API key is also an account, which the README promises hird has none of.

**3. Reports, not verdicts; no router; nothing steered by inference.** This
is the one that survives even if the first two were engineered around. Take
every question System One could plausibly be asked about a queue, and look
up where the design already put the answer:

| What System One could decide | Where hird's design already put it |
|---|---|
| Which worker should take this task | The dispatch hook — the user's command. *Never:* a router. |
| What capabilities this task needs | The plan author, or the `task_split` caller (§25: labels are human-granted). |
| Is this assertion still true now that the file moved | Footing says *unverified*, never *false* (§14). |
| Are these two assertions the same fact | *Never:* merged similar assertions. |
| Should this review be upheld or sent back | The recused reviewer, a *different* harness (§15, §16). |
| How urgent is this task | `priority`, a human's integer. |
| Is this one task or several | The holder, via `task_split`; `plan lint` reads structure, not prose, so lint and parse cannot drift (§26). |
| Who typed this change | The witness says what moved, not who (§12). |
| Is this question answerable without the human | §24: the question *is* the human's; waking anything else hands the stall around. |

Every row is a decision assigned, on purpose and with a section number, to a
human, to a differently-modelled agent, or to a deterministic read. A
classifier answering any of them from inside hird would not be a feature. It
would be a reversal of the entry that put the decision elsewhere.

## `hird-server`: allowed by the split, still not a fit

The crate split permits HTTP in `server/` — it already carries `axum`. But
the server is the same twelve tools behind a bearer token; the only facts it
holds that the local binary does not are the roster's. The one server-shaped
temptation is inferring a harness or a capability set from what a task or a
client *says*, and `CAPABILITIES.md` has just declined to key capabilities on
the client's own name because *"the capability set would be chosen by the
party the design says may never choose it."* A model's guess is the same
party with a lower floor. Gate 3 applies unchanged.

## The seams that already take outside judgment

Two places in hird are, by construction, where a judgment made *outside*
hird is the user's business and hird's job is only to supply the facts.

### The hooks

`dispatch_hook` and `question_hook` run a command detached through `sh -c`
with the announcement in its environment — `HIRD_EVENT`, `HIRD_TASK`,
`HIRD_TITLE`, `HIRD_PROJECT`, `HIRD_RECUSED`, `HIRD_REQUIRES`, `HIRD_DB`, and
for questions `HIRD_QUESTION` and `HIRD_ASKED_BY` — stdio closed, failures
swallowed, nothing read back. `examples/dispatch-hook.sh` says it plainly:
*"What the command does is your business."* And the roadmap's *Never* entry
for a router ends: *"The dispatch hook maps labels to workers, and that
mapping belongs to the user's command, not to hird."* A hook is exactly where
a classifier may route, because hird has already disclaimed routing there.

**Routing a summons — the real opportunity.** The herdr plugin's
`dispatch.sh` walks a roster in preference order and wakes the first worker
that is not recused, advertises every required capability, and is not busy.
Preference order is the whole placement policy. A `Choice` over the task,
with the roster's surviving workers as labelled options —

```rust
let res = typesafe::blocking::Client::from_env()?
    .system_one(
        json!({ "title": title, "body": body, "requires": requires, "event": event }),
        Questions::new().with("worker", Choice::new("Which worker is the better fit for this task")
            .option("codex",  "Strong at mechanical Rust refactors and test suites")
            .option("claude", "Strong at design changes that touch several modules")),
    )
    .send()?;
let pick = res.choice("worker").unwrap();
```

— picks among them with a confidence, and below a threshold the hook falls
back to roster order. Two rules keep this on the right side of the line:

- **hird's facts filter; the classifier only breaks ties.** `HIRD_RECUSED`
  and `HIRD_REQUIRES` are applied *before* the question is asked and never
  overridden by the answer. A router that lets a model reconsider a recusal
  has rebuilt, outside hird, the router hird refused to be.
- **The answer goes nowhere hird acts on.** The hook wakes a worker; the
  worker still calls `task_claim`, and the queue still runs every gate. A
  wrong pick costs a summons, not a claim.

It fits the mechanics too. Hooks are detached, so a 10 s attempt — 30 s in
the worst retried case — costs hird nothing; the SDK's `blocking` client
exists for a process that makes one call and exits; and the `state` is JSON,
which is what hird already emits.

**Triaging a question — a narrower fit.** `question_hook` fires when a task
parks on a human question. The tempting `Noul` — *can this be answered
without the human?* — is wrong by §24: the question is the human's, full
stop. What does fit a hook is a `Score` on urgency that chooses the
*channel*: a desktop notification now versus the next `hird digest`. Choosing
how to reach the human is a hook's business; answering for them is not.

### The readings

`hird graph --json`, `hird events --json` and `hird mem export` exist so that
tooling outside hird can build on the trail. A reading that asks System One
about *prose* — is this task body self-contained for someone who has not
seen the session (`Score`); does this plan file's task set split one job too
fine or too coarse (`Choice`) — is the lint §26 deliberately kept out of
`plan lint`, which reads structure so that it cannot drift from the parser.
Outside hird it is a report, and it feeds nothing. The value is modest: the
human reads the plan anyway. Lower priority than the router.

A second reading is worth naming for the trap in it. `hird record` counts
verdicts per harness; classifying each send-back's findings by kind —
correctness, style, scope — via `Choice` would make the record say *why* work
comes back, not just how often. The roadmap keeps *"anything that feeds it
back into dispatch"* on the *Never* list, so such a tool prints and never
writes. That is also the shape in which it is safe.

## What it costs, and what is not known

- **The task body leaves the machine.** A router sends the title and body to
  a hosted API. hird's local-first claim stays true — the account and the
  transfer are the hook's, opt-in, in the user's config — but the companion's
  README has to say so in its first paragraph.
- **Price and latency per call** were not verifiable offline. Volume is low
  by construction: one call per claimable transition, which is one per
  announcement on the trail, not one per tool call.
- **Whether it routes better than roster order is empirical**, and hird
  already holds the instrument: `hird record` reports whose work survives a
  review, per harness, and a queue run under each policy for a month answers
  the question with data. The record measures, the hook decides, hird stays
  out — which is the division the roadmap wrote down before this API existed.
- **The SDK needs nothing new.** `blocking`, structured `state`,
  `RetryPolicy::none()` and a short timeout for a hook that should not linger
  are all there.

## Recommendation

1. **Do not add `typesafe-sdk` to `hird` or `hird-server`.** Recorded here so
   the absence reads as a decision. No DESIGN.md entry is needed: nothing in
   the design changes.
2. **Build the router as a companion, outside this repository.** The smallest
   form that keeps every piece where it belongs: `dispatch.sh` in the herdr
   plugin gains an optional `HIRD_HERDR_ROUTER` command; after filtering the
   roster by recusal, capabilities and busyness it hands the survivors to
   that command on stdin with the task facts in the environment, takes the
   first line it prints, and falls back to roster order on empty output or a
   non-zero exit. The command is a ~60-line `blocking` binary living in the
   SDK repository's examples. The plugin stays shell-only, the SDK-using
   binary can live anywhere, and hird is untouched.
3. **One small change to hird would help, and only one:** `hird show <seq>`
   prints prose and has no `--json`, so a hook that wants the body today has
   to parse it or open `HIRD_DB` itself. A `--json` on `show`, emitting the
   same detail `task_get` already serialises, is a reading in the §26 sense
   and adds no tool. Worth doing whether or not the router is.
4. **Skip the readings tool** until somebody wants it; skip the question
   triage until the router has earned its keep.
