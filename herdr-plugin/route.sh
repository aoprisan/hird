# Which harness a task is for — the optional routing opinion.
#
# Sourced by dispatch.sh, the relay hird's dispatch_hook runs. It exposes one
# function, `preferred_harness`, which prints one harness name for the relay
# to try before the rest of the roster, or prints nothing at all.
#
# hird deliberately routes nobody. It knows the *hard* facts about a task —
# which harnesses a claim will refuse (HIRD_RECUSED) and which capabilities
# its worker must advertise (HIRD_REQUIRES) — and it enforces both again,
# atomically, inside the claim. What it has no opinion about is fit: which of
# several equally permitted agents is the better tool for this particular job.
# That judgement is a classification, so it is asked of a classifier. `jev`
# (https://crates.io/crates/jev-repl) sends the task to TypeSafe AI's System
# One as a `choice` over your harness names and reads back one label with a
# confidence.
#
# Two rules keep an opinion from becoming an authority:
#
#   - It is a preference, never a permission. The name this prints only
#     reorders the roster; every worker still has to clear recusal,
#     capabilities and busyness, and a preferred harness that clears none of
#     them simply never matches. The relay walks the whole roster straight
#     afterwards, so an opinion can move a worker up and never rule one out.
#   - Every failure is the same failure. No page, no jev, no key, a call that
#     times out, an answer under the confidence bar, an answer this cannot
#     parse: all of them print nothing, and the relay behaves exactly as it
#     did before this file existed.
#
# Configured through the environment, which wire.sh bakes into the hook line
# so the relay needs nothing from hird's own environment:
#
#   HIRD_JEV_PAGE        the .jev routing page. Missing or unreadable is the
#                        off switch, and it is off by default.
#   HIRD_JEV_BIN         the jev binary (default: jev, from PATH)
#   HIRD_JEV_CONFIDENCE  the bar an answer must clear (default: 0.6)
#   HIRD_JEV_TIMEOUT     seconds one live call may take (default: 10)
#   HIRD_JEV_MOCK        take jev's simulated answers, for demos and tests
#
# The page must ask a question named `harness` whose labels are the harness
# names hird itself uses — what `hird agents` prints and what the roster's
# harness column carries — because that column is what the name is matched
# against. See route.jev.
#
# One thing worth knowing before relying on this: hird spawns the hook from
# whichever process made the announcement — an agent's `hird mcp` session, a
# `hird add` in your shell, a `hird events --follow` that swept an expired
# lease — so the hook inherits *that* environment. TYPESAFE_API_KEY has to be
# somewhere all of them see it (a shell profile), or the calls that lack it
# quietly fall back to the roster order.

jev_bin=${HIRD_JEV_BIN:-jev}
jev_page=${HIRD_JEV_PAGE:-}
jev_floor=${HIRD_JEV_CONFIDENCE:-0.6}
jev_timeout=${HIRD_JEV_TIMEOUT:-10}

# The task as the queue knows it — what the page is asked about.
#
# `hird show` carries the body and the declared file globs, which is by far
# the best evidence of what a task actually is. But it wants a hird on PATH
# and a readable database, and the relay runs detached where neither is
# promised, so it is an upgrade and not a requirement: the announcement's own
# fields are always enough to ask with.
#
# The cut is not cosmetic. A page is priced by its state and a task body has
# no ceiling, so this keeps a routing call the cost of a routing call.
routing_state() {
    _state="task #${HIRD_TASK:-?}: ${HIRD_TITLE:-}
event: ${HIRD_EVENT:-}
requires: ${HIRD_REQUIRES:-none}"
    if [ -n "${HIRD_TASK:-}" ]; then
        _detail=$(hird show "$HIRD_TASK" 2>/dev/null </dev/null) || _detail=
        [ -n "$_detail" ] && _state=$_detail
    fi
    printf '%s\n' "$_state" | head -n 40 | cut -c1-200
}

# The value JSON gives `$2` inside `$1`, read without jq.
#
# The rest of this plugin reads herdr's JSON with grep for the same reason: a
# summons that depends on a JSON parser being installed is a summons that goes
# missing on the machine that did not install one.
#
# Two details earn the awk. The name must be matched as a *key* — followed by
# a colon — because a choice answer also carries `"type": "choice"`, and a
# reader that took the first `choice` it saw would read the field name as the
# harness name. And it must be the *first* such key in what it is given,
# because the caller narrows to one answer by cutting the document at it: a
# page that asks a second question would otherwise have that question's value
# read as this one's. Anything unrecognised comes back empty, which every
# caller treats as no opinion.
json_after() {
    printf '%s' "$1" | tr -d '\n' | awk -v key="$2" '
        {
            if (!match($0, "\"" key "\"[ \t]*:")) exit
            v = substr($0, RSTART + RLENGTH)
            sub(/^[ \t]*"?/, "", v)
            sub(/["},].*$/, "", v)
            sub(/[ \t]+$/, "", v)
            print v
        }
    '
}

preferred_harness() {
    [ -n "$jev_page" ] && [ -r "$jev_page" ] || return 0
    command -v "$jev_bin" >/dev/null 2>&1 || return 0

    set -- run "$jev_page" --json --timeout "$jev_timeout" --state "$(routing_state)"
    if [ -n "${HIRD_JEV_MOCK:-}" ]; then
        set -- "$@" --mock
    elif [ -z "${TYPESAFE_API_KEY:-}" ]; then
        # Without a key jev simulates its answers and says so on stderr, which
        # the relay cannot hear. A simulated answer is not a routing decision;
        # it is a coin that would land in the roster order looking exactly
        # like a judgement. Say nothing instead.
        return 0
    fi

    # stdin is closed because the roster is on the caller's: a jev that read
    # it would swallow the workers the relay has not looked at yet.
    _answer=$("$jev_bin" "$@" 2>/dev/null </dev/null) || return 0

    case $_answer in
        *'"harness"'*) ;;
        *) return 0 ;;
    esac
    _slice=${_answer#*\"harness\"}
    _choice=$(json_after "$_slice" choice)
    _conf=$(json_after "$_slice" confidence)

    # A harness name is a token. Refusing everything else means a mangled
    # parse can only ever fail to match a roster line — never carry anything
    # surprising into one.
    case $_choice in
        '' | *[!A-Za-z0-9._-]*) return 0 ;;
    esac
    case $_conf in
        '' | *[!0-9.]*) return 0 ;;
    esac

    # An unsure answer is worth less than the order a person wrote down.
    awk -v got="$_conf" -v floor="$jev_floor" \
        'BEGIN { exit !(got + 0 >= floor + 0) }' || return 0

    printf '%s\n' "$_choice"
}
