use crate::{
    error::{AppError, Result},
    materialization::SessionMaterializationPlan,
    models::{
        AgentStateSyncResult, ArchiveSessionRequest, ArchiveSessionResult, CleanupReport,
        ConductorAssignmentRecord, ConductorRecord, CostEvent, CostFilter, CostSummary,
        CreateSession, DeleteMode, DeleteSessionRequest, DeletionResult, ForkSessionRequest,
        ForkSessionResult, GroupRecord, McpAttachmentRecord, OutputPage, ProjectRecord,
        ProjectSpec, SessionEvent, SessionRecord, SessionSearchResponse, SessionStatusSnapshot,
        SkillAttachmentRecord, StructuredEvent, WatcherEventRecord, WatcherRecord, WorkspaceRecord,
        WorktreeRecord, now_ts,
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
    routing::{delete, get, patch, post},
};
use names::{Generator, Name};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, env, net::SocketAddr, pin::Pin, time::Duration};
use tokio_stream::{Stream, StreamExt, wrappers::IntervalStream};

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub trait AgentHelmApi: Clone + Send + Sync + 'static {
    fn default_agent(&self) -> ApiResult<String>;
    fn tool_profiles(&self) -> ApiResult<Vec<ApiToolProfile>>;
    fn list_sessions(&self, include_archived: bool) -> ApiResult<Vec<SessionRecord>>;
    fn create_session(&self, request: CreateSession) -> ApiResult<SessionRecord>;
    fn get_session(&self, id: &str) -> ApiResult<SessionRecord>;
    fn status(&self, id: &str) -> ApiResult<SessionRecord>;
    fn status_snapshot(&self, id: &str) -> ApiResult<SessionStatusSnapshot>;
    fn send(&self, id: &str, text: &str) -> ApiResult<()>;
    fn output(&self, id: &str, limit: usize, ansi: bool) -> ApiResult<OutputPage>;
    fn diff(&self, id: &str) -> ApiResult<String>;
    fn session_materialization(&self, id: &str) -> ApiResult<SessionMaterializationPlan>;
    fn stop(&self, id: &str) -> ApiResult<SessionRecord>;
    fn restart(&self, id: &str) -> ApiResult<SessionRecord>;
    fn delete_session(&self, request: DeleteSessionRequest) -> ApiResult<DeletionResult>;
    fn archive_session(&self, request: ArchiveSessionRequest) -> ApiResult<ArchiveSessionResult>;
    fn restore_session(&self, id: &str) -> ApiResult<SessionRecord>;
    fn fork_session(&self, request: ForkSessionRequest) -> ApiResult<ForkSessionResult>;
    fn search_sessions(&self, query: &str, limit: usize) -> ApiResult<SessionSearchResponse>;
    fn events(&self, id: &str, since: i64, limit: usize) -> ApiResult<Vec<SessionEvent>>;
    fn record_session_event(
        &self,
        id: &str,
        kind: &str,
        payload: serde_json::Value,
    ) -> ApiResult<SessionEvent>;
    fn sync_agent_state(&self, id: &str) -> ApiResult<AgentStateSyncResult>;
    fn structured_events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> ApiResult<Vec<StructuredEvent>>;
    fn list_groups(&self) -> ApiResult<Vec<GroupRecord>>;
    fn create_group(
        &self,
        name: String,
        parent: Option<String>,
        default_project_path: Option<String>,
    ) -> ApiResult<GroupRecord>;
    fn update_group(
        &self,
        name: &str,
        default_project_path: Option<String>,
        clear_default_project_path: bool,
        collapsed: Option<bool>,
    ) -> ApiResult<GroupRecord>;
    fn delete_group(&self, name: &str, force: bool) -> ApiResult<()>;
    fn move_session_to_group(&self, id: &str, group_name: String) -> ApiResult<SessionRecord>;
    fn register_project(&self, request: ProjectSpec) -> ApiResult<ProjectRecord>;
    fn list_projects(&self) -> ApiResult<Vec<ProjectRecord>>;
    fn get_project(&self, id: &str) -> ApiResult<ProjectRecord>;
    fn set_project_trust(&self, id: &str, trusted: bool) -> ApiResult<ProjectRecord>;
    fn remove_project(&self, id: &str) -> ApiResult<()>;
    fn list_workspaces(&self, project_id: &str) -> ApiResult<Vec<WorkspaceRecord>>;
    fn get_workspace(&self, id: &str) -> ApiResult<WorkspaceRecord>;
    fn list_worktrees(&self, project_id: &str) -> ApiResult<Vec<WorktreeRecord>>;
    fn get_worktree(&self, id: &str) -> ApiResult<WorktreeRecord>;
    fn create_worktree(
        &self,
        project_id: &str,
        branch: &str,
        carry_state: bool,
    ) -> ApiResult<WorktreeRecord>;
    fn finish_worktree(&self, id: &str) -> ApiResult<WorktreeRecord>;
    fn cleanup_worktrees(&self, project_id: &str) -> ApiResult<CleanupReport>;
    fn list_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>>;
    fn attach_mcp(&self, session_id: &str, server_id: String) -> ApiResult<McpAttachmentRecord>;
    fn attach_project_mcp(
        &self,
        project_id: &str,
        server_id: String,
    ) -> ApiResult<McpAttachmentRecord>;
    fn attach_profile_mcp(&self, server_id: String) -> ApiResult<McpAttachmentRecord>;
    fn detach_mcp(&self, id: &str) -> ApiResult<McpAttachmentRecord>;
    fn sync_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>>;
    fn list_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>>;
    fn attach_skill(&self, session_id: &str, skill_id: String) -> ApiResult<SkillAttachmentRecord>;
    fn attach_project_skill(
        &self,
        project_id: &str,
        skill_id: String,
    ) -> ApiResult<SkillAttachmentRecord>;
    fn attach_profile_skill(&self, skill_id: String) -> ApiResult<SkillAttachmentRecord>;
    fn detach_skill(&self, id: &str) -> ApiResult<SkillAttachmentRecord>;
    fn sync_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>>;
    fn list_watchers(&self) -> ApiResult<Vec<WatcherRecord>>;
    fn list_project_watchers(&self, project_id: &str) -> ApiResult<Vec<WatcherRecord>>;
    fn list_watcher_events(
        &self,
        watcher: &str,
        offset: usize,
        limit: usize,
    ) -> ApiResult<Vec<WatcherEventRecord>>;
    fn create_watcher(
        &self,
        name: String,
        adapter_id: String,
        project_id: Option<String>,
        config_ref: String,
    ) -> ApiResult<WatcherRecord>;
    fn test_watcher(&self, id: &str) -> ApiResult<WatcherRecord>;
    fn start_watcher(&self, id: &str) -> ApiResult<WatcherRecord>;
    fn poll_watcher(&self, id: &str) -> ApiResult<WatcherRecord>;
    fn poll_running_watchers(&self) -> ApiResult<Vec<WatcherRecord>>;
    fn stop_watcher(&self, id: &str) -> ApiResult<WatcherRecord>;
    fn delete_watcher(&self, id: &str) -> ApiResult<()>;
    fn ingest_watcher_event(
        &self,
        id: &str,
        source: String,
        event_type: String,
        payload_ref: String,
        signature_status: String,
    ) -> ApiResult<WatcherEventRecord>;
    fn list_conductors(&self) -> ApiResult<Vec<ConductorRecord>>;
    fn create_conductor(&self, session_id: String) -> ApiResult<ConductorRecord>;
    fn get_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn start_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn heartbeat_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn stop_conductor(&self, id: &str) -> ApiResult<ConductorRecord>;
    fn delete_conductor(&self, id: &str) -> ApiResult<()>;
    fn list_conductor_assignments(&self, id: &str) -> ApiResult<Vec<ConductorAssignmentRecord>>;
    fn complete_conductor_assignment(
        &self,
        id: &str,
        status: &str,
    ) -> ApiResult<ConductorAssignmentRecord>;
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
    fn cost_events(
        &self,
        filter: CostFilter,
        offset: usize,
        limit: usize,
    ) -> ApiResult<Vec<CostEvent>>;
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiToolProfile {
    pub name: String,
    pub installed: bool,
    pub executable: Option<String>,
    pub flags: Vec<String>,
    pub worktree: String,
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
        .route("/s/:id", get(web_dashboard))
        .route("/api/about", get(about::<C>))
        .route(
            "/api/sessions",
            get(list_sessions::<C>).post(create_session::<C>),
        )
        .route(
            "/api/sessions/:id",
            get(get_session::<C>).delete(delete_session::<C>),
        )
        .route("/api/search", get(search_sessions::<C>))
        .route("/api/sessions/:id/status", get(session_status::<C>))
        .route(
            "/api/sessions/:id/status-snapshot",
            get(session_status_snapshot::<C>),
        )
        .route("/api/sessions/:id/send", post(send::<C>))
        .route("/api/sessions/:id/output", get(output::<C>))
        .route("/api/sessions/:id/fork", post(fork_session::<C>))
        .route("/api/sessions/:id/archive", post(archive_session::<C>))
        .route("/api/sessions/:id/restore", post(restore_session::<C>))
        .route("/api/sessions/:id/start", post(restart::<C>))
        .route("/api/sessions/:id/stop", post(stop::<C>))
        .route("/api/sessions/:id/restart", post(restart::<C>))
        .route("/api/sessions/:id/group", post(move_session_group::<C>))
        .route("/api/sessions/:id/diff", get(diff::<C>))
        .route(
            "/api/sessions/:id/materialization",
            get(session_materialization::<C>),
        )
        .route(
            "/api/sessions/:id/events",
            get(session_events::<C>).post(record_session_event::<C>),
        )
        .route("/api/sessions/:id/sync-state", post(sync_agent_state::<C>))
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
        .route("/api/groups", get(list_groups::<C>).post(create_group::<C>))
        .route(
            "/api/groups/*name",
            patch(update_group::<C>).delete(delete_group::<C>),
        )
        .route(
            "/api/projects/:id",
            get(get_project::<C>).delete(delete_project::<C>),
        )
        .route("/api/projects/:id/trust", post(trust_project::<C>))
        .route("/api/projects/:id/untrust", post(untrust_project::<C>))
        .route("/api/projects/:id/workspaces", get(list_workspaces::<C>))
        .route("/api/workspaces/:id", get(get_workspace::<C>))
        .route(
            "/api/projects/:id/worktrees",
            get(list_worktrees::<C>).post(create_worktree::<C>),
        )
        .route(
            "/api/projects/:id/worktrees/cleanup",
            post(cleanup_worktrees::<C>),
        )
        .route("/api/worktrees/:id", get(get_worktree::<C>))
        .route("/api/worktrees/:id/finish", post(finish_worktree::<C>))
        .route("/api/mcp", get(list_mcp::<C>))
        .route("/api/profile/mcp", post(attach_profile_mcp::<C>))
        .route("/api/mcp/sync", post(sync_mcp::<C>))
        .route("/api/mcp/:id/detach", post(detach_mcp::<C>))
        .route("/api/projects/:id/mcp", post(attach_project_mcp::<C>))
        .route("/api/sessions/:id/mcp", post(attach_mcp::<C>))
        .route("/api/skills", get(list_skills::<C>))
        .route("/api/profile/skills", post(attach_profile_skill::<C>))
        .route("/api/skills/sync", post(sync_skills::<C>))
        .route("/api/skills/:id/detach", post(detach_skill::<C>))
        .route("/api/projects/:id/skills", post(attach_project_skill::<C>))
        .route(
            "/api/projects/:id/watchers",
            get(list_project_watchers::<C>),
        )
        .route("/api/sessions/:id/skills", post(attach_skill::<C>))
        .route(
            "/api/watchers",
            get(list_watchers::<C>).post(create_watcher::<C>),
        )
        .route("/api/watchers/poll", post(poll_running_watchers::<C>))
        .route("/api/watchers/:id", delete(delete_watcher::<C>))
        .route(
            "/api/watchers/:id/events",
            get(watcher_events::<C>).post(ingest_watcher_event::<C>),
        )
        .route("/api/watchers/:id/test", post(test_watcher::<C>))
        .route("/api/watchers/:id/start", post(start_watcher::<C>))
        .route("/api/watchers/:id/poll", post(poll_watcher::<C>))
        .route("/api/watchers/:id/stop", post(stop_watcher::<C>))
        .route("/api/conductors", get(list_conductors::<C>))
        .route("/api/sessions/:id/conductor", post(create_conductor::<C>))
        .route(
            "/api/conductors/:id",
            get(get_conductor::<C>).delete(delete_conductor::<C>),
        )
        .route(
            "/api/conductors/:id/assignments",
            get(conductor_assignments::<C>),
        )
        .route(
            "/api/conductor-assignments/:id/complete",
            post(complete_conductor_assignment::<C>),
        )
        .route(
            "/api/conductor-assignments/:id",
            patch(update_conductor_assignment::<C>),
        )
        .route("/api/conductors/:id/start", post(start_conductor::<C>))
        .route(
            "/api/conductors/:id/heartbeat",
            post(heartbeat_conductor::<C>),
        )
        .route("/api/conductors/:id/stop", post(stop_conductor::<C>))
        .route("/api/conductors/:id/send", post(send_conductor::<C>))
        .route("/api/sessions/:id/costs", post(record_cost::<C>))
        .route("/api/costs", get(costs::<C>))
        .route("/api/cost-events", get(cost_events::<C>))
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

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
}

