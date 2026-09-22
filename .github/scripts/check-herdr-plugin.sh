#!/bin/sh

# Behavioral checks for the shell-only Herdr plugin. The Rust suite cannot see
# these scripts, so exercise their contracts with a fake Herdr CLI: routing
# skips occupied workers (including under simultaneous announcements), degrades
# toward prompting rather than silence when it cannot read herdr at all, and
# the doctor recognizes only this installed relay.
#
# A fake jev covers the other half of routing — the opinion about fit. What is
# being checked there is mostly what an opinion may *not* do: outrank a
# recusal, a capability requirement or a busy worker, or be heard at all when
# it is unsure, unparseable, or answered by a simulator nobody asked for. The
# fake records the page it was handed as well as the state, so what it is not
# asked is checked too: a harness the queue has ruled out is an absence in the
# labels, and a question with one answer left is a call that never happened.

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
worker claude claude-code browser,network
worker codex codex,codex-cli filesystem,shell
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
        HIRD_REQUIRES="${1:-}" \
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

# Capability-bound work skips an otherwise idle preferred worker when its
# fourth roster column does not contain every label in HIRD_REQUIRES.
printf '%s\n' idle >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
run_dispatch filesystem,shell
[ "$(sed -n '1p' "$tmp/prompts")" = codex ] ||
    fail "a worker missing a required capability was not skipped"

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

# No roster file at all. The built-in pairing is one list, read both by the
# walk and by the narrowing that decides what the routing question is worth
# asking over, so a recusal has to reach it from either direction.
printf '%s\n' idle >"$tmp/state/claude"
printf '%s\n' idle >"$tmp/state/codex"
: >"$tmp/prompts"
FAKE_HERDR_LOG="$tmp/prompts" \
    FAKE_HERDR_STATE="$tmp/state" \
    HERDR_BIN="$tmp/bin/herdr" \
    HIRD_HERDR_LOCK="$tmp/dispatch.lock" \
    HIRD_EVENT=filed \
    HIRD_TASK=7 \
    HIRD_TITLE="test the relay" \
    HIRD_RECUSED=claude-code \
    sh "$repo/herdr-plugin/dispatch.sh"
[ "$(sed -n '1p' "$tmp/prompts")" = codex ] ||
    fail "the built-in fallback roster did not route around a recusal"

# The manual action obeys the same busy-worker contract as the relay. It has no
# particular task announcement, so capability filtering does not apply.
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

# ------------------------------------------------------------------- routing

# The fake jev. It answers the one question route.sh reads and records the
# state it was asked about, so a test can check that the relay described the
# task rather than merely called something.
cat >"$tmp/bin/jev" <<'EOF'
#!/bin/sh
set -eu

state=
page=
seen_run=
while [ $# -gt 0 ]; do
    case $1 in
        --state)
            state=${2:-}
            shift 2
            ;;
        --json | --mock)
            shift
            ;;
        --*)
            shift 2
            ;;
        run)
            seen_run=1
            shift
            ;;
        *)
            [ -n "$seen_run" ] && [ -z "$page" ] && page=$1
            shift
            ;;
    esac
done
printf '%s\n' "$state" >"$FAKE_JEV_STATE"

# What page it was actually handed: a path, or one on stdin. The narrowing
# is only visible here, so a test that wants to see it reads this.
if [ -n "${FAKE_JEV_PAGE:-}" ]; then
    printf 'page=%s\n' "$page" >"$FAKE_JEV_PAGE"
    if [ "$page" = - ]; then
        cat >>"$FAKE_JEV_PAGE"
    elif [ -r "$page" ]; then
        cat "$page" >>"$FAKE_JEV_PAGE"
    fi
fi

# The body is `jev run --json`'s, pretty-printed and field for field —
# including the `"type": "choice"` that a reader looking for the first
# `choice` in the document would mistake for the answer.
case ${FAKE_JEV_MODE:-plain} in
    fail) exit 1 ;;
    garbage) echo 'Service Unavailable' ;;
    *)
        cat <<JSON
{
  "model": "jev-latest",
  "answers": {
    "harness": {
      "type": "choice",
      "choice": "${FAKE_JEV_CHOICE:-codex}",
      "probabilities": {
        "claude-code": 0.2,
        "codex": 0.8
      },
      "confidence": ${FAKE_JEV_CONFIDENCE:-0.9}
    }
  },
  "usage": {
    "input_tokens": null,
    "output_tokens": null
  }
}
JSON
        ;;
esac
EOF
chmod +x "$tmp/bin/jev"

cp "$repo/herdr-plugin/route.jev" "$tmp/route.jev"

jev_mode=plain
jev_choice=codex
jev_confidence=0.9
jev_page="$tmp/route.jev"
jev_mock=1
jev_key=
jev_roster="$tmp/dispatch.conf"
recused=
requires=

