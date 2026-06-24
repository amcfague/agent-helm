use crate::error::{AppError, Result};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Default)]
pub struct WorkspaceManager;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub root: PathBuf,
    pub repo_identity: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectHooks {
    pub setup: Vec<String>,
    pub teardown: Vec<String>,
}

impl WorkspaceManager {
    pub fn resolve_project(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self.resolve_project_ref(path)?.root)
    }

    pub fn resolve_project_ref(&self, path: impl AsRef<Path>) -> Result<ProjectRef> {
        let path = existing_path(path.as_ref())?;
        let command_path = command_path(&path);
        let root = git_optional(&command_path, ["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
            .map(|path| canonicalize_existing(&path))
            .transpose()?
            .unwrap_or(path);

        Ok(ProjectRef {
            repo_identity: repo_identity_for_root(&root),
            default_branch: default_branch_for_root(&root),
            root,
        })
    }

    pub fn repo_identity(&self, project: impl AsRef<Path>) -> Option<String> {
        let root = self.resolve_project(project).ok()?;
        repo_identity_for_root(&root)
    }

    pub fn default_branch(&self, project: impl AsRef<Path>) -> Option<String> {
        let root = self.resolve_project(project).ok()?;
        default_branch_for_root(&root)
    }

    pub fn create_worktree(
        &self,
        project: impl AsRef<Path>,
        branch: impl AsRef<str>,
    ) -> Result<WorktreeInfo> {
        self.create_worktree_with_state(project, branch, false)
    }

    pub fn create_worktree_with_state(
        &self,
        project: impl AsRef<Path>,
        branch: impl AsRef<str>,
        carry_state: bool,
    ) -> Result<WorktreeInfo> {
        let project = self.resolve_project_ref(project)?;
        let branch = branch.as_ref().trim();
        if branch.is_empty() {
            return Err(AppError::msg("worktree branch cannot be empty"));
        }

        git_required(&project.root, ["check-ref-format", "--branch", branch])?;

        let path = sibling_worktree_path(&project.root, branch)?;
        if path.exists() {
            return Err(AppError::msg(format!(
                "worktree path already exists: {}",
                path.display()
            )));
        }

        let base = project.default_branch.as_deref().unwrap_or("HEAD");
        git_required_os(
            &project.root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(branch),
                path.as_os_str(),
                OsStr::new(base),
            ],
        )?;

        if carry_state {
            carry_worktree_state(&project.root, &path)?;
        }

        self.find_worktree(&project.root, &path)
    }

    pub fn finish_worktree(&self, worktree_path: impl AsRef<Path>) -> Result<()> {
        self.remove_worktree(worktree_path)
    }

    pub fn remove_worktree(&self, worktree_path: impl AsRef<Path>) -> Result<()> {
        let path = existing_path(worktree_path.as_ref())?;
        let root = self.resolve_project(&path)?;
        let command_root = self
            .list_worktrees(&root)?
            .into_iter()
            .find(|worktree| canonicalize_existing(&worktree.path).ok().as_ref() != Some(&root))
            .map(|worktree| worktree.path)
            .unwrap_or_else(|| root.clone());

        git_required_os(
            &command_root,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                path.as_os_str(),
            ],
        )?;
        Ok(())
    }

    pub fn list_worktrees(&self, project: impl AsRef<Path>) -> Result<Vec<WorktreeInfo>> {
        let root = self.resolve_project(project)?;
        parse_worktrees(&git_required(&root, ["worktree", "list", "--porcelain"])?)
    }

    pub fn validate_sandbox_paths<P, A, I>(&self, project: P, allowed_paths: I) -> Result<()>
    where
        P: AsRef<Path>,
        A: AsRef<Path>,
        I: IntoIterator<Item = A>,
    {
        let project = self.resolve_project(project)?;
        let allowed_paths = allowed_paths
            .into_iter()
            .map(|path| existing_path(path.as_ref()))
            .collect::<Result<Vec<_>>>()?;

        if allowed_paths.is_empty()
            || allowed_paths
                .iter()
                .any(|allowed| project.starts_with(allowed))
        {
            return Ok(());
        }

        Err(AppError::msg(format!(
            "project path is outside allowed sandbox paths: {}",
            project.display()
        )))
    }

    pub fn project_hooks(&self, project: impl AsRef<Path>) -> Result<ProjectHooks> {
        let root = self.resolve_project(project)?;
        read_project_hooks(&root)
    }

    pub fn run_setup_hooks(&self, project: impl AsRef<Path>, cwd: impl AsRef<Path>) -> Result<()> {
        self.run_hooks(project, cwd, "setup")
    }

    pub fn run_teardown_hooks(
        &self,
        project: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<()> {
        self.run_hooks(project, cwd, "teardown")
    }

    fn run_hooks(
        &self,
        project: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
        kind: &str,
    ) -> Result<()> {
        let hooks = self.project_hooks(project)?;
        let commands = match kind {
            "setup" => hooks.setup,
            "teardown" => hooks.teardown,
            _ => Vec::new(),
        };
        for command in commands {
            run_shell_hook(cwd.as_ref(), kind, &command)?;
        }
        Ok(())
    }

    fn find_worktree(&self, project: &Path, path: &Path) -> Result<WorktreeInfo> {
        let path = canonicalize_existing(path)?;
        self.list_worktrees(project)?
            .into_iter()
            .find(|worktree| canonicalize_existing(&worktree.path).ok().as_ref() == Some(&path))
            .ok_or_else(|| AppError::msg(format!("worktree not found: {}", path.display())))
    }
}

