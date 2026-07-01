use crate::{
    config::{ToolProfile, ToolWorktreeBehavior},
    error::{AppError, Result},
    models::{
        CreateSession, DeleteMode, ForkSessionRequest, GroupRecord, GroupSettingsUpdate,
        SessionActivity, SessionDeckStatus, SessionRecord, SessionStatus, now_ts,
    },
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use names::{Generator, Name};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
    },
};
use serde::Deserialize;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ACTIVITY_ANIMATION_INTERVAL: Duration = Duration::from_millis(250);
const SESSION_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const DETAILS_REFRESH_DEBOUNCE: Duration = Duration::from_millis(125);
const PREVIEW_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const PREVIEW_SCROLL_LINES: u16 = 1;
const PREVIEW_MAX_SCROLL_LINES: u16 = 5000;
const EMBEDDED_SCROLL_LINES: usize = 1;
const EMBED_READ_BUF_SIZE: usize = 8192;
const EMBED_MIN_ROWS: u16 = 2;
const EMBED_MIN_COLS: u16 = 10;
#[cfg(test)]
const SGR_MOUSE_WHEEL_UP: u8 = 64;
#[cfg(test)]
const SGR_MOUSE_WHEEL_DOWN: u8 = 65;
const MIN_SIDEBAR_WIDTH: u16 = 8;
const MAX_SIDEBAR_PERCENT: u16 = 50;
const SIDEBAR_LIST_PADDING: u16 = 4;
const DIVIDER_HIT_COLUMNS: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TuiLoop {
    next_session_refresh: Instant,
    next_animation_frame: Instant,
    needs_draw: bool,
}

impl TuiLoop {
    fn new(now: Instant) -> Self {
        Self {
            next_session_refresh: now + SESSION_REFRESH_INTERVAL,
            next_animation_frame: now + ACTIVITY_ANIMATION_INTERVAL,
            needs_draw: true,
        }
    }

    fn poll_timeout(self, now: Instant, details_refresh_due: Option<Instant>) -> Duration {
        let until_refresh = self.next_session_refresh.saturating_duration_since(now);
        let until_animation = self.next_animation_frame.saturating_duration_since(now);
        let until_details = details_refresh_due
            .map(|due_at| due_at.saturating_duration_since(now))
            .unwrap_or(EVENT_POLL_INTERVAL);
        until_refresh
            .min(until_animation)
            .min(until_details)
            .min(EVENT_POLL_INTERVAL)
    }

    fn should_refresh_sessions(self, now: Instant) -> bool {
        now >= self.next_session_refresh
    }

    fn should_animate(self, now: Instant) -> bool {
        now >= self.next_animation_frame
    }

    fn mark_animated(&mut self, now: Instant) {
        self.next_animation_frame = now + ACTIVITY_ANIMATION_INTERVAL;
        self.needs_draw = true;
    }

    fn mark_session_refreshed(&mut self, now: Instant) {
        self.next_session_refresh = now + SESSION_REFRESH_INTERVAL;
        self.needs_draw = true;
    }

    fn mark_changed(&mut self) {
        self.needs_draw = true;
    }