impl From<AppError> for ApiError {
    fn from(value: AppError) -> Self {
        let message = value.to_string();
        if is_not_found_message(&message) {
            return Self::not_found(message);
        }
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }
}

fn is_not_found_message(message: &str) -> bool {
    message == "not found" || message.contains(" not found")
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
    default_agent: String,
    default_path: String,
    default_session_name: String,
    agents: Vec<String>,
    tools: Vec<ApiToolProfile>,
}

#[derive(Debug, Deserialize)]
struct ListSessionsQuery {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    archived: bool,
    group: Option<String>,
    status: Option<String>,
    deck_status: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionsResponse {
    sessions: Vec<SessionRecord>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CreateGroupRequest {
    name: String,
    parent: Option<String>,
    default_project_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateGroupRequest {
    default_project_path: Option<String>,
    #[serde(default)]
    clear_default_project_path: bool,
    collapsed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DeleteGroupQuery {
    #[serde(default)]
    force: bool,
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
    #[serde(default = "default_true")]
    start_immediately: bool,
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
    #[serde(default)]
    carry_state: bool,
}

#[derive(Debug, Deserialize)]
struct CreateWatcherRequest {
    name: String,
    adapter_id: Option<String>,
    project_id: Option<String>,
    config: Option<serde_json::Value>,
    config_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IngestWatcherEventRequest {
    source: Option<String>,
    event_type: Option<String>,
    payload_ref: Option<String>,
    payload: Option<serde_json::Value>,
    signature_status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RecordSessionEventRequest {
    kind: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
    state: Option<String>,
    source: Option<String>,
    tool: Option<serde_json::Value>,
}

impl RecordSessionEventRequest {
    fn into_parts(self) -> ApiResult<(String, serde_json::Value)> {
        let kind = self.kind.unwrap_or_else(|| "agent_state".to_string());
        let mut payload = match self.payload {
            serde_json::Value::Null => serde_json::Map::new(),
            serde_json::Value::Object(object) => object,
            _ => return Err(ApiError::bad_request("event payload must be an object")),
        };
        if let Some(state) = self.state {
            payload.insert("state".to_string(), serde_json::json!(state));
        }
        if let Some(source) = self.source {
            payload.insert("source".to_string(), serde_json::json!(source));
        }
        if let Some(tool) = self.tool {
            payload.insert("tool".to_string(), tool);
        }
        Ok((kind, serde_json::Value::Object(payload)))
    }
}

#[derive(Debug, Deserialize)]
struct WatcherEventsQuery {
    offset: Option<usize>,
    limit: Option<usize>,
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
struct CompleteConductorAssignmentRequest {
    status: Option<String>,
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
    #[serde(default = "default_true")]
    include_archived: bool,
    #[serde(default)]
    active_only: bool,
}

#[derive(Debug, Deserialize)]
struct CostEventsQuery {
    project_id: Option<String>,
    group_name: Option<String>,
    session_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    start_at: Option<i64>,
    end_at: Option<i64>,
    #[serde(default = "default_true")]
    include_archived: bool,
    #[serde(default)]
    active_only: bool,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

impl CostsQuery {
    fn include_archived(&self) -> bool {
        self.include_archived && !self.active_only
    }
}

impl CostEventsQuery {
    fn include_archived(&self) -> bool {
        self.include_archived && !self.active_only
    }
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
    let path = request.uri().path();
    if path == "/" || path.starts_with("/s/") {
        return next.run(request).await;
    }
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
input, select, button { font: inherit; border: 1px solid var(--border); border-radius: 4px; padding: 6px 8px; background: canvas; color: canvastext; min-width: 0; }
button { cursor: pointer; }
button.primary { background: var(--accent); color: white; border-color: var(--accent); }
button.danger { color: var(--danger); border-color: color-mix(in srgb, var(--danger) 60%, var(--border)); }
form { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; margin-bottom: 12px; }
form input[name=path], form input[name=prompt], form button[type=submit] { grid-column: 1 / -1; }
ul { list-style: none; margin: 0; padding: 0; }
li { border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent); padding: 8px; cursor: pointer; }
li[aria-selected=true] { outline: 2px solid var(--accent); outline-offset: -2px; }
li.group-row { background: color-mix(in srgb, var(--accent) 10%, transparent); cursor: pointer; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; }
li.session-row { background: color-mix(in srgb, canvas 94%, var(--accent)); }
.row { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.session-title { display: inline-flex; align-items: center; gap: 6px; min-width: 0; }
.pr-badge { color: #ffb86c; font-weight: 700; }
.muted { color: var(--muted); }
.pill { border: 1px solid var(--border); border-radius: 999px; padding: 1px 7px; font-size: 12px; }
pre { margin: 0; padding: 10px; border: 1px solid var(--border); border-radius: 4px; min-height: 220px; overflow: auto; white-space: pre-wrap; }
.tabs { display: flex; gap: 6px; margin: 10px 0; flex-wrap: wrap; }
.tabs button[aria-selected=true] { border-color: var(--accent); color: var(--accent); }
.overlay { position: fixed; inset: 0; display: grid; place-items: start center; padding-top: 12vh; background: color-mix(in srgb, canvas 72%, transparent); z-index: 10; }
.overlay[hidden] { display: none; }
.palette { width: min(720px, calc(100vw - 24px)); border: 1px solid var(--border); border-radius: 8px; background: canvas; box-shadow: 0 18px 60px color-mix(in srgb, canvastext 22%, transparent); overflow: hidden; }
.palette input { width: 100%; border: 0; border-bottom: 1px solid var(--border); border-radius: 0; padding: 12px; }
.palette li { display: flex; justify-content: space-between; gap: 12px; }
.palette-kind { color: var(--muted); font-size: 12px; text-transform: uppercase; }
.shortcuts { width: min(560px, calc(100vw - 24px)); border: 1px solid var(--border); border-radius: 8px; background: canvas; box-shadow: 0 18px 60px color-mix(in srgb, canvastext 22%, transparent); padding: 14px; }
.shortcut-row { display: grid; grid-template-columns: minmax(120px, auto) 1fr; gap: 14px; padding: 7px 0; border-bottom: 1px solid color-mix(in srgb, var(--border) 35%, transparent); }
.shortcut-row:last-child { border-bottom: 0; }
.kbd { display: inline-block; border: 1px solid var(--border); border-radius: 4px; padding: 1px 6px; font-size: 12px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background: color-mix(in srgb, canvas 86%, canvastext); }
.confirm { width: min(460px, calc(100vw - 24px)); border: 1px solid var(--border); border-radius: 8px; background: canvas; box-shadow: 0 18px 60px color-mix(in srgb, canvastext 22%, transparent); padding: 14px; }
.confirm-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 14px; }
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
<input name="name" placeholder="Name">
<select id="new-agent" name="agent"><option value="shell">shell</option></select>
<span id="agent-profile-summary" class="muted"></span>
<input name="group_name" placeholder="Group" value="default">
<input name="path" placeholder="Project path" value=".">
<label class="muted"><input name="save_group_default" type="checkbox"> Save group default</label>
<label class="muted"><input id="new-worktree" name="use_worktree" type="checkbox"> Worktree</label>
<label class="muted"><input name="carry_state" type="checkbox"> Copy to current state</label>
<button class="primary" type="submit">New Session</button>
</form>
<form id="project-add-form" style="grid-template-columns: 1fr 1fr auto auto">
<input name="path" placeholder="Project path">
<input name="default_branch" placeholder="Default branch">
<label class="muted"><input name="trusted" type="checkbox"> Trusted</label>
<button type="submit">Add Project</button>
</form>
<form id="project-select-form" style="grid-template-columns: 1fr auto">
<input name="project" list="project-options" placeholder="Project id">
<datalist id="project-options"></datalist>
<button type="submit">Select Project</button>
</form>
<form id="search-form" style="grid-template-columns: 1fr auto">
<input name="q" placeholder="Search">
<button type="submit">Search</button>
</form>
<form id="group-filter-form" style="grid-template-columns: 1fr auto">
<input id="group-filter" name="group" placeholder="Group filter">
<button type="submit">Filter</button>
</form>
<form id="status-filter-form" style="grid-template-columns: 1fr auto">
<select id="status-filter" name="status">
<option value="">All statuses</option>
<option value="running">Running</option>
<option value="queued">Queued</option>
<option value="waiting">Waiting</option>
<option value="idle">Idle</option>
<option value="starting">Starting</option>
<option value="stopped">Stopped</option>
<option value="errored">Errored</option>
</select>
<button type="submit">Status</button>
</form>
<form id="group-create-form" style="grid-template-columns: repeat(3, minmax(0, 1fr)) auto">
<input name="name" placeholder="New group">
<input name="parent" placeholder="Parent group">
<input name="default_project_path" placeholder="Default working directory">
<button type="submit">Create Group</button>
</form>
<form id="group-update-form" style="grid-template-columns: repeat(4, minmax(0, 1fr)) auto">
<input name="name" placeholder="Update group">
<input name="default_project_path" placeholder="Default working directory">
<label class="muted"><input name="clear_default_project_path" type="checkbox"> Clear path</label>
<select name="collapsed">
<option value="">Collapse</option>
<option value="true">Collapsed</option>
<option value="false">Expanded</option>
</select>
<button type="submit">Update Group</button>
</form>
    <form id="group-delete-form" style="grid-template-columns: 1fr auto auto">
      <input name="name" placeholder="Delete group">
      <label class="muted"><input name="force" type="checkbox"> Force</label>
      <button class="danger" type="submit">Delete Group</button>
    </form>
<label class="muted"><input id="show-archived" type="checkbox"> Archived</label>
<div id="fleet-summary" class="muted"></div>
 <ul id="sessions"></ul>
    </section>
  </aside>
  <section>
<div class="row">
<h2 id="title">No session selected</h2>
<span id="deck-status" class="pill"></span>
</div>
<div id="session-context" class="muted"></div>
<div>
 <button id="start" type="button">Start</button>
 <button id="stop" class="danger" type="button">Stop</button>
<button id="restart" type="button">Restart</button>
<button id="fork" type="button">Fork</button>
<button id="sync-state" type="button">Sync State</button>
<input id="archive-reason" placeholder="Archive reason">
<input id="archive-by" placeholder="Archived by">
<button id="archive" class="danger" type="button">Archive</button>
<button id="restore" type="button">Restore</button>
<button id="remove-session" class="danger" type="button">Remove</button>
<label class="muted"><input id="remove-cleanup-worktree" type="checkbox"> Cleanup worktree</label>
<label class="muted"><input id="remove-purge" type="checkbox"> Purge history</label>
</div>
</div>
<form id="move-group-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
<input name="group_name" placeholder="Move to group">
<button type="submit">Move</button>
</form>
<form id="fork-form" style="grid-template-columns: repeat(3, minmax(0, 1fr)) auto auto auto; margin-top: 10px">
<input name="name" placeholder="Fork name">
<input name="group_name" placeholder="Fork group">
<input name="worktree_branch" placeholder="Fork worktree branch">
<label class="muted"><input name="carry_state" type="checkbox"> Copy to current state</label>
<label class="muted"><input name="no_start" type="checkbox"> No start</label>
<button type="submit">Fork Session</button>
</form>
<form id="project-form" style="grid-template-columns: repeat(4, minmax(0, 1fr)); margin-top: 10px">
<button name="action" value="trust" type="submit">Trust Project</button>
<button name="action" value="untrust" type="submit">Untrust Project</button>
<button name="action" value="cleanup" type="submit">Cleanup Worktrees</button>
<button id="project-remove" class="danger" name="action" value="remove" type="submit">Remove Project</button>
</form>
<form id="worktree-create-form" style="grid-template-columns: 1fr auto auto; margin-top: 10px">
<input name="branch" placeholder="Worktree branch">
<label class="muted"><input name="carry_state" type="checkbox"> Copy to current state</label>
<button type="submit">Create Worktree</button>
</form>
<form id="worktree-finish-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
<input name="id" placeholder="Worktree id">
<button type="submit">Finish Worktree</button>
</form>
<form id="mcp-form" style="grid-template-columns: 1fr auto auto auto auto; margin-top: 10px">
<input name="id" placeholder="MCP server or attachment">
<select name="scope">
<option value="session">Session</option>
<option value="project">Project</option>
<option value="profile">Profile</option>
</select>
<button name="action" value="attach" type="submit">Attach MCP</button>
<button name="action" value="sync" type="submit">Sync MCP</button>
<button name="action" value="detach" type="submit">Detach MCP</button>
</form>
<form id="skill-form" style="grid-template-columns: 1fr auto auto auto auto; margin-top: 10px">
<input name="id" placeholder="Skill or attachment">
<select name="scope">
<option value="session">Session</option>
<option value="project">Project</option>
<option value="profile">Profile</option>
</select>
<button name="action" value="attach" type="submit">Attach Skill</button>
<button name="action" value="sync" type="submit">Sync Skill</button>
<button name="action" value="detach" type="submit">Detach Skill</button>
</form>
<form id="watcher-create-form" style="grid-template-columns: 1fr auto 1fr 1fr auto auto; margin-top: 10px">
<input name="name" placeholder="Watcher name">
<select name="adapter">
<option value="shell">Shell</option>
<option value="manual">Manual</option>
</select>
<input name="command" placeholder="Shell command">
<input name="timeout_ms" placeholder="Timeout ms">
<label class="muted"><input name="require_signature" type="checkbox"> Require signature</label>
<button type="submit">Create Watcher</button>
</form>
<form id="watcher-form" style="grid-template-columns: 1fr auto auto auto auto auto; margin-top: 10px">
<input name="id" placeholder="Watcher">
<button name="action" value="start" type="submit">Start Watcher</button>
<button id="watcher-poll" name="action" value="poll" type="submit">Poll Watcher</button>
<button id="watcher-test" name="action" value="test" type="submit">Test Watcher</button>
<button id="watcher-stop" name="action" value="stop" type="submit">Stop Watcher</button>
<button id="watcher-remove" name="action" value="remove" type="submit">Remove Watcher</button>
</form>
<form id="watcher-events-form" style="grid-template-columns: 1fr auto auto auto; margin-top: 10px">
<input name="id" placeholder="Watcher">
<input name="offset" placeholder="Offset">
<input name="limit" placeholder="Limit">
<button type="submit">Watcher Events</button>
</form>
<form id="watcher-ingest-form" style="grid-template-columns: 1fr 1fr 1fr auto auto; margin-top: 10px">
<input name="id" placeholder="Watcher">
<input name="payload" placeholder="Payload">
<input name="event_type" placeholder="Event type" value="event">
<select name="signature_status">
<option value="not_applicable">No signature</option>
<option value="verified">Verified</option>
<option value="unverified">Unverified</option>
</select>
<button type="submit">Ingest Event</button>
</form>
<form id="watcher-poll-all-form" style="grid-template-columns: 1fr; margin-top: 10px">
<button type="submit">Poll Running Watchers</button>
</form>
<form id="conductor-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
<button type="submit">Setup Conductor</button>
</form>
<form id="conductor-action-form" style="grid-template-columns: 1fr 1fr auto auto auto auto auto auto; margin-top: 10px">
<input name="id" placeholder="Conductor">
<input name="task" placeholder="Task">
<button id="conductor-status" name="action" value="status" type="submit">Status</button>
<button id="conductor-start" name="action" value="start" type="submit">Start</button>
<button id="conductor-heartbeat" name="action" value="heartbeat" type="submit">Heartbeat</button>
<button id="conductor-send" name="action" value="send" type="submit">Send</button>
<button id="conductor-stop" name="action" value="stop" type="submit">Stop</button>
<button id="conductor-remove" name="action" value="remove" type="submit">Remove</button>
</form>
<form id="conductor-assignments-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
<input name="id" placeholder="Conductor">
<button type="submit">Assignments</button>
</form>
<form id="conductor-assignment-status-form" style="grid-template-columns: 1fr auto auto auto; margin-top: 10px">
<input name="id" placeholder="Assignment">
<button name="status" value="completed" type="submit">Complete</button>
<button name="status" value="failed" type="submit">Fail</button>
<button name="status" value="cancelled" type="submit">Cancel</button>
</form>
<div class="tabs">
 <button data-tab="output" type="button" aria-selected="true">Output</button>
 <button data-tab="diff" type="button">Diff</button>
<button data-tab="events" type="button">Structured</button>
<button data-tab="costs" type="button">Costs</button>
<button data-tab="attachments" type="button">MCP/Skills</button>
<button data-tab="worktrees" type="button">Worktrees</button>
<button data-tab="managers" type="button">Managers</button>
</div>
<form id="cost-filter-form" style="grid-template-columns: repeat(4, minmax(0, 1fr)); margin-top: 10px">
<input name="project_id" placeholder="Cost project">
<input name="group_name" placeholder="Cost group">
<input name="session_id" placeholder="Cost session">
<input name="agent" placeholder="Cost agent">
<input name="model" placeholder="Cost model">
<input name="start_at" placeholder="Start timestamp">
<input name="end_at" placeholder="End timestamp">
<label class="muted"><input name="include_archived" type="checkbox" checked> Include archived</label>
<button type="submit">Apply Costs</button>
</form>
<form id="cost-record-form" style="grid-template-columns: repeat(6, minmax(0, 1fr)) auto; margin-top: 10px">
<input name="amount_usd" placeholder="Cost USD">
<input name="model" placeholder="Cost model">
<input name="input_tokens" placeholder="Input tokens">
<input name="output_tokens" placeholder="Output tokens">
<input name="total_tokens" placeholder="Total tokens">
<input name="source" placeholder="Cost source" value="web">
<button type="submit">Record Cost</button>
</form>
<pre id="panel"></pre>
 <form id="send-form" style="grid-template-columns: 1fr auto; margin-top: 10px">
 <input name="text" placeholder="Send input">
 <button type="submit">Send</button>
 </form>
 <form id="session-event-form" style="grid-template-columns: 1fr 1fr 1fr auto; margin-top: 10px">
 <select name="state">
 <option value="working">Working</option>
 <option value="waiting">Waiting</option>
 <option value="idle">Idle</option>
 <option value="queued">Queued</option>
 </select>
 <input name="source" placeholder="State source" value="manual">
 <input name="tool" placeholder="Tool">
 <button type="submit">Record State</button>
 </form>
</section>
</main>
<div id="command-palette" class="overlay" hidden role="dialog" aria-label="Command palette">
<div id="palette-panel" class="palette">
<input id="palette-input" autocomplete="off" placeholder="Command or session">
<ul id="palette-results"></ul>
</div>
</div>
<div id="shortcut-help" class="overlay" hidden role="dialog" aria-label="Keyboard shortcuts">
<div id="shortcut-panel" class="shortcuts">
<div class="row"><strong>Keyboard shortcuts</strong><button id="shortcut-close" type="button">Close</button></div>
<div class="shortcut-row"><span><span class="kbd">Ctrl</span> + <span class="kbd">K</span></span><span>Command palette</span></div>
<div class="shortcut-row"><span><span class="kbd">?</span></span><span>Keyboard shortcuts</span></div>
<div class="shortcut-row"><span><span class="kbd">j</span> / <span class="kbd">k</span></span><span>Next / previous session</span></div>
<div class="shortcut-row"><span><span class="kbd">Enter</span></span><span>Open selected session</span></div>
<div class="shortcut-row"><span><span class="kbd">a</span></span><span>Toggle archived sessions</span></div>
<div class="shortcut-row"><span><span class="kbd">t</span></span><span>Cycle status filter</span></div>
<div class="shortcut-row"><span><span class="kbd">/</span></span><span>Focus search</span></div>
<div class="shortcut-row"><span><span class="kbd">n</span></span><span>New session</span></div>
<div class="shortcut-row"><span><span class="kbd">Esc</span></span><span>Close overlays</span></div>
</div>
</div>
<div id="confirm-dialog" class="overlay" hidden role="dialog" aria-label="Confirm action">
<div id="confirm-panel" class="confirm">
<div class="row"><strong>Confirm action</strong></div>
<p id="confirm-message"></p>
<div class="confirm-actions">
<button id="confirm-cancel" type="button">Cancel</button>
<button id="confirm-accept" class="danger" type="button">Confirm</button>
</div>
</div>
</div>
<footer>Local-first dashboard. Read/write depends on server mode token.</footer>
<script>
let selected = null;
let selectedProject = null;
let tab = "output";
let readOnly = false;
let outputStream = null;
let structuredStream = null;
let toolProfiles = [];
let defaultAgent = "shell";
let searchActive = false;
let statusRefreshRunning = false;
let paletteSessions = [];
let paletteSnapshots = new Map();
let visibleSessionIds = [];
let pendingConfirmAction = null;
const $ = (id) => document.getElementById(id);
function routeSessionId() {
const path = window.location && window.location.pathname ? window.location.pathname : "";
const match = path.match(/^\/s\/([^/?#]+)/);
return match ? decodeURIComponent(match[1]) : null;
}
function pushSessionRoute(sessionId) {
if (!window.history || !window.history.pushState) return;
const target = sessionId ? `/s/${encodeURIComponent(sessionId)}` : "/";
if (window.location && window.location.pathname === target) return;
window.history.pushState({sessionId}, "", target);
}
function clearSelectedSession(updateRoute = true) {
selected = null;
selectedProject = null;
renderSelected(null);
if (updateRoute) pushSessionRoute(null);
}
function selectSession(session, snapshot = null, updateRoute = true) {
selected = session.id;
selectedProject = session.project_id;
renderSelected(session, snapshot);
if (updateRoute) pushSessionRoute(session.id);
}
function commandPaletteRows(query = "") {
const needle = query.trim().toLowerCase();
const commands = [
{kind: "command", label: "New session", action: "new"},
{kind: "command", label: "Refresh", action: "refresh"},
{kind: "tab", label: "Output", tab: "output"},
{kind: "tab", label: "Diff", tab: "diff"},
{kind: "tab", label: "Structured", tab: "events"},
{kind: "tab", label: "Costs", tab: "costs"},
{kind: "tab", label: "MCP/Skills", tab: "attachments"},
{kind: "tab", label: "Worktrees", tab: "worktrees"},
{kind: "tab", label: "Managers", tab: "managers"},
];
const sessions = paletteSessions.map((session) => ({
kind: "session",
label: session.name,
detail: `${session.group_name} / ${session.agent}`,
session,
}));
return [...commands, ...sessions].filter((row) => {
if (!needle) return true;
return `${row.label} ${row.detail || ""} ${row.kind}`.toLowerCase().includes(needle);
});
}
function renderCommandPalette() {
const list = $("palette-results");
list.innerHTML = "";
for (const row of commandPaletteRows($("palette-input").value)) {
const li = document.createElement("li");
const label = document.createElement("span");
label.textContent = row.detail ? `${row.label} · ${row.detail}` : row.label;
const kind = document.createElement("span");
kind.className = "palette-kind";
kind.textContent = row.kind;
li.appendChild(label);
li.appendChild(kind);
li.onclick = () => runCommandPaletteRow(row);
list.appendChild(li);
}
}
function openCommandPalette() {
$("command-palette").hidden = false;
$("palette-input").value = "";
renderCommandPalette();
setTimeout(() => $("palette-input").focus(), 0);
}
function closeCommandPalette() {
$("command-palette").hidden = true;
}
function openShortcutHelp() {
$("shortcut-help").hidden = false;
}
function closeShortcutHelp() {
$("shortcut-help").hidden = true;
}
const statusFilterOrder = ["", "running", "queued", "waiting", "idle", "starting", "stopped", "errored"];
function toggleArchivedFilter() {
const checkbox = $("show-archived");
checkbox.checked = !checkbox.checked;
clearSelectedSession();
loadSessions();
}
function cycleStatusFilter() {
const select = $("status-filter");
const index = statusFilterOrder.indexOf(select.value);
select.value = statusFilterOrder[(index + 1) % statusFilterOrder.length];
loadSessions();
}
function confirmAction(message, action) {
pendingConfirmAction = action;
$("confirm-message").textContent = message;
$("confirm-dialog").hidden = false;
setTimeout(() => $("confirm-cancel").focus(), 0);
}
function closeConfirmDialog() {
pendingConfirmAction = null;
$("confirm-dialog").hidden = true;
}
async function runConfirmedAction() {
const action = pendingConfirmAction;
closeConfirmDialog();
if (action) await action();
}
async function setActiveTab(nextTab) {
tab = nextTab;
document.querySelectorAll(".tabs button").forEach((item) => item.setAttribute("aria-selected", item.dataset.tab === nextTab));
await loadPanel();
}
async function runCommandPaletteRow(row) {
closeCommandPalette();
if (row.kind === "session") {
const snapshot = paletteSnapshots.get(row.session.id);
selectSession(row.session, snapshot);
await loadSelectedSnapshot(row.session.id);
await loadPanel();
await loadSessions();
return;
}
if (row.kind === "tab") {
await setActiveTab(row.tab);
return;
}
if (row.action === "refresh") {
await refreshDashboard();
return;
}
if (row.action === "new") {
$("new-session").querySelector("input[name='name']").focus();
}
}
function visibleSessionIdByOffset(offset) {
if (visibleSessionIds.length === 0) return null;
let index = selected ? visibleSessionIds.indexOf(selected) : -1;
if (index < 0) index = offset > 0 ? -1 : 0;
return visibleSessionIds[(index + offset + visibleSessionIds.length) % visibleSessionIds.length];
}
async function selectVisibleSessionByOffset(offset) {
const id = visibleSessionIdByOffset(offset);
if (!id) return;
const session = paletteSessions.find((candidate) => candidate.id === id);
if (!session) return;
const snapshot = paletteSnapshots.get(id);
selectSession(session, snapshot);
await loadSelectedSnapshot(id);
await loadPanel();
await loadSessions();
}
async function openSelectedSession() {
if (!selected) {
await selectVisibleSessionByOffset(1);
return;
}
await loadSelectedSnapshot(selected);
await loadPanel();
}
function openSelectedSessionInNewTab() {
if (!selected || !window.open) return;
window.open(`/s/${encodeURIComponent(selected)}`, "_blank");
}
const headers = () => {
const token = $("token").value.trim();
return token ? {"x-agent-helm-token": token, "content-type": "application/json"} : {"content-type": "application/json"};
};
async function api(path, opts = {}) {
  const res = await fetch(path, {headers: headers(), ...opts});
  if (!res.ok) throw new Error((await res.text()) || res.statusText);
  if (res.status === 204) return null;
  return res.json();
}
function setStatus(text) {
  $("status").textContent = text;
}
function parseOptionalNonNegativeInteger(data, key) {
  const raw = data.get(key).trim();
  if (!raw) return null;
  if (!/^\d+$/.test(raw)) {
    throw new Error(`${key.replaceAll("_", " ")} must be a non-negative integer`);
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value)) {
    throw new Error(`${key.replaceAll("_", " ")} must be a non-negative integer`);
  }
  return value;
}
function stopOutputStream() {
  if (outputStream) {
    outputStream.abort();
    outputStream = null;
  }
}
function stopStructuredStream() {
  if (structuredStream) {
    structuredStream.abort();
    structuredStream = null;
  }
}
function parseSseBlock(block) {
  const event = {name: "message", data: []};
  for (const line of block.split("\n")) {
    if (line.startsWith("event:")) event.name = line.slice(6).trim();
    if (line.startsWith("data:")) event.data.push(line.slice(5).trimStart());
  }
  return event.data.length ? {name: event.name, data: event.data.join("\n")} : null;
}
async function loadOutputSnapshot(sessionId) {
  const body = await api(`/api/sessions/${sessionId}/output?limit=400`);
  if (selected === sessionId && tab === "output") $("panel").textContent = body.text || "";
}
function stringifyEventValue(value) {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value);
}
function eventPayload(event) {
  if (!event || typeof event !== "object") return event;
  return event.payload || event.payload_ref || event;
}
function eventPreview(event) {
  const payload = eventPayload(event);
  if (payload && typeof payload === "object") {
    const message = payload.message && typeof payload.message === "object" ? payload.message : null;
    const tool = payload.tool && typeof payload.tool === "object" ? payload.tool.name : null;
    for (const value of [
      payload.state,
      payload.text,
      payload.preview,
      payload.output,
      payload.route_decision,
      payload.name,
      tool,
      message ? message.content : null,
    ]) {
      const text = stringifyEventValue(value);
      if (text) return text;
    }
  }
  return stringifyEventValue(payload);
}
function eventSummary(event) {
  const payload = eventPayload(event);
  const kind = event && event.kind ? event.kind : (event && event.event_type ? event.event_type : "event");
  const id = event && (event.id || event.id === 0) ? `#${event.id}` : "";
  const source = event && event.source ? event.source : (payload && payload.source ? payload.source : "");
  const preview = eventPreview(event);
  const header = [kind, id, source].filter(Boolean).join(" ");
  return preview ? `${header} - ${preview}` : header;
}
function renderStructuredPanel(rawEvents, structuredEvents) {
  const rows = [];
  for (const event of structuredEvents || []) rows.push(`structured: ${eventSummary(event)}`);
  for (const event of rawEvents || []) rows.push(`raw: ${eventSummary(event)}`);
  $("panel").textContent = rows.length ? rows.join("\n") : "No events";
}
async function loadStructuredSnapshot(sessionId) {
  const [rawEvents, body] = await Promise.all([
    api(`/api/sessions/${sessionId}/events?limit=200`),
    api(`/api/sessions/${sessionId}/structured-events?limit=200`),
  ]);
  if (selected === sessionId && tab === "events") {
    renderStructuredPanel(rawEvents || [], body.events || []);
  }
}
function startOutputStream(sessionId) {
  stopOutputStream();
  if (!globalThis.ReadableStream || !globalThis.TextDecoder) {
    loadOutputSnapshot(sessionId).catch((err) => {
      if (selected === sessionId && tab === "output") $("panel").textContent = err.message;
    });
    return;
  }

  const controller = new AbortController();
  outputStream = controller;
  $("panel").textContent = "Loading output...";
  (async () => {
    try {
      const response = await fetch(`/api/sessions/${sessionId}/terminal-stream?limit=400`, {
        headers: headers(),
        signal: controller.signal,
      });
      if (!response.ok) throw new Error((await response.text()) || response.statusText);
      if (!response.body) {
        await loadOutputSnapshot(sessionId);
        return;
      }

      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      while (selected === sessionId && tab === "output") {
        const {value, done} = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, {stream: true});
        const blocks = buffer.split("\n\n");
        buffer = blocks.pop() || "";
        for (const block of blocks) {
          const event = parseSseBlock(block);
          if (!event || selected !== sessionId || tab !== "output") continue;
          if (event.name === "terminal") {
            const page = JSON.parse(event.data);
            $("panel").textContent = page.text || "";
          } else if (event.name === "error") {
            $("panel").textContent = event.data;
          }
        }
      }
    } catch (err) {
      if (err.name !== "AbortError" && selected === sessionId && tab === "output") {
        $("panel").textContent = err.message;
      }
    } finally {
      if (outputStream === controller) outputStream = null;
    }
  })();
}
function startStructuredStream(sessionId) {
  stopStructuredStream();
  if (!globalThis.ReadableStream || !globalThis.TextDecoder) {
    loadStructuredSnapshot(sessionId).catch((err) => {
      if (selected === sessionId && tab === "events") $("panel").textContent = err.message;
    });
    return;
  }

  const controller = new AbortController();
  structuredStream = controller;
  const rawEvents = [];
  const structuredEvents = [];
  const renderEvents = () => {
    if (selected === sessionId && tab === "events") {
      renderStructuredPanel(rawEvents, structuredEvents);
    }
  };
  const rawEventsLoad = api(`/api/sessions/${sessionId}/events?limit=200`)
    .then((events) => {
      rawEvents.splice(0, rawEvents.length, ...(events || []));
      renderEvents();
    })
    .catch(() => {});
  $("panel").textContent = "Loading structured events...";
  (async () => {
    try {
      const response = await fetch(`/api/sessions/${sessionId}/structured-stream?limit=200`, {
        headers: headers(),
        signal: controller.signal,
      });
      if (!response.ok) throw new Error((await response.text()) || response.statusText);
      if (!response.body) {
        await loadStructuredSnapshot(sessionId);
        return;
      }

      await rawEventsLoad;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      while (selected === sessionId && tab === "events") {
        const {value, done} = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, {stream: true});
        const blocks = buffer.split("\n\n");
        buffer = blocks.pop() || "";
        for (const block of blocks) {
          const event = parseSseBlock(block);
          if (!event || selected !== sessionId || tab !== "events") continue;
          if (event.name === "structured") {
            const events = JSON.parse(event.data);
            if (Array.isArray(events)) {
              structuredEvents.push(...events);
              if (structuredEvents.length > 200) {
                structuredEvents.splice(0, structuredEvents.length - 200);
              }
              renderEvents();
            }
          } else if (event.name === "error") {
            $("panel").textContent = event.data;
          }
        }
      }
      if (selected === sessionId && tab === "events" && structuredEvents.length === 0) {
        await loadStructuredSnapshot(sessionId);
      }
    } catch (err) {
      if (err.name !== "AbortError" && selected === sessionId && tab === "events") {
        try {
          await loadStructuredSnapshot(sessionId);
        } catch (_) {
          $("panel").textContent = err.message;
        }
      }
    } finally {
      if (structuredStream === controller) structuredStream = null;
    }
  })();
}
function slug(value) {
  const raw = String(value || "").toLowerCase();
  const slug = raw.replace(/[^a-z0-9_.-]+/g, "-").replace(/^-+|-+$/g, "");
  return slug || "session";
}
function projectName(path) {
  const parts = String(path || "").split(/[\\/]+/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : "session";
}
function generatedWorktreeBranch(agent, name, path) {
  const base = name && name.trim() ? name : projectName(path);
  const id = globalThis.crypto && globalThis.crypto.randomUUID
    ? globalThis.crypto.randomUUID().replace(/-/g, "")
    : String(Date.now());
  return `agent-helm/${slug(agent)}/${slug(base)}-${id}`;
}
function selectedToolProfile() {
  const agent = $("new-agent").value || defaultAgent;
  return toolProfiles.find((tool) => tool.name === agent);
}
function syncNewSessionWorktreeDefault() {
  const tool = selectedToolProfile();
  $("new-worktree").checked = tool ? tool.worktree === "always" : false;
  renderAgentProfileSummary();
}
function renderAgentProfileSummary() {
  const tool = selectedToolProfile();
  if (!tool) {
    $("agent-profile-summary").textContent = "";
    return;
  }
  const command = [tool.executable || tool.name, ...((tool.flags || []).filter(Boolean))].join(" ");
  $("agent-profile-summary").textContent = `${command} / worktree ${tool.worktree}`;
}
function renderAgentSelector(about) {
  defaultAgent = about.default_agent || "shell";
  if (about.default_path) {
    const pathInput = $("new-session").elements.path;
    if (!pathInput.value || pathInput.value === ".") pathInput.value = about.default_path;
  }
  if (about.default_session_name) {
    const nameInput = $("new-session").elements.name;
    nameInput.dataset.defaultName = about.default_session_name;
    if (!nameInput.value) nameInput.placeholder = about.default_session_name;
  }
  toolProfiles = Array.isArray(about.tools) ? about.tools : [];
  const agents = (Array.isArray(about.agents) && about.agents.length)
    ? about.agents
    : toolProfiles.filter((tool) => tool.installed).map((tool) => tool.name);
  const select = $("new-agent");
  select.innerHTML = "";
  for (const agent of agents) {
    const option = document.createElement("option");
    option.value = agent;
    option.textContent = agent;
    select.appendChild(option);
  }
  select.value = agents.includes(defaultAgent) ? defaultAgent : (agents[0] || "shell");
  syncNewSessionWorktreeDefault();
}
async function currentProjectContext() {
if (selected) {
const session = await api(`/api/sessions/${selected}`);
selectedProject = session.project_id;
return {projectId: session.project_id, session};
}
return {projectId: selectedProject, session: null};
}
function statusLabel(session, snapshot) {
if (!session) return "";
const status = snapshot ? snapshot.deck_status : session.status;
const activity = snapshot && snapshot.activity && snapshot.activity.label ? snapshot.activity.label : "";
return activity && activity.toLowerCase() !== status ? `${status} · ${activity}` : status;
}
function shortId(value) {
const text = String(value || "");
return text.length > 12 ? text.slice(0, 8) : text;
}
function sessionContextLabel(session) {
if (!session) return "";
const parts = [session.agent, session.group_name, session.project_path].filter(Boolean);
if (session.workspace_id) parts.push(`workspace ${shortId(session.workspace_id)}`);
if (session.worktree_id) parts.push(`worktree ${shortId(session.worktree_id)}`);
if (session.archived) parts.push("archived");
return parts.join(" · ");
}
const fleetStatusOrder = ["running", "queued", "waiting", "idle", "starting", "stopped", "errored"];
function countSessionsByDeckStatus(sessions, snapshots) {
const counts = Object.fromEntries(fleetStatusOrder.map((status) => [status, 0]));
for (const session of sessions) {
const snapshot = snapshots.get(session.id);
const status = String(snapshot ? snapshot.deck_status : session.status || "").toLowerCase();
if (Object.prototype.hasOwnProperty.call(counts, status)) counts[status] += 1;
}
return counts;
}
function renderFleetSummary(sessions, snapshots) {
const counts = countSessionsByDeckStatus(sessions, snapshots);
const parts = fleetStatusOrder
.filter((status) => counts[status] > 0)
.map((status) => `${status} ${counts[status]}`);
$("fleet-summary").textContent = parts.length ? parts.join(" · ") : "no sessions";
}
function renderSelected(session, snapshot) {
if (!session) stopOutputStream();
if (!session) stopStructuredStream();
if (!session) $("panel").textContent = "";
$("title").textContent = session ? session.name : "No session selected";
$("deck-status").textContent = statusLabel(session, snapshot);
$("session-context").textContent = sessionContextLabel(session);
}
function normalizeGroupName(name) {
const value = (name || "default").trim().replace(/^\/+|\/+$/g, "");
return value || "default";
}
function groupParts(name) {
return normalizeGroupName(name).split("/").filter(Boolean);
}
function groupParentName(name) {
const parts = groupParts(name);
parts.pop();
return parts.join("/");
}
function groupLeafName(name) {
const parts = groupParts(name);
return parts[parts.length - 1] || "default";
}
function extractPrNumber(value) {
const text = String(value || "");
const match = text.match(/(^|[^A-Za-z0-9])(?:#|pr[-_/:#]|pull[-_/:#])(\d+)/i);
if (!match) return null;
const number = Number(match[2]);
return Number.isSafeInteger(number) && number > 0 ? number : null;
}
function sessionPrNumber(session) {
for (const value of [session.name, session.group_name, session.project_path]) {
const number = extractPrNumber(value);
if (number) return number;
}
return null;
}
function sessionPrLabel(session) {
const number = sessionPrNumber(session);
return number ? `#${number}` : "";
}
function ensureGroupNode(nodes, name, record = null) {
const normalized = normalizeGroupName(name);
let node = nodes.get(normalized);
if (!node) {
node = {name: normalized, record: null, children: new Set(), sessions: []};
nodes.set(normalized, node);
}
if (record) node.record = record;
const parent = groupParentName(normalized);
if (parent) ensureGroupNode(nodes, parent).children.add(normalized);
return node;
}
function renderGroupRow(list, node, count, depth) {
const li = document.createElement("li");
li.className = "group-row";
li.dataset.groupName = node.name;
li.style.paddingLeft = `${8 + depth * 14}px`;
const row = document.createElement("div");
row.className = "row";
const name = document.createElement("strong");
const collapsed = node.record && node.record.collapsed;
name.textContent = `${collapsed ? "[+]" : "[-]"} ${groupLeafName(node.name)}`;
const pill = document.createElement("span");
pill.className = "pill";
pill.textContent = `${count} session${count === 1 ? "" : "s"}`;
li.setAttribute("aria-expanded", collapsed ? "false" : "true");
li.onclick = async () => {
if (!node.record || !canWrite()) return;
try {
await api(`/api/groups/${node.name}`, {method: "PATCH", body: JSON.stringify({collapsed: !collapsed})});
await loadSessions();
} catch (err) {
setStatus(err.message);
}
};
row.appendChild(name);
row.appendChild(pill);
li.appendChild(row);
list.appendChild(li);
}
function renderSessionRow(list, session, snapshot, depth) {
const rowSession = snapshot ? snapshot.session : session;
const deckStatus = statusLabel(rowSession, snapshot);
const lifecycle = snapshot && snapshot.lifecycle_status !== snapshot.deck_status ? ` / ${snapshot.lifecycle_status}` : "";
const li = document.createElement("li");
li.className = "session-row";
li.setAttribute("aria-selected", selected === rowSession.id);
li.style.paddingLeft = `${8 + depth * 14}px`;
const row = document.createElement("div");
row.className = "row";
const title = document.createElement("span");
title.className = "session-title";
const name = document.createElement("strong");
name.textContent = rowSession.name;
title.appendChild(name);
const prLabel = sessionPrLabel(rowSession);
if (prLabel) {
const pr = document.createElement("span");
pr.className = "pr-badge";
pr.textContent = prLabel;
title.appendChild(pr);
}
const pill = document.createElement("span");
pill.className = "pill";
pill.textContent = `${deckStatus}${lifecycle}`;
row.appendChild(title);
row.appendChild(pill);
const meta = document.createElement("div");
meta.className = "muted";
meta.textContent = rowSession.agent;
li.appendChild(row);
li.appendChild(meta);
li.onclick = () => {
selectSession(rowSession, snapshot);
loadSelectedSnapshot(rowSession.id);
loadPanel();
loadSessions();
};
list.appendChild(li);
return rowSession.id;
}
function renderGroupedSessionList(list, sessions, snapshots, groups) {
const nodes = new Map();
const renderedSessionIds = new Set();
const renderedSessionOrder = [];
for (const group of groups) ensureGroupNode(nodes, group.name, group);
for (const session of sessions) {
const snapshot = snapshots.get(session.id);
const rowSession = snapshot ? snapshot.session : session;
ensureGroupNode(nodes, rowSession.group_name).sessions.push(session);
}
const groupSort = (a, b) => {
const left = nodes.get(a);
const right = nodes.get(b);
const leftOrder = left && left.record ? left.record.display_order : 0;
const rightOrder = right && right.record ? right.record.display_order : 0;
return leftOrder === rightOrder ? a.localeCompare(b) : leftOrder - rightOrder;
};
const counts = new Map();
const countGroup = (name) => {
const node = nodes.get(name);
if (!node) return 0;
let count = node.sessions.length;
for (const child of [...node.children].sort(groupSort)) count += countGroup(child);
counts.set(name, count);
return count;
};
const renderGroup = (name, depth) => {
const node = nodes.get(name);
if (!node) return;
renderGroupRow(list, node, counts.get(name) || 0, depth);
if (node.record && node.record.collapsed) return;
for (const session of node.sessions) {
const renderedId = renderSessionRow(list, session, snapshots.get(session.id), depth + 1);
renderedSessionIds.add(renderedId);
renderedSessionOrder.push(renderedId);
}
for (const child of [...node.children].sort(groupSort)) renderGroup(child, depth + 1);
};
const roots = [...nodes.keys()].filter((name) => !groupParentName(name)).sort(groupSort);
for (const root of roots) countGroup(root);
for (const root of roots) renderGroup(root, 0);
visibleSessionIds = renderedSessionOrder;
return renderedSessionIds;
}
async function loadSelectedSnapshot(id) {
try {
const snapshot = await api(`/api/sessions/${id}/status-snapshot`);
if (selected === id) {
selectedProject = snapshot.session.project_id;
renderSelected(snapshot.session, snapshot);
}
} catch (err) {
setStatus(err.message);
}
}
async function loadRouteSelection() {
const id = routeSessionId();
if (!id) return;
try {
const session = await api(`/api/sessions/${id}`);
selectSession(session, null, false);
await loadSelectedSnapshot(id);
await loadPanel();
} catch (err) {
setStatus(err.message);
pushSessionRoute(null);
}
}
function setReadOnly(value) {
  readOnly = value;
  $("mode").textContent = value ? "read-only" : "read/write";
document.querySelectorAll("#new-session input, #new-session select, #new-session button, #project-add-form input, #project-add-form button, #send-form input, #send-form button, #session-event-form input, #session-event-form select, #session-event-form button, #move-group-form input, #move-group-form button, #fork-form input, #fork-form button, #project-form button, #worktree-create-form input, #worktree-create-form button, #worktree-finish-form input, #worktree-finish-form button, #group-create-form input, #group-create-form button, #group-update-form input, #group-update-form select, #group-update-form button, #group-delete-form input, #group-delete-form button, #mcp-form input, #mcp-form select, #mcp-form button, #skill-form input, #skill-form select, #skill-form button, #watcher-create-form input, #watcher-create-form select, #watcher-create-form button, #watcher-form input, #watcher-form button, #watcher-ingest-form input, #watcher-ingest-form select, #watcher-ingest-form button, #watcher-poll-all-form button, #cost-record-form input, #cost-record-form button, #conductor-form button, #conductor-action-form input[name='task'], #conductor-start, #conductor-heartbeat, #conductor-send, #conductor-stop, #conductor-remove, #conductor-assignment-status-form input, #conductor-assignment-status-form button, #start, #stop, #restart, #fork, #sync-state, #archive-reason, #archive-by, #archive, #restore, #remove-session, #remove-cleanup-worktree, #remove-purge")
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
renderAgentSelector(about);
setReadOnly(about.read_only);
} catch (err) {
setStatus(err.message);
}
}
async function loadProjects() {
try {
const projects = await api("/api/projects");
const options = $("project-options");
options.innerHTML = "";
for (const project of projects) {
const option = document.createElement("option");
option.value = project.id;
option.label = project.root_path;
options.appendChild(option);
}
} catch (err) {
setStatus(err.message);
}
}
async function loadSessions() {
  searchActive = false;
try {
const params = new URLSearchParams();
if ($("show-archived").checked) params.set("archived", "true");
const group = $("group-filter").value.trim();
if (group) params.set("group", group);
const status = $("status-filter").value;
if (status) params.set("deck_status", status);
    const suffix = params.toString();
const body = await api(suffix ? `/api/sessions?${suffix}` : "/api/sessions");
const visibleSessionIds = new Set(body.sessions.map((session) => session.id));
if (selected && !visibleSessionIds.has(selected)) {
clearSelectedSession();
}
const [snapshots, groups] = await Promise.all([
loadSessionSnapshots(body.sessions),
loadGroupRecords(),
]);
paletteSnapshots = snapshots;
paletteSessions = body.sessions.map((session) => {
const snapshot = snapshots.get(session.id);
return snapshot ? snapshot.session : session;
});
renderFleetSummary(body.sessions, snapshots);
const list = $("sessions");
list.innerHTML = "";
const renderedSessionIds = renderGroupedSessionList(list, body.sessions, snapshots, groups);
if (selected && !renderedSessionIds.has(selected)) {
clearSelectedSession();
}
setStatus(`${body.sessions.length} sessions`);
  } catch (err) {
setStatus(err.message);
}
}
async function loadGroupRecords() {
try {
const groups = await api("/api/groups");
return Array.isArray(groups) ? groups : [];
} catch (_) {
return [];
}
}
async function loadSessionSnapshots(sessions) {
const entries = await Promise.all(sessions.map(async (session) => {
try {
const snapshot = await api(`/api/sessions/${session.id}/status-snapshot`);
return [session.id, snapshot];
} catch (_) {
return [session.id, null];
}
}));
return new Map(entries);
}
async function refreshStatusSurfaces() {
if (document.hidden || statusRefreshRunning) return;
statusRefreshRunning = true;
try {
if (!searchActive) await loadSessions();
if (selected) await loadSelectedSnapshot(selected);
} catch (err) {
setStatus(err.message);
} finally {
statusRefreshRunning = false;
}
}
async function searchSessions(query) {
if (!query) {
searchActive = false;
await loadSessions();
return;
}
try {
searchActive = true;
const body = await api(`/api/search?q=${encodeURIComponent(query)}`);
  const list = $("sessions");
  list.innerHTML = "";
for (const result of body.results) {
const li = document.createElement("li");
li.textContent = `${result.session_name} [${result.source}] ${result.snippet}`;
li.onclick = async () => {
const session = await api(`/api/sessions/${result.session_id}`);
selectSession(session);
await loadSelectedSnapshot(session.id);
await loadPanel();
};
list.appendChild(li);
  }
  setStatus(`${body.results.length} matches`);
 } catch (err) {
  setStatus(err.message);
 }
}
async function loadPanel() {
  if (tab !== "output" || !selected) stopOutputStream();
  if (tab !== "events" || !selected) stopStructuredStream();
  if (!selected && tab !== "costs" && tab !== "attachments" && tab !== "managers" && !(tab === "worktrees" && selectedProject)) return;
  try {
    let body;
    if (tab === "diff") {
      body = await api(`/api/sessions/${selected}/diff`);
      $("panel").textContent = body.text || "(no diff)";
    } else if (tab === "events") {
      startStructuredStream(selected);
    } else if (tab === "costs") {
        const form = new FormData($("cost-filter-form"));
        const params = new URLSearchParams();
        const costFilterKeys = ["project_id", "group_name", "session_id", "agent", "model", "start_at", "end_at"];
        for (const key of costFilterKeys) {
          const value = form.get(key).trim();
          if (value) params.set(key, value);
        }
        const hasExplicitCostFilter = costFilterKeys.some((key) => params.has(key));
        if (selected && !hasExplicitCostFilter) params.set("session_id", selected);
        if (form.has("include_archived")) {
          params.set("include_archived", "true");
        } else {
          params.set("active_only", "true");
        }
      const [summary, events] = await Promise.all([
        api(`/api/costs?${params.toString()}`),
        api(`/api/cost-events?${params.toString()}`),
      ]);
      body = {summary, events};
      $("panel").textContent = JSON.stringify(body, null, 2);
} else if (tab === "attachments") {
const [mcp, skills, materialization] = await Promise.all([
api("/api/mcp"),
api("/api/skills"),
selected ? api(`/api/sessions/${selected}/materialization`) : Promise.resolve(null)
]);
const visible = selected
? {
mcp: mcp.filter((item) => item.session_id === selected),
skills: skills.filter((item) => item.session_id === selected),
materialization
}
: selectedProject
? {
mcp: mcp.filter((item) => item.project_id === selectedProject),
skills: skills.filter((item) => item.project_id === selectedProject)
}
: {mcp, skills};
 $("panel").textContent = JSON.stringify(visible, null, 2);
} else if (tab === "worktrees") {
const context = await currentProjectContext();
if (!context.projectId) return;
const workspaces = await api(`/api/projects/${context.projectId}/workspaces`);
const worktrees = await api(`/api/projects/${context.projectId}/worktrees`);
const selectedWorkspace = context.session && context.session.workspace_id
? await api(`/api/workspaces/${context.session.workspace_id}`)
: null;
const selectedWorktree = context.session && context.session.worktree_id
? await api(`/api/worktrees/${context.session.worktree_id}`)
: null;
$("panel").textContent = JSON.stringify({
project_id: context.projectId,
project_path: context.session ? context.session.project_path : "",
workspace_id: context.session ? context.session.workspace_id : null,
worktree_id: context.session ? context.session.worktree_id : null,
selected_workspace: selectedWorkspace,
selected_worktree: selectedWorktree,
workspaces,
worktrees
}, null, 2);
  } else if (tab === "managers") {
    const context = await currentProjectContext();
    const watcherPath = context.projectId ? `/api/projects/${context.projectId}/watchers` : "/api/watchers";
    const [watchers, conductors] = await Promise.all([api(watcherPath), api("/api/conductors")]);
    $("panel").textContent = JSON.stringify({watchers, conductors}, null, 2);
  } else {
    startOutputStream(selected);
  }
  } catch (err) {
    $("panel").textContent = err.message;
  }
}
async function refreshDashboard() {
try {
if (selected) {
const snapshot = await api(`/api/sessions/${selected}/status-snapshot`);
selectedProject = snapshot.session.project_id;
renderSelected(snapshot.session, snapshot);
}
await loadAbout();
await loadProjects();
if (!selected && routeSessionId()) await loadRouteSelection();
await loadSessions();
await loadPanel();
} catch (err) {
setStatus(err.message);
}
}
$("refresh").onclick = refreshDashboard;
$("show-archived").onchange = () => {
clearSelectedSession();
loadSessions();
};
$("command-palette").onclick = closeCommandPalette;
$("palette-panel").onclick = (event) => event.stopPropagation();
$("palette-input").oninput = renderCommandPalette;
$("palette-input").onkeydown = (event) => {
if (event.key === "Escape") {
event.preventDefault();
closeCommandPalette();
}
};
$("shortcut-help").onclick = closeShortcutHelp;
$("shortcut-panel").onclick = (event) => event.stopPropagation();
$("shortcut-close").onclick = closeShortcutHelp;
$("confirm-dialog").onclick = closeConfirmDialog;
$("confirm-panel").onclick = (event) => event.stopPropagation();
$("confirm-cancel").onclick = closeConfirmDialog;
$("confirm-accept").onclick = runConfirmedAction;
$("search-form").onsubmit = async (event) => {
event.preventDefault();
const query = new FormData(event.target).get("q").trim();
await searchSessions(query);
};
$("group-filter-form").onsubmit = async (event) => {
event.preventDefault();
await loadSessions();
};
$("status-filter-form").onsubmit = async (event) => {
  event.preventDefault();
  await loadSessions();
};
$("status-filter").onchange = () => {
  loadSessions();
};
$("new-agent").onchange = syncNewSessionWorktreeDefault;
$("cost-filter-form").onsubmit = async (event) => {
  event.preventDefault();
  tab = "costs";
  await loadPanel();
};
$("cost-record-form").onsubmit = async (event) => {
  event.preventDefault();
  if (!canWrite()) return;
  if (!selected) return;
  const data = new FormData(event.target);
  const amount = Number(data.get("amount_usd"));
  if (!Number.isFinite(amount) || amount < 0) return;
  const body = {amount_usd: amount};
  for (const key of ["model", "source"]) {
    const value = data.get(key).trim();
    if (value) body[key] = value;
  }
  try {
    for (const key of ["input_tokens", "output_tokens", "total_tokens"]) {
      const value = parseOptionalNonNegativeInteger(data, key);
      if (value !== null) body[key] = value;
    }
  } catch (err) {
    setStatus(err.message);
    return;
  }
  $("panel").textContent = JSON.stringify(await api(`/api/sessions/${selected}/costs`, {
    method: "POST",
    body: JSON.stringify(body)
  }), null, 2);
  tab = "costs";
  event.target.reset();
  await loadPanel();
};
$("group-create-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const name = data.get("name").trim();
if (!name) return;
const parent = data.get("parent").trim();
const defaultProjectPath = data.get("default_project_path").trim();
const body = {name};
if (parent) body.parent = parent;
if (defaultProjectPath) body.default_project_path = defaultProjectPath;
await api("/api/groups", {method: "POST", body: JSON.stringify(body)});
event.target.reset();
await loadSessions();
};
$("group-update-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const name = data.get("name").trim();
if (!name) return;
const defaultProjectPath = data.get("default_project_path").trim();
const collapsed = data.get("collapsed");
const body = {};
if (defaultProjectPath) body.default_project_path = defaultProjectPath;
if (data.get("clear_default_project_path") === "on") body.clear_default_project_path = true;
if (collapsed) body.collapsed = collapsed === "true";
await api(`/api/groups/${name}`, {method: "PATCH", body: JSON.stringify(body)});
event.target.reset();
await loadSessions();
};
$("group-delete-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const name = data.get("name").trim();
if (!name) return;
const suffix = data.get("force") === "on" ? "?force=true" : "";
const form = event.target;
confirmAction(`Delete group "${name}"?`, async () => {
await api(`/api/groups/${name}${suffix}`, {method: "DELETE"});
form.reset();
await loadSessions();
});
};
$("project-add-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const form = new FormData(event.target);
const path = form.get("path").trim();
if (!path) return;
const defaultBranch = form.get("default_branch").trim();
const body = {path, trusted: form.has("trusted")};
if (defaultBranch) body.default_branch = defaultBranch;
const project = await api("/api/projects", {method: "POST", body: JSON.stringify(body)});
selectedProject = project.id;
tab = "worktrees";
event.target.reset();
$("panel").textContent = JSON.stringify(project, null, 2);
await loadProjects();
};
$("project-select-form").onsubmit = async (event) => {
event.preventDefault();
selectedProject = new FormData(event.target).get("project").trim();
if (!selectedProject) return;
selected = null;
renderSelected(null);
tab = "worktrees";
event.target.reset();
await loadPanel();
};
$("new-session").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
  const form = new FormData(event.target);
  const data = Object.fromEntries(form);
  const nameInput = event.target.elements.name;
data.name = data.name.trim() || nameInput.dataset.defaultName || "web-session";
data.carry_state = form.has("carry_state");
data.sandbox = false;
const saveGroupDefault = form.has("save_group_default");
delete data.save_group_default;
delete data.use_worktree;
if (form.has("use_worktree")) {
data.worktree = generatedWorktreeBranch(data.agent, data.name, data.path);
}
try {
const session = await api("/api/sessions", {method: "POST", body: JSON.stringify(data)});
if (saveGroupDefault) {
await api(`/api/groups/${data.group_name}`, {method: "PATCH", body: JSON.stringify({default_project_path: data.path})});
}
selectSession(session);
await loadSessions();
await loadPanel();
} catch (err) {
    setStatus(err.message);
  }
};
$("new-agent").onchange = syncNewSessionWorktreeDefault;
$("send-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
if (!selected) return;
const text = new FormData(event.target).get("text");
await api(`/api/sessions/${selected}/send`, {method: "POST", body: JSON.stringify({text})});
event.target.reset();
await loadPanel();
};
$("session-event-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
if (!selected) return;
const data = new FormData(event.target);
const state = data.get("state");
const source = data.get("source").trim() || "manual";
const tool = data.get("tool").trim();
const body = {kind: "agent_state", state, source};
if (tool) body.tool = {name: tool};
await api(`/api/sessions/${selected}/events`, {
method: "POST",
body: JSON.stringify(body)
});
tab = "events";
await loadSelectedSnapshot(selected);
await loadPanel();
};
$("move-group-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
if (!selected) return;
const group_name = new FormData(event.target).get("group_name").trim();
if (!group_name) return;
const session = await api(`/api/sessions/${selected}/group`, {method: "POST", body: JSON.stringify({group_name})});
renderSelected(session);
await loadSessions();
};
$("fork-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
if (!selected) return;
const data = new FormData(event.target);
const body = {
carry_state: data.has("carry_state"),
start_immediately: !data.has("no_start")
};
for (const field of ["name", "group_name", "worktree_branch"]) {
const value = data.get(field).trim();
if (value) body[field] = value;
}
const fork = await api(`/api/sessions/${selected}/fork`, {method: "POST", body: JSON.stringify(body)});
selected = fork.child_session_id;
pushSessionRoute(selected);
event.target.reset();
await loadSessions();
await loadPanel();
};
$("project-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const context = await currentProjectContext();
if (!context.projectId) return;
const action = event.submitter ? event.submitter.value : "trust";
if (action === "remove") {
confirmAction(`Remove project ${context.projectId}?`, async () => {
await api(`/api/projects/${context.projectId}`, {method: "DELETE"});
clearSelectedSession();
await loadSessions();
$("panel").textContent = "";
});
return;
}
let path = `/api/projects/${context.projectId}/trust`;
if (action === "untrust") path = `/api/projects/${context.projectId}/untrust`;
if (action === "cleanup") path = `/api/projects/${context.projectId}/worktrees/cleanup`;
await api(path, {method: "POST", body: "{}"});
tab = "worktrees";
await loadPanel();
};
$("worktree-create-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const form = new FormData(event.target);
const branch = form.get("branch").trim();
if (!branch) return;
const carryState = form.has("carry_state");
const context = await currentProjectContext();
if (!context.projectId) return;
await api(`/api/projects/${context.projectId}/worktrees`, {
method: "POST",
body: JSON.stringify({branch, carry_state: carryState})
});
event.target.reset();
tab = "worktrees";
await loadPanel();
};
$("worktree-finish-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const id = new FormData(event.target).get("id").trim();
if (!id) return;
const worktreeId = id;
const form = event.target;
confirmAction(`Finish worktree ${worktreeId}?`, async () => {
await api(`/api/worktrees/${worktreeId}/finish`, {method: "POST", body: "{}"});
form.reset();
tab = "worktrees";
await loadPanel();
});
};
$("mcp-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const id = data.get("id").trim();
const action = event.submitter ? event.submitter.value : "attach";
if (action === "sync") {
await api("/api/mcp/sync", {method: "POST", body: "{}"});
} else if (action === "detach") {
if (!id) return;
await api(`/api/mcp/${id}/detach`, {method: "POST", body: "{}"});
} else {
if (!id) return;
const scope = data.get("scope");
if (scope === "profile") {
await api("/api/profile/mcp", {method: "POST", body: JSON.stringify({id})});
} else if (scope === "project") {
const context = await currentProjectContext();
if (!context.projectId) return;
await api(`/api/projects/${context.projectId}/mcp`, {method: "POST", body: JSON.stringify({id})});
} else {
if (!selected) return;
await api(`/api/sessions/${selected}/mcp`, {method: "POST", body: JSON.stringify({id})});
}
}
event.target.reset();
tab = "attachments";
await loadPanel();
};
$("skill-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const id = data.get("id").trim();
const action = event.submitter ? event.submitter.value : "attach";
if (action === "sync") {
await api("/api/skills/sync", {method: "POST", body: "{}"});
} else if (action === "detach") {
if (!id) return;
await api(`/api/skills/${id}/detach`, {method: "POST", body: "{}"});
} else {
if (!id) return;
const scope = data.get("scope");
if (scope === "profile") {
await api("/api/profile/skills", {method: "POST", body: JSON.stringify({id})});
} else if (scope === "project") {
const context = await currentProjectContext();
if (!context.projectId) return;
await api(`/api/projects/${context.projectId}/skills`, {method: "POST", body: JSON.stringify({id})});
} else {
if (!selected) return;
await api(`/api/sessions/${selected}/skills`, {method: "POST", body: JSON.stringify({id})});
}
}
event.target.reset();
tab = "attachments";
await loadPanel();
};
$("watcher-create-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const name = data.get("name").trim();
const adapter = data.get("adapter") || "shell";
const command = data.get("command").trim();
const timeout = Number(data.get("timeout_ms"));
const requireSignature = data.get("require_signature") === "on";
if (!name) return;
const context = await currentProjectContext();
const config = {};
if (requireSignature) config.require_signature = true;
if (adapter === "shell") {
if (!command || !context.projectId) return;
config.command = command;
}
if (selected) config.session_id = selected;
if (Number.isFinite(timeout) && timeout > 0) config.timeout_ms = timeout;
const body = {name, adapter_id: adapter, config};
if (context.projectId) body.project_id = context.projectId;
await api("/api/watchers", {
method: "POST",
body: JSON.stringify(body)
});
event.target.reset();
tab = "managers";
await loadPanel();
};
$("watcher-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const id = new FormData(event.target).get("id").trim();
if (!id) return;
const action = event.submitter ? event.submitter.value : "start";
if (action === "remove") {
const watcherId = id;
const form = event.target;
confirmAction(`Remove watcher ${watcherId}?`, async () => {
await api(`/api/watchers/${watcherId}`, {method: "DELETE"});
form.reset();
tab = "managers";
await loadPanel();
});
return;
} else {
await api(`/api/watchers/${id}/${action}`, {method: "POST", body: "{}"});
}
event.target.reset();
tab = "managers";
await loadPanel();
};
$("watcher-events-form").onsubmit = async (event) => {
event.preventDefault();
const data = new FormData(event.target);
const id = data.get("id").trim();
if (!id) return;
const params = new URLSearchParams();
for (const key of ["offset", "limit"]) {
const value = data.get(key).trim();
if (value) params.set(key, value);
}
const suffix = params.toString() ? `?${params}` : "";
$("panel").textContent = JSON.stringify(await api(`/api/watchers/${id}/events${suffix}`), null, 2);
tab = "managers";
event.target.reset();
};
$("watcher-ingest-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const id = data.get("id").trim();
const payload = data.get("payload").trim();
const event_type = data.get("event_type").trim() || "event";
const signature_status = data.get("signature_status") || "not_applicable";
if (!id || !payload) return;
$("panel").textContent = JSON.stringify(await api(`/api/watchers/${id}/events`, {
method: "POST",
body: JSON.stringify({payload, event_type, signature_status})
}), null, 2);
tab = "managers";
event.target.reset();
};
$("watcher-poll-all-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
$("panel").textContent = JSON.stringify(await api("/api/watchers/poll", {method: "POST", body: "{}"}), null, 2);
tab = "managers";
};
$("conductor-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
if (!selected) return;
await api(`/api/sessions/${selected}/conductor`, {method: "POST", body: "{}"});
tab = "managers";
await loadPanel();
};
$("conductor-action-form").onsubmit = async (event) => {
event.preventDefault();
const data = new FormData(event.target);
const id = data.get("id").trim();
if (!id) return;
const action = event.submitter ? event.submitter.value : "start";
if (action === "status") {
$("panel").textContent = JSON.stringify(await api(`/api/conductors/${id}`), null, 2);
tab = "managers";
return;
}
if (!canWrite()) return;
let body = {};
if (action === "remove") {
const conductorId = id;
const form = event.target;
confirmAction(`Remove conductor ${conductorId}?`, async () => {
await api(`/api/conductors/${conductorId}`, {method: "DELETE"});
form.reset();
tab = "managers";
await loadPanel();
});
return;
}
if (action === "send") {
if (!selected) return;
const task = data.get("task").trim();
if (!task) return;
body = {session_id: selected, task_ref: task};
}
$("panel").textContent = JSON.stringify(await api(`/api/conductors/${id}/${action}`, {
method: "POST",
body: JSON.stringify(body)
}), null, 2);
event.target.reset();
tab = "managers";
};
$("conductor-assignments-form").onsubmit = async (event) => {
event.preventDefault();
const id = new FormData(event.target).get("id").trim();
if (!id) return;
$("panel").textContent = JSON.stringify(await api(`/api/conductors/${id}/assignments`), null, 2);
tab = "managers";
event.target.reset();
};
$("conductor-assignment-status-form").onsubmit = async (event) => {
event.preventDefault();
if (!canWrite()) return;
const data = new FormData(event.target);
const id = data.get("id").trim();
const status = event.submitter ? event.submitter.value : data.get("status");
if (!id || !status) return;
$("panel").textContent = JSON.stringify(await api(`/api/conductor-assignments/${id}`, {
method: "PATCH",
body: JSON.stringify({status})
}), null, 2);
tab = "managers";
event.target.reset();
};
for (const id of ["start", "stop", "restart", "fork", "sync-state", "archive", "restore"]) {
$(id).onclick = async () => {
if (!canWrite()) return;
if (!selected) return;
const body = {};
    if (id === "archive") {
      const reason = $("archive-reason").value.trim();
      const archivedBy = $("archive-by").value.trim();
if (reason) body.reason = reason;
if (archivedBy) body.archived_by = archivedBy;
}
const sessionId = selected;
const runAction = async () => {
const result = await api(`/api/sessions/${sessionId}/${id}`, {method: "POST", body: JSON.stringify(body)});
if (id === "fork") selected = result.child_session_id;
if (id === "sync-state") setStatus(result.synced ? `synced ${result.source}` : `no ${result.source} update`);
if (id === "archive") {
$("archive-reason").value = "";
      $("archive-by").value = "";
    }
await loadSessions();
if (selected) await loadSelectedSnapshot(selected);
await loadPanel();
};
if (id === "archive") {
confirmAction(`Archive session ${sessionId}?`, runAction);
return;
}
await runAction();
};
}
$("remove-session").onclick = async () => {
if (!canWrite()) return;
if (!selected) return;
const sessionId = selected;
const cleanup = $("remove-cleanup-worktree").checked;
const purge = $("remove-purge").checked;
const suffix = purge ? "?purge=true" : (cleanup ? "?cleanup_worktree=true" : "");
confirmAction(`Remove session ${sessionId}?`, async () => {
await api(`/api/sessions/${sessionId}${suffix}`, {method: "DELETE"});
$("remove-cleanup-worktree").checked = false;
$("remove-purge").checked = false;
if (selected === sessionId) clearSelectedSession();
await loadSessions();
await loadPanel();
});
};
for (const button of document.querySelectorAll(".tabs button")) {
button.onclick = () => setActiveTab(button.dataset.tab);
}
if (document.addEventListener) document.addEventListener("keydown", (event) => {
const target = event.target || {};
const tag = target.tagName ? target.tagName.toLowerCase() : "";
const typing = target.isContentEditable || ["input", "select", "textarea"].includes(tag);
if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
event.preventDefault();
openCommandPalette();
return;
}
if (event.key === "Escape" && !$("command-palette").hidden) {
event.preventDefault();
closeCommandPalette();
return;
}
if (event.key === "Escape" && !$("shortcut-help").hidden) {
event.preventDefault();
closeShortcutHelp();
return;
}
if (event.key === "Escape" && !$("confirm-dialog").hidden) {
event.preventDefault();
closeConfirmDialog();
return;
}
if (!$("command-palette").hidden || !$("shortcut-help").hidden || !$("confirm-dialog").hidden) return;
if (!typing && event.key === "?") {
event.preventDefault();
openShortcutHelp();
return;
}
if (!typing && event.key === "j") {
event.preventDefault();
selectVisibleSessionByOffset(1);
return;
}
if (!typing && event.key === "k") {
event.preventDefault();
selectVisibleSessionByOffset(-1);
return;
}
if (!typing && event.key === "Enter") {
event.preventDefault();
if (event.shiftKey) openSelectedSessionInNewTab();
else openSelectedSession();
return;
}
if (!typing && event.key.toLowerCase() === "a") {
event.preventDefault();
toggleArchivedFilter();
return;
}
if (!typing && event.key.toLowerCase() === "t") {
event.preventDefault();
cycleStatusFilter();
return;
}
if (!typing && event.key === "/") {
event.preventDefault();
$("search-form").elements.q.focus();
return;
}
if (!typing && event.key.toLowerCase() === "n" && !readOnly) {
event.preventDefault();
$("new-session").querySelector("input[name='name']").focus();
}
});
if (typeof setInterval === "function") setInterval(refreshStatusSurfaces, 3000);
if (document.addEventListener) document.addEventListener("visibilitychange", () => {
if (!document.hidden) refreshStatusSurfaces();
});
if (window.addEventListener) window.addEventListener("popstate", async () => {
const id = routeSessionId();
if (!id) {
clearSelectedSession(false);
await loadSessions();
await loadPanel();
return;
}
try {
const session = await api(`/api/sessions/${id}`);
selectSession(session, null, false);
await loadSelectedSnapshot(id);
await loadSessions();
await loadPanel();
} catch (err) {
setStatus(err.message);
}
});
loadAbout().then(async () => { await loadProjects(); await loadRouteSelection(); await loadSessions(); });
</script>
</body>
</html>"##;

async fn web_dashboard() -> Html<&'static str> {
    Html(WEB_DASHBOARD_HTML)
}

async fn about<C>(State(state): State<ApiState<C>>) -> ApiResult<Json<AboutResponse>>
where
    C: AgentHelmApi,
{
    let tools = state.controller.tool_profiles()?;
    let agents = tools
        .iter()
        .filter(|tool| tool.installed)
        .map(|tool| tool.name.clone())
        .collect();
    Ok(Json(AboutResponse {
        name: "agent-helm",
        version: env!("CARGO_PKG_VERSION"),
        read_only: state.read_only,
        default_agent: state.controller.default_agent()?,
        default_path: env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .to_string_lossy()
            .to_string(),
        default_session_name: generated_session_name(),
        agents,
        tools,
    }))
}

fn generated_session_name() -> String {
    Generator::with_naming(Name::Plain)
        .next()
        .unwrap_or_else(|| "agent-helm-session".to_string())
}

async fn list_sessions<C>(
    State(state): State<ApiState<C>>,
    Query(query): Query<ListSessionsQuery>,
) -> ApiResult<Json<SessionsResponse>>
where
    C: AgentHelmApi,
{
    let status = query.status.as_deref().map(normalize_status_filter);
    let deck_status = query.deck_status.as_deref().map(normalize_status_filter);
    let mut sessions = Vec::new();
    for session in state
        .controller
        .list_sessions(query.all || query.archived)?
    {
        if query.archived && !session.archived {
            continue;
        }
        if query
            .group
            .as_deref()
            .is_some_and(|group| session.group_name != group)
        {
            continue;
        }
        if status.is_none() && deck_status.is_none() {
            sessions.push(session);
            continue;
        }
        let snapshot = state.controller.status_snapshot(&session.id)?;
        if status.as_deref().is_some_and(|status| {
            !session_status_filter_matches(
                snapshot.lifecycle_status.as_str(),
                snapshot.deck_status.as_str(),
                status,
            )
        }) {
            continue;
        }
        if deck_status
            .as_deref()
            .is_some_and(|deck_status| snapshot.deck_status.as_str() != deck_status)
        {
            continue;
        }
        sessions.push(snapshot.session);
    }
    Ok(Json(SessionsResponse { sessions }))
}

fn normalize_status_filter(status: &str) -> String {
    status.trim().to_ascii_lowercase()
}

fn session_status_filter_matches(lifecycle_status: &str, deck_status: &str, status: &str) -> bool {
    let status = normalize_status_filter(status);
    lifecycle_status == status || deck_status == status
}

async fn search_sessions<C>(
    State(state): State<ApiState<C>>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<SessionSearchResponse>>
where
    C: AgentHelmApi,
{
    let limit = query.limit.unwrap_or(20);
    let query = query.query.or(query.q).unwrap_or_default();
    Ok(Json(state.controller.search_sessions(&query, limit)?))
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

async fn session_status<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionStatusSnapshot>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.status_snapshot(&id)?))
}

async fn session_status_snapshot<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionStatusSnapshot>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.status_snapshot(&id)?))
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
        start_immediately: request.start_immediately,
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

async fn restore_session<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.restore_session(&id)?))
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

async fn session_materialization<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionMaterializationPlan>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.session_materialization(&id)?))
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

