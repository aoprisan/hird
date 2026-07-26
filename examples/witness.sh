#!/usr/bin/env bash
#
# The witness: what the working tree says actually happened.
#
# Two agents, one checkout, one file, and no commit between them — the failure
# a status machine cannot see, because both agents report success and git has
# nothing to diff. Watch hird catch it while there is still time to fix it.
#
#   ./examples/witness.sh
#
# Runs against a throwaway database *and* a throwaway git repository, so it
# cannot touch your real queue or your real work.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db
sandbox_repo

# --------------------------------------------------------------- file the work

say "two tasks, both declaring the same loader"

# The declared scope is half the story. It is what lets hird say *who* later on:
# an agent that declares a file has said it holds a copy and means to write it.
port=$(hird add "Port the config loader"  --path 'src/config.rs')
audit=$(hird add "Audit the config loader" --path 'src/*.rs')

hird ls

# -------------------------------------------------------------- two terminals

# Two sessions, held open side by side, exactly as two harnesses would hold
# them. Every call below waits for its answer, so the edits between them really
# do land between them.
session_open codex codex
session_open claude claude-code

say "both agents claim, and see the overlap coming"

# This much the collision detector already saw: two declarations that can name
# the same file. It is a forecast — nothing has happened yet.
session_call codex 1 task_claim "{\"seq\": $port, \"paths\": [\"src/config.rs\"]}"
session_call claude 1 task_claim "{\"seq\": $audit, \"paths\": [\"src/*.rs\"]}"

say "codex edits the loader and checks in"

edit src/config.rs '// codex ported the loader'

# `changed` is not something the agent reported. It is the difference between
# the repository now and the fingerprint hird took when the task was claimed.
session_call codex 2 task_update "{\"seq\": $port, \"note\": \"ported the loader\"}"

say "meanwhile, in the other terminal, claude-code writes the same file"

edit src/config.rs '// claude-code rewrote the loader entirely'
session_call claude 2 task_update "{\"seq\": $audit, \"note\": \"rewrote the loader\"}"

say "codex checks in again — and is told before it writes"

# Codex is holding a copy of src/config.rs that is no longer on disk. It has no
# way to know that: it cannot see claude-code's session, and nothing has been
# committed for git to compare.
session_call codex 3 task_update "{\"seq\": $port, \"note\": \"carrying on\"}"

say "and an edit nobody declared"

edit src/mcp.rs '// nobody said anything about this file'
session_call codex 4 task_update "{\"seq\": $port, \"note\": \"one more thing\"}"

session_close codex
session_close claude

say "hird agents — declared, then witnessed"

# `files` is what was announced. `moved` is what happened. The gap between the
# two lines is the whole point of the screen.
hird agents

say "hird show — the evidence behind the task"

hird show "$port"

say "next"

cat <<EOF
Read the third check-in again. Codex declared src/config.rs, claude-code
declared src/*.rs, the file moved under both of them, and the two of them
disagree about what it now says — so codex is told, by name, to re-read the
file before its next write. Neither agent could have known on its own: they
cannot see each other's sessions, and nothing has been committed for git to
compare.

The fourth call is the quieter half. src/mcp.rs moved and nobody had declared
it, so the collision checks the *other* agents are running cannot see it, and
hird says so — without claiming to know whose edit it was, because with two
agents live in one checkout it does not.

Watch it live:                  HIRD_DB=$HIRD_DB $HIRD_BIN tui
Turn it off:                    witness = false             (see config.toml)
The predicted version:          ./examples/swarm-plan.sh
EOF
