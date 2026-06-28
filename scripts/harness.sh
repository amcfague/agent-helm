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

init_git_project() {
  local path="$1"
  git -C "$path" init -b main >/dev/null
  git -C "$path" config user.email agent-helm-harness@example.invalid
  git -C "$path" config user.name "Agent Helm Harness"
  printf 'harness\n' >"$path/README.md"
  git -C "$path" add README.md
  git -C "$path" commit -m "init" >/dev/null
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
	local body="${2-}"
	[[ -n "$body" ]] || body="{}"
	curl -fsS \
		-H "x-agent-helm-token: $TOKEN" \
		-H "content-type: application/json" \
		-X POST \
		-d "$body" \
		"$API_URL$path"
}

api_post_status() {
	local path="$1"
	local body="${2-}"
	[[ -n "$body" ]] || body="{}"
	curl -sS -o /dev/null -w "%{http_code}" \
		-H "x-agent-helm-token: $TOKEN" \
		-H "content-type: application/json" \
		-X POST \
		-d "$body" \
		"$API_URL$path"
}

api_patch() {
	local path="$1"
	local body="${2-}"
	[[ -n "$body" ]] || body="{}"
	curl -fsS \
		-H "x-agent-helm-token: $TOKEN" \
		-H "content-type: application/json" \
		-X PATCH \
		-d "$body" \
		"$API_URL$path"
}

api_delete() {
	curl -fsS -H "x-agent-helm-token: $TOKEN" -X DELETE "$API_URL$1"
}

dashboard_has() {
	grep -q "$1" <<< "$dashboard_html"
}

write_claude_tool_transcript() {
	local home="$1"
	local session_id="$2"
	local cwd="$3"
	local tool="$4"
	local file="$home/.claude/projects/harness/$session_id.jsonl"
	mkdir -p "$(dirname "$file")"
	python3 -c 'import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
session_id, cwd, tool = sys.argv[2:5]
rows = [
    {"agent_helm_session_id": session_id, "type": "user", "message": {"role": "user", "content": "work"}, "cwd": cwd},
    {"agent_helm_session_id": session_id, "type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "name": tool, "input": {"command": "cargo test"}}]}, "cwd": cwd},
]
path.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
' "$file" "$session_id" "$cwd" "$tool"
}

write_codex_tool_transcript() {
	local home="$1"
	local session_id="$2"
	local cwd="$3"
	local tool="$4"
	local file="$home/.codex/sessions/harness/$session_id.jsonl"
	mkdir -p "$(dirname "$file")"
	python3 -c 'import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
session_id, cwd, tool = sys.argv[2:5]
rows = [
    {"type": "session_meta", "payload": {"agent_helm_session_id": session_id, "cwd": cwd}},
    {"type": "response_item", "payload": {"type": "function_call", "call_id": "call-harness", "name": tool, "arguments": "{\"cmd\":\"cargo test agenthelm-codex-search-needle\"}"}},
]
path.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
' "$file" "$session_id" "$cwd" "$tool"
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
CLI_DEFAULT_PROJECT="$TMP_ROOT/cli-default-project"
API_DEFAULT_PROJECT="$TMP_ROOT/api-default-project"
mkdir -p "$CLI_HOME" "$API_HOME" "$CLI_WORKSPACE" "$API_WORKSPACE" "$CLI_DEFAULT_PROJECT" "$API_DEFAULT_PROJECT"
init_git_project "$CLI_WORKSPACE"
init_git_project "$API_WORKSPACE"
mkdir -p "$CLI_HOME/.config/agent-helm" "$API_HOME/.config/agent-helm"
cat >"$CLI_HOME/.config/agent-helm/settings.toml" <<'TOML'
[tools.review-bot]
executable = "cat"
worktree = "never"
TOML
cp "$CLI_HOME/.config/agent-helm/settings.toml" "$API_HOME/.config/agent-helm/settings.toml"

step "format, lint, and unit tests"
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-features
AGENT_HELM_TMUX_SMOKE=1 cargo test tmux_runtime_smoke_test
AGENT_HELM_TMUX_SMOKE=1 cargo test embedded_tmux_smoke_test
AGENT_HELM_TMUX_SMOKE=1 cargo test terminal_preview_smoke_test

step "build binary with API support"
cargo build --features serve

step "CLI lifecycle smoke"
HOME="$CLI_HOME" "$BIN" init >/dev/null
cli_group="$(HOME="$CLI_HOME" "$BIN" --json group create qa --parent harness --default-project-path "$CLI_WORKSPACE")"
[[ "$(printf '%s' "$cli_group" | json_get name)" == "harness/qa" ]] || fail "CLI group create returned wrong group"
HOME="$CLI_HOME" "$BIN" --json group update harness/qa --collapsed true | grep -q '"collapsed": true' || fail "CLI group update failed"
HOME="$CLI_HOME" "$BIN" group delete harness/qa || fail "CLI group delete failed"
diff_session_json="$(HOME="$CLI_HOME" "$BIN" --json add "$CLI_WORKSPACE" --agent shell --cmd cat --name harness-diff --group harness)"
diff_session_id="$(printf '%s' "$diff_session_json" | json_get id)"
SESSION_IDS+=("$diff_session_id")
printf 'cli changed\n' >"$CLI_WORKSPACE/README.md"
printf 'cli staged\n' >"$CLI_WORKSPACE/cli-staged.txt"
git -C "$CLI_WORKSPACE" add cli-staged.txt
printf 'cli untracked\n' >"$CLI_WORKSPACE/cli-untracked.txt"
cli_diff_text="$(HOME="$CLI_HOME" "$BIN" --json session diff "$diff_session_id" | json_get text)"
printf '%s' "$cli_diff_text" | grep -q '## status' || fail "CLI JSON diff missing status section"
printf '%s' "$cli_diff_text" | grep -q '+cli changed' || fail "CLI JSON diff missing unstaged changes"
printf '%s' "$cli_diff_text" | grep -q '+cli staged' || fail "CLI JSON diff missing staged changes"
printf '%s' "$cli_diff_text" | grep -q '## untracked: cli-untracked.txt' || fail "CLI JSON diff missing untracked section"
git -C "$CLI_WORKSPACE" restore --staged cli-staged.txt
git -C "$CLI_WORKSPACE" restore README.md
rm -f "$CLI_WORKSPACE/cli-staged.txt" "$CLI_WORKSPACE/cli-untracked.txt"
HOME="$CLI_HOME" "$BIN" session remove "$diff_session_id" >/dev/null
review_json="$(HOME="$CLI_HOME" "$BIN" --json add "$CLI_WORKSPACE" --agent review-bot --name harness-review-bot --group harness)"
review_id="$(printf '%s' "$review_json" | json_get id)"
SESSION_IDS+=("$review_id")
[[ "$(printf '%s' "$review_json" | json_get command)" == "cat" ]] || fail "CLI custom tool profile command was not used"
printf '%s' "$review_json" | grep -q '"worktree_id": null' || fail "CLI custom tool profile unexpectedly created a worktree"
HOME="$CLI_HOME" "$BIN" session remove "$review_id" >/dev/null
mkdir -p "$CLI_HOME/.claude/projects/harness"
printf '%s\n' '{"sessionId":"cli-transcript-session","message":{"content":"cli-transcript-agenthelm-needle"},"cwd":"/tmp/cli-transcript"}' \
	>"$CLI_HOME/.claude/projects/harness/cli-transcript-session.jsonl"
[[ "$(HOME="$CLI_HOME" "$BIN" --json session search cli-transcript-agenthelm-needle | json_get results.0.session_id)" == "cli-transcript-session" ]] || fail "CLI session search missed Claude transcript"
cli_claude_json="$(
	HOME="$CLI_HOME" "$BIN" --json add "$CLI_WORKSPACE" \
		--agent claude \
		--cmd cat \
		--name harness-claude-sync \
		--group harness
)"
cli_claude_id="$(printf '%s' "$cli_claude_json" | json_get id)"
SESSION_IDS+=("$cli_claude_id")
write_claude_tool_transcript "$CLI_HOME" "$cli_claude_id" "$CLI_WORKSPACE" "Bash"
cli_sync="$(HOME="$CLI_HOME" "$BIN" --json session sync-state "$cli_claude_id")"
printf '%s' "$cli_sync" | grep -q '"synced": true' || fail "CLI Claude sync-state did not sync"
printf '%s' "$cli_sync" | grep -q '"source": "claude_transcript"' || fail "CLI Claude sync-state source mismatch"
printf '%s' "$cli_sync" | grep -q '"name": "Bash"' || fail "CLI Claude sync-state missed tool"
HOME="$CLI_HOME" "$BIN" --json session sync-state "$cli_claude_id" | grep -q '"synced": false' || fail "CLI Claude sync-state duplicated transcript event"
HOME="$CLI_HOME" "$BIN" --json session events "$cli_claude_id" --limit 20 | grep -q '"source": "claude_transcript"' || fail "CLI Claude sync-state event missing from raw events"
session_json="$(
	HOME="$CLI_HOME" "$BIN" --json add "$CLI_WORKSPACE" \
		--agent codex \
		--cmd cat \
    --name harness-cli \
    --group harness \
    --prompt cli-boot
)"
session_id="$(printf '%s' "$session_json" | json_get id)"
session_workspace_id="$(printf '%s' "$session_json" | json_get workspace_id)"
SESSION_IDS+=("$session_id")
	[[ "$(printf '%s' "$session_json" | json_get status)" == "running" ]] || fail "CLI session did not start"
	wait_cli_output "$CLI_HOME" "$session_id" "cli-boot"
	write_codex_tool_transcript "$CLI_HOME" "$session_id" "$CLI_WORKSPACE" "shell"
	cli_codex_sync="$(HOME="$CLI_HOME" "$BIN" --json session sync-state "$session_id")"
	printf '%s' "$cli_codex_sync" | grep -q '"synced": true' || fail "CLI Codex sync-state did not sync"
	printf '%s' "$cli_codex_sync" | grep -q '"source": "codex_transcript"' || fail "CLI Codex sync-state source mismatch"
	printf '%s' "$cli_codex_sync" | grep -q '"name": "shell"' || fail "CLI Codex sync-state missed tool"
	HOME="$CLI_HOME" "$BIN" --json session sync-state "$session_id" | grep -q '"synced": false' || fail "CLI Codex sync-state duplicated transcript event"
	HOME="$CLI_HOME" "$BIN" --json session events "$session_id" --limit 20 | grep -q '"source": "codex_transcript"' || fail "CLI Codex sync-state event missing from raw events"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json session search agenthelm-codex-search-needle | json_get results.0.session_id)" == "$session_id" ]] || fail "CLI session search missed Codex transcript"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json session search agenthelm-codex-search-needle | json_get results.0.source)" == "codex_transcript" ]] || fail "CLI session search Codex source mismatch"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json session search cli-boot | json_get results.0.session_id)" == "$session_id" ]] || fail "CLI session search missed prompt"
	HOME="$CLI_HOME" "$BIN" session output "$session_id" --limit 40 | grep -q "cli-boot" || fail "CLI session output missed prompt"
	HOME="$CLI_HOME" "$BIN" --json output "$session_id" --limit 40 | json_get text | grep -q "cli-boot" || fail "CLI --json output missed prompt"
	HOME="$CLI_HOME" "$BIN" --json session output "$session_id" --limit 40 | json_get text | grep -q "cli-boot" || fail "CLI --json session output missed prompt"
	session_create_json="$(
		HOME="$CLI_HOME" "$BIN" --json session create "$CLI_WORKSPACE" \
			--agent shell \
			--cmd cat \
			--name harness-session-create \
			--group harness \
			--carry-state
	)"
	session_create_id="$(printf '%s' "$session_create_json" | json_get id)"
	SESSION_IDS+=("$session_create_id")
	[[ "$(printf '%s' "$session_create_json" | json_get status)" == "running" ]] || fail "CLI session create did not start"
	HOME="$CLI_HOME" "$BIN" session remove "$session_create_id" >/dev/null