async fn session_events<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<Vec<SessionEvent>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.events(
        &id,
        query.since.unwrap_or(0),
        query.limit.unwrap_or(200),
    )?))
}

async fn record_session_event<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<RecordSessionEventRequest>,
) -> ApiResult<Json<SessionEvent>>
where
    C: AgentHelmApi,
{
    let (kind, payload) = request.into_parts()?;
    match state.controller.record_session_event(&id, &kind, payload) {
        Ok(event) => Ok(Json(event)),
        Err(error) if is_event_validation_error(&error) => {
            Err(ApiError::bad_request(error.message))
        }
        Err(error) => Err(error),
    }
}

fn is_event_validation_error(error: &ApiError) -> bool {
    error.message.starts_with("agent_state ") || error.message.starts_with("only agent_state")
}

async fn sync_agent_state<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<AgentStateSyncResult>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.sync_agent_state(&id)?))
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

async fn create_group<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Json(request): Json<CreateGroupRequest>,
) -> ApiResult<Json<GroupRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.create_group(
        request.name,
        request.parent,
        request.default_project_path,
    )?))
}

async fn update_group<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(name): Path<String>,
    Json(request): Json<UpdateGroupRequest>,
) -> ApiResult<Json<GroupRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.update_group(
        &name,
        request.default_project_path,
        request.clear_default_project_path,
        request.collapsed,
    )?))
}

