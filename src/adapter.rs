use crate::{
    error::{AppError, Result},
    models::{
        ConductorAssignmentRecord, ForkSessionRequest, LaunchSpec, McpAttachmentRecord,
        SessionActivity, SessionDeckStatus, SessionDeckStatusDerivation, SessionEvent,
        SessionRecord, SessionStatus, SkillAttachmentRecord, StructuredEvent,
    },
};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub command_backed: bool,
    pub resume: bool,
    pub fork: bool,
    pub mcp: bool,
    pub skills: bool,
    pub structured_events: bool,
    pub agent_state_hooks: bool,
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

#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub line: i64,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct TranscriptAgentState {
    pub line: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy)]
struct AgentAdapter {
    id: &'static str,
    default_command: Option<&'static str>,
    capabilities: AgentCapabilities,
}

const AI_AGENT: AgentCapabilities = AgentCapabilities {
    command_backed: true,
    resume: true,
    fork: true,
    mcp: true,
    skills: true,
    structured_events: true,
    agent_state_hooks: true,
};

const COMMAND_ONLY: AgentCapabilities = AgentCapabilities {
    command_backed: true,
    resume: true,
    fork: true,
    mcp: false,
    skills: false,
    structured_events: true,
    agent_state_hooks: false,
};

const ADAPTERS: &[AgentAdapter] = &[
    AgentAdapter {
        id: "shell",
        default_command: None,
        capabilities: COMMAND_ONLY,
    },
    AgentAdapter {
        id: "claude",
        default_command: Some("claude"),
        capabilities: AI_AGENT,
    },
    AgentAdapter {
        id: "codex",
        default_command: Some("codex"),
        capabilities: AI_AGENT,
    },
    AgentAdapter {
        id: "gemini",
        default_command: Some("gemini"),
        capabilities: AI_AGENT,
    },
    AgentAdapter {
        id: "opencode",
        default_command: Some("opencode"),
        capabilities: AI_AGENT,
    },
    AgentAdapter {
        id: "custom",
        default_command: None,
        capabilities: COMMAND_ONLY,
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

    pub fn wrap_agent_state_launch(
        &self,
        agent: &str,
        command: &str,
        executable: &str,
        profile: &str,
        session_id: &str,
    ) -> Result<String> {
        let capabilities = self.capabilities(agent)?;
        if !capabilities.agent_state_hooks {
            return Ok(command.to_string());
        }
        let emit = format!(
            "{} --profile {} --json session record-event {} --state \"$1\" --source {} --tool {} >/dev/null 2>&1 || true",
            shell_quote(executable),
            shell_quote(profile),
            shell_quote(session_id),
            shell_quote("agent_helm_wrapper"),
            shell_quote(agent),
        );
        Ok(format!(
            "agent_helm_emit_state() {{ {emit}; }}; agent_helm_emit_state working; trap 'code=$?; agent_helm_emit_state idle; exit $code' EXIT; {command}"
        ))
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

    pub fn derive_deck_status(
        &self,
        agent: &str,
        lifecycle_status: SessionStatus,
        transcript_state: Option<&Value>,
        recent_events: &[SessionEvent],
        open_assignments: &[ConductorAssignmentRecord],
    ) -> Result<SessionDeckStatusDerivation> {
        let adapter = self.adapter(agent)?;
        let deck_status = match lifecycle_status {
            SessionStatus::Starting => SessionDeckStatus::Starting,
            SessionStatus::Stopped => SessionDeckStatus::Stopped,
            SessionStatus::Errored => SessionDeckStatus::Errored,
            SessionStatus::Running => {
                if let Some((status, activity)) = transcript_state.and_then(activity_from_payload) {
                    return Ok(SessionDeckStatusDerivation {
                        deck_status: status,
                        source_event_id: None,
                        source: "transcript".to_string(),
                        activity: Some(activity),
                    });
                }
                let has_assigned_assignment = open_assignments
                    .iter()
                    .any(|assignment| assignment_status_is(&assignment.status, "assigned"));
                let has_open_assignment = has_assigned_assignment
                    || open_assignments
                        .iter()
                        .any(|assignment| assignment_status_is(&assignment.status, "queued"));
                for event in recent_events {
                    if is_wrapper_lifecycle_agent_state(event)
                        || (has_open_assignment && is_runtime_lifecycle_agent_state(event))
                    {
                        continue;
                    }
                    if let Some((status, activity)) = activity_from_event(event) {
                        return Ok(SessionDeckStatusDerivation {
                            deck_status: status,
                            source_event_id: Some(event.id),
                            source: "session_event".to_string(),
                            activity: Some(activity),
                        });
                    }
                    if event.kind == "input" {
                        return Ok(SessionDeckStatusDerivation {
                            deck_status: SessionDeckStatus::Waiting,
                            source_event_id: Some(event.id),
                            source: "session_event".to_string(),
                            activity: Some(static_activity("waiting", "prompt sent", "input")),
                        });
                    }
                }
                if open_assignments
                    .iter()
                    .any(|assignment| assignment_status_is(&assignment.status, "queued"))
                {
                    return Ok(SessionDeckStatusDerivation {
                        deck_status: SessionDeckStatus::Queued,
                        source_event_id: None,
                        source: "conductor_assignment".to_string(),
                        activity: Some(static_activity("queued", "queued", "conductor_assignment")),
                    });
                }
                if has_assigned_assignment {
                    return Ok(SessionDeckStatusDerivation {
                        deck_status: SessionDeckStatus::Waiting,
                        source_event_id: None,
                        source: "conductor_assignment".to_string(),
                        activity: Some(static_activity(
                            "waiting",
                            "assigned",
                            "conductor_assignment",
                        )),
                    });
                }
                if matches!(adapter.id, "claude" | "codex") {
                    return Ok(SessionDeckStatusDerivation {
                        deck_status: SessionDeckStatus::Idle,
                        source_event_id: None,
                        source: "ai_lifecycle".to_string(),
                        activity: Some(static_activity("idle", "ready", "ai_lifecycle")),
                    });
                }
                SessionDeckStatus::Running
            }
        };
        Ok(SessionDeckStatusDerivation {
            deck_status,
            source_event_id: None,
            source: "lifecycle".to_string(),
            activity: None,
        })
    }

    pub fn fork(
        &self,
        parent: &SessionRecord,
        request: &ForkSessionRequest,
    ) -> Result<AgentForkPlan> {
        let adapter = self.adapter(&parent.agent)?;
        let command = if request.carry_state {
            parent.command.clone()
        } else {
            adapter
                .default_command
                .map(String::from)
                .unwrap_or_else(|| parent.command.clone())
        };
        Ok(AgentForkPlan {
            command,
            inherits_conversation: request.carry_state && adapter.default_command.is_some(),
        })
    }

    pub fn apply_mcp(
        &self,
        session: &SessionRecord,
        records: &[McpAttachmentRecord],
    ) -> Result<AdapterMaterializationPlan> {
        let capabilities = self.capabilities(&session.agent)?;
        if !capabilities.mcp {
            return Err(AppError::msg(format!(
                "agent does not support MCP: {}",
                session.agent
            )));
        }
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
        let capabilities = self.capabilities(&session.agent)?;
        if !capabilities.skills {
            return Err(AppError::msg(format!(
                "agent does not support skills: {}",
                session.agent
            )));
        }
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

    pub fn derive_transcript_agent_state(
        &self,
        agent: &str,
        entries: &[TranscriptEntry],
    ) -> Result<Option<TranscriptAgentState>> {
        let adapter = self.adapter(agent)?;
        Ok(match adapter.id {
            "claude" => claude_transcript_agent_state(entries),
            "codex" => codex_transcript_agent_state(entries),
            _ => None,
        })
    }

    fn adapter(&self, agent: &str) -> Result<&'static AgentAdapter> {
        if let Some(adapter) = ADAPTERS.iter().find(|adapter| adapter.id == agent) {
            return Ok(adapter);
        }
        ADAPTERS
            .iter()
            .find(|adapter| adapter.id == "custom")
            .ok_or_else(|| AppError::msg(format!("unsupported agent: {agent}")))
    }
}

#[derive(Debug, Clone)]
struct ActiveTranscriptCall {
    key: String,
    tool: Option<String>,
}

fn codex_transcript_agent_state(entries: &[TranscriptEntry]) -> Option<TranscriptAgentState> {
    let mut active_calls = Vec::new();
    let mut state = None;
    let mut line = None;
    let mut event = None;
    let mut tool = None;
    let mut call_id = None;

    for entry in entries {
        let payload = codex_event_payload(&entry.value);
        let Some(event_type) =
            json_string(payload, "type").or_else(|| json_string(&entry.value, "type"))
        else {
            continue;
        };

        if codex_call_starts(event_type) {
            let key = transcript_call_key(payload, event_type, entry.line);
            let next_tool = codex_tool_name(payload);
            upsert_active_call(&mut active_calls, key.clone(), next_tool.clone());
            state = Some("occupied");
            line = Some(entry.line);
            event = Some(event_type.to_string());
            tool = next_tool;
            call_id = json_string(payload, "call_id").map(str::to_string);
            continue;
        }

        if codex_call_ends(event_type) {
            let removed_tool =
                remove_active_call(&mut active_calls, payload).or_else(|| codex_tool_name(payload));
            state = Some(if active_calls.is_empty() {
                "thinking"
            } else {
                "occupied"
            });
            line = Some(entry.line);
            event = Some(event_type.to_string());
            tool = active_calls
                .last()
                .and_then(|call| call.tool.clone())
                .or(removed_tool);
            call_id = json_string(payload, "call_id").map(str::to_string);
            continue;
        }

        if codex_reasoning_event(event_type) {
            state = Some(if active_calls.is_empty() {
                "thinking"
            } else {
                "occupied"
            });
            line = Some(entry.line);
            event = Some(event_type.to_string());
            tool = active_calls.last().and_then(|call| call.tool.clone());
            call_id = json_string(payload, "call_id").map(str::to_string);
            continue;
        }

        if codex_idle_event(event_type) {
            active_calls.clear();
            state = Some("idle");
            line = Some(entry.line);
            event = Some(event_type.to_string());
            tool = None;
            call_id = None;
        }
    }

    if let Some(active_call) = active_calls.last() {
        state = Some("occupied");
        tool = active_call.tool.clone();
        if call_id.is_none() {
            call_id = Some(active_call.key.clone());
        }
    }

    let state = state?;
    let mut payload = serde_json::Map::from_iter([
        ("state".to_string(), json!(state)),
        ("source".to_string(), json!("codex_transcript")),
    ]);
    if let Some(event) = event {
        payload.insert("transcript_event".to_string(), json!(event));
    }
    if let Some(tool) = tool {
        payload.insert("tool".to_string(), json!({ "name": tool }));
    }
    if let Some(call_id) = call_id {
        payload.insert("call_id".to_string(), json!(call_id));
    }
    if !active_calls.is_empty() {
        payload.insert("active_call_count".to_string(), json!(active_calls.len()));
    }

    Some(TranscriptAgentState {
        line: line.unwrap_or_else(|| entries.last().map(|entry| entry.line).unwrap_or_default()),
        payload: Value::Object(payload),
    })
}

fn claude_transcript_agent_state(entries: &[TranscriptEntry]) -> Option<TranscriptAgentState> {
    let mut active_calls = Vec::new();
    let mut state = None;
    let mut line = None;
    let mut tool = None;

    for entry in entries {
        let value = &entry.value;
        let role = claude_role(value).unwrap_or("");
        let content = claude_message_content(value).or_else(|| value.get("content"));
        let tool_uses = content.map(claude_tool_use_blocks).unwrap_or_default();
        let tool_results = content.map(claude_tool_result_blocks).unwrap_or_default();
        let has_thinking = content.is_some_and(claude_content_has_thinking);

        if !tool_uses.is_empty() {
            for tool_use in tool_uses {
                let key = claude_tool_use_key(tool_use, entry.line);
                let next_tool = json_string(tool_use, "name")
                    .and_then(non_empty_string)
                    .or_else(|| Some("tool".to_string()));
                upsert_active_call(&mut active_calls, key, next_tool.clone());
                tool = next_tool;
            }
            state = Some("occupied");
            line = Some(entry.line);
            continue;
        }

        if !tool_results.is_empty() {
            for tool_result in tool_results {
                tool = remove_active_call(&mut active_calls, tool_result);
            }
            state = Some(if active_calls.is_empty() {
                "thinking"
            } else {
                "occupied"
            });
            line = Some(entry.line);
            if state == Some("occupied") {
                tool = active_calls.last().and_then(|call| call.tool.clone());
            }
            continue;
        }

        if has_thinking || claude_assistant_is_incomplete(value) {
            state = Some(if active_calls.is_empty() {
                "thinking"
            } else {
                "occupied"
            });
            line = Some(entry.line);
            tool = active_calls.last().and_then(|call| call.tool.clone());
            continue;
        }

        if role.eq_ignore_ascii_case("assistant") && active_calls.is_empty() {
            state = Some("idle");
            line = Some(entry.line);
            tool = None;
        }
    }

    if let Some(active_call) = active_calls.last() {
        state = Some("occupied");
        tool = active_call.tool.clone();
    }

    let state = state?;
    let mut payload = serde_json::Map::from_iter([
        ("state".to_string(), json!(state)),
        ("source".to_string(), json!("claude_transcript")),
    ]);
    if let Some(tool) = tool {
        payload.insert("tool".to_string(), json!({ "name": tool }));
    }
    if !active_calls.is_empty() {
        payload.insert("active_call_count".to_string(), json!(active_calls.len()));
    }

    Some(TranscriptAgentState {
        line: line.unwrap_or_else(|| entries.last().map(|entry| entry.line).unwrap_or_default()),
        payload: Value::Object(payload),
    })
}

fn upsert_active_call(
    active_calls: &mut Vec<ActiveTranscriptCall>,
    key: String,
    tool: Option<String>,
) {
    if let Some(call) = active_calls.iter_mut().find(|call| call.key == key) {
        call.tool = tool;
        return;
    }
    active_calls.push(ActiveTranscriptCall { key, tool });
}

fn remove_active_call(
    active_calls: &mut Vec<ActiveTranscriptCall>,
    payload: &Value,
) -> Option<String> {
    let key = json_string(payload, "call_id")
        .or_else(|| json_string(payload, "tool_use_id"))
        .or_else(|| json_string(payload, "id"));
    if let Some(key) = key
        && let Some(index) = active_calls.iter().position(|call| call.key == key)
    {
        return active_calls.remove(index).tool;
    }
    active_calls.pop().and_then(|call| call.tool)
}

fn transcript_call_key(payload: &Value, event_type: &str, line: i64) -> String {
    json_string(payload, "call_id")
        .or_else(|| json_string(payload, "tool_use_id"))
        .or_else(|| json_string(payload, "id"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{event_type}:{line}"))
}

fn codex_call_starts(event_type: &str) -> bool {
    matches!(
        event_type,
        "function_call"
            | "custom_tool_call"
            | "mcp_tool_call_begin"
            | "mcp_tool_call_start"
            | "patch_apply_begin"
            | "patch_apply_start"
            | "background_command_start"
            | "background_command_started"
            | "background_task_start"
            | "background_task_started"
            | "sub_agent_start"
            | "sub_agent_started"
            | "subagent_start"
            | "subagent_started"
    )
}

fn codex_call_ends(event_type: &str) -> bool {
    matches!(
        event_type,
        "function_call_output"
            | "custom_tool_call_output"
            | "mcp_tool_call_end"
            | "mcp_tool_call_finished"
            | "patch_apply_end"
            | "patch_apply_finished"
            | "background_command_end"
            | "background_command_finished"
            | "background_task_end"
            | "background_task_finished"
            | "sub_agent_end"
            | "sub_agent_finished"
            | "subagent_end"
            | "subagent_finished"
    )
}

fn codex_reasoning_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "reasoning" | "agent_reasoning" | "thinking" | "task_started"
    )
}

fn codex_idle_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "agent_message" | "message" | "task_complete" | "turn_aborted" | "turn_complete"
    )
}

