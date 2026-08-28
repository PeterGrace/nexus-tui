use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use color_eyre::eyre::WrapErr;
use color_eyre::eyre::{bail, Result};
use wait_timeout::ChildExt;

use crate::repo_config;

const HOOK_TIMEOUT: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Default base spec: branch from the remote's default branch.
pub const DEFAULT_BASE_SPEC: &str = "auto";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct RepoInfo {
    pub root: PathBuf,
}

/// Resolved configuration controlling where a new worktree branch starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseSettings {
    /// Base spec: `auto`, `HEAD`, or any git revision (`origin/main`, `develop`).
    pub spec: String,
    /// Whether to fetch a remote base before branching from it.
    pub fetch: bool,
}

/// A concrete start point for a new worktree branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBase {
    /// Commit passed to `git worktree add`. `None` branches from the primary
    /// checkout's HEAD, which is git's own default.
    pub rev: Option<String>,
    /// Human-readable name of what we based on, for status messages.
    pub display: String,
    /// Non-fatal problem hit while resolving (offline, unknown ref, ...).
    pub warning: Option<String>,
}

impl ResolvedBase {
    /// Fall back to the primary checkout's HEAD, carrying an optional reason.
    fn head(warning: Option<String>) -> Self {
        Self {
            rev: None,
            display: "HEAD".to_string(),
            warning,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detect if path is inside a git work tree. Returns repo root.
/// Returns None for non-git dirs, bare repos, errors.
pub fn detect_repo(path: &str) -> Option<RepoInfo> {
    let output = Command::new("git")
        .args(["-C", path, "rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let is_inside = String::from_utf8_lossy(&output.stdout).trim() == "true";
    if !is_inside {
        return None;
    }

    let root_output = Command::new("git")
        .args(["-C", path, "rev-parse", "--show-toplevel"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !root_output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&root_output.stdout)
        .trim()
        .to_string();
    if root.is_empty() {
        return None;
    }

    Some(RepoInfo {
        root: PathBuf::from(root),
    })
}

/// Check if a git branch exists in the repo.
pub fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Sanitize a session name into a valid git branch name.
/// ALLOWLIST approach: only permit [a-zA-Z0-9._/-].
/// Prefixes with the given string (e.g. repo dir name). Empty prefix = no prefix.
pub fn sanitize_branch_name(session_name: &str, prefix: &str) -> String {
    let sanitized = sanitize_ref_component(session_name);

    if sanitized.is_empty() {
        if prefix.is_empty() {
            "session".to_string()
        } else {
            format!("{prefix}/session")
        }
    } else if prefix.is_empty() {
        sanitized
    } else {
        format!("{prefix}/{sanitized}")
    }
}

/// Resolve the worktree branch prefix.
/// Priority: per-repo `.nexus.toml` > global config > repo directory name.
pub fn resolve_branch_prefix(repo_root: &Path, global_prefix: Option<&str>) -> String {
    // 1. Check per-repo .nexus.toml
    let repo_cfg = repo_config::load_repo_config(repo_root);
    if let Some(prefix) = repo_cfg.worktree.branch_prefix {
        return normalize_prefix(&prefix);
    }

    // 2. Check global config
    if let Some(prefix) = global_prefix {
        return normalize_prefix(prefix);
    }

    // 3. Auto-detect from repo directory name
    let dir_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    normalize_prefix(dir_name)
}

/// Resolve where new worktree branches should start from.
/// Priority per field: per-repo `.nexus.toml` > global config > default.
pub fn resolve_base_settings(
    repo_root: &Path,
    global_base: Option<&str>,
    global_fetch: Option<bool>,
) -> BaseSettings {
    let repo_cfg = repo_config::load_repo_config(repo_root);

    let spec = repo_cfg
        .worktree
        .base
        .as_deref()
        .or(global_base)
        .map(normalize_base_spec)
        .unwrap_or_else(|| DEFAULT_BASE_SPEC.to_string());

    let fetch = repo_cfg.worktree.fetch.or(global_fetch).unwrap_or(true);

    BaseSettings { spec, fetch }
}

/// Resolve `settings` into a concrete commit to branch from.
///
/// Never fails: any problem (offline, auth, unknown ref) degrades to branching
/// from the primary checkout's HEAD with a warning attached, so creating a
/// worktree still succeeds when the network does not cooperate.
pub fn resolve_base(repo_root: &Path, settings: &BaseSettings) -> ResolvedBase {
    let spec = settings.spec.as_str();

    // Explicit opt-out: branch from whatever the primary checkout has checked
    // out, which is what Nexus did before this was configurable.
    if spec.eq_ignore_ascii_case("head") {
        return ResolvedBase::head(None);
    }

    let remotes = list_remotes(repo_root);
    let target = if spec == DEFAULT_BASE_SPEC {
        default_remote_branch(repo_root, &remotes)
    } else {
        split_remote_spec(spec, &remotes)
    };

    let Some((remote, branch)) = target else {
        // Not a remote ref — treat the spec as a local revision.
        if spec == DEFAULT_BASE_SPEC {
            // No remotes configured at all; HEAD is the only sane base.
            return ResolvedBase::head(None);
        }
        return match verify_rev(repo_root, spec) {
            Some(rev) => ResolvedBase {
                rev: Some(rev),
                display: spec.to_string(),
                warning: None,
            },
            None => {
                ResolvedBase::head(Some(format!("base '{spec}' not found; branched from HEAD")))
            }
        };
    };

    let display = format!("{remote}/{branch}");

    if !settings.fetch {
        return local_base(repo_root, &remote, &branch, &display, None);
    }

    match fetch_base(repo_root, &remote, &branch) {
        Ok(rev) => ResolvedBase {
            rev: Some(rev),
            display,
            warning: None,
        },
        Err(e) => local_base(
            repo_root,
            &remote,
            &branch,
            &display,
            Some(shorten(&e.to_string())),
        ),
    }
}

/// Empty or whitespace-only base specs fall back to the default.
fn normalize_base_spec(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        DEFAULT_BASE_SPEC.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Normalize a prefix string: replace dots/underscores with dashes, then sanitize.
fn normalize_prefix(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let replaced = raw.replace(['.', '_'], "-");
    sanitize_ref_component(&replaced)
}

/// Core sanitization: allowlist filter, collapse dashes/slashes, strip unsafe sequences.
fn sanitize_ref_component(input: &str) -> String {
    let filtered: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '/' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse runs of `-`, trim leading/trailing `-`
    let mut result = String::new();
    let mut last_was_dash = false;
    for c in filtered.chars() {
        if c == '-' {
            if !last_was_dash {
                result.push('-');
            }
            last_was_dash = true;
        } else {
            result.push(c);
            last_was_dash = false;
        }
    }

    let mut result = result.trim_matches('-').to_string();

    // Strip `..` sequences (git ref traversal) and leading dots
    while result.contains("..") {
        result = result.replace("..", ".");
    }
    result = result.trim_start_matches('.').to_string();

    // Collapse consecutive slashes and strip leading/trailing slashes
    while result.contains("//") {
        result = result.replace("//", "/");
    }
    result = result.trim_matches('/').to_string();

    // Strip trailing `.lock` (reserved by git)
    if result.ends_with(".lock") {
        result.truncate(result.len() - 5);
    }

    result
}

/// Resolve a hook path using the config priority chain:
/// 1. Per-repo `.nexus.toml` path (resolved relative to repo root)
/// 2. Global `config.toml` path (tilde-expanded)
/// 3. Convention: `{repo_root}/.nexus/{hook_name}`
///
/// `hook_name` must be `"on-worktree-create"` or `"on-worktree-teardown"`.
/// Validates: file exists, is a regular file (not symlink), has executable bit.
pub fn resolve_hook_path(
    repo_root: &Path,
    hook_name: &str,
    global_config_path: Option<&str>,
) -> Option<PathBuf> {
    // 1. Per-repo config path (relative to repo root)
    let repo_cfg = repo_config::load_repo_config(repo_root);
    let repo_config_path = match hook_name {
        "on-worktree-create" => repo_cfg.worktree.on_create.as_deref(),
        "on-worktree-teardown" => repo_cfg.worktree.on_teardown.as_deref(),
        _ => None,
    };
    if let Some(rel) = repo_config_path {
        // Reject paths with .. components that could escape repo root
        if Path::new(rel)
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return None;
        }
        let candidate = repo_root.join(rel);
        if validate_hook_file(&candidate) {
            return Some(candidate);
        }
        return None; // Explicitly configured but invalid — don't fall through
    }

    // 2. Global config path (expand ~)
    if let Some(path_str) = global_config_path {
        let expanded = expand_tilde(path_str);
        if validate_hook_file(&expanded) {
            return Some(expanded);
        }
        return None; // Explicitly configured but invalid — don't fall through
    }

    // 3. Convention fallback
    resolve_hook(repo_root, hook_name)
}

/// Expand leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Validate a hook file: exists, not a symlink, executable.
fn validate_hook_file(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    // Reject symlinks
    if path
        .symlink_metadata()
        .map(|m| m.is_symlink())
        .unwrap_or(true)
    {
        return false;
    }

    // Check regular file + executable bit (Unix), regular file only (non-Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = path.metadata() {
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                return false;
            }
        } else {
            return false;
        }
    }

    #[cfg(not(unix))]
    {
        if let Ok(meta) = path.metadata() {
            if !meta.is_file() {
                return false;
            }
        } else {
            return false;
        }
    }

    true
}

/// Create a worktree.
/// If `create_hook` is provided, delegates to that hook script.
/// Otherwise runs `git worktree add <path> -b <branch> [<base>]`.
/// `base` is the resolved start point; `None` branches from the primary
/// checkout's HEAD.
pub fn create_worktree(
    repo_root: &Path,
    session_name: &str,
    worktree_path: &Path,
    branch: &str,
    base: Option<&str>,
    create_hook: Option<&Path>,
) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("cannot create worktree parent dir {}", parent.display()))?;
    }

