#!/usr/bin/env bash
#
# The question: work that knows why it is waiting.
#
# An agent reaches a decision no coding agent should guess, releases the task
# with that question, and the queue stops handing it around. The human answers;
# the next claim receives both voices without knowing to search the history.
#
#   ./examples/question.sh
#
# Runs against a throwaway database, so it cannot touch your real queue.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

say "an agent reaches the human edge of the work"

task=$(hird add "Migrate the config format")

mcp codex <<JSON
$(call 1 task_claim "{\"seq\": $task}")
$(call 2 task_release "{\"seq\": $task, \"reason\": \"the migration branch is isolated\", \"question\": \"Must the old format remain readable?\"}")
JSON

say "the task is open, but another agent is not allowed to guess"

hird ls
hird show "$task"

mcp claude-code <<JSON
$(call 1 task_claim "{\"seq\": $task}")
$(call 2 task_next '{}')
JSON

say "the human answers; that answer is now part of the handoff"

hird answer "$task" "Yes; keep it readable for one release."

mcp claude-code <<JSON
$(call 1 task_claim "{\"seq\": $task}")
JSON

say "next"

cat <<EOF
The first refusal is the point: task_release without a question would have
made the work immediately claimable, sending another agent into the same
missing decision. With a question it remains open but outside dispatch.

The final claim carries 'questions' with both the question and its answer.
The successor does not have to inspect the event trail or rely on chat context.

Answer from the board instead:       HIRD_DB=$HIRD_DB $HIRD_BIN tui
The ordinary handoff:                ./examples/tenure.sh
EOF
