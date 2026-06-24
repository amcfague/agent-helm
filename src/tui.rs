use crate::{
    error::Result,
    models::{CreateSession, SessionRecord, SessionStatus},
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::{cmp::Ordering, io, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardView {
    pub groups: Vec<SessionGroup>,
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
    ) -> Self {
        let query = query.trim().to_ascii_lowercase();
        let hidden_archived = sessions.iter().filter(|session| session.archived).count();
        let mut rows = sessions
            .iter()
            .filter(|session| show_archived || !session.archived)
            .filter(|session| query.is_empty() || matches_query(session, &query))
            .map(SessionSummary::from)
            .collect::<Vec<_>>();

        rows.sort_by(compare_sessions);

        let selected_index = match rows.len() {
            0 => 0,
            len => selected_index.min(len - 1),
        };
        let selected = rows.get(selected_index).cloned();
        let visible_count = rows.len();
        let mut groups: Vec<SessionGroup> = Vec::new();

        for row in rows {
            if groups.last().map(|group| group.name.as_str()) != Some(row.group_name.as_str()) {
                groups.push(SessionGroup {
                    name: row.group_name.clone(),
                    sessions: Vec::new(),
                });
            }
            groups
                .last_mut()
                .expect("group was just inserted")
                .sessions
                .push(row);
        }

        Self {
            groups,
            selected,
            selected_index,
            visible_count,
            hidden_archived,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGroup {
    pub name: String,
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
    pub status: SessionStatus,
    pub archived: bool,
    updated_at: i64,
}

impl From<&SessionRecord> for SessionSummary {
    fn from(session: &SessionRecord) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
            group_name: session.group_name.clone(),
            agent: session.agent.clone(),
            command: session.command.clone(),
            project_path: session.project_path.clone(),
            status: session.status,
            archived: session.archived,
            updated_at: session.updated_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Dashboard,
    Preview,
    Diff,
    Structured,
    Settings,
}

pub const VIEW_MODES: [ViewMode; 5] = [
    ViewMode::Dashboard,
    ViewMode::Preview,
    ViewMode::Diff,
    ViewMode::Structured,
    ViewMode::Settings,
];

impl ViewMode {
    pub fn from_key(key: char) -> Option<Self> {
        VIEW_MODES.iter().copied().find(|mode| mode.key() == key)
    }

    pub fn key(self) -> char {
        match self {
            Self::Dashboard => '1',
            Self::Preview => '2',
            Self::Diff => '3',
            Self::Structured => '4',
            Self::Settings => '5',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Preview => "preview",
            Self::Diff => "diff",
            Self::Structured => "structured",
            Self::Settings => "settings",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Dashboard => "dash",
            Self::Preview => "prev",
            Self::Diff => "diff",
            Self::Structured => "struct",
            Self::Settings => "set",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Preview => "Preview",
            Self::Diff => "Diff",
            Self::Structured => "Structured",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone)]
pub enum TuiAction {
    Create(CreateSession),
    Stop(String),
    Restart(String),
    Fork(String),
    Send { session_id: String, text: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiDetails {
    pub output: String,
    pub diff: String,
    pub structured: String,
}

pub fn run_tui<F, D>(
    sessions: Vec<SessionRecord>,
    mut load_details: D,
    mut handle_action: F,
) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    D: FnMut(&str) -> Result<TuiDetails>,
{
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App {
        sessions,
        details: TuiDetails::default(),
        details_session_id: None,
        query: String::new(),
        selected_index: 0,
        show_archived: false,
        view_mode: ViewMode::Dashboard,
        mode: Mode::Normal,
        status_message: None,
    };

    let result = run_app(
        &mut terminal,
        &mut app,
        &mut load_details,
        &mut handle_action,
    );
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

#[derive(Debug, Clone)]
struct App {
    sessions: Vec<SessionRecord>,
    details: TuiDetails,
    details_session_id: Option<String>,
    query: String,
    selected_index: usize,
    show_archived: bool,
    view_mode: ViewMode,
    mode: Mode,
    status_message: Option<String>,
}

impl App {
    fn view(&self) -> DashboardView {
        DashboardView::build(
            &self.sessions,
            &self.query,
            self.selected_index,
            self.show_archived,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    New(NewForm),
    Send(SendForm),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SendForm {
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewField {
    Name,
    Path,
    Command,
    Group,
    Prompt,
}

const NEW_FIELDS: [NewField; 5] = [
    NewField::Name,
    NewField::Path,
    NewField::Command,
    NewField::Group,
    NewField::Prompt,
];

impl NewField {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Path => "Path",
            Self::Command => "Command",
            Self::Group => "Group",
            Self::Prompt => "Prompt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NewForm {
    name: String,
    path: String,
    command: String,
    group_name: String,
    prompt: String,
    field_index: usize,
}

impl Default for NewForm {
    fn default() -> Self {
        Self {
            name: "new-session".to_string(),
            path: ".".to_string(),
            command: "cat".to_string(),
            group_name: "default".to_string(),
            prompt: String::new(),
            field_index: 0,
        }
    }
}

impl NewForm {
    fn current_field(&self) -> NewField {
        NEW_FIELDS[self.field_index]
    }

    fn current_value_mut(&mut self) -> &mut String {
        match self.current_field() {
            NewField::Name => &mut self.name,
            NewField::Path => &mut self.path,
            NewField::Command => &mut self.command,
            NewField::Group => &mut self.group_name,
            NewField::Prompt => &mut self.prompt,
        }
    }

    fn field_value(&self, field: NewField) -> &str {
        match field {
            NewField::Name => &self.name,
            NewField::Path => &self.path,
            NewField::Command => &self.command,
            NewField::Group => &self.group_name,
            NewField::Prompt => &self.prompt,
        }
    }

    fn next_field(&mut self) {
        self.field_index = (self.field_index + 1).min(NEW_FIELDS.len() - 1);
    }

    fn previous_field(&mut self) {
        self.field_index = self.field_index.saturating_sub(1);
    }

    fn build_request(&self) -> std::result::Result<CreateSession, &'static str> {
        let name = self.name.trim();
        let path = self.path.trim();
        let command = self.command.trim();
        let group_name = self.group_name.trim();

        if name.is_empty() {
            return Err("name is required");
        }
        if path.is_empty() {
            return Err("path is required");
        }
        if command.is_empty() {
            return Err("command is required");
        }
        if group_name.is_empty() {
            return Err("group is required");
        }

        Ok(CreateSession {
            path: path.to_string(),
            agent: "shell".to_string(),
            command: command.to_string(),
            name: name.to_string(),
            group_name: group_name.to_string(),
            worktree: None,
            carry_state: false,
            sandbox: false,
            prompt: non_empty(self.prompt.trim()),
            parent_session_id: None,
        })
    }
}

fn run_app<F, D>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    load_details: &mut D,
    handle_action: &mut F,
) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
    D: FnMut(&str) -> Result<TuiDetails>,
{
    refresh_details(app, load_details, true)?;
    loop {
        refresh_details(app, load_details, false)?;
        terminal.draw(|frame| render(frame, app))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(());
        }

        match app.mode {
            Mode::Normal => {
                if handle_normal_key(key, app, handle_action)? {
                    return Ok(());
                }
            }
            Mode::Search => handle_search_key(key, app),
            Mode::New(_) => handle_new_key(key, app, handle_action)?,
            Mode::Send(_) => handle_send_key(key, app, handle_action)?,
        }
        refresh_details(app, load_details, true)?;
    }
}

fn refresh_details<D>(app: &mut App, load_details: &mut D, force: bool) -> Result<()>
where
    D: FnMut(&str) -> Result<TuiDetails>,
{
    let selected = selected_id(app);
    if !force && selected == app.details_session_id {
        return Ok(());
    }

    let Some(session_id) = selected else {
        app.details = TuiDetails::default();
        app.details_session_id = None;
        return Ok(());
    };

    match load_details(&session_id) {
        Ok(details) => {
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

fn handle_normal_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<bool>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('/') => app.mode = Mode::Search,
        KeyCode::Char('a') => app.show_archived = !app.show_archived,
        KeyCode::Char('n') => {
            app.mode = Mode::New(NewForm::default());
            app.status_message = None;
        }
        KeyCode::Char('s') => {
            if selected_id(app).is_some() {
                app.mode = Mode::Send(SendForm::default());
                app.status_message = None;
            }
        }
        KeyCode::Char('x') => run_selected_action(app, handle_action, TuiAction::Stop, "stopped")?,
        KeyCode::Char('r') => {
            run_selected_action(app, handle_action, TuiAction::Restart, "restarted")?
        }
        KeyCode::Char('f') => run_selected_action(app, handle_action, TuiAction::Fork, "forked")?,
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected_index = app
                .selected_index
                .saturating_add(1)
                .min(app.view().visible_count.saturating_sub(1));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected_index = app.selected_index.saturating_sub(1);
        }
        KeyCode::Char(ch) => {
            if let Some(view_mode) = ViewMode::from_key(ch) {
                app.view_mode = view_mode;
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_search_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.mode = Mode::Normal,
        KeyCode::Backspace => {
            app.query.pop();
            app.selected_index = 0;
        }
        KeyCode::Char(ch) => {
            app.query.push(ch);
            app.selected_index = 0;
        }
        _ => {}
    }
}

fn handle_new_key<F>(key: KeyEvent, app: &mut App, handle_action: &mut F) -> Result<()>
where
    F: FnMut(TuiAction) -> Result<Vec<SessionRecord>>,
{
    let mut submit = false;

    if let Mode::New(form) = &mut app.mode {
        match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Tab | KeyCode::Down => form.next_field(),
            KeyCode::BackTab | KeyCode::Up => form.previous_field(),
            KeyCode::Enter => {
                if form.field_index == NEW_FIELDS.len() - 1 {
                    submit = true;
                } else {
                    form.next_field();
                }
            }
            KeyCode::Backspace => {
                form.current_value_mut().pop();
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                submit = true;
            }
            KeyCode::Char(ch) => {
                form.current_value_mut().push(ch);
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

        match handle_action(TuiAction::Create(request)) {
            Ok(sessions) => {
                app.sessions = sessions;
                app.query.clear();
                app.selected_index = 0;
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
        match handle_action(TuiAction::Send { session_id, text }) {
            Ok(sessions) => {
                app.sessions = sessions;
                app.mode = Mode::Normal;
                app.status_message = Some("sent input".to_string());
            }
            Err(err) => app.status_message = Some(format!("send failed: {err}")),
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
    match handle_action(build(session_id)) {
        Ok(sessions) => {
            app.sessions = sessions;
            app.selected_index = app.selected_index.min(app.sessions.len().saturating_sub(1));
            app.status_message = Some(done.to_string());
        }
        Err(err) => app.status_message = Some(format!("{done} failed: {err}")),
    }
    Ok(())
}

fn selected_id(app: &App) -> Option<String> {
    app.view().selected.map(|session| session.id)
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let view = app.view();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    frame.render_widget(header(app, &view), chunks[0]);
    frame.render_widget(session_list(&view), body[0]);
    match &app.mode {
        Mode::New(form) => frame.render_widget(create_form(form, &app.status_message), body[1]),
        Mode::Send(form) => frame.render_widget(send_form(form, &app.status_message), body[1]),
        _ => frame.render_widget(detail_panel(app, &view), body[1]),
    }
    frame.render_widget(footer(app), chunks[2]);
}

fn header(app: &App, view: &DashboardView) -> Paragraph<'static> {
    let archived = if app.show_archived {
        "archived shown"
    } else {
        "archived hidden"
    };
    let message = app.status_message.as_deref().unwrap_or("");

    Paragraph::new(format!(
        "View: {} | Search: {} | {} | {} visible | {}",
        app.view_mode.label(),
        empty_label(&app.query),
        archived,
        view.visible_count,
        message
    ))
    .block(Block::default().borders(Borders::ALL).title("Agent Helm"))
}

fn session_list(view: &DashboardView) -> List<'static> {
    let selected_id = view.selected.as_ref().map(|session| session.id.as_str());
    let mut items = Vec::new();

    for group in &view.groups {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("{} ({})", group.name, group.sessions.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))));

        for session in &group.sessions {
            let selected = selected_id == Some(session.id.as_str());
            let marker = if selected { ">" } else { " " };
            let archived = if session.archived { " archived" } else { "" };
            let style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            items.push(ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::raw(" "),
                Span::styled(session.status.as_str(), status_style(session.status)),
                Span::raw(format!(" {}{} ", session.name, archived)),
                Span::styled(
                    format!("({})", session.agent),
                    Style::default().fg(Color::Gray),
                ),
            ])));
        }
    }

    if items.is_empty() {
        items.push(ListItem::new("No sessions"));
    }

    List::new(items).block(Block::default().borders(Borders::ALL).title("Sessions"))
}

fn detail_panel(app: &App, view: &DashboardView) -> Paragraph<'static> {
    match app.view_mode {
        ViewMode::Dashboard => dashboard_panel(app, view),
        ViewMode::Preview => preview(view, &app.details.output),
        ViewMode::Diff => {
            selected_panel(ViewMode::Diff, view, &app.details.diff, "No diff loaded.")
        }
        ViewMode::Structured => selected_panel(
            ViewMode::Structured,
            view,
            &app.details.structured,
            "No structured events loaded.",
        ),
        ViewMode::Settings => settings_panel(app, view),
    }
}

fn dashboard_panel(app: &App, view: &DashboardView) -> Paragraph<'static> {
    let selected = view
        .selected
        .as_ref()
        .map(|session| format!("{} ({})", session.name, session.status.as_str()))
        .unwrap_or_else(|| "-".to_string());
    let archived = if app.show_archived { "shown" } else { "hidden" };

    Paragraph::new(vec![
        Line::from(format!("Visible sessions: {}", view.visible_count)),
        Line::from(format!("Archived: {archived}")),
        Line::from(format!("Archived total: {}", view.hidden_archived)),
        Line::from(format!("Selected: {selected}")),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(ViewMode::Dashboard.title()),
    )
    .wrap(Wrap { trim: false })
}

fn preview(view: &DashboardView, output: &str) -> Paragraph<'static> {
    let text = match &view.selected {
        Some(session) => format!(
            "{}\n{}\n{}\n{}\n\n{}",
            session.name,
            session.id,
            session.project_path,
            session.command,
            empty_label(output)
        ),
        None => "No session selected.".to_string(),
    };

    Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(ViewMode::Preview.title()),
        )
        .wrap(Wrap { trim: false })
}

fn selected_panel(
    mode: ViewMode,
    view: &DashboardView,
    content: &str,
    empty: &str,
) -> Paragraph<'static> {
    let text = match &view.selected {
        Some(session) => format!(
            "{}\n{}\n{}\n{}\n\n{}",
            session.name,
            session.id,
            session.project_path,
            session.command,
            if content.trim().is_empty() {
                empty.to_string()
            } else {
                content.to_string()
            }
        ),
        None => "No session selected.".to_string(),
    };

    Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(mode.title()))
        .wrap(Wrap { trim: false })
}

fn settings_panel(app: &App, view: &DashboardView) -> Paragraph<'static> {
    let archived = if app.show_archived { "shown" } else { "hidden" };

    Paragraph::new(vec![
        Line::from(format!("Search: {}", empty_label(&app.query))),
        Line::from(format!("Archived: {archived}")),
        Line::from(format!("Visible sessions: {}", view.visible_count)),
        Line::from(format!("Selected index: {}", view.selected_index)),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(ViewMode::Settings.title()),
    )
    .wrap(Wrap { trim: false })
}

fn create_form(form: &NewForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = Vec::new();

    for field in NEW_FIELDS {
        let active = field == form.current_field();
        let marker = if active { ">" } else { " " };
        let value = form.field_value(field);
        let style = if active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::raw(" "),
            Span::styled(format!("{:<8}", field.label()), style),
            Span::raw(" "),
            Span::raw(value.to_string()),
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
        .block(Block::default().borders(Borders::ALL).title("New Session"))
        .wrap(Wrap { trim: false })
}

fn send_form(form: &SendForm, status_message: &Option<String>) -> Paragraph<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled(">", Style::default().fg(Color::Yellow)),
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
        .block(Block::default().borders(Borders::ALL).title("Send Input"))
        .wrap(Wrap { trim: false })
}

fn footer(app: &App) -> Paragraph<'static> {
    let text = footer_text(app);
    Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )]))
}

