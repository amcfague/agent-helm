use crate::{
    config::{ToolProfile, ToolWorktreeBehavior},
    error::{AppError, Result},
    models::{
        CreateSession, DeleteMode, ForkSessionRequest, GroupRecord, SessionActivity,
        SessionDeckStatus, SessionRecord, SessionStatus,
    },
};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
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
const PREVIEW_SCROLL_LINES: u16 = 3;
const PREVIEW_MAX_SCROLL_LINES: u16 = 5000;
const EMBED_READ_BUF_SIZE: usize = 8192;
const EMBED_MIN_ROWS: u16 = 2;
const EMBED_MIN_COLS: u16 = 10;
const DEFAULT_SIDEBAR_PERCENT: u16 = 24;
const MIN_SIDEBAR_PERCENT: u16 = 18;
const MAX_SIDEBAR_PERCENT: u16 = 50;
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
    pub hidden_archived: usize,
}

impl DashboardView {
    pub fn build(
        sessions: &[SessionRecord],
        query: &str,
        selected_index: usize,
        show_archived: bool,
        status_filter: StatusFilter,
    ) -> Self {
        let collapsed_groups = BTreeSet::new();
        Self::build_with_statuses(
            sessions,
            &BTreeMap::new(),
            &collapsed_groups,
            query,
            selected_index,
            show_archived,
            status_filter,
        )
    }

    pub fn build_with_collapsed(
        sessions: &[SessionRecord],
        collapsed_groups: &BTreeSet<String>,
        query: &str,
        selected_index: usize,
        show_archived: bool,
        status_filter: StatusFilter,
    ) -> Self {
        Self::build_with_statuses(
            sessions,
            &BTreeMap::new(),
            collapsed_groups,
            query,
            selected_index,
            show_archived,
            status_filter,
        )
    }