async fn delete_group<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(name): Path<String>,
    Query(query): Query<DeleteGroupQuery>,
) -> ApiResult<StatusCode>
where
    C: AgentHelmApi,
{
    state.controller.delete_group(&name, query.force)?;
    Ok(StatusCode::NO_CONTENT)
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

async fn trust_project<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.set_project_trust(&id, true)?))
}

async fn untrust_project<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.set_project_trust(&id, false)?))
}

async fn delete_project<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<StatusCode>
where
    C: AgentHelmApi,
{
    state.controller.remove_project(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_workspaces<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<WorkspaceRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_workspaces(&id)?))
}

async fn get_workspace<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorkspaceRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.get_workspace(&id)?))
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

async fn get_worktree<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<WorktreeRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.get_worktree(&id)?))
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
    Ok(Json(state.controller.create_worktree(
        &id,
        &request.branch,
        request.carry_state,
    )?))
}

async fn cleanup_worktrees<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<CleanupReport>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.cleanup_worktrees(&id)?))
}

async fn finish_worktree<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<WorktreeRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.finish_worktree(&id)?))
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

async fn attach_project_mcp<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<AttachmentRequest>,
) -> ApiResult<Json<McpAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.attach_project_mcp(&id, request.id)?))
}

async fn attach_profile_mcp<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Json(request): Json<AttachmentRequest>,
) -> ApiResult<Json<McpAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.attach_profile_mcp(request.id)?))
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

