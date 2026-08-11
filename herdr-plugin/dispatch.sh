#!/bin/sh
#
# The relay: what hird's dispatch_hook runs.
#
# hird runs this detached, through `sh -c`, the moment a task becomes
# claimable, with the announcement in the environment: HIRD_EVENT,
# HIRD_TASK, HIRD_TITLE, HIRD_PROJECT, HIRD_RECUSED, HIRD_DB. Nothing
# is read back and stdout/stderr are closed, so this script's whole
# answer is whom it wakes.
#
# It walks a roster of workers in preference order and prompts the first
# one that (a) the queue would not refuse this task to, and (b) herdr can
# actually reach. HIRD_RECUSED carries the harnesses barred from the task
# — a filed review names whoever did the work under judgement — so the
# summons never knocks on the door the claim would turn away. A prompt
# that fails (no such agent, agent gone) falls through to the next
# worker instead of dying with the summons undelivered.
#
# wire.sh writes the hook line that runs this, baking in two paths so the
# relay needs nothing from hird's environment:
#
#   HERDR_BIN            the herdr binary to prompt through
#   HIRD_HERDR_ROSTER    the roster file (see dispatch.conf)
#
# Absent a readable roster it falls back to the pairing the hird docs
# use: a worker named claude on the claude-code harness, a worker named
# codex on codex.

set -u

herdr=${HERDR_BIN:-herdr}
roster=${HIRD_HERDR_ROSTER:-}
recused=",${HIRD_RECUSED:-},"

summons="hird task #${HIRD_TASK:-?} (\"${HIRD_TITLE:-}\") is ready; work the hird queue."

# Read `worker <agent> <harness[,harness...]>` lines; anything else is
# comment. The harness column is what hird will refuse — commas cannot
# appear in a harness name, so the membership test is safe.
try_roster() {
    while read -r kind agent harnesses _; do
        [ "$kind" = "worker" ] || continue
        [ -n "$agent" ] || continue
        barred=no
        old_ifs=$IFS
        IFS=,
        for h in ${harnesses:-}; do
            case $recused in
                *",$h,"*) barred=yes ;;
            esac
        done
        IFS=$old_ifs
        [ "$barred" = yes ] && continue
        if "$herdr" agent prompt "$agent" "$summons" >/dev/null 2>&1; then
            exit 0
        fi
    done
}

if [ -n "$roster" ] && [ -r "$roster" ]; then
    try_roster <"$roster"
else
    try_roster <<'EOF'
worker claude claude-code
worker codex codex,codex-cli
EOF
fi

# Every worker was barred or unreachable. The task is still on the board;
# the summon action, or the next announcement, can try again.
exit 0
