use crate::{
    error::{AppError, Result},
    models::{LaunchSpec, OutputPage, RuntimeHandle, SandboxLaunchSpec, SessionStatus},
    security::validate_sandbox_allowed_path,
};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

const TMUX_DETACH_KEY: &str = "C-q";
const TMUX_SESSION_OPTIONS: &[(&str, &str)] =
    &[("status", "off"), ("prefix", "None"), ("prefix2", "None")];
const TMUX_WINDOW_OPTIONS: &[(&str, &str)] = &[("pane-border-status", "off")];

pub trait SessionRuntime: Clone + Send + Sync + 'static {
    fn start(&self, session_id: &str, spec: &LaunchSpec) -> Result<RuntimeHandle>;
    fn attach(&self, session_id: &str) -> Result<()>;
    fn send(&self, session_id: &str, text: &str) -> Result<()>;
    fn capture(&self, session_id: &str, limit: usize, ansi: bool) -> Result<OutputPage>;
    fn status(&self, session_id: &str) -> Result<SessionStatus>;
    fn stop(&self, session_id: &str) -> Result<()>;
    fn restart(&self, session_id: &str, spec: &LaunchSpec) -> Result<RuntimeHandle>;
    fn destroy(&self, session_id: &str) -> Result<()>;
}

#[derive(Debug, Clone, Default)]
pub struct FakeRuntime {
    sessions: Arc<Mutex<HashMap<String, FakeSession>>>,
    fail_start: bool,
    fail_destroy: bool,
}

#[derive(Debug, Clone)]
struct FakeSession {
    status: SessionStatus,
    output: Vec<String>,
}

impl SessionRuntime for FakeRuntime {
    fn start(&self, session_id: &str, spec: &LaunchSpec) -> Result<RuntimeHandle> {
        if self.fail_start {
            return Err(AppError::msg("fake runtime start failed"));
        }
        let handle_id = fake_handle_id(session_id);
        let session = FakeSession {
            status: SessionStatus::Running,
            output: Vec::new(),
        };
        let _ = spec;
        self.sessions
            .lock()
            .map_err(|_| AppError::msg("fake runtime lock poisoned"))?
            .insert(session_id.to_owned(), session);
        Ok(RuntimeHandle { id: handle_id })
    }

    fn attach(&self, session_id: &str) -> Result<()> {
        self.with_session(session_id, |_| Ok(()))
    }

    fn send(&self, session_id: &str, text: &str) -> Result<()> {
        self.with_session_mut(session_id, |session| {
            if session.status != SessionStatus::Running {
                return Err(AppError::msg("session is not running"));
            }
            push_output(&mut session.output, text);
            Ok(())
        })
    }

    fn capture(&self, session_id: &str, limit: usize, _ansi: bool) -> Result<OutputPage> {
        self.with_session(session_id, |session| {
            Ok(OutputPage {
                text: last_lines(&session.output, limit),
            })
        })
    }

    fn status(&self, session_id: &str) -> Result<SessionStatus> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| AppError::msg("fake runtime lock poisoned"))?;
        Ok(sessions
            .get(session_id)
            .map(|session| session.status)
            .unwrap_or(SessionStatus::Stopped))
    }

    fn stop(&self, session_id: &str) -> Result<()> {
        self.with_session_mut(session_id, |session| {
            session.status = SessionStatus::Stopped;
            Ok(())
        })
    }

    fn restart(&self, session_id: &str, spec: &LaunchSpec) -> Result<RuntimeHandle> {
        self.start(session_id, spec)
    }

    fn destroy(&self, session_id: &str) -> Result<()> {
        if self.fail_destroy {
            return Err(AppError::msg("fake runtime destroy failed"));
        }
        self.sessions
            .lock()
            .map_err(|_| AppError::msg("fake runtime lock poisoned"))?
            .remove(session_id);
        Ok(())
    }
}

impl FakeRuntime {
    pub fn failing_start() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            fail_start: true,
            fail_destroy: false,
        }
    }

    pub fn failing_destroy() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            fail_start: false,
            fail_destroy: true,
        }
    }
}

impl FakeRuntime {
    fn with_session<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&FakeSession) -> Result<T>,
    ) -> Result<T> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| AppError::msg("fake runtime lock poisoned"))?;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| AppError::msg(format!("session not found: {session_id}")))?;
        f(session)
    }

    fn with_session_mut<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut FakeSession) -> Result<T>,
    ) -> Result<T> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| AppError::msg("fake runtime lock poisoned"))?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| AppError::msg(format!("session not found: {session_id}")))?;
        f(session)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TmuxRuntime;

