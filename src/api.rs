use crate::{
    error::{AppError, Result},
    models::{
        ArchiveSessionRequest, ArchiveSessionResult, ConductorAssignmentRecord, ConductorRecord,
        CostEvent, CostFilter, CostSummary, CreateSession, DeleteMode, DeleteSessionRequest,
        DeletionResult, ForkSessionRequest, ForkSessionResult, GroupRecord, McpAttachmentRecord,
        OutputPage, ProjectRecord, ProjectSpec, SessionRecord, SkillAttachmentRecord,
        StructuredEvent, WatcherRecord, WorktreeRecord, now_ts,
    },
};
use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::{HeaderMap, StatusCode, request::Parts},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, env, net::SocketAddr, pin::Pin, time::Duration};
use tokio_stream::{Stream, StreamExt, wrappers::IntervalStream};

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub trait AgentHelmApi: Clone + Send + Sync + 'static {
    fn list_sessions(&self, include_archived: bool) -> ApiResult<Vec<SessionRecord>>;
    fn create_session(&self, request: CreateSession) -> ApiResult<SessionRecord>;
    fn get_session(&self, id: &str) -> ApiResult<SessionRecord>;
    fn send(&self, id: &str, text: &str) -> ApiResult<()>;
    fn output(&self, id: &str, limit: usize, ansi: bool) -> ApiResult<OutputPage>;
    fn diff(&self, id: &str) -> ApiResult<String>;
    fn stop(&self, id: &str) -> ApiResult<SessionRecord>;
    fn restart(&self, id: &str) -> ApiResult<SessionRecord>;
    fn delete_session(&self, request: DeleteSessionRequest) -> ApiResult<DeletionResult>;
    fn archive_session(&self, request: ArchiveSessionRequest) -> ApiResult<ArchiveSessionResult>;
    fn fork_session(&self, request: ForkSessionRequest) -> ApiResult<ForkSessionResult>;
    fn structured_events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> ApiResult<Vec<StructuredEvent>>;
    fn list_groups(&self) -> ApiResult<Vec<GroupRecord>>;
    fn move_session_to_group(&self, id: &str, group_name: String) -> ApiResult<SessionRecord>;
    fn register_project(&self, request: ProjectSpec) -> ApiResult<ProjectRecord>;
    fn list_projects(&self) -> ApiResult<Vec<ProjectRecord>>;
    fn get_project(&self, id: &str) -> ApiResult<ProjectRecord>;
    fn list_worktrees(&self, project_id: &str) -> ApiResult<Vec<WorktreeRecord>>;
    fn create_worktree(&self, project_id: &str, branch: &str) -> ApiResult<WorktreeRecord>;
    fn list_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>>;
    fn attach_mcp(&self, session_id: &str, server_id: String) -> ApiResult<McpAttachmentRecord>;
    fn detach_mcp(&self, id: &str) -> ApiResult<McpAttachmentRecord>;
    fn sync_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>>;
    fn list_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>>;
    fn attach_skill(&self, session_id: &str, skill_id: String) -> ApiResult<SkillAttachmentRecord>;
    fn detach_skill(&self, id: &str) -> ApiResult<SkillAttachmentRecord>;
    fn sync_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>>;
    fn list_watchers(&self) -> ApiResult<Vec<WatcherRecord>>;
    fn start_watcher(&self, id: &str) -> ApiResult<WatcherRecord>;
    fn stop_watcher(&self, id: &str) -> ApiResult<WatcherRecord>;
    fn list_conductors(&self) -> ApiResult<Vec<ConductorRecord>>;
    fn get_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn start_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn heartbeat_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn stop_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn send_conductor(
        &self,
        conductor_id: &str,
        session_id: String,
        task_ref: String,
    ) -> ApiResult<ConductorAssignmentRecord>;
    fn record_cost(
        &self,
        session_id: &str,
        amount_usd: f64,
        payload: serde_json::Value,
    ) -> ApiResult<CostEvent>;
    fn cost_summary(&self, filter: CostFilter) -> ApiResult<CostSummary>;
}

#[derive(Clone)]
struct ApiState<C> {
    controller: C,
    read_only: bool,
}

#[derive(Clone)]
struct AuthState {
    token: Option<String>,
}