fn carry_worktree_state(source: &Path, target: &Path) -> Result<()> {
    let diff = git_required_bytes(source, ["diff", "--binary", "HEAD"])?;
    if !diff.is_empty() {
        let mut command = Command::new("git");
        command.arg("-C").arg(target).arg("apply");
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin.write_all(&diff)?;
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(AppError::msg(format!(
                "failed to carry tracked worktree state: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }

    let untracked = String::from_utf8_lossy(&git_required_bytes(
        source,
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )?)
    .to_string();
    for relative in untracked.split('\0').filter(|item| !item.is_empty()) {
        let from = source.join(relative);
        if from.is_file() {
            let to = target.join(relative);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(from, to)?;
        }
    }
    Ok(())
}

fn read_project_hooks(root: &Path) -> Result<ProjectHooks> {
    let path = root.join(".agent-helm.toml");
    if !path.exists() {
        return Ok(ProjectHooks::default());
    }
    let raw = fs::read_to_string(&path)?;
    let value: toml::Value = toml::from_str(&raw)?;
    let hooks = value.get("hooks");
    Ok(ProjectHooks {
        setup: hook_commands(hooks, "setup")?,
        teardown: hook_commands(hooks, "teardown")?,
    })
}

fn hook_commands(value: Option<&toml::Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = value.and_then(|value| value.get(key)) else {
        return Ok(Vec::new());
    };
    if let Some(command) = value.as_str() {
        return Ok(vec![command.to_string()]);
    }
    if let Some(commands) = value.as_array() {
        return commands
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| AppError::msg(format!("hooks.{key} entries must be strings")))
            })
            .collect();
    }
    Err(AppError::msg(format!(
        "hooks.{key} must be a string or string array"
    )))
}

fn run_shell_hook(cwd: &Path, kind: &str, command: &str) -> Result<()> {
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(AppError::msg(format!(
        "project {kind} hook failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn repo_identity_for_root(root: &Path) -> Option<String> {
    git_optional(root, ["remote", "get-url", "origin"])
        .map(|url| strip_url_credentials(url.trim()))
        .or_else(|| git_optional(root, ["rev-parse", "--show-toplevel"]))
}

fn default_branch_for_root(root: &Path) -> Option<String> {
    if let Some(branch) = git_optional(
        root,
        ["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Some(
            branch
                .rsplit('/')
                .next()
                .unwrap_or(branch.as_str())
                .to_string(),
        );
    }

    for branch in ["main", "master"] {
        if git_optional(
            root,
            ["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_some()
        {
            return Some(branch.to_string());
        }
    }

    git_optional(root, ["symbolic-ref", "--short", "HEAD"])
        .or_else(|| git_optional(root, ["config", "--get", "init.defaultBranch"]))
}

fn existing_path(path: &Path) -> Result<PathBuf> {
    let path = expand_home(path);
    if !path.exists() {
        return Err(AppError::msg(format!(
            "path does not exist: {}",
            path.display()
        )));
    }
    canonicalize_existing(&path)
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    Ok(path.canonicalize()?)
}

fn command_path(path: &Path) -> PathBuf {
    if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    }
}

fn expand_home(path: &Path) -> PathBuf {
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

fn sibling_worktree_path(root: &Path, branch: &str) -> Result<PathBuf> {
    let repo_name = root.file_name().and_then(OsStr::to_str).ok_or_else(|| {
        AppError::msg(format!("project has no directory name: {}", root.display()))
    })?;
    let branch = branch_slug(branch);
    Ok(root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{repo_name}-{branch}")))
}

fn branch_slug(branch: &str) -> String {
    let slug = branch
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();

    slug.trim_matches('-').to_string()
}

fn parse_worktrees(raw: &str) -> Result<Vec<WorktreeInfo>> {
    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;

    for line in raw.lines() {
        if line.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path),
                branch: None,
                head: None,
                is_bare: false,
                is_detached: false,
            });
        } else if let Some(worktree) = current.as_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                worktree.head = Some(head.to_string());
            } else if let Some(branch) = line.strip_prefix("branch ") {
                worktree.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            } else if line == "bare" {
                worktree.is_bare = true;
            } else if line == "detached" {
                worktree.is_detached = true;
            }
        }
    }

    if let Some(worktree) = current {
        worktrees.push(worktree);
    }

    Ok(worktrees)
}

fn strip_url_credentials(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let Some(at) = url[authority_start..].find('@') else {
        return url.to_string();
    };

    format!(
        "{}{}",
        &url[..authority_start],
        &url[authority_start + at + 1..]
    )
}

fn git_optional<I, S>(cwd: &Path, args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn git_required<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<_>>();
    git_required_os(cwd, args)
}

fn git_required_bytes<I, S>(cwd: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<OsString>>();
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(&args)
        .output()?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(AppError::msg(format!(
        "git command failed: {}{}",
        args.iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        }
    )))
}

