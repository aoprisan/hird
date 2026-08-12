#!/bin/sh
#
# The feed pane: `hird events --follow` — the board as a log, and the
# process that keeps the pairing honest while nobody is looking.
#
# hird has no daemon. A lease that runs out is enforced by whoever reads
# the queue next, and the announcement that would summon a replacement
# is made by that same reader — so a swarm whose agents have all stopped
# calling has nobody left to notice that one of them died, and the relay
# never fires for the one case it is most needed in. A feed left open is
# that somebody: every poll it sweeps, announces what it collected
# through the dispatch hook, and prints the trail as it goes.
#
# The cwd dance is board.sh's, and for the same reason: hird scopes to
# the project it runs in, and a plugin pane starts in the plugin's own
# directory. Repeated rather than sourced from a shared file so that the
# read before the install stays one whole script at a time.

set -u

# Pull one string field out of HERDR_PLUGIN_CONTEXT_JSON. A path with an
# escaped quote in it defeats this; the fallback is the feed unscoped,
# not a failure.
ctx_field() {
    printf '%s' "${HERDR_PLUGIN_CONTEXT_JSON:-}" |
        sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"
}

for dir in "$(ctx_field focused_pane_cwd)" "$(ctx_field workspace_cwd)"; do
    if [ -n "$dir" ] && [ -d "$dir" ]; then
        cd "$dir" && break
    fi
done

if command -v hird >/dev/null 2>&1; then
    exec hird events --follow
fi

echo "hird is not on PATH."
echo
echo "Install it — https://github.com/aoprisan/hird — then reopen this pane."
printf 'Press Enter to close. '
read -r _
exit 1