async fn attach_project_skill<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<AttachmentRequest>,
) -> ApiResult<Json<SkillAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(
        state.controller.attach_project_skill(&id, request.id)?,
    ))
}

async fn attach_profile_skill<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Json(request): Json<AttachmentRequest>,
) -> ApiResult<Json<SkillAttachmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.attach_profile_skill(request.id)?))
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

async fn list_project_watchers<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<WatcherRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_project_watchers(&id)?))
}

async fn watcher_events<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
    Query(query): Query<WatcherEventsQuery>,
) -> ApiResult<Json<Vec<WatcherEventRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_watcher_events(
        &id,
        query.offset.unwrap_or(0),
        query.limit.unwrap_or(50),
    )?))
}

async fn create_watcher<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Json(request): Json<CreateWatcherRequest>,
) -> ApiResult<Json<WatcherRecord>>
where
    C: AgentHelmApi,
{
    let config_ref = request
        .config_ref
        .or_else(|| request.config.map(|config| config.to_string()))
        .unwrap_or_else(|| "{}".to_string());
    Ok(Json(state.controller.create_watcher(
        request.name,
        request.adapter_id.unwrap_or_else(|| "manual".to_string()),
        request.project_id,
        config_ref,
    )?))
}

async fn test_watcher<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<WatcherRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.test_watcher(&id)?))
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