pub async fn serve<C>(
    controller: C,
    listen: String,
    token: Option<String>,
    token_env: String,
    read_only: bool,
) -> Result<()>
where
    C: AgentHelmApi,
{
    let addr: SocketAddr = listen
        .parse()
        .map_err(|err| AppError::msg(format!("invalid listen address: {err}")))?;
    let token = token
        .or_else(|| env::var(&token_env).ok())
        .filter(|token| !token.is_empty());
    if token.is_none() && !addr.ip().is_loopback() {
        return Err(AppError::msg(format!(
            "non-loopback web access requires --token or {token_env}"
        )));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app(controller, read_only, token)).await?;
    Ok(())
}

pub fn app<C>(controller: C, read_only: bool, token: Option<String>) -> Router
where
    C: AgentHelmApi,
{
    Router::new()
        .route("/", get(web_dashboard))
        .route("/api/about", get(about::<C>))
        .route(
            "/api/sessions",
            get(list_sessions::<C>).post(create_session::<C>),
        )
        .route(
            "/api/sessions/:id",
            get(get_session::<C>).delete(delete_session::<C>),
        )
        .route("/api/sessions/:id/send", post(send::<C>))
        .route("/api/sessions/:id/output", get(output::<C>))
        .route("/api/sessions/:id/fork", post(fork_session::<C>))
        .route("/api/sessions/:id/archive", post(archive_session::<C>))
        .route("/api/sessions/:id/stop", post(stop::<C>))
        .route("/api/sessions/:id/restart", post(restart::<C>))
        .route("/api/sessions/:id/group", post(move_session_group::<C>))
        .route("/api/sessions/:id/diff", get(diff::<C>))
        .route(
            "/api/sessions/:id/terminal-stream",
            get(terminal_stream::<C>),
        )
        .route(
            "/api/sessions/:id/structured-events",
            get(structured_events::<C>),
        )
        .route(
            "/api/sessions/:id/structured-stream",
            get(structured_stream::<C>),
        )
        .route(
            "/api/projects",
            get(list_projects::<C>).post(create_project::<C>),
        )
        .route("/api/groups", get(list_groups::<C>))
        .route("/api/projects/:id", get(get_project::<C>))
        .route(
            "/api/projects/:id/worktrees",
            get(list_worktrees::<C>).post(create_worktree::<C>),
        )
        .route("/api/mcp", get(list_mcp::<C>))
        .route("/api/mcp/sync", post(sync_mcp::<C>))
        .route("/api/mcp/:id/detach", post(detach_mcp::<C>))
        .route("/api/sessions/:id/mcp", post(attach_mcp::<C>))
        .route("/api/skills", get(list_skills::<C>))
        .route("/api/skills/sync", post(sync_skills::<C>))
        .route("/api/skills/:id/detach", post(detach_skill::<C>))
        .route("/api/sessions/:id/skills", post(attach_skill::<C>))
        .route("/api/watchers", get(list_watchers::<C>))
        .route("/api/watchers/:id/start", post(start_watcher::<C>))
        .route("/api/watchers/:id/stop", post(stop_watcher::<C>))
        .route("/api/conductors", get(list_conductors::<C>))
        .route("/api/conductors/:id", get(get_conductor::<C>))
        .route("/api/conductors/:id/start", post(start_conductor::<C>))
        .route(
            "/api/conductors/:id/heartbeat",
            post(heartbeat_conductor::<C>),
        )
        .route("/api/conductors/:id/stop", post(stop_conductor::<C>))
        .route("/api/conductors/:id/send", post(send_conductor::<C>))
        .route("/api/sessions/:id/costs", post(record_cost::<C>))
        .route("/api/costs", get(costs::<C>))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            AuthState { token },
            auth_middleware,
        ))
        .with_state(ApiState {
            controller,
            read_only,
        })
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn read_only() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "read_only",
            "read-only mode rejects mutations",
        )
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid API token",
        )
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
}

impl From<AppError> for ApiError {
    fn from(value: AppError) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            value.to_string(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: self.message,
            },
        });
        (self.status, body).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

struct Writable;

#[axum::async_trait]
impl<C> FromRequestParts<ApiState<C>> for Writable
where
    C: AgentHelmApi,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        _parts: &mut Parts,
        state: &ApiState<C>,
    ) -> std::result::Result<Self, Self::Rejection> {
        if state.read_only {
            Err(ApiError::read_only())
        } else {
            Ok(Self)
        }
    }
}

