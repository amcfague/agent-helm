use crate::{
    error::{AppError, Result},
    models::{
        ForkSessionRequest, LaunchSpec, McpAttachmentRecord, SessionEvent, SessionRecord,
        SessionStatus, SkillAttachmentRecord, StructuredEvent,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub command_backed: bool,
    pub resume: bool,
    pub fork: bool,
    pub mcp: bool,
    pub skills: bool,
    pub structured_events: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentForkPlan {
    pub command: String,
    pub inherits_conversation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterMaterializationPlan {
    pub json: String,
    pub restart_required: bool,
}

#[derive(Debug, Clone, Copy)]
struct AgentAdapter {
    id: &'static str,
    default_command: Option<&'static str>,
    capabilities: AgentCapabilities,
}

const COMMAND_BACKED: AgentCapabilities = AgentCapabilities {
    command_backed: true,
    resume: true,
    fork: true,
    mcp: true,
    skills: true,
    structured_events: true,
};

const ADAPTERS: &[AgentAdapter] = &[
    AgentAdapter {
        id: "shell",
        default_command: None,
        capabilities: COMMAND_BACKED,
    },
    AgentAdapter {
        id: "claude",
        default_command: Some("claude"),
        capabilities: COMMAND_BACKED,
    },
    AgentAdapter {
        id: "codex",
        default_command: Some("codex"),
        capabilities: COMMAND_BACKED,
    },
    AgentAdapter {
        id: "gemini",
        default_command: Some("gemini"),
        capabilities: COMMAND_BACKED,
    },
    AgentAdapter {
        id: "opencode",
        default_command: Some("opencode"),
        capabilities: COMMAND_BACKED,
    },
    AgentAdapter {
        id: "custom",
        default_command: None,
        capabilities: COMMAND_BACKED,
    },
];

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry;

impl AgentRegistry {
    pub fn capabilities(&self, agent: &str) -> Result<AgentCapabilities> {
        Ok(self.adapter(agent)?.capabilities)
    }

    pub fn build_launch_spec(&self, session: &SessionRecord) -> Result<LaunchSpec> {
        let adapter = self.adapter(&session.agent)?;
        let command = if session.command.trim().is_empty() {
            adapter
                .default_command
                .ok_or_else(|| AppError::msg(format!("agent requires command: {}", session.agent)))?
                .to_owned()
        } else {
            session.command.clone()
        };

        Ok(LaunchSpec {
            cwd: session.project_path.clone(),
            command,
            sandbox: None,
        })
    }

    pub fn resume(&self, session: &SessionRecord) -> Result<LaunchSpec> {
        self.build_launch_spec(session)
    }

    pub fn detect_status(
        &self,
        agent: &str,
        runtime_status: SessionStatus,
    ) -> Result<SessionStatus> {
        let _ = self.adapter(agent)?;
        Ok(runtime_status)
    }

    pub fn fork(
        &self,
        parent: &SessionRecord,
        request: &ForkSessionRequest,
    ) -> Result<AgentForkPlan> {
        let _ = self.adapter(&parent.agent)?;
        Ok(AgentForkPlan {
            command: parent.command.clone(),
            inherits_conversation: request.carry_state,
        })
    }

    pub fn apply_mcp(
        &self,
        session: &SessionRecord,
        records: &[McpAttachmentRecord],
    ) -> Result<AdapterMaterializationPlan> {
        let _ = self.adapter(&session.agent)?;
        Ok(AdapterMaterializationPlan {
            json: crate::materialization::session_materialization_json(records, &[]),
            restart_required: records.iter().any(|record| record.restart_required),
        })
    }

    pub fn apply_skills(
        &self,
        session: &SessionRecord,
        records: &[SkillAttachmentRecord],
    ) -> Result<AdapterMaterializationPlan> {
        let _ = self.adapter(&session.agent)?;
        Ok(AdapterMaterializationPlan {
            json: crate::materialization::session_materialization_json(&[], records),
            restart_required: records.iter().any(|record| record.restart_required),
        })
    }

    pub fn parse_structured_event(
        &self,
        agent: &str,
        event: &SessionEvent,
    ) -> Result<StructuredEvent> {
        let _ = self.adapter(agent)?;
        Ok(StructuredEvent {
            id: event.id,
            session_id: event.session_id.clone(),
            kind: event.kind.clone(),
            source: event
                .payload
                .get("source")
                .and_then(|value| value.as_str())
                .unwrap_or(agent)
                .to_string(),
            payload: event.payload.clone(),
            created_at: event.created_at,
        })
    }

    fn adapter(&self, agent: &str) -> Result<&'static AgentAdapter> {
        ADAPTERS
            .iter()
            .find(|adapter| adapter.id == agent)
            .ok_or_else(|| AppError::msg(format!("unsupported agent: {agent}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AttachmentStatus, SessionStatus};
    use serde_json::json;

    fn session(agent: &str, command: &str) -> SessionRecord {
        SessionRecord {
            id: "session".into(),
            name: "Session".into(),
            profile: "default".into(),
            group_name: "default".into(),
            project_id: "project".into(),
            workspace_id: "workspace".into(),
            worktree_id: None,
            parent_session_id: None,
            agent: agent.into(),
            command: command.into(),
            project_path: "/tmp/project".into(),
            status: SessionStatus::Starting,
            runtime_id: None,
            archived: false,
            version: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn shell_uses_session_command() {
        let registry = AgentRegistry;
        let spec = registry
            .build_launch_spec(&session("shell", "bash -lc 'echo ok'"))
            .unwrap();

        assert_eq!(spec.cwd, "/tmp/project");
        assert_eq!(spec.command, "bash -lc 'echo ok'");
        assert!(registry.capabilities("shell").unwrap().command_backed);
    }

    #[test]
    fn command_backed_aliases_use_session_command_when_present() {
        let registry = AgentRegistry;

        for agent in ["claude", "codex", "gemini", "opencode", "custom"] {
            let spec = registry
                .build_launch_spec(&session(agent, &format!("{agent} --resume")))
                .unwrap();

            assert_eq!(spec.command, format!("{agent} --resume"));
            assert!(registry.capabilities(agent).unwrap().command_backed);
        }
    }

    #[test]
    fn known_aliases_have_default_commands() {
        let registry = AgentRegistry;

        for agent in ["claude", "codex", "gemini", "opencode"] {
            let spec = registry.build_launch_spec(&session(agent, "")).unwrap();

            assert_eq!(spec.command, agent);
        }
    }

    #[test]
    fn unsupported_and_commandless_custom_are_rejected() {
        let registry = AgentRegistry;

        let unsupported = registry.build_launch_spec(&session("unknown", "unknown"));
        assert!(
            unsupported
                .unwrap_err()
                .to_string()
                .contains("unsupported agent: unknown")
        );

        let commandless_custom = registry.build_launch_spec(&session("custom", ""));
        assert!(
            commandless_custom
                .unwrap_err()
                .to_string()
                .contains("agent requires command: custom")
        );
    }

    #[test]
    fn parses_session_event_into_structured_event() {
        let registry = AgentRegistry;
        let event = SessionEvent {
            id: 7,
            session_id: "session".into(),
            kind: "input".into(),
            payload: json!({"source": "user", "preview": "hello"}),
            created_at: 42,
        };
        let structured = registry.parse_structured_event("shell", &event).unwrap();
        assert_eq!(structured.kind, "input");
        assert_eq!(structured.source, "user");
        assert_eq!(structured.payload["preview"], "hello");
    }

    #[test]
    fn resume_status_and_fork_plans_are_adapter_owned() {
        let registry = AgentRegistry;
        let parent = session("codex", "codex --resume");
        assert_eq!(registry.resume(&parent).unwrap().command, "codex --resume");
        assert_eq!(
            registry
                .detect_status("codex", SessionStatus::Running)
                .unwrap(),
            SessionStatus::Running
        );
        let plan = registry
            .fork(
                &parent,
                &ForkSessionRequest {
                    parent_session_id: parent.id.clone(),
                    name: None,
                    group_name: None,
                    worktree_branch: None,
                    carry_state: false,
                    start_immediately: true,
                },
            )
            .unwrap();
        assert_eq!(plan.command, "codex --resume");
        assert!(!plan.inherits_conversation);
    }

    #[test]
    fn adapter_materialization_reports_restart_need() {
        let registry = AgentRegistry;
        let session = session("codex", "codex");
        let mcp = McpAttachmentRecord {
            id: "mcp".into(),
            profile: "default".into(),
            scope: "session".into(),
            project_id: None,
            session_id: Some(session.id.clone()),
            server_id: "server".into(),
            status: AttachmentStatus::Attached,
            materialized_state: r#"{"path":"/tmp/server.json"}"#.into(),
            restart_required: true,
            version: 0,
            created_at: 0,
            updated_at: 0,
        };
        let skill = SkillAttachmentRecord {
            id: "skill".into(),
            profile: "default".into(),
            scope: "session".into(),
            project_id: None,
            session_id: Some(session.id.clone()),
            skill_id: "skill".into(),
            pool_path: String::new(),
            materialized_path: ".agent-helm/skills/skill".into(),
            status: AttachmentStatus::Attached,
            restart_required: false,
            version: 0,
            created_at: 0,
            updated_at: 0,
        };
        assert!(
            registry
                .apply_mcp(&session, &[mcp])
                .unwrap()
                .restart_required
        );
        assert!(
            registry
                .apply_skills(&session, &[skill])
                .unwrap()
                .json
                .contains(".agent-helm/skills/skill")
        );
    }
}
