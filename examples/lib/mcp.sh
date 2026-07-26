# Shared helpers for the example scripts. Source it, don't run it.
#
#   source "$(dirname "$0")/lib/mcp.sh"
#   sandbox_db                       # throwaway HIRD_DB
#   hird add "something"             # the binary, installed or freshly built
#   mcp codex <<'JSON'               # one MCP session, one JSON-RPC line per call
#   {"jsonrpc":"2.0","id":1,"method":"tools/call", …}
#   JSON

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