#[derive(Debug, Serialize)]
struct AboutResponse {
    name: &'static str,
    version: &'static str,
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct ListSessionsQuery {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    archived: bool,
    group: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionsResponse {
    sessions: Vec<SessionRecord>,
}

#[derive(Debug, Deserialize)]
struct SendRequest {
    text: String,
}

#[derive(Debug, Deserialize)]
struct OutputQuery {
    limit: Option<usize>,
    #[serde(default)]
    ansi: bool,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct DeleteSessionQuery {
    #[serde(default)]
    purge: bool,
    #[serde(default)]
    cleanup_worktree: bool,
}

#[derive(Debug, Deserialize)]
struct ForkRequest {
    name: Option<String>,
    group_name: Option<String>,
    worktree_branch: Option<String>,
    #[serde(default)]
    carry_state: bool,
}

#[derive(Debug, Deserialize)]
struct ArchiveRequest {
    archived_by: Option<String>,
    reason: Option<String>,
    #[serde(default = "default_true")]
    stop_if_running: bool,
}

#[derive(Debug, Deserialize)]
struct MoveGroupRequest {
    group_name: String,
}

#[derive(Debug, Deserialize)]
struct CreateProjectRequest {
    path: String,
    default_branch: Option<String>,
    #[serde(default)]
    trusted: bool,
}

#[derive(Debug, Deserialize)]
struct CreateWorktreeRequest {
    branch: String,
}

#[derive(Debug, Deserialize)]
struct AttachmentRequest {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ConductorSendRequest {
    session_id: String,
    task_ref: String,
}

#[derive(Debug, Deserialize)]
struct RecordCostRequest {
    amount_usd: f64,
    model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    total_tokens: Option<i64>,
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CostsQuery {
    project_id: Option<String>,
    group_name: Option<String>,
    session_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    start_at: Option<i64>,
    end_at: Option<i64>,
    #[serde(default)]
    include_archived: bool,
}

#[derive(Debug, Serialize)]
struct ActionResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct DiffResponse {
    text: String,
}

type SseEventStream = Pin<Box<dyn Stream<Item = std::result::Result<Event, Infallible>> + Send>>;

#[derive(Debug, Serialize)]
struct StructuredEventsResponse {
    mode: &'static str,
    events: Vec<StructuredEvent>,
}

fn default_true() -> bool {
    true
}

async fn auth_middleware(
    State(state): State<AuthState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(token) = state.token {
        let bearer = format!("Bearer {token}");
        let authorized = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == bearer)
            || headers
                .get("x-agent-helm-token")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == token);
        if !authorized {
            return ApiError::unauthorized().into_response();
        }
    }
    next.run(request).await
}

async fn not_found() -> ApiError {
    ApiError::not_found("route not found")
}

const WEB_DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Agent Helm</title>
<style>
:root { color-scheme: light dark; --border: #8d969f; --muted: #68707a; --accent: #1f7a5b; --danger: #a33a2a; }
* { box-sizing: border-box; }
body { margin: 0; font: 14px/1.4 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
header, footer { padding: 10px 14px; border-bottom: 1px solid var(--border); display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
footer { border-top: 1px solid var(--border); border-bottom: 0; color: var(--muted); }
main { display: grid; grid-template-columns: minmax(300px, 36%) 1fr; min-height: calc(100vh - 88px); }
section { min-width: 0; padding: 12px; }
aside { border-right: 1px solid var(--border); min-width: 0; }
h2 { margin: 0; font-size: 18px; }
input, button { font: inherit; border: 1px solid var(--border); border-radius: 4px; padding: 6px 8px; background: canvas; color: canvastext; min-width: 0; }
button { cursor: pointer; }
button.primary { background: var(--accent); color: white; border-color: var(--accent); }
button.danger { color: var(--danger); border-color: color-mix(in srgb, var(--danger) 60%, var(--border)); }
form { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; margin-bottom: 12px; }
form input[name=path], form input[name=prompt], form button[type=submit] { grid-column: 1 / -1; }
ul { list-style: none; margin: 0; padding: 0; }
li { border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent); padding: 8px; cursor: pointer; }
li[aria-selected=true] { outline: 2px solid var(--accent); outline-offset: -2px; }
.row { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--muted); }
.pill { border: 1px solid var(--border); border-radius: 999px; padding: 1px 7px; font-size: 12px; }
pre { margin: 0; padding: 10px; border: 1px solid var(--border); border-radius: 4px; min-height: 220px; overflow: auto; white-space: pre-wrap; }
.tabs { display: flex; gap: 6px; margin: 10px 0; flex-wrap: wrap; }
.tabs button[aria-selected=true] { border-color: var(--accent); color: var(--accent); }
@media (max-width: 760px) {
  main { grid-template-columns: 1fr; }
  aside { border-right: 0; border-bottom: 1px solid var(--border); }
}
</style>
</head>
<body>
<header>
  <strong>Agent Helm</strong>
  <input id="token" type="password" autocomplete="off" placeholder="API token">
  <button id="refresh" type="button">Refresh</button>
  <span id="mode" class="muted"></span>
  <span id="status" class="muted"></span>
</header>
<main>
  <aside>
    <section>
      <form id="new-session">
        <input name="name" placeholder="Name" value="web-session">
        <input name="agent" placeholder="Agent" value="shell">
        <input name="command" placeholder="Command" value="cat">
        <input name="group_name" placeholder="Group" value="default">
        <input name="path" placeholder="Project path" value=".">
        <input name="prompt" placeholder="Initial prompt">
        <button class="primary" type="submit">New Session</button>
      </form>
      <ul id="sessions"></ul>
    </section>
  </aside>
  <section>
    <div class="row">
      <h2 id="title">No session selected</h2>
      <div>
        <button id="stop" class="danger" type="button">Stop</button>
        <button id="restart" type="button">Restart</button>
        <button id="fork" type="button">Fork</button>
      </div>
    </div>
    <div class="tabs">
      <button data-tab="output" type="button" aria-selected="true">Output</button>
      <button data-tab="diff" type="button">Diff</button>
      <button data-tab="events" type="button">Structured</button>
    </div>
    <pre id="panel"></pre>
    <form id="send-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
      <input name="text" placeholder="Send input">
      <button type="submit">Send</button>
    </form>
  </section>
</main>
<footer>Local-first dashboard. Read/write depends on server mode token.</footer>
<script>
let selected = null;
let tab = "output";
let readOnly = false;
const $ = (id) => document.getElementById(id);
const headers = () => {
  const token = $("token").value.trim();
  return token ? {"x-agent-helm-token": token, "content-type": "application/json"} : {"content-type": "application/json"};
};
async function api(path, opts = {}) {
  const res = await fetch(path, {headers: headers(), ...opts});
  if (!res.ok) throw new Error((await res.text()) || res.statusText);
  return res.json();
}
function setStatus(text) {
  $("status").textContent = text;
}
function renderSelected(session) {
  $("title").textContent = session ? session.name : "No session selected";
}
function setReadOnly(value) {
  readOnly = value;
  $("mode").textContent = value ? "read-only" : "read/write";
  document.querySelectorAll("#new-session input, #new-session button, #send-form input, #send-form button, #stop, #restart, #fork")
    .forEach((item) => item.disabled = value);
}
function canWrite() {
  if (!readOnly) return true;
  setStatus("read-only mode");
  return false;
}
async function loadAbout() {
  try {
    const about = await api("/api/about");
    setReadOnly(about.read_only);
  } catch (err) {
    setStatus(err.message);
  }
}
async function loadSessions() {
  try {
    const body = await api("/api/sessions?all=true");
    const list = $("sessions");
    list.innerHTML = "";
    for (const session of body.sessions) {
      const li = document.createElement("li");
      li.setAttribute("aria-selected", selected === session.id);
      li.innerHTML = `<div class="row"><strong>${session.name}</strong><span class="pill">${session.status}</span></div><div class="muted">${session.group_name} / ${session.agent}</div>`;
      li.onclick = () => {
        selected = session.id;
        renderSelected(session);
        loadPanel();
        loadSessions();
      };
      list.appendChild(li);
    }
    setStatus(`${body.sessions.length} sessions`);
  } catch (err) {
    setStatus(err.message);
  }
}
async function loadPanel() {
  if (!selected) return;
  try {
    let body;
    if (tab === "diff") {
      body = await api(`/api/sessions/${selected}/diff`);
      $("panel").textContent = body.text || "(no diff)";
    } else if (tab === "events") {
      body = await api(`/api/sessions/${selected}/structured-events`);
      $("panel").textContent = JSON.stringify(body.events, null, 2);
    } else {
      body = await api(`/api/sessions/${selected}/output?limit=400`);
      $("panel").textContent = body.text || "";
    }
  } catch (err) {
    $("panel").textContent = err.message;
  }
}
$("refresh").onclick = () => {
  loadAbout();
  loadSessions();
  loadPanel();
};
$("new-session").onsubmit = async (event) => {
  event.preventDefault();
  if (!canWrite()) return;
  const data = Object.fromEntries(new FormData(event.target));
  try {
    const session = await api("/api/sessions", {method: "POST", body: JSON.stringify(data)});
    selected = session.id;
    renderSelected(session);
    await loadSessions();
    await loadPanel();
  } catch (err) {
    setStatus(err.message);
  }
};
$("send-form").onsubmit = async (event) => {
  event.preventDefault();
  if (!canWrite()) return;
  if (!selected) return;
  const text = new FormData(event.target).get("text");
  await api(`/api/sessions/${selected}/send`, {method: "POST", body: JSON.stringify({text})});
  event.target.reset();
  await loadPanel();
};
for (const id of ["stop", "restart", "fork"]) {
  $(id).onclick = async () => {
    if (!canWrite()) return;
    if (!selected) return;
    await api(`/api/sessions/${selected}/${id}`, {method: "POST", body: "{}"});
    await loadSessions();
    await loadPanel();
  };
}
for (const button of document.querySelectorAll(".tabs button")) {
  button.onclick = () => {
    tab = button.dataset.tab;
    document.querySelectorAll(".tabs button").forEach((item) => item.setAttribute("aria-selected", item === button));
    loadPanel();
  };
}
loadAbout().then(loadSessions);
</script>
</body>
</html>"##;

async fn web_dashboard() -> Html<&'static str> {
    Html(WEB_DASHBOARD_HTML)
}

#[allow(dead_code)]
async fn legacy_web_dashboard() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Agent Helm</title>
  <style>
    :root { color-scheme: light dark; --border: #88909a; --muted: #68707a; --accent: #1f7a5b; }
    * { box-sizing: border-box; }
    body { margin: 0; font: 14px/1.4 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    header, footer { padding: 10px 14px; border-bottom: 1px solid var(--border); display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
    footer { border-top: 1px solid var(--border); border-bottom: 0; color: var(--muted); }
    main { display: grid; grid-template-columns: minmax(280px, 36%) 1fr; min-height: calc(100vh - 88px); }
    section { min-width: 0; padding: 12px; }
    aside { border-right: 1px solid var(--border); min-width: 0; }
    input, select, button, textarea { font: inherit; border: 1px solid var(--border); border-radius: 4px; padding: 6px 8px; background: canvas; color: canvastext; }
    button { cursor: pointer; }
    button.primary { background: var(--accent); color: white; border-color: var(--accent); }
    form { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; margin-bottom: 12px; }
    form input[name=path], form input[name=prompt] { grid-column: 1 / -1; }
    ul { list-style: none; margin: 0; padding: 0; }
    li { border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent); padding: 8px; cursor: pointer; }
    li[aria-selected=true] { outline: 2px solid var(--accent); outline-offset: -2px; }
    .row { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
    .muted { color: var(--muted); }
    .pill { border: 1px solid var(--border); border-radius: 999px; padding: 1px 7px; font-size: 12px; }
    pre { margin: 0; padding: 10px; border: 1px solid var(--border); border-radius: 4px; min-height: 220px; overflow: auto; white-space: pre-wrap; }
    .tabs { display: flex; gap: 6px; margin: 10px 0; }
    .tabs button[aria-selected=true] { border-color: var(--accent); color: var(--accent); }
    @media (max-width: 760px) { main { grid-template-columns: 1fr; } aside { border-right: 0; border-bottom: 1px solid var(--border); } }
  </style>
</head>
<body>
  <header>
    <strong>Agent Helm</strong>
    <input id="token" type="password" autocomplete="off" placeholder="API token">
    <button id="refresh">Refresh</button>
    <span id="status" class="muted"></span>
  </header>
  <main>
    <aside>
      <section>
        <form id="new-session">
          <input name="name" placeholder="Name" value="web-session">
          <input name="agent" placeholder="Agent" value="shell">
          <input name="command" placeholder="Command" value="cat">
          <input name="group_name" placeholder="Group" value="default">
          <input name="path" placeholder="Project path" value=".">
          <input name="prompt" placeholder="Initial prompt">
          <button class="primary" type="submit">New Session</button>
        </form>
        <ul id="sessions"></ul>
      </section>
    </aside>
    <section>
      <div class="row">
        <h2 id="title">No session selected</h2>
        <div>
          <button id="stop">Stop</button>
          <button id="restart">Restart</button>
          <button id="fork">Fork</button>
        </div>
      </div>
      <div class="tabs">
        <button data-tab="output" aria-selected="true">Output</button>
        <button data-tab="diff">Diff</button>
        <button data-tab="events">Structured</button>
      </div>
      <pre id="panel"></pre>
      <form id="send-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
        <input name="text" placeholder="Send input">
        <button type="submit">Send</button>
      </form>
    </section>
  </main>
  <footer>Local-first dashboard. Read/write depends on server mode and token.</footer>
  <script>
    let selected = null;
    let tab = "output";
    const $ = (id) => document.getElementById(id);
    const headers = () => {
      const token = $("token").value.trim();
      return token ? {"x-agent-helm-token": token, "content-type": "application/json"} : {"content-type": "application/json"};
    };
    async function api(path, opts = {}) {
      const res = await fetch(path, {headers: headers(), ...opts});
      if (!res.ok) throw new Error((await res.text()) || res.statusText);
      return res.json();
    }
    function setStatus(text) { $("status").textContent = text; }
    async function loadSessions() {
      try {
        const body = await api("/api/sessions?all=true");
        const list = $("sessions");
        list.innerHTML = "";
        for (const session of body.sessions) {
          const li = document.createElement("li");
          li.setAttribute("aria-selected", selected === session.id);
          li.innerHTML = `<div class="row"><strong>${session.name}</strong><span class="pill">${session.status}</span></div><div class="muted">${session.group_name} / ${session.agent}</div>`;
          li.onclick = () => { selected = session.id; renderSelected(session); loadPanel(); loadSessions(); };
          list.appendChild(li);
        }
        setStatus(`${body.sessions.length} sessions`);
      } catch (err) { setStatus(err.message); }
    }
    function renderSelected(session) { $("title").textContent = session ? session.name : "No session selected"; }
    async function loadPanel() {
      if (!selected) return;
      try {
        let body;
        if (tab === "diff") {
          body = await api(`/api/sessions/${selected}/diff`);
          $("panel").textContent = body.text || "(no diff)";
        } else if (tab === "events") {
          body = await api(`/api/sessions/${selected}/structured-events`);
          $("panel").textContent = JSON.stringify(body.events, null, 2);
        } else {
          body = await api(`/api/sessions/${selected}/output?limit=400`);
          $("panel").textContent = body.text || "";
        }
      } catch (err) { $("panel").textContent = err.message; }
    }
    $("refresh").onclick = () => { loadSessions(); loadPanel(); };
    $("new-session").onsubmit = async (event) => {
      event.preventDefault();
      const data = Object.fromEntries(new FormData(event.target));
      try {
        const session = await api("/api/sessions", {method: "POST", body: JSON.stringify(data)});
        selected = session.id;
        renderSelected(session);
        await loadSessions();
        await loadPanel();
      } catch (err) { setStatus(err.message); }
    };
    $("send-form").onsubmit = async (event) => {
      event.preventDefault();
      if (!selected) return;
      const text = new FormData(event.target).get("text");
      await api(`/api/sessions/${selected}/send`, {method: "POST", body: JSON.stringify({text})});
      event.target.reset();
      await loadPanel();
    };
    for (const id of ["stop", "restart", "fork"]) {
      $(id).onclick = async () => {
        if (!selected) return;
        await api(`/api/sessions/${selected}/${id}`, {method: "POST", body: "{}"});
        await loadSessions();
        await loadPanel();
      };
    }
    for (const button of document.querySelectorAll(".tabs button")) {
      button.onclick = () => {
        tab = button.dataset.tab;
        document.querySelectorAll(".tabs button").forEach((item) => item.setAttribute("aria-selected", item === button));
        loadPanel();
      };
    }
    loadSessions();
  </script>
</body>
</html>"#,
    )
}

async fn about<C>(State(state): State<ApiState<C>>) -> Json<AboutResponse>
where
    C: AgentHelmApi,
{
    Json(AboutResponse {
        name: "agent-helm",
        version: env!("CARGO_PKG_VERSION"),
        read_only: state.read_only,
    })
}

async fn list_sessions<C>(
    State(state): State<ApiState<C>>,
    Query(query): Query<ListSessionsQuery>,
) -> ApiResult<Json<SessionsResponse>>
where
    C: AgentHelmApi,
{
    Ok(Json(SessionsResponse {
        sessions: state
            .controller
            .list_sessions(query.all || query.archived)?
            .into_iter()
            .filter(|session| !query.archived || session.archived)
            .filter(|session| {
                query
                    .group
                    .as_deref()
                    .is_none_or(|group| session.group_name == group)
            })
            .filter(|session| {
                query
                    .status
                    .as_deref()
                    .is_none_or(|status| session.status.as_str() == status)
            })
            .collect(),
    }))
}

async fn create_session<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Json(request): Json<CreateSession>,
) -> ApiResult<Json<SessionRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.create_session(request)?))
}

