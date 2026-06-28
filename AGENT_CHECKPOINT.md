# Agent Checkpoint

Purpose: keep enough state to resume if an agent run fails midway.
Last updated: 2026-06-26 America/Los_Angeles

## Current Work

- Task: Rust-only Agent Deck parity slice.
- Status: in progress.
- Completed:
  - Re-read repo state and preserved existing dirty TUI/checkpoint edits.
  - Fixed `remove --cleanup-worktree` so it finishes the recorded worktree.
  - Added shared `session search` plus `/api/search` over session metadata, events, live output snippets, and Claude/Codex JSONL transcripts.
  - Added Agent Deck-shaped `session` lifecycle namespace wrappers: `start`, `stop`, `restart`, `remove`, `fork`, `attach`, `show`, `status`, `send`, `output`, `diff`, `search`.
  - Added first-class group lifecycle behavior in the Rust controller/store/API/CLI: create, update, delete/remove, list, and move.
  - Added archived session restore in store/controller/API/CLI via `session restore` and `/api/sessions/:id/restore`.
  - Added `/api/sessions/:id/start` as the HTTP alias for restart/start behavior.
  - Exposed browser dashboard controls for start, archive, restore, and archived-session filtering.
  - Exposed browser dashboard search using existing `/api/search`.
  - Exposed browser dashboard cost summary using existing `/api/costs`.
  - Exposed browser dashboard group filtering, group create/delete, and selected-session group moves.
 - Exposed browser dashboard MCP/skill attachment forms and attachment tab using existing APIs.
 - Exposed browser dashboard selected-session worktree metadata using existing worktree APIs.
 - Exposed browser dashboard watcher start form and watcher/conductor manager tab.
  - Added HTTP conductor setup via `POST /api/sessions/:id/conductor` and switched API harness coverage away from CLI fallback.
  - Fixed tokened web dashboard bootstrap so `/` is browser-loadable while `/api/*` remains token-protected.
  - Fixed browser API helper for `204 No Content` responses used by group deletion.
  - Added browser dashboard selected-session conductor setup form.
  - Added HTTP project trust/untrust/remove, worktree finish/cleanup routes, browser project/worktree controls, and harness coverage for API project trust/worktree cleanup.
  - Conductor send now delivers the task to the target session through the normal input path, with CLI/API harness output checks.
  - Added profile/project-scoped MCP and skill attach paths in controller/CLI/API, with sync coverage for session/project/profile scopes.
  - Adapter capabilities now distinguish command-only shell/custom sessions from MCP/skill-capable agent sessions; session-scoped tool attaches reject unsupported adapters.
  - Fixed TUI archived-session parity by loading archived records, preserving the hidden-by-default filter, and adding archive/restore actions.
  - Fixed TUI preview refresh so output/diff/structured panes update while the selection stays unchanged.
 - TUI create form now uses a muted generated name placeholder, visible configured-agent selector, absolute default path, worktree checkbox, and no command/sandbox/prompt fields.
 - TUI now uses a higher-contrast dark palette, shows derived deck-status words in the session list, and Enter focuses the embedded session panel while shrinking the sidebar instead of attaching full-screen.
 - Renamed Carry UI to Copy current state, made it a default-off checkbox in TUI create/fork forms, and updated the browser label.
 - Status snapshots now carry optional activity metadata from existing `agent_state` events, with tool labels surfaced in TUI session rows and the browser selected-session status pill.
 - Browser dashboard now auto-refreshes visible session status/activity snapshots on a guarded timer without replacing active search results.
 - Gemini transcript `toolCalls[].status` activity parsing now maps executing to running, awaiting approval to waiting, and success to idle/done; OpenCode remains unimplemented until a local schema is grounded.
 - Browser dashboard session list now renders grouped tree rows from `/api/groups` plus visible session status snapshots, including persisted collapsed-group state.
 - Browser dashboard grouped list now clears hidden selected-session preview when a collapsed group hides the session, and group rows toggle persisted collapsed state.
 - Browser dashboard session rows now show explicit PR badges (`#12345`) from session name/group/path, matching TUI behavior.
 - Browser dashboard now serves `/s/:id` deep links, pushes selected sessions into browser history, and restores/clears selection on popstate.
 - Browser dashboard now has a Ctrl+K command palette for tab switching, refresh, new-session focus, and loaded-session jumps.
 - Browser dashboard now has a discoverable keyboard-shortcuts overlay opened with `?` and closed with Esc/click.
- Browser dashboard now supports `j`/`k` visible-session navigation, Enter to open current selection, and Shift+Enter to open the session route in a new tab.
- Browser dashboard now confirms destructive archive/remove session, delete group, and remove project actions through a reusable modal, harness coverage.
- Browser dashboard now also confirms watcher/conductor removals and supports `a`/`t` archived/status filter hotkeys, harness coverage.
- Browser dashboard now shows fleet deck-status summary counts and selected-session context line with agent/group/path/workspace/worktree metadata, harness coverage.
- CLI now exposes `session structured-events` matching the API snapshot shape for structured session events, harness coverage.
- Controller event ingestion now accepts Gemini transcript-shaped `agent_state` payloads without top-level state so existing Gemini tool-status activity derivation is reachable, focused coverage.
- TUI status refresh now polls archive/query-eligible sessions before status filtering, so newly matching sessions can appear under active deck-status filters.
- TUI selected-session actions and sends now invalidate stale deck-status/detail caches immediately after mutation.
- Archive/delete now stop live runtimes detected by runtime status even when stored `runtime_id` metadata is missing, focused controller coverage.
- Archived sessions now reject restart until restored, preventing hidden archived-running sessions.
- Tmux startup now best-effort kills the just-created tmux session if viewport configuration fails.
 - Delete cleanup-worktree now persists stopped runtime state before worktree teardown, so teardown failures do not leave stale running runtime ids.
  - Worktree carry-state now copies ignored files as well as tracked and untracked files, with temp-git coverage.
  - Made tmux sends literal-safe and made tmux stop/destroy idempotent when sessions were already removed.
  - Preserved `errored` session state across runtime status refreshes that report stopped.
  - Archived running sessions now clear runtime id and become stopped before restore.
  - Added store initialization on MCP/skill/watcher/conductor/cost read paths for fresh profiles.
  - Fixed project cost filtering to use session `project_id`, so worktree session costs still count for the parent project.
  - Honored `ForkSessionRequest.start_immediately` in controller/API/CLI; `session fork --no-start` now creates a stopped child session.
  - Added CLI cost-summary filters for project, group, session, agent, model, time range, and active-only views using the existing store summary.
  - Added harness coverage for CLI/API group create/update/delete.
  - Added harness coverage for CLI/API archive/restore round trips.
  - Added harness coverage for browser archive/restore controls and archived filter presence.
  - Added harness coverage for browser search form presence.
  - Added harness coverage for browser costs tab presence.
  - Added harness coverage for browser group filter/create/delete and move form presence.
 - Added harness coverage for browser MCP/skill forms and attachment tab presence.
 - Added harness coverage for browser worktrees tab presence.
 - Added harness coverage for browser watcher form and manager tab presence.
  - Added harness coverage for CLI/API fork-without-start behavior.
