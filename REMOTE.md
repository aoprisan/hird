# Assessment: a hosted service for delegating tasks across machines

*Written against v0.2.2. This is an assessment, not a specification — nothing
here is built. `DESIGN.md` remains the specification; `ROADMAP.md` records
multi-machine sync and a remote transport as the two flagship deferrals. This
document is the work of deciding what, if anything, to buy.*

## The question

> Is there an online hosted service I can use to delegate tasks across
> machines?

Short answer: **yes, but not the kind of service the question implies.** The
category that fits is *dumb object storage plus, optionally, a tunnel* —
roughly $0.10/month — and not any of the managed agent-orchestration or
workflow platforms that market themselves for exactly this. Those fail on
hird's constraints, and they fail for a reason worth stating up front: they
are schedulers and routers, and hird's design forbids both.

The more useful finding is that "across machines" is three different problems
wearing one coat, and they have three different answers.

## Three problems, not one

| | What it means | Wants | Status |
|---|---|---|---|
| **A. Shared state** | File a task on the laptop, work it on the desktop. Both are mine. | Sync | `hird sync`, deferred |
| **B. Remote harness** | A cloud session (Copilot coding agent, Claude Code on the web) works my queue from an ephemeral container. | Transport | HTTP mode, deferred |
| **C. Fleet delegation** | Hand work to machines I don't own, or to other people. | Accounts, tenancy, authz | Out of scope — a different project |

**C is not a gap.** A service with accounts and a roster is on the `Never`
list twice over ("no accounts", "a router"). If that is what is wanted, hird
is the wrong starting point and the honest answer is to say so rather than
to grow it there by increments. Everything below concerns A and B.

They are worth keeping apart because **B does not need sync at all.** A
remote harness reaching a queue that stays local is a reachability problem
with a reachability answer; it introduces no second copy of the state and no
consistency question. Solving B by way of A would be paying the hardest price
in the design for a problem that does not require it.

## What bounds the answer

Four constraints from `ROADMAP.md` do real work here, and three of the five
candidate categories die on them:

- **No daemon, no accounts, no server an agent depends on.** A queue an agent
  can only reach when a vendor is up is not local-first.
- **Pull, not push. No router, no scheduler.** Nothing decides *when* work
  runs or *who* runs it.
- **Twelve MCP tools, six statuses.** A transport may not grow the surface.
- **Reports, not verdicts.** The witness says what moved, not who typed.

`hird web` (v2.9) is the precedent for how far this bends: a loopback,
read-only viewer that dies with the terminal. The roadmap already draws the
line it establishes — between a *screen* and a *transport* — and is explicit
that a transport agents reach the queue through is still the open question.
That line is the one this assessment has to respect.

## The load-bearing technical fact

Everything turns on one query (`DESIGN.md` §5):

```sql
UPDATE tasks
SET status='claimed', claimed_by=?1, lease_expires_at=?2, updated_at=?3
WHERE seq=?4 AND status='open';
```

Zero rows updated means somebody else won. This compare-and-swap is what makes
"nobody assigns anything" safe: N agents race, exactly one claims. It is the
single guarantee the whole swarm model rests on.

**CAS is also the one thing that does not survive naive replication.** An
append-only event trail is CRDT-friendly and merges without coordination —
that is why the roadmap points at it. But claiming is not an append; it is a
consensus. Two machines with async-replicated copies will both observe
`status='open'` and both win. The trail merges cleanly and the queue is still
wrong.

So the real question is not "how do I share a SQLite file" but **"where does
the compare-and-swap happen, and does putting it there cost me local-first?"**

## The categories, assessed

### 1. Managed workflow and orchestration engines — reject

*Temporal Cloud, Inngest, Trigger.dev, Hatchet, Windmill, AWS Step Functions.*

These are the services that come up first when searching for the thing in the
question, and they are the worst fit. They own the task lifecycle: retries,
schedules, routing, placement. hird's six statuses, leases, recusal and
verdicts would become a shadow model duplicated inside somebody else's engine,
and you would be running two queues that disagree during every partition.

They also violate the `Never` list directly rather than incidentally — a
scheduler and a router are the first two entries. Adopting one does not extend
hird; it replaces the part of hird that is the point.

### 2. Managed SQLite/Postgres with replication — works, but breaches local-first

*Turso/libSQL embedded replicas, Cloudflare D1, rqlite, Litestream, ElectricSQL, PowerSync.*