fn codex_event_payload(value: &Value) -> &Value {
    value.get("payload").unwrap_or(value)
}

fn codex_tool_name(value: &Value) -> Option<String> {
    json_string(value, "name")
        .or_else(|| json_string(value, "tool_name"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn claude_role(value: &Value) -> Option<&str> {
    value
        .get("message")
        .and_then(|message| json_string(message, "role"))
        .or_else(|| json_string(value, "role"))
        .or_else(|| json_string(value, "type"))
}

fn claude_message_content(value: &Value) -> Option<&Value> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
}

fn claude_tool_use_key(value: &Value, line: i64) -> String {
    json_string(value, "id")
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool_use:{line}"))
}

fn claude_tool_use_blocks(value: &Value) -> Vec<&Value> {
    claude_content_blocks(value, "tool_use")
}

fn claude_tool_result_blocks(value: &Value) -> Vec<&Value> {
    claude_content_blocks(value, "tool_result")
}

fn claude_content_blocks<'a>(value: &'a Value, block_type: &str) -> Vec<&'a Value> {
    let mut blocks = Vec::new();
    collect_claude_content_blocks(value, block_type, &mut blocks);
    blocks
}

fn collect_claude_content_blocks<'a>(
    value: &'a Value,
    block_type: &str,
    blocks: &mut Vec<&'a Value>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_claude_content_blocks(item, block_type, blocks);
            }
        }
        Value::Object(object) => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == block_type)
            {
                blocks.push(value);
            }
            if let Some(content) = object.get("content") {
                collect_claude_content_blocks(content, block_type, blocks);
            }
        }
        _ => {}
    }
}

