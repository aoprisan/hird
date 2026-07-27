#!/usr/bin/env bash
#
# A plan as a file: read it, file it, edit it, file it again.
#
# swarm-plan.sh files the same shape of graph with `hird add` calls and shell
# variables. This does it from a file that can be reviewed, committed beside
# the code it describes, and applied twice.
#
#   ./examples/plan-file.sh
#
# Runs against a throwaway database, so it cannot touch your real queue.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

plan="$(dirname "$0")/plan.toml"
work="$(mktemp -d)/plan.toml"
cp "$plan" "$work"

# ------------------------------------------------------------- read it first

say "hird plan apply --dry-run — what filing this would do"

# Nothing is written. The waves come from the same code `hird graph` prints,
# so what you read here is what the board will say.
#
# The two lines below the waves are the reason a plan file is worth having:
# hird can intersect two globs before either file exists, so it can name the
# tasks that look parallel and will in fact be handed out one at a time — and
# the ones that told the queue nothing about their files at all.
hird plan apply "$work" --dry-run

# ------------------------------------------------------------------- file it

say "hird plan apply — the whole graph, in one transaction"

# Either all of it lands or none of it does. A shell script that dies halfway
# leaves real tasks behind, missing exactly the dependencies that were going
# to keep them in order.
hird plan apply "$work"

say "hird graph — the same waves the preview promised"

hird graph

# ------------------------------------------------------ agents pull from it

say "two agents: \"work the queue\""

# Nothing about a planned task is special. `task_next` hands out the most
# important workable one: the schema wins on priority, and the port waits for
# it, so the second agent gets the renderer instead.
mcp codex <<JSON
$(call 1 task_next '{}')
JSON

mcp claude-code <<JSON
$(call 1 task_next '{}')
JSON

# --------------------------------------------------------------- edit and re-run

say "the plan grows a task, and is applied again"

cat >>"$work" <<'EOF'

[[task]]
name = "changelog"
title = "Update the changelog"
paths = ["CHANGELOG.md"]
needs = ["notes"]
EOF

# Each task is remembered under the name the plan gave it, so this files the
# new one and leaves the five already in flight exactly as they are — claims,
# history and all.
hird plan apply "$work"

say "hird ls"

hird ls

say "next"

cat <<EOF
The plan lives at $work; edit it and run \`$HIRD_BIN plan apply\` again.

Watch it live:                 HIRD_DB=$HIRD_DB $HIRD_BIN tui
The same graph, filed by hand: ./examples/swarm-plan.sh
The annotated plan format:     examples/plan.toml
EOF
