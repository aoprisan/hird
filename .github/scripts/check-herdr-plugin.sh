#!/bin/sh

# Behavioral checks for the shell-only Herdr plugin. The Rust suite cannot see
# these scripts, so exercise their two contracts with a fake Herdr CLI: routing
# chooses idle workers (including under simultaneous announcements), and the
# doctor recognizes only this installed relay.

set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/hird-herdr-plugin.XXXXXX")
trap 'rm -rf "$tmp"' 0
trap 'exit 1' HUP INT TERM

fail() {
    echo "herdr plugin check: $*" >&2
    exit 1
}

assert_contains() {
    case $1 in
        *"$2"*) ;;
        *) fail "expected output to contain $2; got: $1" ;;
    esac
}

mkdir -p "$tmp/bin" "$tmp/state" "$tmp/xdg/hird"

cat >"$tmp/bin/herdr" <<'EOF'
#!/bin/sh
set -eu

case "${1:-}:${2:-}" in
    agent:get)
        status=$(sed -n '1p' "$FAKE_HERDR_STATE/${3:?}")
        printf '{"result":{"type":"agent_info","agent":{"agent_status":"%s"}}}\n' "$status"
        ;;
    agent:prompt)
        agent=${3:?}
        printf '%s\n' "$agent" >>"$FAKE_HERDR_LOG"
        printf '%s\n' working >"$FAKE_HERDR_STATE/$agent"
        ;;
    *) exit 2 ;;
esac
EOF
chmod +x "$tmp/bin/herdr"

cat >"$tmp/bin/hird" <<'EOF'
#!/bin/sh
echo "hird 0.1.0"
EOF
chmod +x "$tmp/bin/hird"

cat >"$tmp/dispatch.conf" <<'EOF'
worker claude claude-code
worker codex codex,codex-cli
EOF

run_dispatch() {
    FAKE_HERDR_LOG="$tmp/prompts" \
        FAKE_HERDR_STATE="$tmp/state" \
        HERDR_BIN="$tmp/bin/herdr" \
        HIRD_HERDR_ROSTER="$tmp/dispatch.conf" \
        HIRD_HERDR_LOCK="$tmp/dispatch.lock" \
        HIRD_EVENT=filed \
        HIRD_TASK=7 \
        HIRD_TITLE="test the relay" \
        HIRD_RECUSED= \
        sh "$repo/herdr-plugin/dispatch.sh"
}

# A reachable but working preferred agent is skipped for the idle worker below
# it, rather than accepting every summons merely because prompt can reach it.
printf '%s\n' working >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
run_dispatch
[ "$(sed -n '1p' "$tmp/prompts")" = codex ] ||
    fail "a busy preferred worker was not skipped"

# Two detached hooks can start together. The routing lock lets the first prompt
# reach working before the second chooses, so both idle workers are used.
printf '%s\n' idle >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
run_dispatch &
first=$!
run_dispatch &
second=$!
wait "$first"
wait "$second"
[ "$(wc -l <"$tmp/prompts" | tr -d ' ')" = 2 ] ||
    fail "simultaneous announcements did not produce two prompts"
grep -qx claude "$tmp/prompts" || fail "the first idle worker was not prompted"
grep -qx codex "$tmp/prompts" || fail "the second idle worker was not prompted"

# The manual action obeys the same idle-worker contract as the relay.
printf '%s\n' blocked >"$tmp/state/claude"
printf '%s\n' done >"$tmp/state/codex"
: >"$tmp/prompts"
FAKE_HERDR_LOG="$tmp/prompts" \
    FAKE_HERDR_STATE="$tmp/state" \
    HERDR_BIN_PATH="$tmp/bin/herdr" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    sh "$repo/herdr-plugin/summon.sh" >/dev/null
[ "$(sed -n '1p' "$tmp/prompts")" = codex ] ||
    fail "the manual summons did not skip a blocked worker"

# A similarly named user hook is not plugin wiring.
printf '%s\n' 'dispatch_hook = "sh ~/scripts/my-dispatch.sh"' >"$tmp/xdg/hird/config.toml"
doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_ROOT="$repo/herdr-plugin" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "dispatch_hook: not wired"

# The marker proves ownership, while the recorded relay must still point at
# this checkout rather than one removed by a reinstall.
printf '%s\n' \
    'dispatch_hook = "exec sh '\''/old/plugin/dispatch.sh'\''" # wired by the hird herdr plugin' \
    >"$tmp/xdg/hird/config.toml"
doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_ROOT="$repo/herdr-plugin" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "dispatch_hook: stale plugin wiring"

printf 'dispatch_hook = "exec sh '\''%s/dispatch.sh'\''" # wired by the hird herdr plugin\n' \
    "$repo/herdr-plugin" >"$tmp/xdg/hird/config.toml"
doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_ROOT="$repo/herdr-plugin" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "dispatch_hook: wired to this plugin's relay"

echo "herdr plugin checks passed"