Worth taking seriously because hird *is* a SQLite file, and libSQL embedded
replicas are close to purpose-built for this shape: local reads at microsecond
latency, writes routed to a primary that serializes them.

And that routing is the good news — **the CAS survives.** A claim executed at
the primary is still linearizable against every other machine's claim, so
exactly one agent wins. This is technically sound in a way the naive-sync
story is not.

The cost is not technical, it is architectural: **every mutation now requires
the network and a vendor.** A claim during a flight, a coffee shop with
captive-portal wifi, or a Turso incident is a claim that fails. hird stops
being a local queue with an optional remote and becomes a client of a hosted
database. Also, an embedded replica that has synced late shows a stale board
— which is fine for reads but makes the TUI quietly wrong at exactly the
moments it matters.

Verdict: viable, and the right choice for a *different product* that never
promised local-first. Here it trades the central property for convenience.

### 3. Object storage with conditional writes — **the fit**

*S3, Cloudflare R2, Backblaze B2, GCS, Azure Blob.*

The roadmap guessed this ("ship via S3 like ccsync") before the capability
that makes it properly work existed. It exists now: S3 supports conditional
writes via `If-None-Match` and `If-Match`, returning `412 Precondition Failed`
when the ETag does not match. R2 supports the same and is documented as
strongly consistent globally.

**That is compare-and-swap on storage that is not a service in the sense hird
forbids.** A bucket has no daemon, nothing to administer, nothing that goes
down in a way that is anybody's operational problem, and no accounts beyond a
key in a config file. `hird sync` stays what the roadmap wants it to be: a
command you or a hook runs, moving events through dumb storage.

The shape that falls out is unusually clean, because the two halves of the
state want opposite things and object storage gives each what it wants:

- **The trail needs no coordination at all.** Each machine appends only under
  its own prefix — `events/<machine-id>/<ulid>.ndjson`, immutable segments.
  No two writers ever touch one key, so there is no race to lose. Replay is
  a merge sorted by ULID, and ULIDs already sort by time.
- **Only ownership needs CAS,** and it is one small object per task.
  `claims/<task-id>`: acquire with `If-None-Match: *`, renew and release with
  `If-Match: <etag>`. A 412 is precisely today's "zero rows updated".

The pleasing part is that **hird's lease maps onto this one-to-one.** The
claim object carries the holder and an expiry; a lease that lapses is a claim
object whose expiry has passed, which any reader may overwrite with `If-Match`
— the same lazy sweep, the same self-healing within TTL, the same semantics
described in §5, just with an ETag where the WHERE clause was. Nothing about
the model has to be reinvented to go distributed; the existing model already
has the right shape.

Cost: pennies per month. R2 has no egress fees, which matters if the trail is
replayed often.

### 4. Tunnels — the answer to B, and not a hosted queue

*Tailscale (`serve`/Funnel), Cloudflare Tunnel, ngrok.*

For the remote-harness problem, the correct move is to notice it is not a
state problem. Put `hird mcp` in HTTP mode behind a tunnel and a cloud session
reaches **the queue on your machine** — one copy, still authoritative, no sync,
no CAS question, nothing to reconcile.

Prefer **Tailscale `serve`** over anything public-facing: it keeps the queue
on the tailnet with device identity in front, rather than exposing a writable
MCP endpoint to the internet. `funnel` and Cloudflare Tunnel both put it on
the public internet, and Cloudflare earns that only if you specifically want
Access policies/SSO ahead of it. An unauthenticated public MCP endpoint that
can file, claim and close work is not a tunnel decision, it is an incident.

This is cheap and largely unblocked. But the roadmap's objection to it is
correct, and it is the finding that matters most here — see below.

### 5. Hosted agent-delegation platforms — adjacent, not substitutes

*Copilot coding agent, hosted agent services.*

These delegate to **their** cloud VMs. They do not give you a shared queue
across **your** machines; they give you a fleet you do not own. Useful
alongside hird — such a fleet is a consumer of the queue, which is exactly
problem B — but they do not answer the question asked.

## Three costs that are easy to miss

These are the findings I would want on the table before any of this is built,
because none of them is a storage problem and all three are load-bearing.