    fn mark_drawn(&mut self) {
        self.needs_draw = false;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusCounts {
    occupied: usize,
    thinking: usize,
    running: usize,
    starting: usize,
    queued: usize,
    waiting: usize,
    idle: usize,
    stopped: usize,
    errored: usize,
}

impl StatusCounts {
    fn add(&mut self, status: SessionDeckStatus) {
        match status {
            SessionDeckStatus::Occupied => self.occupied += 1,
            SessionDeckStatus::Thinking => self.thinking += 1,
            SessionDeckStatus::Running => self.running += 1,
            SessionDeckStatus::Starting => self.starting += 1,
            SessionDeckStatus::Queued => self.queued += 1,
            SessionDeckStatus::Waiting => self.waiting += 1,
            SessionDeckStatus::Idle => self.idle += 1,
            SessionDeckStatus::Stopped => self.stopped += 1,
            SessionDeckStatus::Errored => self.errored += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiSessionStatus {
    pub deck_status: SessionDeckStatus,
    pub activity: Option<SessionActivity>,
}

impl From<SessionDeckStatus> for TuiSessionStatus {
    fn from(deck_status: SessionDeckStatus) -> Self {
        Self {
            deck_status,
            activity: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOperationKind {
    Create,
    Fork,
    Delete,
}

impl PendingOperationKind {
    fn creates_placeholder(self) -> bool {
        matches!(self, Self::Create | Self::Fork)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingOperationState {
    Running,
    Failed(String),
}

#[derive(Debug, Clone)]
struct PendingOperation {
    kind: PendingOperationKind,
    session: SessionRecord,
    state: PendingOperationState,
    running_label: String,
    failed_label: String,
    failure_prefix: String,
    success_message: String,
    expected_name: Option<String>,
    expected_group: Option<String>,
    parent_session_id: Option<String>,
    delete_selection_target: Option<String>,
}

impl PendingOperation {
    fn session_id(&self) -> &str {
        &self.session.id
    }

    fn status(&self) -> TuiSessionStatus {
        match &self.state {
            PendingOperationState::Running => TuiSessionStatus {
                deck_status: SessionDeckStatus::Starting,
                activity: Some(SessionActivity {
                    state: "working".to_string(),
                    label: self.running_label.clone(),
                    source: "tui_pending".to_string(),
                    tool: None,
                }),
            },
            PendingOperationState::Failed(_) => TuiSessionStatus {
                deck_status: SessionDeckStatus::Errored,
                activity: Some(SessionActivity {
                    state: "errored".to_string(),
                    label: self.failed_label.clone(),
                    source: "tui_pending".to_string(),
                    tool: None,
                }),
            },
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.state, PendingOperationState::Running)
    }

    fn is_failed_placeholder(&self) -> bool {
        self.kind.creates_placeholder() && matches!(self.state, PendingOperationState::Failed(_))
    }
}

#[derive(Debug, Clone)]
struct BackgroundActionRequest {
    session_id: String,
    action: TuiAction,
}

#[derive(Debug)]
struct BackgroundActionResult {
    session_id: String,
    result: std::result::Result<Vec<SessionRecord>, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardRow {
    Group {
        group_index: usize,
        name: String,
        collapsed: bool,
        session_count: usize,
        counts: StatusCounts,
    },
    Session {
        session: SessionSummary,
        last_in_group: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardView {
    pub groups: Vec<SessionGroup>,
    pub rows: Vec<DashboardRow>,
    pub visible_sessions: Vec<SessionSummary>,
    pub selected: Option<SessionSummary>,
    pub selected_index: usize,
    pub visible_count: usize,
}

impl DashboardView {
    pub fn build(
        sessions: &[SessionRecord],
        query: &str,
        selected_index: usize,
        status_filter: StatusFilter,
    ) -> Self {
        let collapsed_groups = BTreeSet::new();
        Self::build_with_statuses(
            sessions,
            &BTreeMap::new(),
            &collapsed_groups,
            query,
            selected_index,
            status_filter,
        )
    }

    pub fn build_with_collapsed(
        sessions: &[SessionRecord],
        collapsed_groups: &BTreeSet<String>,
        query: &str,
        selected_index: usize,
        status_filter: StatusFilter,
    ) -> Self {
        Self::build_with_statuses(
            sessions,
            &BTreeMap::new(),
            collapsed_groups,
            query,
            selected_index,
            status_filter,
        )
    }

    fn build_with_statuses(
        sessions: &[SessionRecord],
        deck_statuses: &BTreeMap<String, TuiSessionStatus>,
        collapsed_groups: &BTreeSet<String>,
        query: &str,
        selected_index: usize,
        status_filter: StatusFilter,
    ) -> Self {
        let query = query.trim().to_ascii_lowercase();
        let mut rows = sessions
            .iter()
            .filter(|session| {
                status_filter.matches(
                    deck_statuses
                        .get(&session.id)
                        .map(|status| status.deck_status)
                        .unwrap_or_else(|| lifecycle_deck_status(session.status)),
                )
            })
            .filter(|session| query.is_empty() || matches_query(session, &query))
            .map(|session| SessionSummary::from_record(session, deck_statuses.get(&session.id)))
            .collect::<Vec<_>>();

        rows.sort_by(compare_sessions);

        let visible_sessions = rows
            .iter()
            .filter(|session| !collapsed_group_or_child(&session.group_name, collapsed_groups))
            .cloned()
            .collect::<Vec<_>>();

        let selected_index = match visible_sessions.len() {
            0 => 0,
            len => selected_index.min(len - 1),
        };
        let selected = visible_sessions.get(selected_index).cloned();
        let visible_count = visible_sessions.len();
        let mut groups: Vec<SessionGroup> = Vec::new();

        for row in rows {
            if collapsed_group_child(&row.group_name, collapsed_groups)
                && !collapsed_groups.contains(&row.group_name)
            {
                continue;
            }
            if groups.last().map(|group| group.name.as_str()) != Some(row.group_name.as_str()) {
                groups.push(SessionGroup {
                    name: row.group_name.clone(),
                    collapsed: collapsed_groups.contains(&row.group_name),
                    sessions: Vec::new(),
                });
            }
            groups
                .last_mut()
                .expect("group was just inserted")
                .sessions
                .push(row);
        }

        let mut dashboard_rows = Vec::new();
        for (group_index, group) in groups.iter().enumerate() {
            dashboard_rows.push(DashboardRow::Group {
                group_index,
                name: group.name.clone(),
                collapsed: group.collapsed,
                session_count: group.sessions.len(),
                counts: group_status_counts(group),
            });
            if group.collapsed {
                continue;
            }
            for (session_index, session) in group.sessions.iter().enumerate() {
                dashboard_rows.push(DashboardRow::Session {
                    session: session.clone(),
                    last_in_group: session_index + 1 == group.sessions.len(),
                });
            }
        }

        Self {
            groups,
            rows: dashboard_rows,
            visible_sessions,
            selected,
            selected_index,
            visible_count,
        }
    }
}

fn collapsed_group_or_child(group_name: &str, collapsed_groups: &BTreeSet<String>) -> bool {
    collapsed_groups
        .iter()
        .any(|collapsed| group_name == collapsed || group_is_child_of(group_name, collapsed))
}

fn collapsed_group_child(group_name: &str, collapsed_groups: &BTreeSet<String>) -> bool {
    collapsed_groups
        .iter()
        .any(|collapsed| group_is_child_of(group_name, collapsed))
}

fn group_is_child_of(group_name: &str, collapsed: &str) -> bool {
    group_name
        .strip_prefix(collapsed)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGroup {
    pub name: String,
    pub collapsed: bool,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub group_name: String,
    pub agent: String,
    pub command: String,
    pub project_path: String,
    pub runtime_id: Option<String>,
    pub pr_number: Option<u64>,
    pub status: SessionStatus,
    pub deck_status: SessionDeckStatus,
    pub activity: Option<SessionActivity>,
    updated_at: i64,
}

impl From<&SessionRecord> for SessionSummary {
    fn from(session: &SessionRecord) -> Self {
        Self::from_record(session, None)
    }
}

impl SessionSummary {
    fn from_record(session: &SessionRecord, status: Option<&TuiSessionStatus>) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
            group_name: session.group_name.clone(),
            agent: session.agent.clone(),
            command: session.command.clone(),
            project_path: session.project_path.clone(),
            runtime_id: session.runtime_id.clone(),
            pr_number: session_pr_number(session),
            status: session.status,
            deck_status: status
                .map(|status| status.deck_status)
                .unwrap_or_else(|| lifecycle_deck_status(session.status)),
            activity: status.and_then(|status| status.activity.clone()),
            updated_at: session.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TuiAction {
    Create(CreateSession),
    Attach(String),
    Stop(String),
    Fork(ForkSessionRequest),
    Search {
        query: String,
        limit: usize,
    },
    CreateGroup {
        name: String,
        default_project_path: String,
    },
    UpdateGroup {
        name: String,
        update: GroupSettingsUpdate,
    },
    Remove {
        session_id: String,
        mode: DeleteMode,
    },
    Refresh,
    SetGroupCollapsed {
        group_name: String,
        collapsed: bool,
    },
    MoveToGroup {
        session_id: String,
        group_name: String,
    },
    RenameSession {
        session_id: String,
        name: String,
    },
    SaveToolSettings(Vec<ToolLaunchSettings>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiDetails {
    pub deck_status: SessionDeckStatus,
    pub activity: Option<SessionActivity>,
    pub output: String,
    pub workspace: Option<TuiWorkspaceContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiWorkspaceContext {
    pub workspace_id: String,
    pub workspace_path: String,
    pub worktree_id: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_branch: Option<String>,
}

impl Default for TuiDetails {
    fn default() -> Self {
        Self {
            deck_status: SessionDeckStatus::Stopped,
            activity: None,
            output: String::new(),
            workspace: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeadroomMetrics {
    pub requests: u64,
    pub tokens_saved: u64,
    pub savings_percent: f64,
    pub compression_savings_usd: f64,
}

impl HeadroomMetrics {
    pub fn load(path: &Path) -> Option<Self> {
        let raw = fs::read_to_string(path).ok()?;
        let file: HeadroomSavingsFile = serde_json::from_str(&raw).ok()?;
        file.display_session.or(file.lifetime).map(Into::into)
    }

    fn label(&self) -> String {
        format!(
            " hr {} req {} saved {:.1}% ${:.0}",
            compact_count(self.requests),
            compact_count(self.tokens_saved),
            self.savings_percent,
            self.compression_savings_usd
        )
    }
}

#[derive(Debug, Clone)]
pub struct TuiInitialState {
    pub sessions: Vec<SessionRecord>,
    pub groups: Vec<GroupRecord>,
    pub default_agent: String,
    pub headroom_metrics: Option<HeadroomMetrics>,
    pub agent_choices: Vec<String>,
    pub tool_settings: Vec<ToolLaunchSettings>,
}

#[derive(Debug, Deserialize)]
struct HeadroomSavingsFile {
    display_session: Option<HeadroomSavingsBlock>,
    lifetime: Option<HeadroomSavingsBlock>,
}

#[derive(Debug, Deserialize)]
struct HeadroomSavingsBlock {
    requests: u64,
    tokens_saved: u64,
    #[serde(default)]
    savings_percent: f64,
    #[serde(default)]
    compression_savings_usd: f64,
}

impl From<HeadroomSavingsBlock> for HeadroomMetrics {
    fn from(value: HeadroomSavingsBlock) -> Self {
        Self {
            requests: value.requests,
            tokens_saved: value.tokens_saved,
            savings_percent: value.savings_percent,
            compression_savings_usd: value.compression_savings_usd,
        }
    }
}

pub fn run_tui<F, D, S>(
    initial: TuiInitialState,
    mut load_deck_statuses: S,
    mut load_details: D,
    mut handle_action: F,
) -> Result<()>
where
    F: Fn(TuiAction) -> Result<Vec<SessionRecord>> + Clone + Send + 'static,
    D: FnMut(&str) -> Result<TuiDetails>,
    S: FnMut(&[String]) -> Result<Vec<(String, TuiSessionStatus)>>,
{
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    enable_raw_mode()?;
    if let Err(err) = execute!(terminal.backend_mut(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(err.into());
    }
    if let Err(err) = clear_terminal_screen(&mut terminal) {
        let _ = restore_terminal(&mut terminal);
        return Err(err);
    }
    let mut app = app_from_initial(initial);

    let result = run_app(
        &mut terminal,
        &mut app,
        &mut load_deck_statuses,
        &mut load_details,
        &mut handle_action,
    );
    let cleanup_result = restore_terminal(&mut terminal);
    result?;
    cleanup_result
}

fn clear_terminal_screen(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    execute!(terminal.backend_mut(), TerminalClear(ClearType::All))?;
    Ok(())
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let clear_result = clear_terminal_screen(terminal);
    let raw_result = disable_raw_mode().map_err(AppError::from);
    let screen_result = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .map_err(AppError::from);
    let cursor_result = terminal.show_cursor().map_err(AppError::from);

    clear_result?;
    raw_result?;
    screen_result?;
    cursor_result?;
    Ok(())
}

fn sync_mouse_capture(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    enabled: &mut bool,
    app: &App,
) -> Result<()> {
    let should_enable = wants_mouse_capture(app);
    if should_enable == *enabled {
        return Ok(());
    }
    let command_result = if should_enable {
        execute!(terminal.backend_mut(), EnableMouseCapture)
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture)
    };
    command_result?;
    *enabled = should_enable;
    Ok(())
}

fn wants_mouse_capture(app: &App) -> bool {
    app.mouse_capture && (matches!(app.mode, Mode::Session(_)) || mouse_preview_available(app))
}

fn mouse_preview_available(app: &App) -> bool {
    if !matches!(app.mode, Mode::Normal | Mode::Search) {
        return false;
    }
    if pending_operation_for_selected(app).is_some() {
        return false;
    }
    app.view()
        .selected
        .is_some_and(|session| session_is_live(session.status))
}

struct App {
    sessions: Vec<SessionRecord>,
    collapsed_groups: BTreeSet<String>,
    group_default_paths: BTreeMap<String, String>,
    group_defaults: BTreeMap<String, GroupDefaults>,
    deck_statuses: BTreeMap<String, TuiSessionStatus>,
    pending_operations: BTreeMap<String, PendingOperation>,
    next_pending_operation_id: u64,
    details: TuiDetails,
    details_session_id: Option<String>,
    query: String,
    search_results_active: bool,
    selected_index: usize,
    detail_scroll: u16,
    preview_scroll: u16,
    status_filter: StatusFilter,
    sidebar_width: Option<u16>,
    resizing_sidebar: bool,
    mouse_capture: bool,
    animation_frame: usize,
    last_agent: String,
    headroom_metrics: Option<HeadroomMetrics>,
    agent_choices: Vec<String>,
    tool_settings: Vec<ToolLaunchSettings>,
    preview: Option<TerminalPreview>,
    preview_generation: u64,
    pending_preview: Option<PreviewRequest>,
    pending_details_refresh: Option<PendingDetailsRefresh>,
    embedded: Option<EmbeddedTmux>,
    mode: Mode,
    status_message: Option<String>,
}

fn app_from_initial(initial: TuiInitialState) -> App {
    let agent_choices = normalize_agent_choices(initial.agent_choices);
    let default_agent = normalized_agent(&initial.default_agent);
    let mut collapsed_groups = BTreeSet::new();
    let mut group_default_paths = BTreeMap::new();
    let mut group_defaults = BTreeMap::new();
    for group in initial.groups {
        if group.collapsed {
            collapsed_groups.insert(group.name.clone());
        }
        group_default_paths.insert(group.name.clone(), group.default_project_path.clone());
        group_defaults.insert(group.name.clone(), GroupDefaults::from(&group));
    }
    App {
        sessions: initial.sessions,
        collapsed_groups,
        group_default_paths,
        group_defaults,
        deck_statuses: BTreeMap::new(),
        pending_operations: BTreeMap::new(),
        next_pending_operation_id: 1,
        details: TuiDetails::default(),
        details_session_id: None,
        query: String::new(),
        search_results_active: false,
        selected_index: 0,
        detail_scroll: 0,
        preview_scroll: 0,
        status_filter: StatusFilter::All,
        sidebar_width: None,
        resizing_sidebar: false,
        mouse_capture: true,
        animation_frame: 0,
        last_agent: default_agent,
        headroom_metrics: initial.headroom_metrics,
        agent_choices,
        tool_settings: normalize_tool_settings(initial.tool_settings),
        preview: None,
        preview_generation: 0,
        pending_preview: None,
        pending_details_refresh: None,
        embedded: None,
        mode: Mode::Normal,
        status_message: None,
    }
}

impl App {
    fn view(&self) -> DashboardView {
        DashboardView::build_with_statuses(
            &self.sessions,
            &self.deck_statuses,
            &self.collapsed_groups,
            self.local_filter_query(),
            self.selected_index,
            self.status_filter,
        )
    }

    fn local_filter_query(&self) -> &str {
        if self.search_results_active {
            ""
        } else {
            &self.query
        }
    }
}

fn next_pending_session_id(app: &mut App) -> String {
    let id = format!("tui-pending-{}", app.next_pending_operation_id);
    app.next_pending_operation_id += 1;
    id
}

fn pending_create_session(session_id: String, request: &CreateSession) -> SessionRecord {
    let now = now_ts();
    SessionRecord {
        id: session_id,
        name: request.name.clone(),
        profile: "tui".to_string(),
        group_name: request.group_name.clone(),
        project_id: String::new(),
        workspace_id: String::new(),
        worktree_id: None,
        parent_session_id: request.parent_session_id.clone(),
        agent: request.agent.clone(),
        command: request.command.clone(),
        project_path: request.path.clone(),
        status: SessionStatus::Starting,
        runtime_id: None,
        archived: false,
        version: 0,
        created_at: now,
        updated_at: now,
    }
}

fn pending_fork_session(
    session_id: String,
    request: &ForkSessionRequest,
    parent: &SessionRecord,
) -> SessionRecord {
    let now = now_ts();
    SessionRecord {
        id: session_id,
        name: request
            .name
            .clone()
            .unwrap_or_else(|| format!("{} fork", parent.name)),
        profile: parent.profile.clone(),
        group_name: request
            .group_name
            .clone()
            .unwrap_or_else(|| parent.group_name.clone()),
        project_id: parent.project_id.clone(),
        workspace_id: parent.workspace_id.clone(),
        worktree_id: parent.worktree_id.clone(),
        parent_session_id: Some(parent.id.clone()),
        agent: parent.agent.clone(),
        command: parent.command.clone(),
        project_path: parent.project_path.clone(),
        status: SessionStatus::Starting,
        runtime_id: None,
        archived: false,
        version: 0,
        created_at: now,
        updated_at: now,
    }
}

fn apply_pending_statuses(app: &mut App) {
    for operation in app.pending_operations.values() {
        app.deck_statuses
            .insert(operation.session_id().to_string(), operation.status());
    }
}

fn merge_pending_sessions(app: &mut App) {
    let pending_sessions = app
        .pending_operations
        .values()
        .map(|operation| operation.session.clone())
        .collect::<Vec<_>>();
    for session in pending_sessions {
        if !app
            .sessions
            .iter()
            .any(|existing| existing.id == session.id)
        {
            app.sessions.push(session);
        }
    }
    apply_pending_statuses(app);
}

fn pending_operation_for_selected(app: &App) -> Option<&PendingOperation> {
    let session_id = selected_id(app)?;
    app.pending_operations.get(&session_id)
}

fn block_selected_pending_operation(app: &mut App, action: &str) -> bool {
    let Some(operation) = pending_operation_for_selected(app) else {
        return false;
    };
    if operation.is_running() {
        app.status_message = Some(format!("{action} blocked: {}", operation.running_label));
        return true;
    }
    if operation.kind.creates_placeholder() {
        app.status_message = Some("dismiss failed create with d".to_string());
        return true;
    }
    false
}

fn find_session_record(app: &App, session_id: &str) -> Option<SessionRecord> {
    app.sessions
        .iter()
        .find(|session| session.id == session_id)
        .cloned()
}

struct EmbeddedTmux {
    target: String,
    parser: vt100::Parser,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewRequest {
    generation: u64,
    session_id: String,
    target: String,
    rows: u16,
    cols: u16,
    scroll: u16,
}

impl PreviewRequest {
    fn matches(&self, session_id: &str, target: &str, rows: u16, cols: u16, scroll: u16) -> bool {
        self.session_id == session_id
            && self.target == target
            && self.rows == rows.max(1)
            && self.cols == cols.max(1)
            && self.scroll == scroll.min(PREVIEW_MAX_SCROLL_LINES)
    }
}

struct PreviewResult {
    request: PreviewRequest,
    preview: TerminalPreview,
    #[cfg(test)]
    capture_duration: Duration,
}

struct PreviewWorker {
    requests: Sender<PreviewRequest>,
    results: Receiver<PreviewResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingDetailsRefresh {
    due_at: Instant,
    force: bool,
}

struct TerminalPreview {
    session_id: String,
    target: String,
    parser: Option<vt100::Parser>,
    error: Option<String>,
    refreshed_at: Instant,
    rows: u16,
    cols: u16,
    scroll: u16,
}

impl TerminalPreview {
    fn capture(session_id: String, target: String, rows: u16, cols: u16, scroll: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let scroll = scroll.min(PREVIEW_MAX_SCROLL_LINES);
        let capture_rows = rows.saturating_add(scroll).max(rows);
        sync_tmux_preview_window_size(&target, rows, cols);
        let result = capture_tmux_preview(&target, capture_rows, cols);
        Self::from_capture_result(
            session_id,
            target,
            rows,
            cols,
            scroll,
            result,
            Instant::now(),
        )
    }

    fn from_capture_result(
        session_id: String,
        target: String,
        rows: u16,
        cols: u16,
        scroll: u16,
        result: std::result::Result<vt100::Parser, String>,
        refreshed_at: Instant,
    ) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let scroll = scroll.min(PREVIEW_MAX_SCROLL_LINES);
        match result {
            Ok(parser) => Self {
                session_id,
                target,
                parser: Some(parser),
                error: None,
                refreshed_at,
                rows,
                cols,
                scroll,
            },
            Err(error) => Self {
                session_id,
                target,
                parser: None,
                error: Some(error),
                refreshed_at,
                rows,
                cols,
                scroll,
            },
        }
    }

    fn is_current(
        &self,
        session_id: &str,
        target: &str,
        rows: u16,
        cols: u16,
        scroll: u16,
    ) -> bool {
        self.session_id == session_id
            && self.target == target
            && self.rows == rows.max(1)
            && self.cols == cols.max(1)
            && self.scroll == scroll.min(PREVIEW_MAX_SCROLL_LINES)
    }

    fn is_stale(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.refreshed_at) >= PREVIEW_REFRESH_INTERVAL
    }
}

impl PreviewWorker {
    fn spawn() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
        let (result_tx, result_rx) = mpsc::channel::<PreviewResult>();

        thread::spawn(move || {
            while let Ok(mut request) = request_rx.recv() {
                while let Ok(latest) = request_rx.try_recv() {
                    request = latest;
                }

                let started_at = Instant::now();
                let preview = TerminalPreview::capture(
                    request.session_id.clone(),
                    request.target.clone(),
                    request.rows,
                    request.cols,
                    request.scroll,
                );
                #[cfg(test)]
                let capture_duration = started_at.elapsed();
                #[cfg(not(test))]
                let _ = started_at.elapsed();

                let result = PreviewResult {
                    request,
                    preview,
                    #[cfg(test)]
                    capture_duration,
                };
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        });

        Self {
            requests: request_tx,
            results: result_rx,
        }
    }
}

impl EmbeddedTmux {
    fn spawn(target: String, rows: u16, cols: u16) -> Result<Self> {
        let rows = rows.max(EMBED_MIN_ROWS);
        let cols = cols.max(EMBED_MIN_COLS);
        reset_tmux_window_size(&target);
        enable_tmux_mouse(&target)?;

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| AppError::msg(format!("open embedded terminal: {err}")))?;

        let mut command = CommandBuilder::new("tmux");
        command.arg("attach-session");
        command.arg("-t");
        command.arg(&target);
        command.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|err| AppError::msg(format!("attach embedded tmux session: {err}")))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| AppError::msg(format!("clone embedded terminal reader: {err}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| AppError::msg(format!("open embedded terminal writer: {err}")))?;
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = stop.clone();
        thread::Builder::new()
            .name(format!("agent-helm-embed-{target}"))
            .spawn(move || {
                let mut buf = [0u8; EMBED_READ_BUF_SIZE];
                loop {
                    if stop_reader.load(AtomicOrdering::Relaxed) {
                        break;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(|err| AppError::msg(format!("spawn embedded terminal reader: {err}")))?;

        let mut parser = vt100::Parser::new(rows, cols, 0);
        if let Some(snapshot) = capture_tmux_snapshot(&target, rows) {
            process_captured_terminal_bytes(&mut parser, &snapshot);
        }

        Ok(Self {
            target,
            parser,
            master: pair.master,
            writer,
            child,
            rx,
            stop,
            rows,
            cols,
        })
    }

    fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(bytes) = self.rx.try_recv() {
            self.parser.process(&bytes);
            changed = true;
        }
        changed
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(EMBED_MIN_ROWS);
        let cols = cols.max(EMBED_MIN_COLS);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn write_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(bytes) = encode_embedded_key(key, self.parser.screen().application_cursor()) {
            self.write_bytes(&bytes)?;
        }
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if !bytes.is_empty() {
            self.writer
                .write_all(bytes)
                .map_err(|err| AppError::msg(format!("write embedded terminal input: {err}")))?;
            self.writer
                .flush()
                .map_err(|err| AppError::msg(format!("flush embedded terminal input: {err}")))?;
        }
        Ok(())
    }

    fn render(&self, buf: &mut Buffer, area: Rect) {
        render_vt100_screen(self.parser.screen(), buf, area);
    }

    fn render_hidden_cursor(&self, buf: &mut Buffer, area: Rect) {
        if self.parser.screen().hide_cursor() {
            render_vt100_cursor_overlay(self.parser.screen(), buf, area);
        }
    }

    fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if self.parser.screen().hide_cursor() || area.width == 0 || area.height == 0 {
            return None;
        }
        let (row, col) = self.parser.screen().cursor_position();
        if row >= area.height || col >= area.width {
            return None;
        }
        Some((area.x + col, area.y + row))
    }
}

impl Drop for EmbeddedTmux {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Relaxed);
        let _ = self.child.kill();
    }
}

fn capture_tmux_snapshot(target: &str, rows: u16) -> Option<Vec<u8>> {
    Command::new("tmux")
        .arg("capture-pane")
        .arg("-p")
        .arg("-e")
        .arg("-J")
        .arg("-t")
        .arg(target)
        .arg("-S")
        .arg(format!("-{rows}"))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

fn capture_tmux_preview(
    target: &str,
    rows: u16,
    cols: u16,
) -> std::result::Result<vt100::Parser, String> {
    if let Ok(bytes) = run_tmux_preview_capture(target, rows, true)
        && let Some(parser) = terminal_preview_parser_from_captures(Some(bytes), None, rows, cols)
    {
        return Ok(parser);
    }

    run_tmux_preview_capture(target, rows, false)
        .map(|bytes| terminal_parser_from_bytes(&bytes, rows, cols))
}

fn run_tmux_preview_capture(
    target: &str,
    rows: u16,
    alternate_screen: bool,
) -> std::result::Result<Vec<u8>, String> {
    let mut command = Command::new("tmux");
    command.arg("capture-pane").arg("-p").arg("-e");
    if alternate_screen {
        command.arg("-a").arg("-q");
    } else {
        command.arg("-S").arg(format!("-{rows}"));
    }
    command.arg("-t").arg(target);

    let output = command
        .output()
        .map_err(|err| format!("capture tmux preview: {err}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!("capture tmux preview exited {}", output.status))
    } else {
        Err(stderr)
    }
}

fn terminal_parser_from_bytes(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
    let bytes = trim_capture_trailing_newline(bytes);
    let captured_rows = bytes
        .split(|byte| *byte == b'\n')
        .count()
        .min(u16::MAX as usize) as u16;
    let mut parser = vt100::Parser::new(rows.max(captured_rows).max(1), cols.max(1), 0);
    process_captured_terminal_bytes(&mut parser, bytes);
    parser
}

fn process_captured_terminal_bytes(parser: &mut vt100::Parser, bytes: &[u8]) {
    let normalized = normalize_capture_newlines(bytes);
    parser.process(&normalized);
}

fn normalize_capture_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut previous = None;
    for byte in bytes {
        if *byte == b'\n' && previous != Some(b'\r') {
            normalized.push(b'\r');
        }
        normalized.push(*byte);
        previous = Some(*byte);
    }
    normalized
}

fn trim_capture_trailing_newline(bytes: &[u8]) -> &[u8] {
    if let Some(trimmed) = bytes.strip_suffix(b"\r\n") {
        trimmed
    } else if let Some(trimmed) = bytes.strip_suffix(b"\n") {
        trimmed
    } else if let Some(trimmed) = bytes.strip_suffix(b"\r") {
        trimmed
    } else {
        bytes
    }
}

fn terminal_preview_parser_from_captures(
    alternate: Option<Vec<u8>>,
    visible: Option<Vec<u8>>,
    rows: u16,
    cols: u16,
) -> Option<vt100::Parser> {
    if let Some(bytes) = alternate {
        let parser = terminal_parser_from_bytes(&bytes, rows, cols);
        if terminal_screen_has_visible_content(parser.screen()) {
            return Some(parser);
        }
    }
    visible.map(|bytes| terminal_parser_from_bytes(&bytes, rows, cols))
}

fn terminal_screen_has_visible_content(screen: &vt100::Screen) -> bool {
    screen.contents().chars().any(|ch| !ch.is_whitespace())
}

fn reset_tmux_window_size(target: &str) {
    let _ = Command::new("tmux")
        .arg("set-option")
        .arg("-t")
        .arg(target)
        .arg("window-size")
        .arg("latest")
        .status();
}

fn enable_tmux_mouse(target: &str) -> Result<()> {
    let status = Command::new("tmux")
        .arg("set-option")
        .arg("-t")
        .arg(target)
        .arg("mouse")
        .arg("on")
        .status()
        .map_err(|err| AppError::msg(format!("enable tmux mouse: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::msg(format!("enable tmux mouse: {status}")))
    }
}

fn sync_tmux_preview_window_size(target: &str, rows: u16, cols: u16) {
    let _ = Command::new("tmux")
        .arg("resize-window")
        .arg("-t")
        .arg(target)
        .arg("-x")
        .arg(cols.max(1).to_string())
        .arg("-y")
        .arg(rows.max(1).to_string())
        .status();
}

fn tmux_session_name_for_id(session_id: &str) -> String {
    let mut fragment = String::new();
    for byte in session_id.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => fragment.push(byte as char),
            _ => fragment.push_str(&format!("{byte:02x}")),
        }
    }
    format!("agent-helm-{fragment}")
}

fn render_vt100_screen(screen: &vt100::Screen, buf: &mut Buffer, area: Rect) {
    render_vt100_screen_from(screen, buf, area, 0);
}

fn render_vt100_screen_tail(screen: &vt100::Screen, buf: &mut Buffer, area: Rect, scroll: u16) {
    let (rows, _) = screen.size();
    let visible_rows = rows.min(area.height);
    let max_scroll = rows.saturating_sub(visible_rows);
    let scroll = scroll.min(max_scroll);
    let start_row = rows.saturating_sub(visible_rows).saturating_sub(scroll);
    render_vt100_screen_from(screen, buf, area, start_row);
}

fn render_vt100_screen_from(screen: &vt100::Screen, buf: &mut Buffer, area: Rect, start_row: u16) {
    let (rows, cols) = screen.size();
    let start_row = start_row.min(rows);
    let rows = rows.saturating_sub(start_row).min(area.height);
    let cols = cols.min(area.width);
    for row in 0..rows {
        for col in 0..cols {
            let Some(source) = screen.cell(start_row + row, col) else {
                continue;
            };
            if source.is_wide_continuation() {
                continue;
            }
            let symbol = if source.has_contents() {
                source.contents()
            } else {
                " "
            };
            let style = vt100_cell_style(source);
            if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                cell.set_symbol(symbol);
                cell.set_style(style);
            }
        }
    }
}

fn render_vt100_cursor_overlay(screen: &vt100::Screen, buf: &mut Buffer, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (row, col) = screen.cursor_position();
    if row >= area.height || col >= area.width {
        return;
    }

    if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
        let style = cell.style().add_modifier(Modifier::REVERSED);
        cell.set_style(style);
    }
}

fn vt100_cell_style(cell: &vt100::Cell) -> Style {
    let mut fg = vt100_color(cell.fgcolor());
    let mut bg = vt100_color(cell.bgcolor());
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut style = Style::default().fg(fg).bg(bg);
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.dim() {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn vt100_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn encode_embedded_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char(ch) => Some(encode_char_key(ch, ctrl, alt)),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace if alt && !ctrl && !shift => Some(b"\x1b\x7f".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Left if alt && !ctrl && !shift => Some(b"\x1bb".to_vec()),
        KeyCode::Right if alt && !ctrl && !shift => Some(b"\x1bf".to_vec()),
        KeyCode::Left => Some(cursor_key(b'D', shift, ctrl, alt, application_cursor)),
        KeyCode::Right => Some(cursor_key(b'C', shift, ctrl, alt, application_cursor)),
        KeyCode::Up => Some(cursor_key(b'A', shift, ctrl, alt, application_cursor)),
        KeyCode::Down => Some(cursor_key(b'B', shift, ctrl, alt, application_cursor)),
        KeyCode::Home => Some(cursor_key(b'H', shift, ctrl, alt, application_cursor)),
        KeyCode::End => Some(cursor_key(b'F', shift, ctrl, alt, application_cursor)),
        KeyCode::PageUp => Some(tilde_key(b"5", shift, ctrl, alt)),
        KeyCode::PageDown => Some(tilde_key(b"6", shift, ctrl, alt)),
        KeyCode::Insert => Some(tilde_key(b"2", shift, ctrl, alt)),
        KeyCode::Delete => Some(tilde_key(b"3", shift, ctrl, alt)),
        KeyCode::F(number @ 1..=12) => Some(function_key(number, shift, ctrl, alt)),
        _ => None,
    }
}

fn encode_char_key(ch: char, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    if ctrl {
        let lower = ch.to_ascii_lowercase();
        let byte = match lower {
            'a'..='z' => (lower as u8) & 0x1f,
            '@' | ' ' => 0x00,
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '?' => 0x7f,
            _ => lower as u8,
        };
        out.push(byte);
    } else {
        let mut buf = [0; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
    out
}

fn cursor_key(letter: u8, shift: bool, ctrl: bool, alt: bool, application_cursor: bool) -> Vec<u8> {
    let modifier = modifier_code(shift, ctrl, alt);
    if modifier == 1 {
        if application_cursor {
            vec![0x1b, b'O', letter]
        } else {
            vec![0x1b, b'[', letter]
        }
    } else {
        let mut out = format!("\x1b[1;{modifier}").into_bytes();
        out.push(letter);
        out
    }
}

fn tilde_key(number: &[u8], shift: bool, ctrl: bool, alt: bool) -> Vec<u8> {
    let modifier = modifier_code(shift, ctrl, alt);
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(number);
    if modifier != 1 {
        out.push(b';');
        out.extend_from_slice(modifier.to_string().as_bytes());
    }
    out.push(b'~');
    out
}

fn function_key(number: u8, shift: bool, ctrl: bool, alt: bool) -> Vec<u8> {
    const BASE: [&[u8]; 12] = [
        b"11", b"12", b"13", b"14", b"15", b"17", b"18", b"19", b"20", b"21", b"23", b"24",
    ];
    tilde_key(BASE[(number - 1) as usize], shift, ctrl, alt)
}

fn modifier_code(shift: bool, ctrl: bool, alt: bool) -> u8 {
    1 + u8::from(shift) + (u8::from(alt) * 2) + (u8::from(ctrl) * 4)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    All,
    Occupied,
    Thinking,
    Running,
    Queued,
    Waiting,
    Idle,
    Stopped,
    Errored,
    Starting,
}

impl StatusFilter {
    fn matches(self, status: SessionDeckStatus) -> bool {
        match self {
            Self::All => true,
            Self::Occupied => status == SessionDeckStatus::Occupied,
            Self::Thinking => status == SessionDeckStatus::Thinking,
            Self::Running => status == SessionDeckStatus::Running,
            Self::Queued => status == SessionDeckStatus::Queued,
            Self::Waiting => status == SessionDeckStatus::Waiting,
            Self::Idle => status == SessionDeckStatus::Idle,
            Self::Stopped => status == SessionDeckStatus::Stopped,
            Self::Errored => status == SessionDeckStatus::Errored,
            Self::Starting => status == SessionDeckStatus::Starting,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Occupied,
            Self::Occupied => Self::Thinking,
            Self::Thinking => Self::Running,
            Self::Running => Self::Queued,
            Self::Queued => Self::Waiting,
            Self::Waiting => Self::Idle,
            Self::Idle => Self::Stopped,
            Self::Stopped => Self::Errored,
            Self::Errored => Self::Starting,
            Self::Starting => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Occupied => "occupied",
            Self::Thinking => "thinking",
            Self::Running => "running",
            Self::Queued => "queued",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
            Self::Errored => "errored",
            Self::Starting => "starting",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    New(NewForm),
    CreateGroup(GroupForm),
    GroupSettings(GroupSettingsForm),
    Fork(ForkForm),
    Session(SendForm),
    Move(MoveForm),
    Rename(RenameForm),
    ToolSettings(ToolSettingsForm),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SendForm {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MoveForm {
    session_id: String,
    group_name: String,
}

impl MoveForm {
    fn for_session(session: &SessionSummary) -> Self {
        Self {
            session_id: session.id.clone(),
            group_name: session.group_name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenameForm {
    session_id: String,
    name: String,
}

impl RenameForm {
    fn for_session(session: &SessionSummary) -> Self {
        Self {
            session_id: session.id.clone(),
            name: session.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupField {
    Name,
    DefaultPath,
}

const GROUP_FIELDS: [GroupField; 2] = [GroupField::Name, GroupField::DefaultPath];

impl GroupField {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::DefaultPath => "Default working dir",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Name => Self::DefaultPath,
            Self::DefaultPath => Self::DefaultPath,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Name => Self::Name,
            Self::DefaultPath => Self::Name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupForm {
    name: String,
    default_project_path: String,
    current_field: GroupField,
}

impl GroupForm {
    fn new() -> Self {
        Self {
            name: String::new(),
            default_project_path: default_new_session_path(),
            current_field: GroupField::Name,
        }
    }

    fn current_field(&self) -> GroupField {
        self.current_field
    }

    fn current_value_mut(&mut self) -> &mut String {
        match self.current_field {
            GroupField::Name => &mut self.name,
            GroupField::DefaultPath => &mut self.default_project_path,
        }
    }

    fn field_value(&self, field: GroupField) -> &str {
        match field {
            GroupField::Name => &self.name,
            GroupField::DefaultPath => &self.default_project_path,
        }
    }

    fn next_field(&mut self) {
        self.current_field = self.current_field.next();
    }

    fn previous_field(&mut self) {
        self.current_field = self.current_field.previous();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupSettingsField {
    DefaultPath,
    DefaultAgent,
    DefaultWorktree,
    DefaultCarryState,
}

const GROUP_SETTINGS_FIELDS: [GroupSettingsField; 4] = [
    GroupSettingsField::DefaultPath,
    GroupSettingsField::DefaultAgent,
    GroupSettingsField::DefaultWorktree,
    GroupSettingsField::DefaultCarryState,
];

impl GroupSettingsField {
    fn label(self) -> &'static str {
        match self {
            Self::DefaultPath => "Default working dir",
            Self::DefaultAgent => "Default agent",
            Self::DefaultWorktree => "Create worktree",
            Self::DefaultCarryState => "Copy current state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupSettingsForm {
    name: String,
    default_project_path: String,
    default_agent: String,
    default_worktree: Option<bool>,
    default_carry_state: Option<bool>,
    field_index: usize,
}

impl GroupSettingsForm {
    fn from_group(group: &GroupDefaults) -> Self {
        Self {
            name: group.name.clone(),
            default_project_path: group.default_project_path.clone(),
            default_agent: group.default_agent.clone().unwrap_or_default(),
            default_worktree: group.default_worktree,
            default_carry_state: group.default_carry_state,
            field_index: 0,
        }
    }

    fn current_field(&self) -> GroupSettingsField {
        GROUP_SETTINGS_FIELDS[self.field_index]
    }

    fn current_value_mut(&mut self) -> Option<&mut String> {
        match self.current_field() {
            GroupSettingsField::DefaultPath => Some(&mut self.default_project_path),
            GroupSettingsField::DefaultAgent
            | GroupSettingsField::DefaultWorktree
            | GroupSettingsField::DefaultCarryState => None,
        }
    }

    fn next_field(&mut self) {
        self.field_index = (self.field_index + 1).min(GROUP_SETTINGS_FIELDS.len() - 1);
    }

    fn previous_field(&mut self) {
        self.field_index = self.field_index.saturating_sub(1);
    }

    fn cycle_agent(&mut self, agent_choices: &[String], delta: isize) {
        let mut choices = vec![String::new()];
        choices.extend(effective_agent_choices(agent_choices, &self.default_agent));
        let current = choices
            .iter()
            .position(|agent| agent == &self.default_agent)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(choices.len() as isize) as usize;
        self.default_agent = choices[next].clone();
    }

    fn cycle_worktree(&mut self, delta: isize) {
        self.default_worktree = cycle_optional_bool(self.default_worktree, delta);
    }

    fn cycle_carry_state(&mut self, delta: isize) {
        self.default_carry_state = cycle_optional_bool(self.default_carry_state, delta);
    }

    fn update(&self) -> GroupSettingsUpdate {
        GroupSettingsUpdate {
            default_project_path: Some(self.default_project_path.trim().to_string()),
            default_agent: Some(non_empty(self.default_agent.trim())),
            default_worktree: Some(self.default_worktree),
            default_carry_state: Some(self.default_carry_state),
            ..GroupSettingsUpdate::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupDefaults {
    name: String,
    default_project_path: String,
    default_agent: Option<String>,
    default_worktree: Option<bool>,
    default_carry_state: Option<bool>,
}

impl From<&GroupRecord> for GroupDefaults {
    fn from(group: &GroupRecord) -> Self {
        Self {
            name: group.name.clone(),
            default_project_path: group.default_project_path.clone(),
            default_agent: group.default_agent.clone(),
            default_worktree: group.default_worktree,
            default_carry_state: group.default_carry_state,
        }
    }
}

fn cycle_optional_bool(value: Option<bool>, delta: isize) -> Option<bool> {
    let values = [None, Some(true), Some(false)];
    let current = values
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(0);
    let next = (current as isize + delta).rem_euclid(values.len() as isize) as usize;
    values[next]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLaunchSettings {
    pub name: String,
    pub installed: bool,
    pub executable: String,
    pub flags: Vec<String>,
    pub worktree: ToolWorktreeBehavior,
}

impl ToolLaunchSettings {
    pub fn from_profile(name: String, profile: &ToolProfile) -> Self {
        Self {
            name,
            installed: profile.installed,
            executable: profile.executable.clone().unwrap_or_default(),
            flags: profile.flags.clone(),
            worktree: profile.worktree,
        }
    }

    pub fn to_profile(&self) -> ToolProfile {
        ToolProfile {
            installed: self.installed,
            executable: non_empty(self.executable.trim()),
            flags: self.flags.clone(),
            worktree: self.worktree,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSettingsField {
    Tool,
    Installed,
    Executable,
    Flags,
    Worktree,
}

const TOOL_SETTINGS_FIELDS: [ToolSettingsField; 5] = [
    ToolSettingsField::Tool,
    ToolSettingsField::Installed,
    ToolSettingsField::Executable,
    ToolSettingsField::Flags,
    ToolSettingsField::Worktree,
];

impl ToolSettingsField {
    fn label(self) -> &'static str {
        match self {
            Self::Tool => "Tool",
            Self::Installed => "Installed",
            Self::Executable => "Binary",
            Self::Flags => "Flags",
            Self::Worktree => "Worktree",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolSettingsForm {
    tools: Vec<ToolLaunchSettings>,
    selected_tool: usize,
    field_index: usize,
    executable: String,
    flags: String,
}

impl ToolSettingsForm {
    fn new(settings: &[ToolLaunchSettings], preferred_tool: &str) -> Self {
        let mut tools = settings.to_vec();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        if tools.is_empty() {
            tools.push(ToolLaunchSettings {
                name: "codex".to_string(),
                installed: true,
                executable: "codex".to_string(),
                flags: Vec::new(),
                worktree: ToolWorktreeBehavior::Always,
            });
        }
        let selected_tool = tools
            .iter()
            .position(|tool| tool.name == preferred_tool)
            .or_else(|| tools.iter().position(|tool| tool.name == "codex"))
            .unwrap_or(0);
        let mut form = Self {
            tools,
            selected_tool,
            field_index: 0,
            executable: String::new(),
            flags: String::new(),
        };
        form.load_selected_tool();
        form
    }

    fn current_field(&self) -> ToolSettingsField {
        TOOL_SETTINGS_FIELDS[self.field_index]
    }

    fn current_tool(&self) -> &ToolLaunchSettings {
        &self.tools[self.selected_tool]
    }

    fn current_tool_mut(&mut self) -> &mut ToolLaunchSettings {
        &mut self.tools[self.selected_tool]
    }

    fn current_value_mut(&mut self) -> Option<&mut String> {
        match self.current_field() {
            ToolSettingsField::Executable => Some(&mut self.executable),
            ToolSettingsField::Flags => Some(&mut self.flags),
            ToolSettingsField::Tool
            | ToolSettingsField::Installed
            | ToolSettingsField::Worktree => None,
        }
    }

    fn next_field(&mut self) {
        self.field_index = (self.field_index + 1).min(TOOL_SETTINGS_FIELDS.len() - 1);
    }

    fn previous_field(&mut self) {
        self.field_index = self.field_index.saturating_sub(1);
    }

    fn cycle_tool(&mut self, delta: isize) {
        self.sync_selected_tool();
        let next =
            (self.selected_tool as isize + delta).rem_euclid(self.tools.len() as isize) as usize;
        self.selected_tool = next;
        self.load_selected_tool();
    }

    fn toggle_installed(&mut self) {
        let tool = self.current_tool_mut();
        tool.installed = !tool.installed;
    }

    fn cycle_worktree(&mut self, delta: isize) {
        let values = [
            ToolWorktreeBehavior::Always,
            ToolWorktreeBehavior::Manual,
            ToolWorktreeBehavior::Never,
        ];
        let current = values
            .iter()
            .position(|value| *value == self.current_tool().worktree)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(values.len() as isize) as usize;
        self.current_tool_mut().worktree = values[next];
    }

    fn settings(mut self) -> Vec<ToolLaunchSettings> {
        self.sync_selected_tool();
        self.tools
    }

    fn load_selected_tool(&mut self) {
        let (executable, flags) = {
            let tool = self.current_tool();
            (tool.executable.clone(), tool.flags.join(" "))
        };
        self.executable = executable;
        self.flags = flags;
    }

    fn sync_selected_tool(&mut self) {
        let executable = self.executable.trim().to_string();
        let flags = split_flags(&self.flags);
        let tool = self.current_tool_mut();
        tool.executable = executable;
        tool.flags = flags;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewField {
    Name,
    Agent,
    Path,
    Group,
    Worktree,
    CarryState,
}

const NEW_FIELDS: [NewField; 6] = [
    NewField::Name,
    NewField::Agent,
    NewField::Path,
    NewField::Group,
    NewField::Worktree,
    NewField::CarryState,
];
const BUILT_IN_AGENT_CHOICES: [&str; 5] = ["shell", "claude", "codex", "gemini", "opencode"];

impl NewField {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Agent => "Agent",
            Self::Path => "Path",
            Self::Group => "Group",
            Self::Worktree => "Worktree",
            Self::CarryState => "Copy current state",
        }
    }

    fn is_text_entry(self) -> bool {
        matches!(self, Self::Name | Self::Path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NewForm {
    name: String,
    default_name: String,
    agent: String,
    path: String,
    group_name: String,
    worktree: bool,
    carry_state: bool,
    field_index: usize,
    launching: bool,
}

impl Default for NewForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            default_name: generated_session_name(),
            agent: "shell".to_string(),
            path: default_new_session_path(),
            group_name: "default".to_string(),
            worktree: false,
            carry_state: false,
            field_index: 0,
            launching: false,
        }
    }
}

impl NewForm {
    fn with_agent(agent: &str) -> Self {
        Self {
            agent: normalized_agent(agent),
            ..Self::default()
        }
    }

    fn for_session(session: &SessionSummary) -> Self {
        Self {
            name: format!("{} copy", session.name),
            agent: normalized_agent(&session.agent),
            path: session.project_path.clone(),
            group_name: session.group_name.clone(),
            ..Self::default()
        }
    }

    fn current_field(&self) -> NewField {
        NEW_FIELDS[self.field_index]
    }

    fn current_value_mut(&mut self) -> Option<&mut String> {
        match self.current_field() {
            NewField::Name => Some(&mut self.name),
            NewField::Path => Some(&mut self.path),
            NewField::Agent | NewField::Group | NewField::Worktree | NewField::CarryState => None,
        }
    }

    fn field_value(&self, field: NewField) -> &str {
        match field {
            NewField::Name => &self.name,
            NewField::Agent => &self.agent,
            NewField::Path => &self.path,
            NewField::Group => &self.group_name,
            NewField::Worktree => {
                if self.worktree {
                    "true"
                } else {
                    "false"
                }
            }
            NewField::CarryState => {
                if self.carry_state {
                    "true"
                } else {
                    "false"
                }
            }
        }
    }

    fn default_name(&self) -> String {
        self.default_name.clone()
    }

    fn next_field(&mut self) {
        self.field_index = (self.field_index + 1).min(NEW_FIELDS.len() - 1);
    }

    fn previous_field(&mut self) {
        self.field_index = self.field_index.saturating_sub(1);
    }

    fn cycle_agent(&mut self, agent_choices: &[String], delta: isize) {
        let choices = effective_agent_choices(agent_choices, &self.agent);
        let current = choices
            .iter()
            .position(|agent| agent == &self.agent)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(choices.len() as isize) as usize;
        self.agent = choices[next].to_string();
    }

    fn build_request(&self) -> std::result::Result<CreateSession, &'static str> {
        let name = self.name.trim();
        let agent = self.agent.trim();
        let path = self.path.trim();
        let group_name = self.group_name.trim();
        if agent.is_empty() {
            return Err("agent is required");
        }
        if path.is_empty() {
            return Err("path is required");
        }
        if group_name.is_empty() {
            return Err("group is required");
        }
        let request_name = if name.is_empty() {
            self.default_name()
        } else {
            name.to_string()
        };

        Ok(CreateSession {
            path: path.to_string(),
            agent: agent.to_string(),
            command: String::new(),
            name: request_name.clone(),
            group_name: group_name.to_string(),
            worktree: self
                .worktree
                .then(|| auto_worktree_branch(agent, &request_name, path)),
            carry_state: self.carry_state,
            sandbox: false,
            prompt: None,
            parent_session_id: None,
        })
    }
}

fn new_form_for_agent(app: &App, agent: &str) -> NewForm {
    let mut form = NewForm::with_agent(agent);
    if let Some(group_name) = active_group_name(app) {
        form.group_name = group_name;
    } else {
        select_existing_group(&mut form.group_name, &app.group_default_paths);
    }
    apply_group_defaults(&mut form, app);
    form
}

fn active_group_name(app: &App) -> Option<String> {
    app.view().selected.map(|session| session.group_name)
}

fn new_form_for_session(app: &App, session: &SessionSummary) -> NewForm {
    let mut form = NewForm::for_session(session);
    select_existing_group(&mut form.group_name, &app.group_default_paths);
    form.worktree = tool_creates_worktree_by_default(&app.tool_settings, &form.agent);
    form
}

fn apply_group_defaults(form: &mut NewForm, app: &App) {
    apply_group_defaults_from_maps(form, &app.group_defaults, &app.tool_settings);
}

fn apply_group_defaults_from_maps(
    form: &mut NewForm,
    group_defaults: &BTreeMap<String, GroupDefaults>,
    tool_settings: &[ToolLaunchSettings],
) {
    if let Some(group) = group_defaults.get(&form.group_name) {
        apply_group_default_settings(form, group, tool_settings);
        return;
    }
    form.worktree = tool_creates_worktree_by_default(tool_settings, &form.agent);
    form.carry_state = false;
}

fn apply_group_default_settings(
    form: &mut NewForm,
    group: &GroupDefaults,
    tool_settings: &[ToolLaunchSettings],
) {
    if !group.default_project_path.trim().is_empty() {
        form.path = group.default_project_path.clone();
    }
    if let Some(agent) = &group.default_agent {
        form.agent = normalized_agent(agent);
    }
    form.worktree = group
        .default_worktree
        .unwrap_or_else(|| tool_creates_worktree_by_default(tool_settings, &form.agent));
    form.carry_state = group.default_carry_state.unwrap_or(false);
}

fn group_default_path<'a>(
    group_default_paths: &'a BTreeMap<String, String>,
    group_name: &str,
) -> Option<&'a str> {
    group_default_paths
        .get(group_name)
        .map(String::as_str)
        .filter(|path| !path.is_empty())
}

fn group_names(group_default_paths: &BTreeMap<String, String>) -> Vec<String> {
    group_default_paths.keys().cloned().collect()
}

fn select_existing_group(group_name: &mut String, group_default_paths: &BTreeMap<String, String>) {
    if group_default_paths.contains_key(group_name.as_str()) {
        return;
    }
    *group_name = group_default_paths
        .keys()
        .next()
        .cloned()
        .unwrap_or_default();
}

fn cycle_group_name(
    group_name: &mut String,
    group_default_paths: &BTreeMap<String, String>,
    delta: isize,
) {
    let len = group_default_paths.len();
    if len == 0 {
        group_name.clear();
        return;
    }
    let current = group_default_paths
        .keys()
        .position(|k| k == group_name)
        .unwrap_or(0);
    let next = (current as isize + delta).rem_euclid(len as isize) as usize;
    *group_name = group_default_paths.keys().nth(next).unwrap().clone();
}

fn apply_group_default_path(
    form: &mut NewForm,
    group_default_paths: &BTreeMap<String, String>,
) -> bool {
    let Some(path) = group_default_path(group_default_paths, &form.group_name) else {
        return false;
    };
    form.path = path.to_string();
    true
}

fn normalized_agent(agent: &str) -> String {
    let agent = agent.trim();
    if agent.is_empty() {
        "shell".to_string()
    } else {
        agent.to_string()
    }
}

fn default_agent_choices() -> Vec<String> {
    BUILT_IN_AGENT_CHOICES
        .iter()
        .map(|agent| agent.to_string())
        .collect()
}

fn normalize_agent_choices(agent_choices: Vec<String>) -> Vec<String> {
    let mut choices = Vec::new();
    for agent in agent_choices {
        let agent = agent.trim();
        if !agent.is_empty() && !choices.iter().any(|choice| choice == agent) {
            choices.push(agent.to_string());
        }
    }
    if choices.is_empty() {
        choices = default_agent_choices();
    }
    choices
}

fn effective_agent_choices(agent_choices: &[String], current_agent: &str) -> Vec<String> {
    let mut choices = if agent_choices.is_empty() {
        default_agent_choices()
    } else {
        agent_choices.to_vec()
    };
    if !current_agent.trim().is_empty() && !choices.iter().any(|agent| agent == current_agent) {
        choices.push(current_agent.to_string());
    }
    choices
}

fn normalize_tool_settings(mut settings: Vec<ToolLaunchSettings>) -> Vec<ToolLaunchSettings> {
    settings.sort_by(|left, right| left.name.cmp(&right.name));
    for agent in BUILT_IN_AGENT_CHOICES {
        if !settings.iter().any(|tool| tool.name == agent) {
            settings.push(ToolLaunchSettings {
                name: agent.to_string(),
                installed: true,
                executable: agent.to_string(),
                flags: Vec::new(),
                worktree: if agent == "shell" {
                    ToolWorktreeBehavior::Never
                } else {
                    ToolWorktreeBehavior::Always
                },
            });
        }
    }
    settings.sort_by(|left, right| left.name.cmp(&right.name));
    settings
}

fn agent_choices_from_tool_settings(settings: &[ToolLaunchSettings]) -> Vec<String> {
    let mut choices = settings
        .iter()
        .filter(|tool| tool.installed)
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    choices.sort();
    if let Some(shell_index) = choices.iter().position(|name| name == "shell") {
        let shell = choices.remove(shell_index);
        choices.insert(0, shell);
    }
    normalize_agent_choices(choices)
}

fn tool_creates_worktree_by_default(settings: &[ToolLaunchSettings], agent: &str) -> bool {
    settings
        .iter()
        .find(|tool| tool.name == agent)
        .is_some_and(|tool| tool.worktree.creates_worktree_by_default())
}

fn split_flags(flags: &str) -> Vec<String> {
    flags
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>()
}

fn default_new_session_path() -> String {
    std::env::current_dir()
        .unwrap_or_else(|_| Path::new(".").to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn generated_session_name() -> String {
    Generator::with_naming(Name::Plain)
        .next()
        .unwrap_or_else(|| "agent-helm-session".to_string())
}

fn auto_worktree_branch(agent: &str, name: &str, path: &str) -> String {
    crate::util::auto_worktree_branch(agent, name, path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkField {
    Name,
    Group,
    Worktree,
    CarryState,
    Start,
}

const FORK_FIELDS: [ForkField; 5] = [
    ForkField::Name,
    ForkField::Group,
    ForkField::Worktree,
    ForkField::CarryState,
    ForkField::Start,
];

impl ForkField {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Group => "Group",
            Self::Worktree => "Worktree",
            Self::CarryState => "Copy current state",
            Self::Start => "Start",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ForkForm {
    parent_session_id: String,
    name: String,
    group_name: String,
    worktree: String,
    carry_state: bool,
    start: String,
    field_index: usize,
}

impl ForkForm {
    fn for_session(session: &SessionSummary) -> Self {
        Self {
            parent_session_id: session.id.clone(),
            name: format!("{} fork", session.name),
            group_name: session.group_name.clone(),
            worktree: String::new(),
            carry_state: false,
            start: "true".to_string(),
            field_index: 0,
        }
    }

    fn current_field(&self) -> ForkField {
        FORK_FIELDS[self.field_index]
    }

    fn current_value_mut(&mut self) -> Option<&mut String> {
        match self.current_field() {
            ForkField::Name => Some(&mut self.name),
            ForkField::Group => Some(&mut self.group_name),
            ForkField::Worktree => Some(&mut self.worktree),
            ForkField::CarryState => None,
            ForkField::Start => Some(&mut self.start),
        }
    }

    fn field_value(&self, field: ForkField) -> &str {
        match field {
            ForkField::Name => &self.name,
            ForkField::Group => &self.group_name,
            ForkField::Worktree => &self.worktree,
            ForkField::CarryState => {
                if self.carry_state {
                    "true"
                } else {
                    "false"
                }
            }
            ForkField::Start => &self.start,
        }
    }

    fn next_field(&mut self) {
        self.field_index = (self.field_index + 1).min(FORK_FIELDS.len() - 1);
    }

    fn previous_field(&mut self) {
        self.field_index = self.field_index.saturating_sub(1);
    }

    fn build_request(&self) -> std::result::Result<ForkSessionRequest, &'static str> {
        let start_immediately = parse_bool_field(self.start.trim(), "start")?;

        Ok(ForkSessionRequest {
            parent_session_id: self.parent_session_id.clone(),
            name: non_empty(self.name.trim()),
            group_name: non_empty(self.group_name.trim()),
            worktree_branch: non_empty(self.worktree.trim()),
            carry_state: self.carry_state,
            start_immediately,
        })
    }
}

fn run_app<F, D, S>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    load_deck_statuses: &mut S,
    load_details: &mut D,
    handle_action: &mut F,
) -> Result<()>
where
    F: Fn(TuiAction) -> Result<Vec<SessionRecord>> + Clone + Send + 'static,
    D: FnMut(&str) -> Result<TuiDetails>,
    S: FnMut(&[String]) -> Result<Vec<(String, TuiSessionStatus)>>,
{
    refresh_deck_statuses(app, load_deck_statuses)?;
    refresh_details(app, load_details, true)?;
    initialize_sidebar_width(app, terminal.size()?);
    let preview_worker = PreviewWorker::spawn();
    let (background_tx, background_rx) = mpsc::channel();
    let background_action_handler = (*handle_action).clone();
    let mut enqueue_background_action = move |request: BackgroundActionRequest| {
        spawn_background_action_thread(
            background_action_handler.clone(),
            background_tx.clone(),
            request,
        );
    };
    let mut tui_loop = TuiLoop::new(Instant::now());
    let mut mouse_capture_enabled = false;
    loop {
        let now = Instant::now();
        if drain_background_action_results(app, &background_rx, handle_action)? {
            schedule_details_refresh(app, Instant::now(), true);
            tui_loop.mark_changed();
        }
        sync_mouse_capture(terminal, &mut mouse_capture_enabled, app)?;
        if tui_loop.should_refresh_sessions(now) {
            refresh_sessions(app, handle_action)?;
            refresh_deck_statuses(app, load_deck_statuses)?;
            schedule_details_refresh(app, Instant::now(), true);
            tui_loop.mark_session_refreshed(Instant::now());
            continue;
        }
        if tui_loop.should_animate(now) {
            app.animation_frame = app.animation_frame.wrapping_add(1);
            tui_loop.mark_animated(Instant::now());
        }

        if sync_embedded_tmux(terminal, app) {
            tui_loop.mark_changed();
        }
        if sync_terminal_preview(terminal, app, &preview_worker, now) {
            tui_loop.mark_changed();
        }

        if tui_loop.needs_draw {
            terminal.draw(|frame| render(frame, app))?;
            tui_loop.mark_drawn();
        }

        if !event::poll(tui_loop.poll_timeout(Instant::now(), pending_details_due(app)))? {
            if refresh_details_if_due(app, load_details, Instant::now())? {
                tui_loop.mark_changed();
            }
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if is_global_quit_key(key, &app.mode) {
                    return Ok(());
                }

                let previous_selected_id = selected_id(app);
                if process_key_with_background(
                    terminal,
                    app,
                    handle_action,
                    &mut enqueue_background_action,
                    key,
                )? {
                    return Ok(());
                }
                schedule_details_refresh(
                    app,
                    Instant::now(),
                    selected_id(app) != previous_selected_id,
                );
                tui_loop.mark_changed();
            }
            Event::Mouse(mouse) => {
                let size = terminal.size()?;
                let area = Rect::new(0, 0, size.width, size.height);
                if process_mouse(app, mouse, area) {
                    tui_loop.mark_changed();
                }
            }
            _ => {}
        }
    }
}

fn spawn_background_action_thread<F>(
    handle_action: F,
    results: Sender<BackgroundActionResult>,
    request: BackgroundActionRequest,
) where
    F: Fn(TuiAction) -> Result<Vec<SessionRecord>> + Send + 'static,
{
    thread::spawn(move || {
        let result = handle_action(request.action).map_err(|err| err.to_string());
        let _ = results.send(BackgroundActionResult {
            session_id: request.session_id,
            result,
        });
    });
}

fn drain_background_action_results<F>(
    app: &mut App,
    results: &Receiver<BackgroundActionResult>,
    handle_action: &mut F,
) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut changed = false;
    while let Ok(result) = results.try_recv() {
        changed |= apply_background_action_result(app, result, handle_action)?;
    }
    Ok(changed)
}

#[cfg(test)]
fn complete_background_requests_synchronously<F>(
    app: &mut App,
    handle_action: &mut F,
    requests: Vec<BackgroundActionRequest>,
) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    for request in requests {
        let session_id = request.session_id;
        let result = handle_action(request.action).map_err(|err| err.to_string());
        apply_background_action_result(
            app,
            BackgroundActionResult { session_id, result },
            handle_action,
        )?;
    }
    Ok(())
}

fn apply_background_action_result<F>(
    app: &mut App,
    result: BackgroundActionResult,
    handle_action: &mut F,
) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let Some(mut operation) = app.pending_operations.remove(&result.session_id) else {
        return Ok(false);
    };

    match result.result {
        Ok(sessions) => match operation.kind {
            PendingOperationKind::Create | PendingOperationKind::Fork => {
                let pending_id = operation.session.id.clone();
                app.sessions.retain(|session| session.id != pending_id);
                app.deck_statuses.remove(&pending_id);
                app.sessions = sessions;
                merge_pending_sessions(app);
                if let Some(created_id) = find_created_session_id(&operation, &app.sessions) {
                    select_matching_session(app, |session| session.id == created_id);
                } else {
                    select_matching_session(app, |_| false);
                }
                app.mode = Mode::Normal;
                app.status_message = Some(operation.success_message);
            }
            PendingOperationKind::Delete => {
                let deleted_id = operation.session.id.clone();
                app.sessions = sessions;
                app.deck_statuses.remove(&deleted_id);
                invalidate_session_cache(app, &deleted_id);
                if app.search_results_active && !app.query.trim().is_empty() {
                    refresh_sessions(app, handle_action)?;
                } else {
                    merge_pending_sessions(app);
                }
                if let Some(target_session_id) = operation.delete_selection_target.take() {
                    select_matching_session(app, |session| session.id == target_session_id);
                } else {
                    select_matching_session(app, |_| false);
                }
                app.status_message = Some(operation.success_message);
            }
        },
        Err(message) => {
            operation.state = PendingOperationState::Failed(message.clone());
            app.pending_operations
                .insert(operation.session.id.clone(), operation.clone());
            merge_pending_sessions(app);
            app.status_message = Some(format!("{}: {message}", operation.failure_prefix));
        }
    }

    Ok(true)
}

fn find_created_session_id(
    operation: &PendingOperation,
    sessions: &[SessionRecord],
) -> Option<String> {
    find_created_session_id_with_parent(operation, sessions, true)
        .or_else(|| find_created_session_id_with_parent(operation, sessions, false))
}

fn find_created_session_id_with_parent(
    operation: &PendingOperation,
    sessions: &[SessionRecord],
    require_parent: bool,
) -> Option<String> {
    let mut matches = sessions
        .iter()
        .filter(|session| {
            if require_parent
                && let Some(parent_session_id) = operation.parent_session_id.as_deref()
            {
                if session.parent_session_id.as_deref() != Some(parent_session_id) {
                    return false;
                }
            }
            if let Some(expected_name) = operation.expected_name.as_deref() {
                if session.name != expected_name {
                    return false;
                }
            }
            if let Some(expected_group) = operation.expected_group.as_deref() {
                if session.group_name != expected_group {
                    return false;
                }
            }
            true
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    matches.first().map(|session| session.id.clone())
}

fn sync_embedded_tmux(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> bool {
    if !matches!(app.mode, Mode::Session(_)) {
        if app.embedded.is_some() {
            app.embedded = None;
            return true;
        }
        return false;
    }

    let view = app.view();
    let Some(session) = view.selected.as_ref() else {
        app.embedded = None;
        app.mode = Mode::Normal;
        app.status_message = Some("no session selected".to_string());
        return true;
    };
    if app.pending_operations.contains_key(&session.id) {
        app.embedded = None;
        app.mode = Mode::Normal;
        app.status_message = Some("operation in progress".to_string());
        return true;
    }
    if !matches!(
        session.status,
        SessionStatus::Running | SessionStatus::Starting
    ) {
        app.embedded = None;
        app.mode = Mode::Normal;
        app.status_message = Some("session is not running".to_string());
        return true;
    }

    let Ok(size) = terminal.size() else {
        return false;
    };
    let area = embedded_terminal_area(size, &view, app.sidebar_width);
    let rows = area.height.max(EMBED_MIN_ROWS);
    let cols = area.width.max(EMBED_MIN_COLS);
    let target = session
        .runtime_id
        .clone()
        .unwrap_or_else(|| tmux_session_name_for_id(&session.id));

    let mut changed = false;
    let needs_spawn = app
        .embedded
        .as_ref()
        .is_none_or(|embedded| embedded.target != target);
    if needs_spawn {
        match EmbeddedTmux::spawn(target.clone(), rows, cols) {
            Ok(embedded) => {
                app.embedded = Some(embedded);
                app.status_message = None;
                changed = true;
            }
            Err(err) => {
                app.embedded = None;
                app.mode = Mode::Normal;
                app.status_message = Some(format!("attach failed: {err}"));
                return true;
            }
        }
    }

    if let Some(embedded) = app.embedded.as_mut() {
        embedded.resize(rows, cols);
        changed |= embedded.drain();
    }

    changed
}

fn sync_terminal_preview(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    worker: &PreviewWorker,
    now: Instant,
) -> bool {
    let mut changed = apply_preview_results(app, worker);

    if !matches!(app.mode, Mode::Normal | Mode::Search) {
        return clear_terminal_preview(app) || changed;
    }

    let view = app.view();
    let Some(session) = view.selected.as_ref() else {
        return clear_terminal_preview(app) || changed;
    };
    if app.pending_operations.contains_key(&session.id) {
        return clear_terminal_preview(app) || changed;
    }
    if !session_is_live(session.status) {
        return clear_terminal_preview(app) || changed;
    }

    let Ok(size) = terminal.size() else {
        return changed;
    };
    let area = terminal_preview_area(size, &view, app.sidebar_width);
    if area.width == 0 || area.height == 0 {
        return clear_terminal_preview(app) || changed;
    }

    let target = tmux_target_for_session(session);
    let rows = area.height;
    let cols = area.width;
    let scroll = app.preview_scroll;
    let needs_refresh = app.preview.as_ref().is_none_or(|preview| {
        !preview.is_current(&session.id, &target, rows, cols, scroll) || preview.is_stale(now)
    });
    if !needs_refresh {
        return changed;
    }
    if app
        .pending_preview
        .as_ref()
        .is_some_and(|request| request.matches(&session.id, &target, rows, cols, scroll))
    {
        return changed;
    }

    changed |= enqueue_preview_request(app, worker, session.id.clone(), target, rows, cols, scroll);
    changed
}

fn clear_terminal_preview(app: &mut App) -> bool {
    let changed = app.preview.is_some() || app.pending_preview.is_some();
    app.preview = None;
    app.pending_preview = None;
    changed
}

fn apply_preview_results(app: &mut App, worker: &PreviewWorker) -> bool {
    let mut changed = false;
    while let Ok(result) = worker.results.try_recv() {
        changed |= apply_preview_result(app, result);
    }
    changed
}

fn apply_preview_result(app: &mut App, result: PreviewResult) -> bool {
    let Some(pending) = app.pending_preview.as_ref() else {
        return false;
    };
    if pending != &result.request {
        return false;
    }

    app.preview = Some(result.preview);
    app.pending_preview = None;
    true
}

fn enqueue_preview_request(
    app: &mut App,
    worker: &PreviewWorker,
    session_id: String,
    target: String,
    rows: u16,
    cols: u16,
    scroll: u16,
) -> bool {
    let request = next_preview_request(app, session_id, target, rows, cols, scroll);
    if worker.requests.send(request.clone()).is_err() {
        app.status_message = Some("preview worker stopped".to_string());
        return false;
    }
    app.pending_preview = Some(request);
    true
}

fn next_preview_request(
    app: &mut App,
    session_id: String,
    target: String,
    rows: u16,
    cols: u16,
    scroll: u16,
) -> PreviewRequest {
    app.preview_generation = app.preview_generation.wrapping_add(1);
    PreviewRequest {
        generation: app.preview_generation,
        session_id,
        target,
        rows: rows.max(1),
        cols: cols.max(1),
        scroll: scroll.min(PREVIEW_MAX_SCROLL_LINES),
    }
}

fn embedded_terminal_area(
    size: ratatui::layout::Size,
    view: &DashboardView,
    sidebar_width: Option<u16>,
) -> Rect {
    let area = Rect::new(0, 0, size.width, size.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let body = body_layout_for_view(chunks[1], view, sidebar_width, 0);
    panel_block("SESSION").inner(body.detail)
}

fn terminal_preview_area(
    size: ratatui::layout::Size,
    view: &DashboardView,
    sidebar_width: Option<u16>,
) -> Rect {
    let area = Rect::new(0, 0, size.width, size.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let body = body_layout_for_view(chunks[1], view, sidebar_width, 0);
    panel_block("PREVIEW").inner(body.detail)
}

fn session_is_live(status: SessionStatus) -> bool {
    matches!(status, SessionStatus::Running | SessionStatus::Starting)
}

fn tmux_target_for_session(session: &SessionSummary) -> String {
    session
        .runtime_id
        .clone()
        .unwrap_or_else(|| tmux_session_name_for_id(&session.id))
}

fn process_key_with_background<B, F, G>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    handle_action: &mut F,
    spawn_background_action: &mut G,
    key: KeyEvent,
) -> Result<bool>
where
    B: ratatui::backend::Backend,
    B::Error: std::fmt::Display,
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    G: FnMut(BackgroundActionRequest),
{
    if matches!(app.mode, Mode::Normal | Mode::Search) && is_plain_ctrl_key(key, 't') {
        toggle_mouse_capture(app);
        return Ok(false);
    }

    match app.mode {
        Mode::Normal if key.code == KeyCode::Enter => {
            focus_selected_session(app);
        }
        Mode::Normal => {
            if handle_normal_key_with_background(key, app, handle_action, spawn_background_action)?
            {
                return Ok(true);
            }
        }
        Mode::Search => handle_search_key(key, app, handle_action)?,
        Mode::New(_) => {
            if new_key_submits(app, key) {
                set_new_form_launching(app, true);
                app.status_message = None;
                terminal
                    .draw(|frame| render(frame, app))
                    .map_err(|err| AppError::msg(format!("draw launch feedback: {err}")))?;
            }
            handle_new_key_with_background(key, app, spawn_background_action)?;
        }
        Mode::CreateGroup(_) => handle_create_group_key(key, app, handle_action)?,
        Mode::GroupSettings(_) => handle_group_settings_key(key, app, handle_action)?,
        Mode::Fork(_) => handle_fork_key_with_background(key, app, spawn_background_action)?,
        Mode::Session(_) => handle_session_key(key, app)?,
        Mode::Move(_) => handle_move_key(key, app, handle_action)?,
        Mode::Rename(_) => handle_rename_key(key, app, handle_action)?,
        Mode::ToolSettings(_) => handle_tool_settings_key(key, app, handle_action)?,
        Mode::Help => handle_help_key(key, app),
    }
    Ok(false)
}

#[cfg(test)]
fn process_key<B, F>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    handle_action: &mut F,
    key: KeyEvent,
) -> Result<bool>
where
    B: ratatui::backend::Backend,
    B::Error: std::fmt::Display,
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut background_requests = Vec::new();
    let result = process_key_with_background(
        terminal,
        app,
        handle_action,
        &mut |request| background_requests.push(request),
        key,
    )?;
    complete_background_requests_synchronously(app, handle_action, background_requests)?;
    Ok(result)
}

fn is_plain_ctrl_key(key: KeyEvent, ch: char) -> bool {
    key.code == KeyCode::Char(ch) && key.modifiers == KeyModifiers::CONTROL
}

fn is_plain_char_key(key: KeyEvent, ch: char) -> bool {
    key.code == KeyCode::Char(ch) && key.modifiers == KeyModifiers::NONE
}

fn toggle_mouse_capture(app: &mut App) {
    app.mouse_capture = !app.mouse_capture;
    app.status_message = Some(if app.mouse_capture {
        "mouse wheel scrolling enabled".to_string()
    } else {
        "terminal text selection enabled".to_string()
    });
}

fn is_global_quit_key(key: KeyEvent, mode: &Mode) -> bool {
    is_plain_ctrl_key(key, 'c') && !matches!(mode, Mode::Session(_))
}

fn new_key_submits(app: &App, key: KeyEvent) -> bool {
    let Mode::New(form) = &app.mode else {
        return false;
    };
    match key.code {
        KeyCode::Enter => form.field_index == NEW_FIELDS.len() - 1,
        KeyCode::Char('s') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

fn set_new_form_launching(app: &mut App, launching: bool) {
    if let Mode::New(form) = &mut app.mode {
        form.launching = launching;
    }
}

fn process_mouse(app: &mut App, mouse: MouseEvent, area: Rect) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp if mouse_on_embedded_session(mouse, area, app) => {
            scroll_embedded_session(app, mouse, area)
        }
        MouseEventKind::ScrollDown if mouse_on_embedded_session(mouse, area, app) => {
            scroll_embedded_session(app, mouse, area)
        }
        MouseEventKind::ScrollUp if mouse_on_session_preview(mouse, area, app) => {
            scroll_session_preview(app, PREVIEW_SCROLL_LINES as i16)
        }
        MouseEventKind::ScrollDown if mouse_on_session_preview(mouse, area, app) => {
            scroll_session_preview(app, -(PREVIEW_SCROLL_LINES as i16))
        }
        MouseEventKind::Down(MouseButton::Left) if mouse_on_divider(mouse, area, app) => {
            app.resizing_sidebar = true;
            set_sidebar_width_from_mouse(app, mouse.column, area)
        }
        MouseEventKind::Drag(MouseButton::Left) if app.resizing_sidebar => {
            set_sidebar_width_from_mouse(app, mouse.column, area)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let was_resizing = app.resizing_sidebar;
            app.resizing_sidebar = false;
            was_resizing
        }
        _ => false,
    }
}

fn scroll_embedded_session(app: &mut App, mouse: MouseEvent, area: Rect) -> bool {
    let terminal_area = embedded_terminal_area(
        ratatui::layout::Size {
            width: area.width,
            height: area.height,
        },
        &app.view(),
        app.sidebar_width,
    );
    if !point_in_rect(mouse.column, mouse.row, terminal_area) {
        return false;
    };
    let direction = match mouse.kind {
        MouseEventKind::ScrollUp => TmuxScrollDirection::Up,
        MouseEventKind::ScrollDown => TmuxScrollDirection::Down,
        _ => return false,
    };
    let Some(target) = app
        .embedded
        .as_ref()
        .map(|embedded| embedded.target.clone())
    else {
        return false;
    };

    match scroll_tmux_history(&target, direction) {
        Ok(()) => {
            if let Some(embedded) = app.embedded.as_mut() {
                embedded.drain();
            }
            true
        }
        Err(err) => {
            app.status_message = Some(format!("scroll failed: {err}"));
            false
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TmuxScrollDirection {
    Up,
    Down,
}

fn scroll_tmux_history(target: &str, direction: TmuxScrollDirection) -> Result<()> {
    if matches!(direction, TmuxScrollDirection::Up) {
        let status = Command::new("tmux")
            .arg("copy-mode")
            .arg("-e")
            .arg("-t")
            .arg(target)
            .status()
            .map_err(|err| AppError::msg(format!("enter tmux copy mode: {err}")))?;
        if !status.success() {
            return Err(AppError::msg(format!("enter tmux copy mode: {status}")));
        }
    }

    let command = match direction {
        TmuxScrollDirection::Up => "scroll-up",
        TmuxScrollDirection::Down => "scroll-down",
    };
    for _ in 0..EMBEDDED_SCROLL_LINES {
        let status = Command::new("tmux")
            .arg("send-keys")
            .arg("-t")
            .arg(target)
            .arg("-X")
            .arg(command)
            .status()
            .map_err(|err| AppError::msg(format!("scroll tmux history: {err}")))?;
        if !status.success() {
            if matches!(direction, TmuxScrollDirection::Down) {
                return Ok(());
            }
            return Err(AppError::msg(format!("scroll tmux history: {status}")));
        }
    }
    Ok(())
}

#[cfg(test)]
fn encode_embedded_mouse_wheel(mouse: MouseEvent, area: Rect) -> Option<Vec<u8>> {
    if !point_in_rect(mouse.column, mouse.row, area) {
        return None;
    }
    let button = match mouse.kind {
        MouseEventKind::ScrollUp => SGR_MOUSE_WHEEL_UP,
        MouseEventKind::ScrollDown => SGR_MOUSE_WHEEL_DOWN,
        _ => return None,
    };
    let column = mouse.column - area.x + 1;
    let row = mouse.row - area.y + 1;
    Some(format!("\x1b[<{button};{column};{row}M").into_bytes())
}

fn scroll_session_preview(app: &mut App, delta: i16) -> bool {
    let previous = app.preview_scroll;
    if delta.is_negative() {
        app.preview_scroll = app.preview_scroll.saturating_sub(delta.unsigned_abs());
    } else {
        app.preview_scroll = app
            .preview_scroll
            .saturating_add(delta as u16)
            .min(PREVIEW_MAX_SCROLL_LINES);
    }
    app.preview_scroll != previous
}

fn focus_selected_session(app: &mut App) {
    if block_selected_pending_operation(app, "open") {
        return;
    }
    if selected_id(app).is_some() {
        app.detail_scroll = 0;
        app.mode = Mode::Session(SendForm::default());
        app.status_message = None;
    } else {
        app.status_message = Some("no session selected".to_string());
    }
}

fn refresh_deck_statuses<S>(app: &mut App, load_deck_statuses: &mut S) -> Result<()>
where
    S: FnMut(&[String]) -> Result<Vec<(String, TuiSessionStatus)>>,
{
    let query = app.query.trim().to_ascii_lowercase();
    let session_ids = app
        .sessions
        .iter()
        .filter(|session| !app.pending_operations.contains_key(&session.id))
        .filter(|session| query.is_empty() || matches_query(session, &query))
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    if session_ids.is_empty() {
        apply_pending_statuses(app);
        return Ok(());
    }

    match load_deck_statuses(&session_ids) {
        Ok(statuses) => {
            for (session_id, deck_status) in statuses {
                app.deck_statuses.insert(session_id, deck_status);
            }
        }
        Err(err) => {
            app.status_message = Some(format!("status failed: {err}"));
        }
    }
    apply_pending_statuses(app);
    Ok(())
}

fn invalidate_session_cache(app: &mut App, session_id: &str) {
    app.deck_statuses.remove(session_id);
    if app.details_session_id.as_deref() == Some(session_id) {
        app.details_session_id = None;
    }
}

fn refresh_details<D>(app: &mut App, load_details: &mut D, force: bool) -> Result<()>
where
    D: FnMut(&str) -> Result<TuiDetails>,
{
    let selected = selected_id(app);
    let Some(session_id) = selected else {
        app.details = TuiDetails::default();
        app.details_session_id = None;
        return Ok(());
    };
    if !force && app.details_session_id.as_deref() == Some(session_id.as_str()) {
        return Ok(());
    }

    if let Some(operation) = app.pending_operations.get(&session_id) {
        let status = operation.status();
        app.deck_statuses.insert(session_id.clone(), status.clone());
        app.details = TuiDetails {
            deck_status: status.deck_status,
            activity: status.activity,
            output: match &operation.state {
                PendingOperationState::Failed(message) => message.clone(),
                PendingOperationState::Running => String::new(),
            },
            workspace: None,
        };
        app.details_session_id = Some(session_id);
        return Ok(());
    }

    match load_details(&session_id) {
        Ok(details) => {
            app.deck_statuses.insert(
                session_id.clone(),
                TuiSessionStatus {
                    deck_status: details.deck_status,
                    activity: details.activity.clone(),
                },
            );
            app.details = details;
            app.details_session_id = Some(session_id);
        }
        Err(err) => {
            app.details = TuiDetails::default();
            app.details_session_id = Some(session_id);
            app.status_message = Some(format!("details failed: {err}"));
        }
    }
    Ok(())
}

fn pending_details_due(app: &App) -> Option<Instant> {
    app.pending_details_refresh.map(|refresh| refresh.due_at)
}

fn schedule_details_refresh(app: &mut App, now: Instant, force: bool) {
    let Some(session_id) = selected_id(app) else {
        app.pending_details_refresh = None;
        return;
    };
    if !force && app.details_session_id.as_deref() == Some(session_id.as_str()) {
        app.pending_details_refresh = None;
        return;
    }

    let force = force
        || app
            .pending_details_refresh
            .is_some_and(|refresh| refresh.force);
    app.pending_details_refresh = Some(PendingDetailsRefresh {
        due_at: now + DETAILS_REFRESH_DEBOUNCE,
        force,
    });
}

fn refresh_details_if_due<D>(app: &mut App, load_details: &mut D, now: Instant) -> Result<bool>
where
    D: FnMut(&str) -> Result<TuiDetails>,
{
    let Some(refresh) = app.pending_details_refresh else {
        return Ok(false);
    };
    if now < refresh.due_at {
        return Ok(false);
    }

    app.pending_details_refresh = None;
    refresh_details(app, load_details, refresh.force)?;
    Ok(true)
}

fn refresh_sessions<F>(app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let selected = selected_id(app);
    app.sessions = if app.search_results_active && !app.query.trim().is_empty() {
        handle_action(TuiAction::Search {
            query: app.query.clone(),
            limit: 200,
        })?
    } else {
        handle_action(TuiAction::Refresh)?
    };
    merge_pending_sessions(app);
    if let Some(id) = selected {
        select_matching_session(app, |session| session.id == id);
    }
    let session_ids = app
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<BTreeSet<_>>();
    app.deck_statuses
        .retain(|session_id, _| session_ids.contains(session_id));
    Ok(())
}

fn handle_normal_key_with_background<F, G>(
    key: KeyEvent,
    app: &mut App,
    handle_action: &mut F,
    spawn_background_action: &mut G,
) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    G: FnMut(BackgroundActionRequest),
{
    if is_plain_char_key(key, 'q')
        || (key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE)
    {
        return Ok(true);
    }

    match key.code {
        KeyCode::Char('?') | KeyCode::Char('h') => app.mode = Mode::Help,
        KeyCode::Char('/') => app.mode = Mode::Search,
        KeyCode::Char('c') => toggle_selected_group(app, handle_action)?,
        KeyCode::Char('e') => expand_groups(app, handle_action)?,
        KeyCode::Char('E') => open_group_settings(app),
        KeyCode::Char('g') => open_tool_settings(app),
        KeyCode::Char('G') => {
            app.mode = Mode::CreateGroup(GroupForm::new());
            app.status_message = None;
        }
        KeyCode::Char('t') => {
            app.status_filter = app.status_filter.next();
            app.selected_index = 0;
            reset_detail_view(app);
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            create_shell_session(app, spawn_background_action)?;
        }
        KeyCode::Char('N') => {
            if block_selected_pending_operation(app, "duplicate") {
                return Ok(false);
            }
            if let Some(session) = app.view().selected {
                app.mode = Mode::New(new_form_for_session(app, &session));
                app.status_message = None;
            } else {
                app.status_message = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('n') => {
            app.mode = Mode::New(new_form_for_agent(app, &app.last_agent));
            app.status_message = None;
        }
        KeyCode::Char('m') => {
            if block_selected_pending_operation(app, "move") {
                return Ok(false);
            }
            if let Some(session) = app.view().selected {
                app.mode = Mode::Move(MoveForm::for_session(&session));
                app.status_message = None;
            } else {
                app.status_message = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('r') => {
            if block_selected_pending_operation(app, "rename") {
                return Ok(false);
            }
            if let Some(session) = app.view().selected {
                app.mode = Mode::Rename(RenameForm::for_session(&session));
                app.status_message = None;
            } else {
                app.status_message = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('d') => run_selected_action_with_background(
            app,
            handle_action,
            spawn_background_action,
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::MetadataOnly,
            },
            "deleted",
        )?,
        KeyCode::Char('D') => run_selected_action_with_background(
            app,
            handle_action,
            spawn_background_action,
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::CleanupWorktree,
            },
            "deleted and cleaned up",
        )?,
        KeyCode::Char('f') => {
            if block_selected_pending_operation(app, "fork") {
                return Ok(false);
            }
            if let Some(session) = app.view().selected {
                app.mode = Mode::Fork(ForkForm::for_session(&session));
                app.status_message = None;
            }
        }
        KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(5),
        KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(5),
        KeyCode::Down | KeyCode::Char('j') => {
            let previous = app.selected_index;
            app.selected_index = app
                .selected_index
                .saturating_add(1)
                .min(app.view().visible_count.saturating_sub(1));
            if app.selected_index != previous {
                reset_detail_view(app);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let previous = app.selected_index;
            app.selected_index = app.selected_index.saturating_sub(1);
            if app.selected_index != previous {
                reset_detail_view(app);
            }
        }
        _ => {}
    }
    Ok(false)
}

#[cfg(test)]
fn handle_normal_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut background_requests = Vec::new();
    let result = handle_normal_key_with_background(key, app, handle_action, &mut |request| {
        background_requests.push(request)
    })?;
    complete_background_requests_synchronously(app, handle_action, background_requests)?;
    Ok(result)
}

fn reset_detail_view(app: &mut App) {
    app.detail_scroll = 0;
    app.preview_scroll = 0;
}

fn open_tool_settings(app: &mut App) {
    app.mode = Mode::ToolSettings(ToolSettingsForm::new(&app.tool_settings, &app.last_agent));
    app.status_message = None;
}

fn open_group_settings(app: &mut App) {
    let Some(session) = app.view().selected else {
        app.status_message = Some("no group selected".to_string());
        return;
    };
    let Some(group) = app.group_defaults.get(&session.group_name) else {
        app.status_message = Some(format!("group not found: {}", session.group_name));
        return;
    };
    app.mode = Mode::GroupSettings(GroupSettingsForm::from_group(group));
    app.status_message = None;
}

fn create_shell_session<G>(app: &mut App, spawn_background_action: &mut G) -> Result<()>
where
    G: FnMut(BackgroundActionRequest),
{
    let selected = app.view().selected;
    let path = selected
        .as_ref()
        .map(|session| session.project_path.clone())
        .unwrap_or_else(default_new_session_path);
    let group_name = selected
        .as_ref()
        .map(|session| session.group_name.clone())
        .unwrap_or_else(|| "default".to_string());
    let name = generated_session_name();
    let request = CreateSession {
        path,
        agent: "shell".to_string(),
        command: String::new(),
        name: name.clone(),
        group_name: group_name.clone(),
        worktree: None,
        carry_state: false,
        sandbox: false,
        prompt: None,
        parent_session_id: None,
    };
    start_create_operation(
        app,
        request,
        "created shell session",
        "create shell failed",
        spawn_background_action,
    );
    Ok(())
}

fn start_create_operation<G>(
    app: &mut App,
    request: CreateSession,
    success_message: &str,
    failure_prefix: &str,
    spawn_background_action: &mut G,
) where
    G: FnMut(BackgroundActionRequest),
{
    let pending_id = next_pending_session_id(app);
    let placeholder = pending_create_session(pending_id.clone(), &request);
    let label = if request.worktree.is_some() {
        "creating worktree"
    } else {
        "creating"
    };
    let operation = PendingOperation {
        kind: PendingOperationKind::Create,
        session: placeholder.clone(),
        state: PendingOperationState::Running,
        running_label: label.to_string(),
        failed_label: "create failed".to_string(),
        failure_prefix: failure_prefix.to_string(),
        success_message: success_message.to_string(),
        expected_name: Some(placeholder.name.clone()),
        expected_group: Some(placeholder.group_name.clone()),
        parent_session_id: None,
        delete_selection_target: None,
    };
    app.pending_operations.insert(pending_id.clone(), operation);
    app.sessions.push(placeholder);
    app.query.clear();
    app.search_results_active = false;
    app.last_agent = request.agent.clone();
    app.mode = Mode::Normal;
    app.status_message = Some(label.to_string());
    apply_pending_statuses(app);
    select_matching_session(app, |session| session.id == pending_id);
    spawn_background_action(BackgroundActionRequest {
        session_id: pending_id,
        action: TuiAction::Create(request),
    });
}

fn toggle_selected_group<F>(app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let view = app.view();
    let Some(group_name) = view
        .selected
        .as_ref()
        .map(|session| session.group_name.clone())
        .or_else(|| view.groups.first().map(|group| group.name.clone()))
    else {
        app.status_message = Some("no group selected".to_string());
        return Ok(());
    };

    let sessions = handle_action(TuiAction::SetGroupCollapsed {
        group_name: group_name.clone(),
        collapsed: true,
    })?;
    app.collapsed_groups.insert(group_name.clone());
    app.sessions = sessions;
    app.selected_index = app
        .selected_index
        .min(app.view().visible_count.saturating_sub(1));
    app.detail_scroll = 0;
    app.status_message = Some(format!("collapsed group {group_name}"));
    Ok(())
}

fn expand_groups<F>(app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    if app.collapsed_groups.is_empty() {
        app.status_message = Some("no collapsed groups".to_string());
        return Ok(());
    }
    let groups = app.collapsed_groups.iter().cloned().collect::<Vec<_>>();
    for group_name in groups {
        app.sessions = handle_action(TuiAction::SetGroupCollapsed {
            group_name,
            collapsed: false,
        })?;
    }
    app.collapsed_groups.clear();
    app.detail_scroll = 0;
    app.status_message = Some("expanded groups".to_string());
    Ok(())
}

fn handle_search_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            let query = app.query.trim().to_string();
            if query.is_empty() {
                app.search_results_active = false;
                app.sessions = handle_action(TuiAction::Refresh)?;
                app.status_message = None;
            } else {
                match handle_action(TuiAction::Search { query, limit: 200 }) {
                    Ok(sessions) => {
                        let count = sessions.len();
                        app.sessions = sessions;
                        app.search_results_active = true;
                        app.selected_index = 0;
                        reset_detail_view(app);
                        app.status_message = Some(format!("{count} search results"));
                    }
                    Err(err) => app.status_message = Some(format!("search failed: {err}")),
                }
            }
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            app.query.pop();
            app.search_results_active = false;
            app.selected_index = 0;
            reset_detail_view(app);
        }
        KeyCode::Char(ch) => {
            app.query.push(ch);
            app.search_results_active = false;
            app.selected_index = 0;
            reset_detail_view(app);
        }
        _ => {}
    }
    Ok(())
}

fn handle_help_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::Char('q') => {
            app.mode = Mode::Normal
        }
        _ => {}
    }
}

fn handle_new_key_with_background<G>(
    key: KeyEvent,
    app: &mut App,
    spawn_background_action: &mut G,
) -> Result<()>
where
    G: FnMut(BackgroundActionRequest),
{
    let mut submit = false;
    let agent_choices = app.agent_choices.clone();
    let tool_settings = app.tool_settings.clone();
    let group_default_paths = app.group_default_paths.clone();
    let group_defaults = app.group_defaults.clone();

    if let Mode::New(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => {
                let apply_group_path = form.current_field() == NewField::Group;
                form.next_field();
                if apply_group_path {
                    apply_group_defaults_from_maps(form, &group_defaults, &tool_settings);
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                let apply_group_path = form.current_field() == NewField::Group;
                form.previous_field();
                if apply_group_path {
                    apply_group_defaults_from_maps(form, &group_defaults, &tool_settings);
                }
            }
            KeyCode::Left if form.current_field() == NewField::Group => {
                cycle_group_name(&mut form.group_name, &group_default_paths, -1);
                apply_group_defaults_from_maps(form, &group_defaults, &tool_settings);
            }
            KeyCode::Right if form.current_field() == NewField::Group => {
                cycle_group_name(&mut form.group_name, &group_default_paths, 1);
                apply_group_defaults_from_maps(form, &group_defaults, &tool_settings);
            }
            KeyCode::Left if form.current_field() == NewField::Agent => {
                form.cycle_agent(&agent_choices, -1);
                form.worktree = tool_creates_worktree_by_default(&tool_settings, &form.agent);
            }
            KeyCode::Right if form.current_field() == NewField::Agent => {
                form.cycle_agent(&agent_choices, 1);
                form.worktree = tool_creates_worktree_by_default(&tool_settings, &form.agent);
            }
            KeyCode::Enter => {
                let apply_group_path = form.current_field() == NewField::Group;
                if form.field_index == NEW_FIELDS.len() - 1 {
                    submit = true;
                } else {
                    form.next_field();
                }
                if apply_group_path {
                    apply_group_default_path(form, &group_default_paths);
                }
            }
            KeyCode::Backspace => {
                if let Some(value) = form.current_value_mut() {
                    value.pop();
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if form.current_field() == NewField::Group {
                    apply_group_default_path(form, &group_default_paths);
                }
                submit = true;
            }
            KeyCode::Char(' ') if form.current_field() == NewField::Group => {
                cycle_group_name(&mut form.group_name, &group_default_paths, 1);
                apply_group_default_path(form, &group_default_paths);
            }
            KeyCode::Char(' ') if form.current_field() == NewField::Agent => {
                form.cycle_agent(&agent_choices, 1);
                form.worktree = tool_creates_worktree_by_default(&tool_settings, &form.agent);
            }
            KeyCode::Char(' ') if form.current_field() == NewField::Worktree => {
                form.worktree = !form.worktree;
            }
            KeyCode::Char(' ') if form.current_field() == NewField::CarryState => {
                form.carry_state = !form.carry_state;
            }
            KeyCode::Char(ch) => {
                if let Some(value) = form.current_value_mut() {
                    value.push(ch);
                }
            }
            _ => {}
        }
    }

    if submit {
        let request = match &app.mode {
            Mode::New(form) => match form.build_request() {
                Ok(request) => request,
                Err(message) => {
                    set_new_form_launching(app, false);
                    app.status_message = Some(message.to_string());
                    return Ok(());
                }
            },
            _ => return Ok(()),
        };

        start_create_operation(
            app,
            request,
            "created session",
            "create failed",
            spawn_background_action,
        );
    }

    Ok(())
}

#[cfg(test)]
fn handle_new_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut background_requests = Vec::new();
    handle_new_key_with_background(key, app, &mut |request| background_requests.push(request))?;
    complete_background_requests_synchronously(app, handle_action, background_requests)
}

fn handle_create_group_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    if let Mode::CreateGroup(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Enter => {
                if form.current_field() == *GROUP_FIELDS.last().unwrap() {
                    submit = true;
                } else {
                    form.next_field();
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                submit = true;
            }
            KeyCode::Backspace => {
                form.current_value_mut().pop();
            }
            KeyCode::Char(ch) => {
                form.current_value_mut().push(ch);
            }
            _ => {}
        }
    }

    if submit {
        let (name, default_project_path) = match &app.mode {
            Mode::CreateGroup(form) => (
                form.name.trim().to_string(),
                form.default_project_path.trim().to_string(),
            ),
            _ => return Ok(()),
        };
        if name.is_empty() {
            app.status_message = Some("group name required".to_string());
            return Ok(());
        }
        if app.group_default_paths.contains_key(&name) {
            app.status_message = Some("group already exists".to_string());
            return Ok(());
        }

        match handle_action(TuiAction::CreateGroup {
            name: name.clone(),
            default_project_path: default_project_path.clone(),
        }) {
            Ok(sessions) => {
                app.group_default_paths
                    .insert(name.clone(), default_project_path.clone());
                app.group_defaults.insert(
                    name.clone(),
                    GroupDefaults {
                        name: name.clone(),
                        default_project_path,
                        default_agent: None,
                        default_worktree: None,
                        default_carry_state: None,
                    },
                );
                app.sessions = sessions;
                app.mode = Mode::Normal;
                app.status_message = Some(format!("created group {name}"));
            }
            Err(err) => {
                app.status_message = Some(format!("create group failed: {err}"));
            }
        }
    }
    Ok(())
}

fn handle_group_settings_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    let agent_choices = app.agent_choices.clone();
    if let Mode::GroupSettings(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Enter => {
                if form.field_index == GROUP_SETTINGS_FIELDS.len() - 1 {
                    submit = true;
                } else {
                    form.next_field();
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                submit = true;
            }
            KeyCode::Left if form.current_field() == GroupSettingsField::DefaultAgent => {
                form.cycle_agent(&agent_choices, -1);
            }
            KeyCode::Right if form.current_field() == GroupSettingsField::DefaultAgent => {
                form.cycle_agent(&agent_choices, 1);
            }
            KeyCode::Left if form.current_field() == GroupSettingsField::DefaultWorktree => {
                form.cycle_worktree(-1);
            }
            KeyCode::Right | KeyCode::Char(' ')
                if form.current_field() == GroupSettingsField::DefaultWorktree =>
            {
                form.cycle_worktree(1);
            }
            KeyCode::Left if form.current_field() == GroupSettingsField::DefaultCarryState => {
                form.cycle_carry_state(-1);
            }
            KeyCode::Right | KeyCode::Char(' ')
                if form.current_field() == GroupSettingsField::DefaultCarryState =>
            {
                form.cycle_carry_state(1);
            }
            KeyCode::Backspace => {
                if let Some(value) = form.current_value_mut() {
                    value.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(value) = form.current_value_mut() {
                    value.push(ch);
                }
            }
            _ => {}
        }
    }

    if submit {
        let (name, update) = match &app.mode {
            Mode::GroupSettings(form) => (form.name.clone(), form.update()),
            _ => return Ok(()),
        };
        match handle_action(TuiAction::UpdateGroup {
            name: name.clone(),
            update: update.clone(),
        }) {
            Ok(sessions) => {
                app.sessions = sessions;
                if let Some(path) = update.default_project_path {
                    app.group_default_paths.insert(name.clone(), path.clone());
                    app.group_defaults.insert(
                        name.clone(),
                        GroupDefaults {
                            name: name.clone(),
                            default_project_path: path,
                            default_agent: update.default_agent.flatten(),
                            default_worktree: update.default_worktree.flatten(),
                            default_carry_state: update.default_carry_state.flatten(),
                        },
                    );
                }
                app.mode = Mode::Normal;
                app.status_message = Some(format!("updated group {name}"));
            }
            Err(err) => {
                app.status_message = Some(format!("update group failed: {err}"));
            }
        }
    }

    Ok(())
}

fn handle_fork_key_with_background<G>(
    key: KeyEvent,
    app: &mut App,
    spawn_background_action: &mut G,
) -> Result<()>
where
    G: FnMut(BackgroundActionRequest),
{
    let mut submit = false;

    if let Mode::Fork(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Enter => {
                if form.field_index == FORK_FIELDS.len() - 1 {
                    submit = true;
                } else {
                    form.next_field();
                }
            }
            KeyCode::Backspace => {
                if let Some(value) = form.current_value_mut() {
                    value.pop();
                }
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                submit = true;
            }
            KeyCode::Char(' ') if form.current_field() == ForkField::CarryState => {
                form.carry_state = !form.carry_state;
            }
            KeyCode::Char(ch) => {
                if let Some(value) = form.current_value_mut() {
                    value.push(ch);
                }
            }
            _ => {}
        }
    }

    if submit {
        let request = match &app.mode {
            Mode::Fork(form) => match form.build_request() {
                Ok(request) => request,
                Err(message) => {
                    app.status_message = Some(message.to_string());
                    return Ok(());
                }
            },
            _ => return Ok(()),
        };
        start_fork_operation(app, request, spawn_background_action);
    }

    Ok(())
}

#[cfg(test)]
fn handle_fork_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut background_requests = Vec::new();
    handle_fork_key_with_background(key, app, &mut |request| background_requests.push(request))?;
    complete_background_requests_synchronously(app, handle_action, background_requests)
}

fn start_fork_operation<G>(
    app: &mut App,
    request: ForkSessionRequest,
    spawn_background_action: &mut G,
) where
    G: FnMut(BackgroundActionRequest),
{
    let Some(parent) = find_session_record(app, &request.parent_session_id) else {
        app.status_message = Some("parent session not found".to_string());
        return;
    };
    let pending_id = next_pending_session_id(app);
    let placeholder = pending_fork_session(pending_id.clone(), &request, &parent);
    let label = if request.worktree_branch.is_some() {
        "creating worktree"
    } else {
        "creating"
    };
    let operation = PendingOperation {
        kind: PendingOperationKind::Fork,
        session: placeholder.clone(),
        state: PendingOperationState::Running,
        running_label: label.to_string(),
        failed_label: "fork failed".to_string(),
        failure_prefix: "fork failed".to_string(),
        success_message: "forked session".to_string(),
        expected_name: Some(placeholder.name.clone()),
        expected_group: Some(placeholder.group_name.clone()),
        parent_session_id: Some(parent.id.clone()),
        delete_selection_target: None,
    };
    app.pending_operations.insert(pending_id.clone(), operation);
    app.sessions.push(placeholder);
    app.query.clear();
    app.search_results_active = false;
    app.mode = Mode::Normal;
    app.status_message = Some(label.to_string());
    apply_pending_statuses(app);
    select_matching_session(app, |session| session.id == pending_id);
    spawn_background_action(BackgroundActionRequest {
        session_id: pending_id,
        action: TuiAction::Fork(request),
    });
}

fn select_matching_session(app: &mut App, matches: impl FnMut(&SessionSummary) -> bool) {
    let previous = selected_id(app);
    let view = app.view();
    if let Some(index) = view.visible_sessions.iter().position(matches) {
        let next_id = view.visible_sessions[index].id.clone();
        app.selected_index = index;
        if previous.as_deref() != Some(next_id.as_str()) {
            reset_detail_view(app);
        }
        return;
    }
    app.selected_index = 0;
    if previous.is_some() {
        reset_detail_view(app);
    }
}

fn handle_session_key(key: KeyEvent, app: &mut App) -> Result<()> {
    if is_plain_ctrl_key(key, 'q') {
        app.embedded = None;
        app.mode = Mode::Normal;
        app.status_message = Some("returned to dashboard".to_string());
        return Ok(());
    }
    if is_plain_ctrl_key(key, 't') {
        toggle_mouse_capture(app);
        return Ok(());
    }

    if let Some(embedded) = app.embedded.as_mut() {
        embedded.write_key(key)?;
    }
    Ok(())
}

fn handle_move_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    if let Mode::Move(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Enter => submit = true,
            KeyCode::Backspace => {
                form.group_name.pop();
            }
            KeyCode::Char(ch) => form.group_name.push(ch),
            _ => {}
        }
    }

    if submit {
        let (session_id, group_name) = match &app.mode {
            Mode::Move(form) => (form.session_id.clone(), form.group_name.trim().to_string()),
            _ => return Ok(()),
        };
        if group_name.is_empty() {
            app.status_message = Some("group is required".to_string());
            return Ok(());
        }
        if !app.group_default_paths.contains_key(&group_name) {
            app.status_message = Some("group must already exist".to_string());
            return Ok(());
        }

        match handle_action(TuiAction::MoveToGroup {
            session_id: session_id.clone(),
            group_name,
        }) {
            Ok(sessions) => {
                app.sessions = sessions;
                if app.search_results_active && !app.query.trim().is_empty() {
                    refresh_sessions(app, handle_action)?;
                }
                select_matching_session(app, |session| session.id == session_id);
                app.mode = Mode::Normal;
                app.status_message = Some("moved session".to_string());
            }
            Err(err) => app.status_message = Some(format!("move failed: {err}")),
        }
    }
    Ok(())
}

fn handle_rename_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    if let Mode::Rename(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Enter => submit = true,
            KeyCode::Backspace => {
                form.name.pop();
            }
            KeyCode::Char(ch) => form.name.push(ch),
            _ => {}
        }
    }

    if submit {
        let (session_id, name) = match &app.mode {
            Mode::Rename(form) => (form.session_id.clone(), form.name.trim().to_string()),
            _ => return Ok(()),
        };
        if name.is_empty() {
            app.status_message = Some("name required".to_string());
            return Ok(());
        }
        match handle_action(TuiAction::RenameSession {
            session_id: session_id.clone(),
            name,
        }) {
            Ok(sessions) => {
                app.sessions = sessions;
                if app.search_results_active && !app.query.trim().is_empty() {
                    refresh_sessions(app, handle_action)?;
                }
                select_matching_session(app, |session| session.id == session_id);
                app.mode = Mode::Normal;
                app.status_message = Some("renamed session".to_string());
            }
            Err(err) => app.status_message = Some(format!("rename failed: {err}")),
        }
    }
    Ok(())
}

fn handle_tool_settings_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    if let Mode::ToolSettings(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Left => match form.current_field() {
                ToolSettingsField::Tool => form.cycle_tool(-1),
                ToolSettingsField::Worktree => form.cycle_worktree(-1),
                _ => {}
            },
            KeyCode::Right => match form.current_field() {
                ToolSettingsField::Tool => form.cycle_tool(1),
                ToolSettingsField::Worktree => form.cycle_worktree(1),
                _ => {}
            },
            KeyCode::Enter => match form.current_field() {
                ToolSettingsField::Installed => form.toggle_installed(),
                ToolSettingsField::Tool => form.cycle_tool(1),
                ToolSettingsField::Worktree => form.cycle_worktree(1),
                _ if form.field_index + 1 == TOOL_SETTINGS_FIELDS.len() => submit = true,
                _ => form.next_field(),
            },
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => submit = true,
            KeyCode::Char(' ') if form.current_field() == ToolSettingsField::Installed => {
                form.toggle_installed();
            }
            KeyCode::Backspace => {
                if let Some(value) = form.current_value_mut() {
                    value.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(value) = form.current_value_mut() {
                    value.push(ch);
                }
            }
            _ => {}
        }
    }
    if submit {
        let settings = match &app.mode {
            Mode::ToolSettings(form) => form.clone().settings(),
            _ => return Ok(()),
        };
        match handle_action(TuiAction::SaveToolSettings(settings.clone())) {
            Ok(sessions) => {
                app.sessions = sessions;
                app.tool_settings = normalize_tool_settings(settings);
                app.agent_choices = agent_choices_from_tool_settings(&app.tool_settings);
                app.mode = Mode::Normal;
                app.status_message = Some("saved tool settings".to_string());
            }
            Err(err) => {
                app.status_message = Some(format!("save settings failed: {err}"));
            }
        }
    }
    Ok(())
}

fn run_selected_action_with_background<F, G, M>(
    app: &mut App,
    handle_action: &mut F,
    spawn_background_action: &mut G,
    build: M,
    done: &str,
) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    G: FnMut(BackgroundActionRequest),
    M: FnOnce(String) -> TuiAction,
{
    let Some(session_id) = selected_id(app) else {
        app.status_message = Some("no session selected".to_string());
        return Ok(());
    };
    let selected_session_id = session_id.clone();
    let action = build(session_id);
    if let TuiAction::Remove { session_id, mode } = &action {
        start_remove_operation(
            app,
            session_id.clone(),
            *mode,
            done,
            spawn_background_action,
        );
        return Ok(());
    }
    if block_selected_pending_operation(app, done) {
        return Ok(());
    }
    let target_session_id = match &action {
        TuiAction::Remove { .. } => delete_selection_target(app, &selected_session_id),
        _ => Some(selected_session_id.clone()),
    };
    match handle_action(action) {
        Ok(sessions) => {
            app.sessions = sessions;
            invalidate_session_cache(app, &selected_session_id);
            if app.search_results_active && !app.query.trim().is_empty() {
                refresh_sessions(app, handle_action)?;
            }
            if let Some(target_session_id) = target_session_id {
                select_matching_session(app, |session| session.id == target_session_id);
            } else {
                select_matching_session(app, |_| false);
            }
            app.detail_scroll = 0;
            app.status_message = Some(done.to_string());
        }
        Err(err) => app.status_message = Some(format!("{done} failed: {err}")),
    }
    Ok(())
}

#[cfg(test)]
fn run_selected_action<F, M>(
    app: &mut App,
    handle_action: &mut F,
    build: M,
    done: &str,
) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    M: FnOnce(String) -> TuiAction,
{
    let Some(session_id) = selected_id(app) else {
        app.status_message = Some("no session selected".to_string());
        return Ok(());
    };
    let selected_session_id = session_id.clone();
    let action = build(session_id);
    let target_session_id = match &action {
        TuiAction::Remove { .. } => delete_selection_target(app, &selected_session_id),
        _ => Some(selected_session_id.clone()),
    };
    match handle_action(action) {
        Ok(sessions) => {
            app.sessions = sessions;
            invalidate_session_cache(app, &selected_session_id);
            if app.search_results_active && !app.query.trim().is_empty() {
                refresh_sessions(app, handle_action)?;
            }
            if let Some(target_session_id) = target_session_id {
                select_matching_session(app, |session| session.id == target_session_id);
            } else {
                select_matching_session(app, |_| false);
                app.detail_scroll = 0;
            }
            app.status_message = Some(done.to_string());
        }
        Err(err) => app.status_message = Some(format!("{done} failed: {err}")),
    }
    Ok(())
}

fn start_remove_operation<G>(
    app: &mut App,
    session_id: String,
    mode: DeleteMode,
    done: &str,
    spawn_background_action: &mut G,
) where
    G: FnMut(BackgroundActionRequest),
{
    if dismiss_failed_placeholder(app, &session_id) {
        return;
    }

    if let Some(operation) = app.pending_operations.get(&session_id) {
        if operation.is_running() {
            app.status_message = Some(format!("delete blocked: {}", operation.running_label));
            return;
        }
    }

    let label = match mode {
        DeleteMode::CleanupWorktree => "deleting worktree",
        DeleteMode::MetadataOnly | DeleteMode::Purge => "deleting",
    };
    let failed_label = match mode {
        DeleteMode::CleanupWorktree => "delete cleanup failed",
        DeleteMode::MetadataOnly | DeleteMode::Purge => "delete failed",
    };
    let session = app
        .pending_operations
        .get(&session_id)
        .map(|operation| operation.session.clone())
        .or_else(|| find_session_record(app, &session_id));
    let Some(session) = session else {
        app.status_message = Some("no session selected".to_string());
        return;
    };
    let target = delete_selection_target(app, &session_id);
    let operation = PendingOperation {
        kind: PendingOperationKind::Delete,
        session,
        state: PendingOperationState::Running,
        running_label: label.to_string(),
        failed_label: failed_label.to_string(),
        failure_prefix: failed_label.to_string(),
        success_message: done.to_string(),
        expected_name: None,
        expected_group: None,
        parent_session_id: None,
        delete_selection_target: target,
    };
    app.pending_operations.insert(session_id.clone(), operation);
    apply_pending_statuses(app);
    app.status_message = Some(label.to_string());
    spawn_background_action(BackgroundActionRequest {
        session_id: session_id.clone(),
        action: TuiAction::Remove { session_id, mode },
    });
}

fn dismiss_failed_placeholder(app: &mut App, session_id: &str) -> bool {
    let should_dismiss = app
        .pending_operations
        .get(session_id)
        .is_some_and(PendingOperation::is_failed_placeholder);
    if !should_dismiss {
        return false;
    }
    app.pending_operations.remove(session_id);
    app.sessions.retain(|session| session.id != session_id);
    app.deck_statuses.remove(session_id);
    select_matching_session(app, |_| false);
    app.status_message = Some("dismissed failed create".to_string());
    true
}

fn delete_selection_target(app: &App, session_id: &str) -> Option<String> {
    let view = app.view();
    let index = view
        .visible_sessions
        .iter()
        .position(|session| session.id == session_id)?;
    let group_name = view.visible_sessions[index].group_name.as_str();
    let first_in_group =
        index == 0 || view.visible_sessions[index - 1].group_name.as_str() != group_name;
    let target = if first_in_group {
        view.visible_sessions.get(index + 1).or_else(|| {
            index
                .checked_sub(1)
                .and_then(|index| view.visible_sessions.get(index))
        })
    } else {
        index
            .checked_sub(1)
            .and_then(|index| view.visible_sessions.get(index))
            .or_else(|| view.visible_sessions.get(index + 1))
    };
    target.map(|session| session.id.clone())
}

fn selected_id(app: &App) -> Option<String> {
    app.view().selected.map(|session| session.id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BodyLayout {
    sidebar: Rect,
    divider: Rect,
    detail: Rect,
}

fn body_layout_for_view(
    area: Rect,
    view: &DashboardView,
    sidebar_width: Option<u16>,
    animation_frame: usize,
) -> BodyLayout {
    let width = sidebar_width.unwrap_or_else(|| auto_sidebar_width(area, view, animation_frame));
    body_layout(area, width)
}

fn initialize_sidebar_width(app: &mut App, size: ratatui::layout::Size) {
    if app.sidebar_width.is_some() {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(Rect::new(0, 0, size.width, size.height));
    let view = app.view();
    app.sidebar_width = Some(auto_sidebar_width(chunks[1], &view, app.animation_frame));
}

fn body_layout(area: Rect, sidebar_width: u16) -> BodyLayout {
    let sidebar_width = clamp_sidebar_width(area, sidebar_width);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    BodyLayout {
        sidebar: chunks[0],
        divider: chunks[1],
        detail: chunks[2],
    }
}

fn auto_sidebar_width(area: Rect, view: &DashboardView, animation_frame: usize) -> u16 {
    clamp_sidebar_width(
        area,
        sidebar_content_width(view, animation_frame).saturating_add(SIDEBAR_LIST_PADDING),
    )
}

fn clamp_sidebar_width(area: Rect, width: u16) -> u16 {
    let max_width = area
        .width
        .saturating_mul(MAX_SIDEBAR_PERCENT)
        .saturating_div(100)
        .min(area.width.saturating_sub(1));
    if max_width == 0 {
        return 0;
    }
    let min_width = MIN_SIDEBAR_WIDTH.min(max_width);
    width.clamp(min_width, max_width)
}

fn sidebar_content_width(view: &DashboardView, animation_frame: usize) -> u16 {
    let mut width = if view.rows.is_empty() {
        display_width("No sessions. Press n to create one.")
    } else {
        0
    };
    for row in &view.rows {
        let row_width = match row {
            DashboardRow::Group {
                group_index,
                name,
                collapsed,
                session_count,
                counts,
            } => display_width(&group_header_row_text(
                *group_index,
                name,
                *collapsed,
                *session_count,
                *counts,
            )),
            DashboardRow::Session {
                session,
                last_in_group,
            } => session_row_width(session, *last_in_group, animation_frame),
        };
        width = width.max(row_width);
    }
    width
}

fn display_width(value: &str) -> u16 {
    let width = Line::from(value).width();
    u16::try_from(width).unwrap_or(u16::MAX)
}

fn group_header_text(group_index: usize, group: &SessionGroup) -> String {
    group_header_row_text(
        group_index,
        &group.name,
        group.collapsed,
        group.sessions.len(),
        group_status_counts(group),
    )
}

fn group_header_row_text(
    group_index: usize,
    group_name: &str,
    collapsed: bool,
    session_count: usize,
    counts: StatusCounts,
) -> String {
    use std::fmt::Write as _;

    let collapse_mark = if collapsed { "+" } else { "-" };
    let mut text = format!(
        "{}. {} ({}) {}",
        group_index + 1,
        group_name,
        session_count,
        collapse_mark
    );
    for (marker, count) in [
        ("◆", counts.occupied),
        ("✦", counts.thinking),
        ("●", counts.running),
        ("▸", counts.starting),
        ("⋯", counts.queued),
        ("◌", counts.waiting),
        ("·", counts.idle),
        ("■", counts.stopped),
        ("×", counts.errored),
    ] {
        if count > 0 {
            let _ = write!(text, " {marker}{count}");
        }
    }
    text
}

fn mouse_on_divider(mouse: MouseEvent, area: Rect, app: &App) -> bool {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let view = app.view();
    let body = body_layout_for_view(chunks[1], &view, app.sidebar_width, app.animation_frame);

    let hit_start = body.divider.x.saturating_sub(DIVIDER_HIT_COLUMNS);
    let hit_end = body
        .divider
        .x
        .saturating_add(body.divider.width)
        .saturating_add(DIVIDER_HIT_COLUMNS);

    mouse.row >= body.divider.y
        && mouse.row < body.divider.y.saturating_add(body.divider.height)
        && mouse.column >= hit_start
        && mouse.column < hit_end
}

fn mouse_on_session_preview(mouse: MouseEvent, area: Rect, app: &App) -> bool {
    if !matches!(app.mode, Mode::Normal | Mode::Search) {
        return false;
    }
    let view = app.view();
    let Some(session) = view.selected.as_ref() else {
        return false;
    };
    if !session_is_live(session.status) {
        return false;
    }
    let preview = terminal_preview_area(
        ratatui::layout::Size {
            width: area.width,
            height: area.height,
        },
        &view,
        app.sidebar_width,
    );
    point_in_rect(mouse.column, mouse.row, preview)
}

fn mouse_on_embedded_session(mouse: MouseEvent, area: Rect, app: &App) -> bool {
    if !matches!(app.mode, Mode::Session(_)) || app.embedded.is_none() {
        return false;
    }
    let embedded = embedded_terminal_area(
        ratatui::layout::Size {
            width: area.width,
            height: area.height,
        },
        &app.view(),
        app.sidebar_width,
    );
    point_in_rect(mouse.column, mouse.row, embedded)
}

fn point_in_rect(column: u16, row: u16, area: Rect) -> bool {
    row >= area.y
        && row < area.y.saturating_add(area.height)
        && column >= area.x
        && column < area.x.saturating_add(area.width)
}

fn set_sidebar_width_from_mouse(app: &mut App, column: u16, area: Rect) -> bool {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let body = chunks[1];
    if body.width == 0 {
        return false;
    }

    let relative = column.saturating_sub(body.x).min(body.width);
    let width = clamp_sidebar_width(body, relative);
    let changed = app.sidebar_width != Some(width);
    app.sidebar_width = Some(width);
    changed
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let view = app.view();
    frame.render_widget(Block::default().style(base_style()), frame.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let body = body_layout_for_view(chunks[1], &view, app.sidebar_width, app.animation_frame);

    frame.render_widget(header(app, &view), chunks[0]);
    let (sessions, mut session_state) = session_list(&view, app.animation_frame);
    frame.render_stateful_widget(sessions, body.sidebar, &mut session_state);
    frame.render_widget(divider(app.resizing_sidebar), body.divider);
    match &app.mode {
        Mode::New(form) => render_create_form(
            frame,
            form,
            &app.agent_choices,
            &app.group_default_paths,
            &app.status_message,
            body.detail,
        ),
        Mode::CreateGroup(form) => {
            frame.render_widget(create_group_form(form, &app.status_message), body.detail)
        }
        Mode::GroupSettings(form) => frame.render_widget(
            group_settings_form(form, &app.agent_choices, &app.status_message),
            body.detail,
        ),
        Mode::Fork(form) => frame.render_widget(fork_form(form, &app.status_message), body.detail),
        Mode::Session(_) => render_session_terminal(frame, app, &view, body.detail),
        Mode::Move(form) => frame.render_widget(move_form(form, &app.status_message), body.detail),
        Mode::Rename(form) => {
            frame.render_widget(rename_form(form, &app.status_message), body.detail)
        }
        Mode::ToolSettings(form) => {
            frame.render_widget(detail_panel(app, &view), body.detail);
            let popup = tool_settings_popup_area(frame.area());
            frame.render_widget(Clear, popup);
            frame.render_widget(tool_settings_form(form, &app.status_message), popup);
        }
        Mode::Help => frame.render_widget(help_panel(), body.detail),
        _ => render_dashboard_detail(frame, app, &view, body.detail),
    }
    frame.render_widget(footer(app), chunks[2]);
}

fn render_create_form(
    frame: &mut Frame<'_>,
    form: &NewForm,
    agent_choices: &[String],
    group_default_paths: &BTreeMap<String, String>,
    status_message: &Option<String>,
    area: Rect,
) {
    frame.render_widget(
        create_form(form, agent_choices, group_default_paths, status_message),
        area,
    );
    if let Some((x, y)) = new_form_cursor_position(form, area) {
        frame.set_cursor_position((x, y));
    }
}

fn tool_settings_popup_area(area: Rect) -> Rect {
    let width = (area.width.saturating_mul(3) / 5).max(48).min(area.width);
    let height = area.height.saturating_sub(2).clamp(1, 12).min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn divider(active: bool) -> Paragraph<'static> {
    let style = if active { title_style() } else { muted_style() };
    Paragraph::new("|").style(style)
}

fn header(app: &App, view: &DashboardView) -> Paragraph<'static> {
    let counts = view_status_counts(view);
    let search = if app.query.trim().is_empty() {
        "all".to_string()
    } else {
        format!("/{}", app.query)
    };
    let message = app.status_message.as_deref().unwrap_or("");
    let mut filters = vec![
        Span::styled(app.status_filter.label(), badge_style()),
        Span::raw(format!(
            "  {} occupied  {} thinking  {} queued  {} waiting  {} idle",
            counts.occupied, counts.thinking, counts.queued, counts.waiting, counts.idle
        )),
        Span::raw(format!("  {} visible", view.visible_count)),
        Span::raw(format!("  filter {search}")),
    ];
    if !message.is_empty() {
        filters.push(Span::raw("  "));
        filters.push(Span::styled(message.to_string(), accent_style()));
    }

    Paragraph::new(vec![
        Line::from(vec![
            Span::styled("< ", muted_style()),
            Span::styled("Agent Helm", accent_style().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                "  x{} ~{} *{} >{} o{} !{}",
                counts.occupied,
                counts.thinking,
                counts.running,
                counts.starting,
                counts.stopped,
                counts.errored
            )),
            Span::styled(format!("  v{}", env!("CARGO_PKG_VERSION")), muted_style()),
            app.headroom_metrics
                .as_ref()
                .map(|metrics| Span::styled(metrics.label(), muted_style()))
                .unwrap_or_else(|| Span::raw("")),
        ]),
        Line::from(filters),
    ])
    .block(panel_block("AGENT HELM"))
}

fn session_list(view: &DashboardView, animation_frame: usize) -> (List<'static>, ListState) {
    let selected_row = selected_list_row(view);
    let selected_id = view.selected.as_ref().map(|session| session.id.as_str());
    let mut items = Vec::new();

    for (group_index, group) in view.groups.iter().enumerate() {
        items.push(ListItem::new(Line::from(Span::styled(
            group_header_text(group_index, group),
            group_style(),
        ))));

        if group.collapsed {
            continue;
        }

        for (session_index, session) in group.sessions.iter().enumerate() {
            let selected = selected_id == Some(session.id.as_str());
            let last_in_group = session_index + 1 == group.sessions.len();
            items.push(
                ListItem::new(session_row_line(session, last_in_group, animation_frame)).style(
                    if selected {
                        selected_row_style()
                    } else {
                        Style::default()
                    },
                ),
            );
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "No sessions. Press n to create one.",
            muted_style(),
        ))));
    }

    let list = List::new(items)
        .block(panel_block("SESSIONS"))
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_symbol("> ")
        .highlight_style(Style::default());
    let state = ListState::default().with_selected(selected_row);
    (list, state)
}

fn session_row_line(
    session: &SessionSummary,
    last_in_group: bool,
    animation_frame: usize,
) -> Line<'static> {
    let branch = if last_in_group { "`-" } else { "|-" };
    let mut row = vec![
        Span::styled(branch, muted_style()),
        Span::raw(" "),
        Span::styled(
            session_activity_marker(session, animation_frame),
            session_activity_style(session),
        ),
        Span::raw(" "),
        Span::raw(session.name.clone()),
        Span::raw(if session.pr_number.is_some() { " " } else { "" }),
        Span::styled(session_pr_label(session.pr_number), pr_style()),
        Span::raw(" "),
        Span::styled(session.agent.clone(), agent_style(&session.agent)),
    ];
    if let Some(label) = session_activity_label(session) {
        row.push(Span::raw(" "));
        row.push(Span::styled(
            label.to_string(),
            activity_label_style(session),
        ));
    }
    Line::from(row)
}

fn session_row_width(session: &SessionSummary, last_in_group: bool, animation_frame: usize) -> u16 {
    let width = session_row_line(session, last_in_group, animation_frame).width();
    u16::try_from(width).unwrap_or(u16::MAX)
}

fn selected_list_row(view: &DashboardView) -> Option<usize> {
    let selected_id = view.selected.as_ref()?.id.as_str();
    view.rows.iter().position(
        |row| matches!(row, DashboardRow::Session { session, .. } if session.id == selected_id),
    )
}

fn render_dashboard_detail(frame: &mut Frame<'_>, app: &App, view: &DashboardView, area: Rect) {
    if view
        .selected
        .as_ref()
        .is_some_and(|session| session_is_live(session.status))
    {
        render_terminal_preview(frame, app, view, area);
    } else {
        frame.render_widget(detail_panel(app, view), area);
    }
}

fn render_terminal_preview(frame: &mut Frame<'_>, app: &App, view: &DashboardView, area: Rect) {
    let title = view
        .selected
        .as_ref()
        .map(|session| format!("PREVIEW {}", session.name))
        .unwrap_or_else(|| "PREVIEW".to_string());
    let block = panel_block_owned(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let matching_preview = view.selected.as_ref().and_then(|session| {
        let target = tmux_target_for_session(session);
        app.preview.as_ref().filter(|preview| {
            preview.is_current(
                &session.id,
                &target,
                inner.height,
                inner.width,
                app.preview_scroll,
            )
        })
    });

    if let Some(preview) = matching_preview {
        if let Some(parser) = preview.parser.as_ref() {
            render_vt100_screen_tail(
                parser.screen(),
                frame.buffer_mut(),
                inner,
                app.preview_scroll,
            );
            return;
        }
        if let Some(error) = preview.error.as_ref() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("Preview unavailable: {error}"),
                    muted_style(),
                ))),
                inner,
            );
            return;
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Loading terminal preview...",
            muted_style(),
        ))),
        inner,
    );
}

fn detail_panel(app: &App, view: &DashboardView) -> Paragraph<'static> {
    dashboard_panel(app, view).scroll((app.detail_scroll, 0))
}

fn render_session_terminal(frame: &mut Frame<'_>, app: &App, view: &DashboardView, area: Rect) {
    let title = view
        .selected
        .as_ref()
        .map(|session| format!("SESSION {}", session.name))
        .unwrap_or_else(|| "SESSION".to_string());
    let block = if view
        .selected
        .as_ref()
        .is_some_and(|session| session_is_live(session.status))
    {
        panel_block_owned(title).border_style(active_border_style())
    } else {
        panel_block_owned(title)
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(embedded) = app.embedded.as_ref() {
        embedded.render(frame.buffer_mut(), inner);
        embedded.render_hidden_cursor(frame.buffer_mut(), inner);
        if let Some((x, y)) = embedded.cursor_position(inner) {
            frame.set_cursor_position((x, y));
        }
    } else {
        let message = Paragraph::new("Attaching tmux session...")
            .style(muted_style())
            .wrap(Wrap { trim: false });
        frame.render_widget(message, inner);
    }
}

fn help_panel() -> Paragraph<'static> {
    Paragraph::new(vec![
        section_line("Navigation"),
        Line::from("j/k or Up/Down select session | c collapse | e expand"),
        Line::from("Enter focuses embedded session viewport | Ctrl-q returns"),
        Line::from("Ctrl-t toggles mouse wheel scrolling and text selection"),
        Line::from(""),
        section_line("Actions"),
        Line::from("n new session | Ctrl-n shell | N duplicate | m move group"),
        Line::from("G create group | E edit selected group defaults"),
        Line::from("f fork | d delete | D delete and clean up"),
        Line::from("g opens profile tool settings"),
        Line::from(""),
        section_line("Filters"),
        Line::from("/ search | t status filter | PageUp/PageDown scroll"),
        Line::from(""),
        section_line("Help"),
        Line::from("? or h open help | Esc/q close"),
    ])
    .block(panel_block("Help"))
    .wrap(Wrap { trim: false })
}

fn dashboard_panel(app: &App, view: &DashboardView) -> Paragraph<'static> {
    Paragraph::new(dashboard_lines(app, view))
        .block(panel_block("Dashboard"))
        .wrap(Wrap { trim: false })
}

fn dashboard_lines(app: &App, view: &DashboardView) -> Vec<Line<'static>> {
    if let Some(session) = &view.selected {
        let details_loaded = app.details_session_id.as_deref() == Some(session.id.as_str());
        let workspace = details_loaded
            .then_some(app.details.workspace.as_ref())
            .flatten();
        let mut lines = session_summary_lines(session, workspace);
        if matches!(
            session.status,
            SessionStatus::Running | SessionStatus::Starting
        ) {
            lines.push(Line::from(""));
            lines.push(section_line("Output"));
            if details_loaded {
                append_content(&mut lines, &app.details.output, "No output loaded.");
            } else {
                append_content(&mut lines, "", "Loading details...");
            }
        }
        return lines;
    }

    let counts = view_status_counts(view);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Fleet", section_style()),
            Span::raw(format!(
                "  {} sessions across {} groups",
                view.visible_count,
                view.groups.len()
            )),
        ]),
        Line::from(format!(
            "x {} occupied   ~ {} thinking   * {} running   > {} starting   : {} queued   ? {} waiting   - {} idle   o {} stopped   ! {} errored",
            counts.occupied,
            counts.thinking,
            counts.running,
            counts.starting,
            counts.queued,
            counts.waiting,
            counts.idle,
            counts.stopped,
            counts.errored
        )),
        Line::from(""),
        section_line("Groups"),
    ];

    for group in &view.groups {
        let counts = group_status_counts(group);
        lines.push(Line::from(format!(
            "{}  {} sessions  x{} ~{} *{} >{} :{} ?{} -{} o{} !{}",
            group.name,
            group.sessions.len(),
            counts.occupied,
            counts.thinking,
            counts.running,
            counts.starting,
            counts.queued,
            counts.waiting,
            counts.idle,
            counts.stopped,
            counts.errored
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("No session selected."));

    lines
}

fn panel_block(title: &'static str) -> Block<'static> {
    Block::default()
        .style(base_style())
        .borders(Borders::ALL)
        .border_style(border_style())
        .title(Span::styled(title, title_style()))
}

fn panel_block_owned(title: String) -> Block<'static> {
    Block::default()
        .style(base_style())
        .borders(Borders::ALL)
        .border_style(border_style())
        .title(Span::styled(title, title_style()))
}

fn session_summary_lines(
    session: &SessionSummary,
    workspace: Option<&TuiWorkspaceContext>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(session.name.clone(), title_style()),
            Span::raw("  "),
            Span::styled(
                deck_status_label(session.deck_status),
                deck_status_style(session.deck_status),
            ),
            Span::raw(format!("   lifecycle {}", session.status.as_str())),
        ]),
        Line::from(vec![Span::styled(
            session.project_path.clone(),
            muted_style(),
        )]),
        Line::from(vec![
            Span::styled("agent ", muted_style()),
            Span::styled(session.agent.clone(), agent_style(&session.agent)),
            Span::raw("   "),
            Span::styled("command ", muted_style()),
            Span::raw(session.command.clone()),
        ]),
        Line::from(vec![
            Span::styled("session ", muted_style()),
            Span::raw(short_id(&session.id)),
            Span::raw("   "),
            Span::styled("state ", muted_style()),
            Span::raw(session.status.as_str()),
        ]),
        Line::from(vec![
            Span::styled("fork ", muted_style()),
            Span::raw("f options"),
            Span::raw("   "),
            Span::styled("send ", muted_style()),
            Span::raw("s input"),
        ]),
    ];
    if let Some(workspace) = workspace {
        lines.insert(
            2,
            Line::from(vec![
                Span::styled("workspace ", muted_style()),
                Span::raw(short_id(&workspace.workspace_id)),
                Span::raw(" "),
                Span::styled(workspace.workspace_path.clone(), muted_style()),
            ]),
        );
        if workspace.worktree_id.is_some()
            || workspace.worktree_branch.is_some()
            || workspace.worktree_path.is_some()
        {
            lines.insert(
                3,
                Line::from(vec![
                    Span::styled("worktree ", muted_style()),
                    Span::raw(
                        workspace
                            .worktree_id
                            .as_deref()
                            .map(short_id)
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                    Span::raw(" "),
                    Span::styled("branch ", muted_style()),
                    Span::raw(
                        workspace
                            .worktree_branch
                            .clone()
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        workspace
                            .worktree_path
                            .clone()
                            .unwrap_or_else(|| "-".into()),
                        muted_style(),
                    ),
                ]),
            );
        }
    }
    lines
}

fn append_content(lines: &mut Vec<Line<'static>>, content: &str, empty: &str) {
    let content = if content.trim().is_empty() {
        empty.to_string()
    } else {
        content.to_string()
    };

    lines.extend(content.lines().map(|line| Line::from(line.to_string())));
}

fn section_line(title: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled("---- ", border_style()),
        Span::styled(title, section_style()),
        Span::styled(" ", border_style()),
    ])
}

fn view_status_counts(view: &DashboardView) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for group in &view.groups {
        for session in &group.sessions {
            counts.add(session.deck_status);
        }
    }
    counts
}

fn group_status_counts(group: &SessionGroup) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for session in &group.sessions {
        counts.add(session.deck_status);
    }
    counts
}

fn session_activity_marker(session: &SessionSummary, animation_frame: usize) -> &'static str {
    let active = session_has_active_activity(session);
    match session.deck_status {
        SessionDeckStatus::Occupied if active => {
            const FRAMES: [&str; 2] = ["◆", "◇"];
            FRAMES[animation_frame % FRAMES.len()]
        }
        SessionDeckStatus::Thinking if active => {
            const FRAMES: [&str; 2] = ["✦", "✧"];
            FRAMES[animation_frame % FRAMES.len()]
        }
        SessionDeckStatus::Running if active => {
            const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
            FRAMES[animation_frame % FRAMES.len()]
        }
        SessionDeckStatus::Starting if active => {
            const FRAMES: [&str; 2] = ["▸", "▹"];
            FRAMES[animation_frame % FRAMES.len()]
        }
        SessionDeckStatus::Occupied => "◆",
        SessionDeckStatus::Thinking => "✦",
        SessionDeckStatus::Running => "●",
        SessionDeckStatus::Starting => "▸",
        SessionDeckStatus::Queued => "⋯",
        SessionDeckStatus::Waiting => "◌",
        SessionDeckStatus::Idle => "·",
        SessionDeckStatus::Stopped => "■",
        SessionDeckStatus::Errored => "×",
    }
}

fn session_activity_style(session: &SessionSummary) -> Style {
    match session.deck_status {
        SessionDeckStatus::Occupied
        | SessionDeckStatus::Thinking
        | SessionDeckStatus::Running
        | SessionDeckStatus::Starting => agent_style(&session.agent),
        status => deck_status_style(status),
    }
}

fn session_activity_label(session: &SessionSummary) -> Option<&str> {
    let activity = session.activity.as_ref()?;
    if activity.source == "runtime_start" {
        return None;
    }
    let label = activity.label.trim();
    if label.is_empty()
        || label.eq_ignore_ascii_case(session.deck_status.as_str())
        || activity_tool_matches_agent(activity, session)
        || (label.eq_ignore_ascii_case("working") && activity.tool.is_none())
    {
        None
    } else {
        Some(label)
    }
}

fn session_has_active_activity(session: &SessionSummary) -> bool {
    let Some(activity) = session.activity.as_ref() else {
        return false;
    };
    if activity.source == "runtime_start" {
        return false;
    }
    matches!(
        activity.state.trim().to_ascii_lowercase().as_str(),
        "occupied" | "running" | "busy" | "working" | "thinking"
    )
}

fn activity_tool_matches_agent(activity: &SessionActivity, session: &SessionSummary) -> bool {
    activity
        .tool
        .as_deref()
        .is_some_and(|tool| tool.eq_ignore_ascii_case(&session.agent))
        || activity
            .label
            .trim()
            .strip_prefix("using ")
            .is_some_and(|tool| tool.eq_ignore_ascii_case(&session.agent))
}

fn activity_label_style(session: &SessionSummary) -> Style {
    session_activity_style(session)
}

fn deck_status_label(status: SessionDeckStatus) -> &'static str {
    match status {
        SessionDeckStatus::Occupied => "x occupied",
        SessionDeckStatus::Thinking => "~ thinking",
        SessionDeckStatus::Running => "* running",
        SessionDeckStatus::Starting => "> starting",
        SessionDeckStatus::Queued => ": queued",
        SessionDeckStatus::Waiting => "? waiting",
        SessionDeckStatus::Idle => "- idle",
        SessionDeckStatus::Stopped => "o stopped",
        SessionDeckStatus::Errored => "! errored",
    }
}

fn worktree_label(worktree: ToolWorktreeBehavior) -> &'static str {
    match worktree {
        ToolWorktreeBehavior::Always => "always",
        ToolWorktreeBehavior::Manual => "manual",
        ToolWorktreeBehavior::Never => "never",
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

fn title_style() -> Style {
    Style::default()
        .fg(Color::Rgb(255, 212, 96))
        .add_modifier(Modifier::BOLD)
}

fn base_style() -> Style {
    Style::default()
        .fg(Color::Rgb(235, 237, 245))
        .bg(Color::Reset)
}

fn section_style() -> Style {
    Style::default()
        .fg(Color::Rgb(78, 205, 196))
        .add_modifier(Modifier::BOLD)
}

fn group_style() -> Style {
    Style::default()
        .fg(Color::Rgb(78, 205, 196))
        .add_modifier(Modifier::BOLD)
}

fn badge_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Rgb(255, 212, 96))
        .add_modifier(Modifier::BOLD)
}

fn selected_row_style() -> Style {
    badge_style()
}

fn border_style() -> Style {
    Style::default().fg(Color::Rgb(118, 126, 148))
}

fn active_border_style() -> Style {
    Style::default().fg(Color::Rgb(255, 212, 96))
}

fn muted_style() -> Style {
    Style::default().fg(Color::Rgb(156, 163, 175))
}

fn accent_style() -> Style {
    Style::default().fg(Color::Rgb(80, 250, 123))
}

fn agent_style(agent: &str) -> Style {
    match agent {
        "claude" => Style::default().fg(Color::Rgb(255, 107, 129)),
        "codex" => Style::default().fg(Color::Rgb(64, 196, 255)),
        "gemini" => Style::default().fg(Color::Rgb(189, 147, 249)),
        "opencode" => Style::default().fg(Color::Rgb(255, 184, 108)),
        _ => accent_style(),
    }
}

fn pr_style() -> Style {
    Style::default()
        .fg(Color::Rgb(255, 184, 108))
        .add_modifier(Modifier::BOLD)
}

fn session_pr_label(pr_number: Option<u64>) -> String {
    pr_number
        .map(|number| format!("#{number}"))
        .unwrap_or_default()
}

fn agent_selector_spans(form: &NewForm, agent_choices: &[String]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, agent) in effective_agent_choices(agent_choices, &form.agent)
        .iter()
        .enumerate()
    {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let selected = agent == &form.agent;
        let label = if selected {
            format!("[{}]", agent)
        } else {
            agent.to_string()
        };
        let style = if selected {
            agent_style(agent).add_modifier(Modifier::BOLD)
        } else {
            muted_style()
        };
        spans.push(Span::styled(label, style));
    }
    spans
}

fn group_selector_spans(
    form: &NewForm,
    group_default_paths: &BTreeMap<String, String>,
) -> Vec<Span<'static>> {
    let groups = group_names(group_default_paths);
    if groups.is_empty() {
        return vec![Span::styled("no groups", muted_style())];
    }

    let mut spans = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let selected = group == &form.group_name;
        let label = if selected {
            format!("[{group}]")
        } else {
            group.to_string()
        };
        let style = if selected {
            title_style().add_modifier(Modifier::BOLD)
        } else {
            muted_style()
        };
        spans.push(Span::styled(label, style));
    }
    spans
}

fn text_entry_style(active: bool, placeholder: bool) -> Style {
    let style = if placeholder {
        muted_style()
    } else {
        Style::default()
    };
    if active {
        style.bg(Color::Rgb(31, 36, 49))
    } else {
        style
    }
}

fn text_entry_spans(form: &NewForm, field: NewField, active: bool) -> Vec<Span<'static>> {
    let value = form.field_value(field);
    let placeholder = field == NewField::Name && value.trim().is_empty();
    let display = if placeholder {
        form.default_name()
    } else {
        value.to_string()
    };
    let border_style = if active {
        active_border_style().add_modifier(Modifier::BOLD)
    } else {
        border_style()
    };
    let value_style = text_entry_style(active, placeholder);
    vec![
        Span::styled("[", border_style),
        Span::styled(" ", value_style),
        Span::styled(display, value_style),
        Span::styled(" ", value_style),
        Span::styled("]", border_style),
    ]
}

fn new_form_cursor_position(form: &NewForm, area: Rect) -> Option<(u16, u16)> {
    if form.launching {
        return None;
    }

    let field = form.current_field();
    if !field.is_text_entry() || area.width < 4 || area.height < 3 {
        return None;
    }

    let row = area.y.saturating_add(1 + form.field_index as u16);
    if row >= area.y.saturating_add(area.height).saturating_sub(1) {
        return None;
    }

    let value_width = form
        .field_value(field)
        .chars()
        .count()
        .min(u16::MAX as usize) as u16;
    let x = area.x.saturating_add(24).saturating_add(value_width);
    let max_x = area.x.saturating_add(area.width).saturating_sub(2);
    Some((x.min(max_x), row))
}

fn create_form(
    form: &NewForm,
    agent_choices: &[String],
    group_default_paths: &BTreeMap<String, String>,
    status_message: &Option<String>,
) -> Paragraph<'static> {
    let mut lines = Vec::new();

    for field in NEW_FIELDS {
        let active = field == form.current_field();
        let marker = if active { ">" } else { " " };
        let style = if active {
            title_style()
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(format!("{:<18}", field.label()), style),
            Span::raw(" "),
        ];
        match field {
            field if field.is_text_entry() => spans.extend(text_entry_spans(form, field, active)),
            NewField::Agent => spans.extend(agent_selector_spans(form, agent_choices)),
            NewField::Group => spans.extend(group_selector_spans(form, group_default_paths)),
            NewField::Worktree => {
                let checkbox = if form.worktree { "[x]" } else { "[ ]" };
                spans.push(Span::raw(checkbox));
            }
            NewField::CarryState => {
                let checkbox = if form.carry_state { "[x]" } else { "[ ]" };
                spans.push(Span::raw(checkbox));
            }
            _ => spans.push(Span::raw(form.field_value(field).to_string())),
        }
        lines.push(Line::from(spans));
    }

    if form.launching {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Launching session...",
            accent_style().add_modifier(Modifier::BOLD),
        )));
    }

    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    Paragraph::new(lines)
        .block(panel_block("NEW SESSION"))
        .wrap(Wrap { trim: false })
}

fn create_group_form(form: &GroupForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = Vec::new();
    for field in GROUP_FIELDS {
        let active = field == form.current_field();
        let marker = if active { ">" } else { " " };
        let style = if active {
            title_style()
        } else {
            Style::default()
        };
        let border_style = if active {
            active_border_style().add_modifier(Modifier::BOLD)
        } else {
            border_style()
        };
        let value_style = text_entry_style(active, false);
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(format!("{:<18}", field.label()), style),
            Span::raw(" "),
            Span::styled("[", border_style),
            Span::styled(" ", value_style),
            Span::styled(form.field_value(field).to_string(), value_style),
            Span::styled(" ", value_style),
            Span::styled("]", border_style),
        ]));
    }

    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    Paragraph::new(lines)
        .block(panel_block("CREATE GROUP"))
        .wrap(Wrap { trim: false })
}

fn tool_settings_form(
    form: &ToolSettingsForm,
    status_message: &Option<String>,
) -> Paragraph<'static> {
    let mut lines = Vec::new();
    for field in TOOL_SETTINGS_FIELDS {
        let active = field == form.current_field();
        let marker = if active { ">" } else { " " };
        let style = if active {
            title_style()
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(format!("{:<10}", field.label()), style),
            Span::raw(" "),
        ];
        match field {
            ToolSettingsField::Tool => {
                for (index, tool) in form.tools.iter().enumerate() {
                    if index > 0 {
                        spans.push(Span::raw(" "));
                    }
                    let selected = index == form.selected_tool;
                    let label = if selected {
                        format!("[{}]", tool.name)
                    } else {
                        tool.name.clone()
                    };
                    let style = if selected {
                        agent_style(&tool.name).add_modifier(Modifier::BOLD)
                    } else {
                        muted_style()
                    };
                    spans.push(Span::styled(label, style));
                }
            }
            ToolSettingsField::Installed => {
                let checkbox = if form.current_tool().installed {
                    "[x]"
                } else {
                    "[ ]"
                };
                spans.push(Span::raw(checkbox));
            }
            ToolSettingsField::Executable => {
                let value = if form.executable.trim().is_empty() {
                    "-"
                } else {
                    form.executable.as_str()
                };
                spans.push(Span::raw(value.to_string()));
            }
            ToolSettingsField::Flags => {
                let value = if form.flags.trim().is_empty() {
                    "-"
                } else {
                    form.flags.as_str()
                };
                spans.push(Span::raw(value.to_string()));
            }
            ToolSettingsField::Worktree => {
                spans.push(Span::raw(worktree_label(form.current_tool().worktree)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "writes ~/.config/agent-helm/settings.toml",
        muted_style(),
    )));
    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            status_style(SessionStatus::Errored),
        )));
    }
    Paragraph::new(lines)
        .block(panel_block("PROFILE TOOL SETTINGS"))
        .wrap(Wrap { trim: false })
}

fn group_settings_form(
    form: &GroupSettingsForm,
    agent_choices: &[String],
    status_message: &Option<String>,
) -> Paragraph<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled("Group ", muted_style()),
        Span::styled(form.name.clone(), title_style()),
    ])];
    for field in GROUP_SETTINGS_FIELDS {
        let active = field == form.current_field();
        let marker = if active { ">" } else { " " };
        let style = if active {
            title_style()
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(format!("{:<18}", field.label()), style),
            Span::raw(" "),
        ];
        match field {
            GroupSettingsField::DefaultPath => {
                let value_style = text_entry_style(active, false);
                spans.extend([
                    Span::styled("[", border_style()),
                    Span::styled(" ", value_style),
                    Span::styled(form.default_project_path.clone(), value_style),
                    Span::styled(" ", value_style),
                    Span::styled("]", border_style()),
                ]);
            }
            GroupSettingsField::DefaultAgent => {
                spans.extend(group_settings_agent_spans(form, agent_choices));
            }
            GroupSettingsField::DefaultWorktree => {
                spans.push(Span::raw(optional_bool_label(form.default_worktree)));
            }
            GroupSettingsField::DefaultCarryState => {
                spans.push(Span::raw(optional_bool_label(form.default_carry_state)));
            }
        }
        lines.push(Line::from(spans));
    }
    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    Paragraph::new(lines)
        .block(panel_block("GROUP SETTINGS"))
        .wrap(Wrap { trim: false })
}