    let env_vars = hook_env_vars(repo_root, worktree_path, branch, session_name, base);

    if let Some(hook) = create_hook {
        execute_hook(hook, &env_vars)?;

        // Verify the hook actually created the worktree directory
        if !worktree_path.exists() {
            bail!(
                "Hook at {} did not create worktree at {}",
                hook.display(),
                worktree_path.display()
            );
        }
    } else {
        let mut args = vec![
            "-C".to_string(),
            repo_root.to_string_lossy().to_string(),
            "worktree".to_string(),
            "add".to_string(),
            worktree_path.to_string_lossy().to_string(),
            "-b".to_string(),
            branch.to_string(),
        ];
        // Omitting the start point makes git branch from the primary
        // checkout's HEAD, which is the documented `base = "HEAD"` behavior.
        if let Some(base) = base {
            args.push(base.to_string());
        }

        let output = Command::new("git")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .wrap_err("failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {}", stderr.trim());
        }
    }

    Ok(())
}

/// Remove a worktree.
/// If `teardown_hook` is provided, delegates to that hook script.
/// If worktree path doesn't exist on disk: skips cleanup, returns Ok.
pub fn remove_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    branch: &str,
    teardown_hook: Option<&Path>,
) -> Result<()> {
    if !worktree_path.exists() {
        return Ok(());
    }

    let env_vars = hook_env_vars(repo_root, worktree_path, branch, "", None);

    if let Some(hook) = teardown_hook {
        execute_hook(hook, &env_vars)?;
    } else {
        // Force remove in case there are uncommitted changes
        let output = Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "worktree",
                "remove",
                "--force",
                &worktree_path.to_string_lossy(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .wrap_err("failed to run git worktree remove")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree remove failed: {}", stderr.trim());
        }

        // Force-delete the branch to match --force worktree removal semantics
        let _ = Command::new("git")
            .args(["-C", &repo_root.to_string_lossy(), "branch", "-D", branch])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Resolve convention hook: check `{repo_root}/.nexus/on-worktree-{create,teardown}`.
/// Validates: file exists, is a regular file (not symlink), has executable bit.
///
/// NOTE: There is an inherent TOCTOU race between this check and the subsequent
/// `execute_hook` call (the file could be replaced between resolution and execution).
/// This is acceptable because hooks live in the user's own repo and this is the same
/// trust model as git's own hook system.
fn resolve_hook(repo_root: &Path, hook_name: &str) -> Option<PathBuf> {
    let hook_path = repo_root.join(".nexus").join(hook_name);
    if validate_hook_file(&hook_path) {
        Some(hook_path)
    } else {
        None
    }
}

/// Execute a hook script DIRECTLY as an executable (not via $SHELL -c).
/// Sets env vars and enforces HOOK_TIMEOUT.
/// On timeout: kills process group to clean up child processes.
fn execute_hook(script: &Path, env: &[(String, String)]) -> Result<()> {
    let mut cmd = Command::new(script);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());

    // Scrub inherited environment, only forwarding safe vars
    cmd.env_clear();
    for key in &["PATH", "HOME", "SHELL", "USER", "LANG", "TERM"] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }

    for (k, v) in env {
        cmd.env(k, v);
    }

    // Spawn in a new process group so we can kill the whole group on timeout
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .wrap_err_with(|| format!("failed to execute hook at {}", script.display()))?;

    match child
        .wait_timeout(HOOK_TIMEOUT)
        .wrap_err("hook wait error")?
    {
        Some(status) => {
            if !status.success() {
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();
                bail!(
                    "Hook {} exited with {}: {}",
                    script.display(),
                    status,
                    stderr.trim()
                );
            }
            Ok(())
        }
        None => {
            // Timeout — kill process group
            #[cfg(unix)]
            {
                let pid = child.id() as libc::pid_t;
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
            let _ = child.wait();
            bail!(
                "Hook {} timed out after {}s",
                script.display(),
                HOOK_TIMEOUT.as_secs()
            );
        }
    }
}

/// Fetch one branch from a remote and return the fetched commit.
///
/// Reads the commit back from `FETCH_HEAD` rather than `refs/remotes/<remote>/*`:
/// a repo with a custom `remote.<name>.fetch` refspec may not maintain those
/// remote-tracking refs at all, so they can be stale or missing entirely.
fn fetch_base(repo_root: &Path, remote: &str, branch: &str) -> Result<String> {
    let mut child = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "fetch",
            "--quiet",
            "--no-tags",
            remote,
            branch,
        ])
        // Never let a credential prompt block the caller: this runs on the
        // TUI's worktree-creation thread with no terminal to prompt on.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("failed to run git fetch")?;

    match child
        .wait_timeout(FETCH_TIMEOUT)
        .wrap_err("git fetch wait error")?
    {
        Some(status) if status.success() => {}
        Some(_) => {
            let stderr = read_stderr(&mut child);
            if stderr.is_empty() {
                bail!("git fetch failed");
            }
            bail!("{}", stderr);
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", FETCH_TIMEOUT.as_secs());
        }
    }

    match verify_rev(repo_root, "FETCH_HEAD") {
        Some(rev) => Ok(rev),
        None => bail!("FETCH_HEAD did not resolve to a commit"),
    }
}