routing_defaults() {
    jev_mode=plain
    jev_choice=codex
    jev_confidence=0.9
    jev_page="$tmp/route.jev"
    jev_mock=1
    jev_key=
    jev_roster="$tmp/dispatch.conf"
    recused=
    requires=
    printf '%s\n' idle >"$tmp/state/claude"
    printf '%s\n' idle >"$tmp/state/codex"
    printf '%s\n' idle >"$tmp/state/copilot"
    : >"$tmp/prompts"
    : >"$tmp/jev-state"
    : >"$tmp/jev-page"
}

# HIRD_DB names a directory that cannot exist, so route.sh's optional
# `hird show` upgrade fails the same way on a machine with hird installed as
# on one without, and the state it falls back to is the announcement's.
run_routed() {
    FAKE_HERDR_LOG="$tmp/prompts" \
        FAKE_HERDR_STATE="$tmp/state" \
        FAKE_HERDR_MODE=plain \
        FAKE_JEV_STATE="$tmp/jev-state" \
        FAKE_JEV_PAGE="$tmp/jev-page" \
        FAKE_JEV_MODE="$jev_mode" \
        FAKE_JEV_CHOICE="$jev_choice" \
        FAKE_JEV_CONFIDENCE="$jev_confidence" \
        HERDR_BIN="$tmp/bin/herdr" \
        HIRD_HERDR_ROSTER="$jev_roster" \
        HIRD_HERDR_LOCK="$tmp/dispatch.lock" \
        HIRD_JEV_BIN="$tmp/bin/jev" \
        HIRD_JEV_PAGE="$jev_page" \
        HIRD_JEV_MOCK="$jev_mock" \
        TYPESAFE_API_KEY="$jev_key" \
        HIRD_DB="$tmp/no/such/dir/hird.db" \
        HIRD_EVENT=filed \
        HIRD_TASK=7 \
        HIRD_TITLE="rename the config loader" \
        HIRD_RECUSED="$recused" \
        HIRD_REQUIRES="$requires" \
        sh "$repo/herdr-plugin/dispatch.sh"
}

prompted() {
    sed -n '1p' "$tmp/prompts"
}

# The opinion reorders the roster: codex is second and claude is idle, and
# codex is summoned anyway because the page said this task is its kind.
routing_defaults
run_routed
[ "$(prompted)" = codex ] || fail "the routing answer did not reorder the roster"

# And it was asked about this task, not merely called.
assert_contains "$(cat "$tmp/jev-state")" "rename the config loader"
assert_contains "$(cat "$tmp/jev-state")" "task #7"

# A third harness, so the queue's bars can rule one out and still leave the
# page something to choose between.
cat >"$tmp/dispatch-three.conf" <<'EOF'
worker claude claude-code browser,network
worker codex codex,codex-cli filesystem,shell
worker copilot copilot filesystem
EOF

# A preference is not a permission. Each of the three bars the relay already
# had outranks the answer — here an answer naming a harness the narrowing did
# not even offer — and in every case the walk falls through to the worker the
# queue would actually allow.
routing_defaults
jev_roster="$tmp/dispatch-three.conf"
recused=claude-code
jev_choice=claude-code
run_routed
[ "$(prompted)" = codex ] || fail "a routing answer outranked a recusal"

routing_defaults
jev_roster="$tmp/dispatch-three.conf"
requires=filesystem
jev_choice=claude-code
run_routed
[ "$(prompted)" = codex ] || fail "a routing answer outranked a capability requirement"

routing_defaults
printf '%s\n' working >"$tmp/state/codex"
run_routed
[ "$(prompted)" = claude ] || fail "a routing answer outranked a busy worker"

# A name no roster line carries is not an error, just an opinion about nobody:
# the preferred pass matches no worker and the ordinary walk follows it.
routing_defaults
jev_choice=aider
run_routed
[ "$(prompted)" = claude ] || fail "an unroutable answer did not fall through to the roster"

# An unsure answer is worth less than the order a person wrote down.
routing_defaults
jev_confidence=0.4
run_routed
[ "$(prompted)" = claude ] || fail "an answer below the confidence bar was acted on"

# Every way the call can go wrong ends in the roster order rather than in
# silence: routing is an upgrade over the walk, never a gate in front of it.
routing_defaults
jev_mode=fail
run_routed
[ "$(prompted)" = claude ] || fail "a failed routing call did not fall back to the roster"

routing_defaults
jev_mode=garbage
run_routed
[ "$(prompted)" = claude ] || fail "an unparseable routing answer was not ignored"

routing_defaults
jev_page="$tmp/no-such-page.jev"
run_routed
[ "$(prompted)" = claude ] || fail "a missing page did not leave the roster order alone"
[ ! -s "$tmp/jev-state" ] || fail "a missing page still called jev"

