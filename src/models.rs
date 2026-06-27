use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr};

/// Parse a snake_case string into an enum variant, returning `default` on mismatch.
macro_rules! from_db {
    ($ty:ty, $value:expr, $default:expr) => {
        <$ty as std::str::FromStr>::from_str($value).unwrap_or($default)
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Stopped,
    Errored,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::Stopped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionDeckStatus {
    Starting,
    Running,
    Queued,
    Waiting,
    Idle,
    Stopped,
    Errored,
}

impl SessionDeckStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDeckStatusDerivation {
    pub deck_status: SessionDeckStatus,
    pub source_event_id: Option<i64>,
    pub source: String,
    pub activity: Option<SessionActivity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatusSnapshot {
    pub session: SessionRecord,
    pub lifecycle_status: SessionStatus,
    pub deck_status: SessionDeckStatus,
    pub source_event_id: Option<i64>,
    pub source: String,
    pub activity: Option<SessionActivity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    pub state: String,
    pub label: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    pub profile: String,
    pub group_name: String,
    pub project_id: String,
    pub workspace_id: String,
    pub worktree_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub agent: String,
    pub command: String,
    pub project_path: String,
    pub status: SessionStatus,
    pub runtime_id: Option<String>,
    pub archived: bool,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub id: i64,
    pub session_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredEvent {
    pub id: i64,
    pub session_id: String,
    pub kind: String,
    pub source: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateSyncResult {
    pub session_id: String,
    pub synced: bool,
    pub source: String,
    pub event: Option<SessionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSearchResponse {
    pub query: String,
    pub results: Vec<SessionSearchResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSearchResult {
    pub session_id: String,
    pub session_name: String,
    pub group_name: String,
    pub agent: String,
    pub cwd: String,
    pub source: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRecord {
    pub id: String,
    pub profile: String,
    pub name: String,
    pub default_project_path: String,
    pub collapsed: bool,
    pub display_order: i64,
    pub metadata: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProjectTrustState {
    Trusted,
    Untrusted,
}

impl ProjectTrustState {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::Untrusted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSpec {
    pub profile: String,
    pub root_path: String,
    pub default_branch: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: String,
    pub profile: String,
    pub root_path: String,
    pub repo_identity: String,
    pub default_branch: String,
    pub trust_state: ProjectTrustState,
    pub hooks_hash: String,
    pub config_hash: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    pub project_id: String,
    pub path: String,
    pub worktree_id: Option<String>,
    pub sandbox_id: Option<String>,
    pub multi_repo_roots: String,
    pub cleanup_policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: String,
    pub project_id: String,
    pub path: String,
    pub worktree_id: Option<String>,
    pub sandbox_id: Option<String>,
    pub multi_repo_roots: String,
    pub cleanup_policy: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WorktreeStatus {
    Creating,
    Ready,
    Finished,
    Errored,
}

impl WorktreeStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::Errored)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRequest {
    pub project_id: String,
    pub branch: String,
    pub base_branch: String,
    pub path: String,
    pub carry_state: bool,
    pub include_ignored: bool,
    pub run_setup_hooks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: String,
    pub project_id: String,
    pub path: String,
    pub branch: String,
    pub base_branch: String,
    pub status: WorktreeStatus,
    pub cleanup_allowed: bool,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupReport {
    pub project_id: String,
    pub inspected: usize,
    pub removed: usize,
    pub missing: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSessionRequest {
    pub parent_session_id: String,
    pub name: Option<String>,
    pub group_name: Option<String>,
    pub worktree_branch: Option<String>,
    pub carry_state: bool,
    pub start_immediately: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSessionResult {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub workspace_id: String,
    pub worktree_id: Option<String>,
    pub started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DeleteMode {
    MetadataOnly,
    CleanupWorktree,
    Purge,
}

impl DeleteMode {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::MetadataOnly)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSessionRequest {
    pub session_id: String,
    pub mode: DeleteMode,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletionResult {
    pub session_id: String,
    pub deleted: bool,
    pub runtime_stopped: bool,
    pub worktree_cleaned: bool,
    pub purged: bool,
    pub history_retained: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveSessionRequest {
    pub session_id: String,
    pub archived_by: String,
    pub reason: String,
    pub stop_if_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveSessionResult {
    pub session_id: String,
    pub archived: bool,
    pub runtime_stopped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AttachmentStatus {
    Pending,
    Attached,
    Detached,
    Errored,
}

impl AttachmentStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::Pending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAttachmentRecord {
    pub id: String,
    pub profile: String,
    pub scope: String,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub server_id: String,
    pub status: AttachmentStatus,
    pub materialized_state: String,
    pub restart_required: bool,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillAttachmentRecord {
    pub id: String,
    pub profile: String,
    pub scope: String,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub skill_id: String,
    pub pool_path: String,
    pub materialized_path: String,
    pub status: AttachmentStatus,
    pub restart_required: bool,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WatcherStatus {
    Starting,
    Running,
    Stopped,
    Errored,
}

impl WatcherStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::Stopped)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatcherRecord {
    pub id: String,
    pub profile: String,
    pub project_id: Option<String>,
    pub adapter_id: String,
    pub name: String,
    pub config_ref: String,
    pub status: WatcherStatus,
    pub last_event_at: i64,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatcherEventRecord {
    pub id: String,
    pub watcher_id: String,
    pub source: String,
    pub event_type: String,
    pub payload_ref: String,
    pub signature_status: String,
    pub route_decision: String,
    pub delivered: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ConductorStatus {
    Starting,
    Running,
    Stopped,
    Errored,
}

impl ConductorStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn from_db(value: &str) -> Self {
        from_db!(Self, value, Self::Stopped)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConductorRecord {
    pub id: String,
    pub profile: String,
    pub session_id: String,
    pub status: ConductorStatus,
    pub watched_sessions: String,
    pub channel_bindings: String,
    pub last_heartbeat_at: i64,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConductorAssignmentRecord {
    pub id: String,
    pub conductor_id: String,
    pub session_id: String,
    pub task_ref: String,
    pub status: String,
    pub assigned_at: i64,
    pub completed_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostFilter {
    pub profile: String,
    pub project_id: Option<String>,
    pub group_name: Option<String>,
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub start_at: i64,
    pub end_at: i64,
    pub include_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummary {
    pub profile: String,
    pub total_cost_micros: i64,
    pub event_count: i64,
    pub session_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub start_at: i64,
    pub end_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEvent {
    pub id: i64,
    pub session_id: String,
    pub amount_usd: f64,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub cwd: String,
    pub command: String,
    pub sandbox: Option<SandboxLaunchSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxLaunchSpec {
    pub image: String,
    pub container_cwd: String,
    pub allowed_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHandle {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputPage {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSession {
    pub path: String,
    pub agent: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub name: String,
    pub group_name: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub carry_state: bool,
    #[serde(default)]
    pub sandbox: bool,
    pub prompt: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