async fn poll_watcher<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<WatcherRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.poll_watcher(&id)?))
}

async fn poll_running_watchers<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
) -> ApiResult<Json<Vec<WatcherRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.poll_running_watchers()?))
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

async fn delete_watcher<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<StatusCode>
where
    C: AgentHelmApi,
{
    state.controller.delete_watcher(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ingest_watcher_event<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<IngestWatcherEventRequest>,
) -> ApiResult<Json<WatcherEventRecord>>
where
    C: AgentHelmApi,
{
    let payload_ref = request
        .payload_ref
        .or_else(|| {
            request.payload.map(|payload| {
                payload
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| payload.to_string())
            })
        })
        .unwrap_or_default();
    Ok(Json(
        state.controller.ingest_watcher_event(
            &id,
            request.source.unwrap_or_else(|| "external".to_string()),
            request.event_type.unwrap_or_else(|| "event".to_string()),
            payload_ref,
            request
                .signature_status
                .unwrap_or_else(|| "not_applicable".to_string()),
        )?,
    ))
}

async fn list_conductors<C>(
    State(state): State<ApiState<C>>,
) -> ApiResult<Json<Vec<ConductorRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_conductors()?))
}

async fn create_conductor<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<Json<ConductorRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.create_conductor(id)?))
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

async fn delete_conductor<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
) -> ApiResult<StatusCode>
where
    C: AgentHelmApi,
{
    state.controller.delete_conductor(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn conductor_assignments<C>(
    State(state): State<ApiState<C>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ConductorAssignmentRecord>>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.list_conductor_assignments(&id)?))
}

