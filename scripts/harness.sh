#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/debug/agent-helm"
TOKEN="agent-helm-harness-token"

TMP_ROOT=""
API_PID=""
SESSION_IDS=()

fail() {
  printf 'harness: %s\n' "$*" >&2
  exit 1
}

step() {
  printf 'harness: %s\n' "$*"
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

cleanup() {
  local status=$?
  if [[ -n "${API_PID:-}" ]]; then
    kill "$API_PID" >/dev/null 2>&1 || true
    wait "$API_PID" >/dev/null 2>&1 || true
  fi
  for id in "${SESSION_IDS[@]:-}"; do
    tmux kill-session -t "agent-helm-$id" >/dev/null 2>&1 || true
  done
  if [[ -n "${TMP_ROOT:-}" ]]; then
    rm -rf "$TMP_ROOT"
  fi
  exit "$status"
}
trap cleanup EXIT

json_get() {
  python3 -c 'import json,sys
data=json.load(sys.stdin)
for part in sys.argv[1].split("."):
    data = data[int(part)] if part.isdigit() else data[part]
print(data)' "$1"
}

json_has_id() {
  python3 -c 'import json,sys
needle=sys.argv[1]
data=json.load(sys.stdin)
if isinstance(data, dict):
    data = data.get("sessions", [])
sys.exit(0 if any(item.get("id") == needle for item in data) else 1)' "$1"
}

wait_cli_output() {
  local home="$1"
  local id="$2"
  local needle="$3"
  local out=""
  for _ in {1..50}; do
    out="$(HOME="$home" "$BIN" output "$id" --limit 80)"
    [[ "$out" == *"$needle"* ]] && return 0
    sleep 0.1
  done
  printf '%s\n' "$out" >&2
  fail "CLI output never contained: $needle"
}

api_get() {
	curl -fsS -H "x-agent-helm-token: $TOKEN" "$API_URL$1"
}

api_stream() {
	curl -fsS --no-buffer --max-time 3 -H "x-agent-helm-token: $TOKEN" "$API_URL$1" 2>/dev/null || true
}

api_post() {
	local path="$1"
	local body="${2:-{}}"
  curl -fsS \
    -H "x-agent-helm-token: $TOKEN" \
    -H "content-type: application/json" \
    -X POST \
    -d "$body" \
    "$API_URL$path"
}

api_delete() {
  curl -fsS -H "x-agent-helm-token: $TOKEN" -X DELETE "$API_URL$1"
}

wait_api_output() {
  local id="$1"
  local needle="$2"
  local body=""
  local text=""
  for _ in {1..50}; do
    body="$(api_get "/api/sessions/$id/output?limit=80")"
    text="$(printf '%s' "$body" | json_get text)"
    [[ "$text" == *"$needle"* ]] && return 0
    sleep 0.1
  done
  printf '%s\n' "$body" >&2
  fail "API output never contained: $needle"
}

need cargo
need curl
need python3
need tmux

cd "$ROOT"
TMP_ROOT="$(mktemp -d)"
CLI_HOME="$TMP_ROOT/cli-home"
API_HOME="$TMP_ROOT/api-home"
CLI_WORKSPACE="$TMP_ROOT/cli-workspace"
API_WORKSPACE="$TMP_ROOT/api-workspace"
mkdir -p "$CLI_HOME" "$API_HOME" "$CLI_WORKSPACE" "$API_WORKSPACE"

step "format, lint, and unit tests"
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-features
AGENT_HELM_TMUX_SMOKE=1 cargo test tmux_runtime_smoke_test

step "build binary with API support"
cargo build --features serve

step "CLI lifecycle smoke"
HOME="$CLI_HOME" "$BIN" init >/dev/null
session_json="$(
  HOME="$CLI_HOME" "$BIN" --json add "$CLI_WORKSPACE" \
    --agent shell \
    --cmd cat \
    --name harness-cli \
    --group harness \
    --prompt cli-boot
)"
session_id="$(printf '%s' "$session_json" | json_get id)"
SESSION_IDS+=("$session_id")
[[ "$(printf '%s' "$session_json" | json_get status)" == "running" ]] || fail "CLI session did not start"
	wait_cli_output "$CLI_HOME" "$session_id" "cli-boot"
	HOME="$CLI_HOME" "$BIN" send "$session_id" cli-ping
	wait_cli_output "$CLI_HOME" "$session_id" "cli-ping"
	HOME="$CLI_HOME" "$BIN" --json list --group harness | json_has_id "$session_id" || fail "CLI list --group missed session"
	HOME="$CLI_HOME" "$BIN" --json list --status running | json_has_id "$session_id" || fail "CLI list --status missed session"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json stop "$session_id" | json_get status)" == "stopped" ]] || fail "CLI stop failed"
