use crate::{
    adapter::AgentRegistry,
    config::AppConfig,
    error::{AppError, Result},
    materialization::session_materialization_json,
    models::{
        ArchiveSessionRequest, ArchiveSessionResult, AttachmentStatus, CleanupReport,
        ConductorAssignmentRecord, ConductorRecord, ConductorStatus, CostEvent, CostFilter,
        CostSummary, CreateSession, DeleteMode, DeleteSessionRequest, DeletionResult,
        ForkSessionRequest, ForkSessionResult, GroupRecord, LaunchSpec, McpAttachmentRecord,
        OutputPage, ProjectRecord, ProjectSpec, ProjectTrustState, SandboxLaunchSpec,
        SessionRecord, SessionStatus, SkillAttachmentRecord, StructuredEvent, WatcherEventRecord,
        WatcherRecord, WatcherStatus, WorkspaceRecord, WorktreeRecord, WorktreeStatus, now_ts,
    },
    runtime::SessionRuntime,
    security::{
        TrustGatedOperation, redact_event_payload, require_project_trust,
        validate_sandbox_allowed_path,
    },
    store::SessionStore,
    workspace::WorkspaceManager,
};
use serde_json::json;
use std::{path::Path, process::Command};
use uuid::Uuid;

#[cfg(feature = "serve")]
use crate::api::{AgentHelmApi, ApiResult};

