use crate::models::{AttachmentStatus, McpAttachmentRecord, SkillAttachmentRecord};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpMaterializationEntry {
    pub id: String,
    pub scope: String,
    pub server_id: String,
    pub materialized_path: String,
    pub restart_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillMaterializationEntry {
    pub id: String,
    pub scope: String,
    pub skill_id: String,
    pub materialized_path: String,
    pub restart_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMaterializationPlan {
    pub mcp: Vec<McpMaterializationEntry>,
    pub skills: Vec<SkillMaterializationEntry>,
}

pub fn mcp_materialization_plan(records: &[McpAttachmentRecord]) -> Vec<McpMaterializationEntry> {
    let mut plan: Vec<_> = records
        .iter()
        .filter(|record| record.status == AttachmentStatus::Attached)
        .map(|record| McpMaterializationEntry {
            id: record.id.clone(),
            scope: record.scope.clone(),
            server_id: record.server_id.clone(),
            materialized_path: mcp_materialized_path(record),
            restart_required: record.restart_required,
        })
        .collect();
    plan.sort_by(|a, b| (&a.scope, &a.server_id, &a.id).cmp(&(&b.scope, &b.server_id, &b.id)));
    plan
}

pub fn skill_materialization_plan(
    records: &[SkillAttachmentRecord],
) -> Vec<SkillMaterializationEntry> {
    let mut plan: Vec<_> = records
        .iter()
        .filter(|record| record.status == AttachmentStatus::Attached)
        .map(|record| SkillMaterializationEntry {
            id: record.id.clone(),
            scope: record.scope.clone(),
            skill_id: record.skill_id.clone(),
            materialized_path: skill_materialized_path(record),
            restart_required: record.restart_required,
        })
        .collect();
    plan.sort_by(|a, b| (&a.scope, &a.skill_id, &a.id).cmp(&(&b.scope, &b.skill_id, &b.id)));
    plan
}

pub fn session_materialization_plan(
    mcp: &[McpAttachmentRecord],
    skills: &[SkillAttachmentRecord],
) -> SessionMaterializationPlan {
    SessionMaterializationPlan {
        mcp: mcp_materialization_plan(mcp),
        skills: skill_materialization_plan(skills),
    }
}

pub fn session_materialization_json(
    mcp: &[McpAttachmentRecord],
    skills: &[SkillAttachmentRecord],
) -> String {
    serde_json::to_string_pretty(&session_materialization_plan(mcp, skills))
        .expect("session materialization plan serializes")
}

fn mcp_materialized_path(record: &McpAttachmentRecord) -> String {
    materialized_state_path(&record.materialized_state)
        .unwrap_or_else(|| format!(".agent-helm/mcp/{}.json", record.server_id))
}

fn materialized_state_path(state: &str) -> Option<String> {
    let value: Value = serde_json::from_str(state).ok()?;
    ["materialized_path", "path"]
        .iter()
        .filter_map(|key| value.get(key)?.as_str())
        .find(|path| !path.is_empty())
        .map(ToOwned::to_owned)
}

fn skill_materialized_path(record: &SkillAttachmentRecord) -> String {
    if record.materialized_path.is_empty() {
        format!(".agent-helm/skills/{}", record.skill_id)
    } else {
        record.materialized_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_plans_are_sorted_and_active_only() {
        let mcp = vec![
            mcp_record("mcp-z", "zeta", AttachmentStatus::Attached, "{}", true),
            mcp_record(
                "mcp-off",
                "off",
                AttachmentStatus::Detached,
                r#"{"materialized_path":"/tmp/off.json"}"#,
                true,
            ),
            mcp_record(
                "mcp-a",
                "alpha",
                AttachmentStatus::Attached,
                r#"{"materialized_path":"/tmp/alpha.json"}"#,
                false,
            ),
        ];
        let skills = vec![
            skill_record("skill-z", "zeta", "", AttachmentStatus::Attached, false),
            skill_record(
                "skill-a",
                "alpha",
                "/tmp/alpha-skill",
                AttachmentStatus::Attached,
                true,
            ),
            skill_record("skill-off", "off", "", AttachmentStatus::Pending, true),
        ];

        assert_eq!(
            mcp_materialization_plan(&mcp),
            vec![
                McpMaterializationEntry {
                    id: "mcp-a".into(),
                    scope: "session".into(),
                    server_id: "alpha".into(),
                    materialized_path: "/tmp/alpha.json".into(),
                    restart_required: false,
                },
                McpMaterializationEntry {
                    id: "mcp-z".into(),
                    scope: "session".into(),
                    server_id: "zeta".into(),
                    materialized_path: ".agent-helm/mcp/zeta.json".into(),
                    restart_required: true,
                },
            ]
        );
        assert_eq!(
            skill_materialization_plan(&skills),
            vec![
                SkillMaterializationEntry {
                    id: "skill-a".into(),
                    scope: "session".into(),
                    skill_id: "alpha".into(),
                    materialized_path: "/tmp/alpha-skill".into(),
                    restart_required: true,
                },
                SkillMaterializationEntry {
                    id: "skill-z".into(),
                    scope: "session".into(),
                    skill_id: "zeta".into(),
                    materialized_path: ".agent-helm/skills/zeta".into(),
                    restart_required: false,
                },
            ]
        );
    }

    #[test]
    fn session_materialization_json_is_stable() {
        let mcp = vec![mcp_record(
            "mcp-a",
            "alpha",
            AttachmentStatus::Attached,
            r#"{"path":"/tmp/alpha.json"}"#,
            false,
        )];
        let skills = vec![skill_record(
            "skill-a",
            "alpha",
            "/tmp/alpha-skill",
            AttachmentStatus::Attached,
            true,
        )];

        assert_eq!(
            session_materialization_json(&mcp, &skills),
            r#"{
  "mcp": [
    {
      "id": "mcp-a",
      "scope": "session",
      "server_id": "alpha",
      "materialized_path": "/tmp/alpha.json",
      "restart_required": false
    }
  ],
  "skills": [
    {
      "id": "skill-a",
      "scope": "session",
      "skill_id": "alpha",
      "materialized_path": "/tmp/alpha-skill",
      "restart_required": true
    }
  ]
}"#
        );
    }

    fn mcp_record(
        id: &str,
        server_id: &str,
        status: AttachmentStatus,
        materialized_state: &str,
        restart_required: bool,
    ) -> McpAttachmentRecord {
        McpAttachmentRecord {
            id: id.into(),
            profile: "default".into(),
            scope: "session".into(),
            project_id: None,
            session_id: Some("session-1".into()),
            server_id: server_id.into(),
            status,
            materialized_state: materialized_state.into(),
            restart_required,
            version: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn skill_record(
        id: &str,
        skill_id: &str,
        materialized_path: &str,
        status: AttachmentStatus,
        restart_required: bool,
    ) -> SkillAttachmentRecord {
        SkillAttachmentRecord {
            id: id.into(),
            profile: "default".into(),
            scope: "session".into(),
            project_id: None,
            session_id: Some("session-1".into()),
            skill_id: skill_id.into(),
            pool_path: format!("/pool/{skill_id}"),
            materialized_path: materialized_path.into(),
            status,
            restart_required,
            version: 0,
            created_at: 1,
            updated_at: 1,
        }
    }
}
