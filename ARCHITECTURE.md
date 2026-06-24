# Agent Helm Architecture

Agent Helm is a local AI session command center for developers who run many terminal-native agents at once. It combines the broad product behavior of `agent-deck` with the cleaner maintainability patterns of `agent-of-empires`, while leaving the implementation language and UI toolkit open.

This document began as architecture only. The repo now includes a Rust
implementation for the first local-controller milestone; broader product items
below are backlog unless the README names them as current scope.

## Product Goals

- Fast local command center for AI coding sessions across projects, agents, groups, and worktrees.
- Terminal-native first: CLI and TUI must be complete surfaces, not thin launchers for a web app.
- Web UI as a companion surface for browser terminals, structured agent views, diffs, and phone access.
- Local-first storage and runtime: sessions continue running when the UI exits.
- Maintainable internals: one controller, explicit storage contracts, narrow adapters, and testable lifecycle flows.
- Safe by default around project hooks, sandbox paths, web access, remote events, and secrets.

## Non-Goals

- No cloud control plane as a required dependency.
- No hosted account system for first release.
- No mandatory programming language, UI framework, database engine, or frontend toolkit.
- No source compatibility promise with either source project.
- No copied source code from `agent-deck` or `agent-of-empires`.
- No implementation scaffold until the architecture is accepted.

## Source Synthesis

Agent Helm keeps these `agent-deck` product behaviors:

- Fleet dashboard: one place to see running, waiting, idle, stopped, errored, and archived sessions.
- Groups: nested or named grouping, default paths, moving sessions between groups, and group-scoped navigation.
- Search: fuzzy session search plus transcript or conversation search when indexed data exists.
- Forking: quick fork and explicit fork flows, with parent linkage and optional worktree or state carryover.
- MCP management: define servers once, attach or detach per session or profile, and restart affected sessions when needed.
- Skills management: managed skill pool, project-level attachment, and deterministic materialization into agent-specific locations.
- Conductor and watchers: supervised coordination sessions plus external event adapters that can wake or route work.
- Cost tracking: append cost events, show current spend by session, group, project, model, and time window.
- Web UI: browser terminal access, read-only mode, token-protected local server, archived sessions, costs, and fork actions.
- Multi-agent support: Claude-like, Codex-like, Gemini-like, OpenCode-like, shell, and custom command agents through adapters.
- Worktree ergonomics: create, finish, cleanup, setup hooks, ignored-file include rules, and bare-repo awareness.
- Sandbox ergonomics: optional container-backed sessions, shared auth volumes, container shell, and one-shot sandboxed runs.

Agent Helm keeps these `agent-of-empires` maintainability patterns:

- Clear module boundaries around session storage, runtime, projects, workspaces, settings, TUI, web, and agent adapters.
- Structured view model for agent protocol events, so web and TUI can render tool calls without scraping terminal text.
- Web daemon that talks to the same controller as the CLI and TUI instead of owning separate business logic.
- Explicit storage locking and atomic update rules for cross-process safety.
- Project registry as a first-class data model with repo identity, default branch, trust state, and workspace roots.
- Worktree manager separated from session lifecycle so branch and filesystem rules are reusable.
- Sandboxing model separated from agent launch so trust and path restrictions can be tested independently.
- Diff view as a first-class surface for reviewing changes from TUI and web.
- Theme discipline: semantic color tokens shared across surfaces, density appropriate to a developer dashboard, and no decorative UI drift.
- Settings schema and profile merge rules that make global, profile, project, and session overrides explicit.

## Core Architecture

All user surfaces call one application controller. No surface mutates storage or tmux directly.

```text
CLI / TUI / Web / HTTP API
        |
        v
ApplicationController
        |
        +-- SessionStore
        +-- SessionRuntime
        +-- AgentAdapter registry
        +-- WorkspaceManager
        +-- ProjectRegistry
        +-- MCP and Skills layer
        +-- Orchestrator and Watchers
        +-- NotificationRouter
        +-- CostTelemetry
```

### Surfaces

