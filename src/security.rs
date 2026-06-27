use crate::{
    error::{AppError, Result},
    models::ProjectTrustState,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

const REDACTED: &str = "[REDACTED]";
const SECRET_KEYS: &[&str] = &[
    "authorization",
    "client_secret",
    "private_key",
    "private-key",
    "secret_key",
    "access_key",
    "access-key",
    "api_key",
    "api-key",
    "apikey",
    "password",
    "passwd",
    "secret",
    "token",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustGatedOperation {
    Hooks,
    Mcp,
    Skills,
    Watchers,
}

impl TrustGatedOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hooks => "hooks",
            Self::Mcp => "mcp",
            Self::Skills => "skills",
            Self::Watchers => "watchers",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPathValidation {
    pub path: PathBuf,
    pub allowed_paths: Vec<PathBuf>,
}

pub fn project_trust_allows(trust_state: ProjectTrustState) -> bool {
    trust_state == ProjectTrustState::Trusted
}

pub fn require_project_trust(
    trust_state: ProjectTrustState,
    operation: TrustGatedOperation,
) -> Result<()> {
    if project_trust_allows(trust_state) {
        Ok(())
    } else {
        Err(AppError::msg(format!(
            "project must be trusted before running {}",
            operation.as_str()
        )))
    }
}

pub fn validate_sandbox_allowed_path<P, A, I>(
    path: P,
    allowed_paths: I,
) -> Result<SandboxPathValidation>
where
    P: AsRef<Path>,
    A: AsRef<Path>,
    I: IntoIterator<Item = A>,
{
    let path = existing_path(path.as_ref())?;
    let allowed_paths = allowed_paths
        .into_iter()
        .map(|allowed| existing_path(allowed.as_ref()))
        .collect::<Result<Vec<_>>>()?;

    if allowed_paths.is_empty()
        || allowed_paths
            .iter()
            .any(|allowed| path.starts_with(allowed))
    {
        Ok(SandboxPathValidation {
            path,
            allowed_paths,
        })
    } else {
        Err(AppError::msg(format!(
            "sandbox path is outside allowed paths: {}",
            path.display()
        )))
    }
}

pub fn redact_event_payload(payload: &str) -> String {
    let mut redacted = String::with_capacity(payload.len());
    let mut index = 0;

    while index < payload.len() {
        if let Some((value_start, value_end)) = bearer_value_at(payload, index) {
            redacted.push_str(&payload[index..value_start]);
            redacted.push_str(REDACTED);
            index = value_end;
        } else if let Some((value_start, value_end)) = secret_value_at(payload, index) {
            redacted.push_str(&payload[index..value_start]);
            redacted.push_str(REDACTED);
            index = value_end;
        } else {
            let ch = payload[index..].chars().next().expect("index is in bounds");
            redacted.push(ch);
            index += ch.len_utf8();
        }
    }

    redacted
}

pub fn redact_json_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_event_payload(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_json_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, redact_json_value(value)))
                .collect(),
        ),
        value => value,
    }
}

fn existing_path(path: &Path) -> Result<PathBuf> {
    crate::util::existing_path(path)
}

fn bearer_value_at(input: &str, index: usize) -> Option<(usize, usize)> {
    let bearer = "bearer";
    if !starts_with_ignore_ascii_case(input, index, bearer) || !boundary_before(input, index) {
        return None;
    }

    let mut value_start = index + bearer.len();
    if !input[value_start..].chars().next()?.is_ascii_whitespace() {
        return None;
    }
    value_start = skip_whitespace(input, value_start);
    let value_end = unquoted_value_end(input, value_start);
    (value_start < value_end).then_some((value_start, value_end))
}