**1. The task number collides, and it is the human interface.**
`tasks.seq` is `INTEGER UNIQUE NOT NULL`, minted from a dense counter in
`meta.next_seq`. Task `id` is a ULID and merges fine — but `seq` is the thing
the README's central promise is made of: *"pick up task 42."* Two machines
filing offline both mint 43. Every fix costs something visible: partition the
counter per machine (numbers become sparse and jump), qualify it
(`laptop/43` — the promise gets longer), or renumber on merge (the number
someone wrote down goes stale, which is the worst of the three). This is a UX
cost, not an engineering one, and it is the single most under-priced item in
the sync design.

**2. The witness cannot see the other machine.**
The witness reads *a* working tree, and a contention is defined (§12) as a
path two live tasks both declared, that has moved under both, that they
disagree about. Two agents on two machines editing one file is the exact case
it was built for and the exact case it structurally cannot observe. A shared
queue with two trees means footprints, contention, footing and the exhibit are
all machine-local. The right handling is to **report them as machine-scoped
rather than let them silently go quiet** — a board that shows no contention
because nobody looked is worse than one that says it did not look. Consistent
with "reports, not verdicts": the honest report is *unwitnessed here*.

**3. A remote harness runs with the lights off.**
Same root cause, sharper edge, and the roadmap already says it: a cloud
session has a remote tree, so witness, footing and exhibit are all dark for
it. It can file, claim, work and close — but it cannot be watched, and its
memory arrives without footing. That is not a transport bug to fix later; it
is a semantic hole that the transport creates. Worth shipping anyway, but
worth shipping *labelled*.

## Recommendation

**Do not buy a hosted task-delegation service.** The ones that match the phrase
are schedulers, and a scheduler is the thing this design has refused for
twenty-five sections. Buy a bucket.

Staged, cheapest-and-cleanest first:

1. **Memory sync only.** Assertions are append-only with supersede pointers;
   footing is keyed by file hash. This half is genuinely commutative — it
   needs no CAS, cannot lose a race, and delivers the most obviously good
   thing on its own: *what one machine learned, the other knows.* It also
   validates the whole transport with nothing at stake, since a mismerged
   memory is recoverable and a mismerged claim is two agents in one file.
2. **Queue sync**, on R2 or S3 conditional writes, with the claim-object
   design above. Requires deciding the `seq` question first — that is the
   gate, not the storage.
3. **Transport**, independently and in parallel: `hird mcp --http` behind
   Tailscale `serve`. It does not depend on 1 or 2 and should not wait for
   them.

Running cost across all three: **under $1/month**, plus a Tailscale free tier.
The expensive part is the design work in step 2, and specifically the two
paragraphs about `seq` and the witness — not the infrastructure.

## What would change this

- **If problem C is the real requirement** — delegating to machines you do not
  own, or to other people — none of the above applies. That needs identity,
  authorization and tenancy, it is a hosted product with accounts, and it
  should be built as a separate thing that consumes hird rather than as a
  version of hird.
- **If offline claiming turns out not to matter in practice** — if every
  machine is always online anyway — then Turso's embedded replicas are
  materially less work than the bucket design, because the primary does the
  serialization you would otherwise be implementing against ETags. The
  question to answer honestly first is whether local-first is a real
  requirement or an inherited habit.
- **If S3-compatible conditional-write support regresses** across the
  providers targeted, option 3 loses its foundation and the choice collapses
  back to option 2's trade-off.

## Sources

- [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) · [multi-writer applications on S3](https://aws.amazon.com/blogs/storage/building-multi-writer-applications-on-amazon-s3-using-native-controls/) · [leader election with S3 conditional writes](https://www.morling.dev/blog/leader-election-with-s3-conditional-writes/)
- [R2 consistency](https://developers.cloudflare.com/r2/reference/consistency) · [R2 Workers API](https://developers.cloudflare.com/r2/api/workers/workers-api-reference/)
- [Durable Objects SQLite storage](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/) · [zero-latency SQLite in Durable Objects](https://blog.cloudflare.com/sqlite-in-durable-objects/)
- [Turso embedded replicas](https://betterstack.com/community/guides/databases/turso-explained/)
- [Tailscale Funnel](https://tailscale.com/kb/1223/funnel) · [Cloudflare Tunnel vs ngrok vs Tailscale](https://dev.to/mechcloud_academy/cloudflare-tunnel-vs-ngrok-vs-tailscale-choosing-the-right-secure-tunneling-solution-4inm)
- [Delegating tasks to the Copilot coding agent](https://github.blog/changelog/2026-02-17-delegate-tasks-to-copilot-coding-agent-from-visual-studio/)
