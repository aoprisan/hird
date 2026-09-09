# Assessment: a hosted service for delegating tasks across machines

> **Update.** The constraints below were treated as fixed in the first pass.
> Their author has since lifted them, and named the actual target: **a hird
> MCP server he hosts, that his own agent and a colleague's agent both connect
> to, passing tasks between them, with the agents running in different
> environments with different capabilities.** That is problem C, which the
> first pass scoped out on constraints rather than on merit. [§ A hosted
> shared queue](#a-hosted-shared-queue-problem-c-taken-seriously) assesses it
> properly, and the answer to *"does it work out of the box today?"* is **no —
> but not mainly for the reasons auth and encryption suggest.**

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

## A hosted shared queue (problem C, taken seriously)

**Target:** one `hird mcp` on a host you control. Your agent connects. A
colleague's agent connects. You pass tasks between you. The agents run in
different environments with different capabilities.

**Does it work out of the box today? No.** Auth and encryption are genuinely
missing and you named them correctly — but they are the *easy* half, and
fixing only them would produce a server that is worse than broken, because it
would run and give wrong answers.

### The good news first

The queue core needs nothing. Central hosting is **strictly easier than sync**:
one SQLite file on one machine means the claim CAS (§5) is already
linearizable across every connected agent, and none of the distributed-consensus
problem from the earlier sections exists. Atomic claiming, leases and their
lazy expiry, dependencies, readiness, `task_split`, reviews, verdicts and the
sent-back loop all work unmodified. That is most of the value, and it is free.

Two things then go wrong: one architectural, one semantic.

### The blocker: session state is process state

`hird mcp` speaks stdio only — `rmcp::transport::stdio()`, with rmcp's
features set to `["server", "macros", "transport-io"]`. But the missing
transport is the small problem. The real one is that **the design assumes one
process per session**, so everything that ought to be per-*connection* is
per-*process*, read once from the server's own environment and CWD at startup:

| State | Comes from | On a shared server |
|---|---|---|
| Actor identity | `HIRD_HARNESS` + a session id minted at process start, then **latched** | Every client is the same agent |
| Project scope | `HIRD_PROJECT`, else git toplevel of **the server's CWD** | Everyone lands in the host's project |
| Capabilities | `HIRD_CAPABILITIES` on the server | Every agent advertises identical labels |
| Witness | Built from `project` in `HirdMcp::new` | Reads the host's tree for everybody |

The capability row is worth pausing on, because it inverts the feature you
asked for. Capability-aware dispatch (v2.6) reads `HIRD_CAPABILITIES` from the
process; a hosted server has exactly one, so *"agents in different
environments with different capabilities"* becomes "every agent advertises the
host's capabilities." The mechanism exists and is well-shaped — labels are
small, human-controlled tokens — it is just wired to the wrong end of the
connection. It has to become something the client presents at initialization
(and, once there is auth, something the server is willing to believe).

The identity row is the same shape and worse consequences: `AgentId` latches
its harness in a `OnceLock` and is documented as deliberately un-rewritable,
because "an actor string that changed mid-session would leave this process
unable to find its own leases." That reasoning is correct *for one process per
session* and becomes the bug the moment one process holds many.

### The semantic problem: hird has no concept of a person

This is the finding that survives all the plumbing, and it is not fixed by
adding auth.

**Recusal bars a harness, not a session** — §15 says so outright: *"The bar is
the harness, not the session. Two Claude Code windows are one."* That is the
right call for one person running three harnesses, which is what it was
designed for. Invert the situation and it breaks: if you and your colleague
both run Claude Code, hird considers you **the same reviewer**. Your
colleague is refused the review of your work — the exact cross-check the
review loop exists to provide, blocked because the queue cannot tell two
people apart. Whether recusal works at all becomes an accident of whether you
happened to pick different harnesses.

The same axis error runs through everything keyed on the harness: the routed
summons (`HIRD_RECUSED` carries harness names), and `hird record`, which
measures "whose work survives a reading by a different model" — a sentence
that quietly means *a different person* once two people share a queue, and
cannot report it.

So authentication is not only a security control here. **It is the missing
identity axis**, and adopting it means deciding what recusal, routing and the
record are keyed on: harness, person, or the pair. That is a design decision,
not a login screen.

### The tree-reading half goes wrong, not dark

Earlier sections said a remote harness runs with witness, footing and exhibit
*dark*. On a hosted server it is worse than dark: the witness reads the
**server's** working tree and reports about it confidently. Contentions,
footprints, `ground_shifted` and footing would all be computed against a
checkout nobody is editing, or — if the host happens to hold a clone —
against the wrong one. Under the "reports, not verdicts" principle this is the
one real violation in the whole design: a report that is not merely absent but
false.

The honest handling is to **disable the tree-reading half server-side** and say
so, rather than let it answer. The valuable version — each agent witnessing
its own tree and shipping evidence to the shared queue — is a real feature and
a substantial build, not a config change.

### Security: the part not on your list

Two config keys run shell commands on the host: `dispatch_hook` and
`question_hook`, both through `sh -c` with task-derived values in the
environment (`HIRD_TASK`, `HIRD_TITLE`, `HIRD_PROJECT`, `HIRD_RECUSED`,
`HIRD_REQUIRES`). Today that is entirely safe — it is your machine, your
config, your tasks.

On a shared server it means **a colleague filing a task causes shell execution
on your host**, with fields they control in that shell's environment. The
values are passed as environment variables rather than interpolated into the
command, which is the right construction and avoids the obvious injection —
but any hook that expands `$HIRD_TITLE` into another command re-opens it, and
the documented `case ",$HIRD_RECUSED," in` routing idiom is exactly the kind
of thing people extend by hand. This deserves a decision before the port is
open, not after: hooks off by default when serving multiple identities, or a
hard rule that hook input is never interpolated.

Also worth stating plainly: **`project` is a filter, not a boundary.** One
global DB holds every project, tools take an `all_projects: true` escape
hatch, and the project string is supplied by the client's own environment.
Nothing today stops a connected agent reading or claiming another project's
tasks. Multi-tenancy needs server-side authorization bound to the
authenticated identity; the existing scoping is a convenience and was never
built to hold a boundary.

### Auth and encryption — the standard answer

The good news is that none of this needs inventing, because MCP standardized
it:

- **Transport:** Streamable HTTP, which replaced HTTP+SSE. Revision
  2026-07-28 — the one hird already targets — makes the protocol core
  stateless, which suits a multi-client server. rmcp ships a
  streamable-HTTP server feature; hird enables only `transport-io`.
- **Authorization:** OAuth 2.1 with mandatory PKCE, the MCP server acting as a
  *resource server only* and validating tokens from an external authorization
  server. Discovery via RFC 9728 protected resource metadata, RFC 8414 for
  authorization-server metadata, and RFC 8707 resource indicators so a token
  is bound to your server and cannot be replayed elsewhere.
- **Encryption:** TLS terminated at a reverse proxy. No new code.

Bring your own identity provider rather than growing accounts inside hird —
that keeps the authorization server out of the codebase, and it is what the
spec expects.

### What works today, with no code: SSH

Worth knowing before building anything, because it hits most of the target
now.

Point the colleague's MCP client at a stdio command that happens to be remote:

```
ssh queue.example.internal 'HIRD_HARNESS=codex HIRD_CAPABILITIES=browser,linux HIRD_PROJECT=/srv/acme hird mcp'
```

This works **because** of the one-process-per-session design rather than in
spite of it. SSH spawns a fresh `hird mcp` per connection, so identity,
project and capabilities are per-session again — correctly, and per person —
and every column in the table above lands right. You get authentication (keys),
encryption (SSH), and per-agent capability labels, today, against v0.2.2 with
no changes. Both agents share one SQLite file on one host, so claiming stays
atomic.

The caveats are real but bounded: give each person their own account (a shared
one collapses the identity axis again, and recusal still cannot separate two
people on the same harness), lock the keys down with `command=` restrictions in
`authorized_keys` so this grants queue access rather than a shell, turn the
witness off, and set `HIRD_PROJECT` explicitly since the server's CWD is
meaningless here. It also does not reach a browser-based or cloud harness that
cannot spawn an `ssh` command — which is precisely where the Streamable HTTP
build earns its keep.

### Rough shape of the build

In dependency order; the first two are most of the work:

1. **Per-connection session state.** Identity, project and capabilities move
   from process env to connection state, presented at initialization and
   validated server-side. Unavoidable, touches `HirdMcp` broadly, and gates
   everything else.
2. **Decide the identity axis.** What recusal, routing and the record key on
   once a person is distinguishable from a harness. Design, not plumbing —
   and the thing that decides whether a two-person queue reviews correctly.
3. **Streamable HTTP transport** via rmcp's feature, TLS at a proxy.
4. **OAuth 2.1 resource-server validation**, external IdP.
5. **Authorization on project scope**, turning the filter into a boundary.
6. **Hooks and witness off** when serving multiple identities, explicitly and
   loudly.

Steps 3 and 4 are the ones you named. They are perhaps a third of the work,
and none of it is safe to ship without 1, 2 and 6.

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
- [MCP authorization (2026-07-28)](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/authorization) · [the MCP auth spec explained](https://www.descope.com/blog/post/mcp-auth-spec) · [OAuth 2.1 for remote MCP servers over Streamable HTTP](https://mcp.directory/blog/oauth-21-for-remote-mcp-servers-streamable-http-explained-2026) · [authn/authz in MCP](https://stackoverflow.blog/2026/01/21/is-that-allowed-authentication-and-authorization-in-model-context-protocol/)
- [Delegating tasks to the Copilot coding agent](https://github.blog/changelog/2026-02-17-delegate-tasks-to-copilot-coding-agent-from-visual-studio/)
