# Shared helpers for the example scripts. Source it, don't run it.
#
#   source "$(dirname "$0")/lib/mcp.sh"
#   sandbox_db                       # throwaway HIRD_DB
#   hird add "something"             # the binary, installed or freshly built
#   mcp codex <<'JSON'               # one MCP session, one JSON-RPC line per call
#   {"jsonrpc":"2.0","id":1,"method":"tools/call", …}
#   JSON
#
# The MCP 2026-07-28 equivalents — no handshake, `_meta` on every request — are
# `discover`, `call_stateless` and `mcp_stateless`, at the bottom of this file.

set -euo pipefail

# ------------------------------------------------------------------ the binary

# Prefer an installed `hird`; fall back to building the checkout we live in, so
# the examples work straight out of a clone.
HIRD_BIN="${HIRD_BIN:-}"
if [[ -z "$HIRD_BIN" ]]; then
    if command -v hird >/dev/null 2>&1; then
        HIRD_BIN="$(command -v hird)"
    else
        repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
        echo "hird is not on PATH — building $repo" >&2
        (cd "$repo" && cargo build --quiet)
        HIRD_BIN="$repo/target/debug/hird"
    fi
fi

hird() { "$HIRD_BIN" "$@"; }

# ---------------------------------------------------------------- the database

# Point HIRD_DB at a fresh temporary file so an example can never disturb the
# real board.
sandbox_db() {
    HIRD_DB="$(mktemp -d)/hird.db"
    export HIRD_DB
    echo "database: $HIRD_DB"
}

# --------------------------------------------------------------- MCP sessions

# Run one MCP session as $1, reading JSON-RPC requests from stdin.
#
# This is exactly what a harness does: spawn `hird mcp`, initialize, call tools
# over stdio. Doing it by hand here keeps the examples honest — claiming and
# completing are agent-side operations with no CLI equivalent, and this is what
# the agent's tool call actually looks like on the wire.
mcp() {
    local harness="$1"
    {
        echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"hird-examples","version":"0"}}}'
        echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
        cat
    } | HIRD_HARNESS="$harness" "$HIRD_BIN" mcp | tool_results
}

# Print the payload of each tool result, one per line, dropping the handshake.
# `jq` makes it readable; without it you get the raw JSON-RPC, which is fine.
tool_results() {
    if command -v jq >/dev/null 2>&1; then
        jq -r 'select(.result.content != null)
               | (if .result.isError == true then "ERROR: " else "" end)
                 + .result.content[0].text'
    else
        grep -v '"serverInfo"' || true
    fi
}

# A `tools/call` request line, for feeding to `mcp`.
#
#   call 1 task_claim '{"seq":1}'
call() {
    printf '{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' \
        "$1" "$2" "$3"
}

# A heading, so the transcript is readable when several things happen at once.
say() {
    printf '\n== %s\n%s\n' "$*" \
        '--------------------------------------------------------------------'
}

# ------------------------------------------------- long-lived MCP sessions

# `mcp` above is a whole session in one go, which is all most examples need.
# Anything that has to interleave — edit a file, *then* make the call that has
# to notice, while a second agent does the same thing in between — needs the
# sessions held open, for two reasons. A lease belongs to the session that took
# it, so a fresh process is a fresh agent that cannot check in on it. And a
# script feeding requests down a pipe runs far ahead of the server reading
# them, so nothing written between two lines of a heredoc lands between them.
#
# So: one pair of fifos per session, and every call waits for its own answer
# before the script moves on. This is what a harness does, minus the model.
#
#   session_open codex codex
#   session_call codex 1 task_claim '{"seq":1}'
#   edit src/config.rs '// ported'
#   session_call codex 2 task_update '{"seq":1,"note":"ported"}'
#   session_close codex
session_open() {
    local tag="$1" harness="$2" dir r w
    dir="$(mktemp -d)"
    mkfifo "$dir/in" "$dir/out"
    HIRD_HARNESS="$harness" "$HIRD_BIN" mcp <"$dir/in" >"$dir/out" 2>/dev/null &
    printf -v "${tag}_pid" '%s' "$!"
    # In this order: opening the write end releases the server's blocked open
    # of its stdin, which lets it get as far as opening its stdout for us.
    exec {w}>"$dir/in"
    exec {r}<"$dir/out"
    printf -v "${tag}_w" '%s' "$w"
    printf -v "${tag}_r" '%s' "$r"

    printf '%s\n' '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"hird-examples","version":"0"}}}' >&"$w"
    IFS= read -r _ <&"$r"
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}' >&"$w"
}

