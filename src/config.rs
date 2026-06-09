use std::path::PathBuf;

use color_eyre::eyre::bail;
use color_eyre::Result;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct NexusConfig {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub groups: Vec<GroupDef>,
    #[serde(default)]
    pub tmux: TmuxConfig,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    /// User-defined session launch types, offered in the new-session picker
    /// alongside the built-in Claude and Custom entries.
    #[serde(default)]
    pub session_types: Vec<SessionTypeDef>,
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroupDef {
    pub name: String,
    #[serde(default)]
    pub icon: String,
}

/// A named launch profile: a label shown in the picker and the command tmux runs.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionTypeDef {
    pub name: String,
    pub cmd: String,
}

#[derive(Debug, Deserialize)]
pub struct TmuxConfig {
    #[serde(default = "default_socket_name")]
    pub socket_name: String,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            socket_name: default_socket_name(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct WorktreeConfig {
    #[serde(default)]
    pub branch_prefix: Option<String>,
    #[serde(default)]
    pub on_create: Option<String>,
    #[serde(default)]
    pub on_teardown: Option<String>,
}

// ---------------------------------------------------------------------------
// Default value helpers
// ---------------------------------------------------------------------------

fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .expect("Cannot determine data directory. Set $HOME or XDG_DATA_HOME.")
        .join("nexus/nexus.db")
}

fn default_socket_name() -> String {
    "nexus".to_string()
}

// ---------------------------------------------------------------------------
// Config file path
// ---------------------------------------------------------------------------

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("nexus/config.toml"))
}

// ---------------------------------------------------------------------------
// Loading & validation
// ---------------------------------------------------------------------------

pub fn load_config() -> Result<NexusConfig> {
    let path = match config_path() {
        Some(p) => p,
        None => return Ok(NexusConfig::default()),
    };

    if !path.exists() {
        return Ok(NexusConfig::default());
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| color_eyre::eyre::eyre!("failed to read config at {}: {e}", path.display()))?;

    parse_and_validate(&raw)
        .map_err(|e| color_eyre::eyre::eyre!("invalid config at {}: {e}", path.display()))
}

pub fn parse_and_validate(toml_str: &str) -> Result<NexusConfig> {
    let config: NexusConfig =
        toml::from_str(toml_str).map_err(|e| color_eyre::eyre::eyre!("TOML parse error: {e}"))?;

    validate(&config)?;
    Ok(config)
}

fn validate(config: &NexusConfig) -> Result<()> {
    for (i, g) in config.groups.iter().enumerate() {
        if g.name.trim().is_empty() {
            bail!("groups[{}].name must not be empty", i);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let cfg = NexusConfig::default();
        assert!(validate(&cfg).is_ok());
        assert!(cfg.general.db_path.ends_with("nexus/nexus.db"));
        assert_eq!(cfg.tmux.socket_name, "nexus");
        assert!(cfg.groups.is_empty());
    }

    #[test]
    fn test_parse_full_toml() {
        let toml_str = r#"
[general]
db_path = "/custom/db/nexus.db"

[[groups]]
name = "Work"
icon = "briefcase"

[[groups]]
name = "Personal"
icon = "home"

[tmux]
socket_name = "custom-nexus"
"#;

        let cfg = parse_and_validate(toml_str).unwrap();
        assert_eq!(cfg.general.db_path, PathBuf::from("/custom/db/nexus.db"));
        assert_eq!(cfg.groups.len(), 2);
        assert_eq!(cfg.groups[0].name, "Work");
        assert_eq!(cfg.groups[0].icon, "briefcase");
        assert_eq!(cfg.groups[1].name, "Personal");
        assert_eq!(cfg.tmux.socket_name, "custom-nexus");
    }

    #[test]
    fn test_empty_toml_returns_defaults() {
        let cfg = parse_and_validate("").unwrap();
        assert!(cfg.groups.is_empty());
    }

    #[test]
    fn test_missing_file_returns_defaults() {
        let cfg = load_config().unwrap();
        assert!(cfg.general.db_path.ends_with("nexus/nexus.db"));
    }

    #[test]
    fn test_empty_group_name() {
        let toml_str = r#"
[[groups]]
name = "  "
icon = "x"
"#;
        let err = parse_and_validate(toml_str);
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("name"), "error: {msg}");
    }

    #[test]
    fn test_partial_config_fills_defaults() {
        let toml_str = r#"
[general]
"#;
        let cfg = parse_and_validate(toml_str).unwrap();
        assert!(cfg.general.db_path.ends_with("nexus/nexus.db"));
        assert_eq!(cfg.tmux.socket_name, "nexus");
    }

    #[test]
    fn test_worktree_prefix_configured() {
        let toml_str = r#"
[worktree]
branch_prefix = "team"
"#;
        let cfg = parse_and_validate(toml_str).unwrap();
        assert_eq!(cfg.worktree.branch_prefix, Some("team".to_string()));
    }

    #[test]
    fn test_worktree_prefix_empty_string() {
        let toml_str = r#"
[worktree]
branch_prefix = ""
"#;
        let cfg = parse_and_validate(toml_str).unwrap();
        assert_eq!(cfg.worktree.branch_prefix, Some(String::new()));
    }

    #[test]
    fn test_worktree_prefix_absent() {
        let cfg = parse_and_validate("").unwrap();
        assert_eq!(cfg.worktree.branch_prefix, None);
    }

    #[test]
    fn test_worktree_hooks_configured() {
        let toml_str = r#"
[worktree]
on_create = "scripts/wt-create.sh"
on_teardown = "scripts/wt-teardown.sh"
"#;
        let cfg = parse_and_validate(toml_str).unwrap();
        assert_eq!(
            cfg.worktree.on_create,
            Some("scripts/wt-create.sh".to_string())
        );
        assert_eq!(
            cfg.worktree.on_teardown,
            Some("scripts/wt-teardown.sh".to_string())
        );
    }

    #[test]
    fn test_worktree_hooks_absent() {
        let cfg = parse_and_validate("").unwrap();
        assert!(cfg.worktree.on_create.is_none());
        assert!(cfg.worktree.on_teardown.is_none());
    }

    #[test]
    fn test_session_types_parsed() {
        let toml_str = r#"
[[session_types]]
name = "Cook"
cmd = "bash"

[[session_types]]
name = "Notes"
cmd = "nvim ~/notes.md"
"#;
        let cfg = parse_and_validate(toml_str).unwrap();
        assert_eq!(cfg.session_types.len(), 2);
        assert_eq!(cfg.session_types[0].name, "Cook");
        assert_eq!(cfg.session_types[0].cmd, "bash");
        assert_eq!(cfg.session_types[1].cmd, "nvim ~/notes.md");
    }

    #[test]
    fn test_session_types_absent() {
        let cfg = parse_and_validate("").unwrap();
        assert!(cfg.session_types.is_empty());
    }

    #[test]
    fn test_invalid_toml_syntax() {
        let bad = r#"
[general
tick_rate_ms = 100
"#;
        let err = parse_and_validate(bad);
        assert!(err.is_err());
    }
}