impl SessionRuntime for TmuxRuntime {
    fn start(&self, session_id: &str, spec: &LaunchSpec) -> Result<RuntimeHandle> {
        let tmux_id = tmux_session_name(session_id);
        let launch_command = tmux_launch_command(&tmux_id, spec)?;
        let mut command = tmux();
        command
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&tmux_id)
            .arg("-c")
            .arg(&spec.cwd)
            .arg("sh")
            .arg("-lc")
            .arg(&launch_command);
        run_tmux(command)?;
        if let Err(err) = configure_tmux_viewport(&tmux_id) {
            let _ = kill_tmux_session(session_id);
            return Err(err);
        }
        Ok(RuntimeHandle { id: tmux_id })
    }

    fn attach(&self, session_id: &str) -> Result<()> {
        let hotkey = TmuxDetachHotkey::install()?;
        let mut command = tmux();
        command
            .arg("attach-session")
            .arg("-t")
            .arg(tmux_session_name(session_id));
        let attach_result = run_tmux_status(command);
        let restore_result = hotkey.restore();
        match (attach_result, restore_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(err), Ok(())) | (Ok(()), Err(err)) => Err(err),
            (Err(attach_err), Err(restore_err)) => Err(AppError::msg(format!(
                "attach failed: {attach_err}; restore binding also failed: {restore_err}"
            ))),
        }
    }

    fn send(&self, session_id: &str, text: &str) -> Result<()> {
        let mut command = tmux();
        command
            .arg("send-keys")
            .arg("-t")
            .arg(tmux_session_name(session_id))
            .arg("-l")
            .arg("--")
            .arg(text);
        run_tmux(command)?;

        let mut enter = tmux();
        enter
            .arg("send-keys")
            .arg("-t")
            .arg(tmux_session_name(session_id))
            .arg("Enter");
        run_tmux(enter)
    }

    fn capture(&self, session_id: &str, limit: usize, ansi: bool) -> Result<OutputPage> {
        if limit == 0 {
            return Ok(OutputPage {
                text: String::new(),
            });
        }

        let mut command = tmux();
        command
            .arg("capture-pane")
            .arg("-p")
            .arg("-t")
            .arg(tmux_session_name(session_id))
            .arg("-S")
            .arg(format!("-{limit}"));
        if ansi {
            command.arg("-e");
        }

        let output = run_tmux_output(command)?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(OutputPage {
            text: last_text_lines(text.trim_end_matches('\n'), limit),
        })
    }

    fn status(&self, session_id: &str) -> Result<SessionStatus> {
        let mut command = tmux();
        command
            .arg("has-session")
            .arg("-t")
            .arg(tmux_session_name(session_id));
        let output = command.output()?;
        if output.status.success() {
            Ok(SessionStatus::Running)
        } else {
            Ok(SessionStatus::Stopped)
        }
    }

    fn stop(&self, session_id: &str) -> Result<()> {
        kill_tmux_session(session_id)
    }

    fn restart(&self, session_id: &str, spec: &LaunchSpec) -> Result<RuntimeHandle> {
        if self.status(session_id)? == SessionStatus::Running {
            self.stop(session_id)?;
        }
        self.start(session_id, spec)
    }

    fn destroy(&self, session_id: &str) -> Result<()> {
        kill_tmux_session(session_id)
    }
}

fn fake_handle_id(session_id: &str) -> String {
    format!("fake-{}", safe_session_fragment(session_id))
}

fn tmux_session_name(session_id: &str) -> String {
    format!("agent-helm-{}", safe_session_fragment(session_id))
}

fn safe_session_fragment(session_id: &str) -> String {
    let mut out = String::new();
    for byte in session_id.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => out.push(byte as char),
            other => out.push_str(&format!("_{other:02x}")),
        }
    }

    if out.is_empty() {
        "empty".to_owned()
    } else {
        out
    }
}

fn tmux_launch_command(tmux_id: &str, spec: &LaunchSpec) -> Result<String> {
    if let Some(sandbox) = &spec.sandbox {
        let allowed_paths = sandbox
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        validate_sandbox_allowed_path(&spec.cwd, &allowed_paths)?;
        if !docker_available() {
            return Err(AppError::msg("docker is required for sandbox sessions"));
        }
        return Ok(docker_launch_command(tmux_id, spec, sandbox));
    }
    Ok(spec.command.clone())
}

