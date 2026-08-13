#!/bin/sh
#
# The wiring, watched: point hird's dispatch_hook at the relay, and say
# exactly what was written where.
#
# Runs as a popup pane so the whole transcript is in front of you. It
# does two things and narrates both:
#
#   1. Seeds the worker roster in this plugin's config directory, if one
#      is not already there. The roster is yours after that — herdr
#      never touches plugin config again.
#   2. Writes hird's dispatch_hook key to run dispatch.sh, with the
#      herdr binary and the roster path baked into the hook line, so
#      the relay needs nothing from hird's environment.
#
# It replaces a hook it wrote before (a reinstall moves the plugin
# root), treats the shipped default `dispatch_hook = ""` as unset, and
# refuses to clobber anything else: a hook of your own is shown, next to
# the line you would add by hand.

set -u

root=${HERDR_PLUGIN_ROOT:?}
conf_dir=${HERDR_PLUGIN_CONFIG_DIR:?}
herdr_bin=${HERDR_BIN_PATH:-herdr}

hird_conf="${XDG_CONFIG_HOME:-$HOME/.config}/hird/config.toml"
roster="$conf_dir/dispatch.conf"
lock="$conf_dir/dispatch.lock"

# Quote for the shell that will run the hook line (`sh -c`, from hird).
shq() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# Escape for a TOML basic (double-quoted) string.
toml_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# The trailing comment is how a later wiring recognises its own work: the
# hook command itself is not evidence, since somebody else's hook may run a
# script of their own called dispatch.sh.
marker="# wired by the hird herdr plugin"

hook="HERDR_BIN=$(shq "$herdr_bin") HIRD_HERDR_ROSTER=$(shq "$roster") HIRD_HERDR_LOCK=$(shq "$lock") exec sh $(shq "$root/dispatch.sh")"
line="dispatch_hook = \"$(toml_escape "$hook")\" $marker"

finish() {
    echo
    printf 'Press Enter to close. '
    read -r _
    exit "$1"
}

echo "Wiring hird dispatch into herdr"
echo

# ------------------------------------------------------------- the roster

if [ -f "$roster" ]; then
    echo "Roster already in place: $roster"
else
    mkdir -p "$conf_dir" 2>/dev/null
    if cat >"$roster" <<'EOF'
# Whom the hird summons may knock on, in preference order.
#
# One line per worker:
#
#   worker <herdr agent name> <hird harness[,harness...]> [capability[,capability...]]
#
# The agent name is what `herdr agent list` shows — the name the agent
# was started under. The harness column is how hird knows the same
# agent: what `hird agents` and `hird record` print, e.g. claude-code,
# codex, copilot. It is what HIRD_RECUSED is matched against, so a
# review of that agent's work is never routed back to its own door.
# List every name the harness may report, comma-separated, no spaces.
#
# The optional fourth column lists the capabilities this worker registers
# with hird (for example browser,network). A task carrying HIRD_REQUIRES is
# routed only to a worker whose column contains every required capability.
# Omit it for a general worker with no special capabilities.
#
# The relay prompts the first idle worker that is not recused and answers;
# reorder the lines to change whom it tries first when several are idle.

worker claude claude-code
worker codex codex,codex-cli
EOF
    then
        echo "Seeded the worker roster: $roster"
    else
        # Not fatal: the relay falls back to the same two workers this file
        # would have named. But the hook below will point at a roster that
        # is not there, so say so rather than claim a seeding that failed.
        rm -f "$roster"
        echo "Could not write the roster: $roster"
        echo "The relay will use its built-in claude/codex fallback until"
        echo "that file exists."
    fi
fi
[ -f "$roster" ] && sed -n 's/^worker /  worker /p' "$roster"
echo

# ---------------------------------------------------------------- the hook

current=""
if [ -f "$hird_conf" ]; then
    current=$(sed -n 's/^[[:space:]]*dispatch_hook[[:space:]]*=[[:space:]]*//p' "$hird_conf" | head -n 1)
fi

# The value with any trailing comment and trailing space trimmed off. A `#`
# inside a quoted hook defeats it, which only ever turns an empty default
# into something refused — the safe direction.
bare=$(printf '%s' "${current%%#*}" | sed 's/[[:space:]]*$//')

case $current in
    '')
        mode="write" ;;                     # no key yet
    *"$marker"*)
        mode="write" ;;                     # ours, from an earlier wiring
    '"""'* | "'''"*)
        mode="refuse" ;;                    # multi-line: not ours, not touched
    *)
        case $bare in
            '""' | "''")
                mode="write" ;;             # the shipped empty default
            *)
                mode="refuse" ;;            # a hook of your own
        esac ;;
esac

if [ "$mode" = refuse ]; then
    echo "hird already has a dispatch_hook this plugin did not write:"
    echo
    echo "  dispatch_hook = $current"
    echo
    echo "Leaving it alone. To route through this plugin instead, set in"
    echo "$hird_conf:"
    echo
    echo "  $line"
    finish 1
fi

mkdir -p "$(dirname "$hird_conf")"
[ -f "$hird_conf" ] || : >"$hird_conf"

tmp="$hird_conf.hird-herdr-plugin.$$"
if ! LINE=$line awk '
    !done && $0 ~ /^[[:space:]]*dispatch_hook[[:space:]]*=/ {
        print ENVIRON["LINE"]; done = 1; next
    }
    { print }
    END { if (!done) print ENVIRON["LINE"] }
' "$hird_conf" >"$tmp"; then
    rm -f "$tmp"
    echo "Could not rewrite $hird_conf; nothing was changed."
    finish 1
fi
mv "$tmp" "$hird_conf"

echo "Wired. $hird_conf now carries:"
echo
echo "  $line"
echo
echo "From here on, every task that becomes claimable — filed, unblocked,"
echo "review filed, sent back, handed back, lease expired — prompts the"
echo "first roster worker the queue would not refuse it to."
echo
echo "Try it from any shell in a project:"
echo
echo "  hird add \"try the wiring\""
echo
echo "and watch the summons land on an idle agent. The board pane shows"
echo "the rest."

finish 0