- CLI: scriptable commands for every lifecycle operation.
- TUI: fast fleet dashboard, grouped navigation, search, structured session preview, diff view, and settings.
- Web: local browser dashboard with terminal stream, structured view, diff view, project/worktree metadata, and read-only mode.
- HTTP API: stable local automation surface used by web and external tools.

### Application Controller

The controller owns validation, permission checks, lifecycle orchestration, and event fan-out. It exposes command-shaped methods such as `create_session`, `send_input`, `fork_session`, `stop_session`, `archive_session`, and `attach_structured_view`.

### Session Store

The store owns durable profile state, session metadata, group metadata, event logs, cost events, project registry data, and conductor state. It must provide atomic row or record updates and must not rewrite full state for one session change unless the selected storage backend makes that transaction cheap and safe.

### Session Runtime

The runtime starts and controls terminal processes. The default runtime is tmux-backed so sessions survive UI exits, SSH disconnects, and web daemon restarts. Runtime implementations are replaceable behind the `SessionRuntime` interface.

### Agent Adapters

Adapters hide agent-specific launch, resume, status, fork, MCP, skill, and structured-protocol behavior. Terminal-only agents still work through runtime output and process status; richer agents can emit structured events.

### Workspace And Worktree Manager

The workspace manager owns project resolution, worktree creation, ignored-file inclusion, setup and teardown hooks, branch naming, cleanup, and multi-repo workspace metadata. Session creation asks this layer for a launch directory instead of building paths itself.

### MCP And Skills Layer

MCP and skills are profile, project, and session-scoped configuration layers. The controller resolves desired state, writes project/session state, and asks the relevant agent adapter how to materialize it.

### Orchestration, Watchers, And Notifications

The orchestrator coordinates conductor sessions, worker sessions, watcher events, reminders, escalations, and routing rules. Watchers are adapters for external event sources. Notifications are an output layer, not business logic.

### Costs And Telemetry

Cost telemetry records append-only events with enough fields to recompute pricing later. Local operational telemetry is opt-in and must never include secrets, transcripts, or source code unless explicitly exported by the user.

## Interfaces

Pseudo-interfaces are implementation-neutral. Names describe contracts, not required syntax.

```text
interface SessionStore
  load_profile(profile_id) -> ProfileSnapshot
  list_sessions(filter) -> SessionSummary[]
  get_session(session_id) -> SessionRecord
  create_session(record, initial_events) -> SessionRecord
  update_session(session_id, expected_version, patch) -> SessionRecord
  append_session_event(session_id, event) -> EventId
  append_cost_event(session_id, cost_event) -> EventId
  archive_session(session_id, archived_by, reason) -> SessionRecord
  delete_session(session_id, mode) -> DeletionResult
  with_profile_lock(profile_id, operation) -> Result
```

```text
interface SessionRuntime
  start(session_id, launch_spec) -> RuntimeHandle
  attach(session_id, attach_options) -> AttachResult
  send(session_id, bytes_or_text) -> SendResult
  capture(session_id, cursor, limit) -> OutputPage
  status(session_id) -> RuntimeStatus
  stop(session_id, signal_policy) -> StopResult
  restart(session_id, launch_spec) -> RuntimeHandle
  destroy(session_id) -> DestroyResult
```

```text
interface AgentAdapter
  id -> AgentId
  capabilities() -> AgentCapabilities
  build_launch_spec(session_record, resolved_config) -> LaunchSpec
  detect_status(runtime_snapshot, recent_events) -> AgentStatus
  resume(session_record) -> LaunchSpec
  fork(parent_session, fork_request) -> ForkPlan
  apply_mcp(session_record, resolved_mcp) -> MaterializationPlan
  apply_skills(session_record, resolved_skills) -> MaterializationPlan
  parse_structured_event(raw_event) -> StructuredEvent?
```

```text
interface WorkspaceManager
  resolve_project(path) -> ProjectRef
  register_project(project_spec) -> ProjectRecord
  create_workspace(project_ref, workspace_request) -> WorkspaceRecord
  create_worktree(project_ref, worktree_request) -> WorktreeRecord
  finish_worktree(worktree_id, finish_policy) -> FinishResult
  cleanup_orphans(project_ref) -> CleanupReport
  validate_sandbox_paths(project_ref, sandbox_spec) -> ValidationResult
```

