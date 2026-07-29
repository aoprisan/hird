#!/usr/bin/env bash
#
# The verdict — the review closes its own loop.
#
# Recusal put the work in front of a second harness (./examples/review.sh).
# What it did not do is listen to the answer. A review used to end in prose,
# and then a human had to read it, decide it meant "broken", find the task it
# reviewed, and reopen it by hand, carrying the findings across themselves.
#
# Now the review ends in a verdict, and the queue acts on it. `sent_back`
# reopens the work with the reviewer's findings appended to its brief; the
# redo files a fresh review; the loop runs until a review says `upheld`. The
# human watches the board instead of being its transport.
#
#   ./examples/verdict.sh
#
# Runs against a throwaway database *and* a throwaway git repository, so it
# cannot touch your real queue or your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "file the work, marked for review"

port=$(hird add "Port the config loader" \
    --body "Keep the env-var precedence." \
    --review --path 'src/config.rs')

# ------------------------------------------------------------- codex does it

say "codex works it and finishes — the review files itself"

session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port, \"paths\": [\"src/config.rs\"]}"
edit src/config.rs '// ported: env first, then the file'
session_call codex 2 task_complete \
    "{\"seq\": $port, \"result\": \"ported; env still wins over the file\"}"
session_close codex

review=$((port + 1))

# ------------------------------------------------- claude-code sends it back

say "claude-code reviews it, and the work does not stand"

session_open claude claude-code
session_call claude 1 task_claim "{\"seq\": $review}"

# The result is written for whoever redoes the work, because that is exactly
# where the queue is about to put it.
session_call claude 2 task_complete \
    "{\"seq\": $review, \"result\": \"the file path still wins over the env var when both are set; invert the precedence in load()\", \"verdict\": \"sent_back\"}"
session_close claude

say "nobody reopened anything — and the work is open again"

# The findings arrived in the brief. The next agent to claim this task is
# handed them the way it is handed everything else: without knowing to ask.
hird show "$port"

# ------------------------------------------------------------ round two

say "codex takes its own work back up and redoes it"

# The bar was on the review, never on the fix: the author may fix their own
# work, because the redo will be read by somebody else anyway.
session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port}"
edit src/config.rs '// ported: env first, then the file — precedence inverted'
session_call codex 2 task_complete \
    "{\"seq\": $port, \"result\": \"precedence inverted; env wins\"}"
session_close codex

say "finishing round two filed a fresh review, recused the same way"

hird ls

# ------------------------------------------------------------- the loop ends

say "claude-code upholds round two"

session_open claude claude-code
session_call claude 1 task_claim "{\"seq\": $((review + 1))}"
session_call claude 2 task_complete \
    "{\"seq\": $((review + 1)), \"result\": \"env wins now; matches the brief\", \"verdict\": \"upheld\"}"
session_close claude

say "the work, with its rounds on the record"

hird show "$port"

# ------------------------------------------------------------- the record

say "hird record — whose work survives a reading by a different model"

hird record

say "next"

cat <<EOF
Count what you did not do. You did not read the first review, decide it meant
broken, reopen the work, or paste the findings anywhere. The verdict did all
four, and the second round happened because the queue ran it, not because you
remembered to.

The record at the end is the measurement only hird can take: it wrote down who
worked every task and who judged it, so it can say — per harness — how much
work was upheld, how much came back, and how often the first attempt survived.
It is a report, not a scheduler. Nothing routes work by it; deciding what to
do about a harness that ships rework is your call, made over a table instead
of a hunch.

A verdict a human disagrees with is not the last word either: reopen or cancel
the task and the queue follows you — sent_back never overrules a move you
already made.

The other verdict, upheld first pass:  ./examples/review.sh
Where the reviewed file list comes from: ./examples/witness.sh
In a plan file:                          review = true      (see plan.toml)
Watch the rounds live:                   HIRD_DB=$HIRD_DB $HIRD_BIN tui
EOF