/// Resolve a remote branch from local refs only, with no network access.
/// `fetch_err` is `Some` when a fetch was attempted and failed.
fn local_base(
    repo_root: &Path,
    remote: &str,
    branch: &str,
    display: &str,
    fetch_err: Option<String>,
) -> ResolvedBase {
    let candidates = if branch == "HEAD" {
        vec![format!("refs/remotes/{remote}/HEAD")]
    } else {
        vec![
            format!("refs/remotes/{remote}/{branch}"),
            format!("refs/heads/{branch}"),
        ]
    };

    let found = candidates.iter().find_map(|c| verify_rev(repo_root, c));

    match (found, fetch_err) {
        (Some(rev), None) => ResolvedBase {
            rev: Some(rev),
            display: display.to_string(),
            warning: None,
        },
        (Some(rev), Some(err)) => ResolvedBase {
            rev: Some(rev),
            display: display.to_string(),
            warning: Some(format!("fetch failed ({err}); used local {display}")),
        },
        (None, None) => ResolvedBase::head(Some(format!(
            "{display} not found locally; branched from HEAD"
        ))),
        (None, Some(err)) => {
            ResolvedBase::head(Some(format!("fetch failed ({err}); branched from HEAD")))
        }
    }
}

/// Resolve a revision to a commit SHA. `None` if it does not exist.
fn verify_rev(repo_root: &Path, rev: &str) -> Option<String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// List configured remote names.
fn list_remotes(repo_root: &Path) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "remote"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Pick the remote and branch that `auto` tracks: the default branch of
/// `origin`, or of the first remote when there is no `origin`.
fn default_remote_branch(repo_root: &Path, remotes: &[String]) -> Option<(String, String)> {
    let remote = remotes
        .iter()
        .find(|r| *r == "origin")
        .or_else(|| remotes.first())?
        .clone();

    // `refs/remotes/<remote>/HEAD` names the remote's default branch. The SHA
    // it points at may be stale, but the *name* is all we need — the fetch
    // that follows is authoritative. Falling back to the literal `HEAD`
    // refspec asks the remote for its own default branch directly.
    let branch = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .and_then(|s| s.strip_prefix(&format!("{remote}/")).map(str::to_string))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "HEAD".to_string());

    Some((remote, branch))
}