[[ "$(HOME="$CLI_HOME" "$BIN" --json restart "$session_id" | json_get status)" == "running" ]] || fail "CLI restart failed"
HOME="$CLI_HOME" "$BIN" send "$session_id" cli-after-restart
wait_cli_output "$CLI_HOME" "$session_id" "cli-after-restart"
	project_id="$(HOME="$CLI_HOME" "$BIN" --json project list | json_get 0.id)"
	HOME="$CLI_HOME" "$BIN" --json project show "$project_id" >/dev/null
	HOME="$CLI_HOME" "$BIN" --json worktree cleanup "$project_id" | grep -q '"inspected": 0' || fail "CLI worktree cleanup did not return a report"
	HOME="$CLI_HOME" "$BIN" --json project trust "$project_id" >/dev/null
	mcp_id="$(HOME="$CLI_HOME" "$BIN" --json mcp attach "$session_id" harness-mcp | json_get id)"
skill_id="$(HOME="$CLI_HOME" "$BIN" --json skill attach "$session_id" harness-skill | json_get id)"
HOME="$CLI_HOME" "$BIN" --json mcp sync | grep -q '"restart_required": false' || fail "CLI MCP sync did not clear restart flag"
HOME="$CLI_HOME" "$BIN" --json skill sync | grep -q '"restart_required": false' || fail "CLI skill sync did not clear restart flag"
HOME="$CLI_HOME" "$BIN" --json mcp detach "$mcp_id" | grep -q '"status": "detached"' || fail "CLI MCP detach failed"
HOME="$CLI_HOME" "$BIN" --json skill detach "$skill_id" | grep -q '"status": "detached"' || fail "CLI skill detach failed"
	watcher_id="$(HOME="$CLI_HOME" "$BIN" --json watcher start harness-watch | json_get id)"
	HOME="$CLI_HOME" "$BIN" --json watcher stop "$watcher_id" | grep -q '"status": "stopped"' || fail "CLI watcher stop failed"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher test harness-dry-run | json_get status)" == "stopped" ]] || fail "CLI watcher test did not return stopped validation"
	if HOME="$CLI_HOME" "$BIN" --json watcher list | grep -q "harness-dry-run"; then
		fail "CLI watcher test mutated watcher state"
	fi
	conductor_id="$(HOME="$CLI_HOME" "$BIN" --json conductor setup "$session_id" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json conductor start "$conductor_id" | grep -q '"status": "running"' || fail "CLI conductor start failed"
	HOME="$CLI_HOME" "$BIN" --json conductor status "$conductor_id" | grep -q '"status": "running"' || fail "CLI conductor status failed"
	HOME="$CLI_HOME" "$BIN" --json conductor send "$conductor_id" "$session_id" harness-task | grep -q '"status": "assigned"' || fail "CLI conductor send failed"
HOME="$CLI_HOME" "$BIN" --json conductor stop "$conductor_id" | grep -q '"status": "stopped"' || fail "CLI conductor stop failed"
HOME="$CLI_HOME" "$BIN" --json costs >/dev/null
HOME="$CLI_HOME" "$BIN" remove "$session_id" >/dev/null
if HOME="$CLI_HOME" "$BIN" --json list | json_has_id "$session_id"; then
  fail "removed CLI session is still visible"
fi
if ! HOME="$CLI_HOME" "$BIN" --json list --archived | json_has_id "$session_id"; then
  fail "removed CLI session was not archived"
fi

step "HTTP API smoke"
port="$(python3 -c 'import socket
s=socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()')"
	API_URL="http://127.0.0.1:$port"
	api_log="$TMP_ROOT/api.log"
	if HOME="$API_HOME" "$BIN" serve --listen "0.0.0.0:$port" >"$api_log" 2>&1; then
		fail "non-loopback API started without a token"
	fi
	grep -q "requires --token" "$api_log" || fail "non-loopback API token guard returned the wrong error"
	HOME="$API_HOME" "$BIN" serve --listen "127.0.0.1:$port" --token "$TOKEN" >"$api_log" 2>&1 &
	API_PID=$!

for _ in {1..50}; do
  if api_get /api/about >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$API_PID" >/dev/null 2>&1; then
    cat "$api_log" >&2
    fail "API server exited"
  fi
  sleep 0.1
done
api_get /api/about >/dev/null || {
  cat "$api_log" >&2
  fail "API server did not become ready"
}
if curl -fsS "$API_URL/api/about" >/dev/null 2>&1; then
  fail "API accepted an unauthenticated request"
fi
	dashboard_html="$(api_get "/")"
	printf '%s' "$dashboard_html" | grep -q "<title>Agent Helm</title>" || fail "web dashboard did not render"
	printf '%s' "$dashboard_html" | grep -q 'id="new-session"' || fail "web dashboard missing create form"
	printf '%s' "$dashboard_html" | grep -q ">New Session<" || fail "web dashboard missing create button"
	if command -v node >/dev/null 2>&1; then
		dashboard_js="$TMP_ROOT/dashboard.js"
		printf '%s' "$dashboard_html" | python3 -c 'import re,sys
html = sys.stdin.read()
match = re.search(r"<script>(.*?)</script>", html, re.S)
if not match:
    raise SystemExit("missing dashboard script")
print(match.group(1))
' >"$dashboard_js"
		node --check "$dashboard_js" >/dev/null || fail "web dashboard script has syntax errors"
	else
		step "Skipping dashboard script syntax check because node is unavailable"
	fi

	create_body="$(python3 -c 'import json,sys
