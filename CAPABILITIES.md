# Assessment: capabilities per agent type on the central queue

> **Question.** Could `hird-server` take a configuration that assigns
> capability labels per agent type — `opencode` gets one set, `pi` another —
> instead of per roster entry?
>
> **Short answer.** Yes, and most of it already works with no code. The
> roster is per token, a token can be per harness, and a token pins its
> harness; so "one token per person per harness, each with its own labels"
> is the feature, today. What is worth building is small: a table of
> harness-typed *defaults* in the roster that a pinned worker inherits. What
> is not worth building is the version that keys capabilities on the name a
> client gives itself — it would turn a name the design deliberately made
> cheap to be wrong about into a credential.

## What is there now

Three facts from the code decide the shape of any answer.

**Capabilities are per token, fixed at connect time.** `server/src/roster.rs`
maps each bearer token to a `Worker`: an identity, an optional pinned harness,
a project, and a list of capability labels. `Fleet::assemble` in
`server/src/main.rs` builds one MCP endpoint per token and constructs the
`Session` — identity, project, capabilities — inside the service factory, so
the labels are decided before the first request on that session is read.

**The harness name is the one thing a client may say about itself.** When the
roster leaves `harness` unset, `AgentId` stays unnamed until the first
`tools/call` arrives with a `clientInfo.name`, which `HirdMcp::call_tool`
offers to `AgentId::name_from_client`. §29 permits this because *"being wrong
there costs a badge"*: the harness is a colour in a TUI column and the axis
recusal bars on, and the design accepted that a client can mis-name itself
because nothing valuable hangs on the name. The roster's `harness` pin is the
operator taking that half back.

**Nothing lets an agent overstate its environment.** §25 (capability-aware
dispatch) is explicit: *"No capability tool exists for an agent to overstate
its own environment."* Labels are human-controlled tokens — `HIRD_CAPABILITIES`
in a registration the human wrote, or `capabilities` in a roster the operator
wrote. A task that requires `browser` is refused to a session that did not have
`browser` granted by a person.

## The two ways to key it, and why only one is safe

A per-agent-type config has to decide what "agent type" means to the server.
There are exactly two sources for it.

### A. The client's own name — do not build this

```toml
# The unsafe shape: capabilities follow whatever the client calls itself.
[harness.opencode]
capabilities = ["browser", "linux"]
[harness.pi]
capabilities = ["linux"]
```

applied to an *unpinned* worker, so that a session announcing itself as
`opencode` gets the first set and one announcing `pi` gets the second.

This is mechanically possible and wrong three times over.

1. **It contradicts §25.** The `clientInfo.name` is set by the harness
   software and forwarded by any MCP client that cares to; a session that
   wants `browser` sends `name: "opencode"`. The capability set would be
   chosen by the party the design says may never choose it.
2. **It inverts the timing.** Capabilities are fixed when the session is
   built; the client's name arrives on the first tool call. The labels would
   have to become a `OnceLock` resolved in `call_tool`, and the `instructions`
   text — which today tells the agent exactly what it advertises — would be
   served before the answer is known. §30 fixed every field of a `Session`
   for the life of the connection on purpose; this would reopen one of them.
3. **It fails quietly.** The names `opencode` and `pi` send are not something
   hird controls, and this assessment could not verify them offline. A harness
   release that changes its `clientInfo.name` drops that harness's
   capabilities on the next connect, and the symptom is tasks marked
   `incompatible` with no error anywhere.

There is a bounded variant — per-token *named sets*, where the operator lists
`{ opencode = [...], pi = [...] }` on one token and the client's name only
selects among sets the human already granted to that token. It stays inside
§25 because the ceiling is still operator-set. It also inherits problems 2 and
3, and it exists to serve one token across many harnesses, which the roster
already discourages for a different reason (see below). Not a first step.

### B. The roster's pinned harness — this works, and is small

The harness the *operator* pins is a fact the operator asserted, so anything
derived from it is still human-controlled. A harness-typed table of defaults,
applied to workers that pin that harness:

```toml
project = "/srv/acme"

# Labels every worker of this harness type starts with. Applied only when a
# worker pins `harness`; the client's own name never reaches this table.
[harness.opencode]
capabilities = ["browser", "linux"]

[harness.pi]
capabilities = ["linux"]

[[worker]]
token = "…"
identity = "ana"
harness = "opencode"          # inherits browser, linux
capabilities = ["macos"]      # and adds its own

[[worker]]
token = "…"
identity = "ana"
harness = "pi"                # inherits linux
```

Semantics worth fixing before writing it:

- **Union, not override.** A worker's own `capabilities` are added to the
  harness defaults. Subtraction is not needed: a worker that should have
  fewer labels than its type pins a different (or no) type.
- **Only pinned workers inherit.** A worker without `harness` gets no
  defaults, whatever the client later calls itself. That is the whole safety
  argument in one rule, and it should be a test.
- **Match after the same normalisation `AgentId` applies to the pin**, so
  `OpenCode` in one place and `opencode` in another do not silently miss.
- **Validate at parse.** Today a bad label in the roster is only discovered
  when that worker connects, because `Session::new` normalises and the roster
  parser does not. Refusing it at `hird-server` start is the same posture the
  parser already takes for short tokens and missing projects, and it should
  land with this change whether or not the table does.

Size: a `harness` map on `RosterFile`, a merge step in `Roster::parse`, four
tests, and the example, README §"The central queue over HTTP", DESIGN §31 and
`docs/server.html` roster table updated. Well under a day.

## What works today, with no code

The user-visible feature — Ana's OpenCode gets `browser`, Ana's pi does not —
is one token per (person, harness):

```toml
[[worker]]
token = "…"
identity = "ana"
harness = "opencode"
capabilities = ["browser", "linux"]

[[worker]]
token = "…"
identity = "ana"
harness = "pi"
capabilities = ["linux"]
```

Each harness is registered with its own token, both act as `ana`, the actors
are `ana/opencode:…` and `ana/pi:…`, and recusal — which bars by harness — can
tell them apart. Option B is a compression of this, not a new power; it earns
its place when the roster is people × harnesses and the same three labels are
being retyped.

This shape is also the one the server should prefer for a reason unrelated to
capabilities: an unpinned harness on a shared queue lets the client choose the
name recusal keys on, so a client could in principle read its own work by
calling itself something else. Locally that is the human's own environment and
§29's "costs a badge" holds; on the server it is a stranger's request. One
token per harness makes pinning the natural default, and the harness defaults
table only rewards pinning further.

## A caveat on what a "type" can say

Capability labels were introduced for the *environment* — `browser`, `macos`,
`gpu.cuda` — and an agent type does not determine the environment: `opencode`
on a laptop and `opencode` on a headless CI box are the same type with
different labels. Harness-level defaults suit labels that are properties of
the harness or model rather than the machine — `web-search`, `vision`,
`subagents`, `long-context` — and hird's vocabulary is the operator's, so
both kinds are legitimate. The merge rule above (type defaults ∪ worker's own)
is what keeps machine labels per worker while letting harness labels be said
once.

## Recommendation

1. **Now:** document the per-(person, harness) token pattern in
   `examples/roster.toml` and the README; it is the feature, and it is
   invisible.
2. **Build:** the `[harness.<name>]` defaults table, union semantics, applied
   to pinned workers only; and validate roster capability labels at parse
   time, in the same change.
3. **Do not build:** capabilities keyed on `clientInfo.name`. If a single
   token that spans harnesses turns out to be a real need, the per-token named
   sets variant is the bounded form, and it should come with its own §-entry
   in DESIGN.md explaining why the client gets to *narrow* and never to widen.
