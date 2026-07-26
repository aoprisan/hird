#!/usr/bin/env bash
#
# Automatic dispatch: you file a plan, several agents pull from it.
#
# Nobody is assigned anything. Each agent calls `task_next` and is handed the
# most important task that is *actually workable* — open, every dependency
# finished, and no other agent inside its files.
#
#   ./examples/swarm-plan.sh
#
# Runs against a throwaway database, so it cannot touch your real queue.
# Compare with manual-dispatch.sh, which hands work out by number instead.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

# ---------------------------------------------------------------- file the plan

say "the plan"

# Priority sorts what is handed out first. `--needs` decides what can be handed
# out at all. `--path` is how the queue knows two tasks would collide.
schema=$(hird add "Design the storage schema" --priority 3 --path 'src/db.rs')
render=$(hird add "Rewrite the renderer"      --priority 2 --path 'src/tui/**')
audit=$(hird add "Audit the renderer tests"   --priority 1 --path 'src/tui/**')
repos=$(hird add "Port the repository layer"  --priority 2 --path 'src/repo/**' \
    --needs "$schema")
notes=$(hird add "Write the release notes" --needs "$repos,$render")

hird ls

say "hird graph — the same plan as dispatch waves"

# Waves, not edges: how much of this can run at once, and what the critical
# path is.
hird graph

# ------------------------------------------------------- three agents pull work

say "agent one: \"work the queue\""

# One `task_next` call. The queue picks and claims in the same atomic step, so
# two agents calling at the same instant cannot come away with the same task.
# Task $schema wins on priority.
mcp codex <<JSON
$(call 1 task_next '{}')
JSON

say "agent two: \"work the queue\""

# Task $repos is blocked by $schema, which is claimed but not done, so this
# agent gets $render instead.
mcp claude-code <<JSON
$(call 1 task_next '{}')
JSON

say "agent three: nothing left that it can safely take"

# $audit declares the same files as $render, which is live; $repos and $notes are
# still waiting on unfinished work. Rather than hand out a collision, the queue
# comes back idle — and says which of the two reasons it was.
mcp copilot <<JSON
$(call 1 task_next '{}')
JSON

say "agent three again, told not to avoid collisions"

# `avoid_conflicts: false` takes work in strict priority order and leaves the
# agents to sort the overlap out themselves. The claim reports the overlap it
# just created, with the name of the agent already in those files.
mcp copilot <<JSON
$(call 1 task_next '{"avoid_conflicts": false}')
JSON

# ------------------------------------------------- what a claim by number costs

say "naming a number the queue would not have chosen"

# Manual dispatch still works while a swarm is running — and the two ways a
# claim by number can be refused both read as a sentence you can act on.
mcp codex <<JSON
$(call 1 task_claim "{\"seq\": $repos}")
$(call 2 task_claim "{\"seq\": $render}")
JSON

# ------------------------------------------------------------------ the board

say "hird agents — who is where, and where they overlap"

hird agents

say "hird ls"

hird ls

say "next"

cat <<EOF
Three agents, one plan, no assignment. Task $repos becomes claimable the moment
$schema is done; $notes waits for both $repos and $render.

Watch it live:                  HIRD_DB=$HIRD_DB $HIRD_BIN tui
Hand work out by number:        ./examples/manual-dispatch.sh
Refuse overlapping claims:      path_conflicts = "refuse"   (see config.toml)
EOF