fn group_settings_agent_spans(
    form: &GroupSettingsForm,
    agent_choices: &[String],
) -> Vec<Span<'static>> {
    let mut choices = vec![String::new()];
    choices.extend(effective_agent_choices(agent_choices, &form.default_agent));
    choices
        .into_iter()
        .enumerate()
        .flat_map(|(index, agent)| {
            let selected = agent == form.default_agent;
            let label = if agent.is_empty() {
                "inherit".to_string()
            } else {
                agent
            };
            let label = if selected {
                format!("[{label}]")
            } else {
                label
            };
            let style = if selected {
                title_style()
            } else {
                muted_style()
            };
            let mut spans = Vec::new();
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(label, style));
            spans
        })
        .collect()
}

fn optional_bool_label(value: Option<bool>) -> &'static str {
    match value {
        None => "inherit",
        Some(true) => "[x]",
        Some(false) => "[ ]",
    }
}

fn fork_form(form: &ForkForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled("Parent", muted_style()),
        Span::raw(" "),
        Span::raw(short_id(&form.parent_session_id)),
    ])];

    for field in FORK_FIELDS {
        let active = field == form.current_field();
        let marker = if active { ">" } else { " " };
        let style = if active {
            title_style()
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(format!("{:<18}", field.label()), style),
            Span::raw(" "),
        ];
        if field == ForkField::CarryState {
            let checkbox = if form.carry_state { "[x]" } else { "[ ]" };
            spans.push(Span::raw(checkbox));
        } else {
            spans.push(Span::raw(form.field_value(field).to_string()));
        }
        lines.push(Line::from(spans));
    }

    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    Paragraph::new(lines)
        .block(panel_block("FORK SESSION"))
        .wrap(Wrap { trim: false })
}