/// Split `origin/main` into `("origin", "main")` when the left side names a
/// configured remote. Returns `None` for local revisions like `develop`.
fn split_remote_spec(spec: &str, remotes: &[String]) -> Option<(String, String)> {
    let (remote, branch) = spec.split_once('/')?;
    if branch.is_empty() || !remotes.iter().any(|r| r == remote) {
        return None;
    }
    Some((remote.to_string(), branch.to_string()))
}

/// Drain a child's stderr into a trimmed string.
fn read_stderr(child: &mut std::process::Child) -> String {
    child
        .stderr
        .take()
        .map(|mut s| {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut s, &mut buf).ok();
            buf
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Collapse multi-line git stderr into one short line fit for a status bar.
fn shorten(msg: &str) -> String {
    let line = msg
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(79).collect::<String>())
    } else {
        line.to_string()
    }
}

fn hook_env_vars(
    repo_root: &Path,
    worktree_path: &Path,
    branch: &str,
    session_name: &str,
    base: Option<&str>,
) -> Vec<(String, String)> {
    vec![
        (
            "NEXUS_WORKTREE_PATH".to_string(),
            worktree_path.to_string_lossy().to_string(),
        ),
        ("NEXUS_BRANCH".to_string(), branch.to_string()),
        ("NEXUS_SESSION_NAME".to_string(), session_name.to_string()),
        (
            "NEXUS_REPO_ROOT".to_string(),
            repo_root.to_string_lossy().to_string(),
        ),
        // Empty when the base resolved to the primary checkout's HEAD, so a
        // hook can test `-n "$NEXUS_BASE_REF"` before using it.
        (
            "NEXUS_BASE_REF".to_string(),
            base.unwrap_or_default().to_string(),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize_branch_name with prefix ---

    #[test]
    fn test_sanitize_branch_name_basic() {
        assert_eq!(
            sanitize_branch_name("my-feature", "nexus"),
            "nexus/my-feature"
        );
    }

    #[test]
    fn test_sanitize_branch_name_spaces() {
        assert_eq!(
            sanitize_branch_name("my feature", "nexus"),
            "nexus/my-feature"
        );
    }

    #[test]
    fn test_sanitize_branch_name_special_chars() {
        assert_eq!(
            sanitize_branch_name("hello!@#$world", "nexus"),
            "nexus/hello-world"
        );
    }

    #[test]
    fn test_sanitize_branch_name_unicode() {
        assert_eq!(sanitize_branch_name("café☕", "nexus"), "nexus/caf");
    }

    #[test]
    fn test_sanitize_branch_name_empty() {
        assert_eq!(sanitize_branch_name("", "nexus"), "nexus/session");
    }

    #[test]
    fn test_sanitize_branch_name_only_special() {
        assert_eq!(sanitize_branch_name("!!!", "nexus"), "nexus/session");
    }

    #[test]
    fn test_sanitize_branch_name_force_flag() {
        assert_eq!(sanitize_branch_name("--force", "nexus"), "nexus/force");
    }

    #[test]
    fn test_sanitize_branch_name_dash_b() {
        assert_eq!(sanitize_branch_name("-b", "nexus"), "nexus/b");
    }

    #[test]
    fn test_sanitize_branch_name_path_escape() {
        assert_eq!(sanitize_branch_name("../escape", "nexus"), "nexus/escape");
    }

    #[test]
    fn test_sanitize_branch_name_dot_lock() {
        assert_eq!(
            sanitize_branch_name("my-branch.lock", "nexus"),
            "nexus/my-branch"
        );
    }

    #[test]
    fn test_sanitize_branch_name_consecutive_dashes() {
        assert_eq!(sanitize_branch_name("a---b", "nexus"), "nexus/a-b");
    }

    #[test]
    fn test_sanitize_branch_name_dots_and_slashes() {
        assert_eq!(
            sanitize_branch_name("feat/v2.0/thing", "nexus"),
            "nexus/feat/v2.0/thing"
        );
    }

    // --- sanitize_branch_name with custom/empty prefix ---

    #[test]
    fn test_sanitize_branch_name_custom_prefix() {
        assert_eq!(sanitize_branch_name("fix-bug", "my-app"), "my-app/fix-bug");
    }

    #[test]
    fn test_sanitize_branch_name_empty_prefix() {
        assert_eq!(sanitize_branch_name("fix-bug", ""), "fix-bug");
    }

    #[test]
    fn test_sanitize_branch_name_empty_name_empty_prefix() {
        assert_eq!(sanitize_branch_name("", ""), "session");
    }

    #[test]
    fn test_sanitize_branch_name_empty_name_custom_prefix() {
        assert_eq!(sanitize_branch_name("", "team"), "team/session");
    }

    // --- normalize_prefix ---

    #[test]
    fn test_normalize_prefix_dots() {
        assert_eq!(normalize_prefix("my.company.app"), "my-company-app");
    }

    #[test]
    fn test_normalize_prefix_underscores() {
        assert_eq!(normalize_prefix("my_app_name"), "my-app-name");
    }

    #[test]
    fn test_normalize_prefix_empty() {
        assert_eq!(normalize_prefix(""), "");
    }

    #[test]
    fn test_normalize_prefix_clean() {
        assert_eq!(normalize_prefix("my-app"), "my-app");
    }

    // --- resolve_branch_prefix ---

    #[test]
    fn test_resolve_prefix_from_repo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-project");
        std::fs::create_dir(&repo).unwrap();
        assert_eq!(resolve_branch_prefix(&repo, None), "my-project");
    }

    #[test]
    fn test_resolve_prefix_global_overrides_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-project");
        std::fs::create_dir(&repo).unwrap();
        assert_eq!(resolve_branch_prefix(&repo, Some("team")), "team");
    }

    #[test]
    fn test_resolve_prefix_repo_config_overrides_global() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-project");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(
            repo.join(".nexus.toml"),
            "[worktree]\nbranch_prefix = \"custom\"\n",
        )
        .unwrap();
        assert_eq!(resolve_branch_prefix(&repo, Some("global")), "custom");
    }

    #[test]
    fn test_resolve_prefix_repo_config_empty_disables() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-project");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(
            repo.join(".nexus.toml"),
            "[worktree]\nbranch_prefix = \"\"\n",
        )
        .unwrap();
        assert_eq!(resolve_branch_prefix(&repo, Some("global")), "");
    }

    #[test]
    fn test_resolve_prefix_global_empty_disables() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-project");
        std::fs::create_dir(&repo).unwrap();
        assert_eq!(resolve_branch_prefix(&repo, Some("")), "");
    }

    #[test]
    fn test_resolve_prefix_dir_with_dots_normalized() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my.company.app");
        std::fs::create_dir(&repo).unwrap();
        assert_eq!(resolve_branch_prefix(&repo, None), "my-company-app");
    }

    #[test]
    fn test_detect_repo_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_repo(tmp.path().to_str().unwrap());
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_repo_real_git() {
        let tmp = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", tmp.path().to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        let result = detect_repo(tmp.path().to_str().unwrap());
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    fn init_test_repo(path: &Path) {
        Command::new("git")
            .args(["init", path.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        // Configure user for CI environments where no global git config exists
        for args in [
            vec!["-C", path.to_str().unwrap(), "config", "user.name", "test"],
            vec![
                "-C",
                path.to_str().unwrap(),
                "config",
                "user.email",
                "test@test.com",
            ],
        ] {
            Command::new("git")
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
        }
        Command::new("git")
            .args([
                "-C",
                path.to_str().unwrap(),
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
    }

    // --- base resolution helpers ---

    /// Run a git command in `path`, asserting success.
    fn git(path: &Path, args: &[&str]) -> String {
        let mut full = vec!["-C", path.to_str().unwrap()];
        full.extend_from_slice(args);
        let out = Command::new("git")
            .args(&full)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit_file(path: &Path, name: &str, content: &str) -> String {
        std::fs::write(path.join(name), content).unwrap();
        git(path, &["add", name]);
        git(path, &["commit", "-m", &format!("add {name}")]);
        git(path, &["rev-parse", "HEAD"])
    }

    fn head_sha(path: &Path) -> String {
        git(path, &["rev-parse", "HEAD"])
    }

    fn current_branch(path: &Path) -> String {
        git(path, &["branch", "--show-current"])
    }

    /// Build an "origin" repo plus a clone of it, both with git identity set.
    fn init_origin_and_clone(tmp: &Path) -> (PathBuf, PathBuf) {
        let origin = tmp.join("origin");
        std::fs::create_dir(&origin).unwrap();
        init_test_repo(&origin);
        commit_file(&origin, "a.txt", "one");

        let clone = tmp.join("clone");
        Command::new("git")
            .args([
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        git(&clone, &["config", "user.name", "test"]);
        git(&clone, &["config", "user.email", "test@test.com"]);

        (origin, clone)
    }

    fn auto() -> BaseSettings {
        BaseSettings {
            spec: DEFAULT_BASE_SPEC.to_string(),
            fetch: true,
        }
    }

    #[test]
    fn test_split_remote_spec() {
        let remotes = vec!["origin".to_string(), "upstream".to_string()];
        assert_eq!(
            split_remote_spec("origin/main", &remotes),
            Some(("origin".to_string(), "main".to_string()))
        );
        assert_eq!(
            split_remote_spec("upstream/release/2.x", &remotes),
            Some(("upstream".to_string(), "release/2.x".to_string()))
        );
        // Left side is not a configured remote -> local revision
        assert_eq!(split_remote_spec("feature/foo", &remotes), None);
        // No slash at all -> local revision
        assert_eq!(split_remote_spec("develop", &remotes), None);
        // Trailing slash is not a branch
        assert_eq!(split_remote_spec("origin/", &remotes), None);
    }

    #[test]
    fn test_shorten_collapses_multiline() {
        assert_eq!(
            shorten("fatal: could not read\nsecond line"),
            "fatal: could not read"
        );
        assert_eq!(shorten("\n\n  real message  \n"), "real message");
        let long = "x".repeat(200);
        let short = shorten(&long);
        assert_eq!(short.chars().count(), 80);
        assert!(short.ends_with('\u{2026}'));
    }

    #[test]
    fn test_resolve_base_settings_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = resolve_base_settings(tmp.path(), None, None);
        assert_eq!(settings.spec, DEFAULT_BASE_SPEC);
        assert!(settings.fetch);
    }

    #[test]
    fn test_resolve_base_settings_global_used() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = resolve_base_settings(tmp.path(), Some("origin/develop"), Some(false));
        assert_eq!(settings.spec, "origin/develop");
        assert!(!settings.fetch);
    }

    #[test]
    fn test_resolve_base_settings_repo_overrides_global() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".nexus.toml"),
            "[worktree]\nbase = \"upstream/main\"\nfetch = false\n",
        )
        .unwrap();
        let settings = resolve_base_settings(tmp.path(), Some("origin/develop"), Some(true));
        assert_eq!(settings.spec, "upstream/main");
        assert!(!settings.fetch);
    }

    #[test]
    fn test_resolve_base_settings_empty_spec_falls_back_to_auto() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = resolve_base_settings(tmp.path(), Some("   "), None);
        assert_eq!(settings.spec, DEFAULT_BASE_SPEC);
    }

    #[test]
    fn test_resolve_base_head_spec_branches_from_head() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, clone) = init_origin_and_clone(tmp.path());
        let settings = BaseSettings {
            spec: "HEAD".to_string(),
            fetch: true,
        };
        let base = resolve_base(&clone, &settings);
        assert_eq!(base.rev, None);
        assert_eq!(base.display, "HEAD");
        assert!(base.warning.is_none());
    }

    #[test]
    fn test_resolve_base_auto_without_remotes_is_head() {
        let tmp = tempfile::tempdir().unwrap();
        init_test_repo(tmp.path());
        let base = resolve_base(tmp.path(), &auto());
        assert_eq!(base.rev, None);
        assert!(base.warning.is_none());
    }

    #[test]
    fn test_resolve_base_local_revision() {
        let tmp = tempfile::tempdir().unwrap();
        init_test_repo(tmp.path());
        let first = commit_file(tmp.path(), "a.txt", "one");
        git(tmp.path(), &["branch", "develop"]);
        commit_file(tmp.path(), "b.txt", "two");

        let settings = BaseSettings {
            spec: "develop".to_string(),
            fetch: true,
        };
        let base = resolve_base(tmp.path(), &settings);
        assert_eq!(base.rev.as_deref(), Some(first.as_str()));
        assert_eq!(base.display, "develop");
        assert!(base.warning.is_none());
    }

    #[test]
    fn test_resolve_base_unknown_revision_warns_and_uses_head() {
        let tmp = tempfile::tempdir().unwrap();
        init_test_repo(tmp.path());
        commit_file(tmp.path(), "a.txt", "one");

        let settings = BaseSettings {
            spec: "no-such-branch".to_string(),
            fetch: true,
        };
        let base = resolve_base(tmp.path(), &settings);
        assert_eq!(base.rev, None);
        assert_eq!(base.display, "HEAD");
        let warning = base.warning.expect("expected a warning");
        assert!(warning.contains("no-such-branch"), "got: {warning}");
    }

    #[test]
    fn test_resolve_base_auto_fetches_new_remote_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, clone) = init_origin_and_clone(tmp.path());

        // Origin moves ahead after the clone; the clone has not fetched.
        let latest = commit_file(&origin, "b.txt", "two");
        assert_ne!(head_sha(&clone), latest);

        let base = resolve_base(&clone, &auto());
        assert_eq!(
            base.rev.as_deref(),
            Some(latest.as_str()),
            "auto should fetch and use the remote's tip, not the local HEAD"
        );
        assert!(
            base.warning.is_none(),
            "unexpected warning: {:?}",
            base.warning
        );
        assert_eq!(base.display, format!("origin/{}", current_branch(&origin)));
    }

    #[test]
    fn test_resolve_base_ignores_stale_tracking_ref_under_custom_refspec() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, clone) = init_origin_and_clone(tmp.path());

        // Point the fetch refspec somewhere other than refs/remotes/origin/*,
        // which leaves refs/remotes/origin/<branch> frozen at clone time.
        git(
            &clone,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/fork/*",
            ],
        );
        let stale = head_sha(&clone);
        let latest = commit_file(&origin, "b.txt", "two");

        let base = resolve_base(&clone, &auto());
        assert_eq!(
            base.rev.as_deref(),
            Some(latest.as_str()),
            "should read FETCH_HEAD, not the stale remote-tracking ref"
        );
        assert_ne!(base.rev.as_deref(), Some(stale.as_str()));
    }

    #[test]
    fn test_resolve_base_without_fetch_uses_stale_tracking_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, clone) = init_origin_and_clone(tmp.path());
        let at_clone_time = head_sha(&clone);
        let latest = commit_file(&origin, "b.txt", "two");

        let settings = BaseSettings {
            spec: format!("origin/{}", current_branch(&origin)),
            fetch: false,
        };
        let base = resolve_base(&clone, &settings);
        assert_eq!(base.rev.as_deref(), Some(at_clone_time.as_str()));
        assert_ne!(base.rev.as_deref(), Some(latest.as_str()));
        assert!(base.warning.is_none());
    }

    #[test]
    fn test_resolve_base_offline_remote_falls_back_with_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, clone) = init_origin_and_clone(tmp.path());
        let at_clone_time = head_sha(&clone);

        // Repoint origin at a path that does not exist to simulate being offline.
        git(
            &clone,
            &["remote", "set-url", "origin", "/nonexistent/repo.git"],
        );

        let base = resolve_base(&clone, &auto());
        assert_eq!(
            base.rev.as_deref(),
            Some(at_clone_time.as_str()),
            "should degrade to the local remote-tracking ref"
        );
        let warning = base.warning.expect("expected a fetch warning");
        assert!(warning.contains("fetch failed"), "got: {warning}");
    }

    #[test]
    fn test_create_worktree_uses_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_test_repo(&repo);
        let first = commit_file(&repo, "a.txt", "one");
        let second = commit_file(&repo, "b.txt", "two");
        assert_ne!(first, second);

        let wt_path = repo.join(".worktrees").join("based");
        create_worktree(&repo, "based", &wt_path, "nexus/based", Some(&first), None).unwrap();

        assert_eq!(
            head_sha(&wt_path),
            first,
            "worktree should start at the requested base, not the primary HEAD"
        );
    }

    #[test]
    fn test_create_worktree_without_base_uses_primary_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_test_repo(&repo);
        commit_file(&repo, "a.txt", "one");
        let second = commit_file(&repo, "b.txt", "two");

        let wt_path = repo.join(".worktrees").join("unbased");
        create_worktree(&repo, "unbased", &wt_path, "nexus/unbased", None, None).unwrap();

        assert_eq!(head_sha(&wt_path), second);
    }

    #[test]
    fn test_hook_env_vars_includes_base_ref() {
        let vars = hook_env_vars(
            Path::new("/repo"),
            Path::new("/repo/.worktrees/x"),
            "nexus/x",
            "x",
            Some("abc123"),
        );
        let base = vars.iter().find(|(k, _)| k == "NEXUS_BASE_REF").unwrap();
        assert_eq!(base.1, "abc123");
    }

    #[test]
    fn test_hook_env_vars_base_ref_empty_when_head() {
        let vars = hook_env_vars(
            Path::new("/repo"),
            Path::new("/repo/.worktrees/x"),
            "nexus/x",
            "x",
            None,
        );
        let base = vars.iter().find(|(k, _)| k == "NEXUS_BASE_REF").unwrap();
        assert_eq!(base.1, "");
    }

    #[test]
    fn test_branch_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        init_test_repo(path);

        // Default branch should exist
        let default_branch = Command::new("git")
            .args(["-C", path.to_str().unwrap(), "branch", "--show-current"])
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        let default = String::from_utf8_lossy(&default_branch.stdout)
            .trim()
            .to_string();
        assert!(branch_exists(path, &default));

        // Non-existent branch
        assert!(!branch_exists(path, "nexus/does-not-exist"));
    }

    #[test]
    fn test_create_and_remove_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_test_repo(&repo);

        let wt_path = repo.join(".worktrees").join("test-session");
        let branch = "nexus/test-session";

        // Create
        create_worktree(&repo, "test-session", &wt_path, branch, None, None).unwrap();
        assert!(wt_path.exists());
        assert!(branch_exists(&repo, branch));

        // Remove
        remove_worktree(&repo, &wt_path, branch, None).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_remove_worktree_missing_path_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let non_existent = tmp.path().join("does-not-exist");
        // Should return Ok when path doesn't exist
        remove_worktree(tmp.path(), &non_existent, "nexus/whatever", None).unwrap();
    }

    #[test]
    fn test_resolve_hook_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_hook(tmp.path(), "on-worktree-create").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_hook_not_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let nexus_dir = tmp.path().join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let hook = nexus_dir.join("on-worktree-create");
        std::fs::write(&hook, "#!/bin/bash\necho hello").unwrap();
        // NOT setting executable bit
        assert!(resolve_hook(tmp.path(), "on-worktree-create").is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_hook_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let nexus_dir = tmp.path().join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let hook = nexus_dir.join("on-worktree-create");
        std::fs::write(&hook, "#!/bin/bash\necho hello").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = resolve_hook(tmp.path(), "on-worktree-create");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), hook);
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_hook_symlink_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let nexus_dir = tmp.path().join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();

        // Create a real file
        let real_hook = tmp.path().join("real-hook");
        std::fs::write(&real_hook, "#!/bin/bash\necho hello").unwrap();
        std::fs::set_permissions(&real_hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Create symlink
        let hook_link = nexus_dir.join("on-worktree-create");
        std::os::unix::fs::symlink(&real_hook, &hook_link).unwrap();

        assert!(resolve_hook(tmp.path(), "on-worktree-create").is_none());
    }

    // --- resolve_hook_path priority chain ---

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_hook_path_repo_config_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Convention hook
        let nexus_dir = repo.join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let convention = nexus_dir.join("on-worktree-create");
        std::fs::write(&convention, "#!/bin/bash\necho convention").unwrap();
        make_executable(&convention);

        // Repo-config hook
        let scripts = repo.join("scripts");
        std::fs::create_dir(&scripts).unwrap();
        let repo_hook = scripts.join("create.sh");
        std::fs::write(&repo_hook, "#!/bin/bash\necho repo").unwrap();
        make_executable(&repo_hook);

        // Write .nexus.toml pointing to repo hook
        std::fs::write(
            repo.join(".nexus.toml"),
            "[worktree]\non_create = \"scripts/create.sh\"\n",
        )
        .unwrap();

        let result = resolve_hook_path(repo, "on-worktree-create", Some("/global/hook.sh"));
        assert_eq!(result, Some(repo.join("scripts/create.sh")));
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_hook_path_global_config_over_convention() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Convention hook
        let nexus_dir = repo.join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let convention = nexus_dir.join("on-worktree-create");
        std::fs::write(&convention, "#!/bin/bash\necho convention").unwrap();
        make_executable(&convention);

        // Global config hook (use an absolute path in tmp)
        let global_hook = tmp.path().join("global-hook.sh");
        std::fs::write(&global_hook, "#!/bin/bash\necho global").unwrap();
        make_executable(&global_hook);

        let result = resolve_hook_path(
            repo,
            "on-worktree-create",
            Some(global_hook.to_str().unwrap()),
        );
        assert_eq!(result, Some(global_hook));
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_hook_path_convention_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        let nexus_dir = repo.join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let convention = nexus_dir.join("on-worktree-create");
        std::fs::write(&convention, "#!/bin/bash\necho convention").unwrap();
        make_executable(&convention);

        let result = resolve_hook_path(repo, "on-worktree-create", None);
        assert_eq!(result, Some(convention));
    }

    #[test]
    fn test_resolve_hook_path_no_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_hook_path(tmp.path(), "on-worktree-create", None);
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_hook_path_repo_config_invalid_no_fallthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Convention hook exists
        let nexus_dir = repo.join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let convention = nexus_dir.join("on-worktree-create");
        std::fs::write(&convention, "#!/bin/bash\necho convention").unwrap();
        #[cfg(unix)]
        make_executable(&convention);

        // Repo config points to non-existent file — should NOT fall through to convention
        std::fs::write(
            repo.join(".nexus.toml"),
            "[worktree]\non_create = \"nonexistent.sh\"\n",
        )
        .unwrap();
        let result = resolve_hook_path(repo, "on-worktree-create", None);
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_hook_path_global_config_invalid_no_fallthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Convention hook exists
        let nexus_dir = repo.join(".nexus");
        std::fs::create_dir(&nexus_dir).unwrap();
        let convention = nexus_dir.join("on-worktree-create");
        std::fs::write(&convention, "#!/bin/bash\necho convention").unwrap();
        #[cfg(unix)]
        make_executable(&convention);

        // Global config points to non-existent file — should NOT fall through
        let result = resolve_hook_path(repo, "on-worktree-create", Some("/nonexistent/hook.sh"));
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_hook_path_repo_config_path_traversal_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();

        // Write .nexus.toml with path traversal attempt
        std::fs::write(
            repo.join(".nexus.toml"),
            "[worktree]\non_create = \"../../../etc/evil.sh\"\n",
        )
        .unwrap();

        let result = resolve_hook_path(repo, "on-worktree-create", None);
        assert!(result.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_validate_hook_file_rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-a-file");
        std::fs::create_dir(&dir).unwrap();
        // Directories should not pass validation
        assert!(!validate_hook_file(&dir));
    }

    #[test]
    fn test_expand_tilde() {
        let expanded = expand_tilde("~/scripts/hook.sh");
        assert!(!expanded.to_string_lossy().starts_with('~'));
        assert!(expanded.to_string_lossy().ends_with("scripts/hook.sh"));

        // Absolute path unchanged
        let abs = expand_tilde("/usr/bin/hook.sh");
        assert_eq!(abs, PathBuf::from("/usr/bin/hook.sh"));
    }
}
