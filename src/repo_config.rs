use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct RepoConfig {
    #[serde(default)]
    pub worktree: RepoWorktreeConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoWorktreeConfig {
    #[serde(default)]
    pub branch_prefix: Option<String>,
    /// Where new worktree branches start: `auto`, `HEAD`, or a git revision.
    #[serde(default)]
    pub base: Option<String>,
    /// Fetch a remote base before branching from it. Defaults to true.
    #[serde(default)]
    pub fetch: Option<bool>,
    #[serde(default)]
    pub on_create: Option<String>,
    #[serde(default)]
    pub on_teardown: Option<String>,
}

/// Load per-repo config from `.nexus.toml` at the repo root.
/// Returns defaults silently if the file is missing or malformed.
pub fn load_repo_config(repo_root: &Path) -> RepoConfig {
    let path = repo_root.join(".nexus.toml");
    if !path.exists() {
        return RepoConfig::default();
    }
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return RepoConfig::default();
    };
    toml::from_str(&raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_repo_config(tmp.path());
        assert!(cfg.worktree.branch_prefix.is_none());
    }

    #[test]
    fn test_load_with_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".nexus.toml"),
            "[worktree]\nbranch_prefix = \"custom\"\n",
        )
        .unwrap();
        let cfg = load_repo_config(tmp.path());
        assert_eq!(cfg.worktree.branch_prefix, Some("custom".to_string()));
    }

    #[test]
    fn test_load_with_empty_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".nexus.toml"),
            "[worktree]\nbranch_prefix = \"\"\n",
        )
        .unwrap();
        let cfg = load_repo_config(tmp.path());
        assert_eq!(cfg.worktree.branch_prefix, Some(String::new()));
    }

    #[test]
    fn test_load_with_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".nexus.toml"),
            "[worktree]\non_create = \"hooks/create.sh\"\non_teardown = \"hooks/teardown.sh\"\n",
        )
        .unwrap();
        let cfg = load_repo_config(tmp.path());
        assert_eq!(cfg.worktree.on_create, Some("hooks/create.sh".to_string()));
        assert_eq!(
            cfg.worktree.on_teardown,
            Some("hooks/teardown.sh".to_string())
        );
    }

    #[test]
    fn test_load_hooks_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nexus.toml"), "[worktree]\n").unwrap();
        let cfg = load_repo_config(tmp.path());
        assert!(cfg.worktree.on_create.is_none());
        assert!(cfg.worktree.on_teardown.is_none());
    }

    #[test]
    fn test_load_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nexus.toml"), "[invalid\n").unwrap();
        let cfg = load_repo_config(tmp.path());
        assert!(cfg.worktree.branch_prefix.is_none());
    }

    #[test]
    fn test_load_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nexus.toml"), "").unwrap();
        let cfg = load_repo_config(tmp.path());
        assert!(cfg.worktree.branch_prefix.is_none());
    }

    #[test]
    fn test_load_base_and_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".nexus.toml"),
            "[worktree]\nbase = \"upstream/main\"\nfetch = false\n",
        )
        .unwrap();
        let cfg = load_repo_config(tmp.path());
        assert_eq!(cfg.worktree.base, Some("upstream/main".to_string()));
        assert_eq!(cfg.worktree.fetch, Some(false));
    }

    #[test]
    fn test_base_and_fetch_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nexus.toml"), "[worktree]\n").unwrap();
        let cfg = load_repo_config(tmp.path());
        assert!(cfg.worktree.base.is_none());
        assert!(cfg.worktree.fetch.is_none());
    }
}