#[derive(Clone)]
pub struct ApplicationController<R: SessionRuntime> {
    pub config: AppConfig,
    pub store: SessionStore,
    runtime: R,
    agents: AgentRegistry,
    workspace: WorkspaceManager,
    read_only: bool,
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
        })
    }

    pub fn init_profile(&self) -> Result<()> {
        self.store.init()
    }

    fn launch_spec(&self, session: &SessionRecord) -> Result<LaunchSpec> {
        let mut spec = self.agents.resume(session)?;
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
        Ok(spec)
    }

    pub fn create_session(&self, request: CreateSession) -> Result<SessionRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let project = self.ensure_project(&request.path, false)?;
        let mut project_path = project.root_path.clone();
        let mut worktree_id = None;
        if let Some(branch) = request
            .worktree
            .as_deref()
            .filter(|branch| !branch.is_empty())
        {
            let info = self.workspace.create_worktree_with_state(
                &project.root_path,
                branch,
                request.carry_state,
            )?;
            if project.trust_state == ProjectTrustState::Trusted {
                self.workspace
                    .run_setup_hooks(&project.root_path, &info.path)?;
            }
            let now = now_ts();
            let worktree = self.store.create_worktree(&WorktreeRecord {
                id: Uuid::new_v4().simple().to_string(),
                project_id: project.id.clone(),
                path: info.path.to_string_lossy().to_string(),
                branch: info.branch.unwrap_or_else(|| branch.to_string()),
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
        if request.sandbox {
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
            group_name: defaulted(request.group_name, &self.config.default_group),
            project_id: project.id,
            workspace_id: workspace.id,
            worktree_id,
            parent_session_id: request.parent_session_id,
            agent: defaulted(request.agent, &self.config.default_agent),
            command: request.command,
            project_path,
            status: SessionStatus::Starting,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: now,
            updated_at: now,
        };

        let session = self.store.create_session(&session)?;
        let spec = self.launch_spec(&session)?;
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
        }
        Ok(session)
    }

    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionRecord>> {
        self.store.init()?;
        self.store.list_sessions(include_archived)
    }

    pub fn list_groups(&self) -> Result<Vec<GroupRecord>> {
        self.store.init()?;
        self.store.list_groups(&self.config.profile)
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

    pub fn get_session(&self, id: &str) -> Result<SessionRecord> {
        self.store.init()?;
        self.store.get_session(id)
    }

    pub fn status(&self, id: &str) -> Result<SessionRecord> {
        let session = self.get_session(id)?;
        let runtime_status = self
            .agents
            .detect_status(&session.agent, self.runtime.status(id)?)?;
        if runtime_status != session.status {
            return match self
                .store
                .update_status(id, session.version, runtime_status)
            {
                Ok(updated) => {
                    self.append_status_event(id, updated.status, "runtime_status")?;
                    Ok(updated)
                }
                Err(_) => self.store.get_session(id),
            };
        }
        Ok(session)
    }

    pub fn output(&self, id: &str, limit: usize, ansi: bool) -> Result<OutputPage> {
        self.runtime.capture(id, limit, ansi)
    }

    pub fn diff(&self, id: &str) -> Result<String> {
        let session = self.get_session(id)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&session.project_path)
            .arg("diff")
            .output()?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(AppError::msg(format!(
                "git diff failed for {}: {}",
                session.project_path,
                stderr.trim()
            )))
        }
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

    pub fn stop(&self, id: &str) -> Result<SessionRecord> {
        self.ensure_writable()?;
        let session = self.get_session(id)?;
        self.runtime.stop(id)?;
        let session = self
            .store
            .update_status(id, session.version, SessionStatus::Stopped)?;
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
        Ok(session)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.ensure_writable()?;
        let _ = self.runtime.destroy(id);
        self.store.remove_session(id)
    }

    pub fn archive_session(&self, request: ArchiveSessionRequest) -> Result<ArchiveSessionResult> {
        self.ensure_writable()?;
        let mut runtime_stopped = false;
        if request.stop_if_running {
            let _ = self.runtime.destroy(&request.session_id);
            runtime_stopped = true;
        }
        let session = self.store.archive_session(&request.session_id)?;
        self.store.append_session_event(
            &request.session_id,
            "archived",
            json!({ "by": request.archived_by, "reason": request.reason }),
        )?;
        Ok(ArchiveSessionResult {
            session_id: session.id,
            archived: session.archived,
            runtime_stopped,
        })
    }

    pub fn delete_session(&self, request: DeleteSessionRequest) -> Result<DeletionResult> {
        self.ensure_writable()?;
        let _ = self.runtime.destroy(&request.session_id);
        match request.mode {
            DeleteMode::Purge => self.store.purge_session(&request.session_id)?,
            _ => self.store.remove_session(&request.session_id)?,
        }
        Ok(DeletionResult {
            session_id: request.session_id,
            deleted: true,
            runtime_stopped: true,
            worktree_cleaned: matches!(request.mode, DeleteMode::CleanupWorktree),
            purged: matches!(request.mode, DeleteMode::Purge),
            history_retained: !matches!(request.mode, DeleteMode::Purge),
        })
    }

    pub fn fork_session(&self, request: ForkSessionRequest) -> Result<ForkSessionResult> {
        self.ensure_writable()?;
        let parent = self.get_session(&request.parent_session_id)?;
        let fork_plan = self.agents.fork(&parent, &request)?;
        let create = CreateSession {
            path: parent.project_path.clone(),
            agent: parent.agent.clone(),
            command: fork_plan.command,
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
        let child = self.create_session(create)?;
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
            started: true,
        })
    }

    pub fn register_project(&self, spec: ProjectSpec) -> Result<ProjectRecord> {
        self.ensure_writable()?;
        self.store.init()?;
        let project_ref = self.workspace.resolve_project_ref(&spec.root_path)?;
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
        self.store.delete_project(id)
    }

    pub fn list_worktrees(&self, project_id: &str) -> Result<Vec<WorktreeRecord>> {
        self.store.init()?;
        self.store.list_worktrees(project_id)
    }

    pub fn create_project_worktree(
        &self,
        project_id: &str,
        branch: &str,
    ) -> Result<WorktreeRecord> {
        self.ensure_writable()?;
        let project = self.get_project(project_id)?;
        let info = self.workspace.create_worktree(&project.root_path, branch)?;
        if project.trust_state == ProjectTrustState::Trusted {
            self.workspace
                .run_setup_hooks(&project.root_path, &info.path)?;
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
        self.ensure_writable()?;
        let mut worktree = self.store.get_worktree(id)?;
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
                self.workspace.finish_worktree(&worktree.path)?;
                worktree.updated_at = now_ts();
                self.store.update_worktree(&worktree, worktree.version)?;
                report.removed += 1;
            } else {
                report.skipped += 1;
            }
        }
        Ok(report)
    }

    pub fn list_mcp_attachments(&self) -> Result<Vec<McpAttachmentRecord>> {
        self.store
            .list_mcp_attachments(&self.config.profile, None, None, None)
    }

    pub fn attach_mcp(&self, session_id: &str, server_id: String) -> Result<McpAttachmentRecord> {
        self.ensure_writable()?;
        let now = now_ts();
        self.store.create_mcp_attachment(&McpAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "session".to_string(),
            project_id: None,
            session_id: Some(session_id.to_string()),
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
        let plan_json = session_materialization_json(
            &self.list_mcp_attachments()?,
            &self.list_skill_attachments()?,
        );
        let mut synced = Vec::new();
        for mut attachment in self.list_mcp_attachments()? {
            if attachment.status == AttachmentStatus::Attached {
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

    pub fn list_skill_attachments(&self) -> Result<Vec<SkillAttachmentRecord>> {
        self.store
            .list_skill_attachments(&self.config.profile, None, None, None)
    }

    pub fn attach_skill(
        &self,
        session_id: &str,
        skill_id: String,
    ) -> Result<SkillAttachmentRecord> {
        self.ensure_writable()?;
        let now = now_ts();
        self.store.create_skill_attachment(&SkillAttachmentRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            scope: "session".to_string(),
            project_id: None,
            session_id: Some(session_id.to_string()),
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
        self.store.list_watchers(&self.config.profile, None)
    }

    pub fn create_watcher(&self, name: String, adapter_id: String) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        let now = now_ts();
        self.store.create_watcher(&WatcherRecord {
            id: Uuid::new_v4().simple().to_string(),
            profile: self.config.profile.clone(),
            project_id: None,
            adapter_id,
            name,
            config_ref: "{}".to_string(),
            status: WatcherStatus::Stopped,
            last_event_at: 0,
            version: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn start_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        let mut watcher = self
            .store
            .get_watcher(id_or_name)
            .or_else(|_| self.create_watcher(id_or_name.to_string(), "manual".to_string()))?;
        if let Some(project_id) = watcher.project_id.as_deref() {
            let project = self.store.get_project(project_id)?;
            require_project_trust(project.trust_state, TrustGatedOperation::Watchers)?;
        }
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
        Ok(watcher)
    }

    pub fn test_watcher(&self, id_or_name: &str) -> Result<WatcherRecord> {
        self.store.get_watcher(id_or_name).or_else(|_| {
            let now = now_ts();
            Ok(WatcherRecord {
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
            })
        })
    }

    pub fn stop_watcher(&self, id: &str) -> Result<WatcherRecord> {
        self.ensure_writable()?;
        let mut watcher = self.store.get_watcher(id)?;
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

    pub fn list_conductors(&self) -> Result<Vec<ConductorRecord>> {
        self.store.list_conductors(&self.config.profile)
    }

    pub fn get_conductor(&self, id: &str) -> Result<ConductorRecord> {
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

    pub fn cost_summary(&self, filter: CostFilter) -> Result<CostSummary> {
        self.store.cost_summary(&filter)
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
        if let Some(object) = payload.as_object_mut() {
            object
                .entry("agent".to_string())
                .or_insert_with(|| json!(session.agent));
        }
        self.store
            .append_cost_event(session_id, amount_usd, payload)
    }

    pub fn events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<crate::models::SessionEvent>> {
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
}

fn defaulted(value: String, default: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    }
}

fn project_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("session")
        .to_string()
}

#[cfg(feature = "serve")]
impl<R: SessionRuntime> AgentHelmApi for ApplicationController<R> {
    fn list_sessions(&self, include_archived: bool) -> ApiResult<Vec<SessionRecord>> {
        ApplicationController::list_sessions(self, include_archived).map_err(Into::into)
    }

    fn create_session(&self, request: CreateSession) -> ApiResult<SessionRecord> {
        ApplicationController::create_session(self, request).map_err(Into::into)
    }

    fn get_session(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::get_session(self, id).map_err(Into::into)
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

    fn stop(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::stop(self, id).map_err(Into::into)
    }

    fn restart(&self, id: &str) -> ApiResult<SessionRecord> {
        ApplicationController::restart(self, id).map_err(Into::into)
    }

    fn delete_session(&self, request: DeleteSessionRequest) -> ApiResult<DeletionResult> {
        ApplicationController::delete_session(self, request).map_err(Into::into)
    }

    fn archive_session(&self, request: ArchiveSessionRequest) -> ApiResult<ArchiveSessionResult> {
        ApplicationController::archive_session(self, request).map_err(Into::into)
    }

    fn fork_session(&self, request: ForkSessionRequest) -> ApiResult<ForkSessionResult> {
        ApplicationController::fork_session(self, request).map_err(Into::into)
    }

    fn structured_events(
        &self,
        id: &str,
        since: i64,
        limit: usize,
    ) -> ApiResult<Vec<StructuredEvent>> {
        ApplicationController::structured_events(self, id, since, limit).map_err(Into::into)
    }

    fn list_groups(&self) -> ApiResult<Vec<GroupRecord>> {
        ApplicationController::list_groups(self).map_err(Into::into)
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

    fn list_worktrees(&self, project_id: &str) -> ApiResult<Vec<WorktreeRecord>> {
        ApplicationController::list_worktrees(self, project_id).map_err(Into::into)
    }

    fn create_worktree(&self, project_id: &str, branch: &str) -> ApiResult<WorktreeRecord> {
        ApplicationController::create_project_worktree(self, project_id, branch).map_err(Into::into)
    }

    fn list_mcp(&self) -> ApiResult<Vec<McpAttachmentRecord>> {
        ApplicationController::list_mcp_attachments(self).map_err(Into::into)
    }

    fn attach_mcp(&self, session_id: &str, server_id: String) -> ApiResult<McpAttachmentRecord> {
        ApplicationController::attach_mcp(self, session_id, server_id).map_err(Into::into)
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

    fn detach_skill(&self, id: &str) -> ApiResult<SkillAttachmentRecord> {
        ApplicationController::detach_skill(self, id).map_err(Into::into)
    }

    fn sync_skills(&self) -> ApiResult<Vec<SkillAttachmentRecord>> {
        ApplicationController::sync_skills(self).map_err(Into::into)
    }

    fn list_watchers(&self) -> ApiResult<Vec<WatcherRecord>> {
        ApplicationController::list_watchers(self).map_err(Into::into)
    }

    fn start_watcher(&self, id: &str) -> ApiResult<WatcherRecord> {
        ApplicationController::start_watcher(self, id).map_err(Into::into)
    }

    fn stop_watcher(&self, id: &str) -> ApiResult<WatcherRecord> {
        ApplicationController::stop_watcher(self, id).map_err(Into::into)
    }

    fn list_conductors(&self) -> ApiResult<Vec<ConductorRecord>> {
        ApplicationController::list_conductors(self).map_err(Into::into)
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(controller.list_sessions(false).unwrap().len(), 1);

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
        assert!(controller.list_sessions(false).unwrap().is_empty());
        let archived = controller.list_sessions(true).unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].archived);
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
    fn mcp_and_skill_sync_require_trusted_project() {
        let controller = controller(false);
        let session = controller
            .create_session(CreateSession {
                path: ".".into(),
                agent: "shell".into(),
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
        assert_eq!(controller.sync_mcp().unwrap().len(), 1);
        assert_eq!(controller.sync_skills().unwrap().len(), 1);
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

        let sessions = controller.list_sessions(true).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Errored);
        assert_eq!(
            controller.events(&sessions[0].id, 0, 10).unwrap()[0].payload["source"],
            "runtime_start_failed"
        );
    }
}