HOME="$CLI_HOME" "$BIN" session send "$session_id" cli-ping
wait_cli_output "$CLI_HOME" "$session_id" "cli-ping"
HOME="$CLI_HOME" "$BIN" --json session status "$session_id" | grep -q '"deck_status": "waiting"' || fail "CLI session status missing waiting deck status"
HOME="$CLI_HOME" "$BIN" --json session status-snapshot "$session_id" | grep -q '"deck_status": "waiting"' || fail "CLI status snapshot missing waiting deck status"
HOME="$CLI_HOME" "$BIN" --json list --status waiting | json_has_id "$session_id" || fail "CLI list --status waiting missed deck-status session"
HOME="$CLI_HOME" "$BIN" --json session record-event "$session_id" --state working --source cli-harness --tool Bash | grep -q '"kind": "agent_state"' || fail "CLI session record-event failed"
HOME="$CLI_HOME" "$BIN" --json session status "$session_id" | grep -q '"deck_status": "running"' || fail "CLI session record-event did not update deck status"
HOME="$CLI_HOME" "$BIN" --json session record-event "$session_id" --state idle | grep -q '"source": "cli"' || fail "CLI session record-event did not default source"
if HOME="$CLI_HOME" "$BIN" --json session record-event "$session_id" --kind output --state working >/dev/null 2>&1; then
	fail "CLI session record-event accepted unsupported kind"
fi
HOME="$CLI_HOME" "$BIN" --json costs record "$session_id" 0.0123 --model harness-cli --input-tokens 10 --output-tokens 15 >/dev/null
[[ "$(HOME="$CLI_HOME" "$BIN" --json costs --session "$session_id" --model harness-cli | json_get event_count)" == "1" ]] || fail "CLI cost event was not counted"
[[ "$(HOME="$CLI_HOME" "$BIN" --json costs --session "$session_id" --model harness-cli | json_get total_tokens)" == "25" ]] || fail "CLI cost tokens were not summarized"
if HOME="$CLI_HOME" "$BIN" --json costs record "$session_id" 0.01 --input-tokens=-1 >/dev/null 2>&1; then
	fail "CLI cost record accepted negative tokens"
fi
cli_total_cost="$(HOME="$CLI_HOME" "$BIN" --json costs record "$session_id" 0.01 --model harness-cli-total --total-tokens 99 --source cli-harness-total)"
[[ "$(printf '%s' "$cli_total_cost" | json_get payload.source)" == "cli-harness-total" ]] || fail "CLI cost source was not recorded"
[[ "$(HOME="$CLI_HOME" "$BIN" --json costs --session "$session_id" --model harness-cli-total | json_get total_tokens)" == "99" ]] || fail "CLI cost total tokens were not recorded"
[[ "$(HOME="$CLI_HOME" "$BIN" --json costs --session "$session_id" --model harness-cli-total events --limit 1 | json_get 0.payload.source)" == "cli-harness-total" ]] || fail "CLI cost events missed recorded source"
cli_events="$(HOME="$CLI_HOME" "$BIN" --json session events "$session_id" --limit 20)"
printf '%s' "$cli_events" | grep -q '"kind": "input"' || fail "CLI session events missing input event"
printf '%s' "$cli_events" | grep -q '"kind": "agent_state"' || fail "CLI session events missing ingested agent_state event"
printf '%s' "$cli_events" | grep -q '"source": "cli"' || fail "CLI session events missing default CLI source"
printf '%s' "$cli_events" | python3 -c 'import json,sys; events=json.load(sys.stdin); sys.exit(0 if any(e.get("kind") == "input" and e.get("payload", {}).get("source") == "user" and "source" not in e for e in events) else 1)' || fail "CLI session events did not return raw user input event"
cli_structured_events="$(HOME="$CLI_HOME" "$BIN" --json session structured-events "$session_id" --limit 20)"
printf '%s' "$cli_structured_events" | grep -q '"mode": "snapshot"' || fail "CLI structured events missing snapshot mode"
printf '%s' "$cli_structured_events" | grep -q '"kind": "input"' || fail "CLI structured events missing input event"
printf '%s' "$cli_structured_events" | grep -q '"source": "cli"' || fail "CLI structured events missing recorded source"
HOME="$CLI_HOME" "$BIN" --json list --group harness | json_has_id "$session_id" || fail "CLI list --group missed session"
	HOME="$CLI_HOME" "$BIN" --json list --status running | json_has_id "$session_id" || fail "CLI list --status missed session"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json session stop "$session_id" | json_get status)" == "stopped" ]] || fail "CLI session stop failed"
	[[ "$(HOME="$CLI_HOME" "$BIN" --json session start "$session_id" | json_get status)" == "running" ]] || fail "CLI session start failed"
HOME="$CLI_HOME" "$BIN" session send "$session_id" cli-after-restart
wait_cli_output "$CLI_HOME" "$session_id" "cli-after-restart"
fork_json="$(HOME="$CLI_HOME" "$BIN" --json session fork "$session_id" --name harness-paused-fork --no-start)"
fork_id="$(printf '%s' "$fork_json" | json_get child_session_id)"
printf '%s' "$fork_json" | grep -q '"started": false' || fail "CLI session fork --no-start started child"
HOME="$CLI_HOME" "$BIN" --json session status "$fork_id" | grep -q '"status": "stopped"' || fail "CLI session fork --no-start child was not stopped"
HOME="$CLI_HOME" "$BIN" session remove "$fork_id" >/dev/null
HOME="$CLI_HOME" "$BIN" --json session archive "$session_id" --reason harness-restore >/dev/null
if HOME="$CLI_HOME" "$BIN" --json list | json_has_id "$session_id"; then
	fail "CLI archived session is still visible"
fi
HOME="$CLI_HOME" "$BIN" --json session restore "$session_id" | grep -q '"archived": false' || fail "CLI session restore failed"
HOME="$CLI_HOME" "$BIN" --json session start "$session_id" >/dev/null
project_id="$(HOME="$CLI_HOME" "$BIN" --json project list | json_get 0.id)"
HOME="$CLI_HOME" "$BIN" --json project show "$project_id" >/dev/null
HOME="$CLI_HOME" "$BIN" --json workspace show "$session_workspace_id" | grep -q '"path":' || fail "CLI workspace show failed"
HOME="$CLI_HOME" "$BIN" --json workspace list "$project_id" | json_has_id "$session_workspace_id" || fail "CLI workspace list missed session workspace"
cli_default_project="$(HOME="$CLI_HOME" "$BIN" --json project add "$CLI_DEFAULT_PROJECT" --default-branch harness-main --trust)"
[[ "$(printf '%s' "$cli_default_project" | json_get default_branch)" == "harness-main" ]] || fail "CLI project add did not preserve default branch"
cli_default_project_update="$(HOME="$CLI_HOME" "$BIN" --json project add "$CLI_DEFAULT_PROJECT" --default-branch harness-trunk)"
[[ "$(printf '%s' "$cli_default_project_update" | json_get default_branch)" == "harness-trunk" ]] || fail "CLI project re-add did not update default branch"
HOME="$CLI_HOME" "$BIN" --json worktree cleanup "$project_id" | grep -q '"inspected":' || fail "CLI worktree cleanup did not return a report"
if HOME="$CLI_HOME" "$BIN" --json worktree cleanup missing-project >/dev/null 2>&1; then
	fail "CLI worktree cleanup accepted missing project"
fi
if HOME="$CLI_HOME" "$BIN" --json worktree list missing-project >/dev/null 2>&1; then
	fail "CLI worktree list accepted missing project"
fi
HOME="$CLI_HOME" "$BIN" --json project trust "$project_id" >/dev/null
cli_watcher_config="$(python3 -c 'import json,sys; print(json.dumps({"command":"printf cli-watch","session_id":sys.argv[1]}))' "$session_id")"
cli_watcher_id="$(HOME="$CLI_HOME" "$BIN" --json watcher create cli-watch --adapter shell --project "$project_id" --config "$cli_watcher_config" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json watcher list --project "$project_id" | json_has_id "$cli_watcher_id" || fail "CLI watcher list --project missed watcher"
[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher start cli-watch | json_get id)" == "$cli_watcher_id" ]] || fail "CLI watcher start by name failed"
HOME="$CLI_HOME" "$BIN" --json watcher events cli-watch | grep -q "cli-watch" || fail "CLI watcher events by name missing cli-watch"
wait_cli_output "$CLI_HOME" "$session_id" "cli-watch"
[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher poll cli-watch | json_get status)" == "running" ]] || fail "CLI watcher poll by name failed"
[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher events "$cli_watcher_id" | grep -o "cli-watch" | wc -l | tr -d ' ')" -ge 2 ]] || fail "CLI watcher poll did not append output"
HOME="$CLI_HOME" "$BIN" --json watcher poll-all | grep -q '"status": "running"' || fail "CLI watcher poll-all failed"
[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher events "$cli_watcher_id" | grep -o "cli-watch" | wc -l | tr -d ' ')" -ge 3 ]] || fail "CLI watcher poll-all did not append output"
cli_external_watcher_config="$(python3 -c 'import json,sys; print(json.dumps({"session_id":sys.argv[1]}))' "$session_id")"
cli_external_watcher_id="$(HOME="$CLI_HOME" "$BIN" --json watcher create cli-external-watch --config "$cli_external_watcher_config" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json watcher start "$cli_external_watcher_id" >/dev/null
HOME="$CLI_HOME" "$BIN" --json watcher ingest "$cli_external_watcher_id" cli-external-watch --event-type push | grep -q '"route_decision": "session:' || fail "CLI watcher ingest did not route"
HOME="$CLI_HOME" "$BIN" --json watcher events "$cli_external_watcher_id" | grep -q "cli-external-watch" || fail "CLI watcher ingest missing event"
wait_cli_output "$CLI_HOME" "$session_id" "cli-external-watch"
HOME="$CLI_HOME" "$BIN" watcher remove "$cli_external_watcher_id" || fail "CLI external watcher remove failed"
mcp_id="$(HOME="$CLI_HOME" "$BIN" --json mcp attach "$session_id" harness-mcp | json_get id)"
HOME="$CLI_HOME" "$BIN" --json mcp attach-project "$project_id" harness-project-mcp | grep -q '"scope": "project"' || fail "CLI project MCP attach failed"
HOME="$CLI_HOME" "$BIN" --json mcp attach-profile harness-profile-mcp | grep -q '"scope": "profile"' || fail "CLI profile MCP attach failed"
skill_id="$(HOME="$CLI_HOME" "$BIN" --json skill attach "$session_id" harness-skill | json_get id)"
HOME="$CLI_HOME" "$BIN" --json skill attach-project "$project_id" harness-project-skill | grep -q '"scope": "project"' || fail "CLI project skill attach failed"
HOME="$CLI_HOME" "$BIN" --json skill attach-profile harness-profile-skill | grep -q '"scope": "profile"' || fail "CLI profile skill attach failed"
materialization_json="$(HOME="$CLI_HOME" "$BIN" --json session materialization "$session_id")"
printf '%s' "$materialization_json" | grep -q '"server_id": "harness-mcp"' || fail "CLI session materialization missing MCP"
printf '%s' "$materialization_json" | grep -q '"skill_id": "harness-skill"' || fail "CLI session materialization missing skill"
cli_shell_tools_json="$(HOME="$CLI_HOME" "$BIN" --json session create "$CLI_WORKSPACE" --agent shell --cmd cat --name harness-shell-tools --group harness)"
cli_shell_tools_id="$(printf '%s' "$cli_shell_tools_json" | json_get id)"
SESSION_IDS+=("$cli_shell_tools_id")
cli_shell_materialization="$(HOME="$CLI_HOME" "$BIN" --json session materialization "$cli_shell_tools_id")"
printf '%s' "$cli_shell_materialization" | grep -q '"mcp": \[\]' || fail "CLI shell materialization inherited MCP"
printf '%s' "$cli_shell_materialization" | grep -q '"skills": \[\]' || fail "CLI shell materialization inherited skills"
mcp_sync="$(HOME="$CLI_HOME" "$BIN" --json mcp sync)"
[[ "$mcp_sync" == *'"restart_required": false'* ]] || fail "CLI MCP sync did not clear restart flag"
skill_sync="$(HOME="$CLI_HOME" "$BIN" --json skill sync)"
[[ "$skill_sync" == *'"restart_required": false'* ]] || fail "CLI skill sync did not clear restart flag"
HOME="$CLI_HOME" "$BIN" --json mcp detach "$mcp_id" | grep -q '"status": "detached"' || fail "CLI MCP detach failed"
HOME="$CLI_HOME" "$BIN" --json skill detach "$skill_id" | grep -q '"status": "detached"' || fail "CLI skill detach failed"
watcher_id="$(HOME="$CLI_HOME" "$BIN" --json watcher create harness-watch | json_get id)"
[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher start harness-watch | json_get id)" == "$watcher_id" ]] || fail "CLI watcher start by name failed"
HOME="$CLI_HOME" "$BIN" --json watcher stop "$watcher_id" | grep -q '"status": "stopped"' || fail "CLI watcher stop failed"
HOME="$CLI_HOME" "$BIN" watcher remove "$watcher_id" || fail "CLI watcher remove failed"
if HOME="$CLI_HOME" "$BIN" --json watcher list | json_has_id "$watcher_id"; then
	fail "CLI removed watcher is still visible"