```text
interface WatcherAdapter
  id -> WatcherId
  validate(config) -> ValidationResult
  start(config, event_sink) -> WatcherHandle
  stop(handle) -> StopResult
  normalize(raw_event) -> WatcherEvent
```

```text
interface Orchestrator
  route_event(watcher_event) -> RouteDecision
  assign(conductor_id, session_id, task) -> Assignment
  heartbeat(conductor_id) -> ConductorHealth
  summarize_fleet(filter) -> FleetSummary
  escalate(event, target) -> NotificationRequest
```

## Lifecycle Flows

### Create Session

1. Surface submits name, profile, project path, group, agent, sandbox, worktree, MCP, skills, and optional initial prompt.
2. Controller resolves profile config, project config, repo trust state, and session defaults.
3. Workspace manager resolves or creates project/workspace/worktree.
4. Controller asks agent adapter for launch spec and materialization plans.
5. Session store creates the session record and initial events under profile lock.
6. Runtime starts the tmux-backed process.
7. Controller records runtime handle, emits status update, and sends initial prompt if present.

### Start Or Restart Session

1. Controller loads session and verifies it is startable.
2. Agent adapter builds launch or resume spec.
3. Runtime starts process in the stored workspace.
4. Store records runtime metadata and status event.

### Attach Session

1. Surface requests terminal attach.
2. Controller verifies read/write permission for the surface.
3. Runtime attaches to the tmux session or returns web terminal stream metadata.
4. Structured-event consumers may attach in parallel without owning terminal input.

### Send Input

1. Surface sends text, bytes, or command payload.
2. Controller rejects writes in read-only mode or to non-running sessions.
3. Runtime writes to the session pane or protocol channel.
4. Store appends a user-input event without storing secrets marked as redacted.

### Status Update

1. Runtime status, hook events, protocol events, or bounded polling provide new state.
2. Agent adapter maps raw runtime state to agent-aware status.
3. Store updates only changed session fields and appends status event when status changes.
4. Controller fans out updates to TUI, web, notifications, conductor, and cost telemetry.

### Fork Session

1. Surface submits parent session and fork options.
2. Controller loads parent, agent capabilities, project/workspace data, and fork defaults.
3. Agent adapter produces a fork plan for conversation inheritance.
4. Workspace manager optionally creates branch/worktree and copies allowed uncommitted or ignored state.
5. Store creates child session with parent linkage.
6. Runtime starts child session from fork plan.

### Stop Session

1. Controller verifies session is running or starting.
2. Runtime sends graceful stop, then escalates according to signal policy.
3. Store records stopped state, runtime exit metadata, and event.
4. Notifications and orchestrator receive final state.

### Resume Session

1. Controller loads stopped or archived session.
2. Agent adapter validates resume capability and conversation handle.
3. Workspace manager verifies workspace still exists or offers repair.
4. Runtime starts resume launch spec.
5. Store records runtime handle and status.

### Delete Session

1. Controller checks delete mode: metadata-only, with worktree cleanup, or full purge.
2. Runtime is stopped if needed.
3. Workspace manager runs teardown hooks and cleanup where allowed.
4. Store tombstones or deletes session metadata according to retention policy.
5. Append-only history remains unless full purge is explicitly requested.

### Archive Session

1. Controller stops runtime if still running unless policy allows live archive.
2. Store marks session archived, preserving transcript references, cost events, parent linkage, and workspace metadata.
3. Default list views hide archived sessions; archive views can restore or delete them.

### Web Structured-View Attach

1. Web requests structured view for a session.
2. Controller verifies auth token and read/write mode.
3. Agent adapter exposes structured protocol stream when available.
4. Web receives normalized tool calls, prompts, approvals, file edits, diffs, and status events.
5. If no structured stream exists, web falls back to terminal output plus captured metadata.

## Public Interfaces

### CLI Shape

Top-level command:

```text
agent-helm [--profile <name>] [--json] <command>
```

Lifecycle commands:

```text
agent-helm add <path> [--agent <id>] [--group <name>] [--name <name>] [--worktree <branch>] [--sandbox] [--prompt <text>]
agent-helm list [--all] [--archived] [--group <name>] [--status <status>]
agent-helm attach <session>
agent-helm send <session> <text>
agent-helm stop <session>
agent-helm restart <session>
agent-helm remove <session> [--purge] [--cleanup-worktree]
agent-helm fork <session> [--name <name>] [--group <name>] [--worktree <branch>] [--carry-state]
agent-helm serve [--listen <addr>] [--token <token>] [--read-only]
```

Domain commands:

```text
agent-helm project add|list|show|trust|untrust|remove
agent-helm worktree create|finish|cleanup|list
agent-helm mcp list|attach|detach|sync
agent-helm skill list|attach|detach|sync
agent-helm watcher list|start|stop|test
agent-helm conductor setup|list|start|stop|send|status
```

### HTTP Shape

The HTTP API is local-first and token-protected when the server is exposed beyond loopback.

```text
GET    /api/sessions
POST   /api/sessions
GET    /api/sessions/{id}
POST   /api/sessions/{id}/send
GET    /api/sessions/{id}/output
POST   /api/sessions/{id}/fork
POST   /api/sessions/{id}/stop
POST   /api/sessions/{id}/restart
DELETE /api/sessions/{id}

GET    /api/projects
POST   /api/projects
GET    /api/projects/{id}
GET    /api/projects/{id}/worktrees
POST   /api/projects/{id}/worktrees

GET    /api/sessions/{id}/structured-stream
GET    /api/sessions/{id}/terminal-stream
GET    /api/sessions/{id}/diff

GET    /api/mcp
POST   /api/sessions/{id}/mcp
GET    /api/skills
POST   /api/sessions/{id}/skills

GET    /api/watchers
POST   /api/watchers/{id}/start
POST   /api/watchers/{id}/stop
GET    /api/conductors
POST   /api/conductors/{id}/send
GET    /api/costs
```

Read-only mode:

- Allows `GET` endpoints for sessions, output, structured streams, terminal streams, diffs, projects, worktrees, watchers, conductors, and costs.
- Rejects `POST`, `PATCH`, `PUT`, and `DELETE` endpoints that mutate state or send input.
- May allow local UI preferences that do not touch session, project, config, or runtime state.

### Config Shape

Primary config file:

```toml
[profiles.default]
data_dir = "~/.local/share/agent-helm/default"

[agents.claude]
command = "claude"

[session_defaults]
agent = "claude"
group = "default"
status_poll_interval_ms = 1000

[worktrees]
enabled = true
default_location = "sibling"
default_base_branch = "main"

[sandboxing]
enabled_by_default = false
allowed_paths = ["~/git"]
volume_ignores = ["node_modules", ".venv", "target"]

[web]
listen = "127.0.0.1:8420"
read_only = false

[web.auth]
token_env = "AGENT_HELM_WEB_TOKEN"

[mcp]
pool_enabled = false

[skills]
pool_dir = "~/.config/agent-helm/skills/pool"

[watchers]
enabled = true

[conductor]
enabled = true

[costs]
enabled = true
retention_days = 90

[theme]
name = "default"
```

Config merge order:

```text
built-in defaults < global config < profile config < project config < session overrides < CLI/API request
```

Repo config may define session, sandbox, worktree, hook, MCP, skill, and watcher defaults only after the project is trusted.

## State Model

- Profile: data directory, active settings, theme, default agent, feature flags, and auth references.
- Session: id, title, profile, group, project, workspace, agent id, status, runtime handle, parent id, archived flag, timestamps, and version.
- Group: id, name/path, ordering, default project path, collapsed state, and display metadata.
- Project: id, root path, repo identity, default branch, trust state, hooks hash, and config hash.
- Workspace: id, project id, path, worktree id, sandbox id, multi-repo roots, and cleanup policy.
- Agent config: command, environment references, capabilities, resume/fork handles, and protocol mode.
- MCP config: server definitions, scope, materialized state, conflicts, and restart requirements.
- Skill config: pool entries, session attachments, project attachments, materialized paths, and sync status.
- Watcher event: source, normalized type, payload reference, signature status, route decision, and delivery state.
- Conductor state: conductor session id, watched sessions, assignments, heartbeats, escalations, and channel bindings.
- Cost event: session id, agent id, model, token counts, price version, estimated cost, source, and timestamp.