print(json.dumps({
    "path": sys.argv[1],
    "agent": "shell",
    "command": "cat",
    "name": "harness-api",
    "group_name": "harness",
    "prompt": "api-boot",
}))' "$API_WORKSPACE")"
	api_session_json="$(api_post /api/sessions "$create_body")"
	api_session_id="$(printf '%s' "$api_session_json" | json_get id)"
	SESSION_IDS+=("$api_session_id")
	api_get "/api/sessions?group=harness&status=running" | json_has_id "$api_session_id" || fail "API sessions filters missed session"
	wait_api_output "$api_session_id" "api-boot"
api_post "/api/sessions/$api_session_id/send" '{"text":"api-ping"}' >/dev/null
wait_api_output "$api_session_id" "api-ping"
api_get "/api/sessions/$api_session_id/structured-events" | grep -q '"kind":"input"' || fail "API structured events missing input event"
api_stream "/api/sessions/$api_session_id/terminal-stream?limit=80" | grep -q "event: terminal" || fail "API terminal stream did not emit SSE"
api_stream "/api/sessions/$api_session_id/structured-stream" | grep -q "event: structured" || fail "API structured stream did not emit SSE"
api_post "/api/sessions/$api_session_id/costs" '{"amount_usd":0.0123,"model":"harness-model","input_tokens":10,"output_tokens":15}' >/dev/null
[[ "$(api_get "/api/costs?session_id=$api_session_id&include_archived=true" | json_get event_count)" == "1" ]] || fail "API cost event was not counted"
[[ "$(api_get "/api/costs?session_id=$api_session_id&include_archived=true" | json_get total_tokens)" == "25" ]] || fail "API cost tokens were not summarized"
api_get "/api/projects" | grep -q '"root_path"' || fail "API projects missing created project"
api_project_id="$(api_get "/api/projects" | json_get 0.id)"
HOME="$API_HOME" "$BIN" --json project trust "$api_project_id" >/dev/null
api_mcp_id="$(api_post "/api/sessions/$api_session_id/mcp" '{"id":"api-mcp"}' | json_get id)"
api_skill_id="$(api_post "/api/sessions/$api_session_id/skills" '{"id":"api-skill"}' | json_get id)"
api_get "/api/mcp" | grep -q '"server_id":"api-mcp"' || fail "API MCP attachment missing"
api_get "/api/skills" | grep -q '"skill_id":"api-skill"' || fail "API skill attachment missing"
api_post "/api/mcp/sync" >/dev/null
api_post "/api/skills/sync" >/dev/null
api_post "/api/mcp/$api_mcp_id/detach" | grep -q '"status":"detached"' || fail "API MCP detach failed"
api_post "/api/skills/$api_skill_id/detach" | grep -q '"status":"detached"' || fail "API skill detach failed"
api_watcher_id="$(api_post "/api/watchers/api-watch/start" | json_get id)"
api_post "/api/watchers/$api_watcher_id/stop" | grep -q '"status":"stopped"' || fail "API watcher stop failed"
api_conductor_id="$(HOME="$API_HOME" "$BIN" --json conductor setup "$api_session_id" | json_get id)"
api_post "/api/conductors/$api_conductor_id/start" | grep -q '"status":"running"' || fail "API conductor start failed"
api_get "/api/conductors/$api_conductor_id" | grep -q '"status":"running"' || fail "API conductor get failed"
api_post "/api/conductors/$api_conductor_id/heartbeat" | grep -q '"status":"running"' || fail "API conductor heartbeat failed"
api_post "/api/conductors/$api_conductor_id/send" "{\"session_id\":\"$api_session_id\",\"task_ref\":\"api-task\"}" | grep -q '"status":"assigned"' || fail "API conductor send failed"
api_post "/api/conductors/$api_conductor_id/stop" | grep -q '"status":"stopped"' || fail "API conductor stop failed"
api_get "/api/costs?include_archived=true" | grep -q '"event_count"' || fail "API costs missing summary"
api_post "/api/sessions/$api_session_id/stop" >/dev/null
api_delete "/api/sessions/$api_session_id" >/dev/null
if api_get "/api/sessions?archived=true" | json_has_id "$api_session_id"; then
  :
else
  fail "deleted API session was not archived"
fi
kill "$API_PID" >/dev/null 2>&1 || true
wait "$API_PID" >/dev/null 2>&1 || true
API_PID=""

HOME="$API_HOME" "$BIN" serve --listen "127.0.0.1:$port" --token "$TOKEN" --read-only >"$api_log" 2>&1 &
API_PID=$!
for _ in {1..50}; do
	if api_get /api/about >/dev/null 2>&1; then
		break
	fi
	if ! kill -0 "$API_PID" >/dev/null 2>&1; then
		cat "$api_log" >&2
		fail "read-only API server exited"
	fi
	sleep 0.1
done
api_get /api/about | grep -q '"read_only":true' || fail "read-only API did not report read-only mode"
if api_post /api/sessions "$create_body" >/dev/null 2>&1; then
	fail "read-only API accepted session creation"
fi

step "ok"
