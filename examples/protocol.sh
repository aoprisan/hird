#!/usr/bin/env bash
#
# The wire, on the current MCP spec.
#
# Every other example here opens a session the way harnesses have always opened
# one: an `initialize` request, a handshake, then tool calls against the session
# that handshake created. MCP 2026-07-28 deletes all of that. A client asks the
# server to describe itself, or simply sends the request it wanted to send, and
# each request carries the protocol version, the client's own name and its
# capabilities in `params._meta`.
#
# `hird mcp` serves both, and nothing about the queue can tell which one an
# agent is on. This script is here so you can watch that rather than take it on
# trust — and to show the one thing hird does take from the new lifecycle: a
# client that names itself no longer needs HIRD_HARNESS to be somebody.
#
#   ./examples/protocol.sh
#
# Runs against a throwaway database, so it cannot touch your real queue.

source "$(dirname "$0")/lib/mcp.sh"
sandbox_db

# ------------------------------------------------------ what the server speaks

say "server/discover — no handshake, nothing negotiated yet"

# The opener. A client that has never spoken to this server asks what it is and
# what it speaks, and the answer is self-contained: implementation, capabilities,
# instructions, and every protocol revision on offer.
#
# `caching: private, ttl 0ms` is not an oversight. hird's instructions name the
# current project and are shaped by this machine's config file, so they are
# nobody else's to reuse and stale the moment they are read.
discover

# ------------------------------------------------ a whole task, no handshake

hird add "Port the config loader" --path 'src/config.rs' >/dev/null

say "a session that never calls initialize"

# Three ordinary tool calls. The only difference from `manual-dispatch.sh` is
# the `_meta` on each one, which is what `call_stateless` adds — and which is
# the entire 2026-07-28 lifecycle, on the wire, in one field.
{
    call_stateless 1 task_list '{}'
    call_stateless 2 task_claim '{"seq":1}'
    call_stateless 3 task_complete '{"seq":1,"result":"ported, env-var precedence kept"}'
} | mcp_stateless codex

say "the board does not know the difference"
hird show 1

# ------------------------------------------------------- who the client says it is

say "a harness that never set HIRD_HARNESS is still somebody"

# Every harness config `hird register` writes sets HIRD_HARNESS, because that is
# the half of the identity a human controls. Configured by hand, it is the half
# people forget — and a board of `unknown:af31` and `unknown:9f2c` tells you
# nothing about which tool is where.
#
# Under 2026-07-28 the client names itself on every call, so there is something
# to fall back on. Here the environment is left deliberately unset (`-`) and the
# client calls itself `some-editor`.
hird add "Rewrite the renderer" --path 'src/tui/**' >/dev/null

MCP_CLIENT="some-editor"
call_stateless 1 task_claim '{"seq":2}' | mcp_stateless -

say "and on the board"
hird ls

# The rule in one line: HIRD_HARNESS wins wherever it is set, a client's own
# name is the fallback, `unknown` is what is left when neither will say. The
# name is taken once and then held — an agent that changed its name halfway
# through a session would lose track of its own claims.