fn move_form(form: &MoveForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled(">", title_style()),
        Span::raw(" Group "),
        Span::raw(form.group_name.clone()),
    ])];
    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    Paragraph::new(lines)
        .block(panel_block("MOVE SESSION"))
        .wrap(Wrap { trim: false })
}

fn rename_form(form: &RenameForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled(">", title_style()),
        Span::raw(" Name "),
        Span::raw(form.name.clone()),
    ])];
    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    Paragraph::new(lines)
        .block(panel_block("RENAME SESSION"))
        .wrap(Wrap { trim: false })
}

fn footer(app: &App) -> Paragraph<'static> {
    let text = footer_text(app);
    Paragraph::new(Line::from(vec![Span::styled(text, muted_style())]))
}

fn footer_text(app: &App) -> String {
    match &app.mode {
        Mode::Search => "Search: type query | Enter/Esc done | Ctrl-t mouse/select".to_string(),
        Mode::New(_) => {
            "New: Tab field | Enter next/create | Ctrl-S create | Esc cancel".to_string()
        }
        Mode::CreateGroup(_) => {
            "Create group: Tab field | Enter next/create | Ctrl-S create | Esc cancel".to_string()
        }
        Mode::GroupSettings(_) => {
            "Group settings: Tab field | Left/Right cycle | Ctrl-S save | Esc cancel".to_string()
        }
        Mode::Fork(_) => "Fork: Tab field | Enter next/fork | Ctrl-S fork | Esc cancel".to_string(),
        Mode::Session(_) => {
            if app.mouse_capture {
                "Session: wheel scroll mode | Ctrl-q dashboard | Ctrl-t text selection".to_string()
            } else {
                "Session: text selection mode | Ctrl-q dashboard | Ctrl-t wheel scroll".to_string()
            }
        }
        Mode::Move(_) => "Move: type group | Enter move | Esc cancel".to_string(),
        Mode::Rename(_) => "Rename: type name | Enter rename | Esc cancel".to_string(),
        Mode::ToolSettings(_) => {
            "Profile tools: Tab field | Left/Right cycle | Space toggle | Ctrl-S save | Esc cancel".to_string()
        }
        Mode::Help => "Help: Esc/q close".to_string(),
        Mode::Normal => {
            "Enter session r rename m move f fork d delete D cleanup | n new Ctrl-n shell N duplicate | G new group E edit group | g tools | Ctrl-t mouse/select | j/k nav c/e groups Pg scroll / search t status ? help q quit".to_string()
        }
    }
}

