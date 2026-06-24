use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub profile: String,
    pub data_dir: PathBuf,
    pub default_agent: String,
    pub default_group: String,
    pub web_listen: String,
    pub web_read_only: bool,
    pub web_token_env: String,
    pub sandbox_allowed_paths: Vec<PathBuf>,
    pub sandbox_image: String,
}

impl AppConfig {
    pub fn load(profile: &str) -> Result<Self> {
        let mut config = Self::built_in(profile);
        let global = home_dir()?.join(".config/agent-helm/config.toml");
        if global.exists() {
            config.merge_file(&global)?;
        }
        let profile_file = config.data_dir.join("config.toml");
        if profile_file.exists() {
            config.merge_file(&profile_file)?;
        }
        Ok(config)
    }

    pub fn built_in(profile: &str) -> Self {
        Self {
            profile: profile.to_string(),
            data_dir: home_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local/share/agent-helm")
                .join(profile),
            default_agent: "shell".to_string(),
            default_group: "default".to_string(),
            web_listen: "127.0.0.1:8420".to_string(),
            web_read_only: false,
            web_token_env: "AGENT_HELM_WEB_TOKEN".to_string(),
            sandbox_allowed_paths: Vec::new(),
            sandbox_image: "alpine:latest".to_string(),
        }
    }

    pub fn state_db(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }

    fn merge_file(&mut self, path: &PathBuf) -> Result<()> {
        let raw = fs::read_to_string(path)?;
        let file: FileConfig = toml::from_str(&raw)?;
        if let Some(data_dir) = file.data_dir.as_deref() {
            self.data_dir = expand_home(data_dir)?;
        }
        if let Some(data_dir) = file
            .profiles
            .as_ref()
            .and_then(|profiles| profiles.get(&self.profile))
            .and_then(|profile| profile.data_dir.as_deref())
        {
            self.data_dir = expand_home(data_dir)?;
        }
        if let Some(defaults) = file.session_defaults {
            if let Some(agent) = defaults.agent {
                self.default_agent = agent;
            }
            if let Some(group) = defaults.group {
                self.default_group = group;
            }
        }
        if let Some(agent) = file.default_agent {
            self.default_agent = agent;
        }
        if let Some(group) = file.default_group {
            self.default_group = group;
        }
        if let Some(web) = file.web {
            if let Some(listen) = web.listen {
                self.web_listen = listen;
            }
            if let Some(read_only) = web.read_only {
                self.web_read_only = read_only;
            }
            if let Some(auth) = web.auth
                && let Some(token_env) = auth.token_env
            {
                self.web_token_env = token_env;
            }
        }
        if let Some(sandboxing) = file.sandboxing {
            if let Some(paths) = sandboxing.allowed_paths {
                self.sandbox_allowed_paths = paths
                    .iter()
                    .map(|path| expand_home(path))
                    .collect::<Result<Vec<_>>>()?;
            }
            if let Some(image) = sandboxing.image {
                self.sandbox_image = image;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    data_dir: Option<String>,
    profiles: Option<HashMap<String, ProfileConfig>>,
    default_agent: Option<String>,
    default_group: Option<String>,
    session_defaults: Option<SessionDefaultsConfig>,
    web: Option<WebConfig>,
    sandboxing: Option<SandboxingConfig>,
}

#[derive(Debug, Deserialize)]
struct ProfileConfig {
    data_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionDefaultsConfig {
    agent: Option<String>,
    group: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebConfig {
    listen: Option<String>,
    read_only: Option<bool>,
    auth: Option<WebAuthConfig>,
}

#[derive(Debug, Deserialize)]
struct WebAuthConfig {
    token_env: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SandboxingConfig {
    allowed_paths: Option<Vec<String>>,
    image: Option<String>,
}

pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| AppError::msg("HOME is not set"))
}

pub fn expand_home(value: &str) -> Result<PathBuf> {
    if let Some(rest) = value.strip_prefix("~/") {
        Ok(home_dir()?.join(rest))
    } else {
        Ok(PathBuf::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architecture_config_shape_merges_profile_and_session_defaults() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let data_dir = tmp.path().join("state");
        let sandbox_dir = tmp.path().join("sandbox");
        fs::create_dir(&sandbox_dir)?;
        let config_file = tmp.path().join("config.toml");
        fs::write(
            &config_file,
            format!(
                r#"
[profiles.test]
data_dir = "{}"

[session_defaults]
agent = "codex"
group = "work"

[web]
listen = "127.0.0.1:9999"
read_only = true

[web.auth]
token_env = "TOKEN_ENV"

[sandboxing]
allowed_paths = ["{}"]
image = "ubuntu:24.04"
"#,
                data_dir.display(),
                sandbox_dir.display()
            ),
        )?;

        let mut config = AppConfig::built_in("test");
        config.merge_file(&config_file)?;

        assert_eq!(config.data_dir, data_dir);
        assert_eq!(config.default_agent, "codex");
        assert_eq!(config.default_group, "work");
        assert_eq!(config.web_listen, "127.0.0.1:9999");
        assert!(config.web_read_only);
        assert_eq!(config.web_token_env, "TOKEN_ENV");
        assert_eq!(config.sandbox_allowed_paths, vec![sandbox_dir]);
        assert_eq!(config.sandbox_image, "ubuntu:24.04");
        Ok(())
    }
}
