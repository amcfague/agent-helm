use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub profile: String,
    pub data_dir: PathBuf,
    pub default_agent: String,
    pub default_group: String,
    pub tools: HashMap<String, ToolProfile>,
    pub web_listen: String,
    pub web_read_only: bool,
    pub web_token_env: String,
    pub headroom_proxy_savings_path: PathBuf,
    pub sandbox_allowed_paths: Vec<PathBuf>,
    pub sandbox_image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ToolProfile {
    #[serde(default = "default_true")]
    pub installed: bool,
    pub executable: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub worktree: ToolWorktreeBehavior,
}

use strum::IntoStaticStr;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, IntoStaticStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ToolWorktreeBehavior {
    Always,
    #[default]
    Manual,
    Never,
}

impl ToolWorktreeBehavior {
    pub fn creates_worktree_by_default(self) -> bool {
        matches!(self, Self::Always)
    }

    pub fn as_str(self) -> &'static str {
        self.into()
    }
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
        let settings = settings_file_path()?;
        if settings.exists() {
            config.merge_file(&settings)?;
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
            tools: built_in_tools(),
            web_listen: "127.0.0.1:8420".to_string(),
            web_read_only: false,
            web_token_env: "AGENT_HELM_WEB_TOKEN".to_string(),
            headroom_proxy_savings_path: home_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".headroom/proxy_savings.json"),
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
        merge_tools(&mut self.tools, file.tools);
        if let Some(tools) = file
            .profiles
            .as_ref()
            .and_then(|profiles| profiles.get(&self.profile))
            .and_then(|profile| profile.tools.clone())
        {
            merge_tools(&mut self.tools, Some(tools));
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
        if let Some(headroom) = file.headroom
            && let Some(path) = headroom.proxy_savings_path
        {
            self.headroom_proxy_savings_path = expand_home(&path)?;
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

    pub fn tool(&self, agent: &str) -> Option<&ToolProfile> {
        self.tools.get(agent)
    }

    pub fn resolve_tool_command(&self, agent: &str, explicit: &str) -> Result<String> {
        if !explicit.trim().is_empty() {
            return Ok(explicit.to_string());
        }
        let Some(tool) = self.tool(agent) else {
            return Ok(String::new());
        };
        tool.command(agent)
    }

    pub fn tool_worktree_behavior(&self, agent: &str) -> ToolWorktreeBehavior {
        self.tool(agent)
            .map(|tool| tool.worktree)
            .unwrap_or_default()
    }

    pub fn installed_tool_names(&self) -> Vec<String> {
        let mut names = self
            .tools
            .iter()
            .filter(|(_, tool)| tool.installed)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        names.sort();
        if let Some(shell_index) = names.iter().position(|name| name == "shell") {
            let shell = names.remove(shell_index);
            names.insert(0, shell);
        }
        names
    }

    pub fn save_tool_settings(&mut self, tools: HashMap<String, ToolProfile>) -> Result<PathBuf> {
        let path = settings_file_path()?;
        save_tool_settings_to_path(&path, &tools)?;
        self.tools = tools;
        Ok(path)
    }
}

impl ToolProfile {
    fn from_config(config: ToolProfileConfig) -> Self {
        let mut tool = Self {
            installed: true,
            executable: None,
            flags: Vec::new(),
            worktree: ToolWorktreeBehavior::default(),
        };
        tool.apply_config(config);
        tool
    }

    fn apply_config(&mut self, config: ToolProfileConfig) {
        if let Some(installed) = config.installed {
            self.installed = installed;
        }
        if let Some(executable) = config.executable {
            self.executable = Some(executable);
        }
        if let Some(flags) = config.flags {
            self.flags = flags;
        }
        if let Some(worktree) = config.worktree {
            self.worktree = worktree;
        }
    }

    fn command(&self, agent: &str) -> Result<String> {
        if !self.installed {
            return Err(AppError::msg(format!("tool is not installed: {agent}")));
        }
        let executable = self
            .executable
            .as_deref()
            .filter(|executable| !executable.trim().is_empty())
            .ok_or_else(|| AppError::msg(format!("tool requires executable: {agent}")))?;
        let mut parts = Vec::with_capacity(self.flags.len() + 1);
        parts.push(shell_quote(executable));
        parts.extend(self.flags.iter().map(|flag| shell_quote(flag)));
        Ok(parts.join(" "))
    }
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    data_dir: Option<String>,
    profiles: Option<HashMap<String, ProfileConfig>>,
    default_agent: Option<String>,
    default_group: Option<String>,
    tools: Option<HashMap<String, ToolProfileConfig>>,
    session_defaults: Option<SessionDefaultsConfig>,
    headroom: Option<HeadroomConfig>,
    web: Option<WebConfig>,
    sandboxing: Option<SandboxingConfig>,
}

#[derive(Debug, Serialize)]
struct ToolSettingsFile {
    tools: BTreeMap<String, ToolProfile>,
}

#[derive(Debug, Deserialize)]
struct ProfileConfig {
    data_dir: Option<String>,
    tools: Option<HashMap<String, ToolProfileConfig>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct ToolProfileConfig {
    installed: Option<bool>,
    executable: Option<String>,
    flags: Option<Vec<String>>,
    worktree: Option<ToolWorktreeBehavior>,
}

#[derive(Debug, Deserialize)]
struct SessionDefaultsConfig {
    agent: Option<String>,
    group: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HeadroomConfig {
    proxy_savings_path: Option<String>,
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
    crate::util::home_dir()
}

pub fn settings_file_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/agent-helm/settings.toml"))
}

pub fn expand_home(value: &str) -> Result<PathBuf> {
    crate::util::expand_home(Path::new(value))
}

fn default_true() -> bool {
    true
}

fn save_tool_settings_to_path(path: &Path, tools: &HashMap<String, ToolProfile>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = ToolSettingsFile {
        tools: tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect(),
    };
    fs::write(path, toml::to_string_pretty(&file)?)?;
    Ok(())
}

fn built_in_tools() -> HashMap<String, ToolProfile> {
    [
        (
            "shell",
            ToolProfile {
                installed: true,
                executable: Some(env::var("SHELL").unwrap_or_else(|_| "sh".to_string())),
                flags: Vec::new(),
                worktree: ToolWorktreeBehavior::Never,
            },
        ),
        (
            "claude",
            installed_tool("claude", ToolWorktreeBehavior::Always),
        ),
        (
            "codex",
            installed_tool("codex", ToolWorktreeBehavior::Always),
        ),
        (
            "gemini",
            installed_tool("gemini", ToolWorktreeBehavior::Always),
        ),
        (
            "opencode",
            installed_tool("opencode", ToolWorktreeBehavior::Always),
        ),
    ]
    .into_iter()
    .map(|(agent, tool)| (agent.to_string(), tool))
    .collect()
}

fn installed_tool(executable: &str, worktree: ToolWorktreeBehavior) -> ToolProfile {
    ToolProfile {
        installed: true,
        executable: Some(executable.to_string()),
        flags: Vec::new(),
        worktree,
    }
}

fn merge_tools(
    tools: &mut HashMap<String, ToolProfile>,
    overrides: Option<HashMap<String, ToolProfileConfig>>,
) {
    let Some(overrides) = overrides else {
        return;
    };

    for (agent, config) in overrides {
        if let Some(tool) = tools.get_mut(&agent) {
            tool.apply_config(config);
        } else {
            tools.insert(agent, ToolProfile::from_config(config));
        }
    }
}

fn shell_quote(value: &str) -> String {
    crate::util::shell_quote(value)
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

    #[test]
    fn headroom_proxy_savings_path_can_be_configured() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let savings_path = tmp.path().join("proxy_savings.json");
        let config_file = tmp.path().join("config.toml");
        fs::write(
            &config_file,
            format!(
                r#"
[headroom]
proxy_savings_path = "{}"
"#,
                savings_path.display()
            ),
        )?;

        let mut config = AppConfig::built_in("test");
        config.merge_file(&config_file)?;

        assert_eq!(config.headroom_proxy_savings_path, savings_path);
        Ok(())
    }

    #[test]
    fn built_in_tools_include_supported_agents() {
        let config = AppConfig::built_in("test");

        assert_eq!(
            config.tool("shell").map(|tool| tool.worktree),
            Some(ToolWorktreeBehavior::Never)
        );
        for agent in ["claude", "codex", "gemini", "opencode"] {
            let tool = config.tool(agent).expect("built-in tool exists");
            assert!(tool.installed);
            assert_eq!(tool.executable.as_deref(), Some(agent));
            assert_eq!(tool.worktree, ToolWorktreeBehavior::Always);
        }
    }

    #[test]
    fn installed_tool_names_include_custom_profiles_and_skip_disabled() {
        let mut config = AppConfig::built_in("test");
        config.tools.insert(
            "nightly".to_string(),
            ToolProfile {
                installed: true,
                executable: Some("codex-nightly".to_string()),
                flags: Vec::new(),
                worktree: ToolWorktreeBehavior::Manual,
            },
        );
        config.tools.insert(
            "disabled".to_string(),
            ToolProfile {
                installed: false,
                executable: Some("disabled".to_string()),
                flags: Vec::new(),
                worktree: ToolWorktreeBehavior::Manual,
            },
        );

        let names = config.installed_tool_names();

        assert_eq!(names.first().map(String::as_str), Some("shell"));
        assert!(names.iter().any(|name| name == "nightly"));
        assert!(!names.iter().any(|name| name == "disabled"));
    }

    #[test]
    fn tool_config_merges_global_and_profile_overrides() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let config_file = tmp.path().join("config.toml");
        fs::write(
            &config_file,
            r#"
            [tools.codex]
            flags = ["--sandbox", "workspace"]

            [tools.custom]
            executable = "/opt/custom tool"
            flags = ["--name", "two words"]
            worktree = "never"

            [profiles.test.tools.codex]
            executable = "codex-nightly"
            worktree = "manual"
            "#,
        )?;

        let mut config = AppConfig::built_in("test");
        config.merge_file(&config_file)?;

        let codex = config.tool("codex").expect("codex tool exists");
        assert!(codex.installed);
        assert_eq!(codex.executable.as_deref(), Some("codex-nightly"));
        assert_eq!(codex.flags, ["--sandbox", "workspace"]);
        assert_eq!(codex.worktree, ToolWorktreeBehavior::Manual);
        assert_eq!(
            config.resolve_tool_command("codex", "")?,
            "codex-nightly '--sandbox' workspace"
        );
        assert_eq!(
            config.resolve_tool_command("custom", "")?,
            "'/opt/custom tool' '--name' 'two words'"
        );
        assert_eq!(
            config.tool_worktree_behavior("custom"),
            ToolWorktreeBehavior::Never
        );
        Ok(())
    }

    #[test]
    fn tool_config_profile_overrides_only_selected_profile() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let config_file = tmp.path().join("config.toml");
        fs::write(
            &config_file,
            r#"
            [profiles.other.tools.codex]
            executable = "codex-other"
            "#,
        )?;

        let mut config = AppConfig::built_in("test");
        config.merge_file(&config_file)?;

        assert_eq!(
            config
                .tool("codex")
                .and_then(|tool| tool.executable.as_deref()),
            Some("codex")
        );
        Ok(())
    }

    #[test]
    fn resolve_tool_command_errors_when_tool_is_disabled() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let config_file = tmp.path().join("config.toml");
        fs::write(
            &config_file,
            r#"
            [tools.gemini]
            installed = false
            "#,
        )?;

        let mut config = AppConfig::built_in("test");
        config.merge_file(&config_file)?;

        let error = config.resolve_tool_command("gemini", "").unwrap_err();
        assert_eq!(error.to_string(), "tool is not installed: gemini");
        Ok(())
    }

    #[test]
    fn resolve_tool_command_prefers_explicit_command() -> Result<()> {
        let config = AppConfig::built_in("test");

        assert_eq!(
            config.resolve_tool_command("missing", "custom --flag")?,
            "custom --flag"
        );
        Ok(())
    }

    #[test]
    fn save_tool_settings_writes_structured_toml() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let settings_file = tmp.path().join("settings.toml");
        let mut tools = HashMap::new();
        tools.insert(
            "codex".to_string(),
            ToolProfile {
                installed: true,
                executable: Some("/opt/bin/codex-nightly".to_string()),
                flags: vec!["--profile".to_string(), "review".to_string()],
                worktree: ToolWorktreeBehavior::Manual,
            },
        );

        save_tool_settings_to_path(&settings_file, &tools)?;

        let raw = fs::read_to_string(&settings_file)?;
        assert!(raw.contains("[tools.codex]"));
        assert!(raw.contains("executable = \"/opt/bin/codex-nightly\""));
        assert!(raw.contains("flags = ["));
        assert!(raw.contains("\"--profile\""));
        assert!(raw.contains("\"review\""));
        assert!(raw.contains("worktree = \"manual\""));

        let mut config = AppConfig::built_in("test");
        config.merge_file(&settings_file)?;
        assert_eq!(
            config.resolve_tool_command("codex", "")?,
            "/opt/bin/codex-nightly '--profile' review"
        );
        Ok(())
    }
}
