#!/bin/sh
#
# The startup look: one report on the pairing's posture, every time a
# herdr server starts. Changes nothing, blocks nothing; read it with
#
#   herdr plugin log list --plugin hird
#
# Three questions, one line each: is hird installed, is its dispatch
# hook wired to this plugin's relay, does the roster exist.

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

exit 0
