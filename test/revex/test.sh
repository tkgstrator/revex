#!/bin/sh
set -eu

test -x /usr/local/bin/revex
command -v revex >/dev/null 2>&1
test "$(command -v revex)" = /usr/local/bin/revex

# Revex has no --help/--version CLI flag. MCP initialize exercises the binary
# and its dynamic loader without contacting an API or requiring credentials.
test_home=$(mktemp -d "${TMPDIR:-/tmp}/revex-feature-home.XXXXXX")
trap 'rm -rf "$test_home"' 0
trap 'exit 1' 1 2 3 15
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' |
    HOME=$test_home REVEX_BASE_URL=http://127.0.0.1:1/v1 \
    REVEX_MODEL=feature-smoke REVEX_API_KEY=feature-smoke \
    /usr/local/bin/revex --mcp > "$test_home/response"
grep -F '"name":"revex"' "$test_home/response" >/dev/null