async fn get_session<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.get_session(&id)?))
}

async fn send<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<SendRequest>,
) -> ApiResult<Json<ActionResponse>>
where
    C: AgentHelmApi,
{
    state.controller.send(&id, &request.text)?;
    Ok(Json(ActionResponse { ok: true }))
}

async fn output<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> ApiResult<Json<OutputPage>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.output(
        &id,
        query.limit.unwrap_or(200),
        query.ansi,
    )?))
}

async fn stop<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.stop(&id)?))
}

async fn restart<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.restart(&id)?))
}

async fn delete_session<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Query(query): Query<DeleteSessionQuery>,
) -> ApiResult<Json<DeletionResult>>
where
    C: AgentHelmApi,
{
    let mode = if query.purge {
        DeleteMode::Purge
    } else if query.cleanup_worktree {
        DeleteMode::CleanupWorktree
    } else {
        DeleteMode::MetadataOnly
    };
    Ok(Json(state.controller.delete_session(
        DeleteSessionRequest {
            session_id: id,
            mode,
            reason: "api delete".to_string(),
        },
    )?))
}

async fn fork_session<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<ForkRequest>,
) -> ApiResult<Json<ForkSessionResult>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.fork_session(ForkSessionRequest {
        parent_session_id: id,
        name: request.name,
        group_name: request.group_name,
        worktree_branch: request.worktree_branch,
        carry_state: request.carry_state,
        start_immediately: true,
    })?))
}