fn claude_content_has_thinking(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(claude_content_has_thinking),
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "thinking" | "reasoning"))
                || object
                    .get("content")
                    .is_some_and(claude_content_has_thinking)
        }
        _ => false,
    }
}

fn claude_assistant_is_incomplete(value: &Value) -> bool {
    if !claude_role(value).is_some_and(|role| role.eq_ignore_ascii_case("assistant")) {
        return false;
    }
    value
        .get("message")
        .and_then(|message| message.get("stop_reason"))
        .or_else(|| value.get("stop_reason"))
        .is_some_and(Value::is_null)
}

fn json_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn activity_from_event(event: &SessionEvent) -> Option<(SessionDeckStatus, SessionActivity)> {
    if event.kind != "agent_state" {
        return None;
    }
    if let Some(activity) = activity_from_payload(&event.payload) {
        return Some(activity);
    }

    gemini_activity_from_event(event)
}

fn activity_from_payload(payload: &Value) -> Option<(SessionDeckStatus, SessionActivity)> {
    let state = payload
        .get("state")
        .and_then(|value| value.as_str())?
        .trim()
        .to_ascii_lowercase();
    let deck_status = match state.as_str() {
        "queued" => SessionDeckStatus::Queued,
        "waiting" => SessionDeckStatus::Waiting,
        "occupied" => SessionDeckStatus::Occupied,
        "thinking" => SessionDeckStatus::Thinking,
        "running" | "busy" | "working" => SessionDeckStatus::Running,
        "idle" | "done" | "ready" => SessionDeckStatus::Idle,
        _ => return None,
    };
    let source = payload
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or("agent_state")
        .to_string();
    let tool = agent_state_tool_name(payload);
    let label = agent_state_activity_label(&state, tool.as_deref());
    Some((
        deck_status,
        SessionActivity {
            state,
            label,
            source,
            tool,
        },
    ))
}

