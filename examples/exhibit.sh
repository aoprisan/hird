#!/usr/bin/env bash
#
# The exhibit: the witness keeps what it saw.
#
# One agent finishes real work without committing anything; a second agent
# then writes over the same file, also without committing. Git has nothing to
# show for either. Watch hird produce the finished task's diff after the
# fact, and bring back the version the second write landed on.
#
#   ./examples/exhibit.sh
#
# Runs against a throwaway database *and* a throwaway git repository, so it
# cannot touch your real queue or your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "two tasks that will meet in the same file"

port=$(hird add "Port the config loader"  --path 'src/config.rs')
audit=$(hird add "Audit the config loader" --path 'src/*.rs')

# ------------------------------------------------------------- the first agent

say "codex works the loader and finishes — nothing is committed"

session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port, \"paths\": [\"src/config.rs\"]}"
edit src/config.rs '// codex ported the loader, carefully'
session_call codex 2 task_complete "{\"seq\": $port, \"result\": \"ported it\"}"
session_close codex

say "hird diff — the uncommitted diff of a finished task"

# Git cannot answer this: nothing was committed, and the tree is about to
# move on. The witness kept the versions it fingerprinted, so the diff is
# still here.
hird diff "$port"

# ------------------------------------------------------------ the second agent

say "claude-code writes over the same file"

session_open claude claude-code
session_call claude 1 task_claim "{\"seq\": $audit, \"paths\": [\"src/*.rs\"]}"
edit src/config.rs '// claude-code rewrote it from scratch'
session_call claude 2 task_update "{\"seq\": $audit, \"note\": \"rewriting\"}"
session_close claude

say "the diff is still the task's, not the tree's"

hird diff "$port"

say "hird salvage — the version the second write landed on"

# The disk now says what claude-code wrote. The version codex left is not
# gone: the witness saw it, so the witness kept it.
hird salvage "$port" src/config.rs
echo
hird salvage "$port" src/config.rs --baseline

say "next"

cat <<EOF
The first diff and the second are the same diff: task $port's record ends at
the version its holder left behind, so the tree moving on afterwards changes
nothing about what the task is on record as having done.

The salvage is the witness's headline collision carried one step past
detection. Two agents, one checkout, no commits — the second write landed on
the first agent's work, and the last version the witness saw of it came back
with one command. --baseline asked for the other end: the version the task
started from.

The honest limit is in the name: the witness observes at claims, check-ins
and renders, so a version that came and went between two observations was
never seen and cannot come back. hird says "not kept" in that case rather
than guessing.

Reviews read the same store: work filed with --review completes into a
review whose brief carries this diff, so the reviewing harness judges the
change itself. See ./examples/review.sh for that loop.

Watch it live:                  HIRD_DB=$HIRD_DB $HIRD_BIN tui
Turn it off:                    exhibit = false             (see config.toml)
The detection this builds on:   ./examples/witness.sh
EOF