async fn archive_session<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<ArchiveRequest>,
) -> ApiResult<Json<ArchiveSessionResult>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.archive_session(
        ArchiveSessionRequest {
            session_id: id,
            archived_by: request.archived_by.unwrap_or_else(|| "api".to_string()),
            reason: request.reason.unwrap_or_default(),
            stop_if_running: request.stop_if_running,
        },
    )?))
}

async fn diff<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<DiffResponse>>
where
    C: AgentHelmApi,
{
    Ok(Json(DiffResponse {
        text: state.controller.diff(&id)?,
    }))
}

async fn terminal_stream<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> ApiResult<Sse<SseEventStream>>
where
    C: AgentHelmApi,
{
    state.controller.get_session(&id)?;
    let controller = state.controller;
    let limit = query.limit.unwrap_or(2_000);
    let ansi = query.ansi;
    let mut last_text = None::<String>;
    let stream = IntervalStream::new(tokio::time::interval(Duration::from_millis(1_000)))
        .filter_map(move |_| {
            let output = controller.output(&id, limit, ansi);
            match output {
                Ok(page) => {
                    if last_text.as_ref() == Some(&page.text) {
                        None
                    } else {
                        last_text = Some(page.text.clone());
                        Some(sse_json_event("terminal", &page))
                    }
                }
                Err(err) => Some(sse_text_event("error", &err.message)),
            }
        });
    let stream: SseEventStream = Box::pin(stream);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn structured_events<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<StructuredEventsResponse>>
where
    C: AgentHelmApi,
{
    structured_events_snapshot(state, id, query)
}

async fn structured_stream<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Sse<SseEventStream>>
where
    C: AgentHelmApi,
{
    state.controller.get_session(&id)?;
    let controller = state.controller;
    let mut since = query.since.unwrap_or(0);
    let limit = query.limit.unwrap_or(200);
    let stream = IntervalStream::new(tokio::time::interval(Duration::from_millis(1_000)))
        .filter_map(
            move |_| match controller.structured_events(&id, since, limit) {
                Ok(events) if events.is_empty() => None,
                Ok(events) => {
                    if let Some(last) = events.last() {
                        since = last.id;
                    }
                    Some(sse_json_event("structured", &events))
                }
                Err(err) => Some(sse_text_event("error", &err.message)),
            },
        );
    let stream: SseEventStream = Box::pin(stream);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn move_session_group<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<MoveGroupRequest>,
) -> ApiResult<Json<SessionRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(
        state
            .controller
            .move_session_to_group(&id, request.group_name)?,
    ))
}

