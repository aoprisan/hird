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
# one that (a) the queue would not refuse this task to, and (b) herdr reports
# idle and can actually reach. HIRD_RECUSED carries the harnesses barred from
# the task — a filed review names whoever did the work under judgement — so
# the summons never knocks on the door the claim would turn away. A worker
# that is working or blocked is skipped; a prompt that fails (no such agent,
# agent gone) falls through to the next worker instead of dying with the
# summons undelivered.
#
# wire.sh writes the hook line that runs this, baking in two paths so the
# relay needs nothing from hird's environment:
#
#   HERDR_BIN            the herdr binary to prompt through
#   HIRD_HERDR_ROSTER    the roster file (see dispatch.conf)
#   HIRD_HERDR_LOCK      a directory used to serialize simultaneous summons
#
# Absent a readable roster it falls back to the pairing the hird docs
# use: a worker named claude on the claude-code harness, a worker named
# codex on codex.

set -u

herdr=${HERDR_BIN:-herdr}
roster=${HIRD_HERDR_ROSTER:-}
recused=",${HIRD_RECUSED:-},"
if [ -n "${HIRD_HERDR_LOCK:-}" ]; then
    lock=$HIRD_HERDR_LOCK
elif [ -n "$roster" ]; then
    lock="$roster.lock"
else
    lock="${TMPDIR:-/tmp}/hird-herdr-dispatch.lock"
fi
locked=no

summons="hird task #${HIRD_TASK:-?} (\"${HIRD_TITLE:-}\") is ready; work the hird queue."

# Announcements can arrive together — a plan's whole first wave, or a finish
# that releases work and files its review. Serialize their routing so the
# first prompt has reached `working` before the next relay chooses; otherwise
# every process can observe the same preferred worker as idle and pile all of
# the summons onto it.
release_lock() {
    if [ "$locked" != yes ]; then
        return 0
    fi
    rm -f "$lock/pid"
    rmdir "$lock" 2>/dev/null || :
    locked=no
}

acquire_lock() {
    attempts=0
    while ! mkdir "$lock" 2>/dev/null; do
        # Only one waiter may reap a dead owner. Without the second directory,
        # two waiters could both validate the old PID, then one could delete a
        # fresh owner's PID after the other had already recreated the lock.
        if mkdir "$lock.reap" 2>/dev/null; then
            owner=
            if [ -r "$lock/pid" ]; then
                IFS= read -r owner <"$lock/pid" || owner=
            fi
            case $owner in
                '' | *[!0-9]*) ;;
                *)
                    if ! kill -0 "$owner" 2>/dev/null; then
                        rm -f "$lock/pid"
                        rmdir "$lock" 2>/dev/null || :
                    fi ;;
            esac
            rmdir "$lock.reap" 2>/dev/null || :
        fi
        attempts=$((attempts + 1))
        [ "$attempts" -lt 200 ] || return 1
        sleep 0.05
    done
    locked=yes
    if ! printf '%s\n' "$$" >"$lock/pid"; then
        release_lock
        return 1
    fi
}

trap release_lock 0
trap 'exit 1' HUP INT TERM

# Locking is best-effort: an unwritable fallback directory should not turn a
# usable one-worker relay into silence. The idle check below remains useful on
# its own; the lock closes the ordinary simultaneous-announcement race.
acquire_lock || :

is_idle() {
    state=$("$herdr" agent get "$1" 2>/dev/null) || return 1
    printf '%s\n' "$state" |
        grep -Eq '"agent_status"[[:space:]]*:[[:space:]]*"(idle|done)"'
}

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
        is_idle "$agent" || continue
        # Waiting only until `working` keeps the lock for the state transition,
        # not for the task. A later relay can then see this worker is occupied
        # and continue down the roster to another idle pair of hands.
        if "$herdr" agent prompt "$agent" "$summons" \
            --wait --until working --timeout 5000 >/dev/null 2>&1; then
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