fn static_activity(state: &str, label: &str, source: &str) -> SessionActivity {
    SessionActivity {
        state: state.to_string(),
        label: label.to_string(),
        source: source.to_string(),
        tool: None,
    }
}

fn agent_state_tool_name(payload: &Value) -> Option<String> {
    match payload.get("tool")? {
        Value::String(name) => non_empty_string(name),
        Value::Object(object) => object
            .get("name")
            .and_then(|value| value.as_str())
            .and_then(non_empty_string),
        _ => None,
    }
}

fn gemini_activity_from_event(
    event: &SessionEvent,
) -> Option<(SessionDeckStatus, SessionActivity)> {
    let source = event
        .payload
        .get("source")
        .and_then(|value| value.as_str())?;
    if source != "gemini_transcript" {
        return None;
    }
    let messages = event
        .payload
        .get("messages")
        .or_else(|| {
            event
                .payload
                .get("conversation")
                .and_then(|conversation| conversation.get("messages"))
        })
        .and_then(|value| value.as_array())?;
    let latest = messages.iter().rev().find(|message| {
        message
            .get("type")
            .and_then(|value| value.as_str())
            .is_some()
    })?;
    let (state, deck_status, tool) = match latest.get("type").and_then(|value| value.as_str())? {
        "user" => ("waiting", SessionDeckStatus::Waiting, None),
        "gemini" => gemini_message_activity(latest)?,
        _ => return None,
    };
    let label = agent_state_activity_label(state, tool.as_deref());
    Some((
        deck_status,
        SessionActivity {
            state: state.to_string(),
            label,
            source: source.to_string(),
            tool,
        },
    ))
}

