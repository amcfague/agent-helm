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
        let branch = branch.as_ref();
        self.create_named_worktree_with_state(project, branch, branch, carry_state)
    }

    pub fn create_named_worktree_with_state(
        &self,
        project: impl AsRef<Path>,
        branch: impl AsRef<str>,
        name: impl AsRef<str>,
        carry_state: bool,
    ) -> Result<WorktreeInfo> {
        let project = self.resolve_project_ref(project)?;
        let branch = branch.as_ref().trim();
        if branch.is_empty() {
            return Err(AppError::msg("worktree branch cannot be empty"));
        }

        git_required(&project.root, ["check-ref-format", "--branch", branch])?;

        let name = name.as_ref().trim();
        let path =
            sibling_worktree_path(&project.root, if name.is_empty() { branch } else { name })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let branch_ref = format!("refs/heads/{branch}");
        if git_optional(
            &project.root,
            ["rev-parse", "--verify", branch_ref.as_str()],
        )
        .is_some()
        {
            git_required_os(
                &project.root,
                [
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    path.as_os_str(),
                    OsStr::new(branch),
                ],
            )?;
        } else {
            let remote_branch_ref = format!("refs/remotes/origin/{branch}");
            let remote_base = format!("origin/{branch}");
            let base = if git_optional(
                &project.root,
                ["rev-parse", "--verify", remote_branch_ref.as_str()],
            )
            .is_some()
            {
                remote_base.as_str()
            } else {
                project.default_branch.as_deref().unwrap_or("HEAD")
            };
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
        }

        if carry_state && let Err(error) = carry_worktree_state(&project.root, &path) {
            remove_created_worktree(&project.root, &path);
            return Err(error);
        }

        self.find_worktree(&project.root, &path)
    }

    pub fn finish_worktree(&self, worktree_path: impl AsRef<Path>) -> Result<()> {
        self.remove_worktree_with_force(worktree_path, true)
    }

    pub fn discard_created_worktree(
        &self,
        project: impl AsRef<Path>,
        worktree_path: impl AsRef<Path>,
    ) {
        remove_created_worktree(project.as_ref(), worktree_path.as_ref());
    }

    pub fn remove_worktree(&self, worktree_path: impl AsRef<Path>) -> Result<()> {
        self.remove_worktree_with_force(worktree_path, false)
    }

    fn remove_worktree_with_force(
        &self,
        worktree_path: impl AsRef<Path>,
        force: bool,
    ) -> Result<()> {
        let path = existing_path(worktree_path.as_ref())?;
        let root = self.resolve_project(&path)?;
        let command_root = self
            .list_worktrees(&root)?
            .into_iter()
            .find(|worktree| canonicalize_existing(&worktree.path).ok().as_ref() != Some(&root))
            .map(|worktree| worktree.path)
            .unwrap_or_else(|| root.clone());

        let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
        if force {
            args.push(OsString::from("--force"));
        }
        args.push(path.as_os_str().to_os_string());

        git_required_os(&command_root, args)?;
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

    let untracked =
        git_required_bytes(source, ["ls-files", "--others", "--exclude-standard", "-z"])?;
    copy_worktree_files(source, target, &untracked)?;

    let ignored = git_required_bytes(
        source,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    copy_worktree_files(source, target, &ignored)?;

    Ok(())
}

fn copy_worktree_files(source: &Path, target: &Path, raw_files: &[u8]) -> Result<()> {
    let files = String::from_utf8_lossy(raw_files);
    for relative in files.split('\0').filter(|item| !item.is_empty()) {
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

fn remove_created_worktree(project: &Path, path: &Path) {
    let _ = git_required_os(
        project,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            path.as_os_str(),
        ],
    );
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
    crate::util::existing_path(path)
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

fn sibling_worktree_path(root: &Path, name: &str) -> Result<PathBuf> {
    let worktrees = parse_worktrees(&git_required(root, ["worktree", "list", "--porcelain"])?)?;
    let root = primary_worktree_root_from(root, &worktrees)?;
    let repo_name = root.file_name().and_then(OsStr::to_str).ok_or_else(|| {
        AppError::msg(format!("project has no directory name: {}", root.display()))
    })?;
    let name = path_slug(name);
    let base = root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{repo_name}-worktrees"))
        .join(name);
    Ok(unique_path(base, &worktrees))
}

fn primary_worktree_root_from(root: &Path, worktrees: &[WorktreeInfo]) -> Result<PathBuf> {
    let primary = worktrees
        .iter()
        .find(|worktree| !worktree.is_bare)
        .map(|worktree| worktree.path.clone())
        .unwrap_or_else(|| root.to_path_buf());
    canonicalize_existing(&primary)
}

fn unique_path(base: PathBuf, worktrees: &[WorktreeInfo]) -> PathBuf {
    if !worktree_path_taken(&base, worktrees) {
        return base;
    }

    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let name = base
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("session");
    for index in 2.. {
        let candidate = parent.join(format!("{name}-{index}"));
        if !worktree_path_taken(&candidate, worktrees) {
            return candidate;
        }
    }

    unreachable!("unbounded unique worktree path search")
}

fn worktree_path_taken(path: &Path, worktrees: &[WorktreeInfo]) -> bool {
    fs::symlink_metadata(path).is_ok()
        || worktrees
            .iter()
            .any(|worktree| worktree.path.as_path() == path)
}

fn path_slug(value: &str) -> String {
    let mut slug = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }

    let slug = slug.trim_matches('-');
    if slug.is_empty() || slug.chars().all(|ch| ch == '.') {
        "session".to_string()
    } else {
        slug.to_string()
    }
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
        let repo_root = repo.path().canonicalize()?;
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("agent-test");
        assert_eq!(worktree.path, expected_path);
        assert!(worktree.path.exists());
        assert_eq!(worktree.branch.as_deref(), Some("agent/test"));

        let listed = manager.list_worktrees(repo.path())?;
        assert!(listed.iter().any(|entry| entry.branch == worktree.branch));

        manager.finish_worktree(&worktree.path)?;
        assert!(!worktree.path.exists());
        Ok(())
    }

    #[test]
    fn named_worktree_uses_normalized_name_in_repo_sibling_directory() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let manager = WorkspaceManager;

        let worktree = manager.create_named_worktree_with_state(
            repo.path(),
            "agent/named",
            "Codex Session",
            false,
        )?;
        let repo_root = repo.path().canonicalize()?;
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("codex-session");

        assert_eq!(worktree.path, expected_path);
        assert!(worktree.path.exists());

        manager.finish_worktree(&worktree.path)?;
        Ok(())
    }

    #[test]
    fn named_worktree_dot_names_stay_under_repo_sibling_directory() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let manager = WorkspaceManager;

        let worktree =
            manager.create_named_worktree_with_state(repo.path(), "agent/dot-name", "..", false)?;
        let repo_root = repo.path().canonicalize()?;
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_path = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"))
            .join("session");

        assert_eq!(worktree.path, expected_path);
        assert!(worktree.path.exists());

        manager.finish_worktree(&worktree.path)?;
        Ok(())
    }

    #[test]
    fn linked_worktree_creates_sibling_under_primary_repo_worktrees() -> Result<()> {
        if !git_available() {
            return Ok(());
        }
        let repo = git_repo()?;
        let manager = WorkspaceManager;
        let source = manager.create_named_worktree_with_state(
            repo.path(),
            "agent/source",
            "source",
            false,
        )?;
        let child = manager.create_named_worktree_with_state(
            &source.path,
            "agent/child",
            "Child Session",
            false,
        )?;
        let repo_root = repo.path().canonicalize()?;
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let expected_parent = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"));
        assert_eq!(child.path, expected_parent.join("child-session"));
        assert!(child.path.exists());
        manager.finish_worktree(&child.path)?;
        manager.finish_worktree(&source.path)?;
        Ok(())
    }

    #[test]
    fn named_worktree_paths_get_unique_suffixes() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let manager = WorkspaceManager;

        let first =
            manager.create_named_worktree_with_state(repo.path(), "agent/first", "same", false)?;
        let second =
            manager.create_named_worktree_with_state(repo.path(), "agent/second", "same", false)?;

        assert_ne!(first.path, second.path);
        assert_eq!(first.path.file_name().and_then(OsStr::to_str), Some("same"));
        assert_eq!(
            second.path.file_name().and_then(OsStr::to_str),
            Some("same-2")
        );

        manager.finish_worktree(&first.path)?;
        manager.finish_worktree(&second.path)?;
        Ok(())
    }

    #[test]
    fn named_worktree_path_skips_existing_directory_collision() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let repo_root = repo.path().canonicalize()?;
        let repo_name = repo_root.file_name().unwrap().to_string_lossy();
        let worktree_parent = repo_root
            .parent()
            .unwrap()
            .join(format!("{repo_name}-worktrees"));
        fs::create_dir_all(worktree_parent.join("same"))?;

        let manager = WorkspaceManager;
        let worktree = manager.create_named_worktree_with_state(
            repo.path(),
            "agent/collision",
            "same",
            false,
        )?;

        assert_eq!(
            worktree.path.file_name().and_then(OsStr::to_str),
            Some("same-2")
        );

        manager.finish_worktree(&worktree.path)?;
        fs::remove_dir_all(worktree_parent)?;
        Ok(())
    }

    #[test]
    fn named_worktree_path_skips_missing_registered_worktree() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        let manager = WorkspaceManager;

        let stale =
            manager.create_named_worktree_with_state(repo.path(), "agent/stale", "same", false)?;
        fs::remove_dir_all(&stale.path)?;

        let worktree =
            manager.create_named_worktree_with_state(repo.path(), "agent/next", "same", false)?;

        assert_ne!(worktree.path, stale.path);
        assert_eq!(
            worktree.path.file_name().and_then(OsStr::to_str),
            Some("same-2")
        );
        assert!(worktree.path.exists());

        manager.finish_worktree(&worktree.path)?;
        manager.discard_created_worktree(repo.path(), &stale.path);
        Ok(())
    }

    #[test]
    fn creates_new_branch_from_default_branch_not_current_branch() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        run_git(repo.path(), ["checkout", "-b", "topic/current"])?;
        fs::write(repo.path().join("topic.txt"), "current only\n")?;
        run_git(repo.path(), ["add", "topic.txt"])?;
        run_git(repo.path(), ["commit", "-m", "topic change"])?;

        let manager = WorkspaceManager;
        let worktree = manager.create_worktree(repo.path(), "agent/default-base")?;

        assert_eq!(
            fs::read_to_string(worktree.path.join("README.md"))?,
            "test\n"
        );
        assert!(!worktree.path.join("topic.txt").exists());

        manager.finish_worktree(&worktree.path)?;
        Ok(())
    }

    #[test]
    fn creates_worktree_for_existing_branch() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        run_git(repo.path(), ["checkout", "-b", "agent/existing"])?;
        fs::write(repo.path().join("existing.txt"), "existing branch\n")?;
        run_git(repo.path(), ["add", "existing.txt"])?;
        run_git(repo.path(), ["commit", "-m", "existing branch"])?;
        run_git(repo.path(), ["checkout", "main"])?;
        let manager = WorkspaceManager;

        let worktree = manager.create_worktree(repo.path(), "agent/existing")?;
        assert!(worktree.path.exists());
        assert_eq!(worktree.branch.as_deref(), Some("agent/existing"));
        assert_eq!(
            fs::read_to_string(worktree.path.join("existing.txt"))?,
            "existing branch\n"
        );

        manager.finish_worktree(&worktree.path)?;
        Ok(())
    }

    #[test]
    fn creates_worktree_for_remote_tracking_branch() -> Result<()> {
        if !git_available() {
            return Ok(());
        }
        let repo = git_repo()?;
        let origin = TempDir::new()?;
        run_git(origin.path(), ["init", "--bare"])?;
        run_git(
            repo.path(),
            ["remote", "add", "origin", origin.path().to_str().unwrap()],
        )?;
        run_git(repo.path(), ["checkout", "-b", "agent/remote"])?;
        fs::write(repo.path().join("remote.txt"), "from origin\n")?;
        run_git(repo.path(), ["add", "remote.txt"])?;
        run_git(repo.path(), ["commit", "-m", "remote branch"])?;
        run_git(repo.path(), ["push", "-u", "origin", "agent/remote"])?;
        run_git(repo.path(), ["checkout", "main"])?;
        run_git(repo.path(), ["branch", "-D", "agent/remote"])?;

        let manager = WorkspaceManager;
        let worktree = manager.create_worktree(repo.path(), "agent/remote")?;

        assert_eq!(
            fs::read_to_string(worktree.path.join("remote.txt"))?,
            "from origin\n"
        );
        manager.finish_worktree(&worktree.path)?;
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
        fs::write(repo.path().join(".gitignore"), "ignored/\n")?;
        let ignored = repo.path().join("ignored");
        fs::create_dir(&ignored)?;
        fs::write(ignored.join("local.env"), "SECRET=copy\n")?;

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
        assert_eq!(
            fs::read_to_string(worktree.path.join("ignored/local.env"))?,
            "SECRET=copy\n"
        );

        run_git(&worktree.path, ["reset", "--hard"])?;
        run_git(&worktree.path, ["clean", "-fdx"])?;
        manager.finish_worktree(&worktree.path)?;
        Ok(())
    }

    #[test]
    fn removes_created_worktree_when_carry_state_fails() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        run_git(repo.path(), ["checkout", "-b", "agent/conflict"])?;
        fs::write(repo.path().join("README.md"), "branch\n")?;
        run_git(repo.path(), ["add", "README.md"])?;
        run_git(repo.path(), ["commit", "-m", "branch change"])?;
        run_git(repo.path(), ["checkout", "main"])?;
        fs::write(repo.path().join("README.md"), "dirty\n")?;

        let manager = WorkspaceManager;
        let path = sibling_worktree_path(repo.path(), "agent/conflict")?;

        assert!(
            manager
                .create_worktree_with_state(repo.path(), "agent/conflict", true)
                .is_err()
        );
        assert!(!path.exists());
        assert!(
            !manager
                .list_worktrees(repo.path())?
                .iter()
                .any(|entry| entry.path == path)
        );
        Ok(())
    }

    #[test]
    fn discards_created_worktree_after_setup_hook_failure() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        fs::write(
            repo.path().join(".agent-helm.toml"),
            r#"
[hooks]
setup = "printf setup-started > setup.txt && exit 7"
"#,
        )?;

        let manager = WorkspaceManager;
        let worktree = manager.create_worktree(repo.path(), "agent/setup-fails")?;

        assert!(
            manager
                .run_setup_hooks(repo.path(), &worktree.path)
                .is_err()
        );
        assert!(worktree.path.join("setup.txt").exists());
        assert!(
            manager
                .list_worktrees(repo.path())?
                .iter()
                .any(|entry| entry.path == worktree.path)
        );

        manager.discard_created_worktree(repo.path(), &worktree.path);

        assert!(!worktree.path.exists());
        assert!(
            !manager
                .list_worktrees(repo.path())?
                .iter()
                .any(|entry| entry.path == worktree.path)
        );
        Ok(())
    }

    #[test]
    fn finish_worktree_removes_after_teardown_hook_creates_untracked_file() -> Result<()> {
        if !git_available() {
            return Ok(());
        }

        let repo = git_repo()?;
        fs::write(
            repo.path().join(".agent-helm.toml"),
            r#"
[hooks]
teardown = "printf teardown > teardown.txt"
"#,
        )?;

        let manager = WorkspaceManager;
        let worktree = manager.create_worktree(repo.path(), "agent/teardown-dirty")?;

        manager.run_teardown_hooks(repo.path(), &worktree.path)?;
        assert_eq!(
            fs::read_to_string(worktree.path.join("teardown.txt"))?,
            "teardown"
        );

        manager.finish_worktree(&worktree.path)?;

        assert!(!worktree.path.exists());
        assert!(
            !manager
                .list_worktrees(repo.path())?
                .iter()
                .any(|entry| entry.path == worktree.path)
        );
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