fn status_style(status: SessionStatus) -> Style {
    match status {
        SessionStatus::Running => accent_style(),
        SessionStatus::Starting => Style::default().fg(Color::Rgb(255, 184, 108)),
        SessionStatus::Stopped => muted_style(),
        SessionStatus::Errored => Style::default().fg(Color::Rgb(255, 85, 85)),
    }
}

fn deck_status_style(status: SessionDeckStatus) -> Style {
    match status {
        SessionDeckStatus::Occupied => accent_style(),
        SessionDeckStatus::Thinking => Style::default().fg(Color::Rgb(189, 147, 249)),
        SessionDeckStatus::Running => accent_style(),
        SessionDeckStatus::Starting | SessionDeckStatus::Queued => {
            Style::default().fg(Color::Rgb(255, 184, 108))
        }
        SessionDeckStatus::Waiting => Style::default().fg(Color::Rgb(64, 196, 255)),
        SessionDeckStatus::Idle | SessionDeckStatus::Stopped => muted_style(),
        SessionDeckStatus::Errored => Style::default().fg(Color::Rgb(255, 85, 85)),
    }
}

fn lifecycle_deck_status(status: SessionStatus) -> SessionDeckStatus {
    match status {
        SessionStatus::Starting => SessionDeckStatus::Starting,
        SessionStatus::Running => SessionDeckStatus::Running,
        SessionStatus::Stopped => SessionDeckStatus::Stopped,
        SessionStatus::Errored => SessionDeckStatus::Errored,
    }
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_bool_field(value: &str, field: &'static str) -> std::result::Result<bool, &'static str> {
    match value.to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => Err(match field {
            "carry" => "carry must be true or false",
            "sandbox" => "sandbox must be true or false",
            _ => "boolean field must be true or false",
        }),
    }
}

fn session_pr_number(session: &SessionRecord) -> Option<u64> {
    [
        session.name.as_str(),
        session.group_name.as_str(),
        session.project_path.as_str(),
    ]
    .into_iter()
    .find_map(extract_pr_number)
}

fn extract_pr_number(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    let lower = value.to_ascii_lowercase();
    let lower = lower.as_bytes();

    for index in 0..bytes.len() {
        if bytes[index] == b'#'
            && boundary_before(bytes, index)
            && let Some(number) = parse_digits(bytes, index + 1)
        {
            return Some(number);
        }
        if matches_pr_word(lower, index, b"pr")
            && let Some(number) = parse_digits_after_separator(bytes, index + 2)
        {
            return Some(number);
        }
        if matches_pr_word(lower, index, b"pull")
            && let Some(number) = parse_digits_after_separator(bytes, index + 4)
        {
            return Some(number);
        }
    }
    None
}

fn matches_pr_word(bytes: &[u8], index: usize, word: &[u8]) -> bool {
    bytes[index..].starts_with(word)
        && boundary_before(bytes, index)
        && bytes
            .get(index + word.len())
            .is_some_and(|byte| matches!(byte, b'-' | b'/' | b'_' | b':' | b'#'))
}

fn boundary_before(bytes: &[u8], index: usize) -> bool {
    index == 0 || !bytes[index - 1].is_ascii_alphanumeric()
}

fn parse_digits_after_separator(bytes: &[u8], index: usize) -> Option<u64> {
    if bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b'-' | b'/' | b'_' | b':' | b'#'))
    {
        parse_digits(bytes, index + 1)
    } else {
        None
    }
}

