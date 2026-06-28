use crate::{
    error::{AppError, Result},
    models::{
        AttachmentStatus, ConductorAssignmentRecord, ConductorRecord, ConductorStatus, CostEvent,
        CostFilter, CostSummary, GroupRecord, McpAttachmentRecord, ProjectRecord,
        ProjectTrustState, SessionEvent, SessionRecord, SessionStatus, SkillAttachmentRecord,
        WatcherEventRecord, WatcherRecord, WatcherStatus, WorkspaceRecord, WorktreeRecord,
        WorktreeStatus, now_ts,
    },
    security::{redact_event_payload, redact_json_value},
};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct SessionStore {
    db_path: PathBuf,
}

impl SessionStore {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    pub fn init(&self) -> Result<()> {
        self.ensure_parent_dir()?;
        let _lock = self.lock_profile()?;
        let mut conn = self.connect()?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;

        let tx = conn.transaction()?;
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY
            );
            "#,
        )?;

        let current: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;

        if current < 1 {
            tx.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    group_name TEXT NOT NULL,
                    agent TEXT NOT NULL,
                    command TEXT NOT NULL,
                    project_path TEXT NOT NULL,
                    status TEXT NOT NULL,
                    runtime_id TEXT,
                    archived INTEGER NOT NULL DEFAULT 0,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS cost_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    amount_usd REAL NOT NULL,
                    payload TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_sessions_profile_archived
                    ON sessions(profile, archived, updated_at);
                CREATE INDEX IF NOT EXISTS idx_session_events_session_created
                    ON session_events(session_id, created_at, id);
                CREATE INDEX IF NOT EXISTS idx_cost_events_session_created
                    ON cost_events(session_id, created_at, id);

                INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
                "#,
            )?;
        }

        if current < 2 {
            tx.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS projects (
                    id TEXT PRIMARY KEY,
                    profile TEXT NOT NULL,
                    root_path TEXT NOT NULL,
                    repo_identity TEXT NOT NULL,
                    default_branch TEXT NOT NULL,
                    trust_state TEXT NOT NULL,
                    hooks_hash TEXT NOT NULL,
                    config_hash TEXT NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    UNIQUE(profile, root_path)
                );

                CREATE TABLE IF NOT EXISTS worktrees (
                    id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    path TEXT NOT NULL,
                    branch TEXT NOT NULL,
                    base_branch TEXT NOT NULL,
                    status TEXT NOT NULL,
                    cleanup_allowed INTEGER NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS workspaces (
                    id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                    path TEXT NOT NULL,
                    worktree_id TEXT,
                    sandbox_id TEXT,
                    multi_repo_roots TEXT NOT NULL,
                    cleanup_policy TEXT NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS mcp_attachments (
                    id TEXT PRIMARY KEY,
                    profile TEXT NOT NULL,
                    scope TEXT NOT NULL,
                    project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
                    session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE,
                    server_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    materialized_state TEXT NOT NULL,
                    restart_required INTEGER NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS skill_attachments (
                    id TEXT PRIMARY KEY,
                    profile TEXT NOT NULL,
                    scope TEXT NOT NULL,
                    project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
                    session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE,
                    skill_id TEXT NOT NULL,
                    pool_path TEXT NOT NULL,
                    materialized_path TEXT NOT NULL,
                    status TEXT NOT NULL,
                    restart_required INTEGER NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS watchers (
                    id TEXT PRIMARY KEY,
                    profile TEXT NOT NULL,
                    project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
                    adapter_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    config_ref TEXT NOT NULL,
                    status TEXT NOT NULL,
                    last_event_at INTEGER NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS watcher_events (
                    id TEXT PRIMARY KEY,
                    watcher_id TEXT NOT NULL REFERENCES watchers(id) ON DELETE CASCADE,
                    source TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    payload_ref TEXT NOT NULL,
                    signature_status TEXT NOT NULL,
                    route_decision TEXT NOT NULL,
                    delivered INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS conductors (
                    id TEXT PRIMARY KEY,
                    profile TEXT NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    status TEXT NOT NULL,
                    watched_sessions TEXT NOT NULL,
                    channel_bindings TEXT NOT NULL,
                    last_heartbeat_at INTEGER NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS conductor_assignments (
                    id TEXT PRIMARY KEY,
                    conductor_id TEXT NOT NULL REFERENCES conductors(id) ON DELETE CASCADE,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    task_ref TEXT NOT NULL,
                    status TEXT NOT NULL,
                    assigned_at INTEGER NOT NULL,
                    completed_at INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_projects_profile
                    ON projects(profile, updated_at);
                CREATE INDEX IF NOT EXISTS idx_workspaces_project
                    ON workspaces(project_id, updated_at);
                CREATE INDEX IF NOT EXISTS idx_worktrees_project
                    ON worktrees(project_id, updated_at);
                CREATE INDEX IF NOT EXISTS idx_mcp_attachments_profile_scope
                    ON mcp_attachments(profile, scope, project_id, session_id);
                CREATE INDEX IF NOT EXISTS idx_skill_attachments_profile_scope
                    ON skill_attachments(profile, scope, project_id, session_id);
                CREATE INDEX IF NOT EXISTS idx_watchers_profile_project
                    ON watchers(profile, project_id, updated_at);
                CREATE INDEX IF NOT EXISTS idx_watcher_events_watcher_created
                    ON watcher_events(watcher_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_conductors_profile
                    ON conductors(profile, updated_at);
                CREATE INDEX IF NOT EXISTS idx_conductor_assignments_conductor
                    ON conductor_assignments(conductor_id, assigned_at);

                INSERT OR IGNORE INTO schema_migrations(version) VALUES (2);
            "#,
            )?;
        }

        if current < 3 {
            tx.execute_batch(
                r#"
                ALTER TABLE sessions ADD COLUMN project_id TEXT NOT NULL DEFAULT '';
                ALTER TABLE sessions ADD COLUMN workspace_id TEXT NOT NULL DEFAULT '';
                ALTER TABLE sessions ADD COLUMN worktree_id TEXT;
                ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;
                CREATE INDEX IF NOT EXISTS idx_sessions_project
                    ON sessions(profile, project_id, workspace_id);
                INSERT OR IGNORE INTO schema_migrations(version) VALUES (3);
                "#,
            )?;
        }

        if current < 4 {
            tx.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS groups (
                    id TEXT PRIMARY KEY,
                    profile TEXT NOT NULL,
                    name TEXT NOT NULL,
                    default_project_path TEXT NOT NULL,
                    collapsed INTEGER NOT NULL,
                    display_order INTEGER NOT NULL,
                    metadata TEXT NOT NULL,
                    version INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    UNIQUE(profile, name)
                );
                INSERT OR IGNORE INTO groups (
                    id, profile, name, default_project_path, collapsed, display_order,
                    metadata, version, created_at, updated_at
                )
                SELECT
                    lower(hex(randomblob(16))),
                    profile,
                    group_name,
                    '',
                    0,
                    ROW_NUMBER() OVER (PARTITION BY profile ORDER BY group_name) - 1,
                    '{}',
                    0,
                    MIN(created_at),
                    MAX(updated_at)
                FROM sessions
                GROUP BY profile, group_name;
                CREATE INDEX IF NOT EXISTS idx_groups_profile_order
                    ON groups(profile, display_order, name);
                INSERT OR IGNORE INTO schema_migrations(version) VALUES (4);
                "#,
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn create_session(&self, session: &SessionRecord) -> Result<SessionRecord> {
        self.with_transaction(|tx| {
            Self::ensure_group_in(
                tx,
                &session.profile,
                &session.group_name,
                &session.project_path,
                session.created_at,
            )?;
            tx.execute(
                r#"
                INSERT INTO sessions (
                    id, name, profile, group_name, agent, command, project_path, status,
                    runtime_id, archived, version, created_at, updated_at,
                    project_id, workspace_id, worktree_id, parent_session_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                "#,
                params![
                    session.id,
                    session.name,
                    session.profile,
                    session.group_name,
                    session.agent,
                    session.command,
                    session.project_path,
                    session.status.as_str(),
                    session.runtime_id,
                    bool_to_int(session.archived),
                    session.version,
                    session.created_at,
                    session.updated_at,
                    session.project_id,
                    session.workspace_id,
                    session.worktree_id,
                    session.parent_session_id,
                ],
            )?;
            Self::get_session_in(tx, &session.id)
        })
    }

    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<SessionRecord>> {
        let conn = self.connect()?;
        if include_archived {
            self.query_sessions(
                &conn,
                "SELECT * FROM sessions ORDER BY updated_at DESC, created_at DESC",
                [],
            )
        } else {
            self.query_sessions(
                &conn,
                "SELECT * FROM sessions WHERE archived = 0 ORDER BY updated_at DESC, created_at DESC",
                [],
            )
        }
    }

    pub fn get_session(&self, id: &str) -> Result<SessionRecord> {
        let conn = self.connect()?;
        Self::get_session_in(&conn, id)
    }

    pub fn list_groups(&self, profile: &str) -> Result<Vec<GroupRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT *
            FROM groups
            WHERE profile = ?1
            ORDER BY display_order ASC, name ASC
            "#,
        )?;
        let rows = stmt.query_map(params![profile], Self::read_group)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn create_group(
        &self,
        profile: &str,
        name: &str,
        default_project_path: &str,
    ) -> Result<GroupRecord> {
        self.with_transaction(|tx| {
            let now = now_ts();
            Self::ensure_group_in(tx, profile, name, default_project_path, now)?;
            Self::get_group_in(tx, profile, name)
        })
    }

    pub fn update_group(
        &self,
        profile: &str,
        name: &str,
        default_project_path: Option<&str>,
        collapsed: Option<bool>,
    ) -> Result<GroupRecord> {
        self.with_transaction(|tx| {
            let group = Self::get_group_in(tx, profile, name)?;
            let changed = tx.execute(
                r#"
                UPDATE groups
                SET default_project_path = ?3, collapsed = ?4, version = version + 1, updated_at = ?5
                WHERE profile = ?1 AND name = ?2 AND version = ?6
                "#,
                params![
                    profile,
                    name,
                    default_project_path.unwrap_or(&group.default_project_path),
                    bool_to_int(collapsed.unwrap_or(group.collapsed)),
                    now_ts(),
                    group.version,
                ],
            )?;
            ensure_changed(changed, "group version conflict", name)?;
            Self::get_group_in(tx, profile, name)
        })
    }

    pub fn delete_group(
        &self,
        profile: &str,
        name: &str,
        force: bool,
        replacement_group: &str,
    ) -> Result<()> {
        self.with_transaction(|tx| {
            Self::get_group_in(tx, profile, name)?;
            let session_count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM sessions WHERE profile = ?1 AND group_name = ?2",
                params![profile, name],
                |row| row.get(0),
            )?;
            if session_count > 0 && !force {
                return Err(AppError::msg(format!(
                    "group has {session_count} sessions; use --force to move them"
                )));
            }
            if session_count > 0 {
                if replacement_group == name {
                    return Err(AppError::msg("cannot delete group with no replacement"));
                }
                Self::ensure_group_in(tx, profile, replacement_group, "", now_ts())?;
                tx.execute(
                    r#"
                    UPDATE sessions
                    SET group_name = ?3, version = version + 1, updated_at = ?4
                    WHERE profile = ?1 AND group_name = ?2
                    "#,
                    params![profile, name, replacement_group, now_ts()],
                )?;
            }
            let changed = tx.execute(
                "DELETE FROM groups WHERE profile = ?1 AND name = ?2",
                params![profile, name],
            )?;
            ensure_changed(changed, "group not found", name)
        })
    }

    pub fn update_session_group(
        &self,
        id: &str,
        expected_version: i64,
        group_name: &str,
    ) -> Result<SessionRecord> {
        self.with_transaction(|tx| {
            let current = Self::get_session_in(tx, id)?;
            let exists = tx
                .query_row(
                    "SELECT 1 FROM groups WHERE profile = ?1 AND name = ?2",
                    params![current.profile, group_name],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(AppError::msg(format!("group not found: {group_name}")));
            }
            let changed = tx.execute(
                r#"
                UPDATE sessions
                SET group_name = ?3, version = version + 1, updated_at = ?4
                WHERE id = ?1 AND version = ?2
                "#,
                params![id, expected_version, group_name, now_ts()],
            )?;
            ensure_changed(changed, "session version conflict", id)?;
            Self::get_session_in(tx, id)
        })
    }

    pub fn update_runtime(
        &self,
        id: &str,
        expected_version: i64,
        runtime_id: Option<String>,
        status: SessionStatus,
    ) -> Result<SessionRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE sessions
                SET runtime_id = ?3, status = ?4, version = version + 1, updated_at = ?5
                WHERE id = ?1 AND version = ?2
                "#,
                params![id, expected_version, runtime_id, status.as_str(), now_ts()],
            )?;
            ensure_changed(changed, "session version conflict", id)?;
            Self::get_session_in(tx, id)
        })
    }

    pub fn update_status(
        &self,
        id: &str,
        expected_version: i64,
        status: SessionStatus,
    ) -> Result<SessionRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE sessions
                SET status = ?3, version = version + 1, updated_at = ?4
                WHERE id = ?1 AND version = ?2
                "#,
                params![id, expected_version, status.as_str(), now_ts()],
            )?;
            ensure_changed(changed, "session version conflict", id)?;
            Self::get_session_in(tx, id)
        })
    }

    pub fn append_session_event(
        &self,
        session_id: &str,
        kind: &str,
        payload: Value,
    ) -> Result<SessionEvent> {
        self.with_transaction(|tx| {
            let created_at = now_ts();
            let payload = serde_json::to_string(&redact_json_value(payload))?;
            tx.execute(
                r#"
                INSERT INTO session_events (session_id, kind, payload, created_at)
                VALUES (?1, ?2, ?3, ?4)
                "#,
                params![session_id, kind, payload, created_at],
            )?;
            Self::get_session_event_in(tx, tx.last_insert_rowid())
        })
    }

    pub fn append_cost_event(
        &self,
        session_id: &str,
        amount_usd: f64,
        payload: Value,
    ) -> Result<CostEvent> {
        self.with_transaction(|tx| {
            let created_at = now_ts();
            let payload = serde_json::to_string(&redact_json_value(payload))?;
            tx.execute(
                r#"
                INSERT INTO cost_events (session_id, amount_usd, payload, created_at)
                VALUES (?1, ?2, ?3, ?4)
                "#,
                params![session_id, amount_usd, payload, created_at],
            )?;
            Self::get_cost_event_in(tx, tx.last_insert_rowid())
        })
    }

    pub fn session_events(
        &self,
        session_id: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SessionEvent>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, kind, payload, created_at
            FROM session_events
            WHERE session_id = ?1 AND id > ?2
            ORDER BY id ASC
            LIMIT ?3
            "#,
        )?;
        let rows = stmt.query_map(params![session_id, since, limit as i64], Self::read_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn latest_session_events(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionEvent>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, kind, payload, created_at
            FROM session_events
            WHERE session_id = ?1
            ORDER BY id DESC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], Self::read_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn latest_session_status_signal_events(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionEvent>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, kind, payload, created_at
            FROM session_events
            WHERE session_id = ?1 AND kind IN ('agent_state', 'input')
            ORDER BY id DESC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], Self::read_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn latest_session_agent_state_events(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionEvent>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, session_id, kind, payload, created_at
            FROM session_events
            WHERE session_id = ?1 AND kind = 'agent_state'
            ORDER BY id DESC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], Self::read_event)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn remove_session(&self, id: &str) -> Result<()> {
        self.with_transaction(|tx| {
            let related_rows: i64 = tx.query_row(
                r#"
                SELECT
                    (SELECT COUNT(*) FROM session_events WHERE session_id = ?1) +
                    (SELECT COUNT(*) FROM cost_events WHERE session_id = ?1)
                "#,
                params![id],
                |row| row.get(0),
            )?;

            let changed = if related_rows == 0 {
                tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?
            } else {
                let changed = tx.execute(
                    "UPDATE sessions SET archived = 1, status = ?3, runtime_id = NULL, version = version + 1, updated_at = ?2 WHERE id = ?1",
                    params![id, now_ts(), SessionStatus::Stopped.as_str()],
                )?;
                if changed > 0 {
                    tx.execute(
                        "INSERT INTO session_events (session_id, kind, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            id,
                            "archive",
                            serde_json::to_string(&json!({ "source": "remove_session" }))?,
                            now_ts()
                        ],
                    )?;
                }
                changed
            };

            ensure_changed(changed, "session not found", id)
        })
    }

    pub fn archive_session(&self, id: &str) -> Result<SessionRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE sessions
                SET archived = 1, version = version + 1, updated_at = ?2
                WHERE id = ?1
                "#,
                params![id, now_ts()],
            )?;
            ensure_changed(changed, "session not found", id)?;
            Self::get_session_in(tx, id)
        })
    }

    pub fn restore_session(&self, id: &str) -> Result<SessionRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE sessions
                SET archived = 0, version = version + 1, updated_at = ?2
                WHERE id = ?1
                "#,
                params![id, now_ts()],
            )?;
            ensure_changed(changed, "session not found", id)?;
            Self::get_session_in(tx, id)
        })
    }

    pub fn purge_session(&self, id: &str) -> Result<()> {
        self.with_transaction(|tx| {
            let changed = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            ensure_changed(changed, "session not found", id)
        })
    }

    pub fn create_project(&self, project: &ProjectRecord) -> Result<ProjectRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO projects (
                    id, profile, root_path, repo_identity, default_branch, trust_state,
                    hooks_hash, config_hash, version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                "#,
                params![
                    project.id,
                    project.profile,
                    project.root_path,
                    project.repo_identity,
                    project.default_branch,
                    project.trust_state.as_str(),
                    project.hooks_hash,
                    project.config_hash,
                    project.version,
                    project.created_at,
                    project.updated_at,
                ],
            )?;
            Self::get_project_in(tx, &project.id)
        })
    }

    pub fn get_project(&self, id: &str) -> Result<ProjectRecord> {
        let conn = self.connect()?;
        Self::get_project_in(&conn, id)
    }

    pub fn list_projects(&self, profile: &str) -> Result<Vec<ProjectRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM projects WHERE profile = ?1 ORDER BY updated_at DESC, root_path ASC",
        )?;
        let rows = stmt.query_map(params![profile], Self::read_project)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_project(
        &self,
        project: &ProjectRecord,
        expected_version: i64,
    ) -> Result<ProjectRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE projects
                SET profile = ?1, root_path = ?2, repo_identity = ?3, default_branch = ?4,
                    trust_state = ?5, hooks_hash = ?6, config_hash = ?7,
                    version = version + 1, updated_at = ?8
                WHERE id = ?9 AND version = ?10
                "#,
                params![
                    project.profile,
                    project.root_path,
                    project.repo_identity,
                    project.default_branch,
                    project.trust_state.as_str(),
                    project.hooks_hash,
                    project.config_hash,
                    project.updated_at,
                    project.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "project version conflict", &project.id)?;
            Self::get_project_in(tx, &project.id)
        })
    }

    pub fn delete_project(&self, id: &str) -> Result<()> {
        self.delete_by_id("projects", "project", id)
    }

    pub fn create_workspace(&self, workspace: &WorkspaceRecord) -> Result<WorkspaceRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO workspaces (
                    id, project_id, path, worktree_id, sandbox_id, multi_repo_roots,
                    cleanup_policy, version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                "#,
                params![
                    workspace.id,
                    workspace.project_id,
                    workspace.path,
                    workspace.worktree_id,
                    workspace.sandbox_id,
                    workspace.multi_repo_roots,
                    workspace.cleanup_policy,
                    workspace.version,
                    workspace.created_at,
                    workspace.updated_at,
                ],
            )?;
            Self::get_workspace_in(tx, &workspace.id)
        })
    }

    pub fn get_workspace(&self, id: &str) -> Result<WorkspaceRecord> {
        let conn = self.connect()?;
        Self::get_workspace_in(&conn, id)
    }

    pub fn list_workspaces(&self, project_id: &str) -> Result<Vec<WorkspaceRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM workspaces WHERE project_id = ?1 ORDER BY updated_at DESC, path ASC",
        )?;
        let rows = stmt.query_map(params![project_id], Self::read_workspace)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_workspace(
        &self,
        workspace: &WorkspaceRecord,
        expected_version: i64,
    ) -> Result<WorkspaceRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE workspaces
                SET project_id = ?1, path = ?2, worktree_id = ?3, sandbox_id = ?4,
                    multi_repo_roots = ?5, cleanup_policy = ?6,
                    version = version + 1, updated_at = ?7
                WHERE id = ?8 AND version = ?9
                "#,
                params![
                    workspace.project_id,
                    workspace.path,
                    workspace.worktree_id,
                    workspace.sandbox_id,
                    workspace.multi_repo_roots,
                    workspace.cleanup_policy,
                    workspace.updated_at,
                    workspace.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "workspace version conflict", &workspace.id)?;
            Self::get_workspace_in(tx, &workspace.id)
        })
    }

    pub fn delete_workspace(&self, id: &str) -> Result<()> {
        self.delete_by_id("workspaces", "workspace", id)
    }

    pub fn create_worktree(&self, worktree: &WorktreeRecord) -> Result<WorktreeRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO worktrees (
                    id, project_id, path, branch, base_branch, status, cleanup_allowed,
                    version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                "#,
                params![
                    worktree.id,
                    worktree.project_id,
                    worktree.path,
                    worktree.branch,
                    worktree.base_branch,
                    worktree.status.as_str(),
                    bool_to_int(worktree.cleanup_allowed),
                    worktree.version,
                    worktree.created_at,
                    worktree.updated_at,
                ],
            )?;
            Self::get_worktree_in(tx, &worktree.id)
        })
    }

    pub fn get_worktree(&self, id: &str) -> Result<WorktreeRecord> {
        let conn = self.connect()?;
        Self::get_worktree_in(&conn, id)
    }

    pub fn list_worktrees(&self, project_id: &str) -> Result<Vec<WorktreeRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM worktrees WHERE project_id = ?1 ORDER BY updated_at DESC, path ASC",
        )?;
        let rows = stmt.query_map(params![project_id], Self::read_worktree)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_worktree(
        &self,
        worktree: &WorktreeRecord,
        expected_version: i64,
    ) -> Result<WorktreeRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE worktrees
                SET project_id = ?1, path = ?2, branch = ?3, base_branch = ?4,
                    status = ?5, cleanup_allowed = ?6,
                    version = version + 1, updated_at = ?7
                WHERE id = ?8 AND version = ?9
                "#,
                params![
                    worktree.project_id,
                    worktree.path,
                    worktree.branch,
                    worktree.base_branch,
                    worktree.status.as_str(),
                    bool_to_int(worktree.cleanup_allowed),
                    worktree.updated_at,
                    worktree.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "worktree version conflict", &worktree.id)?;
            Self::get_worktree_in(tx, &worktree.id)
        })
    }

    pub fn delete_worktree(&self, id: &str) -> Result<()> {
        self.delete_by_id("worktrees", "worktree", id)
    }

    pub fn create_mcp_attachment(
        &self,
        attachment: &McpAttachmentRecord,
    ) -> Result<McpAttachmentRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO mcp_attachments (
                    id, profile, scope, project_id, session_id, server_id, status,
                    materialized_state, restart_required, version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                "#,
                params![
                    attachment.id,
                    attachment.profile,
                    attachment.scope,
                    attachment.project_id,
                    attachment.session_id,
                    attachment.server_id,
                    attachment.status.as_str(),
                    attachment.materialized_state,
                    bool_to_int(attachment.restart_required),
                    attachment.version,
                    attachment.created_at,
                    attachment.updated_at,
                ],
            )?;
            Self::get_mcp_attachment_in(tx, &attachment.id)
        })
    }

    pub fn get_mcp_attachment(&self, id: &str) -> Result<McpAttachmentRecord> {
        let conn = self.connect()?;
        Self::get_mcp_attachment_in(&conn, id)
    }

    pub fn list_mcp_attachments(
        &self,
        profile: &str,
        scope: Option<&str>,
        project_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<McpAttachmentRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT * FROM mcp_attachments
            WHERE profile = ?1
              AND (?2 IS NULL OR scope = ?2)
              AND (?3 IS NULL OR project_id = ?3)
              AND (?4 IS NULL OR session_id = ?4)
            ORDER BY updated_at DESC, server_id ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![profile, scope, project_id, session_id],
            Self::read_mcp_attachment,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_mcp_attachment(
        &self,
        attachment: &McpAttachmentRecord,
        expected_version: i64,
    ) -> Result<McpAttachmentRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE mcp_attachments
                SET profile = ?1, scope = ?2, project_id = ?3, session_id = ?4,
                    server_id = ?5, status = ?6, materialized_state = ?7,
                    restart_required = ?8, version = version + 1, updated_at = ?9
                WHERE id = ?10 AND version = ?11
                "#,
                params![
                    attachment.profile,
                    attachment.scope,
                    attachment.project_id,
                    attachment.session_id,
                    attachment.server_id,
                    attachment.status.as_str(),
                    attachment.materialized_state,
                    bool_to_int(attachment.restart_required),
                    attachment.updated_at,
                    attachment.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "mcp attachment version conflict", &attachment.id)?;
            Self::get_mcp_attachment_in(tx, &attachment.id)
        })
    }

    pub fn delete_mcp_attachment(&self, id: &str) -> Result<()> {
        self.delete_by_id("mcp_attachments", "mcp attachment", id)
    }

    pub fn create_skill_attachment(
        &self,
        attachment: &SkillAttachmentRecord,
    ) -> Result<SkillAttachmentRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO skill_attachments (
                    id, profile, scope, project_id, session_id, skill_id, pool_path,
                    materialized_path, status, restart_required, version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                "#,
                params![
                    attachment.id,
                    attachment.profile,
                    attachment.scope,
                    attachment.project_id,
                    attachment.session_id,
                    attachment.skill_id,
                    attachment.pool_path,
                    attachment.materialized_path,
                    attachment.status.as_str(),
                    bool_to_int(attachment.restart_required),
                    attachment.version,
                    attachment.created_at,
                    attachment.updated_at,
                ],
            )?;
            Self::get_skill_attachment_in(tx, &attachment.id)
        })
    }

    pub fn get_skill_attachment(&self, id: &str) -> Result<SkillAttachmentRecord> {
        let conn = self.connect()?;
        Self::get_skill_attachment_in(&conn, id)
    }

    pub fn list_skill_attachments(
        &self,
        profile: &str,
        scope: Option<&str>,
        project_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<SkillAttachmentRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT * FROM skill_attachments
            WHERE profile = ?1
              AND (?2 IS NULL OR scope = ?2)
              AND (?3 IS NULL OR project_id = ?3)
              AND (?4 IS NULL OR session_id = ?4)
            ORDER BY updated_at DESC, skill_id ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![profile, scope, project_id, session_id],
            Self::read_skill_attachment,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_skill_attachment(
        &self,
        attachment: &SkillAttachmentRecord,
        expected_version: i64,
    ) -> Result<SkillAttachmentRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE skill_attachments
                SET profile = ?1, scope = ?2, project_id = ?3, session_id = ?4,
                    skill_id = ?5, pool_path = ?6, materialized_path = ?7,
                    status = ?8, restart_required = ?9,
                    version = version + 1, updated_at = ?10
                WHERE id = ?11 AND version = ?12
                "#,
                params![
                    attachment.profile,
                    attachment.scope,
                    attachment.project_id,
                    attachment.session_id,
                    attachment.skill_id,
                    attachment.pool_path,
                    attachment.materialized_path,
                    attachment.status.as_str(),
                    bool_to_int(attachment.restart_required),
                    attachment.updated_at,
                    attachment.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "skill attachment version conflict", &attachment.id)?;
            Self::get_skill_attachment_in(tx, &attachment.id)
        })
    }

    pub fn delete_skill_attachment(&self, id: &str) -> Result<()> {
        self.delete_by_id("skill_attachments", "skill attachment", id)
    }

    pub fn create_watcher(&self, watcher: &WatcherRecord) -> Result<WatcherRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO watchers (
                    id, profile, project_id, adapter_id, name, config_ref, status,
                    last_event_at, version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                "#,
                params![
                    watcher.id,
                    watcher.profile,
                    watcher.project_id,
                    watcher.adapter_id,
                    watcher.name,
                    watcher.config_ref,
                    watcher.status.as_str(),
                    watcher.last_event_at,
                    watcher.version,
                    watcher.created_at,
                    watcher.updated_at,
                ],
            )?;
            Self::get_watcher_in(tx, &watcher.id)
        })
    }

    pub fn get_watcher(&self, id: &str) -> Result<WatcherRecord> {
        let conn = self.connect()?;
        Self::get_watcher_in(&conn, id)
    }

    pub fn list_watchers(
        &self,
        profile: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<WatcherRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT * FROM watchers
            WHERE profile = ?1 AND (?2 IS NULL OR project_id = ?2)
            ORDER BY updated_at DESC, name ASC
            "#,
        )?;
        let rows = stmt.query_map(params![profile, project_id], Self::read_watcher)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_watcher(
        &self,
        watcher: &WatcherRecord,
        expected_version: i64,
    ) -> Result<WatcherRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE watchers
                SET profile = ?1, project_id = ?2, adapter_id = ?3, name = ?4,
                    config_ref = ?5, status = ?6, last_event_at = ?7,
                    version = version + 1, updated_at = ?8
                WHERE id = ?9 AND version = ?10
                "#,
                params![
                    watcher.profile,
                    watcher.project_id,
                    watcher.adapter_id,
                    watcher.name,
                    watcher.config_ref,
                    watcher.status.as_str(),
                    watcher.last_event_at,
                    watcher.updated_at,
                    watcher.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "watcher version conflict", &watcher.id)?;
            Self::get_watcher_in(tx, &watcher.id)
        })
    }

    pub fn delete_watcher(&self, id: &str) -> Result<()> {
        self.delete_by_id("watchers", "watcher", id)
    }

    pub fn append_watcher_event(&self, event: &WatcherEventRecord) -> Result<WatcherEventRecord> {
        self.with_transaction(|tx| {
            let payload_ref = redact_event_payload(&event.payload_ref);
            tx.execute(
                r#"
                INSERT INTO watcher_events (
                    id, watcher_id, source, event_type, payload_ref, signature_status,
                    route_decision, delivered, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    event.id,
                    event.watcher_id,
                    event.source,
                    event.event_type,
                    payload_ref,
                    event.signature_status,
                    event.route_decision,
                    bool_to_int(event.delivered),
                    event.created_at,
                ],
            )?;
            tx.execute(
                "UPDATE watchers SET last_event_at = MAX(last_event_at, ?2), updated_at = MAX(updated_at, ?2) WHERE id = ?1",
                params![event.watcher_id, event.created_at],
            )?;
            Self::get_watcher_event_in(tx, &event.id)
        })
    }

    pub fn list_watcher_events(
        &self,
        watcher_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<WatcherEventRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT * FROM watcher_events
            WHERE watcher_id = ?1
            ORDER BY created_at ASC, id ASC
            LIMIT ?2 OFFSET ?3
            "#,
        )?;
        let rows = stmt.query_map(
            params![watcher_id, limit as i64, offset as i64],
            Self::read_watcher_event,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn create_conductor(&self, conductor: &ConductorRecord) -> Result<ConductorRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO conductors (
                    id, profile, session_id, status, watched_sessions, channel_bindings,
                    last_heartbeat_at, version, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                "#,
                params![
                    conductor.id,
                    conductor.profile,
                    conductor.session_id,
                    conductor.status.as_str(),
                    conductor.watched_sessions,
                    conductor.channel_bindings,
                    conductor.last_heartbeat_at,
                    conductor.version,
                    conductor.created_at,
                    conductor.updated_at,
                ],
            )?;
            Self::get_conductor_in(tx, &conductor.id)
        })
    }

    pub fn get_conductor(&self, id: &str) -> Result<ConductorRecord> {
        let conn = self.connect()?;
        Self::get_conductor_in(&conn, id)
    }

    pub fn list_conductors(&self, profile: &str) -> Result<Vec<ConductorRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM conductors WHERE profile = ?1 ORDER BY updated_at DESC, id ASC",
        )?;
        let rows = stmt.query_map(params![profile], Self::read_conductor)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn update_conductor(
        &self,
        conductor: &ConductorRecord,
        expected_version: i64,
    ) -> Result<ConductorRecord> {
        self.with_transaction(|tx| {
            let changed = tx.execute(
                r#"
                UPDATE conductors
                SET profile = ?1, session_id = ?2, status = ?3, watched_sessions = ?4,
                    channel_bindings = ?5, last_heartbeat_at = ?6,
                    version = version + 1, updated_at = ?7
                WHERE id = ?8 AND version = ?9
                "#,
                params![
                    conductor.profile,
                    conductor.session_id,
                    conductor.status.as_str(),
                    conductor.watched_sessions,
                    conductor.channel_bindings,
                    conductor.last_heartbeat_at,
                    conductor.updated_at,
                    conductor.id,
                    expected_version,
                ],
            )?;
            ensure_changed(changed, "conductor version conflict", &conductor.id)?;
            Self::get_conductor_in(tx, &conductor.id)
        })
    }

    pub fn delete_conductor(&self, id: &str) -> Result<()> {
        self.delete_by_id("conductors", "conductor", id)
    }

    pub fn append_conductor_assignment(
        &self,
        assignment: &ConductorAssignmentRecord,
    ) -> Result<ConductorAssignmentRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                INSERT INTO conductor_assignments (
                    id, conductor_id, session_id, task_ref, status, assigned_at, completed_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    assignment.id,
                    assignment.conductor_id,
                    assignment.session_id,
                    assignment.task_ref,
                    assignment.status,
                    assignment.assigned_at,
                    assignment.completed_at,
                ],
            )?;
            Self::get_conductor_assignment_in(tx, &assignment.id)
        })
    }

    pub fn get_conductor_assignment(&self, id: &str) -> Result<ConductorAssignmentRecord> {
        let conn = self.connect()?;
        Self::get_conductor_assignment_in(&conn, id)
    }

    pub fn update_conductor_assignment(
        &self,
        assignment: &ConductorAssignmentRecord,
    ) -> Result<ConductorAssignmentRecord> {
        self.with_transaction(|tx| {
            tx.execute(
                r#"
                UPDATE conductor_assignments
                SET status = ?2,
                    completed_at = ?3
                WHERE id = ?1
                "#,
                params![assignment.id, assignment.status, assignment.completed_at],
            )?;
            Self::get_conductor_assignment_in(tx, &assignment.id)
        })
    }

    pub fn list_conductor_assignments(
        &self,
        conductor_id: &str,
    ) -> Result<Vec<ConductorAssignmentRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM conductor_assignments WHERE conductor_id = ?1 ORDER BY assigned_at ASC",
        )?;
        let rows = stmt.query_map(params![conductor_id], Self::read_conductor_assignment)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn open_conductor_assignments_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ConductorAssignmentRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT * FROM conductor_assignments WHERE session_id = ?1 AND completed_at = 0 ORDER BY assigned_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], Self::read_conductor_assignment)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn cost_summary(&self, filter: &CostFilter) -> Result<CostSummary> {
        let conn = self.connect()?;
        let project_id = if let Some(project_id) = filter.project_id.as_deref() {
            let exists = conn
                .query_row(
                    "SELECT 1 FROM projects WHERE id = ?1 AND profile = ?2",
                    params![project_id, filter.profile],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if exists.is_none() {
                return Ok(empty_cost_summary(filter));
            }
            Some(project_id.to_string())
        } else {
            None
        };

        let mut stmt = conn.prepare(
            r#"
                SELECT ce.session_id, ce.amount_usd, ce.payload, s.project_id
                FROM cost_events ce
                INNER JOIN sessions s ON s.id = ce.session_id
            WHERE s.profile = ?1
              AND ce.created_at >= ?2
              AND ce.created_at <= ?3
              AND (?4 IS NULL OR s.group_name = ?4)
              AND (?5 IS NULL OR ce.session_id = ?5)
              AND (?6 IS NULL OR s.agent = ?6)
              AND (?7 = 1 OR s.archived = 0)
            ORDER BY ce.created_at ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![
                filter.profile,
                filter.start_at,
                filter.end_at,
                filter.group_name.as_deref(),
                filter.session_id.as_deref(),
                filter.agent.as_deref(),
                bool_to_int(filter.include_archived),
            ],
            |row| {
                let payload: String = row.get(2)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    parse_payload(payload, 2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;

        let mut summary = empty_cost_summary(filter);
        let mut sessions = HashSet::new();
        for row in rows {
            let (session_id, amount_usd, payload, row_project_id) = row?;
            if let Some(project_id) = project_id.as_deref()
                && row_project_id != project_id
            {
                continue;
            }
            if let Some(model) = filter.model.as_deref()
                && payload.get("model").and_then(Value::as_str) != Some(model)
            {
                continue;
            }

            summary.event_count += 1;
            summary.total_cost_micros += (amount_usd * 1_000_000.0).round() as i64;
            summary.input_tokens += payload_i64(&payload, "input_tokens");
            summary.output_tokens += payload_i64(&payload, "output_tokens");
            let total_tokens = payload_i64(&payload, "total_tokens");
            summary.total_tokens += if total_tokens == 0 {
                payload_i64(&payload, "input_tokens") + payload_i64(&payload, "output_tokens")
            } else {
                total_tokens
            };
            sessions.insert(session_id);
        }
        summary.session_count = sessions.len() as i64;
        Ok(summary)
    }

    pub fn cost_events(
        &self,
        filter: &CostFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CostEvent>> {
        let limit = limit.min(500);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.connect()?;
        let project_id = if let Some(project_id) = filter.project_id.as_deref() {
            let exists = conn
                .query_row(
                    "SELECT 1 FROM projects WHERE id = ?1 AND profile = ?2",
                    params![project_id, filter.profile],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if exists.is_none() {
                return Ok(Vec::new());
            }
            Some(project_id.to_string())
        } else {
            None
        };

        let mut stmt = conn.prepare(
            r#"
            SELECT ce.id, ce.session_id, ce.amount_usd, ce.payload, ce.created_at, s.project_id
            FROM cost_events ce
            INNER JOIN sessions s ON s.id = ce.session_id
            WHERE s.profile = ?1
              AND ce.created_at >= ?2
              AND ce.created_at <= ?3
              AND (?4 IS NULL OR s.group_name = ?4)
              AND (?5 IS NULL OR ce.session_id = ?5)
              AND (?6 IS NULL OR s.agent = ?6)
              AND (?7 = 1 OR s.archived = 0)
            ORDER BY ce.created_at DESC, ce.id DESC
            "#,
        )?;
        let rows = stmt.query_map(
            params![
                filter.profile,
                filter.start_at,
                filter.end_at,
                filter.group_name.as_deref(),
                filter.session_id.as_deref(),
                filter.agent.as_deref(),
                bool_to_int(filter.include_archived),
            ],
            |row| {
                let payload: String = row.get("payload")?;
                Ok((
                    CostEvent {
                        id: row.get("id")?,
                        session_id: row.get("session_id")?,
                        amount_usd: row.get("amount_usd")?,
                        payload: parse_payload(payload, 3)?,
                        created_at: row.get("created_at")?,
                    },
                    row.get::<_, String>("project_id")?,
                ))
            },
        )?;

        let mut skipped = 0;
        let mut events = Vec::new();
        for row in rows {
            let (event, row_project_id) = row?;
            if let Some(project_id) = project_id.as_deref()
                && row_project_id != project_id
            {
                continue;
            }
            if let Some(model) = filter.model.as_deref()
                && event.payload.get("model").and_then(Value::as_str) != Some(model)
            {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            events.push(event);
            if events.len() >= limit {
                break;
            }
        }
        Ok(events)
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    fn ensure_parent_dir(&self) -> Result<()> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn lock_profile(&self) -> Result<File> {
        self.ensure_parent_dir()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(&self.db_path))?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn with_transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _lock = self.lock_profile()?;
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    fn delete_by_id(&self, table: &str, kind: &str, id: &str) -> Result<()> {
        self.with_transaction(|tx| {
            let changed = tx.execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])?;
            ensure_changed(changed, &format!("{kind} not found"), id)
        })
    }

    fn query_sessions<P>(
        &self,
        conn: &Connection,
        sql: &str,
        params: P,
    ) -> Result<Vec<SessionRecord>>
    where
        P: rusqlite::Params,
    {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, Self::read_session)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn get_session_in(conn: &Connection, id: &str) -> Result<SessionRecord> {
        conn.query_row(
            "SELECT * FROM sessions WHERE id = ?1",
            params![id],
            Self::read_session,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("session not found: {id}")))
    }

    fn get_project_in(conn: &Connection, id: &str) -> Result<ProjectRecord> {
        conn.query_row(
            "SELECT * FROM projects WHERE id = ?1",
            params![id],
            Self::read_project,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("project not found: {id}")))
    }

    fn get_workspace_in(conn: &Connection, id: &str) -> Result<WorkspaceRecord> {
        conn.query_row(
            "SELECT * FROM workspaces WHERE id = ?1",
            params![id],
            Self::read_workspace,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("workspace not found: {id}")))
    }

    fn get_worktree_in(conn: &Connection, id: &str) -> Result<WorktreeRecord> {
        conn.query_row(
            "SELECT * FROM worktrees WHERE id = ?1",
            params![id],
            Self::read_worktree,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("worktree not found: {id}")))
    }

    fn get_mcp_attachment_in(conn: &Connection, id: &str) -> Result<McpAttachmentRecord> {
        conn.query_row(
            "SELECT * FROM mcp_attachments WHERE id = ?1",
            params![id],
            Self::read_mcp_attachment,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("mcp attachment not found: {id}")))
    }

    fn get_skill_attachment_in(conn: &Connection, id: &str) -> Result<SkillAttachmentRecord> {
        conn.query_row(
            "SELECT * FROM skill_attachments WHERE id = ?1",
            params![id],
            Self::read_skill_attachment,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("skill attachment not found: {id}")))
    }

    fn get_watcher_in(conn: &Connection, id: &str) -> Result<WatcherRecord> {
        conn.query_row(
            "SELECT * FROM watchers WHERE id = ?1",
            params![id],
            Self::read_watcher,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("watcher not found: {id}")))
    }

    fn get_watcher_event_in(conn: &Connection, id: &str) -> Result<WatcherEventRecord> {
        conn.query_row(
            "SELECT * FROM watcher_events WHERE id = ?1",
            params![id],
            Self::read_watcher_event,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("watcher event not found: {id}")))
    }

    fn get_conductor_in(conn: &Connection, id: &str) -> Result<ConductorRecord> {
        conn.query_row(
            "SELECT * FROM conductors WHERE id = ?1",
            params![id],
            Self::read_conductor,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("conductor not found: {id}")))
    }

    fn get_conductor_assignment_in(
        conn: &Connection,
        id: &str,
    ) -> Result<ConductorAssignmentRecord> {
        conn.query_row(
            "SELECT * FROM conductor_assignments WHERE id = ?1",
            params![id],
            Self::read_conductor_assignment,
        )
        .optional()?
        .ok_or_else(|| AppError::msg(format!("conductor assignment not found: {id}")))
    }

    fn get_session_event_in(conn: &Connection, id: i64) -> Result<SessionEvent> {
        conn.query_row(
            "SELECT id, session_id, kind, payload, created_at FROM session_events WHERE id = ?1",
            params![id],
            Self::read_event,
        )
        .map_err(Into::into)
    }

    fn get_cost_event_in(conn: &Connection, id: i64) -> Result<CostEvent> {
        conn.query_row(
            "SELECT id, session_id, amount_usd, payload, created_at FROM cost_events WHERE id = ?1",
            params![id],
            Self::read_cost,
        )
        .map_err(Into::into)
    }

    fn ensure_group_in(
        conn: &Connection,
        profile: &str,
        name: &str,
        default_project_path: &str,
        now: i64,
    ) -> Result<()> {
        conn.execute(
            r#"
            INSERT OR IGNORE INTO groups (
                id, profile, name, default_project_path, collapsed, display_order,
                metadata, version, created_at, updated_at
            )
            VALUES (
                lower(hex(randomblob(16))), ?1, ?2, ?3, 0,
                (SELECT COALESCE(MAX(display_order) + 1, 0) FROM groups WHERE profile = ?1),
                ?5, 0, ?4, ?4
            )
            "#,
            params![
                profile,
                name,
                default_project_path,
                now,
                group_metadata(name)
            ],
        )?;
        Ok(())
    }

    fn get_group_in(conn: &Connection, profile: &str, name: &str) -> Result<GroupRecord> {
        conn.query_row(
            "SELECT * FROM groups WHERE profile = ?1 AND name = ?2",
            params![profile, name],
            Self::read_group,
        )
        .map_err(Into::into)
    }

    fn read_session(row: &Row<'_>) -> rusqlite::Result<SessionRecord> {
        let status: String = row.get("status")?;
        let archived: i64 = row.get("archived")?;
        Ok(SessionRecord {
            id: row.get("id")?,
            name: row.get("name")?,
            profile: row.get("profile")?,
            group_name: row.get("group_name")?,
            project_id: row.get("project_id")?,
            workspace_id: row.get("workspace_id")?,
            worktree_id: row.get("worktree_id")?,
            parent_session_id: row.get("parent_session_id")?,
            agent: row.get("agent")?,
            command: row.get("command")?,
            project_path: row.get("project_path")?,
            status: SessionStatus::from_db(&status),
            runtime_id: row.get("runtime_id")?,
            archived: archived != 0,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_group(row: &Row<'_>) -> rusqlite::Result<GroupRecord> {
        let collapsed: i64 = row.get("collapsed")?;
        let metadata: String = row.get("metadata")?;
        Ok(GroupRecord {
            id: row.get("id")?,
            profile: row.get("profile")?,
            name: row.get("name")?,
            default_project_path: row.get("default_project_path")?,
            collapsed: collapsed != 0,
            display_order: row.get("display_order")?,
            metadata: if metadata.trim().is_empty() || metadata.trim() == "{}" {
                group_metadata(&row.get::<_, String>("name")?)
            } else {
                metadata
            },
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_event(row: &Row<'_>) -> rusqlite::Result<SessionEvent> {
        let payload: String = row.get("payload")?;
        Ok(SessionEvent {
            id: row.get("id")?,
            session_id: row.get("session_id")?,
            kind: row.get("kind")?,
            payload: parse_payload(payload, 3)?,
            created_at: row.get("created_at")?,
        })
    }

    fn read_cost(row: &Row<'_>) -> rusqlite::Result<CostEvent> {
        let payload: String = row.get("payload")?;
        Ok(CostEvent {
            id: row.get("id")?,
            session_id: row.get("session_id")?,
            amount_usd: row.get("amount_usd")?,
            payload: parse_payload(payload, 3)?,
            created_at: row.get("created_at")?,
        })
    }

    fn read_project(row: &Row<'_>) -> rusqlite::Result<ProjectRecord> {
        let trust_state: String = row.get("trust_state")?;
        Ok(ProjectRecord {
            id: row.get("id")?,
            profile: row.get("profile")?,
            root_path: row.get("root_path")?,
            repo_identity: row.get("repo_identity")?,
            default_branch: row.get("default_branch")?,
            trust_state: ProjectTrustState::from_db(&trust_state),
            hooks_hash: row.get("hooks_hash")?,
            config_hash: row.get("config_hash")?,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_workspace(row: &Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
        Ok(WorkspaceRecord {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            path: row.get("path")?,
            worktree_id: row.get("worktree_id")?,
            sandbox_id: row.get("sandbox_id")?,
            multi_repo_roots: row.get("multi_repo_roots")?,
            cleanup_policy: row.get("cleanup_policy")?,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_worktree(row: &Row<'_>) -> rusqlite::Result<WorktreeRecord> {
        let status: String = row.get("status")?;
        let cleanup_allowed: i64 = row.get("cleanup_allowed")?;
        Ok(WorktreeRecord {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            path: row.get("path")?,
            branch: row.get("branch")?,
            base_branch: row.get("base_branch")?,
            status: WorktreeStatus::from_db(&status),
            cleanup_allowed: cleanup_allowed != 0,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_mcp_attachment(row: &Row<'_>) -> rusqlite::Result<McpAttachmentRecord> {
        let status: String = row.get("status")?;
        let restart_required: i64 = row.get("restart_required")?;
        Ok(McpAttachmentRecord {
            id: row.get("id")?,
            profile: row.get("profile")?,
            scope: row.get("scope")?,
            project_id: row.get("project_id")?,
            session_id: row.get("session_id")?,
            server_id: row.get("server_id")?,
            status: AttachmentStatus::from_db(&status),
            materialized_state: row.get("materialized_state")?,
            restart_required: restart_required != 0,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_skill_attachment(row: &Row<'_>) -> rusqlite::Result<SkillAttachmentRecord> {
        let status: String = row.get("status")?;
        let restart_required: i64 = row.get("restart_required")?;
        Ok(SkillAttachmentRecord {
            id: row.get("id")?,
            profile: row.get("profile")?,
            scope: row.get("scope")?,
            project_id: row.get("project_id")?,
            session_id: row.get("session_id")?,
            skill_id: row.get("skill_id")?,
            pool_path: row.get("pool_path")?,
            materialized_path: row.get("materialized_path")?,
            status: AttachmentStatus::from_db(&status),
            restart_required: restart_required != 0,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_watcher(row: &Row<'_>) -> rusqlite::Result<WatcherRecord> {
        let status: String = row.get("status")?;
        Ok(WatcherRecord {
            id: row.get("id")?,
            profile: row.get("profile")?,
            project_id: row.get("project_id")?,
            adapter_id: row.get("adapter_id")?,
            name: row.get("name")?,
            config_ref: row.get("config_ref")?,
            status: WatcherStatus::from_db(&status),
            last_event_at: row.get("last_event_at")?,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_watcher_event(row: &Row<'_>) -> rusqlite::Result<WatcherEventRecord> {
        let delivered: i64 = row.get("delivered")?;
        Ok(WatcherEventRecord {
            id: row.get("id")?,
            watcher_id: row.get("watcher_id")?,
            source: row.get("source")?,
            event_type: row.get("event_type")?,
            payload_ref: row.get("payload_ref")?,
            signature_status: row.get("signature_status")?,
            route_decision: row.get("route_decision")?,
            delivered: delivered != 0,
            created_at: row.get("created_at")?,
        })
    }

    fn read_conductor(row: &Row<'_>) -> rusqlite::Result<ConductorRecord> {
        let status: String = row.get("status")?;
        Ok(ConductorRecord {
            id: row.get("id")?,
            profile: row.get("profile")?,
            session_id: row.get("session_id")?,
            status: ConductorStatus::from_db(&status),
            watched_sessions: row.get("watched_sessions")?,
            channel_bindings: row.get("channel_bindings")?,
            last_heartbeat_at: row.get("last_heartbeat_at")?,
            version: row.get("version")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    fn read_conductor_assignment(row: &Row<'_>) -> rusqlite::Result<ConductorAssignmentRecord> {
        Ok(ConductorAssignmentRecord {
            id: row.get("id")?,
            conductor_id: row.get("conductor_id")?,
            session_id: row.get("session_id")?,
            task_ref: row.get("task_ref")?,
            status: row.get("status")?,
            assigned_at: row.get("assigned_at")?,
            completed_at: row.get("completed_at")?,
        })
    }
}

fn ensure_changed(changed: usize, message: &str, id: &str) -> Result<()> {
    if changed == 0 {
        Err(AppError::msg(format!("{message}: {id}")))
    } else {
        Ok(())
    }
}

fn bool_to_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn group_metadata(name: &str) -> String {
    let parent = name.rsplit_once('/').map(|(parent, _)| parent);
    let depth = name
        .split('/')
        .filter(|part| !part.is_empty())
        .count()
        .saturating_sub(1);
    json!({
        "parent": parent,
        "depth": depth,
    })
    .to_string()
}

fn lock_path(db_path: &Path) -> PathBuf {
    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.db");
    let mut path = db_path.to_path_buf();
    path.set_file_name(format!("{file_name}.lock"));
    path
}

fn parse_payload(payload: String, column: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(&payload)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(err)))
}

fn payload_i64(payload: &Value, key: &str) -> i64 {
    payload
        .get(key)
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        })
        .unwrap_or(0)
}

fn empty_cost_summary(filter: &CostFilter) -> CostSummary {
    CostSummary {
        profile: filter.profile.clone(),
        total_cost_micros: 0,
        event_count: 0,
        session_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        start_at: filter.start_at,
        end_at: filter.end_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_architecture_records_and_summarizes_costs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = SessionStore::new(tmp.path().join("state.db"));
        store.init()?;
        let now = 1_700_000_000;

        store.create_session(&SessionRecord {
            id: "session-1".into(),
            name: "demo".into(),
            profile: "default".into(),
            group_name: "group".into(),
            project_id: "project-1".into(),
            workspace_id: "workspace-1".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: "codex".into(),
            command: "codex".into(),
            project_path: "/repo".into(),
            status: SessionStatus::Running,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        assert_eq!(store.list_groups("default")?[0].name, "group");
        let missing = store
            .update_session_group("session-1", 0, "moved")
            .unwrap_err();
        assert!(missing.to_string().contains("group not found: moved"));
        store.create_group("default", "moved", "")?;
        let moved = store.update_session_group("session-1", 0, "moved")?;
        assert_eq!(moved.group_name, "moved");
        assert_eq!(
            store
                .list_groups("default")?
                .into_iter()
                .map(|group| group.name)
                .collect::<Vec<_>>(),
            vec!["group", "moved"]
        );

        let project = store.create_project(&ProjectRecord {
            id: "project-1".into(),
            profile: "default".into(),
            root_path: "/repo".into(),
            repo_identity: "repo".into(),
            default_branch: "main".into(),
            trust_state: ProjectTrustState::Trusted,
            hooks_hash: "".into(),
            config_hash: "".into(),
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        assert_eq!(store.list_projects("default")?.len(), 1);

        store.create_worktree(&WorktreeRecord {
            id: "worktree-1".into(),
            project_id: project.id.clone(),
            path: "/repo-wt".into(),
            branch: "feature".into(),
            base_branch: "main".into(),
            status: WorktreeStatus::Ready,
            cleanup_allowed: true,
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        store.create_workspace(&WorkspaceRecord {
            id: "workspace-1".into(),
            project_id: project.id.clone(),
            path: "/repo-wt".into(),
            worktree_id: Some("worktree-1".into()),
            sandbox_id: None,
            multi_repo_roots: "[]".into(),
            cleanup_policy: "manual".into(),
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        assert_eq!(store.list_worktrees(&project.id)?.len(), 1);
        assert_eq!(store.list_workspaces(&project.id)?.len(), 1);
        let event = store.append_session_event(
            "session-1",
            "input",
            json!({ "preview": "Authorization: Bearer secret" }),
        )?;
        assert_eq!(event.payload["preview"], "Authorization: Bearer [REDACTED]");

        store.create_mcp_attachment(&McpAttachmentRecord {
            id: "mcp-1".into(),
            profile: "default".into(),
            scope: "session".into(),
            project_id: Some(project.id.clone()),
            session_id: Some("session-1".into()),
            server_id: "server".into(),
            status: AttachmentStatus::Attached,
            materialized_state: "{}".into(),
            restart_required: false,
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        store.create_skill_attachment(&SkillAttachmentRecord {
            id: "skill-1".into(),
            profile: "default".into(),
            scope: "session".into(),
            project_id: Some(project.id.clone()),
            session_id: Some("session-1".into()),
            skill_id: "skill".into(),
            pool_path: "/pool/skill".into(),
            materialized_path: "/repo/.skills/skill".into(),
            status: AttachmentStatus::Attached,
            restart_required: false,
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        assert_eq!(
            store
                .list_mcp_attachments("default", Some("session"), None, Some("session-1"))?
                .len(),
            1
        );
        assert_eq!(
            store
                .list_skill_attachments("default", Some("session"), None, Some("session-1"))?
                .len(),
            1
        );

        store.create_watcher(&WatcherRecord {
            id: "watcher-1".into(),
            profile: "default".into(),
            project_id: Some(project.id.clone()),
            adapter_id: "github".into(),
            name: "issues".into(),
            config_ref: "config".into(),
            status: WatcherStatus::Running,
            last_event_at: 0,
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        let watcher_event = store.append_watcher_event(&WatcherEventRecord {
            id: "watch-event-1".into(),
            watcher_id: "watcher-1".into(),
            source: "github".into(),
            event_type: "issue".into(),
            payload_ref: "token=watcher-secret".into(),
            signature_status: "verified".into(),
            route_decision: "conductor".into(),
            delivered: true,
            created_at: now + 1,
        })?;
        assert_eq!(watcher_event.payload_ref, "token=[REDACTED]");
        assert_eq!(store.list_watcher_events("watcher-1", 0, 10)?.len(), 1);

        store.create_conductor(&ConductorRecord {
            id: "conductor-1".into(),
            profile: "default".into(),
            session_id: "session-1".into(),
            status: ConductorStatus::Running,
            watched_sessions: "[]".into(),
            channel_bindings: "{}".into(),
            last_heartbeat_at: now,
            version: 0,
            created_at: now,
            updated_at: now,
        })?;
        store.append_conductor_assignment(&ConductorAssignmentRecord {
            id: "assignment-1".into(),
            conductor_id: "conductor-1".into(),
            session_id: "session-1".into(),
            task_ref: "task".into(),
            status: "assigned".into(),
            assigned_at: now,
            completed_at: 0,
        })?;
        assert_eq!(store.list_conductor_assignments("conductor-1")?.len(), 1);
        assert_eq!(
            store
                .open_conductor_assignments_for_session("session-1")?
                .len(),
            1
        );
        let mut completed_assignment = store.get_conductor_assignment("assignment-1")?;
        completed_assignment.status = "completed".into();
        completed_assignment.completed_at = now + 1;
        let completed_assignment = store.update_conductor_assignment(&completed_assignment)?;
        assert_eq!(completed_assignment.status, "completed");
        assert_eq!(
            store
                .open_conductor_assignments_for_session("session-1")?
                .len(),
            0
        );

        let cost = store.append_cost_event(
            "session-1",
            0.012345,
            json!({
                "model": "gpt-5",
                "input_tokens": 10,
                "output_tokens": 20,
                "source": "api_key=cost-secret",
            }),
        )?;
        assert_eq!(cost.payload["source"], "api_key=[REDACTED]");
        let summary = store.cost_summary(&CostFilter {
            profile: "default".into(),
            project_id: Some(project.id.clone()),
            group_name: Some("moved".into()),
            session_id: None,
            agent: Some("codex".into()),
            model: Some("gpt-5".into()),
            start_at: now,
            end_at: now_ts(),
            include_archived: false,
        })?;
        assert_eq!(summary.total_cost_micros, 12345);
        assert_eq!(summary.event_count, 1);
        assert_eq!(summary.session_count, 1);
        assert_eq!(summary.total_tokens, 30);
        let events = store.cost_events(
            &CostFilter {
                profile: "default".into(),
                project_id: Some(project.id),
                group_name: Some("moved".into()),
                session_id: None,
                agent: Some("codex".into()),
                model: Some("gpt-5".into()),
                start_at: now,
                end_at: now_ts(),
                include_archived: false,
            },
            0,
            10,
        )?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, cost.id);
        assert_eq!(events[0].payload["source"], "api_key=[REDACTED]");

        Ok(())
    }
}