fn docker_launch_command(tmux_id: &str, spec: &LaunchSpec, sandbox: &SandboxLaunchSpec) -> String {
    format!(
        "docker run --rm -it --name {} -v {} -w {} {} sh -lc {}",
        shell_quote(&format!("{tmux_id}-sandbox")),
        shell_quote(&format!("{}:{}", spec.cwd, sandbox.container_cwd)),
        shell_quote(&sandbox.container_cwd),
        shell_quote(&sandbox.image),
        shell_quote(&spec.command),
    )
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn shell_quote(value: &str) -> String {
    crate::util::shell_quote(value)
}

fn tmux() -> Command {
    Command::new("tmux")
}

fn configure_tmux_viewport(tmux_id: &str) -> Result<()> {
    for (option, value) in TMUX_SESSION_OPTIONS {
        let mut command = tmux();
        command
            .arg("set-option")
            .arg("-t")
            .arg(tmux_id)
            .arg(option)
            .arg(value);
        run_tmux(command)?;
    }
    for (option, value) in TMUX_WINDOW_OPTIONS {
        let mut command = tmux();
        command
            .arg("set-window-option")
            .arg("-t")
            .arg(tmux_id)
            .arg(option)
            .arg(value);
        run_tmux(command)?;
    }
    Ok(())
}

#[derive(Debug)]
struct TmuxDetachHotkey {
    previous: Option<String>,
}

impl TmuxDetachHotkey {
    fn install() -> Result<Self> {
        let previous = tmux_root_binding(TMUX_DETACH_KEY)?;
        bind_tmux_detach_key(TMUX_DETACH_KEY)?;
        Ok(Self { previous })
    }

    fn restore(self) -> Result<()> {
        restore_tmux_root_binding(TMUX_DETACH_KEY, self.previous.as_deref())
    }
}

fn tmux_root_binding(key: &str) -> Result<Option<String>> {
    let mut command = tmux();
    command.arg("list-keys").arg("-T").arg("root").arg(key);
    let output = command.output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_tmux_binding(&output.stdout))
}

fn parse_tmux_binding(output: &[u8]) -> Option<String> {
    let binding = String::from_utf8_lossy(output).trim().to_string();
    (!binding.is_empty()).then_some(binding)
}

fn bind_tmux_detach_key(key: &str) -> Result<()> {
    let mut command = tmux();
    command
        .arg("bind-key")
        .arg("-T")
        .arg("root")
        .arg(key)
        .arg("detach-client");
    run_tmux(command)
}

fn restore_tmux_root_binding(key: &str, previous: Option<&str>) -> Result<()> {
    match previous {
        Some(binding) => source_tmux_command(binding),
        None => {
            let mut command = tmux();
            command
                .arg("unbind-key")
                .arg("-q")
                .arg("-T")
                .arg("root")
                .arg(key);
            run_tmux(command)
        }
    }
}

fn source_tmux_command(command_text: &str) -> Result<()> {
    let mut child = tmux()
        .arg("source-file")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| AppError::msg("tmux source stdin unavailable"))?;
        stdin.write_all(command_text.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    drop(child.stdin.take());
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::msg(format!("tmux exited status {status}")))
    }
}

fn kill_tmux_session(session_id: &str) -> Result<()> {
    let mut status = tmux();
    status
        .arg("has-session")
        .arg("-t")
        .arg(tmux_session_name(session_id));
    if !status.output()?.status.success() {
        return Ok(());
    }

    let mut command = tmux();
    command
        .arg("kill-session")
        .arg("-t")
        .arg(tmux_session_name(session_id));
    run_tmux(command)
}

fn run_tmux_status(mut command: Command) -> Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::msg(format!("tmux exited with status {status}")))
    }
}

fn run_tmux(command: Command) -> Result<()> {
    run_tmux_output(command).map(|_| ())
}

fn run_tmux_output(mut command: Command) -> Result<Output> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(tmux_error(&output))
    }
}

fn tmux_error(output: &Output) -> AppError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };

    if detail.is_empty() {
        AppError::msg(format!("tmux exited with status {}", output.status))
    } else {
        AppError::msg(format!(
            "tmux exited with status {}: {detail}",
            output.status
        ))
    }
}

fn push_output(output: &mut Vec<String>, text: &str) {
    if text.is_empty() {
        output.push(String::new());
        return;
    }

    output.extend(text.lines().map(str::to_owned));
}

fn last_lines(lines: &[String], limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }

    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