# Without a key jev simulates its answers. A simulated answer would land in
# the roster order looking exactly like a judgement, so the call is not made
# at all — and the proof is that the fake was never run.
routing_defaults
jev_mock=
run_routed
[ "$(prompted)" = claude ] || fail "a keyless routing call did not fall back to the roster"
[ ! -s "$tmp/jev-state" ] || fail "jev was asked to route with no key and no mock"

# With a key it is asked again, live.
routing_defaults
jev_mock=
jev_key=not-a-real-key
run_routed
[ "$(prompted)" = codex ] || fail "a keyed routing call was not made"

# ------------------------------------------------- the narrowed question

# Nothing the queue ruled out, nothing to cut: the page goes over as the path
# it always was, so an unconstrained announcement is the call it always was.
routing_defaults
jev_roster="$tmp/dispatch-three.conf"
run_routed
assert_contains "$(cat "$tmp/jev-page")" "page=$tmp/route.jev"

# A recused harness is not an option the page gets to offer. Left in, it takes
# confidence that belongs to the agents that may actually take the task, to
# buy a preferred pass that could only ever match nobody.
routing_defaults
jev_roster="$tmp/dispatch-three.conf"
recused=claude-code
run_routed
asked=$(cat "$tmp/jev-page")
assert_contains "$asked" "page=-"
assert_contains "$asked" "  codex ="
assert_contains "$asked" "  copilot ="
case $asked in
    *"  claude-code ="*) fail "a recused harness was offered as an answer" ;;
esac

# So is a harness this task is not equipped for.
routing_defaults
jev_roster="$tmp/dispatch-three.conf"
requires=filesystem
run_routed
asked=$(cat "$tmp/jev-page")
assert_contains "$asked" "page=-"
assert_contains "$asked" "  codex ="
case $asked in
    *"  claude-code ="*) fail "an unequipped harness was offered as an answer" ;;
esac
[ "$(prompted)" = codex ] || fail "the narrowed answer did not route"

# Under two labels there is nothing to decide. The walk reaches the only
# permitted harness on its own, so the call is not made at all.
routing_defaults
jev_roster="$tmp/dispatch-three.conf"
requires=browser
run_routed
[ ! -s "$tmp/jev-state" ] || fail "jev was asked a question with one possible answer"
[ "$(prompted)" = claude ] || fail "the only permitted worker was not summoned"

# And when the task is for nobody on the roster, there is no question and no
# summons — the board keeps it.
routing_defaults
jev_roster="$tmp/dispatch-three.conf"
requires=gpu.cuda
run_routed
[ ! -s "$tmp/jev-state" ] || fail "jev was asked about a task no worker may take"
[ -z "$(prompted)" ] || fail "a task nobody may take was summoned anyway"

# A label no roster harness carries routes nothing whoever says it, so it is
# cut rather than asked about — and the rest of the page is copied through.
routing_defaults
cat >"$tmp/route-drift.jev" <<'EOF'
A hird task has become claimable and one of several agents should take it.
---
# a page whose labels have drifted from the roster
harness: Which harness is the better tool for this particular task
  claude-code = Ambiguous briefs and sprawling changes
  codex = A clear spec carried out exactly
  aider = A harness nobody on this roster runs
EOF
jev_page="$tmp/route-drift.jev"
run_routed
asked=$(cat "$tmp/jev-page")
assert_contains "$asked" "page=-"
assert_contains "$asked" "harness: Which harness is the better tool"
assert_contains "$asked" "# a page whose labels have drifted from the roster"
assert_contains "$asked" "  claude-code = Ambiguous briefs and sprawling changes"
case $asked in
    *aider*) fail "a label no roster harness carries was still offered" ;;
esac

routing_defaults

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

# The routing line reports the page, and reports the ordinary case — no page —
# as off rather than as something broken.
doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_ROOT="$repo/herdr-plugin" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    HIRD_JEV_BIN="$tmp/bin/jev" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "routing: $tmp/route.jev"

doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_ROOT="$repo/herdr-plugin" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp/xdg" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "routing: off"

# The page's labels and the roster's third column are two lists of the same
# names in two files, and each way they drift is silent at the moment it goes
# wrong: the shipped page describes a copilot this roster does not run, and
# the roster runs a codex-cli the page does not describe.
doctor=$(PATH="$tmp/bin:$PATH" XDG_CONFIG_HOME="$tmp/xdg" \
    HERDR_PLUGIN_ROOT="$repo/herdr-plugin" \
    HERDR_PLUGIN_CONFIG_DIR="$tmp" \
    HIRD_JEV_BIN="$tmp/bin/jev" \
    sh "$repo/herdr-plugin/doctor.sh")
assert_contains "$doctor" "routing labels: copilot"
assert_contains "$doctor" "roster harnesses: codex-cli"

echo "herdr plugin checks passed"
