# Agent Helm Architecture

Agent Helm is a local command center for long-running, terminal-backed agent
sessions. It lets one developer create, group, inspect, fork, stop, archive, and
coordinate many agent processes without making the UI process the owner of those
processes.

The application is local-first:

- The durable state lives in a per-profile SQLite database.
- The session runtime is tmux, so sessions survive UI exits and SSH disconnects.
- All user-facing surfaces call the same controller.
- The terminal UI and browser UI are clients of local state and runtime control;
  neither owns session lifecycle logic.

This document describes the current implementation, not a future product sketch.
Use it as the map for reproducing Agent Helm without rediscovering the runtime,
storage, terminal, and coordination constraints.

## Goals

- Manage many terminal-native agents from one local profile.
- Keep sessions running independently of the CLI, terminal UI, or HTTP server.
- Provide complete CLI and terminal UI workflows, with an optional local browser
  dashboard.
- Preserve enough structured state to render status, activity, worktree context,
  cost, watcher, conductor, and materialization information without scraping the
  screen for everything.
- Keep business logic centralized in the controller.
- Keep runtime-specific behavior behind a small interface.
- Treat project hooks, watcher config, sandbox paths, and materialization as
  trust-gated operations.

## Non-Goals

- No cloud control plane.
- No hosted account or sync service.
- No mandatory browser UI for core operation.
- No terminal UI implementation details in public contracts.
- No source compatibility promise with earlier prototypes.

## Runtime Requirements

Agent Helm assumes these programs and environment capabilities for the full
feature set:

- `tmux`: required for real session runtime, attach, send, capture, and TUI
  embedded session view.
- `git`: required for project detection, worktree creation, worktree cleanup,
  diffs, repo identity, and carry-state behavior.
- A POSIX-like shell: real session launch specs run through `sh -lc`.
- A writable home directory: defaults use `~/.local/share/agent-helm` and
  `~/.config/agent-helm`.

The code also has a fake runtime for tests. The fake runtime implements the same
runtime trait, but it does not exercise tmux behavior.

## Source Layout

- `src/main.rs`: CLI parsing, command dispatch, TUI bootstrap, optional HTTP
  server bootstrap.
- `src/controller.rs`: application service layer. It owns lifecycle validation,
  orchestration across store/runtime/workspaces/adapters, read-only checks, and
  high-level domain operations.
- `src/store.rs`: SQLite persistence, migrations, profile locking, redaction at
  event write time, and query helpers.
- `src/runtime.rs`: runtime interface, tmux runtime, fake runtime, tmux attach
  quirks, tmux capture/send/status/stop behavior.
- `src/adapter.rs`: agent registry, launch specs, status derivation, fork plans,
  structured event normalization, MCP and skill capability checks.
- `src/workspace.rs`: project resolution, git repo identity, default branch
  detection, git worktree creation/removal, carry-state copying, setup/teardown
  hooks, sandbox path validation.
- `src/tui.rs`: terminal dashboard, forms, grouped list, live preview, embedded
  focused session, profile tool settings, keyboard/mouse handling.
- `src/api.rs`: local HTTP API and browser dashboard routes, auth, read-only
  mutation guard, stream endpoints.
- `src/config.rs`: config loading, profile defaults, tool profiles, settings
  file writes, path expansion.
- `src/materialization.rs`: stable JSON view of effective MCP and skill
  attachments.
- `src/security.rs`: trust gates, sandbox path checks, event/JSON secret
  redaction.
- `src/models.rs`: shared DTOs and persisted records.

## Core Dependency Direction

Every surface calls the controller. No surface should write the store directly or
run tmux directly except for the TUI preview/embedded rendering path, which uses
tmux only for read-only preview capture and embedded attach plumbing.

```text
CLI / terminal UI / HTTP API / browser UI
    -> ApplicationController
        -> SessionStore
        -> SessionRuntime
        -> AgentRegistry
        -> WorkspaceManager
        -> security/materialization helpers
```

The controller is the boundary where write permissions, read-only mode, project
trust, runtime status reconciliation, and event appends are enforced.

