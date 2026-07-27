#!/usr/bin/env bash
#
# Footing: what a fact was learned against, and whether that has moved.
#
# One agent works out something true about a file and writes it down. Weeks
# later the file has been rewritten and the fact is still there, in the same
# confident voice, waiting to mislead whoever reads it next. That is the
# failure mode of every agent memory there is, and nothing about the sentence
# gives it away.
#
# The code gives it away. Watch hird notice.
#
#   ./examples/footing.sh
#
# Runs against a throwaway database *and* a throwaway git repository, so it
# cannot touch your real queue or your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "two tasks in the config loader, weeks apart"

port=$(hird add "Port the config loader" --path 'src/config.rs')
audit=$(hird add "Audit the config loader" --path 'src/config.rs')

hird ls

# ------------------------------------------------- the first agent learns something

say "codex works the first one, and writes down what it worked out"

session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port, \"paths\": [\"src/config.rs\"]}"

edit src/config.rs '// the loader: env first, then the file'

# No `paths` argument: `task_seq` is enough. hird knows what that task declared
# and what the witness saw it change, and anchors the fact to those files —
# `anchored_to` in the answer is hird telling the agent what it recorded.
session_call codex 2 mem_store \
    "{\"content\": \"the loader reads the env var before the config file\", \"task_seq\": $port}"

# Finishing settles it. The fact is a statement about the tree this task is
# leaving behind, not the one it found halfway through its own edits — without
# this step every task would mark its own facts shaky by its own hand.
session_call codex 3 task_complete "{\"seq\": $port, \"result\": \"ported\"}"
session_close codex

say "right now, the fact stands on solid ground"

hird mem standing

# ------------------------------------------------------------ the ground moves

say "somebody rewrites the loader — a refactor, a rename, who knows"

edit src/config.rs '// the loader: config file first, env only as a fallback'

# Nobody told hird. Nobody had to: the fact remembers which file it came from
# and what that file said, and the file no longer says it.
say "nobody told hird — and hird noticed anyway"

hird mem standing --shaky

# ------------------------------------------------ the next agent is warned unasked

say "the next agent claims work in those files"

session_open claude claude-code
# `recalled` already handed earlier work's facts to whoever picks up the same
# territory. What is new is `standing` and `caution`: the fact still arrives,
# and it arrives labelled.
session_call claude 1 task_claim "{\"seq\": $audit, \"paths\": [\"src/config.rs\"]}"

say "it reads the file, finds the fact no longer holds, and says so"

# The wrong answer would be to store a second, contradictory assertion and
# leave both on the board. `mem_store` the truth; the old one is superseded by
# the human or by whoever notices, and the new one gets a footing of its own.
session_call claude 2 mem_store \
    "{\"content\": \"the loader reads the config file before the env var\", \"task_seq\": $audit}"

session_call claude 3 task_complete "{\"seq\": $audit, \"result\": \"audited\"}"
session_close claude

say "one fact is behind the code now; the one just recorded is not"

# The old fact is still shaky, because the file really has moved since it was
# written. The new one is firm even though the same file moved *during* the
# task that recorded it — finishing settled it against the tree it left behind.
hird mem standing

# ------------------------------------------------------- the way back to firm

say "and the other direction: a fact that turns out to still hold"

hird mem add "the loader lives in src/config.rs" --path src/config.rs

say "somebody tidies the file, so everything anchored to it goes shaky"

edit src/config.rs '// the loader: config file first, env only as a fallback (tidied)'
hird mem standing --shaky

say "an agent in another harness checks one of them, and says it again"

# Saying it again is the only way anyone has to say "I checked". So saying it
# again is what hird made mean that: no duplicate row, no lost provenance —
# the original is re-anchored to today's code and this agent is recorded as
# another voice for it. `affirmed: true` in the answer is hird saying so.
session_open codex2 codex
session_call codex2 1 mem_store \
    "{\"content\": \"the loader lives in src/config.rs\", \"paths\": [\"src/config.rs\"]}"
session_close codex2

# One fact, back on solid ground, now with two harnesses behind it.
hird mem standing

say "next"

cat <<EOF
Three things happened that nothing else in the room could have done.

The fact codex wrote down was anchored to src/config.rs at a particular
version, without codex naming a single file — the task's declared scope and
what the witness saw it change were enough.

When the loader was rewritten, nobody notified anything. The fact went shaky
because the file no longer hashes to what it hashed to, and every reader after
that — search, the claim's recall, the memory browser — was told so. Note what
hird did *not* say: not that the fact was false. A rename, a reformat and a
rewrite are the same event from here. It said unverified, which is the set
where opening the file pays for itself.

And the way back is the ordinary thing an agent already does. Restating a fact
word for word does not fork the memory; it re-anchors the original and records
a second voice. Two agents in two harnesses saying the same thing independently
is corroboration, and hird is the only thing both of them talk to.

Watch it live:                  HIRD_DB=$HIRD_DB $HIRD_BIN tui   (Memory tab, then f)
Audit everything:               HIRD_DB=$HIRD_DB $HIRD_BIN mem standing
Turn it off:                    memory_footing = false      (see config.toml)
Where the file hashes come from: ./examples/witness.sh
EOF