fi
[[ "$(HOME="$CLI_HOME" "$BIN" --json watcher test harness-dry-run | json_get status)" == "stopped" ]] || fail "CLI watcher test did not return stopped validation"
	if HOME="$CLI_HOME" "$BIN" --json watcher list | grep -q "harness-dry-run"; then
		fail "CLI watcher test mutated watcher state"
	fi
conductor_id="$(HOME="$CLI_HOME" "$BIN" --json conductor setup "$session_id" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json conductor start "$conductor_id" | grep -q '"status": "running"' || fail "CLI conductor start failed"
HOME="$CLI_HOME" "$BIN" --json conductor status "$conductor_id" | grep -q '"status": "running"' || fail "CLI conductor status failed"
HOME="$CLI_HOME" "$BIN" --json conductor heartbeat "$conductor_id" | grep -q '"status": "running"' || fail "CLI conductor heartbeat failed"
cli_assignment_json="$(HOME="$CLI_HOME" "$BIN" --json conductor send "$conductor_id" "$session_id" harness-task)"
printf '%s' "$cli_assignment_json" | grep -q '"status": "assigned"' || fail "CLI conductor send failed"
cli_assignment_id="$(printf '%s' "$cli_assignment_json" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json conductor assignments "$conductor_id" | grep -q "harness-task" || fail "CLI conductor assignments missing task"
HOME="$CLI_HOME" "$BIN" --json conductor complete "$cli_assignment_id" | grep -q '"status": "completed"' || fail "CLI conductor assignment complete failed"
cli_failed_assignment_json="$(HOME="$CLI_HOME" "$BIN" --json conductor send "$conductor_id" "$session_id" harness-fail)"
cli_failed_assignment_id="$(printf '%s' "$cli_failed_assignment_json" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json conductor fail "$cli_failed_assignment_id" | grep -q '"status": "failed"' || fail "CLI conductor assignment fail failed"
cli_cancel_assignment_json="$(HOME="$CLI_HOME" "$BIN" --json conductor send "$conductor_id" "$session_id" harness-cancel)"
cli_cancel_assignment_id="$(printf '%s' "$cli_cancel_assignment_json" | json_get id)"
HOME="$CLI_HOME" "$BIN" --json conductor cancel "$cli_cancel_assignment_id" | grep -q '"status": "cancelled"' || fail "CLI conductor assignment cancel failed"
wait_cli_output "$CLI_HOME" "$session_id" "harness-task"
HOME="$CLI_HOME" "$BIN" --json conductor stop "$conductor_id" | grep -q '"status": "stopped"' || fail "CLI conductor stop failed"
HOME="$CLI_HOME" "$BIN" conductor remove "$conductor_id" || fail "CLI conductor remove failed"
if HOME="$CLI_HOME" "$BIN" --json conductor list | json_has_id "$conductor_id"; then
	fail "CLI removed conductor is still visible"
fi
HOME="$CLI_HOME" "$BIN" --json costs >/dev/null
HOME="$CLI_HOME" "$BIN" session remove "$session_id" >/dev/null
if HOME="$CLI_HOME" "$BIN" --json list | json_has_id "$session_id"; then
  fail "removed CLI session is still visible"
fi
archived_sessions="$(HOME="$CLI_HOME" "$BIN" --json list --archived)"
if ! printf '%s' "$archived_sessions" | json_has_id "$session_id"; then
  fail "removed CLI session was not archived"
fi
printf '%s' "$archived_sessions" | python3 -c 'import json,sys; needle=sys.argv[1]; sessions=json.load(sys.stdin); match=next((s for s in sessions if s.get("id") == needle), None); sys.exit(0 if match and match.get("status") == "stopped" and match.get("runtime_id") is None else 1)' "$session_id" || fail "removed CLI session retained stale runtime state"

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
missing_session_body="$TMP_ROOT/missing-session.json"
missing_session_status="$(curl -sS -o "$missing_session_body" -w "%{http_code}" \
	-H "x-agent-helm-token: $TOKEN" \
	"$API_URL/api/sessions/missing-session")"
[[ "$missing_session_status" == "404" ]] || fail "API missing session did not return 404"
grep -q '"code":"not_found"' "$missing_session_body" || fail "API missing session returned wrong error code"
missing_worktrees_body="$TMP_ROOT/missing-worktrees.json"
missing_worktrees_status="$(curl -sS -o "$missing_worktrees_body" -w "%{http_code}" \
	-H "x-agent-helm-token: $TOKEN" \
	"$API_URL/api/projects/missing-project/worktrees")"
[[ "$missing_worktrees_status" == "404" ]] || fail "API missing project worktrees did not return 404"
grep -q '"code":"not_found"' "$missing_worktrees_body" || fail "API missing project worktrees returned wrong error code"
curl -fsS "$API_URL/" >/dev/null || fail "web dashboard required token before token form could load"
dashboard_route_html="$(curl -fsS "$API_URL/s/browser-route-smoke")"
grep -q "<title>Agent Helm</title>" <<< "$dashboard_route_html" || fail "web dashboard session route did not render"
api_group_body="$(python3 -c 'import json,sys; print(json.dumps({"name":"api","parent":"harness","default_project_path":sys.argv[1]}))' "$API_WORKSPACE")"
api_group="$(api_post "/api/groups" "$api_group_body")"
[[ "$(printf '%s' "$api_group" | json_get name)" == "harness/api" ]] || fail "API group create returned wrong group"
api_patch "/api/groups/harness/api" '{"collapsed":true}' | grep -q '"collapsed":true' || fail "API group update failed"
api_delete "/api/groups/harness/api" >/dev/null || fail "API group delete failed"
dashboard_html="$(curl -fsS "$API_URL/")"
dashboard_has "<title>Agent Helm</title>" || fail "web dashboard did not render"
dashboard_has 'id="new-session"' || fail "web dashboard missing create form"
dashboard_has ">New Session<" || fail "web dashboard missing create button"
dashboard_has 'id="new-agent" name="agent"' || fail "web dashboard missing new-session agent selector"
dashboard_has 'id="agent-profile-summary"' || fail "web dashboard missing agent profile summary"
dashboard_has 'id="new-worktree" name="use_worktree"' || fail "web dashboard missing new-session worktree checkbox"
dashboard_has 'name="carry_state"' || fail "web dashboard missing new-session carry-state field"
dashboard_has 'renderAgentSelector(about)' || fail "web dashboard did not load agent selector from about API"
dashboard_has 'pathInput.value = about.default_path' || fail "web dashboard did not seed default path from about API"
dashboard_has 'nameInput.dataset.defaultName = about.default_session_name' || fail "web dashboard did not seed generated default session name"
dashboard_has 'data.name = data.name.trim() || nameInput.dataset.defaultName' || fail "web dashboard did not submit generated default session name"
dashboard_has 'renderAgentProfileSummary()' || fail "web dashboard did not render selected agent profile summary"
dashboard_has 'generatedWorktreeBranch(data.agent, data.name, data.path)' || fail "web dashboard did not generate worktree branch from session settings"
if dashboard_has 'name="command" placeholder="Command"'; then fail "web dashboard still exposes new-session command field"; fi
if dashboard_has 'name="sandbox"'; then fail "web dashboard still exposes new-session sandbox field"; fi
if dashboard_has 'name="prompt" placeholder="Initial prompt"'; then fail "web dashboard still exposes new-session prompt field"; fi
api_get /api/about | grep -q '"default_agent":' || fail "API about missing default agent"
api_get /api/about | grep -q '"default_path":' || fail "API about missing default path"
api_get /api/about | grep -q '"default_session_name":' || fail "API about missing default session name"
api_get /api/about | grep -q '"agents":' || fail "API about missing agent choices"
api_get /api/about | grep -q '"review-bot"' || fail "API about missing configured custom tool profile"
api_get /api/about | grep -q '"executable":' || fail "API about missing tool executable"
api_get /api/about | grep -q '"flags":' || fail "API about missing tool flags"
dashboard_has 'id="project-add-form"' || fail "web dashboard missing project add form"
dashboard_has 'name="trusted"' || fail "web dashboard missing project trusted field"
dashboard_has 'id="project-select-form"' || fail "web dashboard missing project select form"
dashboard_has 'id="project-options"' || fail "web dashboard missing project picker options"
dashboard_has 'id="search-form"' || fail "web dashboard missing search form"
dashboard_has 'id="send-form"' || fail "web dashboard missing send form"
dashboard_has '/api/sessions/${selected}/send' || fail "web dashboard send did not use send API"
dashboard_has '#send-form input' || fail "web dashboard read-only mode did not disable send input"
dashboard_has 'id="session-event-form"' || fail "web dashboard missing session event form"
dashboard_has '/api/sessions/${selected}/events' || fail "web dashboard did not post session events"
dashboard_has '#session-event-form input' || fail "web dashboard read-only mode did not disable session event inputs"
dashboard_has 'id="sync-state"' || fail "web dashboard missing sync-state action"
dashboard_has '/api/sessions/${sessionId}/${id}' || fail "web dashboard did not post selected-session actions"
dashboard_has '#sync-state' || fail "web dashboard read-only mode did not disable sync-state action"
dashboard_has 'id="group-filter-form"' || fail "web dashboard missing group filter form"
dashboard_has 'id="status-filter-form"' || fail "web dashboard missing status filter form"
dashboard_has 'id="status-filter" name="status"' || fail "web dashboard missing status filter selector"
dashboard_has 'value="queued">Queued</option>' || fail "web dashboard missing queued status filter"
dashboard_has 'value="waiting">Waiting</option>' || fail "web dashboard missing waiting status filter"
dashboard_has 'value="idle">Idle</option>' || fail "web dashboard missing idle status filter"
dashboard_has 'params.set("deck_status", status)' || fail "web dashboard did not send deck status filter"
dashboard_has 'params.set("archived", "true")' || fail "web dashboard did not send archived filter"
dashboard_has 'loadSessionSnapshots(body.sessions)' || fail "web dashboard did not load row status snapshots"
dashboard_has '/api/sessions/${session.id}/status-snapshot' || fail "web dashboard did not use status snapshot rows"
dashboard_has 'deckStatus' || fail "web dashboard did not render deck status rows"
dashboard_has 'id="fleet-summary"' || fail "web dashboard missing fleet summary"
dashboard_has 'countSessionsByDeckStatus' || fail "web dashboard missing fleet status counter"
dashboard_has 'renderFleetSummary(body.sessions, snapshots)' || fail "web dashboard did not render fleet summary from snapshots"
dashboard_has 'id="session-context"' || fail "web dashboard missing selected-session context"
dashboard_has 'sessionContextLabel(session)' || fail "web dashboard missing selected-session context label"
dashboard_has 'renderGroupedSessionList(list, body.sessions, snapshots, groups)' || fail "web dashboard did not render grouped session list"
dashboard_has 'className = "group-row"' || fail "web dashboard missing group tree rows"
dashboard_has 'className = "session-row"' || fail "web dashboard missing grouped session rows"
dashboard_has 'await api("/api/groups")' || fail "web dashboard did not load group records"
dashboard_has 'return renderedSessionIds' || fail "web dashboard grouped list did not report rendered sessions"
dashboard_has '!renderedSessionIds.has(selected)' || fail "web dashboard did not clear collapsed-hidden selection"
dashboard_has '/api/groups/${node.name}' || fail "web dashboard group rows did not toggle collapsed state"
dashboard_has 'className = "pr-badge"' || fail "web dashboard missing session PR badge"
dashboard_has 'sessionPrLabel(rowSession)' || fail "web dashboard did not render PR labels in session rows"
dashboard_has 'window.history.pushState' || fail "web dashboard did not push session routes"
dashboard_has 'routeSessionId' || fail "web dashboard missing session route parser"
dashboard_has 'popstate' || fail "web dashboard missing history popstate handling"
dashboard_has 'if (!selected && routeSessionId()) await loadRouteSelection()' || fail "web dashboard refresh did not retry session route selection"
dashboard_has 'id="command-palette"' || fail "web dashboard missing command palette"
dashboard_has 'commandPaletteRows' || fail "web dashboard missing command palette model"
dashboard_has 'openCommandPalette()' || fail "web dashboard missing command palette opener"
dashboard_has 'event.key.toLowerCase() === "k"' || fail "web dashboard missing command palette hotkey"
dashboard_has 'id="shortcut-help"' || fail "web dashboard missing shortcut help overlay"
dashboard_has 'openShortcutHelp()' || fail "web dashboard missing shortcut help opener"
dashboard_has 'event.key === "?"' || fail "web dashboard missing shortcut help hotkey"
dashboard_has 'id="confirm-dialog"' || fail "web dashboard missing confirm dialog"
dashboard_has 'confirmAction(' || fail "web dashboard missing confirm action helper"
dashboard_has 'Remove session ${sessionId}?' || fail "web dashboard remove action did not confirm"
dashboard_has 'Archive session ${sessionId}?' || fail "web dashboard archive action did not confirm"
dashboard_has 'Remove watcher ${watcherId}?' || fail "web dashboard watcher remove action did not confirm"
dashboard_has 'Remove conductor ${conductorId}?' || fail "web dashboard conductor remove action did not confirm"
dashboard_has 'visibleSessionIdByOffset' || fail "web dashboard missing keyboard session navigation model"
dashboard_has 'selectVisibleSessionByOffset(1)' || fail "web dashboard missing next-session hotkey"
dashboard_has 'selectVisibleSessionByOffset(-1)' || fail "web dashboard missing previous-session hotkey"
dashboard_has 'openSelectedSessionInNewTab' || fail "web dashboard missing shift-enter session route opener"
dashboard_has 'toggleArchivedFilter()' || fail "web dashboard missing archived keyboard filter"
dashboard_has 'cycleStatusFilter()' || fail "web dashboard missing status keyboard filter"
dashboard_has 'event.key.toLowerCase() === "a"' || fail "web dashboard missing archived keyboard shortcut"
dashboard_has 'event.key.toLowerCase() === "t"' || fail "web dashboard missing status keyboard shortcut"
dashboard_has 'Toggle archived sessions' || fail "web dashboard shortcut help missing archived shortcut"
dashboard_has 'Cycle status filter' || fail "web dashboard shortcut help missing status shortcut"
dashboard_has 'refreshStatusSurfaces' || fail "web dashboard did not auto-refresh status surfaces"
dashboard_has 'setInterval(refreshStatusSurfaces, 3000)' || fail "web dashboard did not poll status snapshots"
dashboard_has 'statusRefreshRunning' || fail "web dashboard status polling lacked concurrency guard"
			dashboard_has 'id="group-create-form"' || fail "web dashboard missing group create form"
