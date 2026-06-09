mod ansi;
mod app;
mod capture_worker;
mod cli;
pub(crate) mod config;
mod conversation;
pub(crate) mod db;
mod feedback_scanner;
pub(crate) mod git;
#[cfg(test)]
mod mock;
mod path_complete;
mod repo_config;
mod theme;
mod time_utils;
pub(crate) mod tmux;
pub(crate) mod types;
mod ui;
mod update_checker;
pub(crate) mod widgets;

use clap::Parser;
use color_eyre::eyre::WrapErr;
use color_eyre::Result;
use tmux::sanitize_tmux_name;

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = cli::Cli::parse();

    if let Some(command) = cli.command {
        run_cli(command, cli.json)
    } else {
        run_tui()
    }
}

fn run_cli(command: cli::Commands, json: bool) -> Result<()> {
    let config = config::load_config()?;
    let db = db::Database::open(&config.general.db_path)?;
    // Clean up stale worktree references (crash recovery)
    let _ = db.reconcile_worktrees();

    match command {
        cli::Commands::List { all } => {
            let tree = db.get_visible_tree(all)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tree)?);
            } else {
                print_tree(&tree, 0);
            }
        }
        cli::Commands::Show { session_id } => {
            let tree = db.get_tree()?;
            let session = find_session_in_tree(&tree, &session_id);
            match session {
                Some(s) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(s)?);
                    } else {
                        print_session_detail(s);
                    }
                }
                None => {
                    color_eyre::eyre::bail!("Session '{}' not found", session_id);
                }
            }
        }
        cli::Commands::New {
            name,
            cwd,
            group,
            worktree,
            command,
        } => {
            let _lock = acquire_lock()?;
            let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
            let tmux_name = sanitize_tmux_name(&name);
            let tmux_name = db.next_unique_tmux_name(&tmux_name, None)?;
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "/tmp".to_string())
            });

            let wt_info = if worktree {
                let repo = git::detect_repo(&cwd).ok_or_else(|| {
                    color_eyre::eyre::eyre!("'{}' is not inside a git repository", cwd)
                })?;
                let prefix = git::resolve_branch_prefix(
                    &repo.root,
                    config.worktree.branch_prefix.as_deref(),
                );
                let branch = git::sanitize_branch_name(&name, &prefix);
                if git::branch_exists(&repo.root, &branch) {
                    color_eyre::eyre::bail!("Branch '{}' already exists", branch);
                }
                let sanitized_dir = branch.replace('/', "-");
                let wt_path = repo.root.join(".worktrees").join(&sanitized_dir);
                let create_hook = git::resolve_hook_path(
                    &repo.root,
                    "on-worktree-create",
                    config.worktree.on_create.as_deref(),
                );
                git::create_worktree(&repo.root, &name, &wt_path, &branch, create_hook.as_deref())?;
                Some((
                    wt_path.to_string_lossy().to_string(),
                    types::WorktreeInfo {
                        branch,
                        repo_root: repo.root,
                    },
                ))
            } else {
                None
            };

            let (session_cwd, wt_ref) = match &wt_info {
                Some((wt_cwd, info)) => (wt_cwd.as_str(), Some(info)),
                None => (cwd.as_str(), None),
            };

            let id = db.create_nexus_session(&name, session_cwd, &tmux_name, &command, wt_ref)?;

            if let Some(group_name) = group {
                let gid = match db.get_group_id_by_name(&group_name)? {
                    Some(gid) => gid,
                    None => db.create_group(&group_name, "")?,
                };
                db.assign_session_to_group(&id, gid)?;
            }

            if tmux.is_available() {
                tmux.launch_session(&tmux_name, session_cwd, &command, None)?;
            }
            println!("Created session '{}' ({})", name, id);
        }
        cli::Commands::Launch { session_id } => {
            let _lock = acquire_lock()?;
            let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
            if !tmux.is_available() {
                color_eyre::eyre::bail!("tmux is not available");
            }
            let cwd = db
                .get_session_cwd(&session_id)?
                .ok_or_else(|| color_eyre::eyre::eyre!("Session '{}' has no cwd", session_id))?;
            let name = sanitize_tmux_name(&session_id);
            let tree = db.get_visible_tree(true)?;
            let session = find_session_in_tree(&tree, &session_id);
            let command = session
                .map(|s| s.launch_command.clone())
                .unwrap_or_else(|| "claude".to_string());
            // Only Claude sessions resume a prior conversation.
            let resume_id = if command == "claude" {
                session.and_then(|s| s.claude_session_id.clone())
            } else {
                None
            };
            tmux.launch_session(&name, &cwd, &command, resume_id.as_deref())?;
            db.update_session_status(&session_id, types::SessionStatus::Active)?;
            println!("Launched session '{}'", session_id);
        }
        cli::Commands::Kill { session_name } => {
            let _lock = acquire_lock()?;
            let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
            tmux.kill_session(&session_name)?;
            // Try to find and update DB status by tmux name
            println!("Killed session '{}'", session_name);
        }
        cli::Commands::Groups => {
            let tree = db.get_tree()?;
            let groups: Vec<&types::GroupNode> = tree
                .iter()
                .filter_map(|n| {
                    if let types::TreeNode::Group(g) = n {
                        Some(g)
                    } else {
                        None
                    }
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&groups)?);
            } else {
                for g in &groups {
                    println!("{} ({} sessions)", g.name, g.children.len());
                }
            }
        }
        cli::Commands::Send { session_name, text } => {
            let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
            if !tmux.is_available() {
                color_eyre::eyre::bail!("tmux is not available");
            }
            tmux.send_keys(&session_name, &tmux::SendKeysArgs::Literal(text))?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "sent", "session": session_name})
                );
            } else {
                println!("Sent to '{}'", session_name);
            }
        }
        cli::Commands::Capture {
            session_name,
            strip,
        } => {
            let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
            if !tmux.is_available() {
                color_eyre::eyre::bail!("tmux is not available");
            }
            let raw = tmux.capture_pane(&session_name)?;
            if strip {
                let sanitized = ansi::sanitize_ansi(raw.as_bytes());
                // Remove all remaining escape sequences for plain text
                let plain = String::from_utf8_lossy(&sanitized).replace('\x1b', "");
                print!("{}", plain);
            } else {
                print!("{}", raw);
            }
        }
        cli::Commands::Delete {
            session_id,
            remove_worktree,
        } => {
            let _lock = acquire_lock()?;
            // Kill tmux session if active
            let tree = db.get_tree()?;
            let session = find_session_in_tree(&tree, &session_id);
            if let Some(s) = session {
                if s.status == types::SessionStatus::Active {
                    if let Some(ref tmux_name) = s.tmux_name {
                        let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
                        let _ = tmux.kill_session(tmux_name);
                    }
                }
                // Handle worktree cleanup
                if let Some(ref wt) = s.worktree {
                    if remove_worktree {
                        let wt_path = s.cwd.as_deref();
                        if let Some(wt_path) = wt_path {
                            let teardown_hook = git::resolve_hook_path(
                                &wt.repo_root,
                                "on-worktree-teardown",
                                config.worktree.on_teardown.as_deref(),
                            );
                            git::remove_worktree(
                                &wt.repo_root,
                                wt_path,
                                &wt.branch,
                                teardown_hook.as_deref(),
                            )?;
                        }
                    } else {
                        eprintln!(
                            "Warning: session has a worktree. Use --remove-worktree to also clean it up."
                        );
                    }
                }
            }
            db.delete_session(&session_id)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "deleted", "session": session_id})
                );
            } else {
                println!("Deleted session '{}'", session_id);
            }
        }
        cli::Commands::Rename { session_id, name } => {
            let _lock = acquire_lock()?;
            let new_tmux_name = tmux::sanitize_tmux_name(&name);
            // Rename the live tmux session if it exists
            let tree = db.get_tree()?;
            if let Some(session) = find_session_in_tree(&tree, &session_id) {
                if let Some(ref old_tmux) = session.tmux_name {
                    let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
                    let _ = tmux.rename_session(old_tmux, &new_tmux_name);
                }
            }
            db.update_session_name(&session_id, &name, &new_tmux_name)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "renamed", "session": session_id, "name": name})
                );
            } else {
                println!("Renamed session '{}' to '{}'", session_id, name);
            }
        }
        cli::Commands::Move { session_id, group } => {
            let _lock = acquire_lock()?;
            let gid = match db.get_group_id_by_name(&group)? {
                Some(gid) => gid,
                None => color_eyre::eyre::bail!("Group '{}' not found", group),
            };
            db.move_session_to_group(&session_id, gid)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "moved", "session": session_id, "group": group})
                );
            } else {
                println!("Moved session '{}' to group '{}'", session_id, group);
            }
        }
        cli::Commands::GroupCreate { name } => {
            let _lock = acquire_lock()?;
            let gid = db.create_group(&name, "")?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "created", "group": name, "id": gid})
                );
            } else {
                println!("Created group '{}' (id: {})", name, gid);
            }
        }
        cli::Commands::Update => {
            run_update()?;
        }
    }
    Ok(())
}

