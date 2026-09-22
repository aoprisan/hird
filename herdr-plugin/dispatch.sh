#!/bin/sh
#
# The relay: what hird's dispatch_hook runs.
#
# hird runs this detached, through `sh -c`, the moment a task becomes
# claimable, with the announcement in the environment: HIRD_EVENT,
# HIRD_TASK, HIRD_TITLE, HIRD_PROJECT, HIRD_RECUSED, HIRD_REQUIRES, HIRD_DB. Nothing
# is read back and stdout/stderr are closed, so this script's whole
# answer is whom it wakes.
#
# It walks a roster of workers in preference order and prompts the first
# one that (a) the queue would not refuse this task to, (b) advertises every
# capability the task requires, and (c) herdr does not report as occupied.
# HIRD_RECUSED carries the harnesses barred from the task — a filed review
# names whoever did the work under judgement — while HIRD_REQUIRES carries
# the task's required capabilities. The summons therefore never knocks on a
# door the claim would turn away. A worker that herdr reports working or
# blocked is skipped; a prompt that fails (no such agent, agent gone) falls
# through to the next worker instead of dying with the summons undelivered.
#
# wire.sh writes the hook line that runs this, baking in four paths so the
# relay needs nothing from hird's environment:
#
#   HERDR_BIN            the herdr binary to prompt through
#   HIRD_HERDR_ROSTER    the roster file (see dispatch.conf)
#   HIRD_HERDR_LOCK      a directory used to serialize simultaneous summons
#   HIRD_JEV_PAGE        an optional routing page (see route.sh, route.jev)
#
# Preference order is yours, written down once. It cannot know that this
# task is a sprawling refactor and that one a rename, which is what route.sh
# is for: when a routing page is in place it names one harness to try ahead
# of the others, and the roster is walked twice — that harness first, then
# everybody, so the opinion can move a worker up and never rule one out. No
# page, no jev, no key, or an unsure answer, and there is one pass in your
# order, exactly as before.
#
# The question it is asked is narrowed by the two bars above before it is
# asked: only the harnesses this task may actually go to are offered as
# answers, and when that leaves fewer than two there is nothing to decide and
# no call is made. Not busyness, though — that is a liveness guess, it is
# re-read at the prompt, and the routing call is deliberately made before the
# lock.
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

# How this talks to herdr — reading a worker's state, delivering a summons,
# and taking turns with other relays — lives next door, because summon.sh
# makes the same three assumptions and they must not drift apart.
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
# Whether any harness is a better fit than the roster order says, which is a
# judgement rather than a rule and so lives apart from the bars above.
# shellcheck source=route.sh
. "$(dirname "$0")/route.sh"

summons="hird task #${HIRD_TASK:-?} (\"${HIRD_TITLE:-}\") is ready; work the hird queue."
if [ -n "${HIRD_REQUIRES:-}" ]; then
    summons="$summons This task requires: $HIRD_REQUIRES."
fi

# Whether a roster line's worker may take this task at all: not recused, and
# carrying every capability the task requires. Both are facts about the task
# that hird computed and will check again, atomically, at the claim — unlike
# busyness, which is a liveness guess and is kept out here, at the prompt.
#
# Commas cannot appear in a harness or capability name, so both membership
# tests are substring tests over a comma-wrapped list.
worker_permitted() {
    _permitted=yes
    _old_ifs=$IFS
    IFS=,
    for _h in ${1:-}; do
        case $recused in
            *",$_h,"*) _permitted=no ;;
        esac
    done
    for _required in ${HIRD_REQUIRES:-}; do
        case ",${2:-}," in
            *",$_required,"*) ;;
            *) _permitted=no ;;
        esac
    done
    IFS=$_old_ifs
    [ "$_permitted" = yes ]
}

# The pairing the hird docs use, for a machine with no roster file yet.
default_roster() {
    cat <<'EOF'
worker claude claude-code
worker codex codex,codex-cli
EOF
}

# The harness names this task may actually go to, deduplicated — what the
# routing question is worth asking over. A worker barred on any one of its
# harnesses is barred outright, exactly as the walk below treats it, so the
# names it contributes are none.
eligible_harnesses() {
    if [ -n "$roster" ] && [ -r "$roster" ]; then
        cat "$roster"
    else
        default_roster
    fi | {
        while read -r kind agent harnesses capabilities _; do
            [ "$kind" = "worker" ] || continue
            [ -n "$agent" ] || continue
            worker_permitted "${harnesses:-}" "${capabilities:-}" || continue
            printf '%s\n' "${harnesses:-}" | tr ',' '\n'
        done
    } | awk 'NF && !seen[$0]++'
}

# Asked before the lock is taken, never while holding it: a routing call can
# wait on a network, and a wave of announcements should not queue up behind
# one agent's turn to think.
#
# It is asked only about the harnesses above, because a choice over labels the
# queue has already ruled out is a pass that can match nobody, paid for on
# every announcement — and an answer whose confidence was spent on options
# that were never available. An empty set is not a narrower question but no
# question: this task is for nobody on the roster, and the walk below will say
# so by prompting nobody.
preferred=
eligible=$(eligible_harnesses)
[ -n "$eligible" ] && preferred=$(preferred_harness "$eligible")

trap lock_release 0
trap 'exit 1' HUP INT TERM

# Locking is best-effort: an unusable lock path should not turn a usable
# one-worker relay into silence. The busy check below still holds on its own;
# the lock closes the ordinary simultaneous-announcement race.
lock_acquire || :

# Read `worker <agent> <harness[,harness...]> [capability[,capability...]]`
# lines; anything else is comment. Harnesses route around recusal; the optional
# fourth column routes work only to workers equipped for it — both through
# `worker_permitted`, which is the same predicate the routing question was
# narrowed with, so a preference can never reach further than the walk would.
try_roster() {
    only=${1:-}
    while read -r kind agent harnesses capabilities _; do
        [ "$kind" = "worker" ] || continue
        [ -n "$agent" ] || continue
        # The routing pass, when there is one: consider only the workers on
        # the harness that was preferred. Everything below still applies to
        # them — being preferred is not being permitted.
        if [ -n "$only" ]; then
            wanted=no
            old_ifs=$IFS
            IFS=,
            for h in ${harnesses:-}; do
                [ "$h" = "$only" ] && wanted=yes
            done
            IFS=$old_ifs
            [ "$wanted" = yes ] || continue
        fi
        worker_permitted "${harnesses:-}" "${capabilities:-}" || continue
        worker_busy "$agent" && continue
        if worker_prompt "$agent" "$summons"; then
            exit 0
        fi
    done
}

# Redirection rather than a pipe, in both passes and on purpose: `try_roster`
# reports a delivered summons by exiting the script, and a pipeline would give
# it a subshell to exit instead — leaving the walk below to send a second
# agent to a task that already has one.
walk_roster() {
    if [ -n "$roster" ] && [ -r "$roster" ]; then
        try_roster "${1:-}" <"$roster"
    else
        try_roster "${1:-}" <<EOF
$(default_roster)
EOF
    fi
}

[ -n "$preferred" ] && walk_roster "$preferred"
walk_roster

# Every worker was barred or unreachable. The task is still on the board;
# the summon action, or the next announcement, can try again.
exit 0
