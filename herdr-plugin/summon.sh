#!/bin/sh
#
# The manual summons: wake a worker without waiting for an announcement.
#
# The dispatch hook only speaks when a task *becomes* claimable. A queue
# that filled while the hook was unwired, or whose summons landed on an
# agent that has since died, sits ready in silence. This action walks the
# same roster the relay uses and prompts the first worker herdr can
# reach — no recusal to honor, because no single task is being routed;
# whoever answers calls `task_next` and the queue hands them what it
# will.
#
# Runs as a plugin action: stdout lands in the plugin command log, so
# `herdr plugin log list --plugin hird` says whom it woke.

set -u

herdr=${HERDR_BIN_PATH:-herdr}
roster=${HERDR_PLUGIN_CONFIG_DIR:-}/dispatch.conf

summons="the hird queue has claimable work; work the hird queue."

is_idle() {
    state=$("$herdr" agent get "$1" 2>/dev/null) || return 1
    printf '%s\n' "$state" |
        grep -Eq '"agent_status"[[:space:]]*:[[:space:]]*"(idle|done)"'
}

try_roster() {
    while read -r kind agent _; do
        [ "$kind" = "worker" ] || continue
        [ -n "$agent" ] || continue
        is_idle "$agent" || continue
        if "$herdr" agent prompt "$agent" "$summons" \
            --wait --until working --timeout 5000 2>/dev/null; then
            echo "summoned $agent to the hird queue"
            exit 0
        fi
    done
}

if [ -r "$roster" ]; then
    try_roster <"$roster"
else
    try_roster <<'EOF'
worker claude claude-code
worker codex codex,codex-cli
EOF
fi

echo "no worker on the roster answered; is an agent running under herdr?" >&2
exit 1
