#!/usr/bin/env bash
#
# The recess: the human stands the queue down.
#
# Everything in hird is live all the time — task_next answers whoever asks,
# and a dispatch hook summons a worker the moment a task becomes claimable.
# The moment you need the tree to yourself (a rebase, a merge landing), a
# recess is how you say so: no new claims, the hook quiet, work already
# claimed untouched. `hird resume` lifts it and announces everything that
# waited.
#
#   ./examples/recess.sh
#
# Runs against a throwaway database and a throwaway config, so it can touch
# neither your real queue nor your real hook.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

# ---------------------------------------------------------- a hooked, live queue

say "a dispatch hook makes the queue live"

XDG_CONFIG_HOME="$(mktemp -d)"
export XDG_CONFIG_HOME
mkdir -p "$XDG_CONFIG_HOME/hird"

log="$(mktemp -d)/herald.log"
cat >"$XDG_CONFIG_HOME/hird/config.toml" <<EOF
dispatch_hook = "echo \"\$HIRD_EVENT #\$HIRD_TASK \$HIRD_TITLE\" >> $log"
EOF

herald_said() {
    for _ in $(seq 1 100); do
        grep -q "$1" "$log" 2>/dev/null && return
        sleep 0.05
    done
    echo "the hook never reported: $1" >&2
    exit 1
}

before=$(hird add "Port the config loader")
herald_said "filed #$before"
cat "$log"

# -------------------------------------------------------------------- the recess

say "you need the tree to yourself"

hird recess "rebasing main"

# A claim in flight would continue; a new one is refused, in your words. And
# task_next says which silence this is — in recess, not idle — so an agent
# stands by instead of retrying.
mcp codex <<JSON
$(call 1 task_claim "{\"seq\": $before}")
$(call 2 task_next '{}')
JSON

say "the board wears it, and the hook stays quiet"

hird ls

# Work filed during the recess would be claimable, and the hook does not hear
# about it: a summons to a stood-down queue would send an agent straight into
# the refusal above.
during=$(hird add "Use the new loader")
sleep 0.3
if grep -q "#$during" "$log"; then
    echo "the hook spoke during the recess" >&2
    exit 1
fi
echo "the hook heard nothing about task $during"

# -------------------------------------------------------------------- the resume

say "hird resume lifts it and summons hands to the backlog"

hird resume

herald_said "resumed #$before"
herald_said "resumed #$during"
cat "$log"

say "next"

cat <<EOF
The refusal and the task_next answer above are the whole contract: during a
recess nothing is handed out, agents are told to stand by rather than told
the queue is empty, and work already claimed runs to its own end — a recess
stops the hand-out, not the work.

Both tasks arrived at the hook as 'resumed' the moment the recess lifted:
the backlog summons hands when you are ready for them, and not before.

Watch the same database live:  HIRD_DB=$HIRD_DB $HIRD_BIN tui
The hook's whole vocabulary:   ./examples/dispatch-hook.sh
EOF