fn structured_events_snapshot<C>(
    state: ApiState<C>,
    id: String,
    query: EventsQuery,
) -> ApiResult<Json<StructuredEventsResponse>>
where
    C: AgentHelmApi,
{
    Ok(Json(StructuredEventsResponse {
        mode: "snapshot",
        events: state.controller.structured_events(
            &id,
            query.since.unwrap_or(0),
            query.limit.unwrap_or(200),
        )?,
    }))
}

fn sse_json_event<T: Serialize>(
    name: &'static str,
    value: &T,
) -> std::result::Result<Event, Infallible> {
    Ok(Event::default()
        .event(name)
        .data(serde_json::to_string(value).unwrap_or_else(|err| {
            format!(r#"{{"error":"failed to serialize SSE event: {err}"}}"#)
        })))
}

fn sse_text_event(name: &'static str, value: &str) -> std::result::Result<Event, Infallible> {
    Ok(Event::default().event(name).data(value))
}

async fn list_groups<C>(State(state): State<ApiState<C>>) -> ApiResult<Json<Vec<GroupRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_groups()?))
}

async fn list_projects<C>(State(state): State<ApiState<C>>) -> ApiResult<Json<Vec<ProjectRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_projects()?))
}

async fn create_project<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Json(request): Json<CreateProjectRequest>,
) -> ApiResult<Json<ProjectRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.register_project(ProjectSpec {
        profile: String::new(),
        root_path: request.path,
        default_branch: request.default_branch.unwrap_or_default(),
        trusted: request.trusted,
    })?))
}

