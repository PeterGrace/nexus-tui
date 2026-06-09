use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nexus", version, about = "TUI session manager for Claude Code")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Output in JSON format (for subcommands that support it)
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all sessions
    List {
        /// Include dead/past sessions
        #[arg(long)]
        all: bool,
    },
    /// Show details for a specific session
    Show {
        /// Session ID (or prefix)
        session_id: String,
    },
    /// Create a new session
    New {
        /// Session name
        name: String,
        /// Working directory (defaults to current dir)
        #[arg(short, long)]
        cwd: Option<String>,
        /// Assign to group (created if it doesn't exist)
        #[arg(short, long)]
        group: Option<String>,
        /// Create an isolated git worktree for this session
        #[arg(short = 'w', long)]
        worktree: bool,
        /// Command to launch in the session (default: claude). Only `claude`
        /// sessions resume a prior conversation.
        #[arg(long, default_value = "claude")]
        command: String,
    },
    /// Launch/resume a session in tmux
    Launch {
        /// Session ID
        session_id: String,
    },
    /// Kill a tmux session
    Kill {
        /// Session name in tmux
        session_name: String,
    },
    /// List configured groups
    Groups,
    /// Send text to a tmux session
    Send {
        /// Session name in tmux
        session_name: String,
        /// Text to send (literal)
        text: String,
    },
    /// Capture the contents of a tmux session pane
    Capture {
        /// Session name in tmux
        session_name: String,
        /// Strip ANSI escape sequences from output
        #[arg(long)]
        strip: bool,
    },
    /// Delete a session from the database
    Delete {
        /// Session ID
        session_id: String,
        /// Also remove the git worktree (if session has one)
        #[arg(long)]
        remove_worktree: bool,
    },
    /// Rename a session
    Rename {
        /// Session ID
        session_id: String,
        /// New display name
        name: String,
    },
    /// Move a session to a different group
    Move {
        /// Session ID
        session_id: String,
        /// Target group name
        #[arg(long)]
        group: String,
    },
    /// Create a new group
    #[command(name = "group-create")]
    GroupCreate {
        /// Group name
        name: String,
    },
    /// Update nexus to the latest version from source
    Update,
}
