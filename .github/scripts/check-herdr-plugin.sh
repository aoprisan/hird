#!/bin/sh

# Behavioral checks for the shell-only Herdr plugin. The Rust suite cannot see
# these scripts, so exercise their contracts with a fake Herdr CLI: routing
# skips occupied workers (including under simultaneous announcements), degrades
# toward prompting rather than silence when it cannot read herdr at all, and
# the doctor recognizes only this installed relay.

set -eu

repo=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
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

# The fake herdr. FAKE_HERDR_MODE bends it into the shapes a real one can take:
# `plain` answers agent get with the JSON the plugin expects, `mute` fails the
# subcommand outright (an older herdr, or one that renamed it), and `slow`
# delivers the prompt but reports the agent still starting, so `--wait` times
# out on a summons that did land.
cat >"$tmp/bin/herdr" <<'EOF'
#!/bin/sh
set -eu

mode=${FAKE_HERDR_MODE:-plain}

case "${1:-}:${2:-}" in
    agent:get)
        [ "$mode" = mute ] && exit 3
        status=$(sed -n '1p' "$FAKE_HERDR_STATE/${3:?}")
        printf '{"result":{"type":"agent_info","agent":{"agent_status":"%s"}}}\n' "$status"
        ;;
    agent:prompt)
        agent=${3:?}
        printf '%s\n' "$agent" >>"$FAKE_HERDR_LOG"
        printf '%s\n' working >"$FAKE_HERDR_STATE/$agent"
        # The prompt landed; the wait for `working` is what fails.
        [ "$mode" = slow ] && exit 4
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
        FAKE_HERDR_MODE="${mode:-plain}" \
        HERDR_BIN="$tmp/bin/herdr" \
        HIRD_HERDR_ROSTER="$tmp/dispatch.conf" \
        HIRD_HERDR_LOCK="$tmp/dispatch.lock" \
        HIRD_EVENT=filed \
        HIRD_TASK=7 \
        HIRD_TITLE="test the relay" \
        HIRD_RECUSED='' \
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

# A herdr whose `agent get` this cannot read at all must not silence the relay.
# The status check is an optimization over the old prompt-whoever-answers
# behavior; losing it costs a redundant prompt, never the summons.
mode=mute
printf '%s\n' idle >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
run_dispatch
[ "$(sed -n '1p' "$tmp/prompts")" = claude ] ||
    fail "an unreadable agent status silenced the relay instead of prompting"

# A prompt that lands on an agent slow to reach `working` times out, but it was
# still delivered — the relay must not send a second agent to the same task.
mode=slow
printf '%s\n' idle >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
run_dispatch
[ "$(wc -l <"$tmp/prompts" | tr -d ' ')" = 1 ] ||
    fail "a delivered-but-slow prompt fell through and double-summoned"
mode=plain

# The roster is on the loop's stdin. A herdr subcommand that reads stdin would
# eat the workers below the one being checked, so the calls must be insulated
# from it: with the preferred worker busy, the second line has to survive.
cat >"$tmp/bin/herdr-greedy" <<'EOF'
#!/bin/sh
set -eu
cat >/dev/null
exec "$FAKE_HERDR_REAL" "$@"
EOF
chmod +x "$tmp/bin/herdr-greedy"
printf '%s\n' working >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
FAKE_HERDR_LOG="$tmp/prompts" \
    FAKE_HERDR_STATE="$tmp/state" \
    FAKE_HERDR_REAL="$tmp/bin/herdr" \
    HERDR_BIN="$tmp/bin/herdr-greedy" \
    HIRD_HERDR_ROSTER="$tmp/dispatch.conf" \
    HIRD_HERDR_LOCK="$tmp/dispatch.lock" \
    HIRD_EVENT=filed \
    HIRD_TASK=7 \
    HIRD_TITLE="test the relay" \
    HIRD_RECUSED='' \
    sh "$repo/herdr-plugin/dispatch.sh"
[ "$(sed -n '1p' "$tmp/prompts")" = codex ] ||
    fail "a stdin-reading herdr swallowed the rest of the roster"

# A lock path that can never be created must cost the announcement nothing
# beyond the lock: the summons still goes out, and promptly.
printf '%s\n' idle >"$tmp/state/claude"
: >"$tmp/prompts"
FAKE_HERDR_LOG="$tmp/prompts" \
    FAKE_HERDR_STATE="$tmp/state" \
    HERDR_BIN="$tmp/bin/herdr" \
    HIRD_HERDR_ROSTER="$tmp/dispatch.conf" \
    HIRD_HERDR_LOCK="$tmp/no/such/parent/dispatch.lock" \
    HIRD_EVENT=filed \
    HIRD_TASK=7 \
    HIRD_TITLE="test the relay" \
    HIRD_RECUSED='' \
    sh "$repo/herdr-plugin/dispatch.sh"
[ "$(sed -n '1p' "$tmp/prompts")" = claude ] ||
    fail "an unusable lock path silenced the relay"

# An owner that died holding the lock is reaped, so one crashed relay does not
# mute every announcement after it.
rm -rf "$tmp/dispatch.lock"
mkdir "$tmp/dispatch.lock"
: >"$tmp/dispatch.lock/pid.2147483647"
printf '%s\n' idle >"$tmp/state/claude"
: >"$tmp/prompts"
run_dispatch
[ "$(sed -n '1p' "$tmp/prompts")" = claude ] ||
    fail "a lock held by a dead owner was not reaped"
[ ! -d "$tmp/dispatch.lock" ] || fail "the lock outlived the relay that took it"

# The manual action obeys the same busy-worker contract as the relay.
printf '%s\n' blocked >"$tmp/state/claude"
printf '%s\n' 'done' >"$tmp/state/codex"
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

# Without a plugin root there is nothing to compare against, and a wired
# install must not be reported as a broken one.
doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "cannot check which checkout"

echo "herdr plugin checks passed"
