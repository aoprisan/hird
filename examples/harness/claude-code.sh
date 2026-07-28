#!/usr/bin/env bash
#
# Register hird with Claude Code, user-wide. `hird register claude-code`
# writes the project-scoped equivalent into ./.mcp.json instead.
#
# HIRD_HARNESS is the one thing that must differ between harnesses: it is how
# the board and the other agents tell this session apart (claude-code:af31).
# Leave it unset and the agent shows up as `unknown`.

set -euo pipefail

claude mcp add hird -e HIRD_HARNESS=claude-code -- hird mcp

# Project-local instead of user-wide:
#
#   claude mcp add hird --scope project -e HIRD_HARNESS=claude-code -- hird mcp
#
# Point a session at a scratch database while you try things out:
#
#   claude mcp add hird-scratch \
#       -e HIRD_HARNESS=claude-code -e HIRD_DB=/tmp/scratch/hird.db -- hird mcp
#
# Check it took:
#
#   claude mcp list
