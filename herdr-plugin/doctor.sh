#!/bin/sh
#
# The startup look: one report on the pairing's posture, every time a
# herdr server starts. Changes nothing, blocks nothing; read it with
#
#   herdr plugin log list --plugin hird
#
# Four questions, one line each: is hird installed, is its dispatch hook
# wired to this plugin's relay, does the roster exist, and is anything
# routing by fit on top of the roster's order.
#
# The last one asks a fifth thing when there is a page to ask it of. The
# page's labels and the roster's third column are two lists of the same names
# kept in two files, and neither of the ways they drift apart says anything at
# the time it goes wrong: a label no roster harness carries is an answer that
# routes nothing, and a roster harness with no label is an agent the page can
# never prefer. The relay cuts the first out of the question it asks, which is
# correct and silent, so the saying-so belongs here.

set -u

if command -v hird >/dev/null 2>&1; then
    echo "hird: $(hird --version 2>/dev/null || echo present)"
else
    echo "hird: not on PATH — install it: https://github.com/aoprisan/hird"
fi

hird_conf="${XDG_CONFIG_HOME:-$HOME/.config}/hird/config.toml"
root=${HERDR_PLUGIN_ROOT:-}
relay="$root/dispatch.sh"
marker="# wired by the hird herdr plugin"

# Reproduce wire.sh's two quoting layers so the health check identifies this
# installed relay, not merely any user hook whose filename contains
# `dispatch.sh`. It also catches a hook left pointing at an earlier managed
# checkout after a reinstall moved the plugin root.
shq() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

toml_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

managed=
if [ -f "$hird_conf" ]; then
    managed=$(grep -F "$marker" "$hird_conf" | head -n 1)
fi
expected=$(toml_escape "$(shq "$relay")")

if [ -n "$managed" ] && [ -z "$root" ]; then
    # Run outside herdr, so there is no plugin root to compare the recorded
    # relay against. Saying "stale" here would be a false alarm about a
    # perfectly good install; say what is actually known instead.
    echo "dispatch_hook: wired to this plugin, but HERDR_PLUGIN_ROOT is unset — cannot check which checkout"
elif [ -n "$managed" ] && [ -f "$relay" ]; then
    case $managed in
        *"$expected"*)
            echo "dispatch_hook: wired to this plugin's relay" ;;
        *)
            echo "dispatch_hook: stale plugin wiring — reopen the wire pane" ;;
    esac
elif [ -n "$managed" ]; then
    echo "dispatch_hook: stale plugin wiring — reopen the wire pane"
else
    echo "dispatch_hook: not wired — open the wire pane: herdr plugin pane open --plugin hird --entrypoint wire"
fi

roster="${HERDR_PLUGIN_CONFIG_DIR:-}/dispatch.conf"
if [ -r "$roster" ]; then
    echo "roster: $roster ($(grep -c '^worker ' "$roster" 2>/dev/null || echo 0) workers)"
else
    echo "roster: none yet — the relay will use its built-in claude/codex fallback"
fi

# The routing page is the off switch, so its absence is the ordinary answer
# rather than a problem. What this cannot honestly report is the key: the
# relay inherits its environment from whatever made the announcement, not
# from the herdr server this runs under, so a key seen here proves nothing
# about a key seen there. Say what turns it on and leave the claim unmade.
page=${HIRD_JEV_PAGE:-${HERDR_PLUGIN_CONFIG_DIR:-}/route.jev}
if [ ! -r "$page" ]; then
    echo "routing: off — the relay walks the roster in your order (route.jev turns it on)"
elif ! command -v "${HIRD_JEV_BIN:-jev}" >/dev/null 2>&1; then
    echo "routing: $page, but jev is not on PATH — install it (cargo install jev-repl) or the roster order stands"
else
    echo "routing: $page — jev names the harness that gets first refusal (a live answer needs TYPESAFE_API_KEY where your agents run)"
fi

# The two lists, against each other. `page_labels` is route.sh's, so the names
# read here are exactly the ones the relay narrows the question with.
if [ -r "$page" ] && [ -r "$roster" ]; then
    # shellcheck source=route.sh
    . "$(dirname "$0")/route.sh"

    labels=$(page_labels "$page")
    harnesses=$(awk '$1 == "worker" && NF >= 3 { print $3 }' "$roster" |
        tr ',' '\n' | awk 'NF && !seen[$0]++')

    missing() {
        printf '%s\n' "$1" | while read -r name; do
            [ -n "$name" ] || continue
            printf '%s\n' "$2" | grep -qxF -- "$name" || printf '%s, ' "$name"
        done | sed 's/, $//'
    }

    unknown=$(missing "$labels" "$harnesses")
    [ -n "$unknown" ] &&
        echo "routing labels: $unknown — no roster line carries these, so an answer naming one routes nothing"

    unlabelled=$(missing "$harnesses" "$labels")
    [ -n "$unlabelled" ] &&
        echo "roster harnesses: $unlabelled — the page describes no such label, so they can never be preferred"
fi

exit 0