fn gemini_message_activity(
    message: &serde_json::Value,
) -> Option<(&'static str, SessionDeckStatus, Option<String>)> {
    let Some(tool_calls) = message.get("toolCalls").and_then(|value| value.as_array()) else {
        return message
            .get("content")
            .is_some()
            .then_some(("done", SessionDeckStatus::Idle, None));
    };
    if let Some(tool_call) = gemini_tool_call_with_status(tool_calls, gemini_tool_status_is_active)
    {
        return Some((
            "working",
            SessionDeckStatus::Running,
            gemini_tool_call_name(tool_call),
        ));
    }
    if let Some(tool_call) =
        gemini_tool_call_with_status(tool_calls, |status| status == "awaiting_approval")
    {
        return Some((
            "waiting",
            SessionDeckStatus::Waiting,
            gemini_tool_call_name(tool_call),
        ));
    }
    if let Some(tool_call) =
        gemini_tool_call_with_status(tool_calls, gemini_tool_status_is_terminal)
    {
        return Some((
            "done",
            SessionDeckStatus::Idle,
            gemini_tool_call_name(tool_call),
        ));
    }
    None
}

fn gemini_tool_call_with_status(
    tool_calls: &[serde_json::Value],
    matches_status: impl Fn(&str) -> bool,
) -> Option<&serde_json::Value> {
    tool_calls.iter().rev().find(|tool_call| {
        tool_call
            .get("status")
            .and_then(|value| value.as_str())
            .map(|status| matches_status(&status.trim().to_ascii_lowercase()))
            .unwrap_or(false)
    })
}

fn gemini_tool_status_is_active(status: &str) -> bool {
    matches!(status, "validating" | "scheduled" | "executing")
}

fn gemini_tool_status_is_terminal(status: &str) -> bool {
    matches!(status, "success" | "error" | "cancelled")
}

fn gemini_tool_call_name(tool_call: &serde_json::Value) -> Option<String> {
    tool_call
        .get("name")
        .or_else(|| tool_call.get("displayName"))
        .and_then(|value| value.as_str())
        .and_then(non_empty_string)
}

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn agent_state_activity_label(state: &str, tool: Option<&str>) -> String {
    match (state, tool) {
        ("occupied" | "running" | "busy" | "working", Some(tool)) => format!("using {tool}"),
        ("idle" | "done" | "ready", Some(tool)) => format!("{tool} done"),
        ("thinking", _) => "thinking".to_string(),
        ("occupied" | "running" | "busy" | "working", _) => "working".to_string(),
        ("idle" | "done" | "ready", _) => "ready".to_string(),
        ("waiting", _) => "waiting".to_string(),
        ("queued", _) => "queued".to_string(),
        _ => state.to_string(),
    }
}

fn assignment_status_is(status: &str, expected: &str) -> bool {
    status.trim().eq_ignore_ascii_case(expected)
}

fn is_runtime_lifecycle_agent_state(event: &SessionEvent) -> bool {
    if event.kind != "agent_state" {
        return false;
    }
    let state = event
        .payload
        .get("state")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let source = event
        .payload
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    state == "running"
        && matches!(
            source.as_str(),
            "runtime_start" | "runtime_restart" | "runtime_status"
        )
}

fn is_wrapper_lifecycle_agent_state(event: &SessionEvent) -> bool {
    if event.kind != "agent_state" {
        return false;
    }
    let source = event
        .payload
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    source == "agent_helm_wrapper"
}

