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
if [ -f "$hird_conf" ] && grep -q '^[[:space:]]*dispatch_hook.*dispatch\.sh' "$hird_conf"; then
    echo "dispatch_hook: wired to this plugin's relay"
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