## Storage Contracts

- Every profile has independent state and locks.
- Mutations are atomic: either the full intended change is visible or no change is visible.
- Cross-process writers use explicit profile locks.
- Long-running runtime operations never hold storage locks.
- Session updates use expected version or equivalent conflict detection.
- Single-session changes update a single row or record plus append event; they do not rewrite unrelated session state.
- History-bearing data uses append-only event logs: status changes, lifecycle events, watcher events, conductor actions, costs, and deletes.
- Compaction is allowed only as a background maintenance operation with snapshot plus retained event cursor.
- Storage files, if used, are written with temp-file, fsync, rename, and directory fsync semantics where the platform supports them.
- Secrets are referenced by environment variable, keychain, local secret store, or untracked file path; tracked config never contains secret values.

## Performance Rules

- Do not rewrite full state for a single-row or single-session change.
- Bound status polling by active sessions, visible sessions, and backoff state.
- Prefer hook, protocol, and event paths over terminal pane scraping.
- Use pane scraping only as a fallback for agents without richer status or protocol signals.
- Index transcripts lazily and incrementally; startup must not scan every transcript.
- Web terminal streams and structured streams use isolated workers or bounded tasks so one slow browser cannot block session control.
- Startup path loads profile metadata, session summaries, and current runtime handles first; expensive indexes, costs, and transcripts load after first paint.
- Diff generation is on demand and cached by repo state.
- Cost recomputation is a background operation over append-only cost events.

## Security Model

- Local-first storage under user-owned config and data directories.
- Web server binds to loopback by default.
- Web token auth is required for non-loopback access and recommended for all browser access.
- Read-only web mode blocks all writes, sends, restarts, deletes, forks, config changes, and project trust changes.
- Repo trust gate is required before project hooks, project MCP config, project skill materialization, sandbox overrides, or watcher config can run.
- Sandbox path restrictions are validated before launch and again before bind mounting.
- Worktree setup and teardown hooks run only for trusted projects.
- Webhook watcher adapters verify HMAC or equivalent signatures when the event source supports signing.
- Watcher payloads are stored as events with redaction support.
- Secrets never go into tracked config, logs, transcripts, cost events, or exported diagnostics unless explicitly included by the user.
- Remote or tunneled access must surface address, auth mode, and read/write mode before enabling.

## Testing Strategy

- Store tests: locking, atomic writes, version conflicts, append-only events, profile isolation, and migration behavior.
- Runtime tests: tmux session start, attach, send, capture, stop, restart, and orphan recovery through a fake runtime plus smoke tests for real tmux.
- Agent adapter tests: capability flags, launch spec generation, status detection, resume, fork planning, MCP materialization, and structured event parsing.
- Workspace tests: project resolution, bare repo handling, worktree creation, ignored-file inclusion, setup/teardown hooks, cleanup, and path validation.
- Controller tests: create, start, attach, send, status update, fork, stop, resume, delete, archive, and read-only rejection.
- Web/API tests: route permissions, token auth, structured stream, terminal stream, diff endpoint, fork endpoint, and read-only behavior.
- TUI tests: grouped list rendering, search, status transitions, diff view entry, structured view entry, and theme token use.
- Security tests: repo trust gate, sandbox restrictions, webhook signature checks, secret redaction, and no tracked secret config.
- Performance tests: startup with many sessions, bounded polling, lazy transcript indexing, event fan-out under slow web clients, and single-session update cost.

## Acceptance Criteria

- README documents the current milestone and runnable checks.
- CLI, TUI, optional HTTP API, config, storage, lifecycle, state, security, and
  testing contracts stay covered by implementation or docs.
- Broader architecture items remain explicit backlog until promoted into the
  README milestone.
- No source code is copied from `agent-deck` or `agent-of-empires`.
