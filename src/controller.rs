use crate::{
    adapter::{AgentRegistry, TranscriptEntry},
    config::AppConfig,
    error::{AppError, Result},
    materialization::{
        SessionMaterializationPlan, session_materialization_json, session_materialization_plan,
    },
    models::{
        AgentStateSyncResult, AttachmentStatus, CleanupReport, ConductorAssignmentRecord,
        ConductorRecord, ConductorStatus, CostEvent, CostFilter, CostSummary, CreateSession,
        DeleteMode, DeleteSessionRequest, DeletionResult, ForkSessionRequest, ForkSessionResult,
        GroupRecord, GroupSettingsUpdate, LaunchSpec, McpAttachmentRecord, OutputPage,
        ProjectRecord, ProjectSpec, ProjectTrustState, SandboxLaunchSpec, SessionRecord,
        SessionSearchResponse, SessionSearchResult, SessionStatus, SessionStatusSnapshot,
        SkillAttachmentRecord, StructuredEvent, WatcherEventRecord, WatcherRecord, WatcherStatus,
        WorkspaceRecord, WorktreeRecord, WorktreeStatus, now_ts,
    },
    runtime::SessionRuntime,
    security::{
        TrustGatedOperation, redact_event_payload, require_project_trust,
        validate_sandbox_allowed_path,
    },
    store::SessionStore,
    workspace::WorkspaceManager,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};
use uuid::Uuid;

#[cfg(feature = "serve")]
use crate::api::{AgentHelmApi, ApiResult, ApiToolProfile};

#[derive(Clone)]
pub struct ApplicationController<R: SessionRuntime> {
    pub config: AppConfig,
    pub store: SessionStore,
    runtime: R,
    agents: AgentRegistry,
    workspace: WorkspaceManager,
    read_only: bool,
    transcript_path_cache: Arc<Mutex<BTreeMap<String, TranscriptPathCacheEntry>>>,
}

#[derive(Clone, Debug)]
struct TranscriptPathCacheEntry {
    path: Option<PathBuf>,
    checked_at: i64,
}

impl<R: SessionRuntime> ApplicationController<R> {
    pub fn new(config: AppConfig, runtime: R, read_only: bool) -> Result<Self> {
        let store = SessionStore::new(config.state_db());
        Ok(Self {
            config,
            store,
            runtime,
            agents: AgentRegistry,
            workspace: WorkspaceManager,
            read_only,
            transcript_path_cache: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn init_profile(&self) -> Result<()> {
        self.store.init()
    }

    fn launch_spec(&self, session: &SessionRecord) -> Result<LaunchSpec> {
        let mut spec = self.agents.resume(session)?;
        if session.workspace_id.is_empty() {
            return Ok(spec);
        }
        let workspace = self.store.get_workspace(&session.workspace_id)?;
        if workspace.sandbox_id.is_some() {
            spec.sandbox = Some(SandboxLaunchSpec {
                image: self.config.sandbox_image.clone(),
                container_cwd: "/workspace".to_string(),
                allowed_paths: self
                    .config
                    .sandbox_allowed_paths
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect(),
            });
        }
        if spec.sandbox.is_none() && self.uses_agent_state_hooks(&session.agent)? {
            let executable = env::current_exe()?;
            spec.command = self.agents.wrap_agent_state_launch(
                &session.agent,
                &spec.command,
                &executable.to_string_lossy(),
                &self.config.profile,
                &session.id,
            )?;
        }
        Ok(spec)
    }

    pub fn create_session(&self, request: CreateSession) -> Result<SessionRecord> {
        self.create_session_with_start(request, true, None)
    }

    fn create_session_with_start(
        &self,
        request: CreateSession,
        start_immediately: bool,
        inherited_worktree_id: Option<String>,
    ) -> Result<SessionRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let group_name = defaulted(request.group_name.clone(), &self.config.default_group);
        let group = self
            .store
            .list_groups(&self.config.profile)?
            .into_iter()
            .find(|group| group.name == group_name);
        let default_agent = group
            .as_ref()
            .and_then(|group| group.default_agent.as_deref())
            .unwrap_or(&self.config.default_agent);
        let agent = defaulted(request.agent.clone(), default_agent);
        let command = self.config.resolve_tool_command(&agent, &request.command)?;
        let inherited_worktree = inherited_worktree_id
            .as_deref()
            .map(|id| self.store.get_worktree(id))
            .transpose()?;
        let project = if let Some(worktree) = &inherited_worktree {
            self.store.get_project(&worktree.project_id)?
        } else {
            self.ensure_project(&request.path, false)?
        };
        let mut project_path = if inherited_worktree.is_some() {
            request.path.clone()
        } else {
            project.root_path.clone()
        };
        let mut worktree_id = inherited_worktree_id;
        let mut sandbox_validated = false;
        let requested_worktree = request
            .worktree
            .as_deref()
            .filter(|branch| !branch.is_empty())
            .map(str::to_string);
        let auto_worktree = if requested_worktree.is_none()
            && inherited_worktree.is_none()
            && request.parent_session_id.is_none()
            && group
                .as_ref()
                .and_then(|group| group.default_worktree)
                .unwrap_or_else(|| {
                    self.config
                        .tool_worktree_behavior(&agent)
                        .creates_worktree_by_default()
                }) {
            Some(auto_worktree_branch(&agent, &request.name, &request.path))
        } else {
            None
        };

        if let Some(branch) = requested_worktree.or(auto_worktree) {
            let worktree_name = if request.name.trim().is_empty() {
                project_name(&project.root_path)
            } else {
                request.name.clone()
            };
            let info = self.workspace.create_named_worktree_with_state(
                &project.root_path,
                &branch,
                &worktree_name,
                request.carry_state,
            )?;
            if project.trust_state == ProjectTrustState::Trusted
                && let Err(error) = self
                    .workspace
                    .run_setup_hooks(&project.root_path, &info.path)
            {
                self.workspace
                    .discard_created_worktree(&project.root_path, &info.path);
                return Err(error);
            }
            if request.sandbox {
                if let Err(error) =
                    validate_sandbox_allowed_path(&info.path, &self.config.sandbox_allowed_paths)
                {
                    self.workspace
                        .discard_created_worktree(&project.root_path, &info.path);
                    return Err(error);
                }
                sandbox_validated = true;
            }
            let now = now_ts();
            let worktree = self.store.create_worktree(&WorktreeRecord {
                id: Uuid::new_v4().simple().to_string(),
                project_id: project.id.clone(),
                path: info.path.to_string_lossy().to_string(),
                branch: info.branch.unwrap_or(branch),
                base_branch: project.default_branch.clone(),
                status: WorktreeStatus::Ready,
                cleanup_allowed: true,
                version: 0,
                created_at: now,
                updated_at: now,
            })?;
            project_path = worktree.path.clone();
            worktree_id = Some(worktree.id);
        }
        if request.sandbox && !sandbox_validated {
            validate_sandbox_allowed_path(&project_path, &self.config.sandbox_allowed_paths)?;
        }
        let now = now_ts();
        let name = if request.name.trim().is_empty() {
            project_name(&project_path)
        } else {
            request.name
        };
        let workspace = self.store.create_workspace(&WorkspaceRecord {
            id: Uuid::new_v4().simple().to_string(),
            project_id: project.id.clone(),
            path: project_path.clone(),
            worktree_id: worktree_id.clone(),
            sandbox_id: request.sandbox.then(|| "local".to_string()),
            multi_repo_roots: "[]".to_string(),
            cleanup_policy: "manual".to_string(),
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        let session = SessionRecord {
            id: Uuid::new_v4().simple().to_string(),
            name,
            profile: self.config.profile.clone(),
            group_name,
            project_id: project.id,
            workspace_id: workspace.id,
            worktree_id,
            parent_session_id: request.parent_session_id,
            agent,
            command,
            project_path,
            status: if start_immediately {
                SessionStatus::Starting
            } else {
                SessionStatus::Stopped
            },
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: now,
            updated_at: now,
        };

        let session = self.store.create_session(&session)?;
        if !start_immediately {
            self.append_status_event(&session.id, session.status, "created_stopped")?;
            return Ok(session);
        }
        let spec = match self.launch_spec(&session) {
            Ok(spec) => spec,
            Err(err) => {
                let _ = self
                    .store
                    .update_status(&session.id, session.version, SessionStatus::Errored)
                    .and_then(|failed| {
                        self.append_status_event(&failed.id, failed.status, "launch_spec_failed")
                    });
                return Err(err);
            }
        };
        let handle = match self.runtime.start(&session.id, &spec) {
            Ok(handle) => handle,
            Err(err) => {
                let _ = self
                    .store
                    .update_status(&session.id, session.version, SessionStatus::Errored)
                    .and_then(|failed| {
                        self.append_status_event(&failed.id, failed.status, "runtime_start_failed")
                    });
                return Err(err);
            }
        };
        let session = self.store.update_runtime(
            &session.id,
            session.version,
            Some(handle.id),
            SessionStatus::Running,
        )?;
        self.append_status_event(&session.id, session.status, "runtime_start")?;
        self.append_runtime_agent_state_event(&session, "running", "runtime_start")?;
        if let Some(prompt) = request.prompt.filter(|prompt| !prompt.is_empty()) {
            self.runtime.send(&session.id, &prompt)?;
            let redacted = redact_event_payload(&prompt);
            self.store.append_session_event(
                &session.id,
                "input",
                json!({
                    "source": "initial_prompt",
                    "redacted": redacted != prompt,
                    "preview": redacted,
                }),
            )?;
            self.append_agent_state_event(&session.id, "waiting", "initial_prompt")?;
        }
        Ok(session)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        self.store.init()?;
        self.store
            .list_sessions()?
            .into_iter()
            .map(|session| self.status(&session.id))
            .collect()
    }

    pub fn list_groups(&self) -> Result<Vec<GroupRecord>> {
        self.store.init()?;
        self.store.list_groups(&self.config.profile)
    }

    pub fn create_group(
        &self,
        name: String,
        parent: Option<String>,
        default_project_path: Option<String>,
    ) -> Result<GroupRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let name = group_path(parent.as_deref(), &name, &self.config.default_group)?;
        self.store.create_group(
            &self.config.profile,
            &name,
            default_project_path.as_deref().unwrap_or(""),
        )
    }

    pub fn update_group(
        &self,
        name: &str,
        default_project_path: Option<String>,
        clear_default_project_path: bool,
        collapsed: Option<bool>,
    ) -> Result<GroupRecord> {
        let default_project_path = if clear_default_project_path {
            Some(String::new())
        } else {
            default_project_path
        };
        self.update_group_settings(
            name,
            GroupSettingsUpdate {
                default_project_path,
                collapsed,
                ..GroupSettingsUpdate::default()
            },
        )
    }

    pub fn update_group_settings(
        &self,
        name: &str,
        update: GroupSettingsUpdate,
    ) -> Result<GroupRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        self.store.update_group(
            &self.config.profile,
            &defaulted(name.to_string(), &self.config.default_group),
            &update,
        )
    }

    pub fn delete_group(&self, name: &str, force: bool) -> Result<()> {
        self.ensure_writable()?;
        self.store.init()?;
        let name = defaulted(name.to_string(), &self.config.default_group);
        let replacement = parent_group(&name).unwrap_or_else(|| self.config.default_group.clone());
        self.store
            .delete_group(&self.config.profile, &name, force, &replacement)
    }

    pub fn move_session_to_group(&self, id: &str, group_name: String) -> Result<SessionRecord> {
        self.ensure_writable()?;
        let session = self.get_session(id)?;
        self.store.update_session_group(
            id,
            session.version,
            &defaulted(group_name, &self.config.default_group),
        )
    }

    pub fn rename_session(&self, id: &str, name: String) -> Result<SessionRecord> {
        self.ensure_writable()?;
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::msg("session name required"));
        }
        let session = self.get_session(id)?;
        self.store.update_session_name(id, session.version, name)
    }

    pub fn get_session(&self, id: &str) -> Result<SessionRecord> {
        self.store.init()?;
        self.store.get_session(id)
    }

    pub fn status(&self, id: &str) -> Result<SessionRecord> {
        let session = self.get_session(id)?;
        let runtime_status = self
            .agents
            .detect_status(&session.agent, self.runtime.status(id)?)?;
        if session.status == SessionStatus::Errored && runtime_status == SessionStatus::Stopped {
            return Ok(session);
        }
        if runtime_status != session.status
            || runtime_status == SessionStatus::Stopped && session.runtime_id.is_some()
        {
            if self.read_only {
                let mut reconciled = session;
                reconciled.status = runtime_status;
                if runtime_status == SessionStatus::Stopped {
                    reconciled.runtime_id = None;
                }
                return Ok(reconciled);
            }
            return match if runtime_status == SessionStatus::Stopped {
                self.store
                    .update_runtime(id, session.version, None, SessionStatus::Stopped)
            } else {
                self.store
                    .update_status(id, session.version, runtime_status)
            } {
                Ok(updated) => {
                    self.append_status_event(id, updated.status, "runtime_status")?;
                    if updated.status == SessionStatus::Running {
                        self.append_runtime_agent_state_event(
                            &updated,
                            "running",
                            "runtime_status",
                        )?;
                    }
                    Ok(updated)
                }
                Err(_) => self.store.get_session(id),
            };
        }
        Ok(session)
    }

    fn runtime_is_running(&self, session: &SessionRecord) -> Result<bool> {
        let runtime_status = self
            .agents
            .detect_status(&session.agent, self.runtime.status(&session.id)?)?;
        Ok(runtime_status == SessionStatus::Running)
    }

    pub fn status_snapshot(&self, id: &str) -> Result<SessionStatusSnapshot> {
        let session = self.status(id)?;
        let recent_events = self.store.latest_session_status_signal_events(id, 20)?;
        let transcript_state =
            self.transcript_agent_state_for_snapshot(&session, &recent_events)?;
        let open_assignments = self.store.open_conductor_assignments_for_session(id)?;
        let derivation = self.agents.derive_deck_status(
            &session.agent,
            session.status,
            transcript_state.as_ref(),
            &recent_events,
            &open_assignments,
        )?;
        Ok(SessionStatusSnapshot {
            lifecycle_status: session.status,
            session,
            deck_status: derivation.deck_status,
            source_event_id: derivation.source_event_id,
            source: derivation.source,
            activity: derivation.activity,
        })
    }

    fn transcript_agent_state_for_snapshot(
        &self,
        session: &SessionRecord,
        recent_events: &[crate::models::SessionEvent],
    ) -> Result<Option<Value>> {
        if let Some((_, payload)) =
            latest_transcript_agent_state_from_events(session, recent_events)?
        {
            self.cache_transcript_path(session, &payload);
            return Ok(Some(payload));
        }

        if let Some(path) = self.cached_transcript_path(&session.id) {
            if let Some((_, payload)) =
                latest_transcript_agent_state_from_path(session, &path, true)?
            {
                self.cache_transcript_path(session, &payload);
                return Ok(Some(payload));
            }
            self.clear_cached_transcript_path(&session.id, &path);
        }

        if self.should_skip_transcript_discovery(&session.id) {
            return Ok(None);
        }

        if let Some((_, payload)) = latest_recent_transcript_agent_state(session)? {
            self.cache_transcript_path(session, &payload);
            return Ok(Some(payload));
        }

        self.cache_transcript_miss(session);
        Ok(None)
    }