    fn build_with_statuses(
        sessions: &[SessionRecord],
        deck_statuses: &BTreeMap<String, TuiSessionStatus>,
        collapsed_groups: &BTreeSet<String>,
        query: &str,
        selected_index: usize,
        show_archived: bool,
        status_filter: StatusFilter,
    ) -> Self {
        let query = query.trim().to_ascii_lowercase();
        let hidden_archived = if show_archived {
            0
        } else {
            sessions.iter().filter(|session| session.archived).count()
        };
        let mut rows = sessions
            .iter()
            .filter(|session| show_archived || !session.archived)
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
            hidden_archived,
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
    pub archived: bool,
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
            archived: session.archived,
            updated_at: session.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TuiAction {
    Create(CreateSession),
    Attach(String),
    Stop(String),
    Restart(String),
    Fork(ForkSessionRequest),
    Archive(String),
    Restore(String),
    SyncState(String),
    Search {
        query: String,
        limit: usize,
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
    Send {
        session_id: String,
        text: String,
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
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    D: FnMut(&str) -> Result<TuiDetails>,
    S: FnMut(&[String]) -> Result<Vec<(String, TuiSessionStatus)>>,
{
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut app = app_from_initial(initial);

    let result = run_app(
        &mut terminal,
        &mut app,
        &mut load_deck_statuses,
        &mut load_details,
        &mut handle_action,
    );
    terminal.clear()?;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

struct App {
    sessions: Vec<SessionRecord>,
    collapsed_groups: BTreeSet<String>,
    deck_statuses: BTreeMap<String, TuiSessionStatus>,
    details: TuiDetails,
    details_session_id: Option<String>,
    query: String,
    search_results_active: bool,
    selected_index: usize,
    detail_scroll: u16,
    preview_scroll: u16,
    show_archived: bool,
    status_filter: StatusFilter,
    sidebar_percent: u16,
    resizing_sidebar: bool,
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
    App {
        sessions: initial.sessions,
        collapsed_groups: initial
            .groups
            .into_iter()
            .filter(|group| group.collapsed)
            .map(|group| group.name)
            .collect(),
        deck_statuses: BTreeMap::new(),
        details: TuiDetails::default(),
        details_session_id: None,
        query: String::new(),
        search_results_active: false,
        selected_index: 0,
        detail_scroll: 0,
        preview_scroll: 0,
        show_archived: false,
        status_filter: StatusFilter::All,
        sidebar_percent: DEFAULT_SIDEBAR_PERCENT,
        resizing_sidebar: false,
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
            self.show_archived,
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
            self.writer
                .write_all(&bytes)
                .map_err(|err| AppError::msg(format!("write embedded terminal key: {err}")))?;
            self.writer
                .flush()
                .map_err(|err| AppError::msg(format!("flush embedded terminal key: {err}")))?;
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
            Self::All => Self::Running,
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
    Fork(ForkForm),
    Send(SendForm),
    Session(SendForm),
    Move(MoveForm),
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
            NewField::Group => Some(&mut self.group_name),
            NewField::Agent | NewField::Worktree | NewField::CarryState => None,
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
    form.worktree = tool_creates_worktree_by_default(&app.tool_settings, &form.agent);
    form
}

fn new_form_for_session(app: &App, session: &SessionSummary) -> NewForm {
    let mut form = NewForm::for_session(session);
    form.worktree = tool_creates_worktree_by_default(&app.tool_settings, &form.agent);
    form
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

fn project_name(path: &str) -> String {
    crate::util::project_name(path)
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
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    D: FnMut(&str) -> Result<TuiDetails>,
    S: FnMut(&[String]) -> Result<Vec<(String, TuiSessionStatus)>>,
{
    refresh_deck_statuses(app, load_deck_statuses)?;
    refresh_details(app, load_details, true)?;
    let preview_worker = PreviewWorker::spawn();
    let mut tui_loop = TuiLoop::new(Instant::now());
    loop {
        let now = Instant::now();
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
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(());
                }

                let previous_selected_id = selected_id(app);
                if process_key(terminal, app, handle_action, key)? {
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

    let Some(session) = app.view().selected else {
        app.embedded = None;
        app.mode = Mode::Normal;
        app.status_message = Some("no session selected".to_string());
        return true;
    };
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
    let area = embedded_terminal_area(size, app.sidebar_percent);
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
    if !session_is_live(session.status) {
        return clear_terminal_preview(app) || changed;
    }

    let Ok(size) = terminal.size() else {
        return changed;
    };
    let area = terminal_preview_area(size, app.sidebar_percent);
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

fn embedded_terminal_area(size: ratatui::layout::Size, sidebar_percent: u16) -> Rect {
    let area = Rect::new(0, 0, size.width, size.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let body = body_layout(chunks[1], sidebar_percent);
    panel_block("SESSION").inner(body.detail)
}

fn terminal_preview_area(size: ratatui::layout::Size, sidebar_percent: u16) -> Rect {
    let area = Rect::new(0, 0, size.width, size.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let body = body_layout(chunks[1], sidebar_percent);
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

fn process_key<F>(
    _terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    handle_action: &mut F,
    key: KeyEvent,
) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    match app.mode {
        Mode::Normal if key.code == KeyCode::Enter => {
            focus_selected_session(app);
        }
        Mode::Normal => {
            if handle_normal_key(key, app, handle_action)? {
                return Ok(true);
            }
        }
        Mode::Search => handle_search_key(key, app, handle_action)?,
        Mode::New(_) => handle_new_key(key, app, handle_action)?,
        Mode::Fork(_) => handle_fork_key(key, app, handle_action)?,
        Mode::Send(_) => handle_send_key(key, app, handle_action)?,
        Mode::Session(_) => handle_session_key(key, app)?,
        Mode::Move(_) => handle_move_key(key, app, handle_action)?,
        Mode::ToolSettings(_) => handle_tool_settings_key(key, app, handle_action)?,
        Mode::Help => handle_help_key(key, app),
    }
    Ok(false)
}

fn process_mouse(app: &mut App, mouse: MouseEvent, area: Rect) -> bool {
    match mouse.kind {
        MouseEventKind::ScrollUp if mouse_on_session_preview(mouse, area, app) => {
            scroll_session_preview(app, PREVIEW_SCROLL_LINES as i16)
        }
        MouseEventKind::ScrollDown if mouse_on_session_preview(mouse, area, app) => {
            scroll_session_preview(app, -(PREVIEW_SCROLL_LINES as i16))
        }
        MouseEventKind::Down(MouseButton::Left) if mouse_on_divider(mouse, area, app) => {
            app.resizing_sidebar = true;
            set_sidebar_percent_from_mouse(app, mouse.column, area)
        }
        MouseEventKind::Drag(MouseButton::Left) if app.resizing_sidebar => {
            set_sidebar_percent_from_mouse(app, mouse.column, area)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let was_resizing = app.resizing_sidebar;
            app.resizing_sidebar = false;
            was_resizing
        }
        _ => false,
    }
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
        .filter(|session| app.show_archived || !session.archived)
        .filter(|session| query.is_empty() || matches_query(session, &query))
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    if session_ids.is_empty() {
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

fn handle_normal_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('?') | KeyCode::Char('h') => app.mode = Mode::Help,
        KeyCode::Char('/') => app.mode = Mode::Search,
        KeyCode::Char('a') => app.show_archived = !app.show_archived,
        KeyCode::Char('c') => toggle_selected_group(app, handle_action)?,
        KeyCode::Char('e') => expand_groups(app, handle_action)?,
        KeyCode::Char('g') => open_tool_settings(app),
        KeyCode::Char('t') => {
            app.status_filter = app.status_filter.next();
            app.selected_index = 0;
            reset_detail_view(app);
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            create_shell_session(app, handle_action)?;
        }
        KeyCode::Char('N') => {
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
        KeyCode::Char('s') => {
            if selected_id(app).is_some() {
                app.mode = Mode::Send(SendForm::default());
                app.status_message = None;
            }
        }
        KeyCode::Char('m') => {
            if let Some(session) = app.view().selected {
                app.mode = Mode::Move(MoveForm::for_session(&session));
                app.status_message = None;
            } else {
                app.status_message = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('x') => run_selected_action(app, handle_action, TuiAction::Stop, "stopped")?,
        KeyCode::Char('r') => {
            run_selected_action(app, handle_action, TuiAction::Restart, "restarted")?
        }
        KeyCode::Char('f') => {
            if let Some(session) = app.view().selected {
                app.mode = Mode::Fork(ForkForm::for_session(&session));
                app.status_message = None;
            }
        }
        KeyCode::Char('d') => {
            run_selected_action(app, handle_action, TuiAction::Archive, "archived")?
        }
        KeyCode::Char('u') => {
            run_selected_action(app, handle_action, TuiAction::Restore, "restored")?
        }
        KeyCode::Char('S') => run_selected_action_refreshing_details(
            app,
            handle_action,
            TuiAction::SyncState,
            "synced state",
        )?,
        KeyCode::Delete if key.modifiers.contains(KeyModifiers::SHIFT) => run_selected_action(
            app,
            handle_action,
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::CleanupWorktree,
            },
            "removed",
        )?,
        KeyCode::Delete => run_selected_action(
            app,
            handle_action,
            |session_id| TuiAction::Remove {
                session_id,
                mode: DeleteMode::MetadataOnly,
            },
            "removed",
        )?,
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

fn reset_detail_view(app: &mut App) {
    app.detail_scroll = 0;
    app.preview_scroll = 0;
}

fn open_tool_settings(app: &mut App) {
    app.mode = Mode::ToolSettings(ToolSettingsForm::new(&app.tool_settings, &app.last_agent));
    app.status_message = None;
}

fn create_shell_session<F>(app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
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
    match handle_action(TuiAction::Create(request)) {
        Ok(sessions) => {
            app.sessions = sessions;
            app.query.clear();
            app.search_results_active = false;
            app.last_agent = "shell".to_string();
            select_matching_session(app, |session| {
                session.name == name && session.group_name == group_name
            });
            app.mode = Mode::Normal;
            app.status_message = Some("created shell session".to_string());
        }
        Err(err) => app.status_message = Some(format!("create shell failed: {err}")),
    }
    Ok(())
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

fn handle_new_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    let agent_choices = app.agent_choices.clone();
    let tool_settings = app.tool_settings.clone();

    if let Mode::New(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Left if form.current_field() == NewField::Agent => {
                form.cycle_agent(&agent_choices, -1);
                form.worktree = tool_creates_worktree_by_default(&tool_settings, &form.agent);
            }
            KeyCode::Right if form.current_field() == NewField::Agent => {
                form.cycle_agent(&agent_choices, 1);
                form.worktree = tool_creates_worktree_by_default(&tool_settings, &form.agent);
            }
            KeyCode::Enter => {
                if form.field_index == NEW_FIELDS.len() - 1 {
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
                    app.status_message = Some(message.to_string());
                    return Ok(());
                }
            },
            _ => return Ok(()),
        };

        let created_name = if request.name.trim().is_empty() {
            project_name(&request.path)
        } else {
            request.name.clone()
        };
        let created_group = request.group_name.clone();
        let created_agent = request.agent.clone();
        match handle_action(TuiAction::Create(request)) {
            Ok(sessions) => {
                app.sessions = sessions;
                app.query.clear();
                app.search_results_active = false;
                app.last_agent = created_agent;
                select_matching_session(app, |session| {
                    session.name == created_name && session.group_name == created_group
                });
                app.mode = Mode::Normal;
                app.status_message = Some("created session".to_string());
            }
            Err(err) => {
                app.status_message = Some(format!("create failed: {err}"));
            }
        }
    }

    Ok(())
}

fn handle_fork_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
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
        let expected_name = request.name.clone();
        let expected_group = request.group_name.clone();
        match handle_action(TuiAction::Fork(request)) {
            Ok(sessions) => {
                app.sessions = sessions;
                if let Some(name) = expected_name {
                    select_matching_session(app, |session| {
                        session.name == name
                            && expected_group
                                .as_deref()
                                .is_none_or(|group| session.group_name == group)
                    });
                }
                app.mode = Mode::Normal;
                app.status_message = Some("forked session".to_string());
            }
            Err(err) => {
                app.status_message = Some(format!("fork failed: {err}"));
            }
        }
    }

    Ok(())
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

fn handle_send_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;
    if let Mode::Send(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Enter => submit = true,
            KeyCode::Backspace => {
                form.text.pop();
            }
            KeyCode::Char(ch) => form.text.push(ch),
            _ => {}
        }
    }
    if submit {
        let text = match &app.mode {
            Mode::Send(form) => form.text.trim().to_string(),
            _ => String::new(),
        };
        if text.is_empty() {
            app.status_message = Some("send text required".to_string());
            return Ok(());
        }
        let Some(session_id) = selected_id(app) else {
            app.status_message = Some("no session selected".to_string());
            return Ok(());
        };
        let selected_session_id = session_id.clone();
        match handle_action(TuiAction::Send { session_id, text }) {
            Ok(sessions) => {
                app.sessions = sessions;
                invalidate_session_cache(app, &selected_session_id);
                app.mode = Mode::Normal;
                app.status_message = Some("sent input".to_string());
            }
            Err(err) => app.status_message = Some(format!("send failed: {err}")),
        }
    }
    Ok(())
}

fn handle_session_key(key: KeyEvent, app: &mut App) -> Result<()> {
    let is_ctrl_q = key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_q {
        app.embedded = None;
        app.mode = Mode::Normal;
        app.status_message = Some("returned to dashboard".to_string());
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
    match handle_action(build(session_id)) {
        Ok(sessions) => {
            app.sessions = sessions;
            invalidate_session_cache(app, &selected_session_id);
            if app.search_results_active && !app.query.trim().is_empty() {
                refresh_sessions(app, handle_action)?;
            }
            select_matching_session(app, |session| session.id == selected_session_id);
            app.detail_scroll = 0;
            app.status_message = Some(done.to_string());
        }
        Err(err) => app.status_message = Some(format!("{done} failed: {err}")),
    }
    Ok(())
}

fn run_selected_action_refreshing_details<F, M>(
    app: &mut App,
    handle_action: &mut F,
    build: M,
    done: &str,
) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    M: FnOnce(String) -> TuiAction,
{
    run_selected_action(app, handle_action, build, done)?;
    if app.status_message.as_deref() == Some(done) {
        app.details_session_id = None;
    }
    Ok(())
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

fn body_layout(area: Rect, sidebar_percent: u16) -> BodyLayout {
    let sidebar_percent = sidebar_percent.clamp(MIN_SIDEBAR_PERCENT, MAX_SIDEBAR_PERCENT);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(sidebar_percent),
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

fn mouse_on_divider(mouse: MouseEvent, area: Rect, app: &App) -> bool {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    let body = body_layout(chunks[1], app.sidebar_percent);

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
    let Some(session) = app.view().selected else {
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
        app.sidebar_percent,
    );
    point_in_rect(mouse.column, mouse.row, preview)
}

fn point_in_rect(column: u16, row: u16, area: Rect) -> bool {
    row >= area.y
        && row < area.y.saturating_add(area.height)
        && column >= area.x
        && column < area.x.saturating_add(area.width)
}

fn set_sidebar_percent_from_mouse(app: &mut App, column: u16, area: Rect) -> bool {
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
    let percent =
        (relative.saturating_mul(100) / body.width).clamp(MIN_SIDEBAR_PERCENT, MAX_SIDEBAR_PERCENT);
    let changed = app.sidebar_percent != percent;
    app.sidebar_percent = percent;
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
    let body = body_layout(chunks[1], app.sidebar_percent);

    frame.render_widget(header(app, &view), chunks[0]);
    let (sessions, mut session_state) = session_list(&view, app.animation_frame);
    frame.render_stateful_widget(sessions, body.sidebar, &mut session_state);
    frame.render_widget(divider(app.resizing_sidebar), body.divider);
    match &app.mode {
        Mode::New(form) => frame.render_widget(
            create_form(form, &app.agent_choices, &app.status_message),
            body.detail,
        ),
        Mode::Fork(form) => frame.render_widget(fork_form(form, &app.status_message), body.detail),
        Mode::Send(form) => frame.render_widget(send_form(form, &app.status_message), body.detail),
        Mode::Session(_) => render_session_terminal(frame, app, &view, body.detail),
        Mode::Move(form) => frame.render_widget(move_form(form, &app.status_message), body.detail),
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
    let archived = if app.show_archived { "all" } else { "active" };
    let search = if app.query.trim().is_empty() {
        "all".to_string()
    } else {
        format!("/{}", app.query)
    };
    let message = app.status_message.as_deref().unwrap_or("");
    let mut filters = vec![
        Span::styled(app.status_filter.label(), badge_style()),
        Span::raw(format!(
            "  {} queued  {} waiting  {} idle",
            counts.queued, counts.waiting, counts.idle
        )),
        Span::raw(format!("  {} visible", view.visible_count)),
        Span::raw(format!("  {} hidden", view.hidden_archived)),
        Span::raw(format!("  filter {search}")),
        Span::raw(format!("  archive {archived}")),
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
                "  * {} running  > {} starting  o {} stopped  ! {} errored",
                counts.running, counts.starting, counts.stopped, counts.errored
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
        let counts = group_status_counts(group);
        let collapse_mark = if group.collapsed { "+" } else { "-" };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "{}. {} ({}) {} *{} >{} :{} ?{} -{} o{} !{}",
                group_index + 1,
                group.name,
                group.sessions.len(),
                collapse_mark,
                counts.running,
                counts.starting,
                counts.queued,
                counts.waiting,
                counts.idle,
                counts.stopped,
                counts.errored
            ),
            group_style(),
        ))));

        if group.collapsed {
            continue;
        }

        for (session_index, session) in group.sessions.iter().enumerate() {
            let selected = selected_id == Some(session.id.as_str());
            let branch = if session_index + 1 == group.sessions.len() {
                "`-"
            } else {
                "|-"
            };
            let archived = if session.archived { " archived" } else { "" };
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
                Span::raw(archived),
                Span::raw(" "),
                Span::styled(session.agent.clone(), agent_style(&session.agent)),
                Span::raw(" "),
                Span::styled(
                    session.deck_status.as_str(),
                    deck_status_style(session.deck_status),
                ),
            ];
            if let Some(label) = session_activity_label(session) {
                row.push(Span::raw(" "));
                row.push(Span::styled(
                    label.to_string(),
                    activity_label_style(session),
                ));
            }
            items.push(ListItem::new(Line::from(row)).style(if selected {
                selected_row_style()
            } else {
                Style::default()
            }));
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
    let block = panel_block_owned(title);
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
        Line::from(""),
        section_line("Actions"),
        Line::from("n new session | Ctrl-n shell | N duplicate | s send input | m move group"),
        Line::from("S sync state | r restart | x stop | f fork | d archive | u restore"),
        Line::from("Del remove | Shift+Del cleanup"),
        Line::from("g opens profile tool settings"),
        Line::from(""),
        section_line("Filters"),
        Line::from("/ search | a toggle archived | t status filter | PageUp/PageDown scroll"),
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
    let archived = if app.show_archived { "shown" } else { "hidden" };
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
            "* {} running   > {} starting   : {} queued   ? {} waiting   - {} idle   o {} stopped   ! {} errored",
            counts.running,
            counts.starting,
            counts.queued,
            counts.waiting,
            counts.idle,
            counts.stopped,
            counts.errored
        )),
        Line::from(format!(
            "archived {archived}; {} hidden",
            view.hidden_archived
        )),
        Line::from(""),
        section_line("Groups"),
    ];

    for group in &view.groups {
        let counts = group_status_counts(group);
        lines.push(Line::from(format!(
            "{}  {} sessions  *{} >{} :{} ?{} -{} o{} !{}",
            group.name,
            group.sessions.len(),
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
            Span::raw(if session.archived {
                "archived"
            } else {
                "active"
            }),
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
    match session.deck_status {
        SessionDeckStatus::Running | SessionDeckStatus::Starting => {
            const FRAMES: [&str; 4] = [".", "o", "O", "o"];
            FRAMES[animation_frame % FRAMES.len()]
        }
        SessionDeckStatus::Queued => ":",
        SessionDeckStatus::Waiting => "?",
        SessionDeckStatus::Idle => "-",
        SessionDeckStatus::Stopped => "o",
        SessionDeckStatus::Errored => "!",
    }
}

fn session_activity_style(session: &SessionSummary) -> Style {
    match session.deck_status {
        SessionDeckStatus::Running | SessionDeckStatus::Starting => agent_style(&session.agent),
        status => deck_status_style(status),
    }
}

fn session_activity_label(session: &SessionSummary) -> Option<&str> {
    let label = session.activity.as_ref()?.label.trim();
    if label.is_empty() || label.eq_ignore_ascii_case(session.deck_status.as_str()) {
        None
    } else {
        Some(label)
    }
}

fn activity_label_style(session: &SessionSummary) -> Style {
    session_activity_style(session)
}

fn deck_status_label(status: SessionDeckStatus) -> &'static str {
    match status {
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

fn create_form(
    form: &NewForm,
    agent_choices: &[String],
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
            NewField::Name if form.name.trim().is_empty() => {
                spans.push(Span::styled(form.default_name(), muted_style()));
            }
            NewField::Agent => spans.extend(agent_selector_spans(form, agent_choices)),
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

fn send_form(form: &SendForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled(">", title_style()),
        Span::raw(" Text     "),
        Span::raw(form.text.clone()),
    ])];
    if let Some(message) = status_message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    Paragraph::new(lines)
        .block(panel_block("SEND INPUT"))
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

fn footer(app: &App) -> Paragraph<'static> {
    let text = footer_text(app);
    Paragraph::new(Line::from(vec![Span::styled(text, muted_style())]))
}

fn footer_text(app: &App) -> String {
    match &app.mode {
        Mode::Search => "Search: type query | Enter/Esc done".to_string(),
        Mode::New(_) => {
            "New: Tab field | Enter next/create | Ctrl-S create | Esc cancel".to_string()
        }
        Mode::Fork(_) => "Fork: Tab field | Enter next/fork | Ctrl-S fork | Esc cancel".to_string(),
        Mode::Send(_) => "Send: type input | Enter send | Esc cancel".to_string(),
        Mode::Session(_) => "Session: interactive tmux viewport | Ctrl-q dashboard".to_string(),
        Mode::Move(_) => "Move: type group | Enter move | Esc cancel".to_string(),
        Mode::ToolSettings(_) => {
            "Profile tools: Tab field | Left/Right cycle | Space toggle | Ctrl-S save | Esc cancel".to_string()
        }
        Mode::Help => "Help: Esc/q close".to_string(),
        Mode::Normal => {
                "Enter session s send S sync m move r restart f fork x stop d/u archive/restore Del remove Shift+Del cleanup | n new Ctrl-n shell N duplicate | g tools | j/k nav c/e groups Pg scroll / search a archived t status ? help q quit".to_string()
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
        let view = DashboardView::build(&sessions, "", 0, false, StatusFilter::All);

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
            false,
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
            false,
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

        let view = DashboardView::build(&sessions, "test", 0, false, StatusFilter::All);

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
    fn archived_is_hidden_by_default() {
        let sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "old", true),
        ];

        let hidden = DashboardView::build(&sessions, "", 0, false, StatusFilter::All);
        let shown = DashboardView::build(&sessions, "", 0, true, StatusFilter::All);

        assert_eq!(hidden.visible_count, 1);
        assert_eq!(hidden.hidden_archived, 1);
        assert_eq!(shown.visible_count, 2);
        assert_eq!(shown.hidden_archived, 0);
    }

    #[test]
    fn status_filter_limits_visible_sessions() {
        let running = record("1", "ops", "deploy", false);
        let mut stopped = record("2", "ops", "old", false);
        stopped.status = SessionStatus::Stopped;

        let view = DashboardView::build(&[running, stopped], "", 0, false, StatusFilter::Stopped);

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
            false,
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
        let statuses = BTreeMap::from([("1".to_string(), SessionDeckStatus::Waiting.into())]);
        let collapsed_groups = BTreeSet::new();
        let view = DashboardView::build_with_statuses(
            &[running],
            &statuses,
            &collapsed_groups,
            "",
            0,
            false,
            StatusFilter::All,
        );

        let counts = group_status_counts(&view.groups[0]);
        assert_eq!(counts.running, 0);
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
            false,
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
            false,
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
            deck_statuses: BTreeMap::new(),
            details: TuiDetails::default(),
            details_session_id: None,
            query: String::new(),
            search_results_active: false,
            selected_index: 0,
            detail_scroll: 0,
            preview_scroll: 0,
            show_archived: false,
            status_filter: StatusFilter::All,
            sidebar_percent: DEFAULT_SIDEBAR_PERCENT,
            resizing_sidebar: false,
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
            "Enter session s send S sync m move r restart f fork x stop d/u archive/restore Del remove Shift+Del cleanup | n new Ctrl-n shell N duplicate | g tools | j/k nav c/e groups Pg scroll / search a archived t status ? help q quit"
        );
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
        app.sidebar_percent = 32;

        focus_selected_session(&mut app);

        assert_eq!(app.sidebar_percent, 32);
        assert_eq!(app.detail_scroll, 0);
        assert!(matches!(app.mode, Mode::Session(_)));
        assert_eq!(app.status_message, None);
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
        assert_eq!(app.status_filter, StatusFilter::Running);
        assert_eq!(app.selected_index, 0);
        assert_eq!(app.detail_scroll, 0);

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
    fn new_form_prefills_selected_session_context() {
        let mut session = record("1", "work/api", "deploy", false);
        session.project_path = "/tmp/project".to_string();
        let mut app = test_app(vec![session]);
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
    fn move_form_submits_group_change_and_preserves_selection() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
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
    fn sync_state_key_runs_action_and_refreshes_details() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut synced = None;
        let mut handle = |action| {
            if let TuiAction::SyncState(id) = action {
                synced = Some(id);
            }
            Ok(refreshed.clone())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(synced.as_deref(), Some("1"));
        assert_eq!(app.details_session_id, None);
        assert_eq!(app.status_message.as_deref(), Some("synced state"));
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
        let mut restarted = None;
        let mut handle = |action| {
            if let TuiAction::Restart(id) = action {
                restarted = Some(id);
            }
            Ok(refreshed.clone())
        };

        run_selected_action(&mut app, &mut handle, TuiAction::Restart, "restarted").unwrap();

        assert_eq!(restarted.as_deref(), Some("2"));
        assert_eq!(app.view().selected.unwrap().id, "2");
        assert_eq!(app.detail_scroll, 0);
        assert_eq!(app.status_message.as_deref(), Some("restarted"));
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
    fn normal_keys_run_selected_actions() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut actions = Vec::new();
        let mut handle = |action| {
            match action {
                TuiAction::Stop(id) => actions.push(format!("stop:{id}")),
                TuiAction::Restart(id) => actions.push(format!("restart:{id}")),
                TuiAction::Archive(id) => actions.push(format!("archive:{id}")),
                TuiAction::Restore(id) => actions.push(format!("restore:{id}")),
                TuiAction::Remove { session_id, mode } => {
                    actions.push(format!("remove:{session_id}:{}", mode.as_str()))
                }
                _ => actions.push("other".to_string()),
            }
            Ok(refreshed.clone())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_normal_key(
            KeyEvent::new(KeyCode::Delete, KeyModifiers::SHIFT),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(
            actions,
            vec![
                "stop:1",
                "restart:1",
                "archive:1",
                "restore:1",
                "remove:1:metadata_only",
                "remove:1:cleanup_worktree"
            ]
        );
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
    fn send_mode_submits_selected_input() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        let refreshed = app.sessions.clone();
        let mut sent = None;
        let mut handle = |action| {
            if let TuiAction::Send { session_id, text } = action {
                sent = Some((session_id, text));
            }
            Ok(refreshed.clone())
        };

        handle_normal_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_send_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_send_key(
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();
        handle_send_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(sent, Some(("1".to_string(), "hi".to_string())));
        assert_eq!(app.status_message.as_deref(), Some("sent input"));
    }

    #[test]
    fn send_mode_invalidates_details_cache() {
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.mode = Mode::Send(SendForm {
            text: "hello".to_string(),
        });
        let refreshed = app.sessions.clone();
        let mut handle = |action| {
            assert!(matches!(
                action,
                TuiAction::Send {
                    ref session_id,
                    ref text
                } if session_id == "1" && text == "hello"
            ));
            Ok(refreshed.clone())
        };

        handle_send_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert!(!app.deck_statuses.contains_key("1"));
        assert_eq!(app.details_session_id, None);
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
            app.sidebar_percent,
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

        assert!(text.contains("running using Bash"));
    }

    #[test]
    fn session_activity_marker_animates_running_sessions() {
        let running = SessionSummary::from(&record("1", "ops", "deploy", false));
        let mut stopped_record = record("2", "ops", "done", false);
        stopped_record.status = SessionStatus::Stopped;
        let stopped = SessionSummary::from(&stopped_record);
        let mut waiting = running.clone();
        waiting.deck_status = SessionDeckStatus::Waiting;
        let mut queued = running.clone();
        queued.deck_status = SessionDeckStatus::Queued;
        let mut idle = running.clone();
        idle.deck_status = SessionDeckStatus::Idle;

        assert_ne!(
            session_activity_marker(&running, 0),
            session_activity_marker(&running, 2)
        );
        assert_eq!(session_activity_marker(&queued, 0), ":");
        assert_eq!(session_activity_marker(&waiting, 0), "?");
        assert_eq!(session_activity_marker(&idle, 0), "-");
        assert_eq!(session_activity_marker(&stopped, 0), "o");
        assert_eq!(session_activity_marker(&stopped, 2), "o");
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
    fn body_layout_uses_smaller_default_sidebar_and_divider() {
        assert_eq!(DEFAULT_SIDEBAR_PERCENT, 24);

        let layout = body_layout(Rect::new(0, 4, 100, 20), DEFAULT_SIDEBAR_PERCENT);

        assert_eq!(layout.sidebar.width, 24);
        assert_eq!(layout.divider.x, 24);
        assert_eq!(layout.divider.width, 1);
        assert_eq!(layout.detail.x, 25);
    }

    #[test]
    fn mouse_drag_resizes_sidebar_from_divider() {
        let area = Rect::new(0, 0, 100, 20);
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);

        let changed = process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 24,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );

        assert!(!changed);
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
        assert_eq!(app.sidebar_percent, 40);

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
        assert_eq!(app.sidebar_percent, DEFAULT_SIDEBAR_PERCENT);
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
            app.sidebar_percent,
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
            deck_statuses: BTreeMap::from([("1".to_string(), SessionDeckStatus::Waiting.into())]),
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
            show_archived: false,
            status_filter: StatusFilter::All,
            sidebar_percent: DEFAULT_SIDEBAR_PERCENT,
            resizing_sidebar: false,
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
