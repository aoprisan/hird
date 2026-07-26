#!/usr/bin/env bash
#
# Manual dispatch: you file the work, you name the task, one agent takes it.
#
# This is the "pick up task 42" workflow — no dependency graph, no self-service
# queue, nothing handed out behind your back. Automatic dispatch (see
# swarm-plan.sh) was added alongside this, not in place of it: `task_next` is a
# tool an agent chooses to call, so an agent you never point at the queue sits
# idle until you name a number.
#
#   ./examples/manual-dispatch.sh
#
# Runs against a throwaway database, so it cannot touch your real queue.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

# --------------------------------------------------------------- file the work

say "you file the work"

# `hird add` prints the task number and nothing else, so it composes.
loader=$(hird add "Port the config loader to serde" \
    --body "Keep the env-var precedence: --db beats HIRD_DB beats the XDG default.
Tests in tests/config.rs must pass unchanged." \
    --priority 2 \
    --path 'src/config.rs')

# A brief long enough to deserve a file goes in one. `--body-file -` reads stdin.
notes=$(hird add "Write the release notes" \
    --body-file "$(dirname "$0")/task-body.md")

# Priority sorts the board. It gates nothing — negative is fine.
chore=$(hird add "Delete the dead glob helper" --priority -1)

echo "filed tasks $loader, $notes and $chore"
hird ls

# ------------------------------------------------------- hand one out by number

say "you say: \"pick up task $loader\""

# In a harness you say it in English and the agent does the rest. Here we speak
# the same JSON-RPC by hand, so you can see what "pick up task N" costs on the
# wire: one claim, naming the number you said, then the ordinary working rhythm.
#
# It is all one session because a claim is a lease held by *this* agent — only
# the holder may scope, update, complete or fail the task, which is why none of
# these have a CLI verb.
mcp claude-code <<JSON
$(call 1 task_claim "{\"seq\": $loader}")
$(call 2 task_get "{\"seq\": $loader}")
$(call 3 task_scope "{\"seq\": $loader, \"paths\": [\"src/config.rs\", \"tests/config.rs\"]}")
$(call 4 task_update "{\"seq\": $loader, \"note\": \"serde derive in place, porting the env-var layer\", \"status\": \"in_progress\"}")
$(call 5 mem_store "{\"content\": \"Config precedence is --db > HIRD_DB > XDG default; tests/config.rs pins it\", \"tags\": \"config,testing\", \"task_seq\": $loader}")
$(call 6 task_complete "{\"seq\": $loader, \"result\": \"Ported to serde. Precedence unchanged, tests/config.rs green.\"}")
JSON

say "a second agent tries the same number"

# Claims are a single compare-and-set, so the loser is told who holds it — or,
# here, that the work is already finished. Either way it gets a sentence it can
# repeat to you verbatim instead of working the task anyway.
mcp codex <<JSON
$(call 1 task_claim "{\"seq\": $loader}")
JSON

# ------------------------------------------- what the next agent is handed free

say "the next agent in those files is handed what this one learned"

# Nothing above told hird these two tasks were related, and nobody is going to
# call mem_search. But task $loader declared tests/config.rs and recorded a fact
# while it held it, so a task declaring tests/** is working the same territory —
# and the claim comes back with that fact attached, saying where it came from.
audit=$(hird add "Audit the loader tests" --path 'tests/**')

mcp codex <<JSON
$(call 1 task_claim "{\"seq\": $audit}")
JSON

# You can see exactly what your agents are being told, and spot it going stale.
hird recall "$audit"

# ------------------------------------------------------------ what you can drive

say "your overrides work regardless of who holds a task"

hird cancel "$chore" --reason "not worth doing yet"
hird reopen "$chore" --reason "the helper is causing warnings after all"

say "the record"

hird ls
echo
hird show "$loader"
echo
hird mem search config

say "next"

cat <<EOF
Nothing above needed a plan, a dependency or a scope declaration. Task $notes is
still open and unclaimed: it will stay that way until you name it.

Recall needed no plan either — only that task $loader had said which files it was
in before it wrote down what it learned there.

Watch the same database live:   HIRD_DB=$HIRD_DB $HIRD_BIN tui
Let agents pull work instead:   ./examples/swarm-plan.sh
EOF