    fn cached_transcript_path(&self, session_id: &str) -> Option<PathBuf> {
        self.transcript_path_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(session_id).and_then(|entry| entry.path.clone()))
    }

    fn should_skip_transcript_discovery(&self, session_id: &str) -> bool {
        self.transcript_path_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(session_id).cloned())
            .is_some_and(|entry| {
                entry.path.is_none()
                    && now_ts().saturating_sub(entry.checked_at)
                        < SNAPSHOT_TRANSCRIPT_DISCOVERY_RETRY_SECS
            })
    }

    fn cache_transcript_path(&self, session: &SessionRecord, payload: &Value) {
        let Some(path) = payload.get("transcript_path").and_then(Value::as_str) else {
            return;
        };
        if let Ok(mut cache) = self.transcript_path_cache.lock() {
            cache.insert(
                session.id.clone(),
                TranscriptPathCacheEntry {
                    path: Some(PathBuf::from(path)),
                    checked_at: now_ts(),
                },
            );
        }
    }

    fn cache_transcript_miss(&self, session: &SessionRecord) {
        if let Ok(mut cache) = self.transcript_path_cache.lock() {
            cache.insert(
                session.id.clone(),
                TranscriptPathCacheEntry {
                    path: None,
                    checked_at: now_ts(),
                },
            );
        }
    }

    fn clear_cached_transcript_path(&self, session_id: &str, path: &Path) {
        if let Ok(mut cache) = self.transcript_path_cache.lock() {
            if cache
                .get(session_id)
                .and_then(|entry| entry.path.as_deref())
                .is_some_and(|cached| cached == path)
            {
                cache.remove(session_id);
            }
        }
    }

    pub fn output(&self, id: &str, limit: usize, ansi: bool) -> Result<OutputPage> {
        let session = self.status(id)?;
        if session.status != SessionStatus::Running {
            return Err(AppError::msg("session is not running"));
        }
        self.runtime.capture(id, limit, ansi)
    }

    pub fn diff(&self, id: &str) -> Result<String> {
        let session = self.get_session(id)?;
        let project_path = Path::new(&session.project_path);
        let mut sections = Vec::new();

        let status = git_text(project_path, ["status", "--short"])?;
        if !status.trim().is_empty() {
            sections.push(diff_section("status", status));
        }

        let unstaged = git_text(project_path, ["diff", "--no-ext-diff"])?;
        if !unstaged.trim().is_empty() {
            sections.push(diff_section("unstaged", unstaged));
        }

        let staged = git_text(project_path, ["diff", "--cached", "--no-ext-diff"])?;
        if !staged.trim().is_empty() {
            sections.push(diff_section("staged", staged));
        }

        let untracked = git_text(project_path, ["ls-files", "--others", "--exclude-standard"])?;
        for file in untracked.lines().filter(|line| !line.trim().is_empty()) {
            let diff = git_text_allow_exit(
                project_path,
                ["diff", "--no-index", "--", "/dev/null", file],
                &[0, 1],
            )?;
            if !diff.trim().is_empty() {
                sections.push(diff_section(&format!("untracked: {file}"), diff));
            }
        }

        Ok(sections.join("\n"))
    }

    pub fn attach(&self, id: &str) -> Result<()> {
        self.ensure_writable()?;
        let session = self.status(id)?;
        if session.status != SessionStatus::Running {
            return Err(AppError::msg("session is not running"));
        }
        self.runtime.attach(id)
    }

    pub fn send(&self, id: &str, text: &str) -> Result<()> {
        self.ensure_writable()?;
        let session = self.status(id)?;
        if session.status != SessionStatus::Running {
            return Err(AppError::msg("session is not running"));
        }
        self.runtime.send(id, text)?;
        let redacted = redact_event_payload(text);
        self.store.append_session_event(
            id,
            "input",
            json!({
                "source": "user",
                "redacted": redacted != text,
                "preview": redacted,
            }),
        )?;
        self.append_agent_state_event(id, "waiting", "user_input")?;
        Ok(())
    }

    fn append_status_event(&self, id: &str, status: SessionStatus, source: &str) -> Result<()> {
        self.store.append_session_event(
            id,
            "status",
            json!({ "status": status.as_str(), "source": source }),
        )?;
        Ok(())
    }

    fn append_agent_state_event(&self, id: &str, state: &str, source: &str) -> Result<()> {
        self.store.append_session_event(
            id,
            "agent_state",
            json!({ "state": state, "source": source }),
        )?;
        Ok(())
    }

    pub fn stop(&self, id: &str) -> Result<SessionRecord> {
        self.ensure_writable()?;
        let session = self.get_session(id)?;
        self.runtime.stop(id)?;
        let session =
            self.store
                .update_runtime(id, session.version, None, SessionStatus::Stopped)?;
        self.append_status_event(id, session.status, "runtime_stop")?;
        Ok(session)
    }

    pub fn restart(&self, id: &str) -> Result<SessionRecord> {
        self.ensure_writable()?;
        let session = self.get_session(id)?;
        let spec = self.launch_spec(&session)?;
        let handle = self.runtime.restart(id, &spec)?;
        let session = self.store.update_runtime(
            id,
            session.version,
            Some(handle.id),
            SessionStatus::Running,
        )?;
        self.append_status_event(id, session.status, "runtime_restart")?;
        self.append_runtime_agent_state_event(&session, "running", "runtime_restart")?;
        Ok(session)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.ensure_writable()?;
        self.runtime.destroy(id)?;
        self.store.remove_session(id)
    }

    pub fn delete_session(&self, request: DeleteSessionRequest) -> Result<DeletionResult> {
        self.ensure_writable()?;
        let mut session = self.get_session(&request.session_id)?;
        let runtime_stopped = self.runtime_is_running(&session)?;
        if runtime_stopped {
            self.runtime.destroy(&request.session_id)?;
            session = self.store.update_runtime(
                &request.session_id,
                session.version,
                None,
                SessionStatus::Stopped,
            )?;
        }
        let mut worktree_cleaned = false;
        if matches!(request.mode, DeleteMode::CleanupWorktree)
            && let Some(worktree_id) = session.worktree_id.as_deref()
        {
            let deleting_id = request.session_id.as_str();
            let has_other_owner = self.store.list_sessions()?.into_iter().any(|other| {
                other.id != deleting_id && other.worktree_id.as_deref() == Some(worktree_id)
            });
            if !has_other_owner {
                let worktree = self.store.get_worktree(worktree_id)?;
                self.finish_worktree_allowing_owner(worktree_id, Some(deleting_id))?;
                worktree_cleaned = worktree.cleanup_allowed;
            }
        }
        self.store.remove_session(&request.session_id)?;
        Ok(DeletionResult {
            session_id: request.session_id,
            deleted: true,
            runtime_stopped,
            worktree_cleaned,
            purged: matches!(request.mode, DeleteMode::Purge),
            history_retained: false,
        })
    }

    pub fn fork_session(&self, request: ForkSessionRequest) -> Result<ForkSessionResult> {
        self.ensure_writable()?;
        let parent = self.get_session(&request.parent_session_id)?;
        let fork_plan = self.agents.fork(&parent, &request)?;
        let command = fork_plan.command;
        let create = CreateSession {
            path: parent.project_path.clone(),
            agent: parent.agent.clone(),
            command,
            name: request
                .name
                .clone()
                .unwrap_or_else(|| format!("{} fork", parent.name)),
            group_name: request
                .group_name
                .clone()
                .unwrap_or_else(|| parent.group_name.clone()),
            worktree: request.worktree_branch.clone(),
            carry_state: request.carry_state,
            sandbox: false,
            prompt: None,
            parent_session_id: Some(parent.id.clone()),
        };
        let inherited_worktree_id = if request.worktree_branch.as_deref().unwrap_or("").is_empty() {
            parent.worktree_id.clone()
        } else {
            None
        };
        let child = self.create_session_with_start(
            create,
            request.start_immediately,
            inherited_worktree_id,
        )?;
        self.store.append_session_event(
            &child.id,
            "forked",
            json!({
                "parent_session_id": parent.id,
                "carry_state": request.carry_state,
                "inherits_conversation": fork_plan.inherits_conversation,
            }),
        )?;
        Ok(ForkSessionResult {
            parent_session_id: request.parent_session_id,
            child_session_id: child.id,
            workspace_id: child.workspace_id,
            worktree_id: child.worktree_id,
            started: request.start_immediately,
        })
    }

    pub fn register_project(&self, spec: ProjectSpec) -> Result<ProjectRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let project_ref = self.workspace.resolve_project_ref(&spec.root_path)?;
        let root_path = project_ref.root.to_string_lossy().to_string();
        if let Some(mut project) = self
            .store
            .list_projects(&self.config.profile)?
            .into_iter()
            .find(|project| project.root_path == root_path)
        {
            let requested_default_branch = spec.default_branch.trim();
            let should_update_default_branch = !requested_default_branch.is_empty()
                && requested_default_branch != project.default_branch;
            let should_update_trust =
                spec.trusted && project.trust_state != ProjectTrustState::Trusted;
            if should_update_trust || should_update_default_branch {
                if should_update_trust {
                    project.trust_state = ProjectTrustState::Trusted;
                }
                if should_update_default_branch {
                    project.default_branch = requested_default_branch.to_string();
                }
                project.updated_at = now_ts();
                return self.store.update_project(&project, project.version);
            }
            return Ok(project);
        }
        let now = now_ts();
        self.store.create_project(&ProjectRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            root_path,
            repo_identity: project_ref
                .repo_identity
                .unwrap_or_else(|| spec.root_path.clone()),
            default_branch: defaulted(
                spec.default_branch,
                project_ref.default_branch.as_deref().unwrap_or("main"),
            ),
            trust_state: if spec.trusted {
                ProjectTrustState::Trusted
            } else {
                ProjectTrustState::Untrusted
            },
            hooks_hash: String::new(),
            config_hash: String::new(),
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRecord>> {
        self.store.init()?;
        self.store.list_projects(&self.config.profile)
    }

    pub fn get_project(&self, id: &str) -> Result<ProjectRecord> {
        self.store.init()?;
        self.store.get_project(id)
    }

    pub fn set_project_trust(&self, id: &str, trusted: bool) -> Result<ProjectRecord> {
        self.ensure_writable()?;
        let mut project = self.get_project(id)?;
        project.trust_state = if trusted {
            ProjectTrustState::Trusted
        } else {
            ProjectTrustState::Untrusted
        };
        project.updated_at = now_ts();
        self.store.update_project(&project, project.version)
    }

    pub fn remove_project(&self, id: &str) -> Result<()> {
        self.ensure_writable()?;
        let project = self.get_project(id)?;
        let session_count = self
            .store
            .list_sessions()?
            .into_iter()
            .filter(|session| session.project_id == project.id)
            .count();
        if session_count > 0 {
            return Err(AppError::msg(format!(
                "project still has {session_count} session(s)"
            )));
        }
        let workspace_count = self.store.list_workspaces(&project.id)?.len();
        if workspace_count > 0 {
            return Err(AppError::msg(format!(
                "project still has {workspace_count} workspace(s)"
            )));
        }
        let worktree_count = self.store.list_worktrees(&project.id)?.len();
        if worktree_count > 0 {
            return Err(AppError::msg(format!(
                "project still has {worktree_count} worktree(s)"
            )));
        }
        let mcp_count = self
            .store
            .list_mcp_attachments(
                &self.config.profile,
                Some("project"),
                Some(&project.id),
                None,
            )?
            .into_iter()
            .filter(|attachment| attachment.status != AttachmentStatus::Detached)
            .count();
        if mcp_count > 0 {
            return Err(AppError::msg(format!(
                "project still has {mcp_count} MCP attachment(s)"
            )));
        }
        let skill_count = self
            .store
            .list_skill_attachments(
                &self.config.profile,
                Some("project"),
                Some(&project.id),
                None,
            )?
            .into_iter()
            .filter(|attachment| attachment.status != AttachmentStatus::Detached)
            .count();
        if skill_count > 0 {
            return Err(AppError::msg(format!(
                "project still has {skill_count} skill attachment(s)"
            )));
        }
        let watcher_count = self
            .store
            .list_watchers(&self.config.profile, Some(&project.id))?
            .len();
        if watcher_count > 0 {
            return Err(AppError::msg(format!(
                "project still has {watcher_count} watcher(s)"
            )));
        }
        self.store.delete_project(id)
    }

    pub fn list_worktrees(&self, project_id: &str) -> Result<Vec<WorktreeRecord>> {
        self.store.init()?;
        self.get_project(project_id)?;
        self.store.list_worktrees(project_id)
    }

    pub fn get_worktree(&self, id: &str) -> Result<WorktreeRecord> {
        self.store.init()?;
        self.store.get_worktree(id)
    }

    pub fn list_workspaces(&self, project_id: &str) -> Result<Vec<WorkspaceRecord>> {
        self.store.init()?;
        self.get_project(project_id)?;
        self.store.list_workspaces(project_id)
    }

    pub fn get_workspace(&self, id: &str) -> Result<WorkspaceRecord> {
        self.store.init()?;
        self.store.get_workspace(id)
    }

    pub fn create_project_worktree(
        &self,
        project_id: &str,
        branch: &str,
        carry_state: bool,
    ) -> Result<WorktreeRecord> {
        self.ensure_writable()?;
        let project = self.get_project(project_id)?;
        let info =
            self.workspace
                .create_worktree_with_state(&project.root_path, branch, carry_state)?;
        if project.trust_state == ProjectTrustState::Trusted
            && let Err(error) = self
                .workspace
                .run_setup_hooks(&project.root_path, &info.path)
        {
            self.workspace
                .discard_created_worktree(&project.root_path, &info.path);
            return Err(error);
        }
        let now = now_ts();
        self.store.create_worktree(&WorktreeRecord {
            id: Uuid::new_v4().simple().to_string(),
            project_id: project.id,
            path: info.path.to_string_lossy().to_string(),
            branch: info.branch.unwrap_or_else(|| branch.to_string()),
            base_branch: project.default_branch,
            status: WorktreeStatus::Ready,
            cleanup_allowed: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn finish_worktree(&self, id: &str) -> Result<WorktreeRecord> {
        self.finish_worktree_allowing_owner(id, None)
    }

    fn finish_worktree_allowing_owner(
        &self,
        id: &str,
        allowed_session_id: Option<&str>,
    ) -> Result<WorktreeRecord> {
        self.ensure_writable()?;
        let mut worktree = self.store.get_worktree(id)?;
        let has_owner = self.store.list_sessions()?.into_iter().any(|session| {
            session.worktree_id.as_deref() == Some(id)
                && Some(session.id.as_str()) != allowed_session_id
        });
        if has_owner {
            return Err(AppError::msg("worktree is still attached to a session"));
        }
        let project = self.store.get_project(&worktree.project_id)?;
        if worktree.cleanup_allowed {
            if project.trust_state == ProjectTrustState::Trusted {
                self.workspace
                    .run_teardown_hooks(&project.root_path, &worktree.path)?;
            }
            self.workspace.finish_worktree(&worktree.path)?;
        }
        worktree.status = WorktreeStatus::Finished;
        worktree.updated_at = now_ts();
        self.store.update_worktree(&worktree, worktree.version)
    }

    pub fn cleanup_worktrees(&self, project_id: &str) -> Result<CleanupReport> {
        self.ensure_writable()?;
        self.get_project(project_id)?;
        let mut report = CleanupReport {
            project_id: project_id.to_string(),
            inspected: 0,
            removed: 0,
            missing: 0,
            skipped: 0,
        };
        for mut worktree in self.store.list_worktrees(project_id)? {
            report.inspected += 1;
            if !Path::new(&worktree.path).exists() {
                worktree.status = WorktreeStatus::Finished;
                worktree.updated_at = now_ts();
                self.store.update_worktree(&worktree, worktree.version)?;
                report.missing += 1;
            } else if worktree.status == WorktreeStatus::Finished && worktree.cleanup_allowed {
                if self
                    .store
                    .list_sessions()?
                    .into_iter()
                    .any(|session| session.worktree_id.as_deref() == Some(worktree.id.as_str()))
                {
                    report.skipped += 1;
                    continue;
                }
                self.finish_worktree(&worktree.id)?;
                report.removed += 1;
            } else {
                report.skipped += 1;
            }
        }
        Ok(report)
    }

    pub fn list_mcp_attachments(&self) -> Result<Vec<McpAttachmentRecord>> {
        self.store.init()?;
        self.store
            .list_mcp_attachments(&self.config.profile, None, None, None)
    }

    pub fn session_materialization(&self, session_id: &str) -> Result<SessionMaterializationPlan> {
        let session = self.get_session(session_id)?;
        let capabilities = self.agents.capabilities(&session.agent)?;
        let mcp = if capabilities.mcp {
            self.effective_mcp_attachments_for_session(&session)?
        } else {
            Vec::new()
        };
        let skills = if capabilities.skills {
            self.effective_skill_attachments_for_session(&session)?
        } else {
            Vec::new()
        };
        Ok(session_materialization_plan(&mcp, &skills))
    }

    pub fn attach_mcp(&self, session_id: &str, server_id: String) -> Result<McpAttachmentRecord> {
        self.ensure_writable()?;
        let session = self.get_session(session_id)?;
        if !self.agents.capabilities(&session.agent)?.mcp {
            return Err(AppError::msg(format!(
                "agent does not support MCP: {}",
                session.agent
            )));
        }
        let now = now_ts();
        self.store.create_mcp_attachment(&McpAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "session".to_string(),
            project_id: None,
            session_id: Some(session.id),
            server_id,
            status: AttachmentStatus::Attached,
            materialized_state: "{}".to_string(),
            restart_required: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn attach_project_mcp(
        &self,
        project_id: &str,
        server_id: String,
    ) -> Result<McpAttachmentRecord> {
        self.ensure_writable()?;
        let project = self.get_project(project_id)?;
        let now = now_ts();
        self.store.create_mcp_attachment(&McpAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "project".to_string(),
            project_id: Some(project.id),
            session_id: None,
            server_id,
            status: AttachmentStatus::Attached,
            materialized_state: "{}".to_string(),
            restart_required: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn attach_profile_mcp(&self, server_id: String) -> Result<McpAttachmentRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let now = now_ts();
        self.store.create_mcp_attachment(&McpAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "profile".to_string(),
            project_id: None,
            session_id: None,
            server_id,
            status: AttachmentStatus::Attached,
            materialized_state: "{}".to_string(),
            restart_required: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn detach_mcp(&self, id: &str) -> Result<McpAttachmentRecord> {
        self.ensure_writable()?;
        let mut attachment = self.store.get_mcp_attachment(id)?;
        attachment.status = AttachmentStatus::Detached;
        attachment.restart_required = true;
        attachment.updated_at = now_ts();
        self.store
            .update_mcp_attachment(&attachment, attachment.version)
    }

    pub fn sync_mcp(&self) -> Result<Vec<McpAttachmentRecord>> {
        self.ensure_writable()?;
        self.ensure_mcp_trust()?;
        let mut synced = Vec::new();
        for mut attachment in self.list_mcp_attachments()? {
            if attachment.status == AttachmentStatus::Attached {
                let plan_json = session_materialization_json(
                    &self.effective_mcp_attachments_for(&attachment)?,
                    &self.effective_skill_attachments_for_mcp(&attachment)?,
                );
                attachment.materialized_state = plan_json.clone();
                attachment.restart_required = false;
                attachment.updated_at = now_ts();
                let version = attachment.version;
                synced.push(self.store.update_mcp_attachment(&attachment, version)?);
            } else {
                synced.push(attachment);
            }
        }
        Ok(synced)
    }

    fn effective_mcp_attachments_for(
        &self,
        target: &McpAttachmentRecord,
    ) -> Result<Vec<McpAttachmentRecord>> {
        let (project_id, session_id) = self.attachment_target(target)?;
        self.effective_mcp_attachments(project_id.as_deref(), session_id.as_deref())
    }

    fn effective_mcp_attachments_for_session(
        &self,
        session: &SessionRecord,
    ) -> Result<Vec<McpAttachmentRecord>> {
        self.effective_mcp_attachments(Some(&session.project_id), Some(&session.id))
    }

    fn effective_mcp_attachments(
        &self,
        project_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<McpAttachmentRecord>> {
        let mut records =
            self.store
                .list_mcp_attachments(&self.config.profile, Some("profile"), None, None)?;
        if let Some(project_id) = project_id {
            records.extend(self.store.list_mcp_attachments(
                &self.config.profile,
                Some("project"),
                Some(project_id),
                None,
            )?);
        }
        if let Some(session_id) = session_id {
            records.extend(self.store.list_mcp_attachments(
                &self.config.profile,
                Some("session"),
                None,
                Some(session_id),
            )?);
        }
        Ok(records)
    }

    fn effective_skill_attachments_for_mcp(
        &self,
        target: &McpAttachmentRecord,
    ) -> Result<Vec<SkillAttachmentRecord>> {
        let (project_id, session_id) = self.attachment_target(target)?;
        self.effective_skill_attachments(project_id.as_deref(), session_id.as_deref())
    }

    fn effective_skill_attachments_for_session(
        &self,
        session: &SessionRecord,
    ) -> Result<Vec<SkillAttachmentRecord>> {
        self.effective_skill_attachments(Some(&session.project_id), Some(&session.id))
    }

    fn effective_skill_attachments(
        &self,
        project_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<SkillAttachmentRecord>> {
        let mut records =
            self.store
                .list_skill_attachments(&self.config.profile, Some("profile"), None, None)?;
        if let Some(project_id) = project_id {
            records.extend(self.store.list_skill_attachments(
                &self.config.profile,
                Some("project"),
                Some(project_id),
                None,
            )?);
        }
        if let Some(session_id) = session_id {
            records.extend(self.store.list_skill_attachments(
                &self.config.profile,
                Some("session"),
                None,
                Some(session_id),
            )?);
        }
        Ok(records)
    }

    fn attachment_target(
        &self,
        target: &McpAttachmentRecord,
    ) -> Result<(Option<String>, Option<String>)> {
        if target.scope == "session" {
            let session_id = target
                .session_id
                .as_deref()
                .ok_or_else(|| AppError::msg("session MCP attachment missing session id"))?;
            let session = self.get_session(session_id)?;
            return Ok((Some(session.project_id), Some(session.id)));
        }
        Ok((target.project_id.clone(), target.session_id.clone()))
    }

    pub fn list_skill_attachments(&self) -> Result<Vec<SkillAttachmentRecord>> {
        self.store.init()?;
        self.store
            .list_skill_attachments(&self.config.profile, None, None, None)
    }

    pub fn attach_skill(
        &self,
        session_id: &str,
        skill_id: String,
    ) -> Result<SkillAttachmentRecord> {
        self.ensure_writable()?;
        let session = self.get_session(session_id)?;
        if !self.agents.capabilities(&session.agent)?.skills {
            return Err(AppError::msg(format!(
                "agent does not support skills: {}",
                session.agent
            )));
        }
        let now = now_ts();
        self.store.create_skill_attachment(&SkillAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "session".to_string(),
            project_id: None,
            session_id: Some(session.id),
            skill_id,
            pool_path: String::new(),
            materialized_path: String::new(),
            status: AttachmentStatus::Attached,
            restart_required: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn attach_project_skill(
        &self,
        project_id: &str,
        skill_id: String,
    ) -> Result<SkillAttachmentRecord> {
        self.ensure_writable()?;
        let project = self.get_project(project_id)?;
        let now = now_ts();
        self.store.create_skill_attachment(&SkillAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "project".to_string(),
            project_id: Some(project.id),
            session_id: None,
            skill_id,
            pool_path: String::new(),
            materialized_path: String::new(),
            status: AttachmentStatus::Attached,
            restart_required: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn attach_profile_skill(&self, skill_id: String) -> Result<SkillAttachmentRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let now = now_ts();
        self.store.create_skill_attachment(&SkillAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "profile".to_string(),
            project_id: None,
            session_id: None,
            skill_id,
            pool_path: String::new(),
            materialized_path: String::new(),
            status: AttachmentStatus::Attached,
            restart_required: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn detach_skill(&self, id: &str) -> Result<SkillAttachmentRecord> {
        self.ensure_writable()?;
        let mut attachment = self.store.get_skill_attachment(id)?;
        attachment.status = AttachmentStatus::Detached;
        attachment.restart_required = true;
        attachment.updated_at = now_ts();
        self.store
            .update_skill_attachment(&attachment, attachment.version)
    }

    pub fn sync_skills(&self) -> Result<Vec<SkillAttachmentRecord>> {
        self.ensure_writable()?;
        self.ensure_skill_trust()?;
        let mut synced = Vec::new();
        for mut attachment in self.list_skill_attachments()? {
            if attachment.status == AttachmentStatus::Attached {
                if attachment.materialized_path.is_empty() {
                    attachment.materialized_path =
                        format!(".agent-helm/skills/{}", attachment.skill_id);
                }
                attachment.restart_required = false;
                attachment.updated_at = now_ts();
                let version = attachment.version;
                synced.push(self.store.update_skill_attachment(&attachment, version)?);
            } else {
                synced.push(attachment);
            }
        }
        Ok(synced)
    }

    pub fn list_watchers(&self) -> Result<Vec<WatcherRecord>> {
        self.store.init()?;
        self.store.list_watchers(&self.config.profile, None)
    }

    pub fn list_project_watchers(&self, project_id: &str) -> Result<Vec<WatcherRecord>> {
        self.store.init()?;
        self.get_project(project_id)?;
        self.store
            .list_watchers(&self.config.profile, Some(project_id))
    }

    fn find_watcher(&self, id_or_name: &str) -> Result<Option<WatcherRecord>> {
        if let Ok(watcher) = self.store.get_watcher(id_or_name) {
            return Ok(Some(watcher));
        }

        let matches = self
            .list_watchers()?
            .into_iter()
            .filter(|watcher| watcher.name == id_or_name)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(None),
            [watcher] => Ok(Some(watcher.clone())),
            _ => Err(AppError::msg(format!(
                "watcher name is ambiguous: {id_or_name}"
            ))),
        }
    }

    fn resolve_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        self.find_watcher(id_or_name)?
            .ok_or_else(|| AppError::msg(format!("watcher not found: {id_or_name}")))
    }

    pub fn create_watcher(&self, name: String, adapter_id: String) -> Result<WatcherRecord> {
        self.create_watcher_config(name, adapter_id, None, "{}".to_string())
    }

    pub fn create_watcher_config(
        &self,
        name: String,
        adapter_id: String,
        project_id: Option<String>,
        config_ref: String,
    ) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        if let Some(project_id) = project_id.as_deref() {
            self.get_project(project_id)?;
        }
        validate_watcher_config(&adapter_id, project_id.as_deref(), &config_ref)?;
        let now = now_ts();
        self.store.create_watcher(&WatcherRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            project_id,
            adapter_id,
            name,
            config_ref,
            status: WatcherStatus::Stopped,
            last_event_at: 0,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn start_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        let mut watcher = self.resolve_watcher(id_or_name)?;
        if let Some(project_id) = watcher.project_id.as_deref() {
            let project = self.store.get_project(project_id)?;
            require_project_trust(project.trust_state, TrustGatedOperation::Watchers)?;
        }
        validate_watcher_config(
            &watcher.adapter_id,
            watcher.project_id.as_deref(),
            &watcher.config_ref,
        )?;
        watcher.status = WatcherStatus::Running;
        watcher.updated_at = now_ts();
        let watcher = self.store.update_watcher(&watcher, watcher.version)?;
        self.store.append_watcher_event(&WatcherEventRecord {
            id: Uuid::new_v4().simple().to_string(),
            watcher_id: watcher.id.clone(),
            source: "controller".to_string(),
            event_type: "started".to_string(),
            payload_ref: "{}".to_string(),
            signature_status: "not_applicable".to_string(),
            route_decision: "none".to_string(),
            delivered: true,
            created_at: now_ts(),
        })?;
        if watcher.adapter_id == "shell" {
            self.run_shell_watcher(&watcher)?;
        }
        Ok(watcher)
    }

    pub fn poll_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        let watcher = self.resolve_watcher(id_or_name)?;
        if watcher.status != WatcherStatus::Running {
            return Err(AppError::msg("watcher is not running"));
        }
        if !watcher_supports_poll(&watcher.adapter_id) {
            return Err(AppError::msg(format!(
                "watcher adapter does not support polling: {}",
                watcher.adapter_id
            )));
        }
        if let Some(project_id) = watcher.project_id.as_deref() {
            let project = self.store.get_project(project_id)?;
            require_project_trust(project.trust_state, TrustGatedOperation::Watchers)?;
        }
        validate_watcher_config(
            &watcher.adapter_id,
            watcher.project_id.as_deref(),
            &watcher.config_ref,
        )?;
        if watcher.adapter_id == "shell" {
            self.run_shell_watcher(&watcher)?;
        }
        self.store.get_watcher(&watcher.id)
    }

    pub fn poll_running_watchers(&self) -> Result<Vec<WatcherRecord>> {
        self.ensure_writable()?;
        let watchers = self.list_watchers()?;
        let mut polled = Vec::new();
        let mut first_error = None;
        for watcher in watchers.into_iter().filter(|watcher| {
            watcher.status == WatcherStatus::Running && watcher_supports_poll(&watcher.adapter_id)
        }) {
            match self.poll_watcher(&watcher.id) {
                Ok(watcher) => polled.push(watcher),
                Err(err) => {
                    first_error.get_or_insert_with(|| err.to_string());
                }
            }
        }
        if let Some(err) = first_error {
            return Err(AppError::msg(err));
        }
        Ok(polled)
    }

    pub fn test_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        let watcher = match self.find_watcher(id_or_name)? {
            Some(watcher) => watcher,
            None => {
                let now = now_ts();
                WatcherRecord {
                    id: id_or_name.to_string(),
                    profile: self.config.profile.clone(),
                    project_id: None,
                    adapter_id: "manual".to_string(),
                    name: id_or_name.to_string(),
                    config_ref: "{}".to_string(),
                    status: WatcherStatus::Stopped,
                    last_event_at: 0,
                    version: 0,
                    created_at: now,
                    updated_at: now,
                }
            }
        };
        validate_watcher_config(
            &watcher.adapter_id,
            watcher.project_id.as_deref(),
            &watcher.config_ref,
        )?;
        Ok(watcher)
    }

    fn run_shell_watcher(&self, watcher: &WatcherRecord) -> Result<()> {
        let config = watcher_config(&watcher.config_ref)?;
        let command = required_watcher_command(&config)?;
        let project_id = watcher
            .project_id
            .as_deref()
            .ok_or_else(|| AppError::msg("shell watcher requires project"))?;
        let project = self.store.get_project(project_id)?;
        let timeout_ms = watcher_timeout_ms(&config)?;
        let output = run_shell_watcher_command(&project.root_path, command, timeout_ms)?;
        let payload = if output.success || output.stderr.is_empty() {
            output.stdout
        } else {
            output.stderr
        };
        let mut route_decision = "executed".to_string();
        let mut delivered = output.success;
        let mut route_error = None;
        if let (true, Some(session_id)) = (output.success, watcher_route_session_id(&config)) {
            route_decision = format!("session:{session_id}");
            if let Err(err) = self.send(session_id, &payload) {
                delivered = false;
                route_error = Some(err.to_string());
            }
        }
        self.store.append_watcher_event(&WatcherEventRecord {
            id: Uuid::new_v4().simple().to_string(),
            watcher_id: watcher.id.clone(),
            source: "shell".to_string(),
            event_type: if output.timed_out {
                "timeout"
            } else if output.success {
                "output"
            } else {
                "error"
            }
            .to_string(),
            payload_ref: payload.clone(),
            signature_status: "not_applicable".to_string(),
            route_decision,
            delivered,
            created_at: now_ts(),
        })?;
        if let Some(err) = route_error {
            self.mark_watcher_errored(&watcher.id)?;
            return Err(AppError::msg(format!("watcher route failed: {err}")));
        }
        if output.success {
            return Ok(());
        }

        self.mark_watcher_errored(&watcher.id)?;
        if output.timed_out {
            Err(AppError::msg(format!(
                "watcher shell command timed out after {}ms",
                timeout_ms.unwrap_or_default()
            )))
        } else {
            Err(AppError::msg(format!(
                "watcher shell command failed: {}",
                payload.trim()
            )))
        }
    }

    fn mark_watcher_errored(&self, id: &str) -> Result<()> {
        let mut watcher = self.store.get_watcher(id)?;
        watcher.status = WatcherStatus::Errored;
        watcher.updated_at = now_ts();
        self.store.update_watcher(&watcher, watcher.version)?;
        Ok(())
    }

    pub fn stop_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        let mut watcher = self.resolve_watcher(id_or_name)?;
        watcher.status = WatcherStatus::Stopped;
        watcher.updated_at = now_ts();
        let watcher = self.store.update_watcher(&watcher, watcher.version)?;
        self.store.append_watcher_event(&WatcherEventRecord {
            id: Uuid::new_v4().simple().to_string(),
            watcher_id: watcher.id.clone(),
            source: "controller".to_string(),
            event_type: "stopped".to_string(),
            payload_ref: "{}".to_string(),
            signature_status: "not_applicable".to_string(),
            route_decision: "none".to_string(),
            delivered: true,
            created_at: now_ts(),
        })?;
        Ok(watcher)
    }

    pub fn delete_watcher(&self, id_or_name: &str) -> Result<()> {
        self.ensure_writable()?;
        let watcher = self.resolve_watcher(id_or_name)?;
        self.store.delete_watcher(&watcher.id)
    }

    pub fn ingest_watcher_event(
        &self,
        id_or_name: &str,
        source: String,
        event_type: String,
        payload_ref: String,
        signature_status: String,
    ) -> Result<WatcherEventRecord> {
        self.ensure_writable()?;
        let watcher = self.resolve_watcher(id_or_name)?;
        if watcher.status != WatcherStatus::Running {
            return Err(AppError::msg("watcher is not running"));
        }
        if let Some(project_id) = watcher.project_id.as_deref() {
            let project = self.store.get_project(project_id)?;
            require_project_trust(project.trust_state, TrustGatedOperation::Watchers)?;
        }

        let config = watcher_config(&watcher.config_ref)?;
        if watcher_requires_verified_signature(&config)
            && !signature_status.eq_ignore_ascii_case("verified")
        {
            return Err(AppError::msg(
                "watcher event signature must be verified before ingestion",
            ));
        }
        let mut route_decision = "external".to_string();
        let mut delivered = true;
        if let Some(session_id) = watcher_route_session_id(&config) {
            route_decision = format!("session:{session_id}");
            if self.send(session_id, &payload_ref).is_err() {
                delivered = false;
            }
        }

        self.store.append_watcher_event(&WatcherEventRecord {
            id: Uuid::new_v4().simple().to_string(),
            watcher_id: watcher.id.clone(),
            source,
            event_type,
            payload_ref,
            signature_status,
            route_decision,
            delivered,
            created_at: now_ts(),
        })
    }

    pub fn list_watcher_events(
        &self,
        id_or_name: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<WatcherEventRecord>> {
        self.store.init()?;
        let watcher = self.resolve_watcher(id_or_name)?;
        self.store.list_watcher_events(&watcher.id, offset, limit)
    }

    pub fn list_conductors(&self) -> Result<Vec<ConductorRecord>> {
        self.store.init()?;
        self.store.list_conductors(&self.config.profile)
    }

    pub fn get_conductor(&self, id: &str) -> Result<ConductorRecord> {
        self.store.init()?;
        self.store.get_conductor(id)
    }

    pub fn create_conductor(&self, session_id: String) -> Result<ConductorRecord> {
        self.ensure_writable()?;
        self.get_session(&session_id)?;
        let now = now_ts();
        self.store.create_conductor(&ConductorRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            session_id,
            status: ConductorStatus::Stopped,
            watched_sessions: "[]".to_string(),
            channel_bindings: "{}".to_string(),
            last_heartbeat_at: 0,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn start_conductor(&self, id: &str) -> Result<ConductorRecord> {
        self.ensure_writable()?;
        let mut conductor = self.store.get_conductor(id)?;
        conductor.status = ConductorStatus::Running;
        conductor.last_heartbeat_at = now_ts();
        conductor.updated_at = conductor.last_heartbeat_at;
        self.store.update_conductor(&conductor, conductor.version)
    }

    pub fn heartbeat_conductor(&self, id: &str) -> Result<ConductorRecord> {
        self.ensure_writable()?;
        let mut conductor = self.store.get_conductor(id)?;
        conductor.last_heartbeat_at = now_ts();
        conductor.updated_at = conductor.last_heartbeat_at;
        self.store.update_conductor(&conductor, conductor.version)
    }

    pub fn stop_conductor(&self, id: &str) -> Result<ConductorRecord> {
        self.ensure_writable()?;
        let mut conductor = self.store.get_conductor(id)?;
        conductor.status = ConductorStatus::Stopped;
        conductor.updated_at = now_ts();
        self.store.update_conductor(&conductor, conductor.version)
    }

    pub fn delete_conductor(&self, id: &str) -> Result<()> {
        self.ensure_writable()?;
        let conductor = self.store.get_conductor(id)?;
        self.store.delete_conductor(&conductor.id)
    }

    pub fn list_conductor_assignments(&self, id: &str) -> Result<Vec<ConductorAssignmentRecord>> {
        self.store.init()?;
        let conductor = self.store.get_conductor(id)?;
        self.store.list_conductor_assignments(&conductor.id)
    }

    pub fn send_conductor(
        &self,
        conductor_id: &str,
        session_id: String,
        task_ref: String,
    ) -> Result<ConductorAssignmentRecord> {
        self.ensure_writable()?;
        let conductor = self.store.get_conductor(conductor_id)?;
        if conductor.status != ConductorStatus::Running {
            return Err(AppError::msg("conductor is not running"));
        }
        self.get_session(&session_id)?;
        self.send(&session_id, &task_ref)?;
        let now = now_ts();
        self.store
            .append_conductor_assignment(&ConductorAssignmentRecord {
                id: Uuid::new_v4().simple().to_string(),
                conductor_id: conductor_id.to_string(),
                session_id,
                task_ref,
                status: "assigned".to_string(),
                assigned_at: now,
                completed_at: 0,
            })
    }

    pub fn complete_conductor_assignment(
        &self,
        assignment_id: &str,
        status: &str,
    ) -> Result<ConductorAssignmentRecord> {
        self.ensure_writable()?;
        let mut assignment = self.store.get_conductor_assignment(assignment_id)?;
        let status = terminal_assignment_status(status)?;
        if assignment.completed_at != 0 {
            if assignment.status == status {
                return Ok(assignment);
            }
            return Err(AppError::msg(format!(
                "assignment already closed as {}",
                assignment.status
            )));
        }
        assignment.status = status.to_string();
        assignment.completed_at = now_ts();
        self.store.update_conductor_assignment(&assignment)
    }

    pub fn cost_summary(&self, filter: CostFilter) -> Result<CostSummary> {
        self.store.init()?;
        self.store.cost_summary(&filter)
    }

    pub fn cost_events(
        &self,
        filter: CostFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CostEvent>> {
        self.store.init()?;
        self.store.cost_events(&filter, offset, limit)
    }

    pub fn record_cost(
        &self,
        session_id: &str,
        amount_usd: f64,
        payload: serde_json::Value,
    ) -> Result<CostEvent> {
        self.ensure_writable()?;
        if !amount_usd.is_finite() || amount_usd < 0.0 {
            return Err(AppError::msg(
                "cost amount must be a finite non-negative number",
            ));
        }
        let session = self.get_session(session_id)?;
        let mut payload = payload;
        validate_cost_payload(&payload)?;
        if let Some(object) = payload.as_object_mut() {
            object
                .entry("agent".to_string())
                .or_insert_with(|| json!(session.agent));
        }
        self.store
            .append_cost_event(session_id, amount_usd, payload)
    }

    pub fn record_session_event(
        &self,
        id: &str,
        kind: &str,
        payload: Value,
    ) -> Result<crate::models::SessionEvent> {
        self.ensure_writable()?;
        self.get_session(id)?;
        let kind = kind.trim();
        if kind != "agent_state" {
            return Err(AppError::msg("only agent_state events can be recorded"));
        }
        self.store
            .append_session_event(id, kind, normalize_agent_state_payload(payload)?)
    }

    pub fn events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<crate::models::SessionEvent>> {
        self.get_session(id)?;
        self.store.session_events(id, since, limit)
    }

    pub fn structured_events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<StructuredEvent>> {
        let session = self.get_session(id)?;
        self.events(id, since, limit)?
            .iter()
            .map(|event| self.agents.parse_structured_event(&session.agent, event))
            .collect()
    }

    pub fn sync_agent_state(&self, id: &str) -> Result<AgentStateSyncResult> {
        self.ensure_writable()?;
        let session = self.get_session(id)?;
        let Some((source, payload)) = latest_transcript_agent_state(&session)? else {
            return Ok(AgentStateSyncResult {
                session_id: session.id,
                synced: false,
                source: transcript_source_for_agent(&session.agent),
                event: None,
            });
        };
        if self.latest_agent_state_matches(
            &session.id,
            source,
            payload.get("transcript_path").and_then(Value::as_str),
            payload.get("transcript_line").and_then(Value::as_i64),
        )? {
            return Ok(AgentStateSyncResult {
                session_id: session.id,
                synced: false,
                source: source.to_string(),
                event: None,
            });
        }
        let event = self.record_session_event(&session.id, "agent_state", payload)?;
        Ok(AgentStateSyncResult {
            session_id: session.id,
            synced: true,
            source: source.to_string(),
            event: Some(event),
        })
    }

    pub fn search_sessions(&self, query: &str, limit: usize) -> Result<SessionSearchResponse> {
        let query = query.trim();
        if query.is_empty() {
            return Err(AppError::msg("search query is required"));
        }

        let needle = query.to_ascii_lowercase();
        let mut results = Vec::new();
        let limit = limit.max(1);

        'sessions: for session in self.list_sessions()? {
            if results.len() >= limit {
                break;
            }

            if let Some(snippet) = search_text(&session_search_blob(&session), &needle) {
                results.push(search_result(&session, "session", snippet));
                continue 'sessions;
            }

            for event in self.events(&session.id, 0, 1_000)? {
                if let Some(snippet) = search_text(&event_search_blob(&event.payload), &needle) {
                    results.push(search_result(&session, &event.kind, snippet));
                    continue 'sessions;
                }
            }

            if results.len() >= limit {
                break;
            }

            if let Ok(output) = self.output(&session.id, 200, false)
                && let Some(snippet) = search_text(&output.text, &needle)
            {
                results.push(search_result(&session, "output", snippet));
            }
        }

        if results.len() < limit {
            results.extend(search_claude_transcripts(
                query,
                &needle,
                limit - results.len(),
            )?);
        }
        if results.len() < limit {
            results.extend(search_codex_transcripts(&needle, limit - results.len())?);
        }

        Ok(SessionSearchResponse {
            query: query.to_string(),
            results,
        })
    }

    fn ensure_project(&self, path: &str, trusted: bool) -> Result<ProjectRecord> {
        let project_ref = self.workspace.resolve_project_ref(path)?;
        let root_path = project_ref.root.to_string_lossy().to_string();
        if let Some(project) = self
            .store
            .list_projects(&self.config.profile)?
            .into_iter()
            .find(|project| project.root_path == root_path)
        {
            return Ok(project);
        }

        let now = now_ts();
        self.store.create_project(&ProjectRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            root_path: root_path.clone(),
            repo_identity: project_ref
                .repo_identity
                .unwrap_or_else(|| root_path.clone()),
            default_branch: project_ref
                .default_branch
                .unwrap_or_else(|| "main".to_string()),
            trust_state: if trusted {
                ProjectTrustState::Trusted
            } else {
                ProjectTrustState::Untrusted
            },
            hooks_hash: String::new(),
            config_hash: String::new(),
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    fn ensure_mcp_trust(&self) -> Result<()> {
        for attachment in self.list_mcp_attachments()? {
            if attachment.status == AttachmentStatus::Attached {
                self.ensure_attachment_trust(
                    attachment.project_id.as_deref(),
                    attachment.session_id.as_deref(),
                    TrustGatedOperation::Mcp,
                )?;
            }
        }
        Ok(())
    }

    fn ensure_skill_trust(&self) -> Result<()> {
        for attachment in self.list_skill_attachments()? {
            if attachment.status == AttachmentStatus::Attached {
                self.ensure_attachment_trust(
                    attachment.project_id.as_deref(),
                    attachment.session_id.as_deref(),
                    TrustGatedOperation::Skills,
                )?;
            }
        }
        Ok(())
    }

    fn ensure_attachment_trust(
        &self,
        project_id: Option<&str>,
        session_id: Option<&str>,
        operation: TrustGatedOperation,
    ) -> Result<()> {
        let Some(project_id) = project_id.map(str::to_string).or_else(|| {
            session_id
                .and_then(|id| self.store.get_session(id).ok())
                .map(|session| session.project_id)
        }) else {
            return Ok(());
        };
        let project = self.store.get_project(&project_id)?;
        require_project_trust(project.trust_state, operation)
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.read_only {
            Err(AppError::msg("read-only mode rejects mutations"))
        } else {
            Ok(())
        }
    }

    fn uses_agent_state_hooks(&self, agent: &str) -> Result<bool> {
        Ok(self.agents.capabilities(agent)?.agent_state_hooks)
    }

    fn installs_agent_state_hooks(&self, session: &SessionRecord) -> Result<bool> {
        if !self.uses_agent_state_hooks(&session.agent)? || session.workspace_id.is_empty() {
            return Ok(false);
        }
        let workspace = self.store.get_workspace(&session.workspace_id)?;
        Ok(workspace.sandbox_id.is_none())
    }

    fn append_runtime_agent_state_event(
        &self,
        session: &SessionRecord,
        state: &str,
        source: &str,
    ) -> Result<()> {
        if source == "runtime_start" && self.installs_agent_state_hooks(session)? {
            return Ok(());
        }
        self.append_agent_state_event(&session.id, state, source)
    }

    fn latest_agent_state_matches(
        &self,
        session_id: &str,
        source: &str,
        transcript_path: Option<&str>,
        transcript_line: Option<i64>,
    ) -> Result<bool> {
        let Some(transcript_path) = transcript_path else {
            return Ok(false);
        };
        let Some(transcript_line) = transcript_line else {
            return Ok(false);
        };
        Ok(self
            .store
            .latest_session_agent_state_events(session_id, 50)?
            .iter()
            .any(|event| {
                event.payload["source"].as_str() == Some(source)
                    && event.payload["transcript_path"].as_str() == Some(transcript_path)
                    && event.payload["transcript_line"].as_i64() == Some(transcript_line)
            }))
    }
}

fn defaulted(value: String, default: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    }
}

fn validate_cost_payload(payload: &Value) -> Result<()> {
    let Some(object) = payload.as_object() else {
        return Ok(());
    };
    for key in ["input_tokens", "output_tokens", "total_tokens"] {
        if let Some(value) = object.get(key) {
            validate_cost_token_count(key, value)?;
        }
    }
    Ok(())
}

fn validate_cost_token_count(key: &str, value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let Some(number) = value.as_number() else {
        return Err(AppError::msg(format!(
            "cost {key} must be a non-negative integer"
        )));
    };
    if number.as_i64().is_some_and(|count| count >= 0) {
        return Ok(());
    }
    if number
        .as_u64()
        .is_some_and(|count| i64::try_from(count).is_ok())
    {
        return Ok(());
    }
    Err(AppError::msg(format!(
        "cost {key} must be a non-negative integer"
    )))
}

fn group_path(parent: Option<&str>, name: &str, default: &str) -> Result<String> {
    let name = name.trim().trim_matches('/');
    if name.is_empty() {
        return Err(AppError::msg("group name is required"));
    }
    let parent = parent
        .map(str::trim)
        .map(|parent| parent.trim_matches('/'))
        .filter(|parent| !parent.is_empty() && *parent != "root" && *parent != default);
    Ok(match parent {
        Some(parent) => format!("{parent}/{name}"),
        None => name.to_string(),
    })
}

fn parent_group(name: &str) -> Option<String> {
    name.rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .filter(|parent| !parent.is_empty())
}

fn search_result(session: &SessionRecord, source: &str, snippet: String) -> SessionSearchResult {
    SessionSearchResult {
        session_id: session.id.clone(),
        session_name: session.name.clone(),
        group_name: session.group_name.clone(),
        agent: session.agent.clone(),
        cwd: session.project_path.clone(),
        source: source.to_string(),
        snippet,
    }
}

fn validate_watcher_config(
    adapter_id: &str,
    project_id: Option<&str>,
    config_ref: &str,
) -> Result<()> {
    match adapter_id {
        "manual" => Ok(()),
        "shell" => {
            let config = watcher_config(config_ref)?;
            required_watcher_command(&config)?;
            watcher_timeout_ms(&config)?;
            if project_id.is_none() {
                return Err(AppError::msg("shell watcher requires project"));
            }
            Ok(())
        }
        other => Err(AppError::msg(format!(
            "unsupported watcher adapter: {other}"
        ))),
    }
}

fn watcher_supports_poll(adapter_id: &str) -> bool {
    adapter_id == "shell"
}

fn watcher_config(config_ref: &str) -> Result<serde_json::Value> {
    serde_json::from_str(config_ref)
        .map_err(|err| AppError::msg(format!("invalid watcher config: {err}")))
}

fn required_watcher_command(config: &serde_json::Value) -> Result<&str> {
    config
        .get("command")
        .and_then(|value| value.as_str())
        .filter(|command| !command.trim().is_empty())
        .ok_or_else(|| AppError::msg("shell watcher requires command"))
}

fn watcher_timeout_ms(config: &serde_json::Value) -> Result<Option<u64>> {
    match config.get("timeout_ms") {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|timeout| *timeout > 0)
            .map(Some)
            .ok_or_else(|| AppError::msg("shell watcher timeout_ms must be a positive integer")),
    }
}

fn watcher_route_session_id(config: &serde_json::Value) -> Option<&str> {
    config
        .get("session_id")
        .and_then(|value| value.as_str())
        .filter(|session_id| !session_id.trim().is_empty())
}

fn watcher_requires_verified_signature(config: &serde_json::Value) -> bool {
    config
        .get("require_signature")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

struct ShellWatcherCommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

fn run_shell_watcher_command(
    project_root: &str,
    shell_command: &str,
    timeout_ms: Option<u64>,
) -> Result<ShellWatcherCommandOutput> {
    let mut command = Command::new("sh");
    command
        .arg("-lc")
        .arg(shell_command)
        .current_dir(project_root);
    if let Some(timeout_ms) = timeout_ms {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if child.try_wait()?.is_some() {
                let output = child.wait_with_output()?;
                return Ok(ShellWatcherCommandOutput {
                    success: output.status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    timed_out: false,
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output()?;
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                return Ok(ShellWatcherCommandOutput {
                    success: false,
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: if stderr.is_empty() {
                        format!("timed out after {timeout_ms}ms")
                    } else {
                        stderr
                    },
                    timed_out: true,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    let output = command.output()?;
    Ok(ShellWatcherCommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        timed_out: false,
    })
}

fn git_text<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    git_text_allow_exit(cwd, args, &[0])
}

fn git_text_allow_exit<const N: usize>(
    cwd: &Path,
    args: [&str; N],
    allowed_codes: &[i32],
) -> Result<String> {
    let output = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if output
        .status
        .code()
        .is_some_and(|code| allowed_codes.contains(&code))
    {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(AppError::msg(format!(
        "git command failed for {}: {}",
        cwd.display(),
        stderr.trim()
    )))
}

fn diff_section(title: &str, mut body: String) -> String {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    format!("## {title}\n{body}")
}

fn transcript_search_result(
    session_id: String,
    cwd: String,
    group_name: &str,
    agent: &str,
    source: &str,
    source_path: &Path,
    snippet: String,
) -> SessionSearchResult {
    let session_name = source_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(&session_id)
        .to_string();
    SessionSearchResult {
        session_id,
        session_name,
        group_name: group_name.to_string(),
        agent: agent.to_string(),
        cwd,
        source: source.to_string(),
        snippet,
    }
}

fn session_search_blob(session: &SessionRecord) -> String {
    [
        session.id.as_str(),
        session.name.as_str(),
        session.group_name.as_str(),
        session.agent.as_str(),
        session.command.as_str(),
        session.project_path.as_str(),
        session.status.as_str(),
    ]
    .join("\n")
}

fn search_claude_transcripts(
    query: &str,
    needle: &str,
    limit: usize,
) -> Result<Vec<SessionSearchResult>> {
    let Some(root) = claude_projects_dir() else {
        return Ok(Vec::new());
    };
    search_claude_transcripts_in(&root, query, needle, limit)
}

fn search_codex_transcripts(needle: &str, limit: usize) -> Result<Vec<SessionSearchResult>> {
    let Some(root) = codex_sessions_dir() else {
        return Ok(Vec::new());
    };
    search_codex_transcripts_in(&root, needle, limit)
}

fn claude_projects_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude").join("projects"))
}

fn codex_sessions_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex").join("sessions"))
}

fn search_claude_transcripts_in(
    root: &Path,
    _query: &str,
    needle: &str,
    limit: usize,
) -> Result<Vec<SessionSearchResult>> {
    if limit == 0 || !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;
    files.sort();

    let mut results = Vec::new();
    for file in files {
        if results.len() >= limit {
            break;
        }
        if let Some(result) = search_claude_transcript_file(&file, needle)? {
            results.push(result);
        }
    }
    Ok(results)
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn recent_jsonl_files(root: &Path, session: &SessionRecord, limit: usize) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let min_modified_at = session.created_at.saturating_sub(60);
    collect_recent_jsonl_files(root, min_modified_at, &mut files)?;
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    files.truncate(limit);
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

fn collect_recent_jsonl_files(
    dir: &Path,
    min_modified_at: i64,
    files: &mut Vec<(i64, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_recent_jsonl_files(&path, min_modified_at, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let modified_at = modified_at_secs(&path).unwrap_or(0);
            if modified_at >= min_modified_at {
                files.push((modified_at, path));
            }
        }
    }
    Ok(())
}

fn modified_at_secs(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

fn created_at_secs(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .created()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

fn transcript_file_started_near_session(file: &Path, session: &SessionRecord) -> bool {
    let Some(created_at) = created_at_secs(file) else {
        return false;
    };
    created_at >= session.created_at.saturating_sub(60)
        && created_at
            <= session
                .created_at
                .saturating_add(TRANSCRIPT_CWD_MATCH_START_TOLERANCE_SECS)
}

fn search_claude_transcript_file(path: &Path, needle: &str) -> Result<Option<SessionSearchResult>> {
    let file = fs::File::open(path)?;
    let mut fallback_session_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("claude-transcript")
        .to_string();
    let mut fallback_cwd = String::new();

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(session_id) =
            json_string(&value, "sessionId").or_else(|| json_string(&value, "session_id"))
        {
            fallback_session_id = session_id.to_string();
        }
        if let Some(cwd) = json_string(&value, "cwd") {
            fallback_cwd = cwd.to_string();
        }
        let text = claude_message_text(&value).unwrap_or_else(|| line.clone());
        if let Some(snippet) = search_text(&text, needle) {
            return Ok(Some(transcript_search_result(
                fallback_session_id,
                fallback_cwd,
                "claude",
                "claude",
                "claude_transcript",
                path,
                snippet,
            )));
        }
    }
    Ok(None)
}

fn search_codex_transcripts_in(
    root: &Path,
    needle: &str,
    limit: usize,
) -> Result<Vec<SessionSearchResult>> {
    if limit == 0 || !root.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;
    files.sort();
    let mut results = Vec::new();
    for file in files {
        if results.len() >= limit {
            break;
        }
        if let Some(result) = search_codex_transcript_file(&file, needle)? {
            results.push(result);
        }
    }
    Ok(results)
}

fn search_codex_transcript_file(path: &Path, needle: &str) -> Result<Option<SessionSearchResult>> {
    let file = fs::File::open(path)?;
    let mut fallback_session_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("codex-transcript")
        .to_string();
    let mut fallback_cwd = String::new();

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(session_id) = transcript_session_id(&value) {
            fallback_session_id = session_id.to_string();
        }
        if let Some(cwd) = codex_transcript_cwd(&value) {
            fallback_cwd = cwd.to_string();
        }
        let text = codex_search_text(&value).unwrap_or_else(|| line.clone());
        if let Some(snippet) = search_text(&text, needle) {
            return Ok(Some(transcript_search_result(
                fallback_session_id,
                fallback_cwd,
                "codex",
                "codex",
                "codex_transcript",
                path,
                snippet,
            )));
        }
    }
    Ok(None)
}

const SNAPSHOT_TRANSCRIPT_DISCOVERY_LIMIT: usize = 16;
const SNAPSHOT_TRANSCRIPT_DISCOVERY_RETRY_SECS: i64 = 3;
const TRANSCRIPT_CWD_MATCH_START_TOLERANCE_SECS: i64 = 600;

fn latest_transcript_agent_state(session: &SessionRecord) -> Result<Option<(&'static str, Value)>> {
    match session.agent.as_str() {
        "claude" => {
            let Some(root) = claude_projects_dir() else {
                return Ok(None);
            };
            latest_claude_agent_state_in(&root, session)
                .map(|payload| payload.map(|payload| ("claude_transcript", payload)))
        }
        "codex" => {
            let Some(root) = codex_sessions_dir() else {
                return Ok(None);
            };
            latest_codex_agent_state_in(&root, session)
                .map(|payload| payload.map(|payload| ("codex_transcript", payload)))
        }
        _ => Ok(None),
    }
}

fn latest_recent_transcript_agent_state(
    session: &SessionRecord,
) -> Result<Option<(&'static str, Value)>> {
    match session.agent.as_str() {
        "claude" => {
            let Some(root) = claude_projects_dir() else {
                return Ok(None);
            };
            latest_recent_claude_agent_state_in(&root, session, SNAPSHOT_TRANSCRIPT_DISCOVERY_LIMIT)
                .map(|payload| payload.map(|payload| ("claude_transcript", payload)))
        }
        "codex" => {
            let Some(root) = codex_sessions_dir() else {
                return Ok(None);
            };
            latest_recent_codex_agent_state_in(&root, session, SNAPSHOT_TRANSCRIPT_DISCOVERY_LIMIT)
                .map(|payload| payload.map(|payload| ("codex_transcript", payload)))
        }
        _ => Ok(None),
    }
}

fn latest_transcript_agent_state_from_events(
    session: &SessionRecord,
    recent_events: &[crate::models::SessionEvent],
) -> Result<Option<(&'static str, Value)>> {
    let source = transcript_source_for_agent(&session.agent);
    if source == "unsupported_agent" {
        return Ok(None);
    }
    let Some(path) = recent_events.iter().find_map(|event| {
        if event.kind != "agent_state"
            || event.payload.get("source").and_then(Value::as_str) != Some(source.as_str())
        {
            return None;
        }
        event
            .payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }) else {
        return Ok(None);
    };

    latest_transcript_agent_state_from_path(session, &path, true)
}

fn latest_transcript_agent_state_from_path(
    session: &SessionRecord,
    path: &Path,
    allow_cwd_match: bool,
) -> Result<Option<(&'static str, Value)>> {
    match session.agent.as_str() {
        "claude" => latest_claude_agent_state_file(path, session, allow_cwd_match)
            .map(|payload| payload.map(|payload| ("claude_transcript", payload))),
        "codex" => latest_codex_agent_state_file(path, session, allow_cwd_match)
            .map(|payload| payload.map(|payload| ("codex_transcript", payload))),
        _ => Ok(None),
    }
}

fn transcript_source_for_agent(agent: &str) -> String {
    match agent {
        "claude" => "claude_transcript",
        "codex" => "codex_transcript",
        _ => "unsupported_agent",
    }
    .to_string()
}

fn latest_claude_agent_state_in(root: &Path, session: &SessionRecord) -> Result<Option<Value>> {
    if !root.exists() {
        return Ok(None);
    }
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;
    files.sort();

    let mut latest = None;
    for file in files {
        if let Some(payload) = latest_claude_agent_state_file(&file, session, false)? {
            latest = Some(payload);
        }
    }
    Ok(latest)
}

fn latest_recent_claude_agent_state_in(
    root: &Path,
    session: &SessionRecord,
    limit: usize,
) -> Result<Option<Value>> {
    if !root.exists() {
        return Ok(None);
    }

    for file in recent_jsonl_files(root, session, limit)? {
        if let Some(payload) = latest_claude_agent_state_file(&file, session, true)? {
            return Ok(Some(payload));
        }
    }

    Ok(None)
}

fn latest_claude_agent_state_file(
    file: &Path,
    session: &SessionRecord,
    allow_cwd_match: bool,
) -> Result<Option<Value>> {
    if !file.exists() {
        return Ok(None);
    }
    let stem_matches_session = file
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == session.id);
    let allow_cwd_match = allow_cwd_match && transcript_file_started_near_session(file, session);
    let reader = BufReader::new(fs::File::open(file)?);
    let mut entries = Vec::new();
    let mut metadata = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !stem_matches_session
            && !claude_transcript_matches_session(&value, session, allow_cwd_match)
        {
            continue;
        }
        let line = (index + 1) as i64;
        metadata.push((line, value.clone()));
        entries.push(TranscriptEntry { line, value });
    }
    let Some(mut state) = AgentRegistry.derive_transcript_agent_state("claude", &entries)? else {
        return Ok(None);
    };
    if let Some(object) = state.payload.as_object_mut() {
        object.insert(
            "transcript_path".to_string(),
            json!(file.to_string_lossy().to_string()),
        );
        object.insert("transcript_line".to_string(), json!(state.line));
        if let Some(value) = metadata
            .iter()
            .find(|(line, _)| *line == state.line)
            .map(|(_, value)| value)
        {
            if let Some(session_id) = json_string(value, "agent_helm_session_id")
                .or_else(|| json_string(value, "agentHelmSessionId"))
                .or_else(|| json_string(value, "sessionId"))
                .or_else(|| json_string(value, "session_id"))
            {
                object.insert("transcript_session_id".to_string(), json!(session_id));
            }
            if let Some(cwd) = json_string(value, "cwd") {
                object.insert("cwd".to_string(), json!(cwd));
            }
        }
    }
    Ok(Some(state.payload))
}

fn claude_transcript_matches_session(
    value: &Value,
    session: &SessionRecord,
    allow_cwd_match: bool,
) -> bool {
    let session_id_matches = json_string(value, "agent_helm_session_id")
        .or_else(|| json_string(value, "agentHelmSessionId"))
        .or_else(|| json_string(value, "sessionId"))
        .or_else(|| json_string(value, "session_id"))
        .is_some_and(|id| id == session.id);
    session_id_matches
        || (allow_cwd_match
            && json_string(value, "cwd").is_some_and(|cwd| cwd == session.project_path))
}

fn latest_codex_agent_state_in(root: &Path, session: &SessionRecord) -> Result<Option<Value>> {
    if !root.exists() {
        return Ok(None);
    }
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;
    files.sort();

    let mut latest = None;
    for file in files {
        let stem_matches_session = file
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == session.id);
        let reader = BufReader::new(fs::File::open(&file)?);
        let mut rows = Vec::new();
        let mut file_matches_session = stem_matches_session;
        let mut file_cwd = None;
        let mut file_session_id = None;
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(cwd) = codex_transcript_cwd(&value) {
                file_cwd = Some(cwd.to_string());
            }
            if let Some(session_id) = transcript_session_id(&value) {
                file_session_id = Some(session_id.to_string());
            }
            if codex_transcript_matches_session(&value, session, false) {
                file_matches_session = true;
            }
            rows.push((index + 1, value));
        }
        if !file_matches_session {
            continue;
        }

        let entries = rows
            .iter()
            .map(|(line, value)| TranscriptEntry {
                line: *line as i64,
                value: value.clone(),
            })
            .collect::<Vec<_>>();
        let Some(mut state) = AgentRegistry.derive_transcript_agent_state("codex", &entries)?
        else {
            continue;
        };
        let state_value = rows
            .iter()
            .find(|(line, _)| *line as i64 == state.line)
            .map(|(_, value)| value);
        if let Some(object) = state.payload.as_object_mut() {
            object.insert(
                "transcript_path".to_string(),
                json!(file.to_string_lossy().to_string()),
            );
            object.insert("transcript_line".to_string(), json!(state.line));
            if let Some(session_id) = state_value.and_then(transcript_session_id) {
                object.insert("transcript_session_id".to_string(), json!(session_id));
            } else if let Some(session_id) = file_session_id.as_deref() {
                object.insert("transcript_session_id".to_string(), json!(session_id));
            }
            if let Some(cwd) = state_value.and_then(codex_transcript_cwd) {
                object.insert("cwd".to_string(), json!(cwd));
            } else if let Some(cwd) = file_cwd.as_deref() {
                object.insert("cwd".to_string(), json!(cwd));
            }
        }
        latest = Some(state.payload);
    }
    Ok(latest)
}

fn latest_recent_codex_agent_state_in(
    root: &Path,
    session: &SessionRecord,
    limit: usize,
) -> Result<Option<Value>> {
    if !root.exists() {
        return Ok(None);
    }

    for file in recent_jsonl_files(root, session, limit)? {
        if let Some(payload) = latest_codex_agent_state_file(&file, session, true)? {
            return Ok(Some(payload));
        }
    }

    Ok(None)
}

fn latest_codex_agent_state_file(
    file: &Path,
    session: &SessionRecord,
    allow_cwd_match: bool,
) -> Result<Option<Value>> {
    if !file.exists() {
        return Ok(None);
    }
    let stem_matches_session = file
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == session.id);
    let allow_cwd_match = allow_cwd_match && transcript_file_started_near_session(file, session);
    let reader = BufReader::new(fs::File::open(file)?);
    let mut rows = Vec::new();
    let mut file_matches_session = stem_matches_session;
    let mut file_cwd = None;
    let mut file_session_id = None;
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = codex_transcript_cwd(&value) {
            file_cwd = Some(cwd.to_string());
        }
        if let Some(session_id) = transcript_session_id(&value) {
            file_session_id = Some(session_id.to_string());
        }
        if codex_transcript_matches_session(&value, session, allow_cwd_match) {
            file_matches_session = true;
        }
        rows.push((index + 1, value));
    }
    if !file_matches_session {
        return Ok(None);
    }

    let entries = rows
        .iter()
        .map(|(line, value)| TranscriptEntry {
            line: *line as i64,
            value: value.clone(),
        })
        .collect::<Vec<_>>();
    let Some(mut state) = AgentRegistry.derive_transcript_agent_state("codex", &entries)? else {
        return Ok(None);
    };
    let state_value = rows
        .iter()
        .find(|(line, _)| *line as i64 == state.line)
        .map(|(_, value)| value);
    if let Some(object) = state.payload.as_object_mut() {
        object.insert(
            "transcript_path".to_string(),
            json!(file.to_string_lossy().to_string()),
        );
        object.insert("transcript_line".to_string(), json!(state.line));
        if let Some(session_id) = state_value.and_then(transcript_session_id) {
            object.insert("transcript_session_id".to_string(), json!(session_id));
        } else if let Some(session_id) = file_session_id.as_deref() {
            object.insert("transcript_session_id".to_string(), json!(session_id));
        }
        if let Some(cwd) = state_value.and_then(codex_transcript_cwd) {
            object.insert("cwd".to_string(), json!(cwd));
        } else if let Some(cwd) = file_cwd.as_deref() {
            object.insert("cwd".to_string(), json!(cwd));
        }
    }
    Ok(Some(state.payload))
}

fn codex_transcript_matches_session(
    value: &Value,
    session: &SessionRecord,
    allow_cwd_match: bool,
) -> bool {
    transcript_session_id(value).is_some_and(|id| id == session.id)
        || (allow_cwd_match
            && codex_transcript_cwd(value).is_some_and(|cwd| cwd == session.project_path))
}

fn codex_event_payload(value: &Value) -> &Value {
    value.get("payload").unwrap_or(value)
}

fn transcript_session_id(value: &Value) -> Option<&str> {
    json_string(value, "agent_helm_session_id")
        .or_else(|| json_string(value, "agentHelmSessionId"))
        .or_else(|| json_string(value, "sessionId"))
        .or_else(|| json_string(value, "session_id"))
        .or_else(|| {
            value.get("payload").and_then(|payload| {
                json_string(payload, "agent_helm_session_id")
                    .or_else(|| json_string(payload, "agentHelmSessionId"))
                    .or_else(|| json_string(payload, "sessionId"))
                    .or_else(|| json_string(payload, "session_id"))
            })
        })
}

fn codex_transcript_cwd(value: &Value) -> Option<&str> {
    json_string(value, "cwd").or_else(|| {
        value
            .get("payload")
            .and_then(|payload| json_string(payload, "cwd"))
    })
}

fn codex_tool_name(value: &Value) -> Option<String> {
    json_string(value, "name")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn codex_search_text(value: &Value) -> Option<String> {
    let payload = codex_event_payload(value);
    let mut parts = Vec::new();
    for field in ["message", "text", "output", "arguments"] {
        if let Some(text) = json_string(payload, field).or_else(|| json_string(value, field)) {
            parts.push(text.to_string());
        }
    }
    if let Some(text) = payload
        .get("content")
        .or_else(|| value.get("content"))
        .and_then(json_content_text)
    {
        parts.push(text);
    }
    if let Some(text) = payload
        .get("text_elements")
        .or_else(|| value.get("text_elements"))
        .and_then(json_content_text)
    {
        parts.push(text);
    }
    if let Some(name) = codex_tool_name(payload) {
        parts.push(name);
    }
    let text = parts.join("\n");
    (!text.is_empty()).then_some(text)
}

fn claude_message_text(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    json_content_text(content)
}

fn json_content_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    json_string(item, "text")
                        .or_else(|| json_string(item, "content"))
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        serde_json::Value::Object(_) => json_string(value, "text")
            .or_else(|| json_string(value, "content"))
            .map(str::to_string),
        _ => None,
    }
}

fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|value| value.as_str())
}

fn event_search_blob(payload: &serde_json::Value) -> String {
    payload
        .get("preview")
        .and_then(|value| value.as_str())
        .or_else(|| {
            payload
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|value| value.as_str())
        })
        .map(str::to_string)
        .unwrap_or_else(|| payload.to_string())
}

fn search_text(text: &str, needle: &str) -> Option<String> {
    text.lines()
        .find(|line| line.to_ascii_lowercase().contains(needle))
        .map(|line| clipped_snippet(line.trim()))
}

fn clipped_snippet(line: &str) -> String {
    const MAX: usize = 180;
    if line.len() <= MAX {
        return line.to_string();
    }

    let mut end = MAX;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

fn project_name(path: &str) -> String {
    crate::util::project_name(path)
}

fn auto_worktree_branch(agent: &str, name: &str, path: &str) -> String {
    crate::util::auto_worktree_branch(agent, name, path)
}

fn terminal_assignment_status(status: &str) -> Result<&'static str> {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" => Ok("completed"),
        "failed" => Ok("failed"),
        "cancelled" | "canceled" => Ok("cancelled"),
        _ => Err(AppError::msg(
            "assignment status must be completed, failed, or cancelled",
        )),
    }
}

fn normalize_agent_state_payload(payload: Value) -> Result<Value> {
    let Value::Object(mut object) = payload else {
        return Err(AppError::msg("agent_state payload must be an object"));
    };
    let source = match object.get("source") {
        Some(Value::String(source)) => {
            let source = source.trim();
            if source.is_empty() {
                return Err(AppError::msg("agent_state source cannot be empty"));
            }
            source.to_string()
        }
        Some(_) => return Err(AppError::msg("agent_state source must be a string")),
        None => "api".to_string(),
    };
    object.insert("source".to_string(), json!(source.clone()));

    let state = match object
        .get("state")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|state| !state.is_empty())
    {
        Some(state) => state,
        None if source == "gemini_transcript" && has_gemini_transcript_messages(&object) => {
            if let Some(tool) = object.get_mut("tool") {
                normalize_agent_state_tool(tool)?;
            }
            return Ok(Value::Object(object));
        }
        None => return Err(AppError::msg("agent_state state is required")),
    };
    let state = canonical_agent_state(state)?;
    object.insert("state".to_string(), json!(state));

    if let Some(tool) = object.get_mut("tool") {
        normalize_agent_state_tool(tool)?;
    }

    Ok(Value::Object(object))
}

fn has_gemini_transcript_messages(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| !messages.is_empty())
        || object
            .get("conversation")
            .and_then(|conversation| conversation.get("messages"))
            .and_then(Value::as_array)
            .is_some_and(|messages| !messages.is_empty())
}

fn canonical_agent_state(state: &str) -> Result<&'static str> {
    match state.trim().to_ascii_lowercase().as_str() {
        "queued" => Ok("queued"),
        "waiting" => Ok("waiting"),
        "running" => Ok("running"),
        "busy" => Ok("busy"),
        "working" => Ok("working"),
        "thinking" => Ok("thinking"),
        "idle" => Ok("idle"),
        "done" => Ok("done"),
        "ready" => Ok("ready"),
        _ => Err(AppError::msg(
            "agent_state state must be queued, waiting, running, busy, working, thinking, idle, done, or ready",
        )),
    }
}

fn normalize_agent_state_tool(tool: &mut Value) -> Result<()> {
    match tool {
        Value::String(name) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(AppError::msg("agent_state tool name cannot be empty"));
            }
            *tool = json!({ "name": name });
            Ok(())
        }
        Value::Object(object) => {
            let Some(name) = object.get("name") else {
                return Err(AppError::msg("agent_state tool name is required"));
            };
            let Some(name) = name.as_str().map(str::trim).filter(|name| !name.is_empty()) else {
                return Err(AppError::msg("agent_state tool name must be a string"));
            };
            object.insert("name".to_string(), json!(name));
            Ok(())
        }
        _ => Err(AppError::msg(
            "agent_state tool must be an object or string",
        )),
    }
}

#[cfg(feature = "serve")]
impl<R: SessionRuntime> AgentHelmApi for ApplicationController<R> {
    fn default_agent(&self) -> ApiResult<String> {
        Ok(self.config.default_agent.clone())
    }

    fn tool_profiles(&self) -> ApiResult<Vec<ApiToolProfile>> {
        let mut tools = self
            .config
            .tools
            .iter()
            .map(|(name, tool)| ApiToolProfile {
                name: name.clone(),
                installed: tool.installed,
                executable: tool.executable.clone(),
                flags: tool.flags.clone(),
                worktree: tool.worktree.as_str().to_string(),
            })
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| {
            match (
                left.name.as_str() == "shell",
                right.name.as_str() == "shell",
            ) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left.name.cmp(&right.name),
            }
        });
        Ok(tools)
    }

    fn list_sessions(&self) -> ApiResult<Vec<SessionRecord>> {
        ApplicationController::list_sessions(self).map_err(Into::into)
    }

    fn create_session(&self, request: CreateSession) -> ApiResult<SessionRecord> {
        ApplicationController::create_session(self, request).map_err(Into::into)
    }

    fn get_session(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::get_session(self, id).map_err(Into::into)
    }

    fn status(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::status(self, id).map_err(Into::into)
    }

    fn status_snapshot(&self, id: &str) -> ApiResult<SessionStatusSnapshot> {
        ApplicationController::status_snapshot(self, id).map_err(Into::into)
    }

    fn send(&self, id: &str, text: &str) -> ApiResult<()> {
        ApplicationController::send(self, id, text).map_err(Into::into)
    }

    fn output(&self, id: &str, limit: usize, ansi: bool) -> ApiResult<OutputPage> {
        ApplicationController::output(self, id, limit, ansi).map_err(Into::into)
    }

    fn diff(&self, id: &str) -> ApiResult<String> {
        ApplicationController::diff(self, id).map_err(Into::into)
    }

    fn session_materialization(&self, id: &str) -> ApiResult<SessionMaterializationPlan> {
        ApplicationController::session_materialization(self, id).map_err(Into::into)
    }

    fn stop(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::stop(self, id).map_err(Into::into)
    }

    fn restart(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::restart(self, id).map_err(Into::into)
    }

    fn delete_session(&self, request: DeleteSessionRequest) -> ApiResult<DeletionResult> {
        ApplicationController::delete_session(self, request).map_err(Into::into)
    }

    fn fork_session(&self, request: ForkSessionRequest) -> ApiResult<ForkSessionResult> {
        ApplicationController::fork_session(self, request).map_err(Into::into)
    }

    fn events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> ApiResult<Vec<crate::models::SessionEvent>> {
        ApplicationController::events(self, id, since, limit).map_err(Into::into)
    }

    fn record_session_event(
        &self,
        id: &str,
        kind: &str,
        payload: Value,
    ) -> ApiResult<crate::models::SessionEvent> {
        ApplicationController::record_session_event(self, id, kind, payload).map_err(Into::into)
    }

    fn sync_agent_state(&self, id: &str) -> ApiResult<AgentStateSyncResult> {
        ApplicationController::sync_agent_state(self, id).map_err(Into::into)
    }

    fn structured_events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> ApiResult<Vec<StructuredEvent>> {
        ApplicationController::structured_events(self, id, since, limit).map_err(Into::into)
    }

    fn search_sessions(&self, query: &str, limit: usize) -> ApiResult<SessionSearchResponse> {
        ApplicationController::search_sessions(self, query, limit).map_err(Into::into)
    }

    fn list_groups(&self) -> ApiResult<Vec<GroupRecord>> {
        ApplicationController::list_groups(self).map_err(Into::into)
    }

    fn create_group(
        &self,
        name: String,
        parent: Option<String>,
        default_project_path: Option<String>,
    ) -> ApiResult<GroupRecord> {
        ApplicationController::create_group(self, name, parent, default_project_path)
            .map_err(Into::into)
    }

    fn update_group(
        &self,
        name: &str,
        default_project_path: Option<String>,
        clear_default_project_path: bool,
        collapsed: Option<bool>,
    ) -> ApiResult<GroupRecord> {
        ApplicationController::update_group(
            self,
            name,
            default_project_path,
            clear_default_project_path,
            collapsed,
        )
        .map_err(Into::into)
    }

    fn delete_group(&self, name: &str, force: bool) -> ApiResult<()> {
        ApplicationController::delete_group(self, name, force).map_err(Into::into)
    }

    fn move_session_to_group(&self, id: &str, group_name: String) -> ApiResult<SessionRecord> {
        ApplicationController::move_session_to_group(self, id, group_name).map_err(Into::into)
    }

    fn register_project(&self, request: ProjectSpec) -> ApiResult<ProjectRecord> {
        ApplicationController::register_project(self, request).map_err(Into::into)
    }

    fn list_projects(&self) -> ApiResult<Vec<ProjectRecord>> {
        ApplicationController::list_projects(self).map_err(Into::into)
    }

    fn get_project(&self, id: &str) -> ApiResult<ProjectRecord> {
        ApplicationController::get_project(self, id).map_err(Into::into)
    }

    fn set_project_trust(&self, id: &str, trusted: bool) -> ApiResult<ProjectRecord> {
        ApplicationController::set_project_trust(self, id, trusted).map_err(Into::into)
    }

    fn remove_project(&self, id: &str) -> ApiResult<()> {
        ApplicationController::remove_project(self, id).map_err(Into::into)
    }

    fn list_workspaces(&self, project_id: &str) -> ApiResult<Vec<WorkspaceRecord>> {
        ApplicationController::list_workspaces(self, project_id).map_err(Into::into)
    }

    fn get_workspace(&self, id: &str) -> ApiResult<WorkspaceRecord> {
        ApplicationController::get_workspace(self, id).map_err(Into::into)
    }

    fn list_worktrees(&self, project_id: &str) -> ApiResult<Vec<WorktreeRecord>> {
        ApplicationController::list_worktrees(self, project_id).map_err(Into::into)
    }

    fn get_worktree(&self, id: &str) -> ApiResult<WorktreeRecord> {
        ApplicationController::get_worktree(self, id).map_err(Into::into)
    }

    fn create_worktree(
        &self,
        project_id: &str,
        branch: &str,
        carry_state: bool,
    ) -> ApiResult<WorktreeRecord> {
        ApplicationController::create_project_worktree(self, project_id, branch, carry_state)
            .map_err(Into::into)
    }

    fn finish_worktree(&self, id: &str) -> ApiResult<WorktreeRecord> {
        ApplicationController::finish_worktree(self, id).map_err(Into::into)
    }

    fn cleanup_worktrees(&self, project_id: &str) -> ApiResult<CleanupReport> {
        ApplicationController::cleanup_worktrees(self, project_id).map_err(Into::into)
    }

    fn list_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>> {
        ApplicationController::list_mcp_attachments(self).map_err(Into::into)
    }

    fn attach_mcp(&self, session_id: &str, server_id: String) -> ApiResult<McpAttachmentRecord> {
        ApplicationController::attach_mcp(self, session_id, server_id).map_err(Into::into)
    }

    fn attach_project_mcp(
        &self,
        project_id: &str,
        server_id: String,
    ) -> ApiResult<McpAttachmentRecord> {
        ApplicationController::attach_project_mcp(self, project_id, server_id).map_err(Into::into)
    }

    fn attach_profile_mcp(&self, server_id: String) -> ApiResult<McpAttachmentRecord> {
        ApplicationController::attach_profile_mcp(self, server_id).map_err(Into::into)
    }

    fn detach_mcp(&self, id: &str) -> ApiResult<McpAttachmentRecord> {
        ApplicationController::detach_mcp(self, id).map_err(Into::into)
    }

    fn sync_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>> {
        ApplicationController::sync_mcp(self).map_err(Into::into)
    }

    fn list_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>> {
        ApplicationController::list_skill_attachments(self).map_err(Into::into)
    }

    fn attach_skill(&self, session_id: &str, skill_id: String) -> ApiResult<SkillAttachmentRecord> {
        ApplicationController::attach_skill(self, session_id, skill_id).map_err(Into::into)
    }

    fn attach_project_skill(
        &self,
        project_id: &str,
        skill_id: String,
    ) -> ApiResult<SkillAttachmentRecord> {
        ApplicationController::attach_project_skill(self, project_id, skill_id).map_err(Into::into)
    }

    fn attach_profile_skill(&self, skill_id: String) -> ApiResult<SkillAttachmentRecord> {
        ApplicationController::attach_profile_skill(self, skill_id).map_err(Into::into)
    }

    fn detach_skill(&self, id: &str) -> ApiResult<SkillAttachmentRecord> {
        ApplicationController::detach_skill(self, id).map_err(Into::into)
    }

    fn sync_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>> {
        ApplicationController::sync_skills(self).map_err(Into::into)
    }

    fn list_watchers(&self) -> ApiResult<Vec<WatcherRecord>> {
        ApplicationController::list_watchers(self).map_err(Into::into)
    }

    fn list_project_watchers(&self, project_id: &str) -> ApiResult<Vec<WatcherRecord>> {
        ApplicationController::list_project_watchers(self, project_id).map_err(Into::into)
    }

    fn create_watcher(
        &self,
        name: String,
        adapter_id: String,
        project_id: Option<String>,
        config_ref: String,
    ) -> ApiResult<WatcherRecord> {
        ApplicationController::create_watcher_config(self, name, adapter_id, project_id, config_ref)
            .map_err(Into::into)
    }

    fn start_watcher(&self, id: &str) -> ApiResult<WatcherRecord> {
        ApplicationController::start_watcher(self, id).map_err(Into::into)
    }

    fn poll_watcher(&self, id: &str) -> ApiResult<WatcherRecord> {
        ApplicationController::poll_watcher(self, id).map_err(Into::into)
    }

    fn poll_running_watchers(&self) -> ApiResult<Vec<WatcherRecord>> {
        ApplicationController::poll_running_watchers(self).map_err(Into::into)
    }

    fn stop_watcher(&self, id: &str) -> ApiResult<WatcherRecord> {
        ApplicationController::stop_watcher(self, id).map_err(Into::into)
    }

    fn delete_watcher(&self, id: &str) -> ApiResult<()> {
        ApplicationController::delete_watcher(self, id).map_err(Into::into)
    }

    fn ingest_watcher_event(
        &self,
        id: &str,
        source: String,
        event_type: String,
        payload_ref: String,
        signature_status: String,
    ) -> ApiResult<WatcherEventRecord> {
        ApplicationController::ingest_watcher_event(
            self,
            id,
            source,
            event_type,
            payload_ref,
            signature_status,
        )
        .map_err(Into::into)
    }

    fn list_watcher_events(
        &self,
        watcher: &str,
        offset: usize,
        limit: usize,
    ) -> ApiResult<Vec<WatcherEventRecord>> {
        ApplicationController::list_watcher_events(self, watcher, offset, limit).map_err(Into::into)
    }

    fn test_watcher(&self, id: &str) -> ApiResult<WatcherRecord> {
        ApplicationController::test_watcher(self, id).map_err(Into::into)
    }

    fn list_conductors(&self) -> ApiResult<Vec<ConductorRecord>> {
        ApplicationController::list_conductors(self).map_err(Into::into)
    }

    fn create_conductor(&self, session_id: String) -> ApiResult<ConductorRecord> {
        ApplicationController::create_conductor(self, session_id).map_err(Into::into)
    }

    fn get_conductor(&self, id: &str) -> ApiResult<ConductorRecord> {
        ApplicationController::get_conductor(self, id).map_err(Into::into)
    }

    fn start_conductor(&self, id: &str) -> ApiResult<ConductorRecord> {
        ApplicationController::start_conductor(self, id).map_err(Into::into)
    }

    fn heartbeat_conductor(&self, id: &str) -> ApiResult<ConductorRecord> {
        ApplicationController::heartbeat_conductor(self, id).map_err(Into::into)
    }

    fn stop_conductor(&self, id: &str) -> ApiResult<ConductorRecord> {
        ApplicationController::stop_conductor(self, id).map_err(Into::into)
    }

    fn delete_conductor(&self, id: &str) -> ApiResult<()> {
        ApplicationController::delete_conductor(self, id).map_err(Into::into)
    }

    fn list_conductor_assignments(&self, id: &str) -> ApiResult<Vec<ConductorAssignmentRecord>> {
        ApplicationController::list_conductor_assignments(self, id).map_err(Into::into)
    }

    fn complete_conductor_assignment(
        &self,
        id: &str,
        status: &str,
    ) -> ApiResult<ConductorAssignmentRecord> {
        ApplicationController::complete_conductor_assignment(self, id, status).map_err(Into::into)
    }

    fn send_conductor(
        &self,
        conductor_id: &str,
        session_id: String,
        task_ref: String,
    ) -> ApiResult<ConductorAssignmentRecord> {
        ApplicationController::send_conductor(self, conductor_id, session_id, task_ref)
            .map_err(Into::into)
    }

    fn record_cost(
        &self,
        session_id: &str,
        amount_usd: f64,
        payload: serde_json::Value,
    ) -> ApiResult<CostEvent> {
        ApplicationController::record_cost(self, session_id, amount_usd, payload)
            .map_err(Into::into)
    }

    fn cost_summary(&self, mut filter: CostFilter) -> ApiResult<CostSummary> {
        if filter.profile.is_empty() {
            filter.profile = self.config.profile.clone();
        }
        ApplicationController::cost_summary(self, filter).map_err(Into::into)
    }

    fn cost_events(
        &self,
        mut filter: CostFilter,
        offset: usize,
        limit: usize,
    ) -> ApiResult<Vec<CostEvent>> {
        if filter.profile.is_empty() {
            filter.profile = self.config.profile.clone();
        }
        ApplicationController::cost_events(self, filter, offset, limit).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SessionDeckStatus;
    use crate::runtime::FakeRuntime;

    fn controller(read_only: bool) -> ApplicationController<FakeRuntime> {
        controller_with_runtime(FakeRuntime::default(), read_only)
    }

    fn controller_with_runtime(
        runtime: FakeRuntime,
        read_only: bool,
    ) -> ApplicationController<FakeRuntime> {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = AppConfig::built_in("test");
        config.data_dir = tmp.path().to_path_buf();
        let controller = ApplicationController::new(config, runtime, read_only).unwrap();
        controller.init_profile().unwrap();
        controller
    }

    fn controller_with_data_dir(
        data_dir: &Path,
        read_only: bool,
    ) -> ApplicationController<FakeRuntime> {
        let mut config = AppConfig::built_in("test");
        config.data_dir = data_dir.to_path_buf();
        let controller =
            ApplicationController::new(config, FakeRuntime::default(), read_only).unwrap();
        controller.init_profile().unwrap();
        controller
    }

    fn controller_with_codex_hooks() -> ApplicationController<FakeRuntime> {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = AppConfig::built_in("test");
        config.data_dir = tmp.path().to_path_buf();
        config.tools.get_mut("codex").unwrap().worktree =
            crate::config::ToolWorktreeBehavior::Never;
        let controller = ApplicationController::new(config, FakeRuntime::default(), false).unwrap();
        controller.init_profile().unwrap();
        controller
    }

    fn controller_with_codex_hooks_and_sandbox() -> ApplicationController<FakeRuntime> {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = AppConfig::built_in("test");
        config.data_dir = tmp.path().to_path_buf();
        config.tools.get_mut("codex").unwrap().worktree =
            crate::config::ToolWorktreeBehavior::Never;
        config.sandbox_allowed_paths = vec![std::env::current_dir().unwrap()];
        let controller = ApplicationController::new(config, FakeRuntime::default(), false).unwrap();
        controller.init_profile().unwrap();
        controller
    }

    #[test]
    fn raw_events_reject_missing_session() {
        let controller = controller(false);

        let error = controller.events("missing-session", 0, 10).unwrap_err();

        assert!(error.to_string().contains("session not found"));
    }

    #[test]
    fn raw_events_allow_empty_existing_session_window() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "events".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let events = controller.events(&session.id, i64::MAX, 10).unwrap();

        assert!(events.is_empty());
    }

    #[test]
    fn diff_includes_staged_unstaged_and_untracked_changes() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        std::fs::write(repo.path().join("README.md"), "changed\n").unwrap();
        std::fs::write(repo.path().join("staged.txt"), "staged\n").unwrap();
        git(repo.path(), ["add", "staged.txt"]);
        std::fs::write(repo.path().join("untracked.txt"), "untracked\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "diff".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let diff = controller.diff(&session.id).unwrap();

        assert!(diff.contains("## status"));
        assert!(diff.contains(" M README.md"));
        assert!(diff.contains("A  staged.txt"));
        assert!(diff.contains("?? untracked.txt"));
        assert!(diff.contains("## unstaged"));
        assert!(diff.contains("-test"));
        assert!(diff.contains("+changed"));
        assert!(diff.contains("## staged"));
        assert!(diff.contains("staged.txt"));
        assert!(diff.contains("+staged"));
        assert!(diff.contains("## untracked: untracked.txt"));
        assert!(diff.contains("+untracked"));
    }

    fn init_git_repo(path: &Path) {
        git(path, ["init"]);
        git(path, ["config", "user.email", "agent-helm@example.invalid"]);
        git(path, ["config", "user.name", "Agent Helm"]);
        std::fs::write(path.join("README.md"), "test\n").unwrap();
        git(path, ["add", "README.md"]);
        git(path, ["commit", "-m", "init"]);
    }

    fn git<const N: usize>(path: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn empty_command_uses_tool_profile_and_auto_worktree_for_always() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);

        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: String::new(),
                name: "profile-worktree".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let worktree_id = session.worktree_id.clone().unwrap();
        let worktree = controller.store.get_worktree(&worktree_id).unwrap();

        assert_eq!(session.command, "codex");
        assert_ne!(session.project_path, repo.path().to_string_lossy());
        assert_eq!(session.project_path, worktree.path);
        assert!(worktree.branch.starts_with("agent-helm/codex/"));
        let repo_root = repo.path().canonicalize().unwrap();
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("profile-worktree");
        assert_eq!(Path::new(&worktree.path), expected_path.as_path());
    }

    #[test]
    fn configured_custom_tool_profile_can_launch_without_known_adapter() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let mut config = AppConfig::built_in("test");
        config.data_dir = data.path().to_path_buf();
        config.tools.insert(
            "review-bot".into(),
            crate::config::ToolProfile {
                installed: true,
                executable: Some("printf".into()),
                flags: vec!["review".into()],
                worktree: crate::config::ToolWorktreeBehavior::Never,
            },
        );
        let controller = ApplicationController::new(config, FakeRuntime::default(), false).unwrap();
        controller.init_profile().unwrap();

        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "review-bot".into(),
                command: String::new(),
                name: "custom-profile".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        assert_eq!(session.agent, "review-bot");
        assert_eq!(session.command, "printf review");
        assert_eq!(
            Path::new(&session.project_path).canonicalize().unwrap(),
            repo.path().canonicalize().unwrap()
        );
        assert!(session.worktree_id.is_none());
        assert_eq!(
            controller.status(&session.id).unwrap().status,
            SessionStatus::Running
        );
    }

    #[test]
    fn auto_worktrees_with_same_session_name_get_unique_paths() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);

        let first = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: String::new(),
                name: "same session".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let second = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: String::new(),
                name: "same session".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let first_worktree = controller
            .store
            .get_worktree(first.worktree_id.as_deref().unwrap())
            .unwrap();
        let second_worktree = controller
            .store
            .get_worktree(second.worktree_id.as_deref().unwrap())
            .unwrap();

        assert_ne!(first_worktree.path, second_worktree.path);
        assert_eq!(
            Path::new(&first_worktree.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("same-session")
        );
        assert_eq!(
            Path::new(&second_worktree.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("same-session-2")
        );

        controller
            .delete_session(DeleteSessionRequest {
                session_id: first.id,
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap();
        controller
            .delete_session(DeleteSessionRequest {
                session_id: second.id,
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap();
    }

    #[test]
    fn empty_shell_command_uses_profile_without_worktree_by_default() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let expected_command = controller.config.resolve_tool_command("shell", "").unwrap();

        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: String::new(),
                name: "profile-shell".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        assert_eq!(session.command, expected_command);
        assert_eq!(
            Path::new(&session.project_path).canonicalize().unwrap(),
            repo.path().canonicalize().unwrap()
        );
        assert!(session.worktree_id.is_none());
        assert!(
            controller
                .list_worktrees(&session.project_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn explicit_shell_worktree_overrides_never_profile() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);

        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "explicit-shell-worktree".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-explicit-shell".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let worktree = controller
            .store
            .get_worktree(session.worktree_id.as_deref().unwrap())
            .unwrap();

        assert_eq!(worktree.branch, "agent-helm-explicit-shell");
        assert_eq!(session.project_path, worktree.path);
        let repo_root = repo.path().canonicalize().unwrap();
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("explicit-shell-worktree");
        assert_eq!(Path::new(&worktree.path), expected_path.as_path());
    }

    #[test]
    fn fork_with_branch_uses_child_session_name_for_worktree_path() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let parent = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "parent-session".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: Some("Child Session".into()),
                group_name: None,
                worktree_branch: Some("agent-helm/child-branch".into()),
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();
        let worktree = controller
            .store
            .get_worktree(child.worktree_id.as_deref().unwrap())
            .unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("child-session");

        assert_eq!(worktree.branch, "agent-helm/child-branch");
        assert_eq!(Path::new(&worktree.path), expected_path.as_path());
        assert_eq!(child.project_path, worktree.path);

        controller
            .delete_session(DeleteSessionRequest {
                session_id: child.id,
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap();
    }

    #[test]
    fn fork_without_branch_preserves_parent_no_worktree_even_when_profile_auto_worktrees() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let mut controller = controller_with_data_dir(data.path(), false);
        let parent = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "parent-no-worktree".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller.config.tools.get_mut("shell").unwrap().worktree =
            crate::config::ToolWorktreeBehavior::Always;

        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: None,
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();

        assert!(parent.worktree_id.is_none());
        assert!(child.worktree_id.is_none());
        assert_eq!(child.project_path, parent.project_path);
        assert!(
            controller
                .list_worktrees(&child.project_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fresh_custom_fork_preserves_parent_command() {
        let controller = controller(false);
        let parent = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "custom".into(),
                command: "custom-agent --session parent".into(),
                name: "custom-parent".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: Some("custom-child".into()),
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();

        assert_eq!(child.command, "custom-agent --session parent");
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        let events = controller.store.session_events(&child.id, 0, 10).unwrap();
        assert!(events.iter().any(|event| {
            event.kind == "forked"
                && event
                    .payload
                    .get("inherits_conversation")
                    .and_then(|value| value.as_bool())
                    == Some(false)
        }));
    }

    #[test]
    fn lifecycle_with_fake_runtime() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "demo".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: Some("hello".into()),
                parent_session_id: None,
            })
            .unwrap();
        assert_eq!(session.status, SessionStatus::Running);
        assert!(!session.project_id.is_empty());
        assert!(!session.workspace_id.is_empty());
        assert!(session.parent_session_id.is_none());
        assert_eq!(controller.list_sessions().unwrap().len(), 1);

        controller.attach(&session.id).unwrap();
        controller.send(&session.id, "ping").unwrap();
        let output = controller.output(&session.id, 20, false).unwrap();
        assert!(output.text.contains("ping"));

        assert_eq!(
            controller.stop(&session.id).unwrap().status,
            SessionStatus::Stopped
        );
        assert!(
            controller
                .get_session(&session.id)
                .unwrap()
                .runtime_id
                .is_none()
        );
        assert!(
            controller
                .attach(&session.id)
                .unwrap_err()
                .to_string()
                .contains("not running")
        );
        assert_eq!(
            controller.restart(&session.id).unwrap().status,
            SessionStatus::Running
        );
        let statuses = controller
            .events(&session.id, 0, 20)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "status")
            .map(|event| event.payload["source"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec!["runtime_start", "runtime_stop", "runtime_restart"]
        );
        controller.remove(&session.id).unwrap();
        assert!(controller.list_sessions().unwrap().is_empty());
        assert!(
            controller
                .get_session(&session.id)
                .unwrap_err()
                .to_string()
                .contains("session not found")
        );
    }

    #[test]
    fn delete_failure_keeps_session_when_destroy_fails() {
        let controller = controller_with_runtime(FakeRuntime::failing_destroy(), false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "delete-fails".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let err = controller
            .delete_session(DeleteSessionRequest {
                session_id: session.id.clone(),
                mode: DeleteMode::MetadataOnly,
                reason: "destroy failure".into(),
            })
            .unwrap_err();
        let stored = controller.get_session(&session.id).unwrap();

        assert!(err.to_string().contains("fake runtime destroy failed"));
        assert_eq!(stored.status, SessionStatus::Running);
        assert!(stored.runtime_id.is_some());
    }

    #[test]
    fn status_clears_stale_runtime_id_when_runtime_stopped() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "stopped-runtime".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        assert!(session.runtime_id.is_some());
        controller.runtime.stop(&session.id).unwrap();

        let updated = controller.status(&session.id).unwrap();

        assert_eq!(updated.status, SessionStatus::Stopped);
        assert!(updated.runtime_id.is_none());
    }

    fn restart_runtime_without_metadata(controller: &ApplicationController<FakeRuntime>, id: &str) {
        controller
            .runtime
            .start(
                id,
                &LaunchSpec {
                    cwd: ".".to_string(),
                    command: "cat".to_string(),
                    sandbox: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn delete_stops_live_runtime_with_missing_runtime_id() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "delete-runtime-metadata-missing".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller.stop(&session.id).unwrap();
        restart_runtime_without_metadata(&controller, &session.id);
        assert_eq!(
            controller.status(&session.id).unwrap().status,
            SessionStatus::Running
        );
        assert!(
            controller
                .get_session(&session.id)
                .unwrap()
                .runtime_id
                .is_none()
        );

        let result = controller
            .delete_session(DeleteSessionRequest {
                session_id: session.id.clone(),
                mode: DeleteMode::MetadataOnly,
                reason: "missing runtime metadata".into(),
            })
            .unwrap();

        assert!(result.runtime_stopped);
        assert_eq!(
            controller.runtime.status(&session.id).unwrap(),
            SessionStatus::Stopped
        );
    }

    #[test]
    fn output_requires_recorded_session() {
        let controller = controller(false);
        controller
            .runtime
            .start(
                "orphan",
                &LaunchSpec {
                    cwd: ".".to_string(),
                    command: "cat".to_string(),
                    sandbox: None,
                },
            )
            .unwrap();

        let err = controller.output("orphan", 10, false).unwrap_err();

        assert_eq!(err.to_string(), "session not found: orphan");
    }

    #[test]
    fn attach_and_send_reject_stale_runtime_without_recording_input() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "stale".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller.runtime.destroy(&session.id).unwrap();

        assert!(
            controller
                .attach(&session.id)
                .unwrap_err()
                .to_string()
                .contains("not running")
        );
        assert!(
            controller
                .send(&session.id, "lost")
                .unwrap_err()
                .to_string()
                .contains("not running")
        );
        assert!(
            controller
                .output(&session.id, 20, false)
                .unwrap_err()
                .to_string()
                .contains("not running")
        );
        assert_eq!(
            controller.get_session(&session.id).unwrap().status,
            SessionStatus::Stopped
        );
        let events = controller.events(&session.id, 0, 20).unwrap();

        assert!(events.iter().any(|event| {
            event.kind == "status" && event.payload["source"].as_str() == Some("runtime_status")
        }));
        assert!(!events.iter().any(|event| {
            event.kind == "input" && event.payload["preview"].as_str() == Some("lost")
        }));
    }

    #[test]
    fn list_sessions_reconciles_active_runtime_status() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "listed".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller.runtime.destroy(&session.id).unwrap();

        let sessions = controller.list_sessions().unwrap();

        assert_eq!(sessions[0].status, SessionStatus::Stopped);
        assert!(sessions[0].runtime_id.is_none());
        assert!(
            controller
                .events(&session.id, 0, 20)
                .unwrap()
                .iter()
                .any(|event| {
                    event.kind == "status"
                        && event.payload["source"].as_str() == Some("runtime_status")
                })
        );
    }

    #[test]
    fn conductor_send_delivers_task_to_session() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "worker".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let conductor = controller.create_conductor(session.id.clone()).unwrap();
        controller.start_conductor(&conductor.id).unwrap();

        let assignment = controller
            .send_conductor(&conductor.id, session.id.clone(), "delegated task".into())
            .unwrap();

        assert_eq!(assignment.status, "assigned");
        assert!(
            controller
                .output(&session.id, 20, false)
                .unwrap()
                .text
                .contains("delegated task")
        );
    }

    #[test]
    fn fork_records_parent_and_workspace() {
        let controller = controller(false);
        let parent = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "parent".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: None,
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(fork.workspace_id, child.workspace_id);
        assert!(!fork.workspace_id.is_empty());
    }

    #[test]
    fn fork_reuses_parent_worktree_without_new_branch() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let parent = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "parent".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let parent_worktree_id = parent.worktree_id.clone().unwrap();

        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: None,
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();
        let child_workspace = controller.store.get_workspace(&child.workspace_id).unwrap();

        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(
            fork.worktree_id.as_deref(),
            Some(parent_worktree_id.as_str())
        );
        assert_eq!(
            child.worktree_id.as_deref(),
            Some(parent_worktree_id.as_str())
        );
        assert_eq!(child.project_id, parent.project_id);
        assert_eq!(child.project_path, parent.project_path);
        assert_eq!(child_workspace.path, parent.project_path);
        assert_eq!(
            child_workspace.worktree_id.as_deref(),
            Some(parent_worktree_id.as_str())
        );
    }

    #[test]
    fn fork_can_create_stopped_child() {
        let controller = controller(false);
        let parent = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "parent".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: Some("paused fork".to_string()),
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: false,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();
        assert!(!fork.started);
        assert_eq!(child.status, SessionStatus::Stopped);
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
    }

    #[test]
    fn restart_removed_session_reports_not_found() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "removable".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        controller.remove(&session.id).unwrap();

        let err = controller.restart(&session.id).unwrap_err();
        assert!(err.to_string().contains("session not found"));
    }

    #[test]
    fn search_sessions_finds_event_content() {
        let controller = controller(false);
        let needle = format!("agenthelm-search-needle-{}", Uuid::new_v4().simple());
        let matching = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "metrics".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: Some(format!("implement {needle} metrics")),
                parent_session_id: None,
            })
            .unwrap();
        controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "login".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: Some("refactor login widget".into()),
                parent_session_id: None,
            })
            .unwrap();

        let results = controller.search_sessions(&needle, 20).unwrap();

        assert_eq!(results.query, needle);
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].session_id, matching.id);
        assert!(results.results[0].snippet.contains(&needle));
    }

    #[test]
    fn search_sessions_finds_live_output_content() {
        let needle = format!("runtime-only-agenthelm-needle-{}", Uuid::new_v4().simple());
        let runtime = FakeRuntime::default();
        let controller = controller_with_runtime(runtime.clone(), false);
        let matching = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "output".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        runtime.send(&matching.id, &needle).unwrap();

        let results = controller.search_sessions(&needle, 20).unwrap();

        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].session_id, matching.id);
        assert_eq!(results.results[0].source, "output");
        assert!(results.results[0].snippet.contains(&needle));
    }

    #[test]
    fn search_claude_transcripts_finds_jsonl_content() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-Users-test-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("a1b2c3d4-e5f6-7890-abcd-ef1234567890.jsonl"),
            r#"{"sessionId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","type":"user","message":{"role":"user","content":"implement transcript-agenthelm-needle metrics"},"cwd":"/Users/test/project"}
{"sessionId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","type":"assistant","message":{"role":"assistant","content":"done"}}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("b2c3d4e5-f6a7-8901-bcde-f23456789012.jsonl"),
            r#"{"sessionId":"b2c3d4e5-f6a7-8901-bcde-f23456789012","message":{"content":"unrelated login work"},"cwd":"/Users/test/project"}"#,
        )
        .unwrap();

        let results = search_claude_transcripts_in(
            &home.path().join(".claude/projects"),
            "transcript-agenthelm-needle",
            "transcript-agenthelm-needle",
            20,
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].session_id,
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        );
        assert_eq!(results[0].cwd, "/Users/test/project");
        assert_eq!(results[0].source, "claude_transcript");
        assert!(results[0].snippet.contains("transcript-agenthelm-needle"));
    }

    #[test]
    fn search_codex_transcripts_finds_jsonl_content() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions/2026/06/26");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
        root.join("rollout-session.jsonl"),
        r#"{"type":"session_meta","payload":{"agent_helm_session_id":"codex-session","cwd":"/Users/test/project"}}
{"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"cargo test transcript-codex-agenthelm-needle\"}"}}"#,
    )
    .unwrap();
        std::fs::write(
        root.join("rollout-other.jsonl"),
        r#"{"type":"session_meta","payload":{"agent_helm_session_id":"other-session","cwd":"/Users/test/other"}}
{"type":"response_item","payload":{"type":"function_call","call_id":"other","name":"shell","arguments":"{\"cmd\":\"cargo test unrelated\"}"}}"#,
    )
    .unwrap();

        let results = search_codex_transcripts_in(
            &home.path().join(".codex/sessions"),
            "transcript-codex-agenthelm-needle",
            20,
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "codex-session");
        assert_eq!(results[0].cwd, "/Users/test/project");
        assert_eq!(results[0].source, "codex_transcript");
        assert_eq!(results[0].agent, "codex");
        assert!(
            results[0]
                .snippet
                .contains("transcript-codex-agenthelm-needle")
        );
    }

    #[test]
    fn latest_claude_agent_state_reads_tool_use_transcript() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-Users-test-project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("agent-helm-session.jsonl"),
            r#"{"agent_helm_session_id":"agent-helm-session","type":"user","message":{"role":"user","content":"build it"},"cwd":"/Users/test/project"}
{"agent_helm_session_id":"agent-helm-session","type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]},"cwd":"/Users/test/project"}"#,
        )
        .unwrap();
        let session = SessionRecord {
            id: "agent-helm-session".into(),
            name: "claude".into(),
            profile: "test".into(),
            group_name: "default".into(),
            project_id: "project".into(),
            workspace_id: "workspace".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: "claude".into(),
            command: "claude".into(),
            project_path: "/Users/test/project".into(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: 0,
            updated_at: 0,
        };

        let payload = latest_claude_agent_state_in(&home.path().join(".claude/projects"), &session)
            .unwrap()
            .unwrap();

        assert_eq!(payload["state"], "occupied");
        assert_eq!(payload["source"], "claude_transcript");
        assert_eq!(payload["tool"]["name"], "Bash");
        assert_eq!(payload["transcript_line"], 2);
        assert_eq!(payload["transcript_session_id"], "agent-helm-session");
    }

    #[test]
    fn transcript_sync_dedupe_ignores_noisy_raw_event_window() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "claude".into(),
                command: "cat".into(),
                name: "sync-dedupe".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .store
            .append_session_event(
                &session.id,
                "agent_state",
                json!({
                    "state": "working",
                    "source": "claude_transcript",
                    "transcript_path": "/tmp/claude.jsonl",
                    "transcript_line": 42
                }),
            )
            .unwrap();
        for index in 0..21 {
            controller
                .store
                .append_session_event(
                    &session.id,
                    "status",
                    json!({"status": "running", "source": "noise", "index": index}),
                )
                .unwrap();
        }

        assert!(
            controller
                .latest_agent_state_matches(
                    &session.id,
                    "claude_transcript",
                    Some("/tmp/claude.jsonl"),
                    Some(42),
                )
                .unwrap()
        );
    }

    #[test]
    fn search_sessions_requires_query() {
        let err = controller(false)
            .search_sessions(" ", 20)
            .unwrap_err()
            .to_string();

        assert!(err.contains("search query"));
    }

    #[test]
    fn latest_codex_agent_state_reads_function_call_transcript() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions/2026/06/26");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
        root.join("rollout-unrelated.jsonl"),
        r#"{"type":"session_meta","payload":{"agent_helm_session_id":"other-session","cwd":"/Users/test/other"}}
{"type":"response_item","payload":{"type":"function_call","call_id":"other-call","name":"shell"}}"#,
    )
    .unwrap();
        std::fs::write(
        root.join("rollout-session.jsonl"),
        r#"{"type":"session_meta","payload":{"agent_helm_session_id":"agent-helm-session","cwd":"/Users/test/project"}}
{"type":"response_item","payload":{"type":"reasoning","summary":[]}}
{"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
    )
    .unwrap();
        let session = SessionRecord {
            id: "agent-helm-session".into(),
            name: "codex".into(),
            profile: "test".into(),
            group_name: "default".into(),
            project_id: "project".into(),
            workspace_id: "workspace".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: "codex".into(),
            command: "codex".into(),
            project_path: "/Users/test/project".into(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: 0,
            updated_at: 0,
        };

        let payload = latest_codex_agent_state_in(&home.path().join(".codex/sessions"), &session)
            .unwrap()
            .unwrap();

        assert_eq!(payload["state"], "occupied");
        assert_eq!(payload["source"], "codex_transcript");
        assert_eq!(payload["transcript_event"], "function_call");
        assert_eq!(payload["tool"]["name"], "shell");
        assert_eq!(payload["call_id"], "call-1");
        assert_eq!(payload["transcript_line"], 3);
        assert_eq!(payload["transcript_session_id"], "agent-helm-session");
        assert_eq!(payload["cwd"], "/Users/test/project");
    }

    #[test]
    fn transcript_state_from_events_uses_known_path_without_scanning() {
        let home = tempfile::tempdir().unwrap();
        let transcript = home.path().join("rollout-session.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"session_meta","payload":{"agent_helm_session_id":"agent-helm-session","cwd":"/Users/test/project"}}
{"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
        )
        .unwrap();
        let session = SessionRecord {
            id: "agent-helm-session".into(),
            name: "codex".into(),
            profile: "test".into(),
            group_name: "default".into(),
            project_id: "project".into(),
            workspace_id: "workspace".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: "codex".into(),
            command: "codex".into(),
            project_path: "/Users/test/project".into(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: 0,
            updated_at: 0,
        };

        assert!(
            latest_transcript_agent_state_from_events(&session, &[])
                .unwrap()
                .is_none()
        );

        let events = vec![crate::models::SessionEvent {
            id: 1,
            session_id: session.id.clone(),
            kind: "agent_state".into(),
            payload: json!({
                "state": "thinking",
                "source": "codex_transcript",
                "transcript_path": transcript.to_string_lossy(),
                "transcript_line": 1
            }),
            created_at: 0,
        }];
        let (_, payload) = latest_transcript_agent_state_from_events(&session, &events)
            .unwrap()
            .expect("known transcript state");

        assert_eq!(payload["state"], "occupied");
        assert_eq!(payload["tool"]["name"], "shell");
        assert_eq!(
            payload["transcript_path"].as_str(),
            Some(transcript.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn latest_recent_codex_agent_state_discovers_recent_cwd_transcript() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".codex/sessions/2026/06/26");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
        root.join("rollout-session.jsonl"),
        r#"{"type":"session_meta","payload":{"cwd":"/Users/test/project"}}
{"type":"response_item","payload":{"type":"reasoning","summary":[]}}
{"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
    )
    .unwrap();

        let session = SessionRecord {
            id: "agent-helm-session".into(),
            name: "codex".into(),
            profile: "test".into(),
            group_name: "default".into(),
            project_id: "project".into(),
            workspace_id: "workspace".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: "codex".into(),
            command: "codex".into(),
            project_path: "/Users/test/project".into(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: now_ts(),
            updated_at: 0,
        };

        let payload =
            latest_recent_codex_agent_state_in(&home.path().join(".codex/sessions"), &session, 16)
                .unwrap()
                .unwrap();
        assert_eq!(payload["state"], "occupied");
        assert_eq!(payload["source"], "codex_transcript");
        assert_eq!(payload["cwd"], "/Users/test/project");
    }

    #[test]
    fn latest_codex_agent_state_rejects_older_cwd_only_transcript() {
        let home = tempfile::tempdir().unwrap();
        let transcript = home.path().join("rollout-session.jsonl");
        std::fs::write(
        &transcript,
        r#"{"type":"session_meta","payload":{"cwd":"/Users/test/project"}}
{"type":"response_item","payload":{"type":"reasoning","summary":[]}}
{"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"shell","arguments":"{\"cmd\":\"cargo test\"}"}}"#,
    )
    .unwrap();

        let session = SessionRecord {
            id: "agent-helm-session".into(),
            name: "codex".into(),
            profile: "test".into(),
            group_name: "default".into(),
            project_id: "project".into(),
            workspace_id: "workspace".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: "codex".into(),
            command: "codex".into(),
            project_path: "/Users/test/project".into(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: now_ts().saturating_add(TRANSCRIPT_CWD_MATCH_START_TOLERANCE_SECS + 120),
            updated_at: 0,
        };

        assert!(
            latest_codex_agent_state_file(&transcript, &session, true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn groups_can_be_created_updated_and_deleted() {
        let controller = controller(false);
        let group = controller
            .create_group("api".into(), Some("work".into()), Some("/tmp/api".into()))
            .unwrap();
        assert_eq!(group.name, "work/api");
        assert_eq!(group.default_project_path, "/tmp/api");
        let metadata: serde_json::Value = serde_json::from_str(&group.metadata).unwrap();
        assert_eq!(metadata["parent"], "work");
        assert_eq!(metadata["depth"], 1);

        let group = controller
            .update_group("work/api", Some("/tmp/next".into()), false, Some(true))
            .unwrap();
        assert_eq!(group.default_project_path, "/tmp/next");
        assert!(group.collapsed);

        let group = controller
            .update_group_settings(
                "work/api",
                GroupSettingsUpdate {
                    default_agent: Some(Some("codex".into())),
                    default_worktree: Some(Some(true)),
                    default_carry_state: Some(Some(true)),
                    ..GroupSettingsUpdate::default()
                },
            )
            .unwrap();
        assert_eq!(group.default_agent.as_deref(), Some("codex"));
        assert_eq!(group.default_worktree, Some(true));
        assert_eq!(group.default_carry_state, Some(true));

        controller.delete_group("work/api", false).unwrap();
        assert!(
            !controller
                .list_groups()
                .unwrap()
                .iter()
                .any(|group| group.name == "work/api")
        );
    }

    #[test]
    fn force_delete_group_moves_sessions_to_parent_or_default() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "grouped".into(),
                group_name: "ops".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        assert!(controller.delete_group("ops", false).is_err());
        controller.delete_group("ops", true).unwrap();

        assert_eq!(
            controller.get_session(&session.id).unwrap().group_name,
            controller.config.default_group
        );
    }

    #[test]
    fn workspace_read_surfaces_return_created_session_workspace() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "workspace".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let workspace = controller.get_workspace(&session.workspace_id).unwrap();
        assert_eq!(workspace.id, session.workspace_id);
        assert_eq!(workspace.project_id, session.project_id);
        assert_eq!(workspace.path, session.project_path);
        assert!(
            controller
                .list_workspaces(&session.project_id)
                .unwrap()
                .iter()
                .any(|record| record.id == session.workspace_id)
        );
    }

    #[test]
    fn delete_cleanup_worktree_finishes_recorded_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "worktree".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let worktree_id = session.worktree_id.clone().unwrap();
        let worktree = controller.store.get_worktree(&worktree_id).unwrap();
        assert!(Path::new(&worktree.path).exists());

        let result = controller
            .delete_session(DeleteSessionRequest {
                session_id: session.id,
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap();

        assert!(result.worktree_cleaned);
        assert!(!Path::new(&worktree.path).exists());
        assert_eq!(
            controller.store.get_worktree(&worktree_id).unwrap().status,
            WorktreeStatus::Finished
        );
        assert!(controller.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn delete_cleanup_worktree_clears_runtime_when_teardown_fails() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        std::fs::write(
            repo.path().join(".agent-helm.toml"),
            "[hooks]\nteardown = \"echo teardown failed >&2; exit 7\"\n",
        )
        .unwrap();

        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: true,
            })
            .unwrap();

        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "worktree-teardown".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        assert_eq!(session.status, SessionStatus::Running);
        assert!(session.runtime_id.is_some());
        let worktree_id = session.worktree_id.clone().unwrap();
        let worktree = controller.store.get_worktree(&worktree_id).unwrap();

        let err = controller
            .delete_session(DeleteSessionRequest {
                session_id: session.id.clone(),
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap_err();

        assert!(err.to_string().contains("project teardown hook failed"));
        let session = controller.get_session(&session.id).unwrap();
        assert_eq!(session.status, SessionStatus::Stopped);
        assert!(session.runtime_id.is_none());
        assert_eq!(session.worktree_id.as_deref(), Some(worktree_id.as_str()));
        assert!(Path::new(&worktree.path).exists());
        assert_eq!(
            controller.store.get_worktree(&worktree_id).unwrap().status,
            WorktreeStatus::Ready
        );
    }

    #[test]
    fn sandbox_validation_failure_discards_created_session_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let repo_root = repo.path().canonicalize().unwrap();
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("sandbox-denied");

        let data = tempfile::tempdir().unwrap();
        let mut config = AppConfig::built_in("test");
        config.data_dir = data.path().to_path_buf();
        config.sandbox_allowed_paths = vec![repo_root.clone()];
        let controller = ApplicationController::new(config, FakeRuntime::default(), false).unwrap();
        controller.init_profile().unwrap();

        let err = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "sandbox-denied".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: true,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("sandbox path is outside allowed paths"));
        assert!(!expected_path.exists());
        assert!(controller.list_sessions().unwrap().is_empty());
        let project = controller.list_projects().unwrap().pop().unwrap();
        assert!(controller.list_worktrees(&project.id).unwrap().is_empty());
    }

    #[test]
    fn finish_worktree_rejects_attached_session_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "worktree".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let worktree_id = session.worktree_id.clone().unwrap();
        let worktree = controller.store.get_worktree(&worktree_id).unwrap();

        let err = controller.finish_worktree(&worktree_id).unwrap_err();

        assert!(err.to_string().contains("attached to a session"));
        assert!(Path::new(&worktree.path).exists());
        assert_eq!(
            controller.store.get_worktree(&worktree_id).unwrap().status,
            WorktreeStatus::Ready
        );
    }

    #[test]
    fn delete_cleanup_worktree_keeps_shared_fork_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let parent = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "parent-worktree".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let worktree_id = parent.worktree_id.clone().unwrap();
        let worktree = controller.store.get_worktree(&worktree_id).unwrap();
        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: None,
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();

        let result = controller
            .delete_session(DeleteSessionRequest {
                session_id: child.id,
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap();

        assert!(!result.worktree_cleaned);
        assert!(Path::new(&worktree.path).exists());
        assert_eq!(
            controller.store.get_worktree(&worktree_id).unwrap().status,
            WorktreeStatus::Ready
        );
        assert_eq!(
            controller
                .get_session(&parent.id)
                .unwrap()
                .worktree_id
                .as_deref(),
            Some(worktree_id.as_str())
        );
    }

    #[test]
    fn delete_cleanup_worktree_cleans_removed_parent_fork_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let parent = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "removed-parent-worktree".into(),
                group_name: "default".into(),
                worktree: Some("agent-helm-child".into()),
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let worktree_id = parent.worktree_id.clone().unwrap();
        let worktree = controller.store.get_worktree(&worktree_id).unwrap();
        let fork = controller
            .fork_session(ForkSessionRequest {
                parent_session_id: parent.id.clone(),
                name: None,
                group_name: None,
                worktree_branch: None,
                carry_state: false,
                start_immediately: true,
            })
            .unwrap();
        controller.remove(&parent.id).unwrap();
        let child = controller.get_session(&fork.child_session_id).unwrap();

        let result = controller
            .delete_session(DeleteSessionRequest {
                session_id: child.id,
                mode: DeleteMode::CleanupWorktree,
                reason: "test".into(),
            })
            .unwrap();

        assert!(result.worktree_cleaned);
        assert!(!Path::new(&worktree.path).exists());
        assert_eq!(
            controller.store.get_worktree(&worktree_id).unwrap().status,
            WorktreeStatus::Finished
        );
    }

    #[test]
    fn session_worktree_setup_failure_rolls_back_physical_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        std::fs::write(
            repo.path().join(".agent-helm.toml"),
            "[hooks]\nsetup = \"printf dirty > dirty.txt; exit 7\"\n",
        )
        .unwrap();
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let project = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: true,
            })
            .unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("setup-fails");

        let error = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: String::new(),
                name: "setup fails".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap_err();

        assert!(error.to_string().contains("project setup hook failed"));
        assert!(!expected_path.exists());
        assert!(
            controller
                .store
                .list_worktrees(&project.id)
                .unwrap()
                .is_empty()
        );
        assert!(
            !controller
                .workspace
                .list_worktrees(repo.path())
                .unwrap()
                .iter()
                .any(|worktree| worktree.path == expected_path)
        );
    }

    #[test]
    fn project_worktree_setup_failure_rolls_back_physical_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        std::fs::write(
            repo.path().join(".agent-helm.toml"),
            "[hooks]\nsetup = \"printf dirty > dirty.txt; exit 7\"\n",
        )
        .unwrap();
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let project = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: true,
            })
            .unwrap();
        let repo_root = repo.path().canonicalize().unwrap();
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("agent-helm-setup-fails");

        let error = controller
            .create_project_worktree(&project.id, "agent-helm-setup-fails", false)
            .unwrap_err();

        assert!(error.to_string().contains("project setup hook failed"));
        assert!(!expected_path.exists());
        assert!(
            controller
                .store
                .list_worktrees(&project.id)
                .unwrap()
                .is_empty()
        );
        assert!(
            !controller
                .workspace
                .list_worktrees(repo.path())
                .unwrap()
                .iter()
                .any(|worktree| worktree.path == expected_path)
        );
    }

    #[test]
    fn cleanup_worktrees_runs_trusted_teardown_hooks() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let marker = repo.path().join("teardown-marker");
        std::fs::write(
            repo.path().join(".agent-helm.toml"),
            format!(
                "[hooks]\nteardown = \"printf teardown > '{}'\"\n",
                marker.display()
            ),
        )
        .unwrap();

        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let project = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: true,
            })
            .unwrap();
        let mut worktree = controller
            .create_project_worktree(&project.id, "agent-helm-cleanup", false)
            .unwrap();
        worktree.status = WorktreeStatus::Finished;
        worktree.updated_at = now_ts();
        let worktree = controller
            .store
            .update_worktree(&worktree, worktree.version)
            .unwrap();

        let report = controller.cleanup_worktrees(&project.id).unwrap();

        assert_eq!(report.removed, 1);
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "teardown");
        assert!(!Path::new(&worktree.path).exists());
    }

    #[test]
    fn project_worktree_create_can_carry_state() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        std::fs::write(repo.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(repo.path().join("local.log"), "carry me\n").unwrap();

        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let project = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: true,
            })
            .unwrap();

        let worktree = controller
            .create_project_worktree(&project.id, "agent-helm-carry", true)
            .unwrap();

        let shown = controller.get_worktree(&worktree.id).unwrap();
        assert_eq!(shown.id, worktree.id);
        assert_eq!(shown.project_id, project.id);
        assert_eq!(
            std::fs::read_to_string(Path::new(&worktree.path).join("local.log")).unwrap(),
            "carry me\n"
        );
    }

    #[test]
    fn worktree_project_scope_requires_existing_project() {
        let controller = controller(false);

        assert!(
            controller
                .list_worktrees("missing-project")
                .unwrap_err()
                .to_string()
                .contains("project not found")
        );
        assert!(
            controller
                .cleanup_worktrees("missing-project")
                .unwrap_err()
                .to_string()
                .contains("project not found")
        );
    }

    #[test]
    fn remove_project_rejects_project_with_sessions() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "project owner".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let error = controller.remove_project(&session.project_id).unwrap_err();

        assert!(error.to_string().contains("project still has 1 session"));
        assert!(controller.get_project(&session.project_id).is_ok());
    }

    #[test]
    fn remove_project_deletes_unreferenced_project() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let project = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: false,
            })
            .unwrap();

        controller.remove_project(&project.id).unwrap();

        assert!(
            controller
                .get_project(&project.id)
                .unwrap_err()
                .to_string()
                .contains("project not found")
        );
    }

    #[test]
    fn remove_project_rejects_project_scoped_tools_and_watchers() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let project = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: repo.path().to_string_lossy().to_string(),
                default_branch: String::new(),
                trusted: false,
            })
            .unwrap();

        controller
            .attach_project_mcp(&project.id, "project-mcp".into())
            .unwrap();
        let error = controller.remove_project(&project.id).unwrap_err();
        assert!(error.to_string().contains("project still has 1 MCP"));
        controller
            .detach_mcp(
                &controller
                    .list_mcp_attachments()
                    .unwrap()
                    .into_iter()
                    .find(|attachment| {
                        attachment.project_id.as_deref() == Some(project.id.as_str())
                    })
                    .unwrap()
                    .id,
            )
            .unwrap();

        controller
            .attach_project_skill(&project.id, "project-skill".into())
            .unwrap();
        let error = controller.remove_project(&project.id).unwrap_err();
        assert!(error.to_string().contains("project still has 1 skill"));
        controller
            .detach_skill(
                &controller
                    .list_skill_attachments()
                    .unwrap()
                    .into_iter()
                    .find(|attachment| {
                        attachment.project_id.as_deref() == Some(project.id.as_str())
                    })
                    .unwrap()
                    .id,
            )
            .unwrap();

        controller
            .create_watcher_config(
                "project-watch".into(),
                "manual".into(),
                Some(project.id.clone()),
                "{}".into(),
            )
            .unwrap();
        let error = controller.remove_project(&project.id).unwrap_err();
        assert!(error.to_string().contains("project still has 1 watcher"));
        assert!(controller.get_project(&project.id).is_ok());
    }

    #[test]
    fn register_existing_project_can_apply_trust() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let root_path = repo.path().to_string_lossy().to_string();

        let untrusted = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: root_path.clone(),
                default_branch: String::new(),
                trusted: false,
            })
            .unwrap();
        let trusted = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path,
                default_branch: String::new(),
                trusted: true,
            })
            .unwrap();

        assert_eq!(trusted.id, untrusted.id);
        assert_eq!(trusted.trust_state, ProjectTrustState::Trusted);
    }

    #[test]
    fn register_existing_project_can_update_default_branch() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let root_path = repo.path().to_string_lossy().to_string();
        let initial = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path: root_path.clone(),
                default_branch: "main".into(),
                trusted: false,
            })
            .unwrap();

        let updated = controller
            .register_project(ProjectSpec {
                profile: "test".into(),
                root_path,
                default_branch: "trunk".into(),
                trusted: false,
            })
            .unwrap();

        assert_eq!(updated.id, initial.id);
        assert_eq!(updated.default_branch, "trunk");
    }

    #[test]
    fn mcp_and_skill_sync_require_trusted_project() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: "cat".into(),
                name: "trust".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        controller.attach_mcp(&session.id, "mcp".into()).unwrap();
        controller
            .attach_skill(&session.id, "skill".into())
            .unwrap();
        assert!(
            controller
                .sync_mcp()
                .unwrap_err()
                .to_string()
                .contains("trusted")
        );
        assert!(
            controller
                .sync_skills()
                .unwrap_err()
                .to_string()
                .contains("trusted")
        );

        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        controller
            .attach_project_mcp(&session.project_id, "project-mcp".into())
            .unwrap();
        controller.attach_profile_mcp("profile-mcp".into()).unwrap();
        controller
            .attach_project_skill(&session.project_id, "project-skill".into())
            .unwrap();
        controller
            .attach_profile_skill("profile-skill".into())
            .unwrap();

        let mcp_scopes = controller
            .sync_mcp()
            .unwrap()
            .into_iter()
            .map(|attachment| attachment.scope)
            .collect::<Vec<_>>();
        let skill_scopes = controller
            .sync_skills()
            .unwrap()
            .into_iter()
            .map(|attachment| attachment.scope)
            .collect::<Vec<_>>();

        assert!(mcp_scopes.contains(&"session".to_string()));
        assert!(mcp_scopes.contains(&"project".to_string()));
        assert!(mcp_scopes.contains(&"profile".to_string()));
        assert!(skill_scopes.contains(&"session".to_string()));
        assert!(skill_scopes.contains(&"project".to_string()));
        assert!(skill_scopes.contains(&"profile".to_string()));
    }

    #[test]
    fn mcp_sync_materialization_is_session_scoped() {
        let controller = controller(false);
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();
        init_git_repo(project_a.path());
        init_git_repo(project_b.path());
        let session_a = controller
            .create_session(CreateSession {
                path: project_a.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: "cat".into(),
                name: "session-a".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let session_b = controller
            .create_session(CreateSession {
                path: project_b.path().to_string_lossy().to_string(),
                agent: "codex".into(),
                command: "cat".into(),
                name: "session-b".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session_a.project_id, true)
            .unwrap();
        controller
            .set_project_trust(&session_b.project_id, true)
            .unwrap();

        controller.attach_profile_mcp("profile-mcp".into()).unwrap();
        controller
            .attach_project_mcp(&session_a.project_id, "project-a-mcp".into())
            .unwrap();
        controller
            .attach_project_mcp(&session_b.project_id, "project-b-mcp".into())
            .unwrap();
        let session_a_mcp = controller
            .attach_mcp(&session_a.id, "session-a-mcp".into())
            .unwrap();
        let session_b_mcp = controller
            .attach_mcp(&session_b.id, "session-b-mcp".into())
            .unwrap();
        controller
            .attach_profile_skill("profile-skill".into())
            .unwrap();
        controller
            .attach_project_skill(&session_a.project_id, "project-a-skill".into())
            .unwrap();
        controller
            .attach_project_skill(&session_b.project_id, "project-b-skill".into())
            .unwrap();
        controller
            .attach_skill(&session_a.id, "session-a-skill".into())
            .unwrap();
        controller
            .attach_skill(&session_b.id, "session-b-skill".into())
            .unwrap();

        let plan_a = controller.session_materialization(&session_a.id).unwrap();
        let plan_a_mcp = plan_a
            .mcp
            .iter()
            .map(|entry| entry.server_id.as_str())
            .collect::<Vec<_>>();
        let plan_a_skills = plan_a
            .skills
            .iter()
            .map(|entry| entry.skill_id.as_str())
            .collect::<Vec<_>>();
        assert!(plan_a_mcp.contains(&"profile-mcp"));
        assert!(plan_a_mcp.contains(&"project-a-mcp"));
        assert!(plan_a_mcp.contains(&"session-a-mcp"));
        assert!(!plan_a_mcp.contains(&"project-b-mcp"));
        assert!(!plan_a_mcp.contains(&"session-b-mcp"));
        assert!(plan_a_skills.contains(&"profile-skill"));
        assert!(plan_a_skills.contains(&"project-a-skill"));
        assert!(plan_a_skills.contains(&"session-a-skill"));
        assert!(!plan_a_skills.contains(&"project-b-skill"));
        assert!(!plan_a_skills.contains(&"session-b-skill"));

        let synced = controller.sync_mcp().unwrap();
        let session_a_state = synced
            .iter()
            .find(|attachment| attachment.id == session_a_mcp.id)
            .unwrap()
            .materialized_state
            .clone();
        let session_b_state = synced
            .iter()
            .find(|attachment| attachment.id == session_b_mcp.id)
            .unwrap()
            .materialized_state
            .clone();

        assert!(session_a_state.contains("profile-mcp"));
        assert!(session_a_state.contains("project-a-mcp"));
        assert!(session_a_state.contains("session-a-mcp"));
        assert!(session_a_state.contains("profile-skill"));
        assert!(session_a_state.contains("project-a-skill"));
        assert!(session_a_state.contains("session-a-skill"));
        assert!(!session_a_state.contains("project-b-mcp"));
        assert!(!session_a_state.contains("session-b-mcp"));
        assert!(!session_a_state.contains("project-b-skill"));
        assert!(!session_a_state.contains("session-b-skill"));

        assert!(session_b_state.contains("profile-mcp"));
        assert!(session_b_state.contains("project-b-mcp"));
        assert!(session_b_state.contains("session-b-mcp"));
        assert!(!session_b_state.contains("project-a-mcp"));
        assert!(!session_b_state.contains("session-a-mcp"));
    }

    #[test]
    fn shell_sessions_reject_tool_attachments() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "plain-shell".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        assert!(
            controller
                .attach_mcp(&session.id, "mcp".into())
                .unwrap_err()
                .to_string()
                .contains("agent does not support MCP: shell")
        );
        assert!(
            controller
                .attach_skill(&session.id, "skill".into())
                .unwrap_err()
                .to_string()
                .contains("agent does not support skills: shell")
        );
    }

    #[test]
    fn shell_session_materialization_omits_inherited_tool_attachments() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "plain-shell-materialization".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        controller.attach_profile_mcp("profile-mcp".into()).unwrap();
        controller
            .attach_project_mcp(&session.project_id, "project-mcp".into())
            .unwrap();
        controller
            .attach_profile_skill("profile-skill".into())
            .unwrap();
        controller
            .attach_project_skill(&session.project_id, "project-skill".into())
            .unwrap();

        let plan = controller.session_materialization(&session.id).unwrap();
        assert!(plan.mcp.is_empty());
        assert!(plan.skills.is_empty());
    }

    #[test]
    fn project_watchers_require_trusted_project_to_start() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let now = now_ts();
        let watcher = controller
            .store
            .create_watcher(&WatcherRecord {
                id: "project-watch".into(),
                profile: controller.config.profile.clone(),
                project_id: Some(session.project_id.clone()),
                adapter_id: "manual".into(),
                name: "project-watch".into(),
                config_ref: "{}".into(),
                status: WatcherStatus::Stopped,
                last_event_at: 0,
                version: 0,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        assert!(
            controller
                .start_watcher(&watcher.id)
                .unwrap_err()
                .to_string()
                .contains("trusted")
        );
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        assert_eq!(
            controller.start_watcher(&watcher.id).unwrap().status,
            WatcherStatus::Running
        );
    }

    #[test]
    fn list_project_watchers_filters_by_project() {
        let controller = controller(false);
        let first = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-first".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let second_root = tempfile::tempdir().unwrap();
        init_git_repo(second_root.path());
        let second = controller
            .create_session(CreateSession {
                path: second_root.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-second".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let first_watcher = controller
            .create_watcher_config(
                "first-watch".into(),
                "manual".into(),
                Some(first.project_id.clone()),
                "{}".into(),
            )
            .unwrap();
        controller
            .create_watcher_config(
                "second-watch".into(),
                "manual".into(),
                Some(second.project_id.clone()),
                "{}".into(),
            )
            .unwrap();

        let watchers = controller.list_project_watchers(&first.project_id).unwrap();

        assert_eq!(watchers.len(), 1);
        assert_eq!(watchers[0].id, first_watcher.id);
    }

    #[test]
    fn shell_watcher_start_executes_configured_command() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-shell".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let watcher = controller
            .create_watcher_config(
                "shell-watch".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                format!(
                    r#"{{"command":"printf watcher-fired","session_id":"{}"}}"#,
                    session.id
                ),
            )
            .unwrap();

        let started = controller.start_watcher(&watcher.id).unwrap();
        let events = controller.list_watcher_events(&watcher.id, 0, 10).unwrap();
        let output = controller.output(&session.id, 20, false).unwrap();

        assert_eq!(started.status, WatcherStatus::Running);
        assert!(events.iter().any(|event| {
            event.source == "shell"
                && event.event_type == "output"
                && event.payload_ref.contains("watcher-fired")
                && event.route_decision == format!("session:{}", session.id)
                && event.delivered
        }));
        assert!(output.text.contains("watcher-fired"));
    }

    #[test]
    fn shell_watcher_timeout_marks_watcher_errored() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-timeout".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let watcher = controller
            .create_watcher_config(
                "shell-timeout".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                r#"{"command":"sleep 1","timeout_ms":25}"#.into(),
            )
            .unwrap();

        let err = controller.start_watcher(&watcher.id).unwrap_err();
        let watcher = controller.store.get_watcher(&watcher.id).unwrap();
        let events = controller.list_watcher_events(&watcher.id, 0, 10).unwrap();

        assert!(err.to_string().contains("timed out"));
        assert_eq!(watcher.status, WatcherStatus::Errored);
        assert!(events.iter().any(|event| {
            event.event_type == "timeout"
                && event.payload_ref.contains("timed out")
                && !event.delivered
        }));
    }

    #[test]
    fn poll_watcher_runs_running_shell_watcher_again() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-poll".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let watcher = controller
            .create_watcher_config(
                "shell-poll".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                r#"{"command":"printf watcher-polled"}"#.into(),
            )
            .unwrap();

        controller.start_watcher(&watcher.id).unwrap();
        let polled = controller.poll_watcher(&watcher.id).unwrap();
        let events = controller.list_watcher_events(&watcher.id, 0, 10).unwrap();
        let output_events = events
            .iter()
            .filter(|event| {
                event.event_type == "output" && event.payload_ref.contains("watcher-polled")
            })
            .count();

        assert_eq!(polled.status, WatcherStatus::Running);
        assert_eq!(output_events, 2);
    }

    #[test]
    fn watcher_actions_accept_unique_name() {
        let controller = controller(false);
        let watcher = controller
            .create_watcher_config("named-watch".into(), "manual".into(), None, "{}".into())
            .unwrap();

        assert_eq!(
            controller.start_watcher("named-watch").unwrap().id,
            watcher.id
        );
        let event = controller
            .ingest_watcher_event(
                "named-watch",
                "test".into(),
                "event".into(),
                "payload".into(),
                "not_applicable".into(),
            )
            .unwrap();

        assert_eq!(event.watcher_id, watcher.id);
        assert!(
            controller
                .list_watcher_events("named-watch", 0, 10)
                .unwrap()
                .iter()
                .any(|event| event.payload_ref == "payload")
        );
        assert_eq!(
            controller.test_watcher("named-watch").unwrap().id,
            watcher.id
        );
        assert_eq!(
            controller.stop_watcher("named-watch").unwrap().status,
            WatcherStatus::Stopped
        );
        controller.delete_watcher("named-watch").unwrap();
        assert!(controller.list_watchers().unwrap().is_empty());
    }

    #[test]
    fn duplicate_watcher_names_are_rejected() {
        let controller = controller(false);
        controller
            .create_watcher_config("dupe".into(), "manual".into(), None, "{}".into())
            .unwrap();
        controller
            .create_watcher_config("dupe".into(), "manual".into(), None, "{}".into())
            .unwrap();

        let err = controller.start_watcher("dupe").unwrap_err();

        assert_eq!(err.to_string(), "watcher name is ambiguous: dupe");
    }

    #[test]
    fn start_watcher_requires_existing_watcher() {
        let controller = controller(false);
        let err = controller.start_watcher("missing-watch").unwrap_err();

        assert_eq!(err.to_string(), "watcher not found: missing-watch");
        assert!(controller.list_watchers().unwrap().is_empty());
    }

    #[test]
    fn poll_running_watchers_only_polls_running_watchers() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-poll-all".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let running = controller
            .create_watcher_config(
                "running-poll".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                r#"{"command":"printf running-poll"}"#.into(),
            )
            .unwrap();
        let stopped = controller
            .create_watcher_config(
                "stopped-poll".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                r#"{"command":"printf stopped-poll"}"#.into(),
            )
            .unwrap();

        controller.start_watcher(&running.id).unwrap();
        let polled = controller.poll_running_watchers().unwrap();
        let running_events = controller
            .list_watcher_events(&running.id, 0, 10)
            .unwrap()
            .into_iter()
            .filter(|event| event.payload_ref.contains("running-poll"))
            .count();
        let stopped_events = controller
            .list_watcher_events(&stopped.id, 0, 10)
            .unwrap()
            .into_iter()
            .filter(|event| event.payload_ref.contains("stopped-poll"))
            .count();

        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].id, running.id);
        assert_eq!(running_events, 2);
        assert_eq!(stopped_events, 0);
    }

    #[test]
    fn poll_running_watchers_continues_after_watcher_failure() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        let data = tempfile::tempdir().unwrap();
        let controller = controller_with_data_dir(data.path(), false);
        let session = controller
            .create_session(CreateSession {
                path: repo.path().to_string_lossy().to_string(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-poll-failure".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let failing = controller
        .create_watcher_config(
            "failing-poll".into(),
            "shell".into(),
            Some(session.project_id.clone()),
            json!({
                "command": "if test -f poll-fail; then printf first-fail >&2; exit 7; fi; touch poll-fail; printf first-ok"
            })
            .to_string(),
        )
        .unwrap();
        let healthy = controller
            .create_watcher_config(
                "healthy-poll".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                json!({"command": "printf second-ok"}).to_string(),
            )
            .unwrap();
        controller.start_watcher(&healthy.id).unwrap();
        controller.start_watcher(&failing.id).unwrap();

        let err = controller.poll_running_watchers().unwrap_err().to_string();
        let healthy_events = controller
            .list_watcher_events(&healthy.id, 0, 10)
            .unwrap()
            .into_iter()
            .filter(|event| event.payload_ref.contains("second-ok"))
            .count();

        assert!(err.contains("first-fail"));
        assert_eq!(healthy_events, 2);
    }

    #[test]
    fn watcher_poll_skips_running_manual_watchers() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-manual-poll".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let shell = controller
            .create_watcher_config(
                "shell-poll".into(),
                "shell".into(),
                Some(session.project_id.clone()),
                r#"{"command":"printf shell-poll"}"#.into(),
            )
            .unwrap();
        let manual = controller
            .create_watcher_config("manual-poll".into(), "manual".into(), None, "{}".into())
            .unwrap();
        controller.start_watcher(&shell.id).unwrap();
        controller.start_watcher(&manual.id).unwrap();

        let err = controller.poll_watcher(&manual.id).unwrap_err().to_string();
        let polled = controller.poll_running_watchers().unwrap();

        assert!(err.contains("does not support polling: manual"));
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].id, shell.id);
    }

    #[test]
    fn poll_running_watchers_rejects_read_only_without_running_watchers() {
        let controller = controller(true);
        let err = controller.poll_running_watchers().unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[test]
    fn external_watcher_event_routes_to_configured_session() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-external".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let watcher = controller
            .create_watcher_config(
                "external-watch".into(),
                "manual".into(),
                None,
                format!(r#"{{"session_id":"{}"}}"#, session.id),
            )
            .unwrap();
        controller.start_watcher(&watcher.id).unwrap();

        let event = controller
            .ingest_watcher_event(
                &watcher.id,
                "external".into(),
                "push".into(),
                "external-payload".into(),
                "verified".into(),
            )
            .unwrap();
        let output = controller.output(&session.id, 20, false).unwrap();
        let watcher = controller.store.get_watcher(&watcher.id).unwrap();

        assert_eq!(event.route_decision, format!("session:{}", session.id));
        assert!(event.delivered);
        assert_eq!(watcher.last_event_at, event.created_at);
        assert!(output.text.contains("external-payload"));
    }

    #[test]
    fn external_watcher_event_requires_running_watcher() {
        let controller = controller(false);
        let watcher = controller
            .create_watcher_config(
                "external-stopped".into(),
                "manual".into(),
                None,
                "{}".into(),
            )
            .unwrap();

        let err = controller
            .ingest_watcher_event(
                &watcher.id,
                "external".into(),
                "push".into(),
                "ignored".into(),
                "verified".into(),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("watcher is not running"));
        assert!(
            controller
                .list_watcher_events(&watcher.id, 0, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn external_watcher_event_requires_verified_signature_when_configured() {
        let controller = controller(false);
        let watcher = controller
            .create_watcher_config(
                "signed-watch".into(),
                "manual".into(),
                None,
                r#"{"require_signature":true}"#.into(),
            )
            .unwrap();
        controller.start_watcher(&watcher.id).unwrap();
        let event_count = controller
            .list_watcher_events(&watcher.id, 0, 10)
            .unwrap()
            .len();

        let err = controller
            .ingest_watcher_event(
                &watcher.id,
                "webhook".into(),
                "push".into(),
                "unsigned".into(),
                "not_applicable".into(),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("signature must be verified"));
        assert_eq!(
            controller
                .list_watcher_events(&watcher.id, 0, 10)
                .unwrap()
                .len(),
            event_count
        );

        let event = controller
            .ingest_watcher_event(
                &watcher.id,
                "webhook".into(),
                "push".into(),
                "signed".into(),
                "verified".into(),
            )
            .unwrap();
        assert_eq!(event.signature_status, "verified");
    }

    #[test]
    fn external_project_watcher_event_requires_trusted_project() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "watch-external-trust".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        controller
            .set_project_trust(&session.project_id, true)
            .unwrap();
        let watcher = controller
            .create_watcher_config(
                "external-trust-watch".into(),
                "manual".into(),
                Some(session.project_id.clone()),
                "{}".into(),
            )
            .unwrap();
        controller.start_watcher(&watcher.id).unwrap();
        let event_count = controller
            .list_watcher_events(&watcher.id, 0, 10)
            .unwrap()
            .len();
        controller
            .set_project_trust(&session.project_id, false)
            .unwrap();

        let err = controller
            .ingest_watcher_event(
                &watcher.id,
                "external".into(),
                "push".into(),
                "ignored".into(),
                "verified".into(),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("trusted"));
        assert_eq!(
            controller
                .list_watcher_events(&watcher.id, 0, 10)
                .unwrap()
                .len(),
            event_count
        );
    }

    #[test]
    fn read_only_rejects_mutations() {
        let controller = controller(true);
        let err = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "demo".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("read-only"));
    }

    #[test]
    fn sandbox_session_records_workspace_and_starts() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "sandbox".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: true,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        assert_eq!(session.status, SessionStatus::Running);
        let workspace = controller
            .store
            .get_workspace(&session.workspace_id)
            .unwrap();
        assert_eq!(workspace.sandbox_id.as_deref(), Some("local"));
    }

    #[test]
    fn failed_start_marks_session_errored() {
        let controller = controller_with_runtime(FakeRuntime::failing_start(), false);
        let err = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "fails".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("fake runtime start failed"));

        let sessions = controller.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Errored);
        assert_eq!(
            controller.status(&sessions[0].id).unwrap().status,
            SessionStatus::Errored
        );
        assert_eq!(
            controller.events(&sessions[0].id, 0, 10).unwrap()[0].payload["source"],
            "runtime_start_failed"
        );
    }

    #[test]
    fn status_snapshot_reports_waiting_after_input_without_mutating_session_status() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "waiting".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        controller.send(&session.id, "next").unwrap();
        let snapshot = controller.status_snapshot(&session.id).unwrap();
        let source_event = snapshot
            .source_event_id
            .and_then(|id| {
                controller
                    .events(&session.id, 0, 20)
                    .unwrap()
                    .into_iter()
                    .find(|event| event.id == id)
            })
            .unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Waiting);
        assert_eq!(snapshot.source, "session_event");
        assert_eq!(source_event.kind, "agent_state");
        assert_eq!(source_event.payload["source"], "user_input");
        assert_eq!(
            controller.get_session(&session.id).unwrap().status,
            SessionStatus::Running
        );
    }

    #[test]
    fn initial_prompt_records_waiting_agent_state() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "prompt".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: Some("boot".into()),
                parent_session_id: None,
            })
            .unwrap();

        let snapshot = controller.status_snapshot(&session.id).unwrap();
        let source_event = snapshot
            .source_event_id
            .and_then(|id| {
                controller
                    .events(&session.id, 0, 20)
                    .unwrap()
                    .into_iter()
                    .find(|event| event.id == id)
            })
            .unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Waiting);
        assert_eq!(snapshot.source, "session_event");
        assert_eq!(source_event.kind, "agent_state");
        assert_eq!(source_event.payload["source"], "initial_prompt");
    }

    #[test]
    fn status_snapshot_reports_running_from_runtime_agent_state_event() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "running".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let snapshot = controller.status_snapshot(&session.id).unwrap();
        let source_event = snapshot
            .source_event_id
            .and_then(|id| {
                controller
                    .events(&session.id, 0, 20)
                    .unwrap()
                    .into_iter()
                    .find(|event| event.id == id)
            })
            .unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Running);
        assert_eq!(snapshot.source, "session_event");
        assert_eq!(source_event.kind, "agent_state");
        assert_eq!(source_event.payload["source"], "runtime_start");
    }

    #[test]
    fn record_session_event_accepts_tool_native_agent_state() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "tool-state".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let event = controller
            .record_session_event(
                &session.id,
                "agent_state",
                json!({
                    "state": "WORKING",
                    "source": "claude",
                    "tool": "Bash",
                    "message": "Authorization: Bearer secret",
                }),
            )
            .unwrap();
        let snapshot = controller.status_snapshot(&session.id).unwrap();

        assert_eq!(event.kind, "agent_state");
        assert_eq!(event.payload["state"], "working");
        assert_eq!(event.payload["source"], "claude");
        assert_eq!(event.payload["tool"]["name"], "Bash");
        assert_eq!(event.payload["message"], "Authorization: Bearer [REDACTED]");
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Running);
        assert_eq!(snapshot.source_event_id, Some(event.id));
    }

    #[test]
    fn record_session_event_accepts_gemini_transcript_status_payload() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "gemini".into(),
                command: "cat".into(),
                name: "gemini-transcript".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let event = controller
            .record_session_event(
                &session.id,
                "agent_state",
                json!({
                    "source": "gemini_transcript",
                    "messages": [
                        {"type": "user", "content": "run tests"},
                        {
                            "type": "gemini",
                            "toolCalls": [
                                {"name": "Shell", "status": "executing"}
                            ]
                        }
                    ]
                }),
            )
            .unwrap();

        let snapshot = controller.status_snapshot(&session.id).unwrap();
        let activity = snapshot.activity.unwrap();
        assert_eq!(event.kind, "agent_state");
        assert_eq!(event.payload["source"], "gemini_transcript");
        assert!(event.payload.get("state").is_none());
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Running);
        assert_eq!(snapshot.source_event_id, Some(event.id));
        assert_eq!(activity.source, "gemini_transcript");
        assert_eq!(activity.state, "working");
        assert_eq!(activity.tool.as_deref(), Some("Shell"));
    }

    #[test]
    fn record_cost_rejects_invalid_token_counts() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "cost-validation".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        for payload in [
            json!({"input_tokens": -1}),
            json!({"output_tokens": 1.5}),
            json!({"total_tokens": "many"}),
        ] {
            let error = controller
                .record_cost(&session.id, 0.01, payload)
                .unwrap_err();
            assert!(error.to_string().contains("non-negative integer"));
        }

        controller
            .record_cost(
                &session.id,
                0.01,
                json!({"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}),
            )
            .unwrap();
        let summary = controller
            .cost_summary(CostFilter {
                profile: controller.config.profile.clone(),
                project_id: None,
                group_name: None,
                session_id: Some(session.id),
                agent: None,
                model: None,
                start_at: 0,
                end_at: now_ts(),
            })
            .unwrap();
        assert_eq!(summary.total_tokens, 3);
    }

    #[test]
    fn ai_agent_launch_spec_wraps_command_with_state_emitter() {
        let controller = controller_with_codex_hooks();
        let session = controller
            .create_session_with_start(
                CreateSession {
                    path: ".".into(),
                    agent: "codex".into(),
                    command: "codex --resume".into(),
                    name: "wrapped".into(),
                    group_name: "default".into(),
                    worktree: None,
                    carry_state: false,
                    sandbox: false,
                    prompt: None,
                    parent_session_id: None,
                },
                false,
                None,
            )
            .unwrap();

        let spec = controller.launch_spec(&session).unwrap();

        assert!(spec.command.contains("session record-event"));
        assert!(spec.command.contains("agent_helm_emit_state working"));
        assert!(
            spec.command
                .contains("trap 'code=$?; agent_helm_emit_state idle; exit $code' EXIT")
        );
        assert!(spec.command.contains("--tool codex"));
        assert!(spec.command.ends_with("codex --resume"));
    }

    #[test]
    fn command_only_launch_specs_do_not_use_state_wrapper() {
        let controller = controller(false);
        let session = controller
            .create_session_with_start(
                CreateSession {
                    path: ".".into(),
                    agent: "shell".into(),
                    command: "cat".into(),
                    name: "plain".into(),
                    group_name: "default".into(),
                    worktree: None,
                    carry_state: false,
                    sandbox: false,
                    prompt: None,
                    parent_session_id: None,
                },
                false,
                None,
            )
            .unwrap();

        let spec = controller.launch_spec(&session).unwrap();

        assert_eq!(spec.command, "cat");
    }

    #[test]
    fn ai_agent_runtime_lifecycle_does_not_override_wrapper_state() {
        let controller = controller_with_codex_hooks();
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "codex".into(),
                command: "codex --resume".into(),
                name: "wrapped-running".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let events = controller.events(&session.id, 0, 20).unwrap();

        assert!(!events.iter().any(|event| {
            event.kind == "agent_state" && event.payload["source"].as_str() == Some("runtime_start")
        }));
    }

    #[test]
    fn ai_agent_restart_runtime_state_clears_stale_waiting_state() {
        let controller = controller_with_codex_hooks();
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "codex".into(),
                command: "codex --resume".into(),
                name: "wrapped-restart".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: Some("boot".into()),
                parent_session_id: None,
            })
            .unwrap();

        assert_eq!(
            controller.status_snapshot(&session.id).unwrap().deck_status,
            SessionDeckStatus::Waiting
        );

        controller.restart(&session.id).unwrap();
        let snapshot = controller.status_snapshot(&session.id).unwrap();
        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Running);

        let source_event = snapshot
            .source_event_id
            .and_then(|id| {
                controller
                    .events(&session.id, 0, 20)
                    .unwrap()
                    .into_iter()
                    .find(|event| event.id == id)
            })
            .unwrap();
        assert_eq!(source_event.payload["source"], "runtime_restart");
    }

    #[test]
    fn sandboxed_ai_agent_keeps_runtime_agent_state_fallback() {
        let controller = controller_with_codex_hooks_and_sandbox();
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "codex".into(),
                command: "codex --resume".into(),
                name: "sandboxed-ai".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: true,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let spec = controller.launch_spec(&session).unwrap();
        let events = controller.events(&session.id, 0, 20).unwrap();

        assert!(spec.sandbox.is_some());
        assert!(!spec.command.contains("agent_helm_emit_state"));
        assert!(events.iter().any(|event| {
            event.kind == "agent_state" && event.payload["source"].as_str() == Some("runtime_start")
        }));
    }

    #[test]
    fn record_session_event_rejects_unsupported_public_events() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "bad-state".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();

        let bad_kind = controller
            .record_session_event(&session.id, "output", json!({"state": "working"}))
            .unwrap_err()
            .to_string();
        let bad_state = controller
            .record_session_event(&session.id, "agent_state", json!({"state": "paused"}))
            .unwrap_err()
            .to_string();
        let bad_tool = controller
            .record_session_event(
                &session.id,
                "agent_state",
                json!({"state": "working", "tool": ["Bash"]}),
            )
            .unwrap_err()
            .to_string();
        let missing_tool_name = controller
            .record_session_event(
                &session.id,
                "agent_state",
                json!({"state": "working", "tool": {}}),
            )
            .unwrap_err()
            .to_string();
        let bad_source = controller
            .record_session_event(
                &session.id,
                "agent_state",
                json!({"state": "working", "source": 42}),
            )
            .unwrap_err()
            .to_string();
        let empty_source = controller
            .record_session_event(
                &session.id,
                "agent_state",
                json!({"state": "working", "source": "  "}),
            )
            .unwrap_err()
            .to_string();

        assert!(bad_kind.contains("only agent_state"));
        assert!(bad_state.contains("queued, waiting"));
        assert!(bad_tool.contains("tool must be"));
        assert!(missing_tool_name.contains("tool name is required"));
        assert!(bad_source.contains("source must be a string"));
        assert!(empty_source.contains("source cannot be empty"));
    }

    #[test]
    fn status_snapshot_reports_queued_from_open_conductor_assignment() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "queued".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let conductor = controller.create_conductor(session.id.clone()).unwrap();
        controller
            .store
            .append_conductor_assignment(&ConductorAssignmentRecord {
                id: "queued-assignment".into(),
                conductor_id: conductor.id,
                session_id: session.id.clone(),
                task_ref: "task".into(),
                status: "queued".into(),
                assigned_at: now_ts(),
                completed_at: 0,
            })
            .unwrap();

        let snapshot = controller.status_snapshot(&session.id).unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Queued);
        assert_eq!(snapshot.source, "conductor_assignment");
    }

    #[test]
    fn status_snapshot_reports_waiting_from_open_assigned_conductor_assignment() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "assigned".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let conductor = controller.create_conductor(session.id.clone()).unwrap();
        controller
            .store
            .append_conductor_assignment(&ConductorAssignmentRecord {
                id: "assigned-assignment".into(),
                conductor_id: conductor.id,
                session_id: session.id.clone(),
                task_ref: "task".into(),
                status: "assigned".into(),
                assigned_at: now_ts(),
                completed_at: 0,
            })
            .unwrap();

        let snapshot = controller.status_snapshot(&session.id).unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Waiting);
        assert_eq!(snapshot.source, "conductor_assignment");
    }

    #[test]
    fn status_snapshot_uses_latest_status_signal_beyond_raw_event_window() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "noisy".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let idle = controller
            .store
            .append_session_event(
                &session.id,
                "agent_state",
                json!({"state": "idle", "source": "test"}),
            )
            .unwrap();
        for index in 0..21 {
            controller
                .store
                .append_session_event(
                    &session.id,
                    "status",
                    json!({"status": "running", "source": "noise", "index": index}),
                )
                .unwrap();
        }

        let snapshot = controller.status_snapshot(&session.id).unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Idle);
        assert_eq!(snapshot.source_event_id, Some(idle.id));
        assert_eq!(snapshot.activity.as_ref().unwrap().label, "ready");
        assert_eq!(snapshot.activity.as_ref().unwrap().source, "test");
    }

    #[test]
    fn read_only_status_reconciliation_does_not_persist_runtime_changes() {
        let data = tempfile::tempdir().unwrap();
        let writer = controller_with_data_dir(data.path(), false);
        let session = writer
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "readonly-status".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        assert_eq!(session.status, SessionStatus::Running);
        assert!(session.runtime_id.is_some());
        let event_count = writer.events(&session.id, 0, 100).unwrap().len();

        let reader = controller_with_data_dir(data.path(), true);
        let snapshot = reader.status_snapshot(&session.id).unwrap();

        assert_eq!(snapshot.lifecycle_status, SessionStatus::Stopped);
        assert!(snapshot.session.runtime_id.is_none());
        let stored = writer.get_session(&session.id).unwrap();
        assert_eq!(stored.status, SessionStatus::Running);
        assert!(stored.runtime_id.is_some());
        assert_eq!(
            writer.events(&session.id, 0, 100).unwrap().len(),
            event_count
        );
    }

    #[test]
    fn completed_conductor_assignment_no_longer_drives_status_snapshot() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "completed".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let conductor = controller.create_conductor(session.id.clone()).unwrap();
        controller
            .store
            .append_conductor_assignment(&ConductorAssignmentRecord {
                id: "completed-assignment".into(),
                conductor_id: conductor.id,
                session_id: session.id.clone(),
                task_ref: "task".into(),
                status: "assigned".into(),
                assigned_at: now_ts(),
                completed_at: 0,
            })
            .unwrap();

        let completed = controller
            .complete_conductor_assignment("completed-assignment", "completed")
            .unwrap();
        let snapshot = controller.status_snapshot(&session.id).unwrap();

        assert_eq!(completed.status, "completed");
        assert!(completed.completed_at > 0);
        assert_eq!(snapshot.lifecycle_status, SessionStatus::Running);
        assert_eq!(snapshot.deck_status, SessionDeckStatus::Running);
        assert_eq!(snapshot.source, "session_event");
    }

    #[test]
    fn conductor_assignment_terminal_statuses_are_validated() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
                command: "cat".into(),
                name: "terminal-assignment".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap();
        let conductor = controller.create_conductor(session.id.clone()).unwrap();

        for assignment_id in [
            "failed-assignment",
            "cancelled-assignment",
            "invalid-assignment",
        ] {
            controller
                .store
                .append_conductor_assignment(&ConductorAssignmentRecord {
                    id: assignment_id.into(),
                    conductor_id: conductor.id.clone(),
                    session_id: session.id.clone(),
                    task_ref: "task".into(),
                    status: "assigned".into(),
                    assigned_at: now_ts(),
                    completed_at: 0,
                })
                .unwrap();
        }

        let failed = controller
            .complete_conductor_assignment("failed-assignment", "failed")
            .unwrap();
        let cancelled = controller
            .complete_conductor_assignment("cancelled-assignment", "canceled")
            .unwrap();
        let invalid = controller
            .complete_conductor_assignment("invalid-assignment", "paused")
            .unwrap_err()
            .to_string();
        let closed_change = controller
            .complete_conductor_assignment("failed-assignment", "completed")
            .unwrap_err()
            .to_string();
        let idempotent = controller
            .complete_conductor_assignment("failed-assignment", "failed")
            .unwrap();

        assert_eq!(failed.status, "failed");
        assert!(failed.completed_at > 0);
        assert_eq!(cancelled.status, "cancelled");
        assert!(cancelled.completed_at > 0);
        assert!(invalid.contains("completed, failed, or cancelled"));
        assert!(closed_change.contains("already closed as failed"));
        assert_eq!(idempotent.completed_at, failed.completed_at);
    }

    #[test]
    fn failed_launch_spec_marks_session_errored() {
        let controller = controller(false);
        let err = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "custom".into(),
                command: String::new(),
                name: "fails".into(),
                group_name: "default".into(),
                worktree: None,
                carry_state: false,
                sandbox: false,
                prompt: None,
                parent_session_id: None,
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("agent requires command: custom"));
        let sessions = controller.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Errored);
        assert_eq!(
            controller.events(&sessions[0].id, 0, 10).unwrap()[0].payload["source"],
            "launch_spec_failed"
        );
    }
}