fn run_tui() -> Result<()> {
    let _lock = acquire_lock()?;
    let config = config::load_config()?;
    let db = db::Database::open(&config.general.db_path)?;

    for group_def in &config.groups {
        let icon = if group_def.icon.is_empty() {
            "◈"
        } else {
            &group_def.icon
        };
        if let Err(e) = db.create_group(&group_def.name, icon) {
            // UNIQUE constraint violation means group already exists -- ignore
            if !e.to_string().contains("UNIQUE") {
                return Err(e);
            }
        }
    }

    // Clean up stale worktree references (crash recovery)
    let _ = db.reconcile_worktrees();

    let tree = db.get_visible_tree(false)?;
    let tmux = tmux::TmuxManager::new(&config.tmux.socket_name);
    let tmux_available = tmux.is_available();

    let tmux_sessions = if tmux_available {
        tmux.list_sessions().unwrap_or_default()
    } else {
        vec![]
    };

    let terminal = ratatui::init();
    let result = app::App::new(config, tree, tmux, tmux_available, tmux_sessions, db).run(terminal);
    ratatui::restore();
    result
}

fn find_session_in_tree<'a>(
    tree: &'a [types::TreeNode],
    id: &str,
) -> Option<&'a types::SessionSummary> {
    for node in tree {
        match node {
            types::TreeNode::Session(s) => {
                if s.session_id == id || s.session_id.starts_with(id) {
                    return Some(s);
                }
            }
            types::TreeNode::Group(g) => {
                if let Some(s) = find_session_in_tree(&g.children, id) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn print_tree(tree: &[types::TreeNode], depth: usize) {
    let indent = "  ".repeat(depth);
    for node in tree {
        match node {
            types::TreeNode::Group(g) => {
                println!("{indent}# {} ({} sessions)", g.name, g.children.len());
                print_tree(&g.children, depth + 1);
            }
            types::TreeNode::Session(s) => {
                let status_icon = match s.status {
                    types::SessionStatus::Active => "+",
                    types::SessionStatus::Detached => "~",
                    types::SessionStatus::Dead => "-",
                };
                let wt_badge = s
                    .worktree
                    .as_ref()
                    .map(|wt| format!(" [{}]", wt.branch))
                    .unwrap_or_default();
                println!(
                    "{indent}{status_icon} {} [{}]{wt_badge}",
                    s.display_name, s.last_active
                );
            }
        }
    }
}

fn print_session_detail(s: &types::SessionSummary) {
    println!("Session: {}", s.session_id);
    println!("Name:    {}", s.display_name);
    if let Some(ref cwd) = s.cwd {
        println!("CWD:     {}", cwd.display());
    }
    println!("Status:  {}", s.status.as_str());
    println!("Origin:  {}", s.created_by.as_str());
    if let Some(ref tmux) = s.tmux_name {
        println!("Tmux:    {}", tmux);
    }
    if let Some(ref wt) = s.worktree {
        println!("Branch:  {}", wt.branch);
        println!("Repo:    {}", wt.repo_root.display());
    }
    println!("Active:  {}", s.is_active);
    println!("Last:    {}", s.last_active);
    println!("Created: {}", s.created_at);
}

fn run_update() -> Result<()> {
    let source_dir = env!("CARGO_MANIFEST_DIR");

    let is_dev_install = std::path::Path::new(source_dir).join(".git").exists()
        && !source_dir.contains(".cargo/git/checkouts/");

    if is_dev_install {
        run_update_dev(source_dir)?;
    } else {
        run_update_git()?;
    }

    // Clear the update_available flag and record checked version in DB
    let config = config::load_config()?;
    let db = db::Database::open(&config.general.db_path)?;
    if let Err(e) = db.set_setting("update_available", "false") {
        eprintln!("Warning: could not clear update flag in database: {e}");
    }
    update_checker::record_post_update(&config.general.db_path);

    println!("\nUpdate complete. Restart nexus to use the new version.");
    Ok(())
}

/// Developer mode: pull latest source and rebuild from local path.
fn run_update_dev(source_dir: &str) -> Result<()> {
    use std::process::Command;

    println!("Updating nexus from local source: {}", source_dir);

    let pull_status = Command::new("git")
        .args(["-C", source_dir, "pull", "origin", "main"])
        .status()
        .wrap_err("failed to run git pull")?;

    if !pull_status.success() {
        color_eyre::eyre::bail!("git pull failed");
    }

    println!("\nRebuilding...");
    let install_status = Command::new("cargo")
        .args(["install", "--path", source_dir])
        .status()
        .wrap_err("failed to run cargo install")?;

    if !install_status.success() {
        color_eyre::eyre::bail!("cargo install failed");
    }

    Ok(())
}

/// User mode: reinstall from the upstream git repository.
fn run_update_git() -> Result<()> {
    use std::process::Command;

    let repo_url = env!("CARGO_PKG_REPOSITORY");
    println!("Updating nexus from {}", repo_url);

    let install_status = Command::new("cargo")
        .args(["install", "--git", repo_url, "--force"])
        .status()
        .wrap_err("failed to run cargo install --git")?;

    if !install_status.success() {
        color_eyre::eyre::bail!("cargo install --git failed");
    }

    Ok(())
}

fn acquire_lock() -> Result<fslock::LockFile> {
    let lock_dir = dirs::cache_dir()
        .ok_or_else(|| {
            color_eyre::eyre::eyre!("Cannot determine cache directory. Set XDG_CACHE_HOME or HOME.")
        })?
        .join("nexus");
    std::fs::create_dir_all(&lock_dir)?;

    let mut lock = fslock::LockFile::open(&lock_dir.join("nexus.lock"))?;

    if !lock.try_lock()? {
        color_eyre::eyre::bail!(
            "Another nexus instance is already running.\n  \
             If this is a stale lock, remove: {}",
            lock_dir.join("nexus.lock").display()
        );
    }

    Ok(lock)
}