fn shell_quote(value: &str) -> String {
    crate::util::shell_quote(value)
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

    fn fork_request(parent: &SessionRecord, carry_state: bool) -> ForkSessionRequest {
        ForkSessionRequest {
            parent_session_id: parent.id.clone(),
            name: None,
            group_name: None,
            worktree_branch: None,
            carry_state,
            start_immediately: true,
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
        let capabilities = registry.capabilities("shell").unwrap();
        assert!(capabilities.command_backed);
        assert!(!capabilities.mcp);
        assert!(!capabilities.skills);
        assert!(!capabilities.agent_state_hooks);
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
            assert_eq!(
                registry.capabilities(agent).unwrap().agent_state_hooks,
                agent != "custom"
            );
        }
    }

    #[test]
    fn configured_tool_profile_names_fall_back_to_custom_adapter() {
        let registry = AgentRegistry;
        let spec = registry
            .build_launch_spec(&session("review-bot", "review-bot --strict"))
            .unwrap();
        let capabilities = registry.capabilities("review-bot").unwrap();

        assert_eq!(spec.command, "review-bot --strict");
        assert!(capabilities.command_backed);
        assert!(!capabilities.mcp);
        assert!(!capabilities.skills);
        assert!(!capabilities.agent_state_hooks);
        assert!(
            registry
                .build_launch_spec(&session("review-bot", ""))
                .is_err()
        );
    }

    #[test]
    fn ai_launch_wrapper_emits_agent_state_events() {
        let registry = AgentRegistry;
        let command = registry
            .wrap_agent_state_launch(
                "codex",
                "codex --resume",
                "/tmp/agent helm/bin",
                "test profile",
                "session-1",
            )
            .unwrap();

        assert!(command.contains("session record-event"));
        assert!(command.contains("agent_helm_emit_state working"));
        assert!(command.contains("agent_helm_emit_state idle"));
        assert!(command.contains("--profile 'test profile'"));
        assert!(command.contains("--tool codex"));
        assert!(command.contains("'/tmp/agent helm/bin'"));
        assert!(command.ends_with("codex --resume"));
    }

    #[test]
    fn command_only_launch_wrapper_is_noop() {
        let registry = AgentRegistry;
        let command = registry
            .wrap_agent_state_launch("shell", "cat", "/tmp/agent-helm", "test", "session-1")
            .unwrap();

        assert_eq!(command, "cat");
    }

    #[test]
    fn known_aliases_have_default_commands() {
        let registry = AgentRegistry;

        for agent in ["claude", "codex", "gemini", "opencode"] {
            let spec = registry.build_launch_spec(&session(agent, "")).unwrap();

            assert_eq!(spec.command, agent);
        }
    }

    fn transcript_entries(values: Vec<serde_json::Value>) -> Vec<TranscriptEntry> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| TranscriptEntry {
                line: (index + 1) as i64,
                value,
            })
            .collect()
    }

    fn transcript_state(agent: &str, values: Vec<serde_json::Value>) -> TranscriptAgentState {
        AgentRegistry
            .derive_transcript_agent_state(agent, &transcript_entries(values))
            .unwrap()
            .expect("transcript state")
    }

    #[test]
    fn codex_transcript_state_tracks_reasoning_calls_background_and_idle() {
        let reasoning = transcript_state(
            "codex",
            vec![json!({"type": "response_item", "payload": {"type": "reasoning"}})],
        );
        assert_eq!(reasoning.payload["state"], "thinking");

        let active_call = transcript_state(
            "codex",
            vec![json!({
                "type": "response_item",
                "payload": {"type": "function_call", "call_id": "call-1", "name": "shell"}
            })],
        );
        assert_eq!(active_call.payload["state"], "occupied");
        assert_eq!(active_call.payload["tool"]["name"], "shell");
        assert_eq!(active_call.payload["call_id"], "call-1");

        let completed_call = transcript_state(
            "codex",
            vec![
                json!({"type": "response_item", "payload": {"type": "function_call", "call_id": "call-1", "name": "shell"}}),
                json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call-1"}}),
            ],
        );
        assert_eq!(completed_call.payload["state"], "thinking");

        let multi_call = transcript_state(
            "codex",
            vec![
                json!({"type": "response_item", "payload": {"type": "function_call", "call_id": "call-1", "name": "shell"}}),
                json!({"type": "response_item", "payload": {"type": "custom_tool_call", "call_id": "call-2", "name": "web_search"}}),
                json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call-1"}}),
            ],
        );
        assert_eq!(multi_call.payload["state"], "occupied");
        assert_eq!(multi_call.payload["active_call_count"], 1);
        assert_eq!(multi_call.payload["tool"]["name"], "web_search");

        let background = transcript_state(
            "codex",
            vec![json!({"type": "background_task_started", "id": "bg-1", "name": "sub-agent"})],
        );
        assert_eq!(background.payload["state"], "occupied");

        let idle = transcript_state(
            "codex",
            vec![
                json!({"type": "response_item", "payload": {"type": "function_call", "call_id": "call-1", "name": "shell"}}),
                json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call-1"}}),
                json!({"type": "response_item", "payload": {"type": "agent_message", "content": "done"}}),
            ],
        );
        assert_eq!(idle.payload["state"], "idle");
    }

    #[test]
    fn claude_transcript_state_tracks_reasoning_tools_results_and_idle() {
        let reasoning = transcript_state(
            "claude",
            vec![json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "plan"}]}
            })],
        );
        assert_eq!(reasoning.payload["state"], "thinking");

        let active_tool = transcript_state(
            "claude",
            vec![json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "tool-1", "name": "Bash"}]}
            })],
        );
        assert_eq!(active_tool.payload["state"], "occupied");
        assert_eq!(active_tool.payload["tool"]["name"], "Bash");

        let completed_result = transcript_state(
            "claude",
            vec![
                json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "tool-1", "name": "Bash"}]}}),
                json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"}]}}),
            ],
        );
        assert_eq!(completed_result.payload["state"], "thinking");

        let multi_tool = transcript_state(
            "claude",
            vec![
                json!({"type": "assistant", "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tool-1", "name": "Bash"},
                    {"type": "tool_use", "id": "tool-2", "name": "Read"}
                ]}}),
                json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"}]}}),
            ],
        );
        assert_eq!(multi_tool.payload["state"], "occupied");
        assert_eq!(multi_tool.payload["active_call_count"], 1);
        assert_eq!(multi_tool.payload["tool"]["name"], "Read");

        let idle = transcript_state(
            "claude",
            vec![
                json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "tool-1", "name": "Bash"}]}}),
                json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"}]}}),
                json!({"type": "assistant", "message": {"role": "assistant", "content": "done"}}),
            ],
        );
        assert_eq!(idle.payload["state"], "idle");
    }

    #[test]
    fn transcript_state_precedes_runtime_lifecycle_events() {
        let registry = AgentRegistry;
        let transcript = json!({"state": "idle", "source": "codex_transcript"});
        let events = vec![SessionEvent {
            id: 9,
            session_id: "session".into(),
            kind: "agent_state".into(),
            payload: json!({"state": "running", "source": "runtime_start"}),
            created_at: 9,
        }];

        let derived = registry
            .derive_deck_status(
                "codex",
                SessionStatus::Running,
                Some(&transcript),
                &events,
                &[],
            )
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Idle);
        assert_eq!(derived.source_event_id, None);
        assert_eq!(derived.source, "transcript");
    }

    #[test]
    fn derive_deck_status_uses_latest_agent_state_event() {
        let registry = AgentRegistry;
        let events = vec![SessionEvent {
            id: 7,
            session_id: "session".into(),
            kind: "agent_state".into(),
            payload: json!({"state": "idle"}),
            created_at: 0,
        }];

        let derived = registry
            .derive_deck_status("shell", SessionStatus::Running, None, &events, &[])
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Idle);
        assert_eq!(derived.source_event_id, Some(7));
        assert_eq!(derived.activity.as_ref().unwrap().label, "ready");
    }

    #[test]
    fn derive_deck_status_includes_agent_activity_tool_label() {
        let registry = AgentRegistry;
        let events = vec![SessionEvent {
            id: 8,
            session_id: "session".into(),
            kind: "agent_state".into(),
            payload: json!({
                "state": "working",
                "source": "claude_transcript",
                "tool": {"name": "Bash"}
            }),
            created_at: 0,
        }];

        let derived = registry
            .derive_deck_status("claude", SessionStatus::Running, None, &events, &[])
            .unwrap();

        let activity = derived.activity.expect("activity");
        assert_eq!(derived.deck_status, SessionDeckStatus::Running);
        assert_eq!(activity.state, "working");
        assert_eq!(activity.label, "using Bash");
        assert_eq!(activity.source, "claude_transcript");
        assert_eq!(activity.tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn derive_deck_status_reads_gemini_transcript_tool_statuses() {
        let registry = AgentRegistry;

        for (id, tool_status, expected_deck_status, expected_state, expected_label) in [
            (
                20,
                "executing",
                SessionDeckStatus::Running,
                "working",
                "using run_shell_command",
            ),
            (
                30,
                "awaiting_approval",
                SessionDeckStatus::Waiting,
                "waiting",
                "waiting",
            ),
            (
                40,
                "success",
                SessionDeckStatus::Idle,
                "done",
                "run_shell_command done",
            ),
        ] {
            let events = vec![SessionEvent {
                id,
                session_id: "session".into(),
                kind: "agent_state".into(),
                payload: json!({
                    "source": "gemini_transcript",
                    "messages": [
                        {
                            "id": "user-message",
                            "timestamp": "2026-06-26T00:00:00Z",
                            "type": "user",
                            "content": "run the shell command"
                        },
                        {
                            "id": "gemini-message",
                            "timestamp": "2026-06-26T00:00:01Z",
                            "type": "gemini",
                            "content": "",
                            "toolCalls": [
                                {
                                    "id": "tool-call",
                                    "name": "run_shell_command",
                                    "args": {"command": "git status"},
                                    "status": tool_status,
                                    "timestamp": "2026-06-26T00:00:02Z"
                                }
                            ]
                        }
                    ]
                }),
                created_at: id,
            }];

            let derived = registry
                .derive_deck_status("gemini", SessionStatus::Running, None, &events, &[])
                .unwrap();
            let activity = derived.activity.expect("activity");

            assert_eq!(derived.deck_status, expected_deck_status);
            assert_eq!(derived.source_event_id, Some(id));
            assert_eq!(activity.state, expected_state);
            assert_eq!(activity.label, expected_label);
            assert_eq!(activity.source, "gemini_transcript");
            assert_eq!(activity.tool.as_deref(), Some("run_shell_command"));
        }
    }

    #[test]
    fn derive_deck_status_treats_active_agent_state_as_running() {
        let registry = AgentRegistry;

        for (id, state, expected) in [
            (10, "running", SessionDeckStatus::Running),
            (20, " Busy ", SessionDeckStatus::Running),
            (30, "WORKING", SessionDeckStatus::Running),
            (40, "\tThinking\n", SessionDeckStatus::Thinking),
        ] {
            let events = vec![
                SessionEvent {
                    id,
                    session_id: "session".into(),
                    kind: "agent_state".into(),
                    payload: json!({"state": state}),
                    created_at: id,
                },
                SessionEvent {
                    id: id - 1,
                    session_id: "session".into(),
                    kind: "input".into(),
                    payload: json!({"source": "user"}),
                    created_at: id - 1,
                },
                SessionEvent {
                    id: id - 2,
                    session_id: "session".into(),
                    kind: "agent_state".into(),
                    payload: json!({"state": "waiting"}),
                    created_at: id - 2,
                },
            ];

            let derived = registry
                .derive_deck_status("shell", SessionStatus::Running, None, &events, &[])
                .unwrap();

            assert_eq!(derived.deck_status, expected);
            assert_eq!(derived.source_event_id, Some(id));
        }
    }

    #[test]
    fn derive_deck_status_treats_open_assigned_conductor_assignment_as_waiting() {
        let registry = AgentRegistry;
        let assignments = vec![ConductorAssignmentRecord {
            id: "assignment".into(),
            conductor_id: "conductor".into(),
            session_id: "session".into(),
            task_ref: "task".into(),
            status: " Assigned ".into(),
            assigned_at: 10,
            completed_at: 0,
        }];

        let derived = registry
            .derive_deck_status("shell", SessionStatus::Running, None, &[], &assignments)
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Waiting);
        assert_eq!(derived.source_event_id, None);
        assert_eq!(derived.source, "conductor_assignment");
    }

    #[test]
    fn derive_deck_status_prefers_recent_agent_state_over_assigned_assignment() {
        let registry = AgentRegistry;
        let events = vec![SessionEvent {
            id: 9,
            session_id: "session".into(),
            kind: "agent_state".into(),
            payload: json!({"state": "running"}),
            created_at: 9,
        }];
        let assignments = vec![ConductorAssignmentRecord {
            id: "assignment".into(),
            conductor_id: "conductor".into(),
            session_id: "session".into(),
            task_ref: "task".into(),
            status: "assigned".into(),
            assigned_at: 10,
            completed_at: 0,
        }];

        let derived = registry
            .derive_deck_status("shell", SessionStatus::Running, None, &events, &assignments)
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Running);
        assert_eq!(derived.source_event_id, Some(9));
        assert_eq!(derived.source, "session_event");
    }

    #[test]
    fn derive_deck_status_ignores_runtime_lifecycle_running_when_assignment_open() {
        let registry = AgentRegistry;
        let events = vec![SessionEvent {
            id: 9,
            session_id: "session".into(),
            kind: "agent_state".into(),
            payload: json!({"state": "running", "source": "runtime_start"}),
            created_at: 9,
        }];
        let assignments = vec![ConductorAssignmentRecord {
            id: "assignment".into(),
            conductor_id: "conductor".into(),
            session_id: "session".into(),
            task_ref: "task".into(),
            status: "assigned".into(),
            assigned_at: 10,
            completed_at: 0,
        }];

        let derived = registry
            .derive_deck_status("shell", SessionStatus::Running, None, &events, &assignments)
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Waiting);
        assert_eq!(derived.source_event_id, None);
        assert_eq!(derived.source, "conductor_assignment");
    }

    #[test]
    fn derive_deck_status_does_not_invent_idle_without_event() {
        let registry = AgentRegistry;

        let derived = registry
            .derive_deck_status("shell", SessionStatus::Running, None, &[], &[])
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Running);
        assert_eq!(derived.source_event_id, None);
    }

    #[test]
    fn derive_deck_status_ignores_wrapper_lifecycle_for_ai_agents() {
        let registry = AgentRegistry;
        let events = vec![SessionEvent {
            id: 9,
            session_id: "session".into(),
            kind: "agent_state".into(),
            payload: json!({
                "state": "working",
                "source": "agent_helm_wrapper",
                "tool": {"name": "codex"}
            }),
            created_at: 9,
        }];

        let derived = registry
            .derive_deck_status("codex", SessionStatus::Running, None, &events, &[])
            .unwrap();

        assert_eq!(derived.deck_status, SessionDeckStatus::Idle);
        assert_eq!(derived.source_event_id, None);
        assert_eq!(derived.source, "ai_lifecycle");
    }

    #[test]
    fn commandless_custom_adapters_are_rejected() {
        let registry = AgentRegistry;

        let commandless_unknown = registry.build_launch_spec(&session("unknown", ""));
        assert!(
            commandless_unknown
                .unwrap_err()
                .to_string()
                .contains("agent requires command: unknown")
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
    fn command_only_adapters_reject_tool_materialization() {
        let registry = AgentRegistry;
        let shell = session("shell", "cat");
        assert!(
            registry
                .apply_mcp(&shell, &[])
                .unwrap_err()
                .to_string()
                .contains("agent does not support MCP: shell")
        );
        assert!(
            registry
                .apply_skills(&shell, &[])
                .unwrap_err()
                .to_string()
                .contains("agent does not support skills: shell")
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
            .fork(&parent, &fork_request(&parent, false))
            .unwrap();
        assert_eq!(plan.command, "codex");
        assert!(!plan.inherits_conversation);

        let carry_plan = registry
            .fork(&parent, &fork_request(&parent, true))
            .unwrap();
        assert_eq!(carry_plan.command, "codex --resume");
        assert!(carry_plan.inherits_conversation);
    }

    #[test]
    fn command_only_forks_keep_command_without_inheriting_conversation() {
        let registry = AgentRegistry;

        for (agent, command) in [("shell", "bash -lc 'echo ok'"), ("custom", "custom-agent")] {
            let parent = session(agent, command);
            let plan = registry
                .fork(&parent, &fork_request(&parent, true))
                .unwrap();

            assert_eq!(plan.command, command);
            assert!(!plan.inherits_conversation);
        }
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