async fn get_project<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.get_project(&id)?))
}

async fn list_worktrees<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<WorktreeRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_worktrees(&id)?))
}

async fn create_worktree<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<CreateWorktreeRequest>,
) -> ApiResult<Json<WorktreeRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(
        state.controller.create_worktree(&id, &request.branch)?,
    ))
}

async fn list_mcp<C>(State(state): State<ApiState<C>>) -> ApiResult<Json<Vec<McpAttachmentRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_mcp()?))
}

async fn attach_mcp<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<AttachmentRequest>,
) -> ApiResult<Json<McpAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.attach_mcp(&id, request.id)?))
}

async fn detach_mcp<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<McpAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.detach_mcp(&id)?))
}

async fn sync_mcp<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
) -> ApiResult<Json<Vec<McpAttachmentRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.sync_mcp()?))
}

async fn list_skills<C>(
    State(state): State<ApiState<C>>,
) -> ApiResult<Json<Vec<SkillAttachmentRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_skills()?))
}

async fn attach_skill<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<AttachmentRequest>,
) -> ApiResult<Json<SkillAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.attach_skill(&id, request.id)?))
}

async fn detach_skill<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<SkillAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.detach_skill(&id)?))
}

async fn sync_skills<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
) -> ApiResult<Json<Vec<SkillAttachmentRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.sync_skills()?))
}

