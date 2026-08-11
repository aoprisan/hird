#!/usr/bin/env bash
#
# The feed: the board as a log.
#
# Every mutation in hird lands one row in an append-only trail; `hird events`
# is that trail read across tasks, in the order things happened — the TUI's
# view, for everything that cannot sit at a screen. One shot prints the
# backlog. `--follow` keeps reading, and `--json` turns each line into one
# machine-readable object, which is how a script, a dashboard, or another
# agent watches a swarm work.
#
#   ./examples/events.sh
#
# Runs against a throwaway database and a throwaway git repository, so it can
# touch neither your real queue nor your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "two tasks, and a queue that has not been touched yet"

port=$(hird add "Port the config loader")
audit=$(hird add "Audit the config loader")

# ------------------------------------------------------------- a follower tail

say "a follower starts reading before anything happens"

# This is the monitoring pane: `hird events --follow` in a second terminal, a
# tmux pane, or — as here — a process writing to a log. It polls the same
# SQLite file the TUI polls, at the same cadence, and needs no daemon.
feed="$(mktemp -d)/feed.log"
hird events --follow >"$feed" 2>&1 &
follower=$!

# --------------------------------------------------------------- agents work

say "meanwhile, two harnesses work the queue"

session_open codex codex
session_call codex 1 task_claim "{\"seq\": $port}"
edit src/config.rs '// codex ported the loader'
session_call codex 2 task_update "{\"seq\": $port, \"note\": \"halfway through the port\"}"
session_call codex 3 task_complete "{\"seq\": $port, \"result\": \"ported; env still wins\"}"
session_close codex

session_open claude claude-code
session_call claude 1 task_claim "{\"seq\": $audit}"
session_call claude 2 task_complete "{\"seq\": $audit, \"result\": \"read it; precedence is right\"}"
session_close claude

# Give the follower one more poll to drain the trail, then stop it. Stopping
# costs nothing and forgets nothing — the cursor is the trail itself.
sleep 1
kill "$follower" 2>/dev/null || true
wait "$follower" 2>/dev/null || true

say "what the follower saw, as it happened"

cat "$feed"

# ------------------------------------------------------------ the same, again

say "hird events — the same trail, read after the fact"

# Nothing was stored for the feed and nothing depends on having followed it:
# the one-shot form reads the identical record.
hird events

say "narrowed to what you were waiting for"

hird events --kind claimed,completed

say "one line, one object — the form a machine reads"

hird events --json --kind completed

say "next"

cat <<EOF
The follower's log and the one-shot listing say the same thing, because
neither is a subscription: both are the append-only trail every hird
mutation already writes, read across tasks. That is why the tail can be
stopped and restarted for free, why it needs no daemon, and why the two
agents' doings interleave in the order they actually happened — including
the claims and completions no CLI verb can perform.

--json is the integration surface: pipe it into jq, a log shipper, or the
thing that pages you. The dispatch_hook (./examples/dispatch-hook.sh) wakes
an agent when work appears; the feed reports what the swarm then did.

Watch a live board instead:      HIRD_DB=$HIRD_DB $HIRD_BIN tui
Filter to one agent:             hird events --actor codex:9f2c
Everything in the database:      hird events --all-projects
EOF