## Configuration

Configuration is loaded by profile. The default profile is `default`.

Load order:

1. Built-in defaults.
2. `~/.config/agent-helm/config.toml`.
3. `<profile data dir>/config.toml`.
4. `~/.config/agent-helm/settings.toml`.

The later file wins for overlapping settings. The terminal UI profile tool
settings popup writes `settings.toml`.

Built-in defaults:

```toml
profile = "default"
data_dir = "~/.local/share/agent-helm/default"
default_agent = "shell"
default_group = "default"
web_listen = "127.0.0.1:8420"
web_read_only = false
web_token_env = "AGENT_HELM_WEB_TOKEN"
headroom_proxy_savings_path = "~/.headroom/proxy_savings.json"
sandbox_image = "alpine:latest"
sandbox_allowed_paths = []
```

Tool profiles define how agents are launched:

```toml
[tools.claude]
installed = true
executable = "claude"
flags = []
worktree = "always" # always, manual, never

[tools.shell]
installed = true
worktree = "never"
```

If a session request supplies `--cmd`, that command wins. Otherwise the
controller asks config for the tool command for the selected agent. Unknown tools
can still be command-backed when the command is explicit.

## Persistent State

Each profile has an independent database:

```text
<data_dir>/state.db
<data_dir>/state.db.lock
```

The store uses SQLite in WAL mode with foreign keys enabled. Writes acquire an
exclusive file lock on `state.db.lock`; this is separate from SQLite's own
locking and gives the application a profile-level mutation guard. Long-running
runtime work should not happen while holding store locks.

Primary tables:

- `sessions`: metadata and lifecycle state for every session.
- `groups`: hierarchical group names, collapsed state, default project paths.
- `session_events`: append-only events for status, input, agent state, sync,
  structured payloads, fork/archive/delete context.
- `cost_events`: append-only cost records tied to sessions.
- `projects`: resolved repository roots, repo identity, default branch, trust
  state, config/hook hashes.
- `workspaces`: launch directories and links to worktrees/sandboxes.
- `worktrees`: git worktree records and cleanup status.
- `mcp_attachments`: profile/project/session MCP attachments.
- `skill_attachments`: profile/project/session skill attachments.
- `watchers`: watcher configs and lifecycle state.
- `watcher_events`: normalized watcher events.
- `conductors`: conductor sessions and heartbeat state.
- `conductor_assignments`: work assigned to sessions by conductors.

Records carry a `version` field. Update paths use the current version where
conflict detection matters. IDs are UUID-like lowercase hex strings unless the
record is append-only and uses an integer row id.

## Session Model

`SessionRecord` is the central record:

- `id`, `name`, `profile`, `group_name`
- `project_id`, `workspace_id`, `worktree_id`
- `parent_session_id`
- `agent`, `command`, `project_path`
- `status`: lifecycle status (`starting`, `running`, `stopped`, `errored`)
- `runtime_id`: tmux runtime handle when running
- `archived`
- `version`, `created_at`, `updated_at`

There are two status layers:

- Lifecycle status: what the runtime says about the process.
- Deck status: user-facing activity status derived from lifecycle, recent
  events, and open conductor assignments.

Deck statuses are:

```text
starting, running, queued, waiting, idle, stopped, errored
```

Deck status derivation prefers recent agent/activity events over raw runtime
state. For example, a running tmux process can be shown as `waiting` after user
input is sent, or `queued` when a conductor assignment is queued.

## Controller Responsibilities

The controller owns all application semantics:

- Initialize profile state.
- Create sessions and optionally start them.
- Resolve projects/workspaces/worktrees.
- Build launch specs through the agent registry.
- Wrap launches with agent-state hooks when supported.
- Start, stop, restart, destroy, and status-check runtime sessions.
- Reconcile runtime state back into the store.
- Append lifecycle, input, fork, archive, delete, and agent-state events.
- List and mutate groups.
- Register, trust, untrust, and remove projects.
- Create, finish, and cleanup worktrees.
- Attach/detach/sync MCP and skill materialization.
- Create, test, start, poll, stop, and delete watchers.
- Create/start/heartbeat/stop/delete conductors and assignments.
- Record and summarize costs.
- Enforce read-only mode for all mutations.