dashboard_has 'name="parent"' || fail "web dashboard missing group parent input"
dashboard_has 'name="default_project_path"' || fail "web dashboard missing group default path input"
dashboard_has 'id="group-update-form"' || fail "web dashboard missing group update form"
dashboard_has 'name="clear_default_project_path"' || fail "web dashboard missing group clear default path input"
dashboard_has 'name="collapsed"' || fail "web dashboard missing group collapsed selector"
dashboard_has 'id="group-delete-form"' || fail "web dashboard missing group delete form"
dashboard_has 'name="force"' || fail "web dashboard missing group force delete control"
dashboard_has 'id="move-group-form"' || fail "web dashboard missing move group form"
dashboard_has 'id="fork-form"' || fail "web dashboard missing fork form"
dashboard_has 'name="worktree_branch"' || fail "web dashboard missing fork worktree field"
dashboard_has 'name="no_start"' || fail "web dashboard missing fork no-start control"
dashboard_has 'id="project-form"' || fail "web dashboard missing project form"
dashboard_has 'id="project-remove"' || fail "web dashboard missing project remove control"
dashboard_has 'id="worktree-create-form"' || fail "web dashboard missing worktree create form"
dashboard_has 'id="worktree-finish-form"' || fail "web dashboard missing worktree finish form"
dashboard_has 'Finish worktree ${worktreeId}?' || fail "web dashboard worktree finish action did not confirm"
dashboard_has 'id="mcp-form"' || fail "web dashboard missing MCP form"
dashboard_has 'id="skill-form"' || fail "web dashboard missing skill form"
dashboard_has 'value="project">Project</option>' || fail "web dashboard missing project attachment scope"
dashboard_has 'value="profile">Profile</option>' || fail "web dashboard missing profile attachment scope"
dashboard_has 'value="sync" type="submit">Sync MCP' || fail "web dashboard missing MCP sync control"
dashboard_has 'value="detach" type="submit">Detach Skill' || fail "web dashboard missing skill detach control"
dashboard_has 'id="watcher-form"' || fail "web dashboard missing watcher form"
dashboard_has 'id="watcher-create-form"' || fail "web dashboard missing watcher create form"
dashboard_has 'value="manual">Manual</option>' || fail "web dashboard missing manual watcher adapter option"
dashboard_has 'name="timeout_ms"' || fail "web dashboard missing watcher timeout input"
dashboard_has 'name="require_signature"' || fail "web dashboard missing watcher signature requirement control"
dashboard_has 'id="watcher-events-form"' || fail "web dashboard missing watcher events form"
dashboard_has 'name="offset" placeholder="Offset"' || fail "web dashboard missing watcher events offset input"
dashboard_has 'name="limit" placeholder="Limit"' || fail "web dashboard missing watcher events limit input"
dashboard_has 'new URLSearchParams()' || fail "web dashboard watcher events did not build query params"
dashboard_has '/api/watchers/${id}/events${suffix}' || fail "web dashboard watcher events did not send pagination query"
dashboard_has 'id="watcher-ingest-form"' || fail "web dashboard missing watcher ingest form"
dashboard_has 'name="signature_status"' || fail "web dashboard missing watcher ingest signature status control"
dashboard_has '#watcher-ingest-form select' || fail "web dashboard read-only mode did not disable watcher ingest selects"
dashboard_has 'id="watcher-poll"' || fail "web dashboard missing watcher poll control"
dashboard_has 'id="watcher-poll-all-form"' || fail "web dashboard missing watcher poll-all control"
dashboard_has 'id="watcher-test"' || fail "web dashboard missing watcher test control"
dashboard_has 'id="watcher-stop"' || fail "web dashboard missing watcher stop control"
dashboard_has 'id="watcher-remove"' || fail "web dashboard missing watcher remove control"
dashboard_has 'id="conductor-form"' || fail "web dashboard missing conductor form"
dashboard_has 'id="conductor-action-form"' || fail "web dashboard missing conductor action form"
dashboard_has 'id="conductor-status"' || fail "web dashboard missing conductor status control"
dashboard_has 'id="conductor-start"' || fail "web dashboard missing conductor start control"
dashboard_has 'id="conductor-heartbeat"' || fail "web dashboard missing conductor heartbeat control"
dashboard_has 'id="conductor-send"' || fail "web dashboard missing conductor send control"
dashboard_has 'id="conductor-stop"' || fail "web dashboard missing conductor stop control"
dashboard_has 'id="conductor-remove"' || fail "web dashboard missing conductor remove control"
dashboard_has 'id="conductor-assignments-form"' || fail "web dashboard missing conductor assignments form"
dashboard_has 'id="conductor-assignment-status-form"' || fail "web dashboard missing conductor assignment status form"
dashboard_has '/api/conductor-assignments/${id}' || fail "web dashboard missing conductor assignment status patch"
dashboard_has '#conductor-assignment-status-form input' || fail "web dashboard read-only mode did not disable conductor assignment input"
dashboard_has '#conductor-assignment-status-form button' || fail "web dashboard read-only mode did not disable conductor assignment buttons"
dashboard_has 'id="show-archived"' || fail "web dashboard missing archived filter"
dashboard_has 'id="archive"' || fail "web dashboard missing archive action"
dashboard_has 'id="archive-reason"' || fail "web dashboard missing archive reason input"
dashboard_has 'id="archive-by"' || fail "web dashboard missing archived-by input"
dashboard_has 'body.reason = reason' || fail "web dashboard archive did not send reason"
dashboard_has 'body.archived_by = archivedBy' || fail "web dashboard archive did not send archived_by"
dashboard_has '#archive-reason' || fail "web dashboard read-only mode did not disable archive reason"
dashboard_has 'id="restore"' || fail "web dashboard missing restore action"
dashboard_has 'id="remove-session"' || fail "web dashboard missing remove session action"
dashboard_has 'data-tab="costs"' || fail "web dashboard missing costs tab"
dashboard_has 'function startOutputStream(sessionId)' || fail "web dashboard missing output stream startup"
dashboard_has '/api/sessions/${sessionId}/terminal-stream?limit=400' || fail "web dashboard did not use terminal stream"
dashboard_has 'response.body.getReader()' || fail "web dashboard terminal stream did not read response body"
dashboard_has 'const event = parseSseBlock(block);' || fail "web dashboard terminal stream did not parse SSE blocks"
dashboard_has 'event.name === "terminal"' || fail "web dashboard terminal stream did not handle terminal events"
dashboard_has '$("panel").textContent = page.text || "";' || fail "web dashboard terminal stream did not render terminal event text"
dashboard_has 'loadOutputSnapshot(sessionId)' || fail "web dashboard terminal stream missing output fallback"
dashboard_has 'id="cost-filter-form"' || fail "web dashboard missing cost filter form"
dashboard_has 'name="project_id"' || fail "web dashboard missing cost project filter"
dashboard_has 'name="group_name"' || fail "web dashboard missing cost group filter"
dashboard_has 'name="agent"' || fail "web dashboard missing cost agent filter"
dashboard_has 'name="model"' || fail "web dashboard missing cost model filter"
dashboard_has 'name="start_at"' || fail "web dashboard missing cost start filter"
dashboard_has 'name="end_at"' || fail "web dashboard missing cost end filter"
dashboard_has 'params.set("include_archived", "true")' || fail "web dashboard costs did not send include archived"
dashboard_has 'const hasExplicitCostFilter = costFilterKeys.some((key) => params.has(key))' || fail "web dashboard costs did not respect explicit cost filters"
dashboard_has '/api/cost-events?${params.toString()}' || fail "web dashboard costs did not load cost events"
dashboard_has '>Apply Costs<' || fail "web dashboard missing cost apply button"
dashboard_has '$("cost-filter-form").onsubmit' || fail "web dashboard cost filter form did not submit"
dashboard_has 'id="cost-record-form"' || fail "web dashboard missing cost record form"
dashboard_has 'name="amount_usd"' || fail "web dashboard missing cost amount input"
dashboard_has '/api/sessions/${selected}/costs' || fail "web dashboard cost record did not use session cost API"
dashboard_has '"input_tokens", "output_tokens", "total_tokens"' || fail "web dashboard cost record did not serialize token counts"
dashboard_has '#cost-record-form input' || fail "web dashboard read-only mode did not disable cost record inputs"
dashboard_has 'data-tab="attachments"' || fail "web dashboard missing attachments tab"
dashboard_has '/api/sessions/${selected}/materialization' || fail "web dashboard missing selected-session materialization load"
dashboard_has 'materialization' || fail "web dashboard attachments did not render materialization summary"
dashboard_has 'data-tab="worktrees"' || fail "web dashboard missing worktrees tab"
dashboard_has 'data-tab="managers"' || fail "web dashboard missing managers tab"
dashboard_has 'selectedProject = project.id' || fail "web dashboard project add did not set project context"
dashboard_has 'trusted: form.has("trusted")' || fail "web dashboard project add did not send trusted"
dashboard_has 'body.default_branch = defaultBranch' || fail "web dashboard project add did not send default branch"
dashboard_has 'const projects = await api("/api/projects")' || fail "web dashboard project picker did not list projects"
dashboard_has 'selectedProject = new FormData(event.target).get("project").trim()' || fail "web dashboard project select did not set project context"
dashboard_has '/api/projects/${context.projectId}/workspaces' || fail "web dashboard workspaces did not use project context"
dashboard_has '/api/workspaces/${context.session.workspace_id}' || fail "web dashboard did not load selected workspace details"
dashboard_has 'data.carry_state = form.has("carry_state")' || fail "web dashboard new-session carry-state was not boolean"
dashboard_has 'data.sandbox = false' || fail "web dashboard new-session sandbox was not explicit false"
dashboard_has '/api/projects/${context.projectId}/worktrees' || fail "web dashboard worktrees did not use project context"
dashboard_has '/api/worktrees/${context.session.worktree_id}' || fail "web dashboard worktrees did not load selected worktree details"
dashboard_has 'worktree-create-form" style="grid-template-columns: 1fr auto auto' || fail "web dashboard worktree form did not expose carry-state column"
dashboard_has 'carry_state: carryState' || fail "web dashboard worktree create did not send carry-state"
dashboard_has '/api/projects/${context.projectId}/mcp' || fail "web dashboard MCP attach did not use project context"
dashboard_has '/api/profile/skills' || fail "web dashboard skill attach did not support profile scope"
dashboard_has '/api/mcp/${id}/detach' || fail "web dashboard MCP detach did not use detach API"
dashboard_has '/api/skills/sync' || fail "web dashboard skill sync did not use sync API"
dashboard_has '#watcher-ingest-form input' || fail "web dashboard read-only mode did not disable watcher ingest"
if dashboard_has '#watcher-events-form input'; then
		fail "web dashboard read-only mode disabled watcher event reads"
