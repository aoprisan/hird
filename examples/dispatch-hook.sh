#!/usr/bin/env bash
#
# The dispatch hook: the one push in a pull design.
#
# Everything else in hird waits to be asked — `task_next` is a tool an agent
# chooses to call, and a task that becomes workable while every agent is idle
# sits on the board in silence. The `dispatch_hook` config key closes that
# seam without a daemon: hird runs your command, detached, at the moment a
# task becomes claimable, with the announcement in its environment. What the
# command does is your business; the case it was built for is prompting an
# idle agent through a terminal multiplexer that can address one, e.g. herdr
# (https://herdr.dev):
#
#   dispatch_hook = 'herdr agent prompt worker "hird task $HIRD_TASK is ready; work the hird queue."'
#
# This script needs no herdr: its hook appends one line per announcement to a
# log file, which shows the contract — every event, in order, as a queue goes
# through its whole life.
#
#   ./examples/dispatch-hook.sh
#
# Runs against a throwaway database and a throwaway config, so it can touch
# neither your real queue nor your real hook.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

# ------------------------------------------------------------ configure a hook

say "you configure a dispatch hook"

# The hook is a config key, so the example sandboxes the config directory the
# same way it sandboxes the database. Yours goes in
# ${XDG_CONFIG_HOME:-~/.config}/hird/config.toml.
XDG_CONFIG_HOME="$(mktemp -d)"
export XDG_CONFIG_HOME
mkdir -p "$XDG_CONFIG_HOME/hird"

log="$(mktemp -d)/herald.log"
cat >"$XDG_CONFIG_HOME/hird/config.toml" <<EOF
dispatch_hook = "echo \"\$HIRD_EVENT #\$HIRD_TASK \$HIRD_TITLE [\$HIRD_RECUSED]\" >> $log"
EOF
cat "$XDG_CONFIG_HOME/hird/config.toml"

# The hook runs detached — hird does not wait for it, so a finished command
# can still be writing. The example waits so the transcript reads in order;
# nothing real needs to.
herald_said() {
    for _ in $(seq 1 100); do
        grep -q "$1" "$log" 2>/dev/null && return
        sleep 0.05
    done
    echo "the hook never reported: $1" >&2
    exit 1
}

# ----------------------------------------------------------------- file a plan

say "filing work announces exactly what is claimable"

# Two tasks, one behind the other. Only the first is claimable, so only the
# first is announced: the hook hears about tasks an agent could take *now*.
gate=$(hird add "Port the config loader" --review --path 'src/config.rs')
blocked=$(hird add "Use the new loader" --needs "$gate")

herald_said "filed #$gate"
cat "$log"

# ------------------------------------------------- the finish that frees work

say "completing task $gate frees task $blocked — and files a review"

# One agent works the gate. Completing it makes two announcements: the
# dependent it unblocked, and — because the work was marked --review — the
# review it filed, itself an open task that the queue will refuse to this
# same harness.
mcp claude-code <<JSON
$(call 1 task_claim "{\"seq\": $gate}")
$(call 2 task_complete "{\"seq\": $gate, \"result\": \"Ported. Precedence unchanged.\"}")
JSON

herald_said "unblocked #$blocked"
# The review's announcement carries the one fact only hird knows: whose work
# it is, and so whom the queue will refuse it to. A hook that can address
# more than one agent routes on HIRD_RECUSED instead of waking the author.
herald_said "review_filed.*\[claude-code\]"
review=$(sed -n 's/^review_filed #\([0-9]*\).*/\1/p' "$log")

# ------------------------------------------------------------- the loop closes

say "a sent-back verdict announces the reopened work"

# A different harness reads the review and sends the work back. The verdict
# reopens task $gate with the findings appended to its brief — and the hook
# hears that the redo is waiting for hands.
mcp codex <<JSON
$(call 1 task_claim "{\"seq\": $review}")
$(call 2 task_complete "{\"seq\": $review, \"result\": \"The env-var layer is skipped on the error path; restore it.\", \"verdict\": \"sent_back\"}")
JSON

herald_said "sent_back #$gate"

# --------------------------------------------------------------- the transcript

say "everything the hook heard, in order"

cat "$log"

say "next"

cat <<EOF
Each line above was one detached run of the configured command, told about one
claimable task through HIRD_EVENT, HIRD_TASK, HIRD_TITLE, HIRD_PROJECT,
HIRD_RECUSED and HIRD_DB. Two events did not appear because nothing here
caused them: released (a holder handing work back) and lease_expired (a
holder going quiet — expiry is enforced lazily, and announced by the next
queue call that notices it).

Note the bracket on the review_filed line: the review arrived announced with
[claude-code], the harness that did the work and the one the queue will
refuse it to. Everything else said [] — anybody's to take.

Point the same key at your multiplexer and the log lines become summonses:

  dispatch_hook = 'herdr agent prompt worker "hird task \$HIRD_TASK is ready; work the hird queue."'

And with two agents on the multiplexer, HIRD_RECUSED is what keeps a summons
from knocking on the wrong door — route the review to hands that may take it:

  dispatch_hook = '''
  worker=claude; case ",\$HIRD_RECUSED," in *,claude-code,*) worker=codex;; esac
  herdr agent prompt "\$worker" "hird task \$HIRD_TASK is ready; work the hird queue."
  '''

Watch the same database live:  HIRD_DB=$HIRD_DB $HIRD_BIN tui
Let agents pull work instead:  ./examples/swarm-plan.sh
EOF