async fn list_watchers<C>(State(state): State<ApiState<C>>) -> ApiResult<Json<Vec<WatcherRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_watchers()?))
}

async fn start_watcher<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<WatcherRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.start_watcher(&id)?))
}

async fn stop_watcher<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<WatcherRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.stop_watcher(&id)?))
}

async fn list_conductors<C>(
    State(state): State<ApiState<C>>,
) -> ApiResult<Json<Vec<ConductorRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_conductors()?))
}

async fn get_conductor<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ConductorRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.get_conductor(&id)?))
}

async fn start_conductor<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<ConductorRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.start_conductor(&id)?))
}

async fn heartbeat_conductor<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<ConductorRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.heartbeat_conductor(&id)?))
}

async fn stop_conductor<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<ConductorRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.stop_conductor(&id)?))
}

async fn send_conductor<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<ConductorSendRequest>,
) -> ApiResult<Json<ConductorAssignmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.send_conductor(
        &id,
        request.session_id,
        request.task_ref,
    )?))
}

async fn record_cost<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<RecordCostRequest>,
) -> ApiResult<Json<CostEvent>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.record_cost(
        &id,
        request.amount_usd,
        serde_json::json!({
            "model": request.model,
            "input_tokens": request.input_tokens.unwrap_or(0),
            "output_tokens": request.output_tokens.unwrap_or(0),
            "total_tokens": request.total_tokens.unwrap_or(0),
            "source": request.source.unwrap_or_else(|| "api".to_string()),
        }),
    )?))
}

async fn costs<C>(
    State(state): State<ApiState<C>>,
    Query(query): Query<CostsQuery>,
) -> ApiResult<Json<CostSummary>>
where
    C: AgentHelmApi,
{
    let now = now_ts();
    Ok(Json(state.controller.cost_summary(CostFilter {
        profile: String::new(),
        project_id: query.project_id,
        group_name: query.group_name,
        session_id: query.session_id,
        agent: query.agent,
        model: query.model,
        start_at: query.start_at.unwrap_or(0),
        end_at: query.end_at.unwrap_or(now),
        include_archived: query.include_archived,
    })?))
}