fi
dashboard_has "#conductor-action-form input\\[name='task'\\]" || fail "web dashboard read-only mode did not disable conductor task input"
dashboard_has '#conductor-start' || fail "web dashboard read-only mode did not disable conductor start"
dashboard_has '#conductor-remove' || fail "web dashboard read-only mode did not disable conductor remove"
if dashboard_has '#conductor-status'; then
		fail "web dashboard read-only mode disabled conductor status"
fi
dashboard_has '#watcher-create-form select' || fail "web dashboard read-only mode did not disable watcher adapter selector"
dashboard_has 'const watcherPath = context.projectId' || fail "web dashboard managers did not use selected project context"
dashboard_has 'body.project_id = context.projectId' || fail "web dashboard watcher create did not use project context"
dashboard_has 'adapter_id: adapter' || fail "web dashboard watcher create did not use selected adapter"
dashboard_has 'if (adapter === "shell")' || fail "web dashboard watcher create did not keep shell validation"
dashboard_has '/api/watchers/${id}/events' || fail "web dashboard watcher ingest did not use event API"
dashboard_has '/api/sessions/${sessionId}/events?limit=200' || fail "web dashboard events tab did not load raw events"
dashboard_has '/api/sessions/${selected}/status-snapshot' || fail "web dashboard refresh did not load selected session status snapshot"
dashboard_has 'const visibleSessionIds = new Set' || fail "web dashboard did not track visible sessions"
dashboard_has 'if (selected && !visibleSessionIds.has(selected))' || fail "web dashboard did not clear hidden selected session"
dashboard_has 'id="deck-status"' || fail "web dashboard missing deck status display"
dashboard_has 'action === "status"' || fail "web dashboard conductor status did not use read API"
dashboard_has 'body.parent = parent' || fail "web dashboard group create did not send parent"
dashboard_has 'body.default_project_path = defaultProjectPath' || fail "web dashboard group create did not send default project path"
dashboard_has 'body.clear_default_project_path = true' || fail "web dashboard group update did not send clear default path"
dashboard_has 'body.collapsed = collapsed === "true"' || fail "web dashboard group update did not send collapsed"
dashboard_has '/api/groups/${name}' || fail "web dashboard group update did not use group API"
dashboard_has '?force=true' || fail "web dashboard group delete did not send force"
dashboard_has 'start_immediately: !data.has("no_start")' || fail "web dashboard fork did not send start option"
dashboard_has '/api/sessions/${selected}/fork' || fail "web dashboard fork did not use fork API"
dashboard_has 'if (id === "fork") selected = result.child_session_id' || fail "web dashboard quick fork did not select child"
dashboard_has 'if (selected) await loadSelectedSnapshot(selected)' || fail "web dashboard quick actions did not refresh selected status"
dashboard_has '$("remove-session").onclick' || fail "web dashboard remove session did not wire action"
dashboard_has 'if (!session) $("panel").textContent = "";' || fail "web dashboard did not clear panel on empty selection"
dashboard_has 'id="remove-cleanup-worktree"' || fail "web dashboard remove cleanup checkbox missing"
dashboard_has 'id="remove-purge"' || fail "web dashboard remove purge checkbox missing"
dashboard_has 'cleanup_worktree=true' || fail "web dashboard remove did not pass cleanup flag"
dashboard_has 'purge=true' || fail "web dashboard remove did not pass purge flag"
dashboard_has '#remove-cleanup-worktree' || fail "web dashboard read-only mode did not disable cleanup checkbox"
dashboard_has '#remove-purge' || fail "web dashboard read-only mode did not disable purge checkbox"
dashboard_has 'startStructuredStream(sessionId)' || fail "web dashboard missing structured stream starter"
dashboard_has '/structured-stream?limit=200' || fail "web dashboard structured stream did not use streaming endpoint"
dashboard_has 'event.name === "structured"' || fail "web dashboard structured stream did not parse structured SSE events"
dashboard_has 'function eventSummary(event)' || fail "web dashboard structured events missing readable summary renderer"
dashboard_has 'structured: ${eventSummary(event)}' || fail "web dashboard structured events did not render structured rows"
dashboard_has 'raw: ${eventSummary(event)}' || fail "web dashboard structured events did not render raw rows"
dashboard_has 'loadStructuredSnapshot(sessionId).catch' || fail "web dashboard structured stream missing snapshot fallback"
dashboard_has '/structured-events?limit=200' || fail "web dashboard structured fallback did not use limited snapshot"
dashboard_has 'await loadSelectedSnapshot(selected)' || fail "web dashboard search result did not hydrate selected status"
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
	DASHBOARD_JS="$dashboard_js" node <<'NODE' || fail "web dashboard stream runtime smoke failed"
const fs = require("fs");
const vm = require("vm");

