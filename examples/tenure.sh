#!/usr/bin/env bash
#
# The tenure: a task remembers every hand that held it.
#
# One agent starts a task, leaves uncommitted edits in the tree, and is gone —
# session over, nothing committed, no note left behind. Another harness picks
# the task up. Watch the claim itself tell the successor whose leavings it is
# standing in, and watch the first attempt stay diffable and salvageable after
# the second has written over all of it.
#
#   ./examples/tenure.sh
#
# Runs against a throwaway database *and* a throwaway git repository, so it
# cannot touch your real queue or your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "one task, about to pass through two pairs of hands"

port=$(hird add "Port the config loader" --path 'src/config.rs')

# ------------------------------------------------------------- the first agent

say "codex gets halfway and hands the task back — nothing committed"

session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port, \"paths\": [\"src/config.rs\"]}"
edit src/config.rs '// codex got halfway through the port'
session_call codex 2 task_update "{\"seq\": $port, \"note\": \"halfway\"}"
session_call codex 3 task_release "{\"seq\": $port, \"reason\": \"session ending\"}"
session_close codex

# A lease that expires does the same thing without the courtesy: the task
# returns to the pool, and the edits stay in the tree with nobody to explain
# them.

# ------------------------------------------------------------ the second agent

say "claude-code claims the same task — read the 'previously' field"

# This is the hand-over. The re-claim archives the first holding as a tenure
# instead of destroying its record, and the claim answer says who held the
# task, how that holding ended, and which files moved under them — because
# whatever they left uncommitted is in the tree this claim starts from,
# looking exactly like code that was always there.
session_open claude claude-code
session_call claude 1 task_claim "{\"seq\": $port}"

say "claude-code starts over, writing over what codex left"

edit src/config.rs '// claude-code rewrote it from scratch'
session_call claude 2 task_update "{\"seq\": $port, \"note\": \"starting over\"}"
session_close claude

# ----------------------------------------------------------------- the record

say "hird show — the board tells the story of the hand-over"

hird show "$port"

say "hird diff --tenure 1 — what round one did, unpolluted by round two"

hird diff "$port" --tenure 1

say "hird salvage --tenure 1 — the version the rewrite landed on"

hird salvage "$port" src/config.rs --tenure 1

say "next"

cat <<EOF
The claim answer is the point. The successor did not have to know to ask —
the queue is the only participant that saw both holdings, so it speaks at the
one moment the new holder is guaranteed to be listening.

The diff and the salvage are the archive at work: round one's footprint used
to be destroyed by the very claim that most needed it. Now each holding is a
tenure — who, how it ended, what moved — and 'hird show' numbers them.

The same record covers every way a task changes hands: released, lease
expired, completed and reopened, sent back by a review. A redo sent back by
'verdict' (./examples/verdict.sh) keeps its earlier rounds readable, so the
reviewer of round two can still see what round one actually did.

Watch it live:                  HIRD_DB=$HIRD_DB $HIRD_BIN tui
The watching this rides on:     ./examples/witness.sh
The store it reads from:        ./examples/exhibit.sh
EOF
