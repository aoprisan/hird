# Assessment: a harness router — jev over the agents a task may actually go to

> **Question.** Should the summons be routed by a classifier — `jev` reading
> the task and naming the harness that fits it — and should capabilities take
> part in that choice?
>
> **Short answer.** Yes to both, and the first half already shipped. What has
> not landed is the part that makes it a *router* rather than a hint: the page
> is asked about every harness on the rubric, including the ones the queue has
> already ruled out for this task, so the answer can name a harness that is
> recused or unequipped. That is a routing pass guaranteed to match nobody,
> paid for on every announcement, and — worse — a classification made over
> options that were never available. Narrowing the label set to the permitted
> harnesses before the call is the change worth making: the same two facts
> hird already hands the hook, applied one step earlier. What is not worth
> building is the other direction. A page that says what a task *requires* is
> a model widening a correctness constraint §25 keeps in human hands.

## What is there now

Three facts decide the shape of any answer.

**hird routes nobody, on purpose.** §23 and §25 give the hook two inputs and
no roster: `HIRD_RECUSED`, the harnesses the queue would refuse this task to,
and `HIRD_REQUIRES`, the capability labels its worker must advertise. Both are
re-checked atomically inside the claim. §25 is explicit about the line — *"no
preferred capabilities, scores, automatic installs or fallback attempts. A
requirement is a correctness constraint"* — because the moment hird ranks
workers it owns liveness, addressing and placement, which is the
scheduler-daemon the design forbids. Fit is therefore not hird's question, and
nothing below proposes moving it there.

**The relay already answers it, in the roster's order.** `dispatch.sh` walks
`dispatch.conf` top to bottom and prompts the first worker that is not
recused, carries every required capability, and is not busy. That order is a
preference somebody typed once. It cannot know that this task is a sprawling
refactor and that one a rename.

**The classifier is wired, as a preference and never a permission.**
`route.sh` puts the announcement to a `jev` page as a `choice` over harness
names and prints the winning label; `dispatch.sh` walks the roster twice, that
harness first and then everybody. Every failure — no page, no `jev`, no key, a
timeout, an answer under `HIRD_JEV_CONFIDENCE`, an answer that will not parse
— prints nothing and leaves the walk exactly as it was. The two rules that
keep it honest are that the preferred pass applies all three bars, and that
the unfiltered walk follows it unconditionally, so an opinion can move a
worker up and can never rule one out.

That is a good design for an *opinion*. It is an incomplete one for a router,
because of what the opinion is asked about.

## The question the page is asked is not the question being decided

The page's labels are the harness names its author wrote down. The permitted
set is the roster filtered by the announcement, and it differs per task. The
relay never tells the page which is which, so `routing_state` passes
`requires:` as a line of prose and the model chooses over the whole rubric.
Three things follow.

**A pass that cannot match.** A filed review names its author's harness in
`HIRD_RECUSED`; a task filed with `--requires browser` bars every worker
without the label. If the answer is one of those harnesses — and nothing
stops it from being one, since recusal is the reason the review is being
routed at all — the preferred pass matches no worker and the whole call was
spent to reorder nothing.

**An answer made worse for having been asked wide.** This is the argument
that is not about cost. A `choice` returns a distribution and a confidence
over the labels it was given; a label that cannot be used still takes
probability mass. A three-harness rubric on a review recused from
`claude-code` can answer `claude-code` at 0.55 and `codex` at 0.30 — an
answer over the bar for a harness that is barred, and, once the barred label
is gone, a `codex` that was the clear winner among the two agents that could
actually take it. The relay handles that safely and routes nothing, which is
precisely when routing was most useful: recusal is the one case where the
queue has *already* told the hook that its default order is wrong.

**A call nobody needed.** Every claimable event — `filed`, `unblocked`,
`review_filed`, `sent_back`, `released`, `reopened`, `answered`,
`lease_expired` — is one call. On a swarm of two, a filed review leaves
exactly one permitted harness, so the answer is forced before it is asked.
The common case in the review loop pays for a classification whose outcome
was decided by the roster.

None of the three is a correctness bug. That is worth saying plainly: the
relay is safe as written, and this whole section is about a router that is
slower, dearer and less accurate than the same parts arranged in the other
order.

## Narrowing it: where the two lists meet

The fix is to intersect the page's labels with the harnesses the announcement
permits, and to ask only about what is left.

**What counts as permitted is recusal and capabilities — not busyness.** Those
two are facts about the task that hird computed and will re-check at the claim;
they cannot change between the call and the walk in a way that makes the
narrowing wrong. Busyness is a liveness guess that goes stale in seconds, and
the routing call is deliberately made *before* the relay takes its lock so a
wave of announcements does not queue behind one agent's turn to think.
Narrowing on who was idle a moment ago would put a stale read inside the
question instead of at the prompt, where `lib.sh` keeps it and where it fails
in the cheap direction.

**The mechanism, against the `jev` that exists.** `jev` 0.7 has no flag that
subsets a question's labels for one call: `run` takes `--state`, `--turn`,
`--model`, `--threshold`, `--price`, `--timeout` and `--mock`, and the labels
come from the page. But `jev run -` reads the page from stdin, so the relay
can hand over a page it has cut down instead of a path. The cut is a line
filter over the sketch notation — one `label = description` line per label
under the `harness:` question — and it is **subtractive only**: it removes
label lines and touches nothing else. That is what makes it safe to generate
at runtime. The worst a bad cut can produce is a page `jev` refuses or an
answer that will not parse, and route.sh already spells both of those the same
way it spells everything else that goes wrong, which is silence and the order
you wrote.

**Three cases, and two of them are free.** With *n* labels surviving the
intersection:

- *n* ≥ 2 — ask, over the narrowed page.
- *n* = 1 — do not ask. The answer is forced, and the ordinary walk reaches
  that harness's workers anyway; an opinion here is a call bought to repeat
  the roster.
- *n* = 0 — do not ask. Nothing on the rubric may take this task, and the
  walk will exit quietly on its own.

And when the intersection cuts nothing, send the page by path exactly as
today, so the ordinary unconstrained announcement is byte for byte the call it
already was.

**It also fixes drift, quietly.** The page's labels and `dispatch.conf`'s
third column are two lists of the same names kept in two files. A label naming
no roster harness routes nothing today and is simply cut by the intersection
tomorrow. That is an improvement, not a substitute for saying so: the drift
should be *reported*, which the startup doctor is the place for — a label no
roster harness carries, and a roster harness no label describes. The first can
never route; the second can never be preferred.

## What it costs

**Calibration belongs to the shape you send.** `jev eval` over thirty
hand-routed tasks is what says whether `0.6` is the right bar. A confidence
bar measured on the full rubric does not transfer exactly to a two-label cut
of it — narrower questions are easier and their confidences run higher. This
is an honest caveat and not an objection: the bar's job is to keep an unsure
answer from outranking a human's order, it is tuned for the wide page, and a
narrow page only clears it more often. An operator who wants the number to
mean something on the narrowed shape can eval the narrowed shape. The docs
should say which one they measured.

**The page stops being purely yours.** Today the file `jev check` validates is
the file that gets sent. After this, the file is the rubric and the thing sent
is derived from it. The mitigations are the ones above — subtractive edits,
every failure shaped like no page at all — plus the doctor line, which is what
moves the class of error the cut could introduce back to setup time where a
person is looking.

**Nothing about the claim changes.** The narrowing is a smaller question, not
a new permission. Every worker the answer reaches still clears recusal,
capabilities and busyness in the preferred pass; the unfiltered walk still
follows; the queue still re-checks the hard facts atomically at the claim. An
answer this router cannot give is an answer the relay was going to discard.

## What not to build

**A page that says what a task requires.** The tempting second question —
`requires: which capabilities does this task need` — is out of bounds. §25
puts requirements in human hands (`hird add --requires`, a plan's `requires`,
`hird require`) for the same reason §25 refuses to read them off the claimant:
a requirement is a correctness constraint, and a party that can set it can
clear it. There is a benign relative — a classifier suggesting requirements at
*filing* time, for a human to accept — but that is a reading for `hird plan
lint` to own, in front of a person, not a thing the relay does to an
announcement on its way past.

**Scores, weights, or a second-choice list.** A ranked answer invites the
relay to try three harnesses in order of a model's confidence, which is a
placement optimizer with a roster underneath it. The roster is already the
ranking; the classifier's job is to say when the top of it is wrong for this
one task.

**A router inside hird, or inside `hird-server`.** §23 rejected the roster in
hird's config, and §31 ends on *"nothing routes"*. Nothing here needs it: the
relay has the roster because the relay is the thing that knows how to address
an agent.

**Merging the rubric into the server's capability table.** §31's
`[harness.<name>]` defaults describe what a harness type *has*, and a routing
page describes what it is *good at*. They read like the same table and must
not become one. The first is an operator-set ceiling the queue enforces; the
second is prose that reorders a walk. Putting the rubric where the ceiling
lives is the same mistake as keying capabilities on the name a client gives
itself — it hangs something valuable on a field that was cheap to be wrong
about.

**An answer cache.** Re-announcements of the same task (`released`,
`lease_expired`, `sent_back`) re-ask a question whose state barely moved, and
`jev`'s own `--cache` is an `eval` flag, not a `run` one. A relay-side cache
keyed on a hash of the state would work, but the narrowing removes the calls
that were most obviously wasted, and a cache that has to be invalidated when a
task is edited is a second source of truth for a saving nobody has measured
yet. Price it with `jev cost` first.

## The vocabulary trap, which a router makes worse

Capability labels are the environment: `browser`, `macos`, `gpu.cuda`,
`deploy.staging`. Hird defines no vocabulary, so nothing stops an operator
from writing `refactoring` or `architecture` as a capability and pinning it to
one worker to force work there. Before a router existed that was merely odd.
With one, it is the obvious way to make routing deterministic — and it is the
wrong knob twice: a task whose `--requires refactoring` finds no equipped
worker is `incompatible` and waits forever, where a mis-routed task merely
goes to the second-best agent. The rule is worth one line in the plugin's
docs:

> Something the agent's environment either has or does not — a browser,
> credentials, an OS, a GPU — is a **capability**: hird enforces it and work
> that needs it waits. Something an agent is merely *better at* belongs in the
> routing page's description, where being wrong costs one summons.

## Recommendation

1. **Do:** narrow the label set to the permitted harnesses before the call —
   the intersection, the 0/1-label short circuits, and the unchanged path when
   nothing is cut. `route.sh` plus a helper in `dispatch.sh` that already owns
   both bars, so the eligibility predicate exists once. *(Done in this change,
   with the relay's behavioral check grown to cover it.)*
2. **Do:** report page/roster drift and the cut in `doctor.sh` — labels no
   roster harness carries, roster harnesses no label describes. *(Done.)*
3. **Do:** say which knob is which — capability versus rubric — in the plugin
   README. *(Done.)*
4. **Do not:** a `requires` question, ranked answers, a router in hird, a
   rubric merged into the server's capability table, or an answer cache. If
   the cost table later says the calls are the problem, the cache is the
   bounded form, and it should arrive with the measurement that motivated it.