const source = fs.readFileSync(process.env.DASHBOARD_JS, "utf8");
const elements = new Map();
class FakeFormData {
	constructor(form) {
		this.values = form.values || {};
	}
	get(name) {
		return this.values[name] || "";
	}
	has(name) {
		return Object.prototype.hasOwnProperty.call(this.values, name);
	}
}
function element(id) {
	if (!elements.has(id)) {
		elements.set(id, {
			id,
			value: "",
			textContent: "",
			innerHTML: "",
			checked: false,
			disabled: false,
			style: {},
			dataset: {},
			appendChild() {},
			setAttribute() {},
			reset() {},
			focus() {},
		});
	}
	return elements.get(id);
}
element("token").value = "dashboard-smoke-token";
const terminalStreamPath = "/api/sessions/browser-stream-smoke-session/terminal-stream?limit=400";
const structuredRawEventsPath = "/api/sessions/browser-stream-smoke-session/events?limit=200";
const structuredStreamPath = "/api/sessions/browser-stream-smoke-session/structured-stream?limit=200";
const structuredFallbackRawEventsPath = "/api/sessions/browser-fallback-smoke-session/events?limit=200";
const structuredSnapshotPath = "/api/sessions/browser-fallback-smoke-session/structured-events?limit=200";
const requests = [];
const encoder = new TextEncoder();
const historyPaths = [];
const windowLocation = {pathname: "/"};
const windowHistory = {
	pushState(_state, _title, path) {
		historyPaths.push(path);
		windowLocation.pathname = path;
	},
};
const context = {
	console,
	AbortController,
	ReadableStream,
  TextDecoder,
  FormData: FakeFormData,
	setTimeout,
	clearTimeout,
	document: {
		getElementById: element,
		createElement: (tag) => element(`created-${tag}-${elements.size}`),
		querySelectorAll: (selector) => selector === ".tabs button" ? [{dataset: {tab: "output"}, setAttribute() {}}] : [],
	},
	globalThis: null,
	window: {location: windowLocation, history: windowHistory, addEventListener() {}},
	fetch: async (path) => {
		requests.push(String(path));
		if (String(path) === terminalStreamPath) {
			return {
				ok: true,
				body: new ReadableStream({
					start(controller) {
						controller.enqueue(encoder.encode('event: terminal\ndata: {"text":"browser-stream-smoke"}\n\n'));
						controller.close();
					},
				}),
			};
		}
		if (String(path) === structuredStreamPath) {
			return {
				ok: true,
				body: new ReadableStream({
					start(controller) {
						controller.enqueue(encoder.encode('event: structured\ndata: [{"id":1,"kind":"input","payload":{"text":"structured-stream-smoke"}}]\n\n'));
						controller.close();
					},
				}),
			};
		}
		return {
			ok: true,
			status: 200,
			text: async () => "",
			json: async () => {
    if (String(path) === "/api/about") return {read_only: false, agents: [], default_agent: "shell", default_path: "/tmp/agent-helm-smoke", default_session_name: "web-smoke"};
    if (String(path) === "/api/projects") return [];
    if (String(path) === "/api/groups") return [];
    if (String(path) === "/api/watchers") return [];
    if (String(path) === "/api/conductors") return [];
				if (String(path) === structuredRawEventsPath) return [{id: 10, kind: "input", payload: {text: "raw-stream-smoke"}}];
				if (String(path) === structuredFallbackRawEventsPath) return [{id: 11, kind: "input", payload: {text: "raw-fallback-smoke"}}];
				if (String(path) === structuredSnapshotPath) return {events: [{id: 2, kind: "input", payload: {text: "structured-fallback-smoke"}}]};
          if (String(path).startsWith("/api/costs?")) return {event_count: 0};
          if (String(path).startsWith("/api/cost-events?")) return [];
				if (String(path).startsWith("/api/sessions")) return {sessions: []};
				return {};
			},
		};
	},
};
context.globalThis = context;
vm.createContext(context);
vm.runInContext(source, context, {filename: "dashboard.js"});
const prChecks = vm.runInContext(`[
sessionPrLabel({name: "fix #12345", group_name: "default", project_path: "/tmp/project"}),
sessionPrLabel({name: "release-12345", group_name: "default", project_path: "/tmp/project"}),
sessionPrLabel({name: "agent", group_name: "feature/pr-678", project_path: "/tmp/project"}),
sessionPrLabel({name: "agent", group_name: "default", project_path: "https://github.test/repo/pull/42"})
]`, context);
if (prChecks[0] !== "#12345" || prChecks[1] !== "" || prChecks[2] !== "#678" || prChecks[3] !== "#42") {
  throw new Error(`PR label checks failed: ${JSON.stringify(prChecks)}`);
}
const fleetChecks = vm.runInContext(`(() => {
const sessions = [
  {id: "fleet-1", status: "running"},
  {id: "fleet-2", status: "running"},
  {id: "fleet-3", status: "stopped"},
];
const snapshots = new Map([
  ["fleet-2", {deck_status: "waiting"}],
  ["fleet-3", {deck_status: "idle"}],
]);
renderFleetSummary(sessions, snapshots);
return {text: $("fleet-summary").textContent, counts: countSessionsByDeckStatus(sessions, snapshots)};
})()`, context);
if (fleetChecks.counts.running !== 1 || fleetChecks.counts.waiting !== 1 || fleetChecks.counts.idle !== 1 || !fleetChecks.text.includes("running 1") || !fleetChecks.text.includes("waiting 1") || !fleetChecks.text.includes("idle 1")) {
  throw new Error(`fleet summary checks failed: ${JSON.stringify(fleetChecks)}`);
}
const contextChecks = vm.runInContext(`(() => {
const session = {
  id: "context-session",
  name: "Context Session",
  agent: "codex",
  group_name: "feature/api",
  project_path: "/tmp/agent-helm-context",
  workspace_id: "workspace-abcdef123456",
  worktree_id: "worktree-1234567890",
  archived: true,
  status: "running",
};
renderSelected(session, {deck_status: "waiting", activity: {label: "Bash"}});
const text = $("session-context").textContent;
const title = $("title").textContent;
const status = $("deck-status").textContent;
renderSelected(null);
return {text, title, status, cleared: $("session-context").textContent};
})()`, context);
if (!contextChecks.text.includes("codex") || !contextChecks.text.includes("feature/api") || !contextChecks.text.includes("/tmp/agent-helm-context") || !contextChecks.text.includes("workspace ") || !contextChecks.text.includes("worktree ") || !contextChecks.text.includes("archived") || contextChecks.title !== "Context Session" || contextChecks.status !== "waiting · Bash" || contextChecks.cleared !== "") {
  throw new Error(`session context checks failed: ${JSON.stringify(contextChecks)}`);
}
const routeChecks = vm.runInContext(`(() => {
window.location.pathname = "/s/browser-route-smoke";
const parsed = routeSessionId();
window.location.pathname = "/";
selectSession({id: "browser-push-smoke", project_id: "project-1", name: "Push", group_name: "default", project_path: "/tmp", agent: "shell", status: "running"});
const pushed = window.location.pathname;
clearSelectedSession();
return {parsed, pushed, cleared: window.location.pathname, selected, title: $("title").textContent};
})()`, context);
if (routeChecks.parsed !== "browser-route-smoke" || routeChecks.pushed !== "/s/browser-push-smoke" || routeChecks.cleared !== "/" || routeChecks.selected !== null || routeChecks.title !== "No session selected") {
	throw new Error(`route checks failed: ${JSON.stringify(routeChecks)}`);
}
if (!historyPaths.includes("/s/browser-push-smoke") || !historyPaths.includes("/")) {
	throw new Error(`route history did not record selection and clear: ${historyPaths.join(",")}`);
}
const paletteChecks = vm.runInContext(`(() => {
paletteSessions = [
{id: "palette-session-1", name: "Alpha Work", group_name: "default", agent: "codex", project_id: "project-1", project_path: "/tmp", status: "running"},
{id: "palette-session-2", name: "Beta Shell", group_name: "ops", agent: "shell", project_id: "project-1", project_path: "/tmp", status: "stopped"}
];
const all = commandPaletteRows("").map((row) => row.label);
const filtered = commandPaletteRows("beta").map((row) => row.label);
openCommandPalette();
const opened = !$("command-palette").hidden;
closeCommandPalette();
return {all, filtered, opened, closed: $("command-palette").hidden};
})()`, context);
if (!paletteChecks.all.includes("New session") || !paletteChecks.all.includes("Output") || !paletteChecks.all.includes("Alpha Work") || paletteChecks.filtered.join(",") !== "Beta Shell" || !paletteChecks.opened || !paletteChecks.closed) {
	throw new Error(`palette checks failed: ${JSON.stringify(paletteChecks)}`);
}
const shortcutChecks = vm.runInContext(`(() => {
openShortcutHelp();
const opened = !$("shortcut-help").hidden;
closeShortcutHelp();
return {opened, closed: $("shortcut-help").hidden};
})()`, context);
if (!shortcutChecks.opened || !shortcutChecks.closed) {
	throw new Error(`shortcut checks failed: ${JSON.stringify(shortcutChecks)}`);
}
const confirmChecks = vm.runInContext(`(() => {
let count = 0;
confirmAction("Delete this?", () => { count += 1; });
const opened = !$("confirm-dialog").hidden;
const message = $("confirm-message").textContent;
$("confirm-accept").onclick();
return {opened, message, closed: $("confirm-dialog").hidden, count};
})()`, context);
if (!confirmChecks.opened || confirmChecks.message !== "Delete this?" || !confirmChecks.closed || confirmChecks.count !== 1) {
  throw new Error(`confirm checks failed: ${JSON.stringify(confirmChecks)}`);
}
requests.length = 0;
const filterShortcutChecks = vm.runInContext(`(() => {
$("show-archived").checked = false;
$("status-filter").value = "";
const originalLoadSessions = loadSessions;
let loads = 0;
loadSessions = () => { loads += 1; };
toggleArchivedFilter();
cycleStatusFilter();
loadSessions = originalLoadSessions;
return {archived: $("show-archived").checked, status: $("status-filter").value, loads};
})()`, context);
if (!filterShortcutChecks.archived || filterShortcutChecks.status !== "running" || filterShortcutChecks.loads !== 2) {
  throw new Error(`filter shortcut checks failed: ${JSON.stringify(filterShortcutChecks)}`);
}
requests.length = 0;
const watcherRemoveConfirm = vm.runInContext(`(() => {
selected = null;
selectedProject = null;
tab = "managers";
const form = $("watcher-form");
form.values = {id: "watcher-smoke"};
form.onsubmit({preventDefault() {}, target: form, submitter: {value: "remove"}});
return {opened: !$("confirm-dialog").hidden, message: $("confirm-message").textContent};
})()`, context);
if (!watcherRemoveConfirm.opened || watcherRemoveConfirm.message !== "Remove watcher watcher-smoke?") {
  throw new Error(`watcher remove confirm failed: ${JSON.stringify(watcherRemoveConfirm)}`);
}
if (requests.some((path) => path === "/api/watchers/watcher-smoke")) {
  throw new Error(`watcher remove deleted before confirmation: ${requests.join(",")}`);
}
vm.runInContext('closeConfirmDialog();', context);
requests.length = 0;
const worktreeFinishConfirm = vm.runInContext(`(() => {
selected = null;
selectedProject = "project-smoke";
tab = "worktrees";
const form = $("worktree-finish-form");
form.values = {id: "worktree-smoke"};
form.onsubmit({preventDefault() {}, target: form});
return {opened: !$("confirm-dialog").hidden, message: $("confirm-message").textContent};
})()`, context);
if (!worktreeFinishConfirm.opened || worktreeFinishConfirm.message !== "Finish worktree worktree-smoke?") {
  throw new Error(`worktree finish confirm failed: ${JSON.stringify(worktreeFinishConfirm)}`);
}
if (requests.some((path) => path === "/api/worktrees/worktree-smoke/finish")) {
  throw new Error(`worktree finish ran before confirmation: ${requests.join(",")}`);
}
vm.runInContext('closeConfirmDialog();', context);
requests.length = 0;
const conductorRemoveConfirm = vm.runInContext(`(() => {
selected = null;
selectedProject = null;
tab = "managers";
const form = $("conductor-action-form");
form.values = {id: "conductor-smoke"};
form.onsubmit({preventDefault() {}, target: form, submitter: {value: "remove"}});
return {opened: !$("confirm-dialog").hidden, message: $("confirm-message").textContent};
})()`, context);
if (!conductorRemoveConfirm.opened || conductorRemoveConfirm.message !== "Remove conductor conductor-smoke?") {
  throw new Error(`conductor remove confirm failed: ${JSON.stringify(conductorRemoveConfirm)}`);
}
if (requests.some((path) => path === "/api/conductors/conductor-smoke")) {
  throw new Error(`conductor remove deleted before confirmation: ${requests.join(",")}`);
}
vm.runInContext('closeConfirmDialog();', context);
const navChecks = vm.runInContext(`(() => {
visibleSessionIds = ["nav-a", "nav-b", "nav-c"];
selected = null;
const first = visibleSessionIdByOffset(1);
selected = "nav-a";
const next = visibleSessionIdByOffset(1);
const previous = visibleSessionIdByOffset(-1);
selected = "missing";
const fallback = visibleSessionIdByOffset(-1);
return {first, next, previous, fallback};
})()`, context);
if (navChecks.first !== "nav-a" || navChecks.next !== "nav-b" || navChecks.previous !== "nav-c" || navChecks.fallback !== "nav-c") {
	throw new Error(`keyboard navigation checks failed: ${JSON.stringify(navChecks)}`);
}
vm.runInContext('selected = "browser-stream-smoke-session"; tab = "output"; startOutputStream(selected);', context);
setTimeout(() => {
	if (!requests.includes(terminalStreamPath)) {
		throw new Error(`missing terminal stream request: ${requests.join(",")}`);
	}
	if (element("panel").textContent !== "browser-stream-smoke") {
		throw new Error(`panel did not render stream text: ${element("panel").textContent}`);
	}
	vm.runInContext('selected = "browser-stream-smoke-session"; tab = "events"; startStructuredStream(selected);', context);
	setTimeout(() => {
		if (!requests.includes(structuredStreamPath)) {
			throw new Error(`missing structured stream request: ${requests.join(",")}`);
		}
		if (!requests.includes(structuredRawEventsPath)) {
			throw new Error(`missing structured raw-events request: ${requests.join(",")}`);
		}
			if (!element("panel").textContent.includes("structured-stream-smoke")) {
				throw new Error(`panel did not render structured stream: ${element("panel").textContent}`);
			}
			if (!element("panel").textContent.includes("structured: input")) {
				throw new Error(`panel did not render structured stream row label: ${element("panel").textContent}`);
			}
			if (!element("panel").textContent.includes("raw-stream-smoke")) {
				throw new Error(`panel did not render raw stream events: ${element("panel").textContent}`);
			}
			if (!element("panel").textContent.includes("raw: input")) {
				throw new Error(`panel did not render raw stream row label: ${element("panel").textContent}`);
			}
		vm.runInContext('selected = "browser-fallback-smoke-session"; tab = "events"; globalThis.ReadableStream = null; startStructuredStream(selected);', context);
		setTimeout(() => {
			if (!requests.includes(structuredSnapshotPath)) {
				throw new Error(`missing structured snapshot fallback: ${requests.join(",")}`);
			}
			if (!requests.includes(structuredFallbackRawEventsPath)) {
				throw new Error(`missing structured raw-events fallback: ${requests.join(",")}`);
			}
				if (!element("panel").textContent.includes("structured-fallback-smoke")) {
					throw new Error(`panel did not render structured fallback: ${element("panel").textContent}`);
				}
				if (!element("panel").textContent.includes("structured: input")) {
					throw new Error(`panel did not render structured fallback row label: ${element("panel").textContent}`);
				}
				if (!element("panel").textContent.includes("raw-fallback-smoke")) {
					throw new Error(`panel did not render raw fallback events: ${element("panel").textContent}`);
				}
				if (!element("panel").textContent.includes("raw: input")) {
					throw new Error(`panel did not render raw fallback row label: ${element("panel").textContent}`);
				}
			context.URLSearchParams = URLSearchParams;
			requests.length = 0;
vm.runInContext('selected = "selected-cost-session"; tab = "costs"; $("cost-filter-form").values = {agent: "codex", model: "global-model", start_at: "10", end_at: "20"}; globalThis.costFilterDone = false; globalThis.costFilterError = null; loadPanel().then(() => { globalThis.costFilterDone = true; }).catch((err) => { globalThis.costFilterError = err.message; });', context);
			setTimeout(() => {
				if (context.costFilterError) {
					throw new Error(`cost filter check failed: ${context.costFilterError}`);
				}
				if (!context.costFilterDone) {
					throw new Error("cost filter check did not finish");
				}
          const costRequest = requests.find((path) => path.startsWith("/api/costs?")) || "";
          const costEventsRequest = requests.find((path) => path.startsWith("/api/cost-events?")) || "";
if (!costRequest.includes("agent=codex") || !costRequest.includes("model=global-model") || !costRequest.includes("start_at=10") || !costRequest.includes("end_at=20")) {
throw new Error(`cost filter missed explicit global filters: ${costRequest}`);
}
if (!costEventsRequest.includes("agent=codex") || !costEventsRequest.includes("model=global-model") || !costEventsRequest.includes("start_at=10") || !costEventsRequest.includes("end_at=20")) {
throw new Error(`cost events filter missed explicit global filters: ${costEventsRequest}`);
}
          if (costRequest.includes("session_id=selected-cost-session")) {
            throw new Error(`cost filter incorrectly inherited selected session: ${costRequest}`);
          }
if (costEventsRequest.includes("session_id=selected-cost-session")) {
throw new Error(`cost events filter incorrectly inherited selected session: ${costEventsRequest}`);
}
if (!costRequest.includes("active_only=true")) {
throw new Error(`unchecked archived cost filter did not request active_only: ${costRequest}`);
}
if (!costEventsRequest.includes("active_only=true")) {
throw new Error(`unchecked archived cost events filter did not request active_only: ${costEventsRequest}`);
}
requests.length = 0;
vm.runInContext('selected = "selected-cost-session"; $("status").textContent = ""; $("cost-record-form").values = {amount_usd: "0.01", input_tokens: "not-a-number"}; globalThis.badCostDone = false; globalThis.badCostError = null; $("cost-record-form").onsubmit({preventDefault() {}, target: $("cost-record-form")}).then(() => { globalThis.badCostDone = true; }).catch((err) => { globalThis.badCostError = err.message; });', context);
setTimeout(() => {
if (context.badCostError) {
throw new Error(`invalid cost token check failed: ${context.badCostError}`);
}
if (!context.badCostDone) {
throw new Error("invalid cost token check did not finish");
}
if (requests.some((path) => path.startsWith("/api/sessions/selected-cost-session/costs"))) {
throw new Error(`invalid cost token still posted cost record: ${requests.join(",")}`);
}
if (!element("status").textContent.includes("input tokens must be a non-negative integer")) {
throw new Error(`invalid cost token did not set status: ${element("status").textContent}`);
}
}, 25);
setTimeout(() => {
vm.runInContext('selected = "hidden-session"; $("panel").textContent = "stale-panel"; $("title").textContent = "Hidden"; $("deck-status").textContent = "running"; $("status-filter").value = "stopped"; globalThis.hiddenSelectionResult = null; loadSessions().then(() => { globalThis.hiddenSelectionResult = {selected, title: $("title").textContent, deckStatus: $("deck-status").textContent, panel: $("panel").textContent}; }).catch((err) => { globalThis.hiddenSelectionResult = {error: err.message}; });', context);
					setTimeout(() => {
					const result = context.hiddenSelectionResult;
					if (!result) {
						throw new Error("hidden selection check did not finish");
					}
				if (result.error) {
					throw new Error(`hidden selection check failed: ${result.error}`);
				}
				if (result.selected !== null) {
					throw new Error(`hidden selection was not cleared: ${result.selected}`);
				}
				if (result.title !== "No session selected") {
					throw new Error(`hidden selection title was not cleared: ${result.title}`);
				}
				if (result.deckStatus !== "") {
					throw new Error(`hidden selection deck status was not cleared: ${result.deckStatus}`);
				}
					if (result.panel !== "") {
						throw new Error(`hidden selection panel was not cleared: ${result.panel}`);
					}
					}, 100);
				}, 100);
}, 25);
}, 25);
}, 25);
}, 25);
NODE
	else
		step "Skipping dashboard script syntax check because node is unavailable"
	fi

	mkdir -p "$API_HOME/.claude/projects/harness"
	printf '%s\n' '{"sessionId":"api-transcript-session","message":{"content":"api-transcript-agenthelm-needle"},"cwd":"/tmp/api-transcript"}' \
		>"$API_HOME/.claude/projects/harness/api-transcript-session.jsonl"
	[[ "$(api_get "/api/search?q=api-transcript-agenthelm-needle" | json_get results.0.session_id)" == "api-transcript-session" ]] || fail "API search missed Claude transcript"