fn secret_value_at(input: &str, index: usize) -> Option<(usize, usize)> {
    let (key_start, key_quote) = match input[index..].chars().next()? {
        '"' | '\'' if boundary_before(input, index) => {
            (index + 1, Some(input[index..].chars().next()?))
        }
        _ if boundary_before(input, index) => (index, None),
        _ => return None,
    };

    for key in SECRET_KEYS {
        if !starts_with_ignore_ascii_case(input, key_start, key) {
            continue;
        }

        let mut cursor = key_start + key.len();
        if let Some(quote) = key_quote {
            if input[cursor..].chars().next()? != quote {
                continue;
            }
            cursor += quote.len_utf8();
        }

        cursor = skip_whitespace(input, cursor);
        if !matches!(input[cursor..].chars().next(), Some(':' | '=')) {
            continue;
        }
        cursor += 1;
        cursor = skip_whitespace(input, cursor);

        let (value_start, value_end) = if matches!(input[cursor..].chars().next(), Some('"' | '\''))
        {
            let quote = input[cursor..].chars().next()?;
            let value_start = cursor + quote.len_utf8();
            (value_start, quoted_value_end(input, value_start, quote))
        } else {
            (cursor, unquoted_value_end(input, cursor))
        };

        if key.eq_ignore_ascii_case("authorization")
            && let Some(range) = bearer_value_at(input, value_start)
        {
            return Some(range);
        }

        if value_start < value_end {
            return Some((value_start, value_end));
        }
    }

    None
}

fn starts_with_ignore_ascii_case(input: &str, index: usize, needle: &str) -> bool {
    input
        .get(index..index + needle.len())
        .is_some_and(|value| value.eq_ignore_ascii_case(needle))
}

fn boundary_before(input: &str, index: usize) -> bool {
    input[..index]
        .chars()
        .next_back()
        .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
}

fn skip_whitespace(input: &str, mut index: usize) -> usize {
    while let Some(ch) = input[index..].chars().next() {
        if !ch.is_ascii_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn quoted_value_end(input: &str, start: usize, quote: char) -> usize {
    let mut index = start;
    while let Some(ch) = input[index..].chars().next() {
        if ch == quote {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn unquoted_value_end(input: &str, start: usize) -> usize {
    let mut index = start;
    while let Some(ch) = input[index..].chars().next() {
        if ch.is_ascii_whitespace() || matches!(ch, ',' | '}' | ']') {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn trust_gate_rejects_untrusted_project() {
        assert!(
            require_project_trust(ProjectTrustState::Trusted, TrustGatedOperation::Hooks).is_ok()
        );

        let error =
            require_project_trust(ProjectTrustState::Untrusted, TrustGatedOperation::Watchers)
                .unwrap_err()
                .to_string();

        assert_eq!(error, "project must be trusted before running watchers");
    }

    #[test]
    fn sandbox_path_validation_uses_canonical_paths() -> Result<()> {
        let temp = tempdir()?;
        let allowed = temp.path().join("allowed");
        let project = allowed.join("project");
        std::fs::create_dir_all(&project)?;
        let alias = allowed.join("..").join("allowed").join("project");

        let validation = validate_sandbox_allowed_path(alias, [&allowed])?;

        assert_eq!(validation.path, project.canonicalize()?);
        assert_eq!(validation.allowed_paths, vec![allowed.canonicalize()?]);
        Ok(())
    }

    #[test]
    fn sandbox_path_validation_rejects_paths_outside_allowed_roots() -> Result<()> {
        let temp = tempdir()?;
        let allowed = temp.path().join("allowed");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&allowed)?;
        std::fs::create_dir_all(&project)?;

        let error = validate_sandbox_allowed_path(&project, [&allowed])
            .unwrap_err()
            .to_string();

        assert!(error.starts_with("sandbox path is outside allowed paths: "));
        Ok(())
    }

    #[test]
    fn redacts_obvious_event_payload_secrets() {
        let payload =
            r#"token=abc "api_key":"sk-test" Authorization: Bearer bearer-token normal=ok"#;

        assert_eq!(
            redact_event_payload(payload),
            r#"token=[REDACTED] "api_key":"[REDACTED]" Authorization: Bearer [REDACTED] normal=ok"#
        );
    }

    #[test]
    fn redacts_json_string_values() {
        let payload = serde_json::json!({
            "source": "token=abc",
            "nested": {"header": "Authorization: Bearer secret"},
            "ok": 1
        });
        let redacted = redact_json_value(payload);
        assert_eq!(redacted["source"], "token=[REDACTED]");
        assert_eq!(
            redacted["nested"]["header"],
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(redacted["ok"], 1);
    }
}
