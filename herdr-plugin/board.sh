#!/bin/sh
#
# The board pane: `hird tui`, opened over whatever you were looking at.
#
# hird scopes its board to the project it is run in, and a plugin pane
# starts in the plugin's own directory — the wrong project by
# construction. Herdr hands the right one over in the invocation
# context, so this script moves to the focused pane's directory (or the
# workspace's) before handing the terminal to the TUI.

set -u

# Pull one string field out of HERDR_PLUGIN_CONTEXT_JSON. A path with an
# escaped quote in it defeats this; the fallback is the board unscoped,
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
    exec hird tui
fi

echo "hird is not on PATH."
echo
echo "Install it — https://github.com/aoprisan/hird — then reopen this pane."
printf 'Press Enter to close. '
read -r _
exit 1