fn last_text_lines(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }

    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    let mut page = lines[start..].join("\n");
    if text.ends_with('\n') && !page.is_empty() {
        page.push('\n');
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> LaunchSpec {
        LaunchSpec {
            cwd: ".".to_owned(),
            command: "cat".to_owned(),
            sandbox: None,
        }
    }

    #[test]
    fn fake_runtime_tracks_lifecycle_and_output() {
        let runtime = FakeRuntime::default();

        let handle = runtime.start("session 1", &spec()).unwrap();
        assert_eq!(handle.id, "fake-session_201");
        assert_eq!(runtime.status("session 1").unwrap(), SessionStatus::Running);

        runtime.send("session 1", "first\nsecond\nthird").unwrap();
        assert_eq!(
            runtime.capture("session 1", 2, false).unwrap().text,
            "second\nthird"
        );

        runtime.stop("session 1").unwrap();
        assert_eq!(runtime.status("session 1").unwrap(), SessionStatus::Stopped);

        runtime.restart("session 1", &spec()).unwrap();
        assert_eq!(runtime.status("session 1").unwrap(), SessionStatus::Running);

        runtime.destroy("session 1").unwrap();
        assert_eq!(runtime.status("session 1").unwrap(), SessionStatus::Stopped);
    }

    #[test]
    fn tmux_session_names_escape_unsafe_bytes() {
        assert_eq!(
            tmux_session_name("abc:def/ghi"),
            "agent-helm-abc_3adef_2fghi"
        );
        assert_eq!(tmux_session_name(""), "agent-helm-empty");
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn tmux_viewport_hides_chrome_and_uses_ctrl_q_escape() {
        assert_eq!(TMUX_DETACH_KEY, "C-q");
        assert!(TMUX_SESSION_OPTIONS.contains(&("status", "off")));
        assert!(TMUX_SESSION_OPTIONS.contains(&("prefix", "None")));
        assert!(TMUX_SESSION_OPTIONS.contains(&("prefix2", "None")));
        assert!(TMUX_WINDOW_OPTIONS.contains(&("pane-border-status", "off")));
    }

    #[test]
    fn tmux_binding_parser_ignores_empty_output() {
        assert_eq!(parse_tmux_binding(b"\n"), None);
        assert_eq!(
            parse_tmux_binding(b"bind-key -T root C-q detach-client\n"),
            Some("bind-key -T root C-q detach-client".to_string())
        );
    }

    #[test]
    fn docker_launch_command_mounts_workspace_and_quotes_command() {
        let spec = LaunchSpec {
            cwd: "/tmp/project".to_string(),
            command: "printf 'hello world'".to_string(),
            sandbox: Some(SandboxLaunchSpec {
                image: "alpine:latest".to_string(),
                container_cwd: "/workspace".to_string(),
                allowed_paths: vec!["/tmp".to_string()],
            }),
        };
        let sandbox = spec.sandbox.as_ref().unwrap();
        let command = docker_launch_command("agent-helm-test", &spec, sandbox);
        assert!(command.contains("docker run --rm -it"));
        assert!(command.contains("--name agent-helm-test-sandbox"));
        assert!(command.contains("-v '/tmp/project:/workspace'"));
        assert!(command.contains("-w /workspace"));
        assert!(command.contains("'alpine:latest'"));
        assert!(command.contains("sh -lc 'printf '\\''hello world'\\'''"));
    }

    #[test]
    fn sandbox_launch_command_revalidates_allowed_paths() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            cwd: outside.path().to_string_lossy().to_string(),
            command: "cat".to_string(),
            sandbox: Some(SandboxLaunchSpec {
                image: "alpine:latest".to_string(),
                container_cwd: "/workspace".to_string(),
                allowed_paths: vec![allowed.path().to_string_lossy().to_string()],
            }),
        };
        let err = tmux_launch_command("agent-helm-test", &spec)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sandbox path is outside allowed paths"));
    }

    #[test]
    fn tmux_runtime_smoke_test() {
        if std::env::var_os("AGENT_HELM_TMUX_SMOKE").is_none() {
            return;
        }

        let tempdir = tempfile::tempdir().unwrap();
        let runtime = TmuxRuntime;
        let session_id = format!("smoke-{}", std::process::id());
        let spec = LaunchSpec {
            cwd: tempdir.path().to_string_lossy().to_string(),
            command: "cat".to_owned(),
            sandbox: None,
        };

        runtime.start(&session_id, &spec).unwrap();
        runtime.send(&session_id, "hello").unwrap();
        let output = runtime.capture(&session_id, 20, false).unwrap();
        runtime.destroy(&session_id).unwrap();

        assert!(output.text.contains("hello"));
    }
}
