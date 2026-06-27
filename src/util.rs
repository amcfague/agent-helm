use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};

/// Shell-quote a value for safe embedding in sh commands.
/// Values containing only safe characters that don't start with `-` are
/// returned unquoted. Values starting with `-` are always quoted to prevent
/// flag injection.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Expand a leading `~` or `~/` to the user's home directory.
pub fn expand_home(path: &Path) -> Result<PathBuf> {
    let raw = path.as_os_str().to_string_lossy();
    if raw == "~" {
        home_dir()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        Ok(home_dir()?.join(rest))
    } else {
        Ok(path.to_path_buf())
    }
}

/// Like [`expand_home`] but returns the original path instead of an error
/// when HOME is unset.
fn expand_home_lossy(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    if raw == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf())
    } else if let Some(rest) = raw.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

/// Resolve a path: expand `~`, verify it exists, and canonicalize.
pub fn existing_path(path: &Path) -> Result<PathBuf> {
    let path = expand_home_lossy(path);
    if !path.exists() {
        return Err(AppError::msg(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    Ok(path.canonicalize()?)
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::msg("HOME is not set"))
}

/// Extract the last component of a path as a project name.
pub fn project_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("session")
        .to_string()
}

/// Generate a worktree branch name from agent, session name, and path.
pub fn auto_worktree_branch(agent: &str, name: &str, path: &str) -> String {
    let base = if name.trim().is_empty() {
        project_name(path)
    } else {
        name.to_string()
    };
    format!(
        "agent-helm/{}/{}-{}",
        branch_fragment(agent),
        branch_fragment(&base),
        uuid::Uuid::new_v4().simple()
    )
}

/// Normalize a value into a safe git branch fragment.
pub fn branch_fragment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "session".to_string()
    } else {
        out.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_passes_safe_values_through() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("a/b-c_d.e"), "a/b-c_d.e");
    }

    #[test]
    fn shell_quote_wraps_special_chars() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_always_quotes_dash_prefixed_values() {
        assert_eq!(shell_quote("--flag"), "'--flag'");
        assert_eq!(shell_quote("-x"), "'-x'");
        // Interior dashes are fine
        assert_eq!(shell_quote("a-b"), "a-b");
    }

    #[test]
    fn project_name_extracts_last_component() {
        assert_eq!(project_name("/tmp/my-repo"), "my-repo");
        assert_eq!(project_name(""), "session");
    }

    #[test]
    fn branch_fragment_normalizes() {
        assert_eq!(branch_fragment("Hello World"), "hello-world");
        assert_eq!(branch_fragment(""), "session");
    }
}