Surfaces should not duplicate these rules. If a new command or route mutates
state, add a controller method or reuse an existing one.

## Session Lifecycle

### Create

1. Surface submits path, agent, command, name, group, worktree, sandbox, prompt,
   and parent session if any.
2. Controller enforces writable mode and initializes the store.
3. Agent and group default from config when omitted.
4. Controller resolves the project from the path, unless inheriting a parent
   worktree.
5. Worktree behavior is selected:
   - Explicit `--worktree` creates a named branch worktree.
   - Tool profile `worktree = "always"` auto-creates a worktree.
   - Inherited worktree reuse happens for some fork paths.
   - Otherwise the project root is used.
6. Trusted projects may run setup hooks for created worktrees.
7. Sandbox requests validate the launch path against configured allowed paths.
8. Store creates project/workspace/worktree/session records.
9. If `start_immediately` is false, store records a stopped status event.
10. If starting, controller builds a launch spec, starts the runtime, records the
    runtime handle, and appends runtime/agent-state events.
11. If an initial prompt exists, controller sends it to the runtime and appends a
    redacted input event.

### Status

1. Controller loads the session.
2. If the runtime handle says the process stopped, controller clears the runtime
   handle and stores `stopped` unless the controller is read-only.
3. Running state appends runtime status/agent-state events when useful.
4. `status_snapshot` derives deck status from recent events and conductor
   assignments.

Read-only controllers may return a reconciled in-memory status but must not write
that reconciliation to the database.

### Output

For running sessions, controller calls runtime capture. For stopped sessions,
output currently returns an error because the real source is the live tmux pane.
The terminal UI deliberately skips output loading for running/starting sessions;
the live preview path owns terminal output.

### Send Input

1. Controller verifies writable mode and running status.
2. Runtime sends text to tmux with Enter.
3. Store appends an `input` event with a redacted preview.
4. Store appends an agent-state event indicating waiting.

### Stop and Restart

- Stop kills the tmux session, clears `runtime_id`, sets lifecycle status to
  `stopped`, and appends a status event.
- Restart refuses archived sessions, rebuilds the launch spec, restarts tmux,
  records the new runtime handle, and appends status/agent-state events.

### Archive and Restore

- Archive can stop a running session first, marks the session archived, and
  appends archive metadata.
- Restore clears the archived flag.
- Default list views hide archived sessions unless requested.

### Delete

Delete modes:

- Metadata removal.
- Purge session state.
- Cleanup associated worktree when requested and allowed.

If cleanup is requested, the controller coordinates runtime destruction,
worktree teardown, and store removal. Worktrees still attached to other sessions
cannot be finished.

### Fork

1. Controller loads the parent.
2. Agent registry produces a fork plan.
3. If `carry_state` is true, the child command can inherit the parent command and
   conversation semantics for supported agents.
4. Child session is created with parent linkage.
5. Child may start immediately or remain stopped.
6. Store appends a `forked` event.

## Runtime: tmux

The real runtime is tmux. Session names are derived from the Agent Helm session
id and prefixed:

```text
agent-helm-<safe-session-fragment>
```

Launch behavior:

- `tmux new-session -d -s <name> -c <cwd> sh -lc <launch-command>`
- Session options disable tmux status and prefix keys.
- Window options disable pane border status.
- Runtime handle id is the tmux session name.

Attach behavior:

- CLI attach installs a temporary root binding for `C-q` to detach the tmux
  client.
- The previous `C-q` root binding is restored after attach exits.

Send behavior:

- Text is sent with `tmux send-keys -t <name> <text> Enter`.

Capture behavior:

- Output capture uses `tmux capture-pane -p -S -<limit>`.
- ANSI-preserving capture adds `-e`.
- Captured output trims only the final capture newline for line accounting.

Status behavior:

- Runtime status uses `tmux has-session`.
- Missing tmux session maps to `stopped`.

Destroy behavior:

- Stop and destroy kill the tmux session.

## Agent Registry

The agent registry defines known adapters:

- `shell`
- `claude`
- `codex`
- `gemini`
- `opencode`
- `custom`

Capabilities include:

- command-backed launch
- resume
- fork
- MCP
- skills
- structured events
- agent-state hooks

AI-like agents support MCP, skills, structured events, and agent-state hooks.
Command-only agents do not support MCP/skills, but they still produce normalized
structured events from recorded session events.

Launch specs:

- Session command is shell-quoted and run in the selected working directory.
- Sandbox launches wrap the command in a container command when requested.
- Agent-state hooks wrap supported agent launches so the binary can record
  activity transitions back into Agent Helm.

Status derivation:

- Lifecycle `starting`, `stopped`, and `errored` map directly.
- Running sessions inspect recent events.
- Recent active tool events map to `running`.
- Recent input events map to `waiting`.
- Done/idle agent events map to `idle`.
- Open conductor assignments can map to `queued` or `waiting`.

The registry also normalizes structured events from stored session events so the
web surface can render tool calls and state without owning agent-specific logic.

## Projects, Workspaces, and Worktrees

Project resolution:

- If the path is inside a git repo, the project root is `git rev-parse
  --show-toplevel`.
- Otherwise the existing path is the project root.
- Repo identity prefers origin URL with credentials stripped.
- Default branch prefers `origin/HEAD`, then `main`, then `master`, then current
  branch or git default branch config.

Project trust:

- New projects are untrusted unless explicitly registered/trusted.
- Trusted projects may run setup/teardown hooks and project-scoped privileged
  behavior.
- Trust is required for hooks, MCP, skills, and watchers.

Workspaces:

- A workspace is the path a session launches in.
- It can point at the project root, a git worktree, or a sandbox path.
- It records cleanup policy and multi-root metadata as strings.

Worktrees:

- Named worktrees are created with git worktree.
- Branch names are generated from requested branch/session context.
- Carry-state copies tracked diffs, untracked files, and ignored files from the
  source worktree into the target worktree.
- Setup hooks run only after a trusted project creates a worktree.
- Teardown hooks run only for trusted projects.
- Cleanup refuses worktrees still attached to sessions.

Sandboxing:

- Sandbox paths must exist and be under configured allowed paths.
- Validation happens before launch and before using created worktree paths.
- The default image is configured but sandbox execution remains launch-spec
  wrapping, not a separate runtime.

## Terminal UI

The terminal UI is a stateful local client over the controller. It renders:

- grouped session list
- status/activity counts
- live preview for running sessions
- detail/output panel for non-live sessions
- embedded focused session
- create/fork/move/send/settings/help forms

Startup:

1. `main.rs` loads all sessions including archived sessions.
2. It loads groups.
3. It passes closures to the UI for status loading, detail loading, and actions.
4. UI state owns selection, collapsed groups, filters, scroll, sidebar width,
   preview cache, embedded session state, and modal forms.

Navigation:

- `j/k` and arrow keys change selection.
- `/` enters search.
- `a` toggles archived visibility.
- `t` cycles status filter.
- `c/e` collapse or expand groups.
- `Enter` focuses the selected running session.
- `Ctrl-q` returns from focused session to the dashboard.

Sidebar sizing:

- Default sidebar width is 24 percent.
- The divider can be dragged.
- Minimum is 18 percent; maximum is 50 percent.
- Focusing a session preserves the current width. Preview and focused session
  use the same split, so entering a session should not shift the panel widths.

Live preview:

- The dashboard preview is read-only and uses `tmux capture-pane`.
- Preview capture is off the UI thread.
- Preview capture first syncs the tmux window to the preview viewport size. This
  keeps initial dashboard preview and focused-session attach from disagreeing
  about the top visible row.
- The preview worker is latest-only: when rapid navigation enqueues many
  requests, stale queued requests are skipped.
- Requests carry a generation id, session id, target, dimensions, and scroll.
- Results are applied only when they still match the pending request.
- Stale results are ignored.
- Preview refresh interval is 500 ms.
- Selection/detail movement must not wait for capture-pane.