async fn complete_conductor_assignment<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<CompleteConductorAssignmentRequest>,
) -> ApiResult<Json<ConductorAssignmentRecord>>
where
    C: AgentHelmApi,
{
    Ok(Json(state.controller.complete_conductor_assignment(
        &id,
        request.status.as_deref().unwrap_or("completed"),
    )?))
}

async fn update_conductor_assignment<C>(
    State(state): State<ApiState<C>>,
    _writable: Writable,
    Path(id): Path<String>,
    Json(request): Json<CompleteConductorAssignmentRequest>,
) -> ApiResult<Json<ConductorAssignmentRecord>>
where
    C: AgentHelmApi,
{
    let Some(status) = request.status.as_deref() else {
        return Err(ApiError::bad_request("assignment status is required"));
    };
    Ok(Json(
        state
            .controller
            .complete_conductor_assignment(&id, status)?,
    ))
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
    match state.controller.record_cost(
        &id,
        request.amount_usd,
        serde_json::json!({
            "model": request.model,
            "input_tokens": request.input_tokens.unwrap_or(0),
            "output_tokens": request.output_tokens.unwrap_or(0),
            "total_tokens": request.total_tokens.unwrap_or(0),
            "source": request.source.unwrap_or_else(|| "api".to_string()),
        }),
    ) {
        Ok(event) => Ok(Json(event)),
        Err(error) if is_cost_validation_error(&error) => Err(ApiError::bad_request(error.message)),
        Err(error) => Err(error),
    }
}