create_body="$(python3 -c 'import json,sys
print(json.dumps({
  "path": sys.argv[1],
  "agent": "codex",
  "command": "cat",
    "name": "harness-api",
    "group_name": "harness",
    "prompt": "api-boot",
}))' "$API_WORKSPACE")"
api_session_json="$(api_post /api/sessions "$create_body")"
api_session_id="$(printf '%s' "$api_session_json" | json_get id)"
api_session_path="$(printf '%s' "$api_session_json" | json_get project_path)"
api_workspace_id="$(printf '%s' "$api_session_json" | json_get workspace_id)"
SESSION_IDS+=("$api_session_id")
api_get "/api/sessions?group=harness&status=running" | json_has_id "$api_session_id" || fail "API sessions filters missed session"
wait_api_output "$api_session_id" "api-boot"
tmux kill-session -t "agent-helm-$api_session_id" >/dev/null 2>&1 || fail "API status setup failed"
api_get "/api/sessions?status=stopped" | json_has_id "$api_session_id" || fail "API sessions list did not reconcile runtime status"
api_get "/api/sessions/$api_session_id/status" | grep -q '"lifecycle_status":"stopped"' || fail "API session status did not reconcile runtime"
api_post "/api/sessions/$api_session_id/start" | grep -q '"status":"running"' || fail "API session start after status failed"
api_get "/api/sessions/$api_session_id/status" | grep -q '"deck_status":"running"' || fail "API session status missing running deck status"
api_get "/api/sessions/$api_session_id/status-snapshot" | grep -q '"deck_status":"running"' || fail "API status snapshot missing running deck status"
printf 'api changed\n' >"$api_session_path/README.md"
printf 'api staged\n' >"$api_session_path/api-staged.txt"
git -C "$api_session_path" add api-staged.txt
printf 'api untracked\n' >"$api_session_path/api-untracked.txt"
api_diff_text="$(api_get "/api/sessions/$api_session_id/diff" | json_get text)"
printf '%s' "$api_diff_text" | grep -q '## status' || fail "API diff missing status section"
printf '%s' "$api_diff_text" | grep -q '+api changed' || fail "API diff missing unstaged changes"
printf '%s' "$api_diff_text" | grep -q '+api staged' || fail "API diff missing staged changes"
printf '%s' "$api_diff_text" | grep -q '## untracked: api-untracked.txt' || fail "API diff missing untracked section"
[[ "$(api_get "/api/search?q=api-boot" | json_get results.0.session_id)" == "$api_session_id" ]] || fail "API search missed prompt"
api_fork="$(api_post "/api/sessions/$api_session_id/fork" '{"name":"api-paused-fork","start_immediately":false}')"
api_fork_id="$(printf '%s' "$api_fork" | json_get child_session_id)"
printf '%s' "$api_fork" | grep -q '"started":false' || fail "API fork start_immediately false started child"
api_get "/api/sessions/$api_fork_id" | grep -q '"status":"stopped"' || fail "API fork start_immediately false child was not stopped"
api_delete "/api/sessions/$api_fork_id" >/dev/null || fail "API fork cleanup failed"
api_post "/api/sessions/$api_session_id/archive" '{"reason":"harness-restore"}' >/dev/null
if api_get "/api/sessions" | json_has_id "$api_session_id"; then
	fail "API archived session is still visible"
fi
api_post "/api/sessions/$api_session_id/restore" | grep -q '"archived":false' || fail "API session restore failed"
api_post "/api/sessions/$api_session_id/start" >/dev/null
api_post "/api/sessions/$api_session_id/send" '{"text":"api-ping"}' >/dev/null
wait_api_output "$api_session_id" "api-ping"
api_post "/api/sessions/$api_session_id/events" '{"kind":"agent_state","state":"working","source":"api-harness","tool":{"name":"Bash"}}' | grep -q '"kind":"agent_state"' || fail "API session event ingest failed"
api_get "/api/sessions/$api_session_id/status" | grep -q '"deck_status":"running"' || fail "API session event ingest did not update deck status"
api_post "/api/sessions/$api_session_id/events" '{"state":"idle","source":"api-harness"}' | grep -q '"state":"idle"' || fail "API session event ingest default kind failed"
api_get "/api/sessions?status=running" | json_has_id "$api_session_id" || fail "API lifecycle status filter missed running session"
api_get "/api/sessions?deck_status=idle" | json_has_id "$api_session_id" || fail "API deck status filter missed idle session"
api_get "/api/sessions?status=idle" | json_has_id "$api_session_id" || fail "API status filter missed idle deck-status session"
api_claude_body="$(python3 -c 'import json,sys
print(json.dumps({
"path": sys.argv[1],
    "agent": "claude",
    "command": "cat",
    "name": "harness-api-claude-sync",
    "group_name": "harness",
}))' "$API_WORKSPACE")"
api_claude_json="$(api_post /api/sessions "$api_claude_body")"
api_claude_id="$(printf '%s' "$api_claude_json" | json_get id)"
SESSION_IDS+=("$api_claude_id")
write_claude_tool_transcript "$API_HOME" "$api_claude_id" "$API_WORKSPACE" "Read"
api_sync="$(api_post "/api/sessions/$api_claude_id/sync-state")"
printf '%s' "$api_sync" | grep -q '"synced":true' || fail "API Claude sync-state did not sync"
	printf '%s' "$api_sync" | grep -q '"source":"claude_transcript"' || fail "API Claude sync-state source mismatch"
	printf '%s' "$api_sync" | grep -q '"name":"Read"' || fail "API Claude sync-state missed tool"
	api_post "/api/sessions/$api_claude_id/sync-state" | grep -q '"synced":false' || fail "API Claude sync-state duplicated transcript event"
	write_codex_tool_transcript "$API_HOME" "$api_session_id" "$API_WORKSPACE" "shell"
	api_codex_sync="$(api_post "/api/sessions/$api_session_id/sync-state")"
	printf '%s' "$api_codex_sync" | grep -q '"synced":true' || fail "API Codex sync-state did not sync"
	printf '%s' "$api_codex_sync" | grep -q '"source":"codex_transcript"' || fail "API Codex sync-state source mismatch"
	printf '%s' "$api_codex_sync" | grep -q '"name":"shell"' || fail "API Codex sync-state missed tool"
	api_post "/api/sessions/$api_session_id/sync-state" | grep -q '"synced":false' || fail "API Codex sync-state duplicated transcript event"
	[[ "$(api_get "/api/search?q=agenthelm-codex-search-needle" | json_get results.0.session_id)" == "$api_session_id" ]] || fail "API search missed Codex transcript"
	[[ "$(api_get "/api/search?q=agenthelm-codex-search-needle" | json_get results.0.source)" == "codex_transcript" ]] || fail "API search Codex source mismatch"
	[[ "$(api_post_status "/api/sessions/$api_session_id/events" '{"kind":"output","state":"working"}')" == "400" ]] || fail "API session event ingest accepted unsupported kind"
[[ "$(api_post_status "/api/sessions/$api_session_id/events" '{"state":"paused"}')" == "400" ]] || fail "API session event ingest accepted invalid state"
[[ "$(api_post_status "/api/sessions/$api_session_id/events" '{"payload":{"state":"working","source":42}}')" == "400" ]] || fail "API session event ingest accepted invalid source"
[[ "$(api_post_status "/api/sessions/$api_session_id/events" '{"state":"working","tool":{}}')" == "400" ]] || fail "API session event ingest accepted invalid tool"
api_get "/api/sessions/$api_session_id/events" | grep -q '"kind":"input"' || fail "API raw events missing input event"
api_get "/api/sessions/$api_session_id/events" | grep -q '"source":"api-harness"' || fail "API raw events missing ingested agent_state event"
	api_get "/api/sessions/$api_claude_id/events" | grep -q '"source":"claude_transcript"' || fail "API Claude raw events missing sync event"
	api_get "/api/sessions/$api_session_id/events" | grep -q '"source":"codex_transcript"' || fail "API Codex raw events missing sync event"
	api_get "/api/sessions/$api_session_id/structured-events" | grep -q '"kind":"input"' || fail "API structured events missing input event"
	api_get "/api/sessions/$api_session_id/structured-events" | grep -q '"source":"api-harness"' || fail "API structured events missing ingested agent_state event"
	api_get "/api/sessions/$api_claude_id/structured-events" | grep -q '"source":"claude_transcript"' || fail "API Claude structured events missing sync event"
	api_get "/api/sessions/$api_session_id/structured-events" | grep -q '"source":"codex_transcript"' || fail "API Codex structured events missing sync event"