Preview capture quirks:

- Alternate-screen capture is attempted first with tmux flags for alternate
  content.
- If alternate-screen capture is empty, visible pane capture is used.
- Visible capture uses `-S -<rows>` plus ANSI escapes.
- Final capture newline is trimmed before terminal parsing. Without this, the
  parser creates one extra bottom row and the first visible row disappears.
- Lone `\n` bytes are normalized to `\r\n` before parsing so lines return to
  column zero.
- Rendering bottom-crops the parsed screen to the preview area and supports
  mouse wheel scrollback.

Details loading:

- Details are debounced after selection changes by 125 ms.
- Running/starting sessions do not load controller output on selection change;
  preview owns terminal output.
- Stopped/errored sessions can load non-live details after the debounce.
- If details do not belong to the selected session yet, the detail panel renders
  a loading state rather than stale output.

Embedded focused session:

- The focused view attaches tmux through a PTY and feeds bytes into a terminal
  screen parser.
- Input keys are encoded and written to the PTY.
- Resize updates both the parser and PTY size.
- The command uses `TERM=xterm-256color`.
- The TUI renders terminal cells itself and sets the outer terminal cursor when
  the inner app leaves the cursor visible.
- Some full-screen apps hide the cursor. In focused session mode, Agent Helm
  paints an inverted cursor overlay at the parsed cursor position so text input
  location remains visible.
- Focused attach is not used for dashboard preview; preview remains read-only
  capture-pane snapshots.

## CLI Surface

The top-level shape:

```bash
agent-helm [--profile <name>] [--json] <command>
```

No command opens the terminal UI.

Common session commands:

```bash
agent-helm init
agent-helm add <path> [--agent <id>] [--cmd <cmd>] [--name <name>] [--group <name>] [--worktree <branch>] [--carry-state] [--sandbox] [--prompt <text>]
agent-helm list [--all] [--archived] [--group <name>] [--status <status>]
agent-helm search <query> [--limit <n>]
agent-helm session start|stop|restart <session>
agent-helm session create <path> ...
agent-helm session remove <session> [--purge] [--cleanup-worktree]
agent-helm session archive <session> [--by <actor>] [--reason <text>]
agent-helm session restore <session>
agent-helm session fork <session> [--name <name>] [--group <name>] [--worktree <branch>] [--carry-state] [--no-start]
agent-helm session attach <session>
agent-helm session show <session>
agent-helm session status <session>
agent-helm session status-snapshot <session>
agent-helm session send <session> <text>
agent-helm session output <session> [--limit <n>] [--ansi]
agent-helm session events <session> [--since <ts>]
agent-helm session record-event <session> <kind> <payload-json>
agent-helm session sync-state <session>
agent-helm session diff <session>
agent-helm session materialization <session>
agent-helm session structured-events <session>
```

Other command families:

```bash
agent-helm group list|create|update|delete|move
agent-helm project add|list|show|trust|untrust|remove
agent-helm workspace show|list
agent-helm worktree create|show|finish|cleanup|list
agent-helm mcp list|attach|attach-project|attach-profile|detach|sync
agent-helm skill list|attach|attach-project|attach-profile|detach|sync
agent-helm watcher list|events|create|start|poll|ingest|poll-all|test|stop|remove
agent-helm conductor setup|list|start|heartbeat|stop|remove|send|complete|fail|cancel|assignments|status
agent-helm costs [events|record] [filters]
agent-helm tui
agent-helm serve [--listen <addr>] [--token <token>] [--token-env <env>] [--read-only]
```

Prefer adding new lifecycle behavior under existing command families before
adding new top-level commands.

## HTTP API and Browser UI

The optional HTTP server is a local API plus an embedded browser dashboard.

Server behavior:

- Default listen address is `127.0.0.1:8420`.
- A token can be passed directly or read from an environment variable.
- Non-loopback access should use a token.
- Read-only mode rejects mutating methods before handlers run.
- API errors are JSON envelopes with `code` and `message`.

Main route families:

```text
GET  /
GET  /s/:id
GET  /api/about

GET,POST     /api/sessions
GET,DELETE   /api/sessions/:id
GET          /api/search
GET          /api/sessions/:id/status
GET          /api/sessions/:id/status-snapshot
POST         /api/sessions/:id/send
GET          /api/sessions/:id/output
POST         /api/sessions/:id/fork
POST         /api/sessions/:id/archive
POST         /api/sessions/:id/restore
POST         /api/sessions/:id/start
POST         /api/sessions/:id/stop
POST         /api/sessions/:id/restart
POST         /api/sessions/:id/group
GET          /api/sessions/:id/diff
GET          /api/sessions/:id/materialization
GET,POST     /api/sessions/:id/events
POST         /api/sessions/:id/sync-state
GET          /api/sessions/:id/terminal-stream
GET          /api/sessions/:id/structured-events
GET          /api/sessions/:id/structured-stream

GET,POST     /api/groups
PATCH,DELETE /api/groups/*name

GET,POST     /api/projects
GET,DELETE   /api/projects/:id
POST         /api/projects/:id/trust
POST         /api/projects/:id/untrust
GET          /api/projects/:id/workspaces
GET          /api/workspaces/:id
GET,POST     /api/projects/:id/worktrees
POST         /api/projects/:id/worktrees/cleanup
GET          /api/worktrees/:id
POST         /api/worktrees/:id/finish

GET,POST     /api/mcp
POST         /api/profile/mcp
POST         /api/projects/:id/mcp
POST         /api/sessions/:id/mcp
POST         /api/mcp/sync
POST         /api/mcp/:id/detach

GET,POST     /api/skills
POST         /api/profile/skills
POST         /api/projects/:id/skills
POST         /api/sessions/:id/skills
POST         /api/skills/sync
POST         /api/skills/:id/detach

GET,POST     /api/watchers
DELETE       /api/watchers/:id
GET          /api/projects/:id/watchers
GET,POST     /api/watchers/:id/events
POST         /api/watchers/:id/test
POST         /api/watchers/:id/start
POST         /api/watchers/:id/poll
POST         /api/watchers/:id/stop
POST         /api/watchers/poll

GET          /api/conductors
POST         /api/sessions/:id/conductor
GET,DELETE   /api/conductors/:id
GET          /api/conductors/:id/assignments
POST         /api/conductor-assignments/:id/complete
PATCH        /api/conductor-assignments/:id
POST         /api/conductors/:id/start
POST         /api/conductors/:id/heartbeat
POST         /api/conductors/:id/stop
POST         /api/conductors/:id/send

POST         /api/sessions/:id/costs
GET          /api/costs
GET          /api/cost-events
```

The browser dashboard should remain a client of these routes. It should not
define separate business rules.

## MCP and Skills

MCP and skill attachments exist at three scopes:

- profile
- project
- session

Attachments record:

- scope and target ids
- server or skill id
- materialized path/state
- attachment status
- restart requirement

Effective materialization for a session is the merge of attached profile,
project, and session records. The materialization module produces stable JSON so
the CLI, API, and tests can compare output deterministically.

Project-scoped MCP/skill operations are trust-gated. Detaching marks records
detached and usually makes restart required. Sync recomputes materialized state
and clears restart flags for attached records.

## Watchers

Watchers are stored configs that can receive or poll external events. Current
records track:

- profile
- optional project id
- adapter id
- config ref
- status
- backoff state
- timestamps/version

Watcher events store normalized source, event type, payload reference, signature
status, route decision, delivery flag, and timestamp.

Important constraints:

- Project watchers are trust-gated.
- Ingested payloads are references/strings, not arbitrary executable behavior.
- Polling running watchers records events and updates watcher state.
- Watcher routes should stay controller-owned so CLI/API behavior matches.

## Conductors

A conductor is a session that coordinates work for other sessions.

Conductor records track:

- conductor session id
- lifecycle status
- watched session list
- channel bindings
- last heartbeat
- timestamps/version

Assignments track:

- conductor id
- target session id
- task ref
- status
- assigned/completed timestamps

Assignments feed deck status derivation. For example, queued or assigned work can
make an otherwise running session appear queued/waiting in the dashboard.

## Costs

Costs are append-only events tied to sessions. A cost event contains:

- session id
- amount in USD
- JSON payload with model, source, token counts, etc.
- timestamp

Summary filters:

- profile
- project
- group
- session
- agent
- model
- start/end time
- include archived or active only

Store summary converts dollar amounts into micros for stable integer totals.

## Search, Diff, and Structured Events

Search:

- Searches session metadata.
- Searches captured output where available.
- Searches known transcript formats for supported agents when files can be
  located.
- Returns session id, name, group, agent, cwd, source, and snippet.

Diff:

- Diffs are generated from the session workspace path with git.
- Diff generation is on demand.
- The CLI/API return plain diff text wrapped as needed by the surface.

Structured events:

- Raw session events are normalized by the agent registry.
- API exposes both snapshot and stream routes.
- Structured rendering should prefer normalized events over terminal scraping.

## Security and Safety

Read-only mode:

- Controller rejects writes.
- HTTP API also rejects mutation methods through a request guard.
- Runtime/status reconciliation must not write to the store in read-only mode.

Project trust:

- Hooks, project MCP, project skills, and watchers require trusted projects.
- Trust state is explicit and stored per project.

Sandbox paths:

- Paths are canonicalized.
- Requested sandbox paths must be under an allowed path.
- Missing paths fail validation.

Redaction:

- Event payloads are redacted before being stored when they contain common secret
  keys or bearer tokens.
- Cost/event exports should not bypass store redaction.

Web access:

- Default bind is loopback.
- Token auth is available and should be used for exposed listeners.
- Read/write mode should be obvious before exposing a server beyond loopback.

## Testing and Verification

Standard checks:

```bash
cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features
AGENT_HELM_TMUX_SMOKE=1 cargo test --all-features terminal_preview_smoke_test
./scripts/harness.sh
git diff --check
```

The harness uses temporary state and exercises CLI/API/runtime behavior. Tmux
smoke tests are gated by environment variables so normal unit tests do not
require a real tmux server.

Test layers:

- Store tests cover migrations, CRUD, locking-adjacent behavior, redaction,
  costs, and queries.
- Controller tests cover lifecycle, worktrees, trust gates, watchers,
  conductors, search, and state sync.
- Runtime tests cover fake runtime always and tmux runtime when smoke is enabled.
- Terminal UI tests cover rendering, grouped navigation, preview behavior,
  embedded session behavior, mouse resize/scroll, forms, and settings.
- API tests cover route behavior, auth/read-only behavior, and response shape.

## Reproduction Invariants

These are the details that are easy to miss when rebuilding Agent Helm:

- Sessions must outlive UI processes; tmux is the current mechanism.
- All surfaces must go through the controller for writes.
- The store must be per-profile and protected by an app-level lock file.
- Runtime operations must not hold store locks.
- Read-only mode must not persist status reconciliation.
- Session status and deck status are different concepts.
- Group names are path-like strings; collapsed parent groups hide descendants.
- Project trust gates hooks, project materialization, and watchers.
- Worktree cleanup must refuse worktrees attached to live session records.
- Agent launch commands may be wrapped to emit agent-state events.
- Running-session output in the dashboard should come from preview capture, not
  synchronous controller output loading.
- Terminal preview capture must run off the UI thread.
- Preview requests/results must be generation checked and stale-safe.
- Tmux capture output has a final newline; trim it before terminal parsing.
- Normalize lone newlines before terminal parsing or text starts in the wrong
  column.
- Preview and focused session should use the same panel split.
- Preview capture should size tmux to the preview viewport before reading; if
  this only happens on focused attach, the initial preview can drop top rows.
- Full-screen terminal apps can hide the cursor; focused embedded mode should
  draw a cursor overlay.
- The browser UI is optional and should remain a local API client.
- Stable materialization JSON is required for MCP/skill reproducibility.
- Cost records are append-only; summaries are derived.