fn is_cost_validation_error(error: &ApiError) -> bool {
    error.message.starts_with("cost ")
}

async fn costs<C>(
    State(state): State<ApiState<C>>,
    Query(query): Query<CostsQuery>,
) -> ApiResult<Json<CostSummary>>
where
    C: AgentHelmApi,
{
    let now = now_ts();
    let include_archived = query.include_archived();
    Ok(Json(state.controller.cost_summary(CostFilter {
        profile: String::new(),
        project_id: query.project_id,
        group_name: query.group_name,
        session_id: query.session_id,
        agent: query.agent,
        model: query.model,
        start_at: query.start_at.unwrap_or(0),
        end_at: query.end_at.unwrap_or(now),
        include_archived,
    })?))
}

async fn cost_events<C>(
    State(state): State<ApiState<C>>,
    Query(query): Query<CostEventsQuery>,
) -> ApiResult<Json<Vec<CostEvent>>>
where
    C: AgentHelmApi,
{
    let now = now_ts();
    let include_archived = query.include_archived();
    Ok(Json(state.controller.cost_events(
        CostFilter {
            profile: String::new(),
            project_id: query.project_id,
            group_name: query.group_name,
            session_id: query.session_id,
            agent: query.agent,
            model: query.model,
            start_at: query.start_at.unwrap_or(0),
            end_at: query.end_at.unwrap_or(now),
            include_archived,
        },
        query.offset,
        query.limit.unwrap_or(50),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_errors_with_not_found_messages_map_to_404() {
        let error: ApiError = AppError::msg("session not found: missing").into();

        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.code, "not_found");
        assert_eq!(error.message, "session not found: missing");
    }

    #[test]
    fn other_app_errors_stay_internal() {
        let error: ApiError = AppError::msg("runtime failed").into();

        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.code, "internal_error");
    }

    #[test]
    fn cost_queries_include_archived_by_default_with_active_only_override() {
        let costs: CostsQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(costs.include_archived());

        let explicit_active: CostsQuery =
            serde_json::from_value(serde_json::json!({"include_archived": false})).unwrap();
        assert!(!explicit_active.include_archived());

        let active_only: CostsQuery =
            serde_json::from_value(serde_json::json!({"active_only": true})).unwrap();
        assert!(!active_only.include_archived());

        let events: CostEventsQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(events.include_archived());

        let active_only_events: CostEventsQuery =
            serde_json::from_value(serde_json::json!({"active_only": true})).unwrap();
        assert!(!active_only_events.include_archived());
    }

    #[test]
    fn cost_validation_errors_are_client_errors() {
        let error: ApiError =
            AppError::msg("cost input_tokens must be a non-negative integer").into();
        assert!(is_cost_validation_error(&error));

        let error = ApiError::bad_request(error.message);
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, "bad_request");
    }

    #[test]
    fn session_status_filter_matches_lifecycle_or_deck_status() {
        assert!(session_status_filter_matches(
            "running", "waiting", "running"
        ));
        assert!(session_status_filter_matches(
            "running", "waiting", "waiting"
        ));
        assert!(session_status_filter_matches("running", "idle", " Idle "));
        assert!(!session_status_filter_matches("running", "idle", "waiting"));
    }
}
