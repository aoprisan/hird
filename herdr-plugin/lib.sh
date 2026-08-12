# The herdr contract, in one place.
#
# Sourced by dispatch.sh and summon.sh — the two scripts that talk to herdr
# about workers. What "busy" looks like in `herdr agent get`, what a prompt's
# exit status is worth, and how simultaneous relays take turns are all
# assumptions about a program this repository does not build. Keeping them
# here means a herdr that changes its mind is one edit, not two that drift.
#
# Callers set `herdr` (the binary) before sourcing, and `lock` if they want
# to serialize routing. The defaults below are only so this file reads and
# checks on its own; both callers set both.
herdr=${herdr:-herdr}
lock=${lock:-${TMPDIR:-/tmp}/hird-herdr-dispatch.lock}

# Whether a worker cannot take a summons right now.
#
# Note which way this fails. A summons that goes to a busy worker costs one
# redundant prompt; a check that calls every worker busy costs *every*
# summons the plugin would ever send, silently, because the relay's normal
# way of giving up is to exit 0. So only the states herdr names as occupied
# count as busy: an `agent get` that fails, or output this cannot parse,
# means prompt anyway and let the prompt itself be the judge — which is
# exactly how the relay behaved before it learned to read status at all.
#
# stdin is closed for the call: the roster is on the caller's stdin, and a
# herdr subcommand that read from it would swallow the workers below this
# one.
worker_busy() {
    _state=$("$herdr" agent get "$1" 2>/dev/null </dev/null) || return 1
    printf '%s\n' "$_state" |
        grep -Eq '"agent_status"[[:space:]]*:[[:space:]]*"(working|blocked)"'
}

# Deliver the summons to `$1`, saying whether it landed.
#
# Waiting only until `working` keeps any held lock for the state transition
# rather than for the task, so a later relay can see this worker is occupied
# and walk on to another idle pair of hands.
#
# A timeout is not a refusal. herdr exits non-zero both when it could not
# reach the agent and when it delivered the prompt to an agent that was slow
# to start, and only the first is a reason to try somebody else — falling
# through on the second sends a second agent to the same task. So a failed
# wait re-reads the worker: one that is now busy took the summons.
worker_prompt() {
    if "$herdr" agent prompt "$1" "$2" --wait --until working --timeout 5000 \
        >/dev/null 2>&1 </dev/null; then
        return 0
    fi
    worker_busy "$1"
}

# ------------------------------------------------------------------- locking

# Announcements can arrive together — a plan's whole first wave, or a finish
# that releases work and files its review. Serialize their routing so the
# first prompt has reached `working` before the next relay chooses; otherwise
# every process can observe the same preferred worker as idle and pile all of
# the summons onto it.
#
# The lock is a directory, and its owner's PID is a *file name* inside it.
# That is the whole trick: a waiter that finds a dead owner deletes the
# record of that one process and nothing else, so it can never delete the
# record of a fresh owner that moved in while it was looking. `rmdir` then
# succeeds only if nobody has.

locked=no
_lock_waits=0
_lock_nameless=0
_lock_max_waits=200

lock_release() {
    [ "${locked:-no}" = yes ] || return 0
    rm -f "$lock/pid.$$"
    rmdir "$lock" 2>/dev/null || :
    locked=no
}

# POSIX `sleep` takes whole seconds; GNU, BSD and busybox all accept a
# fraction. Probe once rather than assuming, and where the fraction is
# refused fall back to whole seconds and wait far fewer turns — a relay that
# cannot poll finely must not sit on an announcement for minutes.
lock_wait() {
    if [ -z "${_lock_fine:-}" ]; then
        if sleep 0.05 2>/dev/null; then
            _lock_fine=yes
            return 0
        fi
        _lock_fine=no
        _lock_max_waits=10
    fi
    if [ "$_lock_fine" = yes ]; then
        sleep 0.05
    else
        sleep 1
    fi
}

# Take the lock back from an owner that died holding it.
lock_reap() {
    _named=no
    _held=no
    for _f in "$lock"/pid.*; do
        [ -e "$_f" ] || continue
        _named=yes
        _owner=${_f##*/pid.}
        case $_owner in
            # Not a PID this wrote, so not this script's to remove.
            '' | *[!0-9]*)
                _held=yes
                continue
                ;;
        esac
        if kill -0 "$_owner" 2>/dev/null; then
            _held=yes
            continue
        fi
        rm -f "$_f"
    done
    if [ "$_held" = yes ]; then
        _lock_nameless=0
        return 0
    fi
    if [ "$_named" = no ]; then
        # A lock directory naming nobody at all: either its creator is in the
        # instant between `mkdir` and writing its name, or it was killed in
        # that instant and will never write one. Only the second is worth
        # breaking, so make it prove it by staying nameless.
        _lock_nameless=$((_lock_nameless + 1))
        [ "$_lock_nameless" -ge 20 ] || return 0
    else
        _lock_nameless=0
    fi
    rmdir "$lock" 2>/dev/null || :
}

lock_acquire() {
    _lock_waits=0
    _lock_nameless=0
    while ! mkdir "$lock" 2>/dev/null; do
        # A lock that is not a directory is not one we are queueing behind:
        # an unwritable or missing parent never becomes free, and spinning on
        # it only delays an announcement that would have been delivered.
        [ -d "$lock" ] || return 1
        lock_reap
        _lock_waits=$((_lock_waits + 1))
        [ "$_lock_waits" -lt "$_lock_max_waits" ] || return 1
        lock_wait
    done
    locked=yes
    : >"$lock/pid.$$" 2>/dev/null || {
        lock_release
        return 1
    }
}