fn footer_text(app: &App) -> String {
    match &app.mode {
        Mode::Search => "type search | Enter/Esc done".to_string(),
        Mode::New(_) => "Tab field | Enter next/create | Ctrl-S create | Esc cancel".to_string(),
        Mode::Send(_) => "type input | Enter send | Esc cancel".to_string(),
        Mode::Normal => format!(
            "{} | n new s send x stop r restart f fork / find a arch q quit",
            view_mode_key_labels()
        ),
    }
}

fn view_mode_key_labels() -> String {
    VIEW_MODES
        .iter()
        .map(|mode| format!("{} {}", mode.key(), mode.short_label()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn status_style(status: SessionStatus) -> Style {
    match status {
        SessionStatus::Running => Style::default().fg(Color::Green),
        SessionStatus::Starting => Style::default().fg(Color::Yellow),
        SessionStatus::Stopped => Style::default().fg(Color::Gray),
        SessionStatus::Errored => Style::default().fg(Color::Red),
    }
}

fn empty_label(value: &str) -> String {
    if value.trim().is_empty() {
        "-".to_string()
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

    #[test]
    fn groups_sessions() {
        let sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "core", "build", false),
            record("3", "ops", "logs", false),
        ];
        let view = DashboardView::build(&sessions, "", 0, false);

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
    fn search_matches_command_case_insensitively() {
        let mut sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "core", "build", false),
        ];
        sessions[1].command = "Cargo Test".to_string();

        let view = DashboardView::build(&sessions, "test", 0, false);

        assert_eq!(view.visible_count, 1);
        assert_eq!(view.selected.unwrap().id, "2");
    }

    #[test]
    fn archived_is_hidden_by_default() {
        let sessions = vec![
            record("1", "ops", "deploy", false),
            record("2", "ops", "old", true),
        ];

        let hidden = DashboardView::build(&sessions, "", 0, false);
        let shown = DashboardView::build(&sessions, "", 0, true);

        assert_eq!(hidden.visible_count, 1);
        assert_eq!(hidden.hidden_archived, 1);
        assert_eq!(shown.visible_count, 2);
    }

    #[test]
    fn refresh_details_loads_selected_session() {
        let mut app = App {
            sessions: vec![record("1", "ops", "deploy", false)],
            details: TuiDetails::default(),
            details_session_id: None,
            query: String::new(),
            selected_index: 0,
            show_archived: false,
            view_mode: ViewMode::Preview,
            mode: Mode::Normal,
            status_message: None,
        };

        refresh_details(
            &mut app,
            &mut |session_id| {
                Ok(TuiDetails {
                    output: session_id.to_string(),
                    diff: "diff".to_string(),
                    structured: "structured".to_string(),
                })
            },
            false,
        )
        .unwrap();

        assert_eq!(app.details.output, "1");
        assert_eq!(app.details.diff, "diff");
        assert_eq!(app.details_session_id.as_deref(), Some("1"));
    }

    #[test]
    fn new_form_builds_shell_request() {
        let form = NewForm {
            name: "demo".to_string(),
            path: "/tmp/project".to_string(),
            command: "cat".to_string(),
            group_name: "work".to_string(),
            prompt: "hello".to_string(),
            field_index: 0,
        };

        let request = form.build_request().unwrap();

        assert_eq!(request.agent, "shell");
        assert_eq!(request.name, "demo");
        assert_eq!(request.path, "/tmp/project");
        assert_eq!(request.command, "cat");
        assert_eq!(request.group_name, "work");
        assert_eq!(request.prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn new_form_requires_name_path_command_and_group() {
        let mut form = NewForm::default();
        form.name.clear();
        assert_eq!(form.build_request().unwrap_err(), "name is required");

        form.name = "demo".to_string();
        form.path.clear();
        assert_eq!(form.build_request().unwrap_err(), "path is required");

        form.path = ".".to_string();
        form.command.clear();
        assert_eq!(form.build_request().unwrap_err(), "command is required");

        form.command = "cat".to_string();
        form.group_name.clear();
        assert_eq!(form.build_request().unwrap_err(), "group is required");
    }

    #[test]
    fn view_mode_keys_cover_compact_modes() {
        assert_eq!(ViewMode::from_key('1'), Some(ViewMode::Dashboard));
        assert_eq!(ViewMode::from_key('2'), Some(ViewMode::Preview));
        assert_eq!(ViewMode::from_key('3'), Some(ViewMode::Diff));
        assert_eq!(ViewMode::from_key('4'), Some(ViewMode::Structured));
        assert_eq!(ViewMode::from_key('5'), Some(ViewMode::Settings));
        assert_eq!(ViewMode::from_key('6'), None);
    }

    #[test]
    fn footer_lists_view_mode_keys() {
        let app = test_app(vec![]);

        assert_eq!(
            footer_text(&app),
            "1 dash 2 prev 3 diff 4 struct 5 set | n new s send x stop r restart f fork / find a arch q quit"
        );
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
                TuiAction::Fork(id) => actions.push(format!("fork:{id}")),
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
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &mut app,
            &mut handle,
        )
        .unwrap();

        assert_eq!(actions, vec!["stop:1", "restart:1", "fork:1"]);
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
    fn render_shows_selected_view_mode_panel() {
        let backend = ratatui::backend::TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app(vec![record("1", "ops", "deploy", false)]);
        app.view_mode = ViewMode::Diff;

        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Diff"));
        assert!(text.contains("No diff loaded."));
        assert!(text.contains("1 dash 2 prev 3 diff 4 struct 5 set"));
    }

    fn test_app(sessions: Vec<SessionRecord>) -> App {
        App {
            sessions,
            details: TuiDetails {
                output: "latest output".to_string(),
                ..TuiDetails::default()
            },
            details_session_id: Some("1".to_string()),
            query: String::new(),
            selected_index: 0,
            show_archived: false,
            view_mode: ViewMode::Dashboard,
            mode: Mode::Normal,
            status_message: None,
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