fn parse_digits(bytes: &[u8], mut index: usize) -> Option<u64> {
    let start = index;
    let mut value = 0_u64;
    while let Some(byte) = bytes.get(index) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(u64::from(byte - b'0'));
        index += 1;
    }
    (index > start && value > 0).then_some(value)
}

fn compare_sessions(left: &SessionSummary, right: &SessionSummary) -> Ordering {
    left.group_name
        .cmp(&right.group_name)
        .then_with(|| right.updated_at.cmp(&left.updated_at))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.id.cmp(&right.id))
}

fn matches_query(session: &SessionRecord, query: &str) -> bool {
    [
        session.id.as_str(),
        session.name.as_str(),
        session.group_name.as_str(),
        session.agent.as_str(),
        session.command.as_str(),
        session.project_path.as_str(),
        session.status.as_str(),
    ]
    .iter()
    .any(|field| field.to_ascii_lowercase().contains(query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn base_style_uses_terminal_default_background() {
        assert_eq!(base_style().bg, Some(Color::Reset));
    }

    #[test]
    fn tui_loop_schedules_refresh_and_draws_on_changes() {
        let start = Instant::now();
        let mut tui_loop = TuiLoop::new(start);

        assert!(tui_loop.needs_draw);
        assert_eq!(tui_loop.poll_timeout(start, None), EVENT_POLL_INTERVAL);
        assert!(
            !tui_loop.should_refresh_sessions(
                start + SESSION_REFRESH_INTERVAL - Duration::from_millis(1)
            )
        );
        assert!(tui_loop.should_refresh_sessions(start + SESSION_REFRESH_INTERVAL));

        tui_loop.mark_drawn();
        assert!(!tui_loop.needs_draw);

        tui_loop.mark_changed();
        assert!(tui_loop.needs_draw);

        tui_loop.mark_drawn();
        tui_loop.mark_session_refreshed(start + SESSION_REFRESH_INTERVAL);
        assert!(tui_loop.needs_draw);
        let next_tick = start + SESSION_REFRESH_INTERVAL + Duration::from_millis(990);
        assert_eq!(tui_loop.poll_timeout(next_tick, None), Duration::ZERO);
        assert!(tui_loop.should_animate(next_tick));
        tui_loop.mark_animated(next_tick);
        assert_eq!(
            tui_loop.poll_timeout(next_tick, None),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn groups_sessions() {
        let sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "core", "build", false),
            record("3", "ops", "logs", false),
        ];
        let view = DashboardView::build(&sessions, "", 0, StatusFilter::All);

        assert_eq!(view.visible_count, 3);
        assert_eq!(
            view.groups
                .iter()
                .map(|group| (group.name.as_str(), group.sessions.len()))
                .collect::<Vec<_>>(),
            vec![("core", 1), ("ops", 2)]
        );
    }

    #[test]
    fn dashboard_view_precomputes_visible_rows_and_sessions() {
        let sessions = vec![
            record("1", "ops", "hidden", false),
            record("2", "core", "visible", false),
            record("3", "ops/child", "descendant", false),
        ];
        let collapsed_groups = BTreeSet::from(["ops".to_string()]);

        let view = DashboardView::build_with_collapsed(
            &sessions,
            &collapsed_groups,
            "",
            0,
            StatusFilter::All,
        );

        assert_eq!(
            view.visible_sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["2"]
        );
        assert_eq!(view.visible_count, 1);
        assert_eq!(
            view.rows
                .iter()
                .filter(|row| matches!(row, DashboardRow::Session { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn session_list_state_selects_visible_session_row() {
        let view = DashboardView::build(
            &[
                record("1", "ops", "deploy", false),
                record("2", "ops", "logs", false),
            ],
            "",
            1,
            StatusFilter::All,
        );

        let (_list, state) = session_list(&view, 0);

        assert_eq!(state.selected(), Some(2));
    }

    #[test]
    fn search_matches_command_case_insensitively() {
        let mut sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "core", "build", false),
        ];
        sessions[1].command = "Cargo Test".to_string();

        let view = DashboardView::build(&sessions, "test", 0, StatusFilter::All);

        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "2");
    }

    #[test]
    fn search_enter_uses_shared_search_results() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Search;
        app.query = "output-only".to_string();
        let shared_hit = record("2", "core", "build", false);
        let mut searched = false;
        let mut handle = |action| match action {
            TuiAction::Search { query, limit } => {
                searched = true;
                assert_eq!(query, "output-only");
                assert_eq!(limit, 200);
                Ok(vec![shared_hit.clone()])
            }
            _ => Ok(Vec::new()),
        };

        handle_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let view = app.view();
        assert!(searched);
        assert!(app.search_results_active);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "2");
    }

    #[test]
    fn refresh_sessions_reruns_shared_search() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.query = "output-only".to_string();
        app.search_results_active = true;
        let refreshed_hit = record("2", "core", "build", false);
        let mut searched = false;
        let mut handle = |action| match action {
            TuiAction::Search { query, limit } => {
                searched = true;
                assert_eq!(query, "output-only");
                assert_eq!(limit, 200);
                Ok(vec![refreshed_hit.clone()])
            }
            TuiAction::Refresh => panic!("shared search refresh should not list all sessions"),
            _ => Ok(Vec::new()),
        };

        refresh_sessions(&mut app, &mut handle).unwrap();

        let view = app.view();
        assert!(searched);
        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "2");
    }

    #[test]
    fn selected_action_preserves_shared_search_results() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.query = "output-only".to_string();
        app.search_results_active = true;
        let all_sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "core", "other", false),
        ];
        let search_hit = vec![record("1", "ops", "deploy", false)];
        let mut stopped = false;
        let mut searched = false;
        let mut handle = |action| match action {
            TuiAction::Stop(id) => {
                stopped = id == "1";
                Ok(all_sessions.clone())
            }
            TuiAction::Search { query, limit } => {
                searched = true;
                assert_eq!(query, "output-only");
                assert_eq!(limit, 200);
                Ok(search_hit.clone())
            }
            _ => Ok(Vec::new()),
        };

        run_selected_action(&mut app, &mut handle, TuiAction::Stop, "stopped").unwrap();

        let view = app.view();
        assert!(stopped);
        assert!(searched);
        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "1");
        assert_eq!(app.status_message.as_deref(), Some("stopped"));
    }

    #[test]
    fn delete_selects_next_when_selected_is_first_in_group() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "logs", false),
        ]);
        app.selected_index = 0;
        let mut deleted = false;
        let remaining = vec![record("2", "ops", "logs", false)];
        let mut handle = |action| match action {
            TuiAction::Remove { session_id, mode } => {
                deleted = session_id == "1" && mode == DeleteMode::MetadataOnly;
                Ok(remaining.clone())
            }
            _ => Ok(Vec::new()),
        };

        run_selected_action(
            &mut app,
            &mut handle,
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::MetadataOnly,
            },
            "deleted",
        )
        .unwrap();

        let view = app.view();
        assert!(deleted);
        assert_eq!(view.selected.unwrap().id, "2");
        assert_eq!(app.status_message.as_deref(), Some("deleted"));
    }

    #[test]
    fn delete_selects_previous_when_selected_is_not_first_in_group() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "logs", false),
            record("3", "ops", "review", false),
        ]);
        app.selected_index = 1;
        let remaining = vec![
            record("1", "ops", "deploy", false),
            record("3", "ops", "review", false),
        ];
        let mut handle = |action| match action {
            TuiAction::Remove { session_id, mode } => {
                assert_eq!(session_id, "2");
                assert_eq!(mode, DeleteMode::CleanupWorktree);
                Ok(remaining.clone())
            }
            _ => Ok(Vec::new()),
        };

        run_selected_action(
            &mut app,
            &mut handle,
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::CleanupWorktree,
            },
            "deleted and cleaned up",
        )
        .unwrap();

        let view = app.view();
        assert_eq!(view.selected.unwrap().id, "1");
        assert_eq!(
            app.status_message.as_deref(),
            Some("deleted and cleaned up")
        );
    }

    #[test]
    fn archived_sessions_remain_visible() {
        let sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "old", true),
        ];

        let view = DashboardView::build(&sessions, "", 0, StatusFilter::All);

        assert_eq!(view.visible_count, 2);
    }

    #[test]
    fn status_filter_limits_visible_sessions() {
        let running = record("1", "ops", "deploy", false);
        let mut stopped = record("2", "ops", "old", false);
        stopped.status = SessionStatus::Stopped;

        let view = DashboardView::build(&[running, stopped], "", 0, StatusFilter::Stopped);

        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "2");
    }

    #[test]
    fn status_filter_uses_deck_status_over_lifecycle_status() {
        let running = record("1", "ops", "deploy", false);
        let statuses = BTreeMap::from([("1".to_string(), SessionDeckStatus::Waiting.into())]);
        let collapsed_groups = BTreeSet::new();

        let view = DashboardView::build_with_statuses(
            &[running],
            &statuses,
            &collapsed_groups,
            "",
            0,
            StatusFilter::Waiting,
        );

        assert_eq!(view.visible_count, 1);
        assert_eq!(
            view.selected.unwrap().deck_status,
            SessionDeckStatus::Waiting
        );
    }

    #[test]
    fn status_refresh_polls_sessions_hidden_by_status_filter() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.deck_statuses.clear();
        app.status_filter = StatusFilter::Waiting;

        let mut requested = Vec::new();
        let mut load = |ids: &[String]| {
            requested = ids.to_vec();
            Ok(ids
                .iter()
                .map(|id| (id.clone(), SessionDeckStatus::Waiting.into()))
                .collect())
        };

        refresh_deck_statuses(&mut app, &mut load).unwrap();

        assert_eq!(requested, vec!["1".to_string()]);
        assert_eq!(
            app.view().selected.unwrap().deck_status,
            SessionDeckStatus::Waiting
        );
    }

    #[test]
    fn group_counts_use_deck_status_over_lifecycle_status() {
        let running = record("1", "ops", "deploy", false);
        let thinking = record("2", "ops", "plan", false);
        let occupied = record("3", "ops", "build", false);
        let statuses = BTreeMap::from([
            ("1".to_string(), SessionDeckStatus::Waiting.into()),
            ("2".to_string(), SessionDeckStatus::Thinking.into()),
            ("3".to_string(), SessionDeckStatus::Occupied.into()),
        ]);
        let collapsed_groups = BTreeSet::new();
        let view = DashboardView::build_with_statuses(
            &[running, thinking, occupied],
            &statuses,
            &collapsed_groups,
            "",
            0,
            StatusFilter::All,
        );

        let counts = group_status_counts(&view.groups[0]);
        assert_eq!(counts.running, 0);
        assert_eq!(counts.occupied, 1);
        assert_eq!(counts.thinking, 1);
        assert_eq!(counts.waiting, 1);
    }

    #[test]
    fn collapsed_groups_hide_child_sessions() {
        let sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "core", "build", false),
        ];
        let collapsed_groups = BTreeSet::from(["ops".to_string()]);

        let view = DashboardView::build_with_collapsed(
            &sessions,
            &collapsed_groups,
            "",
            1,
            StatusFilter::All,
        );

        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "2");
        assert!(
            view.groups
                .iter()
                .any(|group| group.name == "ops" && group.collapsed)
        );
    }

    #[test]
    fn collapsed_parent_group_hides_descendant_group_sessions() {
        let sessions = vec![
            record("1", "work", "parent", false),
            record("2", "work/api", "child", false),
            record("3", "core", "build", false),
        ];
        let collapsed_groups = BTreeSet::from(["work".to_string()]);

        let view = DashboardView::build_with_collapsed(
            &sessions,
            &collapsed_groups,
            "",
            1,
            StatusFilter::All,
        );

        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "3");
        assert!(
            view.groups
                .iter()
                .any(|group| group.name == "work" && group.collapsed)
        );
        assert!(!view.groups.iter().any(|group| group.name == "work/api"));
    }

    #[test]
    fn refresh_details_loads_selected_session() {
        let mut app = App {
            sessions: vec![record("1", "ops", "deploy", false)],
            collapsed_groups: BTreeSet::new(),
            group_default_paths: BTreeMap::new(),
            group_defaults: BTreeMap::new(),
            deck_statuses: BTreeMap::new(),
            pending_operations: BTreeMap::new(),
            next_pending_operation_id: 1,
            details: TuiDetails::default(),
            details_session_id: None,
            query: String::new(),
            search_results_active: false,
            selected_index: 0,
            detail_scroll: 0,
            preview_scroll: 0,
            status_filter: StatusFilter::All,
            sidebar_width: None,
            resizing_sidebar: false,
            mouse_capture: true,
            animation_frame: 0,
            last_agent: "shell".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
            preview: None,
            preview_generation: 0,
            pending_preview: None,
            pending_details_refresh: None,
            embedded: None,
            mode: Mode::Normal,
            status_message: None,
        };

        refresh_details(
            &mut app,
            &mut |session_id| {
                Ok(TuiDetails {
                    deck_status: SessionDeckStatus::Waiting,
                    activity: None,
                    output: session_id.to_string(),
                    workspace: None,
                })
            },
            false,
        )
        .unwrap();

        assert_eq!(app.details.output, "1");
        assert_eq!(app.details.deck_status, SessionDeckStatus::Waiting);
        assert_eq!(app.details_session_id.as_deref(), Some("1"));

        refresh_details(
            &mut app,
            &mut |session_id| {
                Ok(TuiDetails {
                    deck_status: SessionDeckStatus::Idle,
                    activity: None,
                    output: format!("{session_id}-fresh"),
                    workspace: None,
                })
            },
            false,
        )
        .unwrap();

        assert_eq!(app.details.output, "1");

        refresh_details(
            &mut app,
            &mut |session_id| {
                Ok(TuiDetails {
                    deck_status: SessionDeckStatus::Idle,
                    activity: None,
                    output: format!("{session_id}-fresh"),
                    workspace: None,
                })
            },
            true,
        )
        .unwrap();

        assert_eq!(app.details.output, "1-fresh");
    }

    #[test]
    fn refresh_details_uses_cache_when_selection_is_unchanged() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.details = TuiDetails {
            output: "cached".to_string(),
            ..TuiDetails::default()
        };
        app.details_session_id = Some("1".to_string());
        let mut calls = 0;

        refresh_details(
            &mut app,
            &mut |_session_id| {
                calls += 1;
                Ok(TuiDetails {
                    output: "fresh".to_string(),
                    ..TuiDetails::default()
                })
            },
            false,
        )
        .unwrap();

        assert_eq!(calls, 0);
        assert_eq!(app.details.output, "cached");
    }

    #[test]
    fn selection_change_schedules_details_without_loading_immediately() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "review", false),
        ]);
        let mut handle = |_action| Ok(Vec::new());
        let previous_selected_id = selected_id(&app);

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        let now = Instant::now();
        let selection_changed = selected_id(&app) != previous_selected_id;
        schedule_details_refresh(&mut app, now, selection_changed);

        assert_eq!(app.selected_index, 1);
        assert_eq!(app.details_session_id.as_deref(), Some("1"));
        assert!(app.pending_details_refresh.is_some());

        let mut calls = 0;
        let before_due = now + DETAILS_REFRESH_DEBOUNCE - Duration::from_millis(1);
        assert!(
            !refresh_details_if_due(
                &mut app,
                &mut |_session_id| {
                    calls += 1;
                    Ok(TuiDetails::default())
                },
                before_due,
            )
            .unwrap()
        );
        assert_eq!(calls, 0);

        assert!(
            refresh_details_if_due(
                &mut app,
                &mut |session_id| {
                    calls += 1;
                    Ok(TuiDetails {
                        output: session_id.to_string(),
                        ..TuiDetails::default()
                    })
                },
                now + DETAILS_REFRESH_DEBOUNCE,
            )
            .unwrap()
        );
        assert_eq!(calls, 1);
        assert_eq!(app.details_session_id.as_deref(), Some("2"));
        assert_eq!(app.details.output, "2");
    }

    #[test]
    fn selection_changes_do_not_wait_for_pending_preview() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "review", false),
        ]);
        let target = tmux_session_name_for_id("1");
        let request = next_preview_request(&mut app, "1".to_string(), target, 10, 20, 0);
        app.pending_preview = Some(request);
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(app.selected_index, 1);
        assert_eq!(app.pending_preview.as_ref().unwrap().session_id, "1");
    }

    #[test]
    fn stale_preview_results_are_ignored() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "review", false),
        ]);
        let first = next_preview_request(
            &mut app,
            "1".to_string(),
            tmux_session_name_for_id("1"),
            10,
            20,
            0,
        );
        app.pending_preview = Some(first.clone());
        let second = next_preview_request(
            &mut app,
            "2".to_string(),
            tmux_session_name_for_id("2"),
            10,
            20,
            0,
        );
        app.pending_preview = Some(second.clone());

        let stale = preview_result(first, b"stale");
        assert!(!apply_preview_result(&mut app, stale));
        assert!(app.preview.is_none());

        let fresh = preview_result(second, b"fresh");
        assert_eq!(fresh.capture_duration, Duration::from_millis(1));
        assert!(apply_preview_result(&mut app, fresh));
        assert_eq!(app.preview.as_ref().unwrap().session_id, "2");
    }

    #[test]
    fn preview_scroll_change_requests_scrolled_capture() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let target = tmux_session_name_for_id("1");
        let first = next_preview_request(&mut app, "1".to_string(), target.clone(), 10, 20, 0);

        assert!(scroll_session_preview(
            &mut app,
            PREVIEW_SCROLL_LINES as i16
        ));
        let preview_scroll = app.preview_scroll;
        let second =
            next_preview_request(&mut app, "1".to_string(), target, 10, 20, preview_scroll);

        assert_ne!(first.generation, second.generation);
        assert_eq!(second.scroll, PREVIEW_SCROLL_LINES);
    }

    #[test]
    fn refresh_deck_statuses_hydrates_visible_rows() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "review", false),
        ]);
        app.deck_statuses.clear();

        refresh_deck_statuses(&mut app, &mut |session_ids| {
            Ok(session_ids
                .iter()
                .map(|session_id| {
                    let deck_status = if session_id == "1" {
                        SessionDeckStatus::Waiting
                    } else {
                        SessionDeckStatus::Idle
                    };
                    (session_id.clone(), deck_status.into())
                })
                .collect())
        })
        .unwrap();

        let view = app.view();
        assert_eq!(
            view.visible_sessions
                .iter()
                .map(|session| (session.id.as_str(), session.deck_status))
                .collect::<Vec<_>>(),
            vec![
                ("1", SessionDeckStatus::Waiting),
                ("2", SessionDeckStatus::Idle)
            ]
        );
    }

    #[test]
    fn new_form_builds_shell_request() {
        let form = NewForm {
            name: "demo".to_string(),
            default_name: "generated-demo".to_string(),
            agent: "codex".to_string(),
            path: "/tmp/project".to_string(),
            group_name: "work".to_string(),
            worktree: true,
            carry_state: true,
            field_index: 0,
            launching: false,
        };

        let request = form.build_request().unwrap();

        assert_eq!(request.agent, "codex");
        assert_eq!(request.name, "demo");
        assert_eq!(request.path, "/tmp/project");
        assert_eq!(request.command, "");
        assert_eq!(request.group_name, "work");
        assert!(
            request
                .worktree
                .as_deref()
                .is_some_and(|branch| branch.starts_with("agent-helm/codex/demo-"))
        );
        assert!(request.carry_state);
        assert!(!request.sandbox);
        assert_eq!(request.prompt, None);
    }

    #[test]
    fn new_form_uses_group_defaults() {
        let mut group = group_record("ops", "/tmp/ops", false);
        group.default_agent = Some("codex".to_string());
        group.default_worktree = Some(true);
        group.default_carry_state = Some(true);
        let app = app_from_initial(TuiInitialState {
            sessions: Vec::new(),
            groups: vec![group],
            default_agent: "shell".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
        });

        let form = new_form_for_agent(&app, "shell");

        assert_eq!(form.group_name, "ops");
        assert_eq!(form.path, "/tmp/ops");
        assert_eq!(form.agent, "codex");
        assert!(form.worktree);
        assert!(form.carry_state);
    }

    #[test]
    fn new_session_form_defaults_to_selected_session_group() {
        let mut beta_group = group_record("beta", "/tmp/beta", false);
        beta_group.default_agent = Some("codex".to_string());
        beta_group.default_worktree = Some(true);
        let mut app = app_from_initial(TuiInitialState {
            sessions: vec![
                record("1", "alpha", "build", false),
                record("2", "beta", "deploy", false),
            ],
            groups: vec![group_record("alpha", "/tmp/alpha", false), beta_group],
            default_agent: "shell".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
        });
        app.selected_index = 1;
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.group_name, "beta");
        assert_eq!(form.path, "/tmp/beta");
        assert_eq!(form.agent, "codex");
        assert!(form.worktree);
    }

    #[test]
    fn new_form_uses_placeholder_name_and_full_default_path() {
        let form = NewForm::default();
        let request = form.build_request().unwrap();

        assert!(form.name.is_empty());
        assert!(!form.default_name().is_empty());
        assert!(!form.default_name().chars().any(|ch| ch.is_ascii_digit()));
        assert!(Path::new(&form.path).is_absolute());
        assert_eq!(request.name, form.default_name());
    }

    #[test]
    fn new_form_uses_default_group_path() {
        let mut app = app_from_initial(TuiInitialState {
            sessions: vec![],
            groups: vec![group_record("default", "/tmp/default", false)],
            default_agent: "shell".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.path, "/tmp/default");
    }

    #[test]
    fn new_form_applies_group_path_when_leaving_group_field() {
        let mut app = test_app(vec![]);
        app.group_default_paths
            .insert("work/api".to_string(), "/tmp/api".to_string());
        app.mode = Mode::New(NewForm {
            path: "/tmp/original".to_string(),
            group_name: "work/api".to_string(),
            field_index: 3,
            ..NewForm::default()
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_new_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.path, "/tmp/api");
        assert_eq!(form.current_field(), NewField::Worktree);
    }

    #[test]
    fn new_form_submits_existing_group_without_saving_group_default() {
        let mut app = test_app(vec![]);
        app.group_default_paths
            .insert("work".to_string(), "/tmp/default".to_string());
        app.mode = Mode::New(NewForm {
            name: "deploy".to_string(),
            path: "/tmp/work".to_string(),
            group_name: "work".to_string(),
            field_index: NEW_FIELDS.len() - 1,
            ..NewForm::default()
        });
        let refreshed = vec![record("1", "work", "deploy", false)];
        let mut actions = Vec::new();
        let mut handle = |action| {
            match action {
                TuiAction::Create(request) => {
                    actions.push(format!("create:{}:{}", request.group_name, request.path));
                }
                _ => actions.push("other".to_string()),
            }
            Ok(refreshed.clone())
        };

        handle_new_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(actions, vec!["create:work:/tmp/work"]);
        assert_eq!(
            app.group_default_paths.get("work").map(String::as_str),
            Some("/tmp/default")
        );
        assert_eq!(app.status_message.as_deref(), Some("created session"));
    }

    #[test]
    fn new_form_agent_field_is_selector() {
        let mut app = test_app(vec![]);
        app.mode = Mode::New(NewForm {
            field_index: 1,
            ..NewForm::default()
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_new_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(new_form_agent(&app), Some("shell"));

        handle_new_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(new_form_agent(&app), Some("claude"));

        handle_new_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(new_form_agent(&app), Some("codex"));

        handle_new_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(new_form_agent(&app), Some("claude"));
    }

    #[test]
    fn new_form_agent_selector_uses_configured_choices() {
        let mut app = test_app(vec![]);
        app.agent_choices = vec!["shell".to_string(), "nightly".to_string()];
        app.mode = Mode::New(NewForm {
            field_index: 1,
            ..NewForm::default()
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_new_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(new_form_agent(&app), Some("nightly"));
    }

    #[test]
    fn new_session_remembers_last_selected_agent() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::New(NewForm {
            agent: "codex".to_string(),
            ..NewForm::default()
        });
        let refreshed = vec![record("2", "ops", "new-session", false)];
        let mut handle = |action| {
            if let TuiAction::Create(request) = action {
                assert_eq!(request.agent, "codex");
            }
            Ok(refreshed.clone())
        };

        handle_new_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.last_agent, "codex");

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(new_form_agent(&app), Some("codex"));
    }

    #[test]
    fn capital_n_duplicates_selected_session_settings() {
        let mut session = record("1", "work/api", "deploy", false);
        session.agent = "codex".to_string();
        session.command = "codex --model gpt-5".to_string();
        session.project_path = "/tmp/project".to_string();
        let mut app = test_app(vec![session]);
        app.group_default_paths
            .insert("work/api".to_string(), "/tmp/project".to_string());
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.agent, "codex");
        assert_eq!(form.path, "/tmp/project");
        assert_eq!(form.group_name, "work/api");
        assert_eq!(form.name, "deploy copy");
    }

    #[test]
    fn configured_default_agent_seeds_new_session_form() {
        let mut app = app_from_initial(TuiInitialState {
            sessions: vec![],
            groups: vec![],
            default_agent: "codex".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.agent, "codex");
        assert!(form.worktree);
    }

    #[test]
    fn agent_selector_refreshes_profile_worktree_default() {
        let mut app = test_app(vec![]);
        app.mode = Mode::New(NewForm {
            field_index: 1,
            ..NewForm::with_agent("shell")
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_new_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.agent, "claude");
        assert!(form.worktree);
    }

    #[test]
    fn new_form_requires_name_agent_path_and_group() {
        let mut form = NewForm::default();
        form.agent.clear();
        assert_eq!(form.build_request().unwrap_err(), "agent is required");

        form.agent = "shell".to_string();
        form.path.clear();
        assert_eq!(form.build_request().unwrap_err(), "path is required");

        form.path = ".".to_string();
        assert_eq!(form.build_request().unwrap().command, "");
        form.group_name.clear();
        assert_eq!(form.build_request().unwrap_err(), "group is required");
    }

    #[test]
    fn arrow_keys_do_not_scroll_or_switch_panels() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.detail_scroll, 0);

        handle_normal_key(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn footer_omits_detail_tab_keys() {
        let app = test_app(vec![]);

        assert_eq!(
            footer_text(&app),
            "Enter session r rename m move f fork d delete D cleanup | n new Ctrl-n shell N duplicate | G new group E edit group | g tools | Ctrl-t mouse/select | j/k nav c/e groups Pg scroll / search t status ? help q quit"
        );
    }

    #[test]
    fn r_key_opens_rename_without_action() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let mut calls = 0;
        let mut handle = |_action| {
            calls += 1;
            Ok(Vec::new())
        };

        let quit = handle_normal_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(!quit);
        assert_eq!(calls, 0);
        assert_eq!(
            app.mode,
            Mode::Rename(RenameForm {
                session_id: "1".to_string(),
                name: "deploy".to_string(),
            })
        );
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn pr_number_is_extracted_only_from_explicit_pr_tokens() {
        assert_eq!(extract_pr_number("fix #12345"), Some(12345));
        assert_eq!(extract_pr_number("feature/pr-12345"), Some(12345));
        assert_eq!(
            extract_pr_number("https://github.test/repo/pull/12345"),
            Some(12345)
        );
        assert_eq!(extract_pr_number("release-12345"), None);
    }

    #[test]
    fn help_mode_opens_and_closes() {
        let mut app = test_app(vec![]);
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(footer_text(&app), "Help: Esc/q close");

        handle_help_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn detail_scroll_keys_scroll_and_reset() {
        let mut app = test_app(vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "logs", false),
        ]);
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.detail_scroll, 5);

        handle_normal_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.detail_scroll, 0);
        handle_normal_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.detail_scroll, 0);

        handle_normal_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.detail_scroll, 5);
    }

    #[test]
    fn enter_opens_embedded_session_input_panel() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.sidebar_width = Some(32);

        focus_selected_session(&mut app);

        assert_eq!(app.sidebar_width, Some(32));
        assert_eq!(app.detail_scroll, 0);
        assert!(matches!(app.mode, Mode::Session(_)));
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn normal_mode_ctrl_q_does_not_quit() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let mut handle = |_action| Ok(Vec::new());

        let quit = handle_normal_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(!quit);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn normal_mode_plain_q_quits() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let mut handle = |_action| Ok(Vec::new());

        let quit = handle_normal_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(quit);
    }

    #[test]
    fn status_filter_key_cycles_and_resets_selection() {
        let mut stopped = record("2", "ops", "logs", false);
        stopped.status = SessionStatus::Stopped;
        let mut app = test_app(vec![record("1", "ops", "deploy", false), stopped]);
        app.deck_statuses
            .insert("1".to_string(), SessionDeckStatus::Running.into());
        app.deck_statuses
            .insert("2".to_string(), SessionDeckStatus::Stopped.into());
        let mut handle = |_action| Ok(Vec::new());
        app.selected_index = 1;
        app.detail_scroll = 5;

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.status_filter, StatusFilter::Occupied);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.detail_scroll, 0);

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.status_filter, StatusFilter::Thinking);

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.status_filter, StatusFilter::Running);

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert_eq!(app.status_filter, StatusFilter::Queued);
        for expected in [
            StatusFilter::Waiting,
            StatusFilter::Idle,
            StatusFilter::Stopped,
        ] {
            handle_normal_key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                &mut app,
                &mut handle,
            )
            .unwrap();
            assert_eq!(app.status_filter, expected);
        }
        assert_eq!(app.view().selected.unwrap().id, "2");
    }

    #[test]
    fn group_collapse_emits_persist_action() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut action = None;
        let mut handle = |tui_action| {
            if let TuiAction::SetGroupCollapsed {
                group_name,
                collapsed,
            } = tui_action
            {
                action = Some((group_name, collapsed));
            }
            Ok(refreshed.clone())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(action, Some(("ops".to_string(), true)));
        assert!(app.collapsed_groups.contains("ops"));
        assert_eq!(app.status_message.as_deref(), Some("collapsed group ops"));
    }

    #[test]
    fn group_settings_saves_overrides() {
        let session = record("1", "ops", "deploy", false);
        let group = group_record("ops", "/tmp/ops", false);
        let mut app = app_from_initial(TuiInitialState {
            sessions: vec![session],
            groups: vec![group],
            default_agent: "shell".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
        });
        open_group_settings(&mut app);
        let Mode::GroupSettings(form) = &mut app.mode else {
            panic!("expected group settings form");
        };
        form.default_agent = "codex".to_string();
        form.default_worktree = Some(true);
        form.default_carry_state = Some(true);

        let refreshed = app.sessions.clone();
        let mut saved = None;
        let mut handle = |action| {
            if let TuiAction::UpdateGroup { name, update } = action {
                saved = Some((name, update));
            }
            Ok(refreshed.clone())
        };
        handle_group_settings_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let (name, update) = saved.expect("group update saved");
        assert_eq!(name, "ops");
        assert_eq!(update.default_agent, Some(Some("codex".to_string())));
        assert_eq!(update.default_worktree, Some(Some(true)));
        assert_eq!(update.default_carry_state, Some(Some(true)));
        assert_eq!(app.status_message.as_deref(), Some("updated group ops"));
    }

    #[test]
    fn new_form_prefills_selected_session_context() {
        let mut session = record("1", "work/api", "deploy", false);
        session.project_path = "/tmp/project".to_string();
        let mut app = test_app(vec![session]);
        app.group_default_paths
            .insert("work/api".to_string(), "/tmp/project".to_string());
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::New(form) = app.mode else {
            panic!("expected new form");
        };
        assert_eq!(form.path, "/tmp/project");
        assert_eq!(form.group_name, "work/api");
    }

    #[test]
    fn ctrl_n_creates_empty_command_shell_session() {
        let mut session = record("1", "work/api", "deploy", false);
        session.project_path = "/tmp/project".to_string();
        let mut app = test_app(vec![session]);
        app.group_default_paths
            .insert("work/api".to_string(), "/tmp/project".to_string());
        let mut created = None;
        let mut handle = |action| {
            if let TuiAction::Create(request) = action {
                let mut new_session = record("2", &request.group_name, &request.name, false);
                new_session.project_path = request.path.clone();
                let refreshed = vec![record("1", "work/api", "deploy", false), new_session];
                created = Some(request);
                return Ok(refreshed);
            }
            Ok(Vec::new())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let request = created.expect("expected create action");
        assert_eq!(request.agent, "shell");
        assert_eq!(request.command, "");
        assert!(!request.name.is_empty());
        assert_ne!(request.name, "shell");
        assert_eq!(request.path, "/tmp/project");
        assert_eq!(request.group_name, "work/api");
        assert_eq!(request.worktree, None);
        assert_eq!(request.prompt, None);
        assert_eq!(app.status_message.as_deref(), Some("created shell session"));
    }

    #[test]
    fn ctrl_n_without_selection_uses_default_path_and_generated_name() {
        let mut app = test_app(Vec::new());
        let mut created = None;
        let mut handle = |action| {
            if let TuiAction::Create(request) = action {
                let new_session = record("1", &request.group_name, &request.name, false);
                created = Some(request);
                return Ok(vec![new_session]);
            }
            Ok(Vec::new())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let request = created.expect("expected create action");
        assert_eq!(request.agent, "shell");
        assert_eq!(request.command, "");
        assert_eq!(request.path, default_new_session_path());
        assert!(!request.name.is_empty());
        assert_ne!(request.name, "shell");
        assert_eq!(request.group_name, "default");
    }

    #[test]
    fn move_key_prefills_selected_session_group() {
        let mut app = test_app(vec![record("1", "work/api", "deploy", false)]);
        let mut handle = |_action| Ok(Vec::new());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let Mode::Move(form) = app.mode else {
            panic!("expected move form");
        };
        assert_eq!(form.session_id, "1");
        assert_eq!(form.group_name, "work/api");
    }

    #[test]
    fn rename_key_prefills_selected_session_name() {
        let mut app = test_app(vec![record("1", "work/api", "deploy", false)]);
        let mut handle = |_action| Ok(Vec::new());
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        let Mode::Rename(form) = app.mode else {
            panic!("expected rename form");
        };
        assert_eq!(form.session_id, "1");
        assert_eq!(form.name, "deploy");
    }

    #[test]
    fn move_form_submits_group_change_and_preserves_selection() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.group_default_paths
            .insert("core".to_string(), String::new());
        app.mode = Mode::Move(MoveForm {
            session_id: "1".to_string(),
            group_name: "core".to_string(),
        });
        let refreshed = vec![
            record("2", "ops", "other", false),
            record("1", "core", "deploy", false),
        ];
        let mut moved = None;
        let mut handle = |action| {
            if let TuiAction::MoveToGroup {
                session_id,
                group_name,
            } = action
            {
                moved = Some((session_id, group_name));
            }
            Ok(refreshed.clone())
        };

        handle_move_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let selected = app.view().selected.unwrap();
        assert_eq!(moved, Some(("1".to_string(), "core".to_string())));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(selected.id, "1");
        assert_eq!(selected.group_name, "core");
        assert_eq!(app.status_message.as_deref(), Some("moved session"));
    }

    #[test]
    fn rename_form_submits_name_change_and_preserves_selection() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Rename(RenameForm {
            session_id: "1".to_string(),
            name: "release".to_string(),
        });
        let refreshed = vec![
            record("2", "ops", "other", false),
            record("1", "ops", "release", false),
        ];
        let mut renamed = None;
        let mut handle = |action| {
            if let TuiAction::RenameSession { session_id, name } = action {
                renamed = Some((session_id, name));
            }
            Ok(refreshed.clone())
        };

        handle_rename_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let selected = app.view().selected.unwrap();
        assert_eq!(renamed, Some(("1".to_string(), "release".to_string())));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(selected.id, "1");
        assert_eq!(selected.name, "release");
        assert_eq!(app.status_message.as_deref(), Some("renamed session"));
    }

    #[test]
    fn rename_form_rejects_empty_name() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Rename(RenameForm {
            session_id: "1".to_string(),
            name: "   ".to_string(),
        });
        let mut handle = |_action| Ok(Vec::new());

        handle_rename_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(matches!(app.mode, Mode::Rename(_)));
        assert_eq!(app.status_message.as_deref(), Some("name required"));
    }

    #[test]
    fn move_form_rejects_unknown_group() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Move(MoveForm {
            session_id: "1".to_string(),
            group_name: "missing".to_string(),
        });
        let mut actions = 0;
        let mut handle = |_action| {
            actions += 1;
            Ok(Vec::new())
        };

        handle_move_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(actions, 0);
        assert!(matches!(app.mode, Mode::Move(_)));
        assert_eq!(
            app.status_message.as_deref(),
            Some("group must already exist")
        );
    }

    #[test]
    fn selected_action_can_attach_session() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut attached = None;
        let mut handle = |action| {
            if let TuiAction::Attach(id) = action {
                attached = Some(id);
            }
            Ok(refreshed.clone())
        };

        run_selected_action(&mut app, &mut handle, TuiAction::Attach, "attached").unwrap();

        assert_eq!(attached.as_deref(), Some("1"));
        assert_eq!(app.status_message.as_deref(), Some("attached"));
    }

    #[test]
    fn selected_action_invalidates_deck_status_and_details_cache() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let mut stopped = record("1", "ops", "deploy", false);
        stopped.status = SessionStatus::Stopped;
        let refreshed = vec![stopped];
        let mut stop_requested = false;
        let mut handle = |action| {
            if let TuiAction::Stop(id) = action {
                stop_requested = id == "1";
            }
            Ok(refreshed.clone())
        };

        run_selected_action(&mut app, &mut handle, TuiAction::Stop, "stopped").unwrap();

        assert!(stop_requested);
        assert!(!app.deck_statuses.contains_key("1"));
        assert_eq!(app.details_session_id, None);
    }

    #[test]
    fn selected_action_preserves_session_after_reordered_refresh() {
        let mut older = record("1", "ops", "older", false);
        older.updated_at = 2;
        let mut selected = record("2", "ops", "selected", false);
        selected.updated_at = 1;
        let mut app = test_app(vec![older.clone(), selected.clone()]);
        app.selected_index = 1;
        app.detail_scroll = 10;

        selected.updated_at = 3;
        let refreshed = vec![older, selected];
        let mut stopped = None;
        let mut handle = |action| {
            if let TuiAction::Stop(id) = action {
                stopped = Some(id);
            }
            Ok(refreshed.clone())
        };

        run_selected_action(&mut app, &mut handle, TuiAction::Stop, "stopped").unwrap();

        assert_eq!(stopped.as_deref(), Some("2"));
        assert_eq!(app.view().selected.unwrap().id, "2");
        assert_eq!(app.detail_scroll, 0);
        assert_eq!(app.status_message.as_deref(), Some("stopped"));
    }

    #[test]
    fn create_selects_created_session() {
        let mut app = test_app(vec![record("old", "ops", "alpha", false)]);
        let form = NewForm::default();
        let default_name = form.default_name();
        app.mode = Mode::New(form);
        let refreshed = vec![
            record("old", "ops", "alpha", false),
            record("new", "default", &default_name, false),
        ];
        let mut created = false;
        let expected_name = default_name.clone();
        let mut handle = |action| {
            if let TuiAction::Create(request) = action {
                created = request.name == expected_name;
            }
            Ok(refreshed.clone())
        };

        handle_new_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(created);
        assert_eq!(app.view().selected.unwrap().id, "new");
        assert_eq!(app.status_message.as_deref(), Some("created session"));
    }

    #[test]
    fn create_with_background_returns_pending_row_before_completion() {
        let mut app = test_app(vec![record("old", "ops", "alpha", false)]);
        let form = NewForm::default();
        let default_name = form.default_name();
        app.mode = Mode::New(form);
        let mut requests = Vec::new();

        handle_new_key_with_background(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut |request| requests.push(request),
        )
        .unwrap();

        assert_eq!(requests.len(), 1);
        assert!(matches!(requests[0].action, TuiAction::Create(_)));
        let selected = app.view().selected.unwrap();
        assert!(selected.id.starts_with("tui-pending-"));
        assert_eq!(selected.name, default_name);
        assert_eq!(selected.deck_status, SessionDeckStatus::Starting);
        assert_eq!(session_activity_label(&selected), Some("creating"));
        assert_eq!(app.status_message.as_deref(), Some("creating"));
    }

    #[test]
    fn create_background_success_replaces_placeholder_and_selects_created_session() {
        let mut app = test_app(vec![record("old", "ops", "alpha", false)]);
        let form = NewForm::default();
        let default_name = form.default_name();
        app.mode = Mode::New(form);
        let mut requests = Vec::new();
        handle_new_key_with_background(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut |request| requests.push(request),
        )
        .unwrap();
        let pending_id = requests[0].session_id.clone();
        let mut created = record("new", "default", &default_name, false);
        created.updated_at = 10;
        let mut handle = |_action| Ok(Vec::new());

        apply_background_action_result(
            &mut app,
            BackgroundActionResult {
                session_id: pending_id.clone(),
                result: Ok(vec![record("old", "ops", "alpha", false), created]),
            },
            &mut handle,
        )
        .unwrap();

        assert!(!app.pending_operations.contains_key(&pending_id));
        assert!(!app.sessions.iter().any(|session| session.id == pending_id));
        assert_eq!(app.view().selected.unwrap().id, "new");
        assert_eq!(app.status_message.as_deref(), Some("created session"));
    }

    #[test]
    fn delete_with_background_marks_row_and_completion_preserves_neighbor_selection() {
        let mut app = test_app(vec![
            record("1", "ops", "alpha", false),
            record("2", "ops", "beta", false),
        ]);
        let mut handle = |_action| Ok(Vec::new());
        let mut requests = Vec::new();

        run_selected_action_with_background(
            &mut app,
            &mut handle,
            &mut |request| requests.push(request),
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::CleanupWorktree,
            },
            "deleted and cleaned up",
        )
        .unwrap();

        assert_eq!(requests.len(), 1);
        let selected = app.view().selected.unwrap();
        assert_eq!(selected.id, "1");
        assert_eq!(selected.deck_status, SessionDeckStatus::Starting);
        assert_eq!(session_activity_label(&selected), Some("deleting worktree"));

        apply_background_action_result(
            &mut app,
            BackgroundActionResult {
                session_id: "1".to_string(),
                result: Ok(vec![record("2", "ops", "beta", false)]),
            },
            &mut handle,
        )
        .unwrap();

        assert!(!app.pending_operations.contains_key("1"));
        assert_eq!(app.view().selected.unwrap().id, "2");
        assert_eq!(
            app.status_message.as_deref(),
            Some("deleted and cleaned up")
        );
    }

    #[test]
    fn create_failure_keeps_errored_placeholder_and_delete_dismisses_it() {
        let mut app = test_app(vec![record("old", "ops", "alpha", false)]);
        app.mode = Mode::New(NewForm::default());
        let mut requests = Vec::new();
        handle_new_key_with_background(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut |request| requests.push(request),
        )
        .unwrap();
        let pending_id = requests[0].session_id.clone();
        let mut handle = |_action| Ok(Vec::new());

        apply_background_action_result(
            &mut app,
            BackgroundActionResult {
                session_id: pending_id.clone(),
                result: Err("boom".to_string()),
            },
            &mut handle,
        )
        .unwrap();

        let selected = app.view().selected.unwrap();
        assert_eq!(selected.id, pending_id);
        assert_eq!(selected.deck_status, SessionDeckStatus::Errored);
        assert_eq!(session_activity_label(&selected), Some("create failed"));
        assert_eq!(app.status_message.as_deref(), Some("create failed: boom"));

        let mut dismiss_requests = Vec::new();
        run_selected_action_with_background(
            &mut app,
            &mut handle,
            &mut |request| dismiss_requests.push(request),
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::MetadataOnly,
            },
            "deleted",
        )
        .unwrap();

        assert!(dismiss_requests.is_empty());
        assert!(!app.sessions.iter().any(|session| session.id == pending_id));
        assert_eq!(
            app.status_message.as_deref(),
            Some("dismissed failed create")
        );
    }

    #[test]
    fn delete_failure_keeps_row_failed_and_retry_restarts_background_delete() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let mut handle = |_action| Ok(Vec::new());
        let mut requests = Vec::new();
        run_selected_action_with_background(
            &mut app,
            &mut handle,
            &mut |request| requests.push(request),
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::MetadataOnly,
            },
            "deleted",
        )
        .unwrap();

        apply_background_action_result(
            &mut app,
            BackgroundActionResult {
                session_id: "1".to_string(),
                result: Err("busy".to_string()),
            },
            &mut handle,
        )
        .unwrap();

        let selected = app.view().selected.unwrap();
        assert_eq!(selected.id, "1");
        assert_eq!(selected.deck_status, SessionDeckStatus::Errored);
        assert_eq!(session_activity_label(&selected), Some("delete failed"));

        let mut retry_requests = Vec::new();
        run_selected_action_with_background(
            &mut app,
            &mut handle,
            &mut |request| retry_requests.push(request),
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::MetadataOnly,
            },
            "deleted",
        )
        .unwrap();

        assert_eq!(retry_requests.len(), 1);
        let selected = app.view().selected.unwrap();
        assert_eq!(selected.deck_status, SessionDeckStatus::Starting);
        assert_eq!(session_activity_label(&selected), Some("deleting"));
    }

    #[test]
    fn pending_status_survives_session_and_deck_refresh() {
        let old = record("old", "ops", "alpha", false);
        let mut app = test_app(vec![old.clone()]);
        app.mode = Mode::New(NewForm::default());
        let mut requests = Vec::new();
        handle_new_key_with_background(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut |request| requests.push(request),
        )
        .unwrap();
        let pending_id = requests[0].session_id.clone();
        let mut handle = |action| match action {
            TuiAction::Refresh => Ok(vec![old.clone()]),
            _ => Ok(Vec::new()),
        };
        refresh_sessions(&mut app, &mut handle).unwrap();

        let mut status_ids = Vec::new();
        refresh_deck_statuses(&mut app, &mut |ids| {
            status_ids = ids.to_vec();
            Ok(vec![("old".to_string(), SessionDeckStatus::Idle.into())])
        })
        .unwrap();

        assert!(!status_ids.contains(&pending_id));
        let pending = app
            .view()
            .visible_sessions
            .into_iter()
            .find(|session| session.id == pending_id)
            .unwrap();
        assert_eq!(pending.deck_status, SessionDeckStatus::Starting);
        assert_eq!(session_activity_label(&pending), Some("creating"));
    }

    #[test]
    fn select_matching_session_respects_collapsed_groups() {
        let mut app = test_app(vec![
            record("1", "ops", "hidden", false),
            record("2", "core", "visible", false),
        ]);
        app.collapsed_groups.insert("ops".to_string());

        select_matching_session(&mut app, |session| session.id == "1");

        assert_eq!(app.selected_index, 0);
        assert_eq!(app.view().selected.unwrap().id, "2");
    }

    #[test]
    fn refresh_sessions_preserves_selection_and_updates_status() {
        let mut app = test_app(vec![
            record("1", "ops", "alpha", false),
            record("2", "ops", "omega", false),
        ]);
        let mut refreshed = app.sessions.clone();
        refreshed[0].status = SessionStatus::Stopped;
        let mut refreshed_once = false;
        let mut handle = |action| {
            if matches!(action, TuiAction::Refresh) {
                refreshed_once = true;
            }
            Ok(refreshed.clone())
        };

        refresh_sessions(&mut app, &mut handle).unwrap();

        let selected = app.view().selected.unwrap();
        assert!(refreshed_once);
        assert_eq!(selected.id, "1");
        assert_eq!(selected.status, SessionStatus::Stopped);
    }

    #[test]
    fn normal_delete_keys_run_selected_remove_actions() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut actions = Vec::new();
        let mut handle = |action| {
            match action {
                TuiAction::Remove { session_id, mode } => {
                    actions.push((session_id, mode));
                }
                _ => actions.push(("other".to_string(), DeleteMode::Purge)),
            }
            Ok(refreshed.clone())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(
            actions,
            vec![
                ("1".to_string(), DeleteMode::MetadataOnly),
                ("1".to_string(), DeleteMode::CleanupWorktree),
            ]
        );
    }

    #[test]
    fn removed_normal_hotkeys_do_nothing() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut actions = Vec::new();
        let mut handle = |action| {
            actions.push(format!("{action:?}"));
            Ok(refreshed.clone())
        };
        for key in [
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT),
        ] {
            handle_normal_key(key, &mut app, &mut handle).unwrap();
        }
        assert!(actions.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn fork_form_submits_options() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = vec![
            record("1", "ops", "deploy", false),
            record("2", "qa", "deploy child", false),
        ];
        let mut fork = None;
        let mut handle = |action| {
            if let TuiAction::Fork(request) = action {
                fork = Some(request);
            }
            Ok(refreshed.clone())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        assert!(matches!(app.mode, Mode::Fork(_)));

        if let Mode::Fork(form) = &mut app.mode {
            form.name = "deploy child".to_string();
            form.group_name = "qa".to_string();
            form.worktree = "feature/fork".to_string();
            form.carry_state = true;
            form.start = "false".to_string();
        }
        handle_fork_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let request = fork.unwrap();
        assert_eq!(request.parent_session_id, "1");
        assert_eq!(request.name.as_deref(), Some("deploy child"));
        assert_eq!(request.group_name.as_deref(), Some("qa"));
        assert_eq!(request.worktree_branch.as_deref(), Some("feature/fork"));
        assert!(request.carry_state);
        assert!(!request.start_immediately);
        assert_eq!(app.view().selected.unwrap().id, "2");
        assert_eq!(app.status_message.as_deref(), Some("forked session"));
    }

    #[test]
    fn session_mode_ctrl_q_returns_to_dashboard() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Session(SendForm::default());

        handle_session_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &mut app,
        )
        .unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.status_message.as_deref(), Some("returned to dashboard"));
    }

    #[test]
    fn session_mode_ctrl_t_toggles_mouse_capture() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Session(SendForm::default());

        handle_session_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut app,
        )
        .unwrap();

        assert!(!app.mouse_capture);
        assert_eq!(
            app.status_message.as_deref(),
            Some("terminal text selection enabled")
        );

        handle_session_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut app,
        )
        .unwrap();

        assert!(app.mouse_capture);
        assert_eq!(
            app.status_message.as_deref(),
            Some("mouse wheel scrolling enabled")
        );
    }

    #[test]
    fn focused_session_defaults_to_mouse_scrolling() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);

        focus_selected_session(&mut app);

        assert!(matches!(app.mode, Mode::Session(_)));
        assert!(app.mouse_capture);
        assert!(wants_mouse_capture(&app));
    }

    #[test]
    fn focused_session_preserves_text_selection_mode() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mouse_capture = false;

        focus_selected_session(&mut app);

        assert!(matches!(app.mode, Mode::Session(_)));
        assert!(!app.mouse_capture);
        assert!(!wants_mouse_capture(&app));
        assert_eq!(
            footer_text(&app),
            "Session: text selection mode | Ctrl-q dashboard | Ctrl-t wheel scroll"
        );
    }

    #[test]
    fn mouse_capture_is_enabled_for_preview_and_session() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        assert!(wants_mouse_capture(&app));

        app.mode = Mode::Session(SendForm::default());
        assert!(wants_mouse_capture(&app));

        app.mouse_capture = false;
        assert!(!wants_mouse_capture(&app));
    }

    #[test]
    fn session_mode_only_plain_ctrl_q_returns_to_dashboard() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Session(SendForm::default());
        handle_session_key(
            KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            &mut app,
        )
        .unwrap();
        assert!(matches!(app.mode, Mode::Session(_)));
        assert_eq!(app.status_message, None);
    }

    #[test]
    fn global_ctrl_c_does_not_quit_session_mode() {
        assert!(!is_global_quit_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &Mode::Session(SendForm::default()),
        ));
        assert!(is_global_quit_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &Mode::Normal,
        ));
    }

    #[test]
    fn render_new_form_marks_active_text_entry() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![]);
        app.mode = Mode::New(NewForm {
            name: "deploy".to_string(),
            ..NewForm::default()
        });

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("NEW SESSION"));
        assert!(text.contains("[ deploy ]"));
        assert!(terminal.backend().cursor_visible());
        let Mode::New(form) = &app.mode else {
            panic!("expected new form");
        };
        let expected_cursor = new_form_cursor_position(
            form,
            body_layout_for_view(
                Rect::new(0, 4, 100, 13),
                &app.view(),
                app.sidebar_width,
                app.animation_frame,
            )
            .detail,
        )
        .unwrap();
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), expected_cursor);
    }

    #[test]
    fn new_session_submit_draws_launching_feedback() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![]);
        app.mode = Mode::New(NewForm {
            name: "deploy".to_string(),
            path: "/tmp/project".to_string(),
            group_name: "ops".to_string(),
            field_index: NEW_FIELDS.len() - 1,
            ..NewForm::default()
        });
        let refreshed = vec![record("1", "ops", "deploy", false)];
        let mut handle = |action| {
            assert!(matches!(action, TuiAction::Create(_)));
            Ok(refreshed.clone())
        };

        process_key(
            &mut terminal,
            &mut app,
            &mut handle,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        )
        .unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Launching session..."));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status_message.as_deref(), Some("created session"));
    }

    #[test]
    fn render_shows_dashboard_panel_for_selected_session() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = test_app(vec![record("1", "ops", "deploy", false)]);

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Agent Helm"));
        assert!(text.contains("SESSIONS"));
        assert!(text.contains("1. ops (1)"));
        assert!(text.contains("PREVIEW deploy"));
        assert!(text.contains("Loading terminal preview"));
        assert!(!text.contains("latest output"));
    }

    #[test]
    fn running_preview_renders_cached_terminal_snapshot() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let area = terminal_preview_area(
            ratatui::layout::Size {
                width: 100,
                height: 18,
            },
            &app.view(),
            app.sidebar_width,
        );
        let target = tmux_session_name_for_id("1");
        app.preview = Some(TerminalPreview {
            session_id: "1".to_string(),
            target,
            parser: Some(terminal_parser_from_bytes(
                b"preview-ready\r\n",
                area.height,
                area.width,
            )),
            error: None,
            refreshed_at: Instant::now(),
            rows: area.height,
            cols: area.width,
            scroll: 0,
        });

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("preview-ready"));
        assert!(!text.contains("latest output"));
    }

    #[test]
    fn terminal_preview_prefers_non_empty_alternate_screen() {
        let parser = terminal_preview_parser_from_captures(
            Some(b"alternate-ready\r\n".to_vec()),
            Some(b"visible-ready\r\n".to_vec()),
            4,
            40,
        )
        .unwrap();
        let contents = parser.screen().contents();

        assert!(contents.contains("alternate-ready"), "{contents}");
        assert!(!contents.contains("visible-ready"), "{contents}");
    }

    #[test]
    fn terminal_preview_falls_back_when_alternate_screen_is_empty() {
        let parser = terminal_preview_parser_from_captures(
            Some(b"\r\n\r\n".to_vec()),
            Some(b"visible-ready\r\n".to_vec()),
            4,
            40,
        )
        .unwrap();
        let contents = parser.screen().contents();

        assert!(contents.contains("visible-ready"), "{contents}");
    }

    #[test]
    fn terminal_preview_capture_newlines_return_to_left_edge() {
        let parser = terminal_parser_from_bytes(b"one\ntwo\nthree\n", 6, 20);
        let screen = parser.screen();

        assert_eq!(screen.cell(0, 0).unwrap().contents(), "o");
        assert_eq!(screen.cell(1, 0).unwrap().contents(), "t");
        assert_eq!(screen.cell(2, 0).unwrap().contents(), "t");
        assert!(!screen.cell(1, 3).unwrap().has_contents());
    }

    #[test]
    fn terminal_preview_trailing_capture_newline_does_not_shift_rows() {
        let parser = terminal_parser_from_bytes(b"first\nsecond\nthird\n", 3, 20);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 3));

        render_vt100_screen_tail(parser.screen(), &mut buffer, Rect::new(0, 0, 20, 3), 0);
        let text = buffer_text(&buffer);

        assert!(text.contains("first"), "{text}");
        assert!(text.contains("second"), "{text}");
        assert!(text.contains("third"), "{text}");
    }

    #[test]
    fn terminal_preview_renders_most_recent_captured_rows() {
        let parser = terminal_parser_from_bytes(b"old\nmiddle\nnewest", 2, 20);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));

        render_vt100_screen_tail(parser.screen(), &mut buffer, Rect::new(0, 0, 20, 2), 0);
        let text = buffer_text(&buffer);

        assert!(!text.contains("old"), "{text}");
        assert!(text.contains("middle"), "{text}");
        assert!(text.contains("newest"), "{text}");
    }

    #[test]
    fn terminal_preview_scroll_offset_moves_up_history() {
        let parser = terminal_parser_from_bytes(b"old\nmiddle\nnewest", 2, 20);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));

        render_vt100_screen_tail(parser.screen(), &mut buffer, Rect::new(0, 0, 20, 2), 1);
        let text = buffer_text(&buffer);

        assert!(text.contains("old"), "{text}");
        assert!(text.contains("middle"), "{text}");
        assert!(!text.contains("newest"), "{text}");
    }

    #[test]
    fn hidden_terminal_cursor_is_rendered_as_overlay() {
        let parser = terminal_parser_from_bytes(b"abc\x1b[1;2H\x1b[?25l", 3, 10);
        assert!(parser.screen().hide_cursor());

        let area = Rect::new(0, 0, 10, 3);
        let mut buffer = Buffer::empty(area);
        render_vt100_screen(parser.screen(), &mut buffer, area);

        let (row, col) = parser.screen().cursor_position();
        assert!(
            !buffer
                .cell((col, row))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );

        render_vt100_cursor_overlay(parser.screen(), &mut buffer, area);

        assert!(
            buffer
                .cell((col, row))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn terminal_preview_cache_invalidates_by_session_target_size_and_age() {
        let now = Instant::now();
        let preview = TerminalPreview {
            session_id: "1".to_string(),
            target: "agent-helm-1".to_string(),
            parser: Some(terminal_parser_from_bytes(b"ready\r\n", 4, 40)),
            error: None,
            refreshed_at: now,
            rows: 4,
            cols: 40,
            scroll: 0,
        };

        assert!(preview.is_current("1", "agent-helm-1", 4, 40, 0));
        assert!(!preview.is_current("2", "agent-helm-1", 4, 40, 0));
        assert!(!preview.is_current("1", "agent-helm-2", 4, 40, 0));
        assert!(!preview.is_current("1", "agent-helm-1", 5, 40, 0));
        assert!(!preview.is_current("1", "agent-helm-1", 4, 41, 0));
        assert!(!preview.is_current("1", "agent-helm-1", 4, 40, 1));
        assert!(!preview.is_stale(now + PREVIEW_REFRESH_INTERVAL - Duration::from_millis(1)));
        assert!(preview.is_stale(now + PREVIEW_REFRESH_INTERVAL));
    }

    #[test]
    fn render_session_terminal_shows_attach_placeholder() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Session(SendForm::default());

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("SESSION"));
        assert!(text.contains("Attaching tmux session"));
        assert!(!text.contains("latest output"));
    }

    #[test]
    fn render_session_terminal_highlights_live_session_border() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Session(SendForm::default());

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let inner = embedded_terminal_area(
            ratatui::layout::Size {
                width: 100,
                height: 18,
            },
            &app.view(),
            app.sidebar_width,
        );
        let border_cell = terminal
            .backend()
            .buffer()
            .cell((inner.x.saturating_sub(1), inner.y.saturating_sub(1)))
            .unwrap();
        assert_eq!(Some(border_cell.fg), active_border_style().fg);
    }

    #[test]
    fn render_session_terminal_does_not_mirror_long_output() {
        let backend = ratatui::backend::TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Session(SendForm::default());
        app.details.output = (0..80)
            .map(|index| format!("output-line-{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("SESSION"));
        assert!(text.contains("Attaching tmux session"));
        assert!(!text.contains("output-line-79"), "{text}");
    }

    #[test]
    fn embedded_tmux_smoke_test() {
        if std::env::var_os("AGENT_HELM_TMUX_SMOKE").is_none() {
            return;
        }
        if !Command::new("tmux")
            .arg("-V")
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let target = format!("agent-helm-embed-smoke-{}", Uuid::new_v4());
        let status = Command::new("tmux")
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&target)
            .arg("sh")
            .arg("-lc")
            .arg("printf 'embed-ready\\n'; sleep 5")
            .status()
            .expect("spawn tmux smoke session");
        assert!(status.success());

        let mut embedded = EmbeddedTmux::spawn(target.clone(), 8, 40).expect("embed tmux session");
        std::thread::sleep(Duration::from_millis(200));
        embedded.drain();
        let contents = embedded.parser.screen().contents();
        let _ = Command::new("tmux")
            .arg("kill-session")
            .arg("-t")
            .arg(&target)
            .status();

        assert!(contents.contains("embed-ready"), "{contents}");
    }

    #[test]
    fn embedded_key_encodes_shift_tab_for_tmux() {
        assert_eq!(
            encode_embedded_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), false)
                .as_deref(),
            Some(&b"\x1b[Z"[..])
        );
    }

    #[test]
    fn terminal_preview_smoke_test() {
        if std::env::var_os("AGENT_HELM_TMUX_SMOKE").is_none() {
            return;
        }
        if !Command::new("tmux")
            .arg("-V")
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let target = format!("agent-helm-preview-smoke-{}", Uuid::new_v4());
        let status = Command::new("tmux")
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&target)
            .arg("sh")
            .arg("-lc")
            .arg("printf 'preview-ready\\n'; sleep 5")
            .status()
            .expect("spawn tmux preview smoke session");
        assert!(status.success());

        let mut contents = String::new();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            let preview = TerminalPreview::capture("smoke".to_string(), target.clone(), 8, 40, 0);
            let parser = preview.parser.expect("capture tmux preview");
            contents = parser.screen().contents();
            if contents.contains("preview-ready") {
                break;
            }
        }
        let _ = Command::new("tmux")
            .arg("kill-session")
            .arg("-t")
            .arg(&target)
            .status();

        assert!(contents.contains("preview-ready"), "{contents}");
    }

    #[test]
    fn session_list_shows_colored_pr_badge_when_known() {
        let backend = ratatui::backend::TestBackend::new(200, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let session = record("1", "ops", "fix pr-12345", false);
        let app = test_app(vec![session]);

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer_text(buffer);
        let hash_cell = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "#")
            .expect("PR badge hash is rendered");

        assert!(text.contains("#12345"));
        assert_eq!(hash_cell.fg, Color::Rgb(255, 184, 108));
    }

    #[test]
    fn session_list_omits_pr_badge_when_unknown() {
        let backend = ratatui::backend::TestBackend::new(200, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = test_app(vec![record("1", "ops", "deploy", false)]);

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(!text.contains("#12345"));
    }

    #[test]
    fn session_list_shows_agent_activity_label() {
        let backend = ratatui::backend::TestBackend::new(200, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.deck_statuses.insert(
            "1".to_string(),
            TuiSessionStatus {
                deck_status: SessionDeckStatus::Running,
                activity: Some(SessionActivity {
                    state: "working".to_string(),
                    label: "using Bash".to_string(),
                    source: "claude_transcript".to_string(),
                    tool: Some("Bash".to_string()),
                }),
            },
        );

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        let row = text
            .lines()
            .find(|line| line.contains("`-") && line.contains("deploy"))
            .expect("session row");

        assert!(row.contains("◐"));
        assert!(row.contains("using Bash"));
        assert!(!row.contains("running"));
    }

    #[test]
    fn session_list_suppresses_redundant_agent_activity_label() {
        let backend = ratatui::backend::TestBackend::new(200, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut session = record("1", "ops", "deploy", false);
        session.agent = "codex".to_string();
        let mut app = test_app(vec![session]);
        app.deck_statuses.insert(
            "1".to_string(),
            TuiSessionStatus {
                deck_status: SessionDeckStatus::Running,
                activity: Some(SessionActivity {
                    state: "working".to_string(),
                    label: "using codex".to_string(),
                    source: "codex_transcript".to_string(),
                    tool: Some("codex".to_string()),
                }),
            },
        );

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        let row = text
            .lines()
            .find(|line| line.contains("`-") && line.contains("deploy"))
            .expect("session row");

        assert!(row.contains("codex"));
        assert!(row.contains("◐"));
        assert!(!row.contains("running"));
        assert!(!row.contains("using codex"));
    }

    #[test]
    fn session_list_does_not_animate_runtime_start_activity() {
        let mut session = SessionSummary::from(&record("1", "ops", "deploy", false));
        session.activity = Some(SessionActivity {
            state: "running".to_string(),
            label: "using codex".to_string(),
            source: "runtime_start".to_string(),
            tool: Some("codex".to_string()),
        });

        assert_eq!(session_activity_marker(&session, 0), "●");
        assert_eq!(session_activity_marker(&session, 1), "●");
        assert_eq!(session_activity_marker(&session, 2), "●");
        assert_eq!(session_activity_label(&session), None);
    }

    #[test]
    fn session_activity_marker_uses_unicode_status_indicators() {
        let running = SessionSummary::from(&record("1", "ops", "deploy", false));
        let activity = SessionActivity {
            state: "working".to_string(),
            label: "using Bash".to_string(),
            source: "claude_transcript".to_string(),
            tool: Some("Bash".to_string()),
        };
        let mut active = running.clone();
        active.activity = Some(activity.clone());
        let mut occupied = running.clone();
        occupied.deck_status = SessionDeckStatus::Occupied;
        let mut active_occupied = occupied.clone();
        active_occupied.activity = Some(activity.clone());
        let mut thinking = running.clone();
        thinking.deck_status = SessionDeckStatus::Thinking;
        let mut active_thinking = thinking.clone();
        active_thinking.activity = Some(activity.clone());
        let mut starting = running.clone();
        starting.deck_status = SessionDeckStatus::Starting;
        let mut active_starting = starting.clone();
        active_starting.activity = Some(activity);
        let mut stopped_record = record("2", "ops", "done", false);
        stopped_record.status = SessionStatus::Stopped;
        let stopped = SessionSummary::from(&stopped_record);
        let mut waiting = running.clone();
        waiting.deck_status = SessionDeckStatus::Waiting;
        let mut queued = running.clone();
        queued.deck_status = SessionDeckStatus::Queued;
        let mut idle = running.clone();
        idle.deck_status = SessionDeckStatus::Idle;
        let mut errored = running.clone();
        errored.deck_status = SessionDeckStatus::Errored;

        assert_eq!(session_activity_marker(&occupied, 0), "◆");
        assert_eq!(session_activity_marker(&active_occupied, 0), "◆");
        assert_eq!(session_activity_marker(&active_occupied, 1), "◇");
        assert_eq!(session_activity_marker(&thinking, 0), "✦");
        assert_eq!(session_activity_marker(&active_thinking, 0), "✦");
        assert_eq!(session_activity_marker(&active_thinking, 1), "✧");
        assert_eq!(session_activity_marker(&running, 0), "●");
        assert_eq!(session_activity_marker(&running, 2), "●");
        assert_eq!(session_activity_marker(&active, 0), "◐");
        assert_eq!(session_activity_marker(&active, 2), "◑");
        assert_eq!(session_activity_marker(&starting, 0), "▸");
        assert_eq!(session_activity_marker(&active_starting, 0), "▸");
        assert_eq!(session_activity_marker(&active_starting, 1), "▹");
        assert_eq!(session_activity_marker(&queued, 0), "⋯");
        assert_eq!(session_activity_marker(&waiting, 0), "◌");
        assert_eq!(session_activity_marker(&idle, 0), "·");
        assert_eq!(session_activity_marker(&stopped, 0), "■");
        assert_eq!(session_activity_marker(&stopped, 2), "■");
        assert_eq!(session_activity_marker(&errored, 0), "×");
    }

    #[test]
    fn session_row_uses_marker_instead_of_status_word() {
        let sessions = vec![record("1", "ops", "deploy", false)];
        let view = DashboardView::build_with_statuses(
            &sessions,
            &BTreeMap::from([("1".to_string(), SessionDeckStatus::Thinking.into())]),
            &BTreeSet::new(),
            "",
            0,
            StatusFilter::All,
        );
        let backend = ratatui::backend::TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let (list, mut state) = session_list(&view, 0);
                frame.render_stateful_widget(list, frame.area(), &mut state);
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        let row = text
            .lines()
            .find(|line| line.contains("`-") && line.contains("deploy"))
            .expect("session row");

        assert!(row.contains("✦"));
        assert!(!row.contains("thinking"));
        assert!(!row.contains("occupied"));
        assert!(!row.contains("idle"));
    }

    #[test]
    fn render_header_shows_headroom_metrics_when_available() {
        let backend = ratatui::backend::TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.headroom_metrics = Some(HeadroomMetrics {
            requests: 12,
            tokens_saved: 3456,
            savings_percent: 27.5,
            compression_savings_usd: 42.0,
        });

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("hr 12 req 3.5k saved 27.5% $42"));
    }

    #[test]
    fn headroom_metrics_loads_display_session_from_proxy_savings_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{
                "display_session": {
                    "requests": 7,
                    "tokens_saved": 1200,
                    "savings_percent": 31.25,
                    "compression_savings_usd": 9.5
                },
                "lifetime": {
                    "requests": 99,
                    "tokens_saved": 1,
                    "savings_percent": 1.0,
                    "compression_savings_usd": 1.0
                }
            }"#,
        )
        .unwrap();

        let metrics = HeadroomMetrics::load(file.path()).unwrap();

        assert_eq!(metrics.requests, 7);
        assert_eq!(metrics.tokens_saved, 1200);
        assert_eq!(metrics.savings_percent, 31.25);
        assert_eq!(metrics.compression_savings_usd, 9.5);
    }

    #[test]
    fn dashboard_active_session_shows_terminal_preview_instead_of_mirrored_output() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = test_app(vec![record("1", "ops", "deploy", false)]);

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("PREVIEW deploy"));
        assert!(text.contains("Loading terminal preview"));
        assert!(!text.contains("latest output"));
        assert!(!text.contains("Fleet"));
    }

    #[test]
    fn body_layout_auto_sizes_sidebar_to_visible_text() {
        let mut session = record("1", "ops", "build", false);
        session.agent = "codex".to_string();
        session.project_path = "/tmp/pr-42".to_string();
        let deck_statuses = BTreeMap::from([(
            "1".to_string(),
            TuiSessionStatus {
                deck_status: SessionDeckStatus::Occupied,
                activity: Some(SessionActivity {
                    state: "occupied".to_string(),
                    label: "editing".to_string(),
                    source: "agent".to_string(),
                    tool: None,
                }),
            },
        )]);
        let view = DashboardView::build_with_statuses(
            &[session],
            &deck_statuses,
            &BTreeSet::new(),
            "",
            0,
            StatusFilter::All,
        );
        let layout = body_layout_for_view(Rect::new(0, 4, 100, 20), &view, None, 0);
        let expected_width = display_width("`- ◆ build #42 codex editing") + SIDEBAR_LIST_PADDING;

        assert_eq!(layout.sidebar.width, expected_width);
        assert_eq!(layout.divider.x, layout.sidebar.width);
        assert_eq!(layout.divider.width, 1);
        assert_eq!(layout.detail.x, layout.sidebar.width + 1);
    }

    #[test]
    fn body_layout_auto_size_grows_and_clamps() {
        let sessions = vec![record(
            "1",
            "operations-with-a-long-visible-group",
            "deploy-with-a-long-visible-session-name",
            false,
        )];
        let view = DashboardView::build(&sessions, "", 0, StatusFilter::All);
        let layout = body_layout_for_view(Rect::new(0, 4, 100, 20), &view, None, 0);

        assert_eq!(layout.sidebar.width, 50);
        assert_eq!(layout.detail.x, 51);
    }

    #[test]
    fn group_header_text_omits_zero_status_counts() {
        let mut occupied = SessionSummary::from(&record("1", "ops", "deploy", false));
        occupied.deck_status = SessionDeckStatus::Occupied;
        let mut idle = SessionSummary::from(&record("2", "ops", "shell", false));
        idle.deck_status = SessionDeckStatus::Idle;
        let group = SessionGroup {
            name: "ops".to_string(),
            collapsed: false,
            sessions: vec![occupied, idle],
        };

        assert_eq!(group_header_text(0, &group), "1. ops (2) - ◆1 ·1");
    }

    #[test]
    fn initialize_sidebar_width_freezes_auto_width() {
        let size = ratatui::layout::Size {
            width: 100,
            height: 20,
        };
        let body = Rect::new(0, 4, 100, 15);
        let mut app = test_app(vec![record("1", "ops", "a", false)]);

        initialize_sidebar_width(&mut app, size);
        let initial = app.sidebar_width.unwrap();

        app.sessions.push(record(
            "2",
            "ops",
            "session-name-long-enough-to-grow-the-auto-sidebar",
            false,
        ));
        assert!(auto_sidebar_width(body, &app.view(), 0) > initial);

        initialize_sidebar_width(&mut app, size);
        assert_eq!(app.sidebar_width, Some(initial));
        assert_eq!(
            body_layout_for_view(body, &app.view(), app.sidebar_width, 0)
                .sidebar
                .width,
            initial
        );
    }

    #[test]
    fn mouse_drag_resizes_sidebar_from_divider() {
        let area = Rect::new(0, 0, 100, 20);
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let body =
            body_layout_for_view(Rect::new(0, 4, 100, 15), &app.view(), app.sidebar_width, 0);

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: body.divider.x,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );

        assert!(changed);
        assert!(app.resizing_sidebar);

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 40,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );

        assert!(changed);
        assert_eq!(app.sidebar_width, Some(40));
        let resized_body =
            body_layout_for_view(Rect::new(0, 4, 100, 15), &app.view(), app.sidebar_width, 0);
        assert_eq!(resized_body.sidebar.width, 40);
        assert_eq!(resized_body.divider.x, 40);
        assert_eq!(resized_body.detail.x, 41);
        let size = ratatui::layout::Size {
            width: area.width,
            height: area.height,
        };
        assert_eq!(
            terminal_preview_area(size, &app.view(), app.sidebar_width).x,
            resized_body.detail.x + 1
        );
        assert_eq!(
            embedded_terminal_area(size, &app.view(), app.sidebar_width).x,
            resized_body.detail.x + 1
        );

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 40,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );

        assert!(changed);
        assert!(!app.resizing_sidebar);
    }

    #[test]
    fn mouse_down_away_from_divider_does_not_resize_sidebar() {
        let area = Rect::new(0, 0, 100, 20);
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );

        assert!(!changed);
        assert!(!app.resizing_sidebar);
        assert_eq!(app.sidebar_width, None);
    }

    #[test]
    fn embedded_mouse_wheel_encodes_sgr_coordinates_relative_to_terminal() {
        let area = Rect::new(10, 4, 20, 8);

        let up = encode_embedded_mouse_wheel(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 12,
                row: 7,
                modifiers: KeyModifiers::NONE,
            },
            area,
        )
        .unwrap();
        assert_eq!(up, b"\x1b[<64;3;4M");

        let down = encode_embedded_mouse_wheel(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
        )
        .unwrap();
        assert_eq!(down, b"\x1b[<65;1;1M");

        assert!(
            encode_embedded_mouse_wheel(
                MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 9,
                    row: 7,
                    modifiers: KeyModifiers::NONE,
                },
                area,
            )
            .is_none()
        );
    }

    #[test]
    fn mouse_wheel_scrolls_running_session_preview() {
        let area = Rect::new(0, 0, 100, 20);
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let preview = terminal_preview_area(
            ratatui::layout::Size {
                width: area.width,
                height: area.height,
            },
            &app.view(),
            app.sidebar_width,
        );

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: preview.x,
                row: preview.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(changed);
        assert_eq!(app.preview_scroll, PREVIEW_SCROLL_LINES);

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: preview.x,
                row: preview.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(changed);
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn mouse_wheel_outside_running_preview_does_not_scroll_preview() {
        let area = Rect::new(0, 0, 100, 20);
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );

        assert!(!changed);
        assert_eq!(app.preview_scroll, 0);
    }

    #[test]
    fn dashboard_terminated_session_shows_state_instead_of_output() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut session = record("1", "ops", "deploy", false);
        session.status = SessionStatus::Stopped;
        let app = test_app(vec![session]);

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("stopped"));
        assert!(text.contains("session 1"));
        assert!(!text.contains("latest output"));
    }

    #[test]
    fn dashboard_shows_workspace_and_worktree_context() {
        let backend = ratatui::backend::TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut session = record("1", "ops", "deploy", false);
        session.status = SessionStatus::Stopped;
        session.worktree_id = Some("worktree-123456".to_string());
        session.project_path = "/repo/worktrees/deploy".to_string();
        let mut app = test_app(vec![session]);
        app.details.workspace = Some(TuiWorkspaceContext {
            workspace_id: "workspace-123456".to_string(),
            workspace_path: "/repo/worktrees/deploy".to_string(),
            worktree_id: Some("worktree-123456".to_string()),
            worktree_path: Some("/repo/worktrees/deploy".to_string()),
            worktree_branch: Some("agent-helm/deploy".to_string()),
        });

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("workspace workspace"));
        assert!(text.contains("worktree worktree"));
        assert!(text.contains("branch agent-helm/deploy"));
        assert!(!text.contains("latest output"));
    }

    #[test]
    fn render_shows_help_panel() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Help;

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Help"));
        assert!(text.contains("Navigation"));
        assert!(
            text.contains("Enter focuses embedded session viewport | Ctrl-q returns"),
            "{text}"
        );
        assert!(text.contains("Esc/q close"));
    }

    #[test]
    fn running_preview_does_not_render_scrolled_mirrored_output() {
        let backend = ratatui::backend::TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.detail_scroll = 10;
        app.details.output = (0..20)
            .map(|index| format!("line-{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Loading terminal preview"));
        assert!(!text.contains("line-00"));
        assert!(!text.contains("line-19"));
    }

    #[test]
    fn profile_tool_settings_key_opens_popup_from_dashboard() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut handle = |_| Ok(refreshed.clone());

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(matches!(app.mode, Mode::ToolSettings(_)));
    }

    #[test]
    fn tool_settings_form_saves_edited_launch_command() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::ToolSettings(ToolSettingsForm::new(&app.tool_settings, "codex"));
        if let Mode::ToolSettings(form) = &mut app.mode {
            form.selected_tool = form
                .tools
                .iter()
                .position(|tool| tool.name == "codex")
                .unwrap();
            form.load_selected_tool();
            form.field_index = 2;
            form.executable = "/opt/bin/codex-nightly".to_string();
            form.flags = "--profile review".to_string();
        }
        let refreshed = app.sessions.clone();
        let mut saved = None;
        let mut handle = |action| {
            if let TuiAction::SaveToolSettings(settings) = action {
                saved = Some(settings);
            }
            Ok(refreshed.clone())
        };

        handle_tool_settings_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &mut app,
            &mut handle,
        )
        .unwrap();

        let settings = saved.expect("settings saved");
        let codex = settings
            .iter()
            .find(|tool| tool.name == "codex")
            .expect("codex settings");
        assert_eq!(codex.executable, "/opt/bin/codex-nightly");
        assert_eq!(codex.flags, ["--profile", "review"]);
        assert!(matches!(app.mode, Mode::Normal));
    }

    fn test_app(sessions: Vec<SessionRecord>) -> App {
        App {
            sessions,
            collapsed_groups: BTreeSet::new(),
            group_default_paths: BTreeMap::new(),
            group_defaults: BTreeMap::new(),
            deck_statuses: BTreeMap::from([("1".to_string(), SessionDeckStatus::Waiting.into())]),
            pending_operations: BTreeMap::new(),
            next_pending_operation_id: 1,
            details: TuiDetails {
                deck_status: SessionDeckStatus::Waiting,
                activity: None,
                output: "latest output".to_string(),
                workspace: None,
            },
            details_session_id: Some("1".to_string()),
            query: String::new(),
            search_results_active: false,
            selected_index: 0,
            detail_scroll: 0,
            preview_scroll: 0,
            status_filter: StatusFilter::All,
            sidebar_width: None,
            resizing_sidebar: false,
            mouse_capture: true,
            animation_frame: 0,
            last_agent: "shell".to_string(),
            headroom_metrics: None,
            agent_choices: default_agent_choices(),
            tool_settings: normalize_tool_settings(Vec::new()),
            preview: None,
            preview_generation: 0,
            pending_preview: None,
            pending_details_refresh: None,
            embedded: None,
            mode: Mode::Normal,
            status_message: None,
        }
    }

    fn preview_result(request: PreviewRequest, bytes: &[u8]) -> PreviewResult {
        let parser = terminal_parser_from_bytes(bytes, request.rows, request.cols);
        let preview = TerminalPreview::from_capture_result(
            request.session_id.clone(),
            request.target.clone(),
            request.rows,
            request.cols,
            request.scroll,
            Ok(parser),
            Instant::now(),
        );
        PreviewResult {
            request,
            preview,
            capture_duration: Duration::from_millis(1),
        }
    }

    fn new_form_agent(app: &App) -> Option<&str> {
        match &app.mode {
            Mode::New(form) => Some(form.agent.as_str()),
            _ => None,
        }
    }

    fn group_record(name: &str, default_project_path: &str, collapsed: bool) -> GroupRecord {
        GroupRecord {
            id: format!("{name}-group"),
            profile: "default".to_string(),
            name: name.to_string(),
            default_project_path: default_project_path.to_string(),
            default_agent: None,
            default_worktree: None,
            default_carry_state: None,
            collapsed,
            display_order: 0,
            metadata: "{}".to_string(),
            version: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| {
                let mut line = String::new();
                for cell in row {
                    line.push_str(cell.symbol());
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn record(id: &str, group: &str, name: &str, archived: bool) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            name: name.to_string(),
            profile: "default".to_string(),
            group_name: group.to_string(),
            project_id: "project".to_string(),
            workspace_id: "workspace".to_string(),
            worktree_id: None,
            parent_session_id: None,
            agent: "shell".to_string(),
            command: "cat".to_string(),
            project_path: ".".to_string(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived,
            version: 0,
            created_at: 1,
            updated_at: 1,
        }
    }
}