fn git_required_os<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<OsString>>();
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(&args)
        .output()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(AppError::msg(format!(
            "git command failed: {}",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        )))
    } else {
        Err(AppError::msg(stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolves_git_root_and_metadata() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let nested = repo.path().join("nested");
        fs::create_dir(&nested)?;
        run_git(
            repo.path(),
            [
                "remote",
                "add",
                "origin",
                "https://token@example.com/acme/repo.git",
            ],
        )?;

        let project = WorkspaceManager.resolve_project_ref(&nested)?;

        assert_eq!(project.root, repo.path().canonicalize()?);
        assert_eq!(
            project.repo_identity.as_deref(),
            Some("https://example.com/acme/repo.git")
        );
        assert_eq!(project.default_branch.as_deref(), Some("main"));
        Ok(())
    }

    #[test]
    fn creates_lists_and_finishes_worktree() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let manager = WorkspaceManager;

        let worktree = manager.create_worktree(repo.path(), "agent/test")?;
        assert!(worktree.path.exists());
        assert_eq!(worktree.branch.as_deref(), Some("agent/test"));

        let listed = manager.list_worktrees(repo.path())?;
        assert!(listed.iter().any(|entry| entry.branch == worktree.branch));

        manager.finish_worktree(&worktree.path)?;
        assert!(!worktree.path.exists());
        Ok(())
    }

    #[test]
    fn carries_tracked_and_untracked_state_to_worktree() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        fs::write(repo.path().join("README.md"), "changed\n")?;
        let notes = repo.path().join("notes");
        fs::create_dir(&notes)?;
        fs::write(notes.join("todo.txt"), "copy me\n")?;

        let manager = WorkspaceManager;
        let worktree = manager.create_worktree_with_state(repo.path(), "agent/carry", true)?;

        assert_eq!(
            fs::read_to_string(worktree.path.join("README.md"))?,
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(worktree.path.join("notes/todo.txt"))?,
            "copy me\n"
        );

        run_git(&worktree.path, ["reset", "--hard"])?;
        run_git(&worktree.path, ["clean", "-fd"])?;
        manager.finish_worktree(&worktree.path)?;
        Ok(())
    }

    #[test]
    fn validates_project_under_allowed_sandbox_path() -> Result<()> {
        let allowed = TempDir::new()?;
        let project = allowed.path().join("repo");
        fs::create_dir(&project)?;

        WorkspaceManager.validate_sandbox_paths(&project, [allowed.path()])?;

        let outside = TempDir::new()?;
        let outside_project = outside.path().join("repo");
        fs::create_dir(&outside_project)?;

        assert!(
            WorkspaceManager
                .validate_sandbox_paths(&outside_project, [allowed.path()])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn project_hooks_parse_and_run_from_project_config() -> Result<()> {
        let project = TempDir::new()?;
        fs::write(
            project.path().join(".agent-helm.toml"),
            r#"
            [hooks]
            setup = "printf setup > setup.txt"
            teardown = ["printf teardown > teardown.txt"]
            "#,
        )?;

        let manager = WorkspaceManager;
        let hooks = manager.project_hooks(project.path())?;
        assert_eq!(hooks.setup, vec!["printf setup > setup.txt"]);
        assert_eq!(hooks.teardown, vec!["printf teardown > teardown.txt"]);

        manager.run_setup_hooks(project.path(), project.path())?;
        manager.run_teardown_hooks(project.path(), project.path())?;
        assert_eq!(
            fs::read_to_string(project.path().join("setup.txt"))?,
            "setup"
        );
        assert_eq!(
            fs::read_to_string(project.path().join("teardown.txt"))?,
            "teardown"
        );
        Ok(())
    }

    fn git_repo() -> Result<TempDir> {
        let repo = TempDir::new()?;
        run_git(repo.path(), ["init"])?;
        run_git(repo.path(), ["checkout", "-b", "main"])?;
        run_git(repo.path(), ["config", "user.email", "test@example.com"])?;
        run_git(repo.path(), ["config", "user.name", "Test User"])?;
        fs::write(repo.path().join("README.md"), "test\n")?;
        run_git(repo.path(), ["add", "README.md"])?;
        run_git(repo.path(), ["commit", "-m", "init"])?;
        Ok(repo)
    }

    fn run_git<I, S>(cwd: &Path, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        git_required(cwd, args).map(|_| ())
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}
