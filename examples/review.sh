#!/usr/bin/env bash
#
# No agent reviews its own work.
#
# Every result line in the queue was written by the agent that did the work.
# It is the last word, and nobody else ever looks. For one agent that is simply
# how it is. For three, it is a waste of the only thing that makes running three
# worth doing — that they are not the same model.
#
# Every harness can review code already. None of them can tell whose code it is
# looking at, because a harness cannot see another harness's session. hird can.
#
#   ./examples/review.sh
#
# Runs against a throwaway database *and* a throwaway git repository, so it
# cannot touch your real queue or your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "file the work, and say it wants a second pair of eyes"

# `--review` is the whole opt-in. It says nothing about who, and nothing about
# when: it says this work should not be the last word of the agent that does it.
port=$(hird add "Port the config loader" \
    --body "Keep the env-var precedence." \
    --review --path 'src/config.rs')

hird show "$port"

# ------------------------------------------------------------- codex does it

say "codex claims it and works it"

session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port, \"paths\": [\"src/config.rs\"]}"

edit src/config.rs '// ported: env first, then the file'

# Nothing here mentions a review. The agent finishes the way it always does.
say "codex finishes — and finds it has filed its own review"

session_call codex 2 task_complete \
    "{\"seq\": $port, \"result\": \"ported; env still wins over the file\"}"

say "the board now has a task nobody asked for"

hird ls

# ------------------------------------------------ and codex cannot take it back

say "codex asks for more work"

# Not "nothing to do". The queue is not idle — it is waiting for a different
# tool, and an agent told "nothing is open" would send the human away from the
# one thing that needs them.
session_call codex 3 task_next '{}'

say "and cannot take it by name either"

# Refused in the same transaction as the compare-and-set, so there is no race
# to win and no second window to try from: the bar is the harness.
session_call codex 4 task_claim "{\"seq\": $((port + 1))}"
session_close codex

# ------------------------------------------------------ claude-code reviews it

say "claude-code, which has never seen this work, picks it up"

session_open claude claude-code
# Note what it is handed: the files that actually moved, not the ones anybody
# said would; what the author *claims* it did, marked as the thing under review
# rather than as the brief; and the original brief underneath it.
session_call claude 1 task_claim "{\"seq\": $((port + 1))}"

session_call claude 2 task_complete \
    "{\"seq\": $((port + 1)), \"result\": \"read src/config.rs; precedence is as claimed\"}"
session_close claude

say "hird show — the review, and what it was recused from"

hird show "$((port + 1))"

say "next"

cat <<EOF
Nobody arranged any of that.

The review was filed by the completion, titled after the work, scoped to
src/config.rs because that is what the witness saw move — not because anybody
declared it — and carrying the author's own summary as the thing to check
rather than as the brief.

The refusal is the part only hird can do. Codex could not take it from a second
window either: the bar is the harness, because two windows of one tool is one
model reading its own homework. And it is enforced where the claim is decided,
in the same transaction as the compare-and-set, so there is no race to win.

Nothing here is a scheduler. A recusal says who may *not* take a task and never
who must. Run only one harness and the review simply sits there unclaimable —
which the board says plainly, because that is a fact about your setup and not
something to paper over.

By hand:                        HIRD_DB=$HIRD_DB $HIRD_BIN recuse <seq> --from <seq>
In a plan file:                 review = true               (see plan.toml)
Watch it live:                  HIRD_DB=$HIRD_DB $HIRD_BIN tui
Where the file list comes from: ./examples/witness.sh
EOF