api_stream "/api/sessions/$api_session_id/terminal-stream?limit=80" | grep -q "event: terminal" || fail "API terminal stream did not emit SSE"
api_stream "/api/sessions/$api_session_id/structured-stream?limit=200" | grep -q "event: structured" || fail "API structured stream did not emit SSE"
api_post "/api/sessions/$api_session_id/costs" '{"amount_usd":0.0123,"model":"harness-model","input_tokens":10,"output_tokens":15}' >/dev/null
[[ "$(api_get "/api/costs?session_id=$api_session_id&include_archived=true" | json_get event_count)" == "1" ]] || fail "API cost event was not counted"
[[ "$(api_get "/api/costs?session_id=$api_session_id&include_archived=true" | json_get total_tokens)" == "25" ]] || fail "API cost tokens were not summarized"
[[ "$(api_post_status "/api/sessions/$api_session_id/costs" '{"amount_usd":0.01,"input_tokens":-1}')" == "400" ]] || fail "API cost record accepted negative tokens"
[[ "$(api_get "/api/cost-events?session_id=$api_session_id&model=harness-model&include_archived=true&limit=1" | json_get 0.payload.model)" == "harness-model" ]] || fail "API cost events missed recorded model"
api_delete "/api/groups/harness?force=true" >/dev/null || fail "API group force delete failed"
api_get "/api/sessions/$api_session_id" | grep -q '"group_name":"default"' || fail "API group force delete did not move session"
api_get "/api/projects" | grep -q '"root_path"' || fail "API projects missing created project"
api_project_id="$(api_get "/api/projects" | json_get 0.id)"
api_get "/api/workspaces/$api_workspace_id" | grep -q '"path":"' || fail "API workspace show failed"
api_get "/api/projects/$api_project_id/workspaces" | json_has_id "$api_workspace_id" || fail "API workspace list missed session workspace"
api_seed_project_body="$(python3 -c 'import json,sys; print(json.dumps({"path":sys.argv[1],"default_branch":"api-seed"}))' "$API_DEFAULT_PROJECT")"
api_post "/api/projects" "$api_seed_project_body" | grep -q '"default_branch":"api-seed"' || fail "API project seed default branch failed"
api_existing_project_body="$(python3 -c 'import json,sys; print(json.dumps({"path":sys.argv[1],"trusted":True,"default_branch":"api-main"}))' "$API_DEFAULT_PROJECT")"
api_post "/api/projects" "$api_existing_project_body" | grep -q '"trust_state":"trusted"' || fail "API existing project trusted re-add failed"
api_get "/api/projects" | grep -q '"default_branch":"api-main"' || fail "API project default branch was not preserved"
api_post "/api/projects/$api_project_id/trust" | grep -q '"trust_state":"trusted"' || fail "API project trust failed"
api_watcher_body="$(python3 -c 'import json,sys; print(json.dumps({"name":"api-watch","adapter_id":"shell","project_id":sys.argv[1],"config":{"command":"printf api-watch","session_id":sys.argv[2]}}))' "$api_project_id" "$api_session_id")"
api_watcher_id="$(api_post "/api/watchers" "$api_watcher_body" | json_get id)"
api_get "/api/projects/$api_project_id/watchers" | json_has_id "$api_watcher_id" || fail "API project watcher list missed watcher"
[[ "$(api_post "/api/watchers/$api_watcher_id/start" | json_get status)" == "running" ]] || fail "API watcher start failed"
api_get "/api/watchers/$api_watcher_id/events" | grep -q "api-watch" || fail "API watcher events missing api-watch"
wait_api_output "$api_session_id" "api-watch"
api_post "/api/watchers/$api_watcher_id/poll" | grep -q '"status":"running"' || fail "API watcher poll failed"
[[ "$(api_get "/api/watchers/$api_watcher_id/events" | grep -o "api-watch" | wc -l | tr -d ' ')" -ge 2 ]] || fail "API watcher poll did not append output"
api_post "/api/watchers/poll" | grep -q '"status":"running"' || fail "API watcher poll-all failed"
[[ "$(api_get "/api/watchers/$api_watcher_id/events" | grep -o "api-watch" | wc -l | tr -d ' ')" -ge 3 ]] || fail "API watcher poll-all did not append output"
api_post "/api/watchers/$api_watcher_id/test" | grep -q '"adapter_id":"shell"' || fail "API watcher test returned wrong adapter"
api_delete "/api/watchers/$api_watcher_id" >/dev/null || fail "API watcher delete failed"
if api_get "/api/projects/$api_project_id/watchers" | json_has_id "$api_watcher_id"; then
	fail "API deleted watcher is still visible"
fi
api_external_watcher_body="$(python3 -c 'import json,sys; print(json.dumps({"name":"api-external-watch","adapter_id":"manual","config":{"session_id":sys.argv[1],"require_signature":True}}))' "$api_session_id")"
api_external_watcher_id="$(api_post "/api/watchers" "$api_external_watcher_body" | json_get id)"
api_post "/api/watchers/$api_external_watcher_id/start" >/dev/null
api_external_event_json="$(api_post "/api/watchers/$api_external_watcher_id/events" '{"source":"external","event_type":"push","payload":"api-external-watch","signature_status":"verified"}')"
printf '%s' "$api_external_event_json" | grep -q '"route_decision":"session:' || fail "API external watcher event did not route"
printf '%s' "$api_external_event_json" | grep -q '"signature_status":"verified"' || fail "API external watcher event did not preserve signature status"
api_get "/api/watchers/$api_external_watcher_id/events" | grep -q "api-external-watch" || fail "API external watcher event missing"
wait_api_output "$api_session_id" "api-external-watch"
api_delete "/api/watchers/$api_external_watcher_id" >/dev/null || fail "API external watcher delete failed"
api_post "/api/projects/$api_project_id/worktrees/cleanup" | grep -q '"inspected":' || fail "API worktree cleanup failed"
api_mcp_id="$(api_post "/api/sessions/$api_session_id/mcp" '{"id":"api-mcp"}' | json_get id)"
api_post "/api/projects/$api_project_id/mcp" '{"id":"api-project-mcp"}' | grep -q '"scope":"project"' || fail "API project MCP attach failed"
api_post "/api/profile/mcp" '{"id":"api-profile-mcp"}' | grep -q '"scope":"profile"' || fail "API profile MCP attach failed"
api_skill_id="$(api_post "/api/sessions/$api_session_id/skills" '{"id":"api-skill"}' | json_get id)"
api_post "/api/projects/$api_project_id/skills" '{"id":"api-project-skill"}' | grep -q '"scope":"project"' || fail "API project skill attach failed"
api_post "/api/profile/skills" '{"id":"api-profile-skill"}' | grep -q '"scope":"profile"' || fail "API profile skill attach failed"
api_get "/api/mcp" | grep -q '"server_id":"api-mcp"' || fail "API MCP attachment missing"
api_get "/api/skills" | grep -q '"skill_id":"api-skill"' || fail "API skill attachment missing"
api_materialization="$(api_get "/api/sessions/$api_session_id/materialization")"
printf '%s' "$api_materialization" | grep -q '"server_id":"api-mcp"' || fail "API session materialization missing MCP"
printf '%s' "$api_materialization" | grep -q '"skill_id":"api-skill"' || fail "API session materialization missing skill"
api_shell_tools_body="$(python3 -c 'import json,sys; print(json.dumps({"path":sys.argv[1],"agent":"shell","command":"cat","name":"api-shell-tools","group_name":"harness"}))' "$API_WORKSPACE")"
api_shell_tools_id="$(api_post /api/sessions "$api_shell_tools_body" | json_get id)"
SESSION_IDS+=("$api_shell_tools_id")
api_shell_materialization="$(api_get "/api/sessions/$api_shell_tools_id/materialization")"
printf '%s' "$api_shell_materialization" | grep -q '"mcp":\[\]' || fail "API shell materialization inherited MCP"
printf '%s' "$api_shell_materialization" | grep -q '"skills":\[\]' || fail "API shell materialization inherited skills"
api_post "/api/mcp/sync" >/dev/null
api_post "/api/skills/sync" >/dev/null
api_post "/api/mcp/$api_mcp_id/detach" | grep -q '"status":"detached"' || fail "API MCP detach failed"
api_post "/api/skills/$api_skill_id/detach" | grep -q '"status":"detached"' || fail "API skill detach failed"
api_watcher_id="$(api_post "/api/watchers" '{"name":"api-watch"}' | json_get id)"
[[ "$(api_post "/api/watchers/api-watch/start" | json_get id)" == "$api_watcher_id" ]] || fail "API watcher start by name failed"
api_post "/api/watchers/$api_watcher_id/stop" | grep -q '"status":"stopped"' || fail "API watcher stop failed"
api_conductor_id="$(api_post "/api/sessions/$api_session_id/conductor" | json_get id)"
api_post "/api/conductors/$api_conductor_id/start" | grep -q '"status":"running"' || fail "API conductor start failed"
api_get "/api/conductors/$api_conductor_id" | grep -q '"status":"running"' || fail "API conductor get failed"
api_post "/api/conductors/$api_conductor_id/heartbeat" | grep -q '"status":"running"' || fail "API conductor heartbeat failed"
api_assignment_json="$(api_post "/api/conductors/$api_conductor_id/send" "{\"session_id\":\"$api_session_id\",\"task_ref\":\"api-task\"}")"
printf '%s' "$api_assignment_json" | grep -q '"status":"assigned"' || fail "API conductor send failed"
api_assignment_id="$(printf '%s' "$api_assignment_json" | json_get id)"
api_get "/api/conductors/$api_conductor_id/assignments" | grep -q "api-task" || fail "API conductor assignments missing task"
api_post "/api/conductor-assignments/$api_assignment_id/complete" '{}' | grep -q '"status":"completed"' || fail "API conductor assignment complete failed"
api_failed_assignment_json="$(api_post "/api/conductors/$api_conductor_id/send" "{\"session_id\":\"$api_session_id\",\"task_ref\":\"api-fail\"}")"
api_failed_assignment_id="$(printf '%s' "$api_failed_assignment_json" | json_get id)"
api_patch "/api/conductor-assignments/$api_failed_assignment_id" '{"status":"failed"}' | grep -q '"status":"failed"' || fail "API conductor assignment fail failed"
api_cancel_assignment_json="$(api_post "/api/conductors/$api_conductor_id/send" "{\"session_id\":\"$api_session_id\",\"task_ref\":\"api-cancel\"}")"
api_cancel_assignment_id="$(printf '%s' "$api_cancel_assignment_json" | json_get id)"
api_patch "/api/conductor-assignments/$api_cancel_assignment_id" '{"status":"cancelled"}' | grep -q '"status":"cancelled"' || fail "API conductor assignment cancel failed"
wait_api_output "$api_session_id" "api-task"
api_post "/api/conductors/$api_conductor_id/stop" | grep -q '"status":"stopped"' || fail "API conductor stop failed"
api_delete "/api/conductors/$api_conductor_id" >/dev/null || fail "API conductor delete failed"
if api_get "/api/conductors" | json_has_id "$api_conductor_id"; then
	fail "API deleted conductor is still visible"
fi
[[ "$(api_get "/api/costs?include_archived=true" | json_get event_count)" == "1" ]] || fail "API costs summary did not use active profile"
api_post "/api/sessions/$api_session_id/stop" >/dev/null
api_delete "/api/sessions/$api_session_id" >/dev/null
if api_get "/api/sessions?archived=true" | json_has_id "$api_session_id"; then
  :
else
  fail "deleted API session was not archived"
fi
[[ "$(api_get "/api/costs?session_id=$api_session_id" | json_get event_count)" == "1" ]] || fail "API costs default omitted archived session"
[[ "$(api_get "/api/costs?session_id=$api_session_id&active_only=true" | json_get event_count)" == "0" ]] || fail "API costs active_only included archived session"
[[ "$(api_get "/api/cost-events?session_id=$api_session_id" | json_get 0.payload.model)" == "harness-model" ]] || fail "API cost events default omitted archived session"
[[ "$(api_get "/api/cost-events?session_id=$api_session_id&active_only=true" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')" == "0" ]] || fail "API cost events active_only included archived session"
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
readonly_body="$TMP_ROOT/read-only-body.json"
readonly_status="$(curl -sS -o "$readonly_body" -w "%{http_code}" \
	-H "x-agent-helm-token: $TOKEN" \
	-H "content-type: application/json" \
	-X POST \
	-d '{' \
	"$API_URL/api/sessions")"
[[ "$readonly_status" == "403" ]] || fail "read-only API parsed malformed mutation before writable check"
grep -q '"code":"read_only"' "$readonly_body" || fail "read-only API malformed mutation returned wrong error"

step "ok"