- Added shell watcher adapter execution: validates JSON config, runs configured command under trusted project root, records output/error watcher events, and exposes CLI/API watcher create paths.
- Added harness coverage for CLI/API shell watcher create/start behavior.
- Added shared watcher event listing through controller, CLI `watcher events`, and `GET /api/watchers/:id/events`.
- Added harness coverage for CLI/API watcher event output inspection.
  - Updated README command examples for session namespace and group lifecycle commands.
- Added API watcher test route via `POST /api/watchers/:id/test`.
- Exposed browser dashboard watcher create/test/events controls in manager tab.
- Browser watcher event reads now expose offset/limit inputs and send them as query params to `GET /api/watchers/:id/events`, while staying available in read-only mode.
- Added harness coverage API watcher test and browser watcher create/test/events controls.
- Exposed browser dashboard worktree creation using existing project worktree API.
- Added harness coverage browser worktree create form presence.
- Added shell watcher `session_id` routing so successful watcher output is sent through existing session input handling.
- Added unit and CLI/API harness coverage for watcher output delivery to sessions.
- Added shell watcher `timeout_ms` config so long-running commands fail bounded, record timeout events, and mark watcher errored.
- Added unit coverage for timed-out shell watchers.
- Exposed browser watcher create fields for selected-session routing and optional `timeout_ms`.
- Added harness coverage browser watcher timeout input presence.
- Exposed browser project removal using existing `DELETE /api/projects/:id` route.
- Added harness coverage browser project remove control presence.
- Exposed browser project registration using existing `POST /api/projects` route.
- Added harness coverage browser project add form presence.
- Added watcher poll operation for running watchers across controller, CLI, API, and browser controls.
- Added unit and CLI/API harness coverage that poll appends another watcher output event.
- Added watcher poll-all operation for all running watchers across controller, CLI, API, and browser controls.
- Added unit and CLI/API harness coverage that poll-all appends output only for running watchers.
- Added project-scoped watcher listing through controller, CLI `watcher list --project`, API `GET /api/projects/:id/watchers`, and selected-session manager view.
- Added unit and CLI/API harness coverage project watcher filtering.
- Exposed browser manager controls for watcher stop and conductor start/heartbeat/send/stop using existing APIs.
- Added CLI `session events` and `conductor heartbeat` wires over existing controller behavior, with harness coverage.
- Fixed worktree creation for existing local branches and added temp-git coverage.
- Added selected-session costs pane to the TUI details modes.
- Switched sandbox Docker launch to interactive TTY mode for tmux-hosted sessions.
- Scoped MCP sync materialization per target profile/project/session instead of writing one global plan, with unit coverage.
- Exposed watcher/conductor removal through controller, CLI, API, browser controls, and CLI/API harness coverage.
- Exposed conductor assignment history through controller, CLI, API, browser form, and CLI/API harness coverage.
- Browser project add now sets project context so project/worktree controls work without selected session.
- Added external watcher event ingestion via `POST /api/watchers/:id/events`, with optional session routing and unit/API harness coverage.
- Added store initialization to watcher creation for fresh profiles.
- Added CLI `watcher ingest` for local long-running adapter processes, with CLI harness coverage.
- External watcher event ingestion now honors project trust gates for project-scoped watchers.
- Added browser project selector so project/worktree controls can target existing registered projects without a selected session.
- Added browser watcher ingest form and project-context watcher creation without requiring selected session.
- Expanded browser MCP/skill controls to support session/project/profile attach, sync, detach, and project-scoped attachment views.
- Fixed browser read-only mode to leave watcher event reads available while disabling watcher event ingestion.
- Browser managers tab now uses selected project context for watcher listing even without selected session.
- Added TUI help mode with `?`/`h` open and Esc/q close, plus render/key tests.
- Added CLI `session create`/`session add` wrapper, `--carry-state` on create paths, and JSON `OutputPage` printing for `output`/`session output`.
- Fresh forks for Claude/Codex/Gemini/OpenCode now use adapter defaults while carry-state forks keep parent commands; shell/custom forks no longer claim conversation inheritance.
- Added controller coverage proving shared search returns live runtime output hits with source `output`.
- Added TUI detail-pane scrolling with PageUp/PageDown, reset on selection/view/search changes, and render/key tests.
- Forking from an existing worktree without requesting a new branch now preserves the parent worktree/project/path association.
- Added TUI status filter cycle with `t`, header status badge, and filter/key tests.
- Replaced TUI quick fork with a fork form for name/group/worktree/carry/start options backed by `ForkSessionRequest`.
- Added browser watcher adapter selector so dashboard can create manual watchers for external event ingestion as well as shell watchers.
- Added derived group tree metadata (`parent`, `depth`) on group records, including legacy `{}` metadata fallback.
- Added TUI collapsed-group tree behavior from persisted group state, plus `c` collapse and `e` expand controls that persist through group updates.
- TUI create now allows empty command values so adapter defaults work for Claude/Codex/Gemini/OpenCode paths.
- Added runtime-reconciled `GET /api/sessions/:id/status` and harness coverage after killing the backing tmux pane.
- `ApplicationController::output` now rejects runtime panes without a recorded session, with unit coverage.
- Watcher actions now resolve unique watcher names as well as ids, with duplicate-name rejection and CLI harness coverage.
- Browser refresh reconciles selected session runtime status through existing `/api/sessions/:id/status`.
- TUI new-session form now pre-fills path and group from selected session context.
- Session list now reconciles active runtime status through the shared controller path, so CLI/API/TUI/web lists do not keep stale running state.
- `ApplicationController::output` now also reconciles runtime status and rejects stale stopped sessions like attach/send.
- Added stale-runtime attach/send/output and list-reconciliation controller coverage plus API list harness coverage.
- Browser group creation now sends optional parent and default project path through the existing group API.
- Browser group updates now send default project path, clear-default-path, and collapsed state through the existing group PATCH API.
- Added raw `GET /api/sessions/:id/events` session event endpoint, API trait wiring, and harness coverage.
- Browser events tab now loads raw session events alongside structured events, harness coverage.
- Browser new-session form now exposes worktree branch, carry-state, and sandbox fields through existing create-session serialization.
- Browser project add now exposes trusted-at-create through existing project API serialization.
- Browser project picker now populates project ids from existing `GET /api/projects`.
- Browser action row now exposes metadata-only session remove through existing `DELETE /api/sessions/:id`.
- Browser fork form now exposes fork name, group, worktree branch, carry-state, and no-start options through the existing fork API.
- TUI collapsed parent groups now hide descendant group sessions and omit descendant group rows, unit coverage.
- TUI post-create/post-fork matching selection now respects collapsed group visibility, unit coverage.
- TUI now supports metadata-only session remove with Delete key backed by existing controller delete behavior, unit coverage.
- TUI idle refresh now reloads session records through the existing action path while preserving selection, unit coverage.
- Worktree cleanup now reuses the teardown-aware finish path for finished worktrees, unit coverage.
- Cleanup teardown coverage now uses the inherent controller worktree method, so no-default-features builds do not depend on the API trait.
- Browser group delete now supports force deletion and API/harness coverage verifies sessions move to default.
- Worktree creation now bases new local branches on matching `origin/<branch>` remote refs before falling back to default branch, unit coverage.
- Session creation now preserves adapter launch-spec failures as errored stored sessions, unit coverage.
- Harness now proves read-only mutation routes reject before malformed JSON body parsing.
- `poll_running_watchers` now rejects read-only controllers before listing watchers, unit coverage.
- CLI `session events` now returns raw `SessionEvent` records matching `/api/sessions/:id/events`; harness asserts `payload.source`.
- Harness now pins browser send form, send API wiring, and read-only send input disabling.
- Browser conductor action controls now include read-only status via `GET /api/conductors/:id`, harness coverage.
- Browser read-only mode now disables conductor mutation controls while keeping conductor status reads enabled.
- Cleanup deletion now preserves shared fork worktrees when any other session, including archived sessions, owns the same worktree, unit coverage.
- Retained session removal now archives sessions as stopped with no runtime id, harness coverage.
- TUI `Delete` uses metadata-only removal and `Shift+Delete` requests worktree cleanup through existing delete mode, key/help coverage.
- `watcher start` now requires an existing watcher instead of implicitly creating state; CLI/API harness paths create watchers first.
- Browser quick fork now selects the returned child session, matching the dedicated fork form.
- Architecture search wording now matches implemented metadata/output/transcript search instead of overclaiming fuzzy search.
- Dashboard harness greps now avoid `pipefail`/SIGPIPE false failures.
- TUI search Enter now calls shared controller search and preserves output/event search hits across idle refresh, unit coverage.
- Worktree list/cleanup now validate project ids before returning empty scoped results, unit and CLI harness coverage.
- Watcher polling now rejects explicit polls for non-pollable manual watchers and skips them in poll-all, unit coverage.
- TUI selected-session actions now preserve active shared-search results after action refreshes, unit coverage.
- Browser session removal now exposes `cleanup_worktree=true` through the existing delete API, harness coverage.
- TUI `m` now opens a one-field move-group form backed by existing move-session controller behavior, bridge and unit coverage.
- Re-adding an existing project with `trusted: true` now upgrades trust state instead of returning the stale untrusted record, unit and API harness coverage.
- `poll_running_watchers` now polls remaining running pollable watchers after one watcher fails, preserving first-error return, unit coverage.
- CLI `costs record` now appends cost events through existing controller cost recording, harness coverage.
- Added read-only session materialization snapshots through shared controller behavior, `session materialization`, and `GET /api/sessions/:id/materialization`, unit/CLI/API harness coverage.
- Session materialization now filters inherited MCP/skill attachments by agent capability, so command-only agents like `shell` do not receive profile/project tool materialization; controller/CLI/API harness coverage.
- Browser worktree finish now uses the existing destructive-action confirmation modal, with harness coverage that the API call is delayed until confirmation.
- Session worktree creation now rolls back just-created physical worktrees when sandbox path validation fails before persisting worktree/session records, temp-git controller coverage.
- Failed carry-state worktree creation now removes the just-created worktree to avoid orphans, workspace unit coverage.
- TUI detail modes now include raw session events backed by existing controller events, key/footer/render coverage.
- Direct worktree finish now rejects worktrees still attached to any session, while session cleanup can still finish its owned worktree, unit coverage.
- TUI selected-session actions now preserve the acted-on session after refreshed rows reorder and reset detail scroll, unit coverage.
- TUI ratatui loop now uses a dirty-redraw scheduler, refreshes sessions/details on a bounded cadence, skips redundant detail reloads, and has focused scheduler coverage.
- TUI Enter on a selected session now opens an embedded session-input panel with live output still visible and keeps input mode after sends instead of only shrinking the sidebar.
- External watcher ingestion now rejects unverified events when watcher config sets `require_signature: true`, preserving existing verified event ingestion.
- Workspace sibling worktree path selection now treats git-registered worktrees as occupied even if their directory is missing, avoiding reuse of stale registered paths.
- CLI `session list`/`session ls` now exists and reuses root list filters/output.
- Browser refresh now retries `/s/:id` route selection when no session is selected, fixing deep-link-before-token selection.
- TUI key handling now avoids forced synchronous detail reloads when the selected session is unchanged, cache-policy coverage.
- Added non-persisted Agent Deck-style status snapshots derived from lifecycle plus recent events/open conductor assignments, exposed through controller, CLI `session status-snapshot`, and `GET /api/sessions/:id/status-snapshot`, unit/CLI/API harness coverage.
- Existing CLI `status`/`session status` and `GET /api/sessions/:id/status` now return derived status snapshots with `deck_status`/`lifecycle_status`, harness coverage.
- Browser selected-session header now renders deck status from status snapshots without changing lifecycle list filtering, harness coverage.
- TUI selected-session summaries now render deck status from status snapshots without changing lifecycle list filtering, render/detail-loader coverage.
- Worktrees now have direct read surfaces through controller, CLI `worktree show`, `GET /api/worktrees/:id`, and browser selected-worktree detail loading, unit/harness coverage.
- Standalone project worktree creation now supports carry-state through controller, CLI `worktree create --carry-state`, API request body, and browser form, with unit/harness coverage.
- Project registration now preserves explicit default branches through CLI `project add --default-branch`, browser add-project form, and API request body, harness coverage.
- Browser costs tab now exposes project, group, session, agent, model, time range, and include-archived filters through existing `/api/costs`, harness coverage.
- Browser costs tab now records selected-session cost events through existing `/api/sessions/:id/costs`, including model/source/token counts and read-only disabling, harness coverage.
- Workspace records now have read surfaces through controller, CLI `workspace show/list`, `GET /api/workspaces/:id`, `GET /api/projects/:id/workspaces`, and browser selected-workspace detail loading, unit/CLI/API/browser harness coverage.
- API store/controller not-found errors now map to HTTP 404 `not_found` instead of 500 `internal_error`, unit/API harness coverage.
- TUI dashboard view now precomputes visible row/session indexes in `DashboardView`; selection preservation scans the cached visible sessions instead of rebuilding the full view per index, unit coverage.
- Tmux runtime status and idempotent kill checks now capture `tmux has-session` output so `cargo run` list/status paths do not print `no server running` when no tmux server exists.
- Configurable tool profiles now support built-in shell/Claude/Codex/Gemini/OpenCode launch commands, custom executable/flags overrides, disabled-tool errors, and per-tool worktree behavior.
- Controller session creation now resolves empty commands through tool profiles and auto-creates worktrees for tools with `worktree = "always"` while keeping shell no-worktree by default and honoring explicit worktree requests.
- Session worktrees now live under sibling `<repo>-worktrees/<normalized-session-name>` directories, including worktrees created from an existing linked worktree, while keeping git branch names unchanged.
- Verified sibling worktree placement, normalized session-name paths, normalized collision suffixes, and linked-worktree base resolution with focused workspace tests.
- Worktree paths now add numeric suffixes on normalized-name collisions, and setup-hook failures roll back dirty physical worktrees before any `WorktreeRecord` is stored.
- Archive/delete/remove now fail before hiding live sessions when runtime destroy fails, and stopped archives report `runtime_stopped: false`, unit coverage.
- TUI now supports `Ctrl-n` quick shell creation, a smaller default sidebar, and mouse dragging on the divider to resize the preview pane.
- Tmux sessions now hide tmux chrome/prefix controls and bind `Ctrl-q` during attach to detach back to Agent Helm, restoring the previous binding afterward.
- TUI new-session agent field is now a selector, remembers the last submitted agent for `n`, and uses capital `N` to duplicate the selected session settings.
- Forks that create a new worktree now use the child session name for the sibling worktree directory while preserving the requested git branch, unit coverage.
- TUI header now shows compact Headroom proxy savings metrics when the configured savings file exists; default path is `~/.headroom/proxy_savings.json`, configurable under `[headroom]`.
- TUI new-session form now shows a muted non-numbered `names` crate-generated placeholder/default, configured profile selector, absolute default path, worktree checkbox, and hides command/sandbox/prompt fields.
- Web dashboard new-session form now uses configured agent selector metadata from `/api/about`, defaults the worktree checkbox from the selected profile, generates worktree branches from session settings, and hides command/sandbox/prompt fields.
- Fresh forks for command-only/custom sessions now preserve the parent command through the adapter fork plan while AI-agent fresh forks still use adapter defaults.
- Web dashboard session list now has a status filter wired to existing `/api/sessions?status=...` support and renders per-row deck status via existing status snapshots.
- Web dashboard attachments tab now includes the selected session's effective materialization plan from `/api/sessions/:id/materialization`.
- Web dashboard output pane now consumes authenticated `/api/sessions/:id/terminal-stream` SSE through `fetch` streaming with `/output` fallback.
- Web dashboard Events tab now streams structured events through authenticated `/api/sessions/:id/structured-stream` while retaining raw event snapshots, with stream and fallback harness coverage.
- Web dashboard Events tab now renders structured/raw events as readable rows instead of a JSON dump, with stream/fallback dashboard smoke coverage.
- Web dashboard costs filter now has an Apply action; search result selection and quick actions refresh selected deck status; clearing selection clears stale panel content.
- Web dashboard session refresh now clears selected-session details when filters hide the selected session, with Node dashboard smoke coverage.
- Web dashboard selected-session actions now expose `sync-state`, posting to the existing transcript state sync endpoint and disabling the action in read-only mode, with harness coverage.
- Web dashboard `/api/about` now exposes tool executable/flags and the new-session agent selector renders selected profile launch/worktree summary, with harness coverage.
- TUI normal mode now exposes uppercase `S` sync-state for selected session, dispatching existing transcript sync then refreshing details, unit coverage.
- TUI selected-session header now shows compact workspace/worktree id, path, and branch context without restoring detail tabs, render coverage.
- Raw session event reads now validate the session id before listing events so missing sessions return not-found instead empty streams, unit coverage.
- CLI `list --status` now preserves lifecycle filtering and also matches Agent Deck statuses (`waiting`, `queued`, `idle`, etc.) via status snapshots, unit/harness coverage.
- Adapter deck-status derivation now maps active `agent_state` values (`running`, `busy`, `working`, `thinking`) to `running`, so newer active events override older input/waiting signals.
- API `GET /api/sessions?status=...` now preserves lifecycle filtering and also matches Agent Deck statuses via status snapshots, while explicit `deck_status` remains supported, unit/harness coverage.
- Runtime start/restart/status reconciliation now records `agent_state: running`; successful user sends and initial prompts record `agent_state: waiting`.
- Adapter deck-status derivation now treats open conductor `assigned` assignments as `waiting` only after queued assignments and non-runtime-lifecycle session events, with adapter/controller coverage.
- Conductor assignments can now be completed through controller, CLI `conductor complete`, and `POST /api/conductor-assignments/:id/complete`; completed assignments no longer drive open-assignment status.
- TUI session list now appends colored `#12345` PR badges only for explicit PR tokens in session metadata.
- TUI detail tabs removed; normal mode no longer binds Left/Right for detail modes, and selection changes still reset detail scroll to dashboard.
- TUI detail-page state removed entirely: no `ViewMode` switching, no hidden diff/events/costs panes, and no diff/events/cost preloads during selected-session refresh.
- Supersedes earlier TUI costs/events detail-mode checkpoint notes; current TUI keeps live output/workspace context without detail tabs.
- Browser dashboard stream harness now checks served JavaScript parses SSE blocks and renders terminal event text into the output panel.
- Browser dashboard runtime smoke now executes `startOutputStream` in Node with a fake SSE `ReadableStream`, asserting the terminal-stream URL and rendered output text.
- TUI session list now animates a per-session activity dot for running/starting sessions; stopped and errored sessions remain static.
- TUI visible rows now hydrate typed `SessionDeckStatus` snapshots so queued/waiting/idle drive sidebar markers, group/fleet counts, and status filtering while lifecycle status remains separate.
- TUI sidebar now renders the precomputed group/session tree with ratatui `ListState`, preserving selected-row scroll/highlight and PR badge color.
- Workspace tests now cover existing directory collision suffixing, default-branch base selection, existing local branch content, and setup-hook rollback cleanup.
- README intro updated from first-milestone shell-only wording to current broader controller scope.
- README checks now include `scripts/harness.sh` and `git diff --check`.
- Conductor assignments now have validated terminal statuses (`completed`, `failed`, `cancelled`) through shared controller behavior, CLI `conductor fail/cancel`, `PATCH /api/conductor-assignments/:id`, browser controls, and harness coverage.
- Public session event ingestion now accepts validated `agent_state` events only through shared controller behavior, CLI `session record-event`, `POST /api/sessions/:id/events`, browser controls, raw/structured event surfaces, and harness positive/negative coverage.
- AI-agent launch commands now install adapter-owned `agent_state` wrappers for Claude/Codex/Gemini/OpenCode, shell/custom remain unwrapped, and sandboxed AI sessions retain runtime lifecycle fallback.
- Claude transcript `session sync-state` now parses matching Claude JSONL tool-use records into deduped `agent_state` events, with CLI/API harness coverage.
- Codex transcript `session sync-state` now parses matching Codex JSONL `function_call` records into deduped `agent_state` events, with CLI/API harness coverage.
- Shared `session search` and `/api/search` now include Codex JSONL transcript content alongside Claude transcript content, with unit and CLI/API harness coverage.
- API session listing now supports explicit `deck_status` filtering while preserving lifecycle `status`; browser status filter sends deck status and includes queued/waiting/idle options, with harness coverage.
- AI-agent restart/status reconciliation now records runtime `running` state even when initial launch hooks suppress `runtime_start`, so stale pre-restart waiting events do not dominate derived deck status.
- TUI new-session flow now seeds `n` from configured `default_agent`, continues remembering last submitted agent, and refreshes the worktree checkbox from selected tool profile defaults, unit coverage.
- Browser read-only mode now disables conductor assignment status inputs/buttons alongside other conductor mutations, harness coverage.
- Stopped runtime reconciliation now clears stale `runtime_id` on explicit stop and observed stopped runtime status, unit coverage.
- Web `/api/about` now exposes an absolute default path and browser new-session form seeds its path from that metadata, harness coverage.
- Browser selected-session removal now exposes purge history separately from cleanup-worktree and sends existing API `purge=true`, harness coverage.
- Deck status snapshots now derive from latest status-signal events (`agent_state`/`input`) instead of latest raw event window, so noisy status events do not hide idle/waiting state, unit coverage.
- Web `/api/about` now exposes `names` crate-generated default session names; browser new-session form shows them as placeholder/default instead of a literal value, harness coverage.
- Browser archive action now exposes reason and archived-by metadata and sends existing API archive audit fields, harness coverage.
- Read-only status reconciliation now returns non-persisted runtime status without updating stored sessions or appending runtime status events, unit coverage.
- Transcript `sync-state` dedupe now checks recent `agent_state` events directly instead of the raw event window, so noisy status events do not cause duplicate transcript syncs, unit coverage.
- CLI `costs record` now accepts `--total-tokens` and `--source`, matching API/web cost recording and preserving explicit total-token summaries.
- API `/api/costs` harness coverage now asserts the active-profile summary returns the recorded cost event, guarding profile scoping.
- Project removal now rejects projects still referenced by sessions, workspaces, or worktrees instead of leaving stale session project metadata; unreferenced project removal remains covered.
- Session diff now includes status, unstaged changes, staged changes, and untracked file diffs while preserving the existing text response shape; controller and API harness coverage.
- Cost events now have a read surface through store/controller, CLI `costs events`, API `/api/cost-events`, and the browser costs panel, with filter-aware harness coverage.
- Configured custom tool profile names now launch through the command-backed custom adapter fallback instead of failing as unsupported, with CLI/API harness coverage.
- Project removal now also rejects live project-scoped MCP attachments, skill attachments, and watchers before store cascades can remove them.
- Project registration now updates an existing project's default branch when re-added with an explicit non-empty `default_branch`.
- Worktree name normalization now keeps dot-only session names inside sibling `<repo>-worktrees/session` instead of allowing `.`/`..` path components, with focused temp-git coverage.
- API cost summary/event routes now include archived sessions by default to match CLI/web, support `active_only=true` for active-only views, and browser unchecked archived filters send that explicit active-only query.
- API cost recording now maps shared cost validation failures to `400 bad_request`, so negative/non-integer token counts are rejected consistently across CLI/API/web paths.
- Browser cost recording now validates optional token fields as non-negative integers before posting, preventing invalid input from being serialized as JSON `null` and recorded as zero.
- Browser cost filters now treat any explicit project/group/session/agent/model/time field as global filter intent, so selected sessions are only auto-scoped when no cost filters are set.
- TUI header/filter counts now include queued/waiting/idle deck statuses while preserving Headroom metrics space, and archived-hidden count is zero when archived sessions are shown.
- TUI help/footer now documents current hotkeys including `Ctrl-n`, `N`, `Delete`, `Shift+Delete`, and page scrolling.
- TUI `Ctrl-n` quick shell creation now uses generated session names and the selected/default absolute path instead of hardcoded `shell`/`.` values.
- Worktree finish now force-removes git worktrees after teardown hooks create untracked files, while direct remove keeps non-force behavior; temp-git regression coverage added.
- Browser watcher create/ingest now exposes signed external event controls: `require_signature` on watcher config and `signature_status` on ingestion, with read-only disabling and API harness coverage.
- OpenCode activity audit found no repo-local transcript/event schema; parser remains generic `agent_state` only until a grounded schema is available.
- TUI Enter session mode now embeds the live tmux session in the Ratatui panel through a PTY/vt100 viewport; keys forward into tmux while focused, and `Ctrl-q` returns to the dashboard.
- Bumped Ratatui/Crossterm to the Bosun-compatible terminal stack and added `portable-pty`/`vt100` for the embedded tmux viewport.
- TUI dashboard preview for the selected running session now uses a throttled read-only tmux `capture-pane` snapshot rendered through the same vt100 renderer, so preview no longer mirrors stale text output.
- Normalized bare line feeds from `tmux capture-pane` before vt100 parsing so preview lines return to the left edge instead of stair-stepping.
- TUI terminal preview now crops from the bottom of the captured tmux screen so it shows the most recent history instead of the first captured rows.
- TUI session preview now supports mouse wheel scrolling over the preview pane, with scroll state reset when changing selection.
- Verification:
- Passing: `cargo fmt`.
- Passing: `cargo fmt --check`.
- Passing: `cargo check --all-features`.
- Passing: `cargo check --features serve`.
- Passing: `cargo test cost_validation_errors_are_client_errors --features serve`.
- Passing: `cargo test cost_queries_include_archived_by_default_with_active_only_override --features serve`.
- Passing: `cargo test cost --all-features`.
- Passing: `cargo test --features serve`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test archived_is_hidden_by_default --all-features`.
- Passing: `cargo test footer_omits_detail_tab_keys --all-features`.
- Passing: `cargo test ctrl_n --all-features`.
- Passing: `cargo test --all-features tui::tests`.
- Passing: `cargo test named_worktree_dot_names_stay_under_repo_sibling_directory --all-features`.
- Passing: `cargo test worktree --all-features`.
- Passing: `cargo check --no-default-features`.
- Passing: `cargo test`.
- Passing: `cargo test --all-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `cargo test ai_agent_restart_runtime_state_clears_stale_waiting_state --all-features`.
- Passing: `cargo test claude_agent_state --all-features`.
- Passing: `cargo test latest_codex_agent_state_reads_function_call_transcript --all-features`.
- Passing: `cargo test search_codex_transcripts_finds_jsonl_content --all-features`.
- Passing: `cargo test configured_default_agent_seeds_new_session_form --all-features`.
- Passing: `cargo test agent_selector_refreshes_profile_worktree_default --all-features`.
- Passing: `cargo test status_clears_stale_runtime_id_when_runtime_stopped --all-features`.
- Passing: `cargo test lifecycle_with_fake_runtime --all-features`.
- Passing: `cargo test list_sessions_reconciles_active_runtime_status --all-features`.
- Passing: `cargo test new_session_remembers_last_selected_agent --all-features`.
- Passing: `cargo test status_snapshot_uses_latest_status_signal_beyond_raw_event_window --all-features`.
- Passing: `cargo test read_only_status_reconciliation_does_not_persist_runtime_changes --all-features`.
- Passing: `cargo test transcript_sync_dedupe_ignores_noisy_raw_event_window --all-features`.
- Passing: `cargo test named_worktree_uses_normalized_name_in_repo_sibling_directory --all-features`.
- Passing: `cargo test fork_with_branch_uses_child_session_name_for_worktree_path --all-features`.
- Passing: `cargo test archive_failure_keeps_live_runtime_metadata_when_destroy_fails --all-features`.
- Passing: `cargo test delete_failure_keeps_session_when_destroy_fails --all-features`.
- Passing: `cargo test archive_stopped_session_reports_runtime_not_stopped --all-features`.
- Passing: `cargo test raw_events_reject_missing_session --all-features`.
- Passing: `cargo test raw_events_allow_empty_existing_session_window --all-features`.
- Passing: `cargo test sync_state_key_runs_action_and_refreshes_details --all-features`.
- Passing: `cargo test list_status_filter --all-features`.
- Passing: `cargo test session_status_filter_matches_lifecycle_or_deck_status --all-features`.
- Passing: `cargo test dashboard_shows_workspace_and_worktree_context --all-features`.
- Passing: `cargo test --all-features`.
- Passing: `cargo check --all-features`.
- Passing: `cargo check --no-default-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: browser dashboard JavaScript `node --check` extraction after watcher event pagination and structured event row rendering.
- Passing: `./scripts/harness.sh` including configured custom profile launch/about checks, CLI JSON diff envelope, project default-branch re-add, CLI explicit total-token/source recording, CLI/API cost-event listing, API active-profile cost summary, and API diff sections.
- Passing: `cargo test remove_project --all-features`.
- Passing: `cargo test conductor_assignment_terminal_statuses_are_validated --all-features`.
- Passing: `cargo test record_session_event --all-features`.
- Passing: `cargo test agent_state --all-features`.
- Passing: `cargo test --all-features --no-run`.
- Passing: dashboard JavaScript `node --check` extraction.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `git diff --check`.
- Passing: `cargo test --features serve api::tests`.
- Passing: `./scripts/harness.sh`.
- Passing: `cargo test --all-features`.
- Passing: `cargo fmt --check`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `git diff --check`.
- Passing: `cargo test delete_cleanup_worktree_clears_runtime_when_teardown_fails --all-features`.
- Passing: `cargo test worktree --all-features`.
- Passing: `cargo test --features serve api::tests`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --features serve api::tests`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --features serve api::tests`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --all-features tui::tests`.
- Passing: `cargo test --all-features restart_archived_session_requires_restore`.
- Passing: `cargo test --features serve api::tests`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --all-features archive_stops_live_runtime_with_missing_runtime_id`.
- Passing: `cargo test --all-features delete_stops_live_runtime_with_missing_runtime_id`.
- Passing: `cargo check --all-features`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo check --all-features`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --all-features record_session_event_accepts_gemini_transcript_status_payload`.
- Passing: `cargo test --all-features derive_deck_status_reads_gemini_transcript_tool_statuses`.
- Passing: `cargo check --all-features`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --all-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `AGENT_HELM_TMUX_SMOKE=1 cargo test --all-features embedded_tmux_smoke_test`.
- Passing: `AGENT_HELM_TMUX_SMOKE=1 cargo test --all-features terminal_preview_smoke_test`.
- Passing: `cargo fmt --check`.
- Passing: `git diff --check`.
- Passing: `cargo test --all-features shell_session_materialization_omits_inherited_tool_attachments`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --all-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `cargo fmt --check`.
- Passing: `git diff --check`.
- Passing: `cargo test --all-features sandbox_validation_failure_discards_created_session_worktree`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `scripts/harness.sh`.
- Passing: `cargo test --all-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `cargo fmt --check`.
- Passing: `git diff --check`.
- Passing: `cargo test`.
- Passing: `cargo test --all-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `cargo fmt --check`.
- Passing: `git diff --check`.
- Passing: `./scripts/harness.sh`.
- Passing: `cargo test workspace::tests::finish_worktree_removes_after_teardown_hook_creates_untracked_file`.
- Passing: `cargo test external_watcher_event_requires_verified_signature_when_configured --all-features`.
- Passing: `bash -n scripts/harness.sh`.
- Passing: `cargo test`.
- Passing: `cargo test --all-features`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `cargo fmt --check`.
- Passing: `git diff --check`.
- Passing: `./scripts/harness.sh`.
- Passing: `cargo test --all-features render_session_terminal`.
- Passing: `cargo test --all-features session_mode_ctrl_q_returns_to_dashboard`.
- Passing: `cargo test --all-features tui::tests`.
- Passing: `cargo test --all-features`.
- Passing: `cargo test`.
- Passing: `cargo clippy --all-targets --all-features`.
- Passing: `cargo fmt --check`.
- Passing: `git diff --check`.
- Passing: `./scripts/harness.sh`.
- Verification blocker cleared: `cargo`, `rtk`, and `target/debug/agent-helm` execute normally again.
- Next step: continue closing Agent Deck parity gaps; full parity is not complete. Active next slices are broader browser parity polish and OpenCode-specific activity fixtures/parsers only if schemas can be grounded.
- Ratatui follow-up scope: table-style row virtualization can wait; current sidebar uses stateful ratatui `ListState` over the precomputed row surface.
- Web follow-up scope: grouped rows, PR badges, output/events streaming, session deep links, command palette, shortcuts overlay, keyboard session navigation, destructive-action confirmation including worktree finish, archived/status hotkeys, fleet status summary, and selected-session context landed; remaining work is broader browser parity polish.
- Runtime/worktree follow-up scope: branch/setup hook edge coverage, delete-cleanup teardown failure state handling, stale runtime metadata teardown, and sandbox-validation worktree rollback landed; remaining work is broader integration behavior around hook lifecycle and cleanup edge cases.
- Status follow-up scope: lifecycle/input-derived `agent_state`, public event ingestion, conductor `assigned`/terminal semantics, launch/exit wrappers, existing agent-state activity labels, grounded Gemini transcript activity, and Gemini transcript event ingestion landed; remaining work is OpenCode state if a schema can be grounded.

- Current next slices: broader browser parity polish; OpenCode-specific transcript state fixtures/parsers only if schemas can be grounded.

## Active Agents

| Agent | Scope | Status | Notes |
| --- | --- | --- | --- |
| Locke | CLI/state parity scout | complete | Found retained remove stale runtime state; stopped/null runtime archive fix landed. |
| Rawls | TUI parity scout | complete | Found missing cleanup delete binding; `Shift+Delete` cleanup mode landed. |
| Nietzsche | API/web parity scout | complete | Found read-only conductor mutation controls stayed enabled; selector/harness fix landed. |
| Copernicus | Runtime/worktree scout | complete | Found shared fork worktree cleanup deletion bug; sibling-owner guard landed. |
| Averroes OpenCode | OpenCode activity schema audit | complete | No repo-local OpenCode transcript/event schema found; no parser invented. |
| Confucius Worktree | Worktree hook cleanup | complete | `finish_worktree` force-removes after teardown hooks create untracked files; focused workspace tests passed. |
| Lagrange Browser | Browser/API signed watcher audit | complete | Recommended signed watcher controls; `require_signature` and `signature_status` browser/API harness coverage landed. |
| Codex coordinator | Implement Rust-only Agent Deck parity slice | in progress | Added cleanup-worktree lifecycle fix, shared search, Claude transcript search, session namespace wrappers, group lifecycle primitives, restore, fork no-start, shell watcher execution, watcher project listing/removal/external ingestion via API/CLI, conductor controls/removal/assignments, TUI costs, existing-branch worktrees, Docker TTY, scoped MCP sync, project-context web controls, TUI persisted group collapse, API status, output record guard, watcher name resolution, browser status refresh, TUI create context, list/output runtime reconciliation, browser nested group creation, worktree project validation, manual watcher poll guard, TUI search-preserving actions, browser cleanup delete control, TUI move-group form, trusted project re-add, poll-all isolation, CLI cost append, session materialization, carry-state rollback, TUI raw events, direct worktree finish guard, TUI action selection preservation, ratatui loop responsiveness, derived status snapshots, status surfaces, worktree read surfaces, workspace read surfaces, API not-found mapping, browser deck-status rendering, TUI deck-status rendering, standalone worktree carry-state, project default-branch registration, and browser cost filters; verification passes. |
| Faraday | Audit long-running watcher adapter parity | complete | Found manual watchers could be polled despite no poll implementation; non-pollable guard landed. |
| Hume | Audit project/worktree web parity | complete | Found browser remove lacked cleanup-worktree option; checkbox/delete flag landed. |
| Franklin | Audit CLI/controller worktree scope parity | complete | Found worktree list/cleanup accepted missing project ids; validation and harness negatives landed. |
| Banach | Audit TUI parity after shared search | complete | Found selected actions discarded active search result sets; refresh preservation landed. |
| Plato | Audit TUI session workflow parity | complete | Found TUI lacked move-to-group despite CLI/controller support; move form landed. |
| Dewey | Audit project/worktree web/API parity | complete | Found trusted re-add ignored existing project state; controller/API harness fix landed. |
| Sartre | Audit long-running watcher adapter parity | complete | Found poll-all stops after first failing pollable watcher; isolation fix landed. |
| McClintock | Audit CLI/state/controller parity | complete | Found CLI cost summary lacks cost-event append command; `costs record` landed. |
| Newton | Audit MCP/skills/materialization parity | complete | Found no read-only session materialization snapshot; controller/API/CLI read path landed. |
| Epicurus | Audit workspace/worktree parity | complete | Found failed carry-state worktree creation left orphan worktrees; rollback fix landed. |
| James | Audit adapter/runtime parity | complete | Found richer Agent Deck waiting/idle/queued status fidelity requires broader model/storage/UI work; no edit. |
| Nietzsche TUI | Audit TUI parity | complete | Raw session events detail mode landed in TUI with key/footer/render coverage. |
| Gauss | CLI watcher create wiring | complete | Added `watcher create` arguments and controller call in `src/main.rs`. |
| Boole | Harness watcher coverage | complete | Added CLI/API shell watcher create/start checks in `scripts/harness.sh`. |
| Heisenberg | CLI watcher event listing | complete | Added `watcher events` command in `src/main.rs`. |
| Tesla | API watcher event listing | complete | Added `GET /api/watchers/:id/events` in `src/api.rs`. |
| Ptolemy | Harness watcher event coverage | complete | Added CLI/API watcher event output checks in `scripts/harness.sh`. |
| Singer | Inspect README/Cargo/src implementation state | complete | Found TUI detail-pane data gap and broader backlog items. |
| Lagrange | Run verification report failures | complete | Found fmt drift and stale cost-summary test filter. |
| Chandrasekhar | Inspect docs/scripts/TODOs for remaining scope | complete | Found stale checkpoint architecture acceptance criteria. |
| Confucius | Audit API/web parity gaps | complete | Found tokened dashboard bootstrap, 204 JSON helper, project/worktree API gaps, and conductor setup gap. Conductor setup/token/204 fixes landed. |
| Poincare | Audit CLI/state/controller parity gaps | complete | Found archive/status/cost/init correctness issues plus broader MCP/watcher/conductor execution gaps. Small state correctness fixes landed. |
| Hegel | Audit runtime/TUI/adapter/worktree parity gaps | complete | Found TUI archived/live-refresh and tmux literal/idempotency issues plus broader adapter/worktree gaps. Small TUI/runtime fixes landed. |
| Linnaeus | Audit CLI/state/controller next gaps | complete | Found missing `conductor heartbeat` and `session events` CLI wires; both landed. |
| Erdos | Audit API/web next gaps | complete | Found missing browser watcher stop and conductor action controls; both landed. |
| Mendel | Audit TUI next gaps | complete | Found missing selected-session costs pane; landed. |
| Archimedes | Audit runtime/worktree next gaps | complete | Found existing-branch worktree and Docker TTY gaps; both landed. |
| Lovelace | Audit MCP/skills/cost next gaps | complete | Found global MCP materialization bug; scoped MCP sync landed. |
| Kuhn | Audit project/worktree web next gaps | complete | Found project add lacked reusable project context; landed. |
| Godel | Audit long-running watcher next gap | complete | Found external watcher event ingestion as smallest daemon-free adapter slice; landed. |
| Bacon | Audit CLI/state/TUI next gaps | complete | Found TUI adapter-default create gap and group collapse persistence; both landed. |
| Carver | Audit API/web next gap | complete | Found missing runtime-reconciled API session status; landed. |
| Bernoulli | Audit runtime/adapter/worktree next gap | complete | Found `output` should require recorded session; landed. |
| Ramanujan | Audit CLI/TUI/state next gap | complete | Found TUI create selected-session context gap; landed. |
| Galileo | Audit API/web next gap | complete | Found browser refresh should reconcile selected status; landed. |
| Averroes | Audit runtime/adapter/worktree next gap | complete | Found stale-runtime attach/send coverage gap; kept as next small test-only candidate. |
| Singer | Audit CLI/TUI/controller next gap | complete | Found list-session runtime reconciliation gap; landed. |
| Laplace | Audit API/web next gap | complete | Found browser nested group create fields gap; landed. |
| Darwin | Audit runtime/adapter/worktree next gap | complete | Found output stale-runtime reconciliation gap; landed. |
| Euclid | Audit TUI latest UX parity | complete | Found current TUI already covers latest requested Ratatui/create/sidebar/session-list requirements; no edit. |
| Feynman | Audit API/web latest parity gap | complete | Found worktree finish skipped destructive confirmation; confirmation and harness smoke landed. |
| Tesla OpenCode | Audit OpenCode activity schema | complete | Found no repo-local OpenCode transcript schema/fixtures, so parser remains intentionally unimplemented. |
| Chandrasekhar refresh | Audit API/web route polish | complete | Found dashboard refresh skipped pending `/s/:id` deep-link selection; retry and harness smoke landed. |
| Turing | Audit workspace lifecycle gap | complete | Found missing-directory registered worktree paths could be reused; stale registered path skip landed. |
| Descartes | Audit CLI parity gap | complete | Added `session list`/`session ls` with root list filters/output. |

## Resume Notes

- Read this file first after any failure context reset.
- Run `rtk git status --short` before editing.
- Do not revert unrelated dirty TUI changes.
- If test execution SIGKILLs again, check `log show --style compact --last 3m --predicate 'process == "agent-helm" OR eventMessage CONTAINS[c] "agent-helm"'`.