# session_call <tag> <id> <tool> <json args>
session_call() {
    local w r line
    w="$1_w"; w="${!w}"
    r="$1_r"; r="${!r}"
    call "$2" "$3" "$4" >&"$w"
    IFS= read -r line <&"$r"
    printf '%s\n' "$line" | tool_results
}

session_close() {
    local w pid
    w="$1_w"; w="${!w}"
    pid="$1_pid"; pid="${!pid}"
    # Closing stdin is how a harness ends a session, and it is enough when
    # there is only one. With two open at once each server has inherited a
    # duplicate of the other's request fifo, so nobody sees end-of-file until
    # both are gone — hence the signal, which `hird mcp` shuts down cleanly on.
    eval "exec $w>&-"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# --------------------------------------------------------------- a working tree

# Create a throwaway git repository and work inside it.
#
# The witness watches the project's working tree, so an example that wants to
# show it needs a real one: a real repository, real commits, real edits. This
# makes one in a temporary directory and points HIRD_PROJECT at it.
sandbox_repo() {
    local dir
    dir="$(mktemp -d)/project"
    mkdir -p "$dir/src"
    cd "$dir"
    git init --quiet --initial-branch=main
    git config user.email "examples@hird.invalid"
    git config user.name "hird examples"
    printf '// the config loader\n' >src/config.rs
    printf '# sandbox\n' >README.md
    git add -A
    git commit --quiet -m "initial"
    HIRD_PROJECT="$dir"
    export HIRD_PROJECT
    echo "project:  $dir"
}

# Write a file in the sandbox project, the way an agent editing would.
#
#   edit src/config.rs '// ported'
edit() {
    printf '%s\n' "$2" >"$1"
}

# ------------------------------------------------- MCP 2026-07-28 (stateless)

# The 2026-07-28 lifecycle has no `initialize` and no session to hold state in.
# A client opens with `server/discover`, or simply with the request it wanted to
# make, and every request carries the protocol version, the client's own name
# and its capabilities in `params._meta`.
#
# `hird mcp` serves both lifecycles, so these helpers exist to show the new one
# on the wire rather than because anything needs it.

# The name the example client gives for itself. hird files a session under this
# when HIRD_HARNESS is unset, which is the whole of what it uses it for.
MCP_CLIENT="${MCP_CLIENT:-hird-examples}"

# The `_meta` every 2026-07-28 request carries in place of a handshake.
stateless_meta() {
    printf '{"io.modelcontextprotocol/protocolVersion":"2026-07-28",'
    printf '"io.modelcontextprotocol/clientInfo":{"name":"%s","version":"0"},' "$MCP_CLIENT"
    printf '"io.modelcontextprotocol/clientCapabilities":{}}'
}

# A self-contained `tools/call` request line, for feeding to `mcp_stateless`.
#
#   call_stateless 1 task_claim '{"seq":1}'
call_stateless() {
    printf '{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"%s","arguments":%s,"_meta":%s}}\n' \
        "$1" "$2" "$3" "$(stateless_meta)"
}

# Run one 2026-07-28 session, reading request lines from stdin.
#
# The harness name is optional here in a way it is not above: pass `-` to leave
# HIRD_HARNESS unset and let the client's own name stand as the identity.
mcp_stateless() {
    local harness="${1:--}"
    if [[ "$harness" == "-" ]]; then
        env -u HIRD_HARNESS "$HIRD_BIN" mcp | tool_results
    else
        HIRD_HARNESS="$harness" "$HIRD_BIN" mcp | tool_results
    fi
}

# Ask the server to describe itself, without opening a session first.
discover() {
    local out
    out=$(printf '{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{"_meta":%s}}\n' \
        "$(stateless_meta)" | env -u HIRD_HARNESS "$HIRD_BIN" mcp 2>/dev/null)
    if command -v jq >/dev/null 2>&1; then
        printf '%s\n' "$out" | jq -r '.result
            | "server:  \(.serverInfo.name) \(.serverInfo.version)",
              "speaks:  \(.supportedVersions | join(", "))",
              "caching: \(.cacheScope), ttl \(.ttlMs)ms"'
    else
        printf '%s\n' "$out"
    fi
}
