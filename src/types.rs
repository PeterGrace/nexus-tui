use std::path::PathBuf;

use ratatui::text::Text;

/// Worktree metadata for isolated sessions. All-or-nothing: if present, all fields are set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeInfo {
    pub branch: String,
    pub repo_root: PathBuf,
}

pub type SessionId = String;
pub type GroupId = i64;

#[derive(Debug, Clone, PartialEq)]
pub enum SelectionTarget {
    Session(SessionId),
    Group(GroupId),
}

#[derive(Debug, Default)]
pub struct SelectionState {
    pub selected: Option<SelectionTarget>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupNode {
    pub id: GroupId,
    pub name: String,
    pub icon: GroupIcon,
    pub children: Vec<TreeNode>,
}

// `SessionSummary` is a flat, frequently-constructed-and-matched value held
// throughout the tree; boxing every session node to save a handful of bytes
// over the size-difference threshold would add indirection at 18 call sites
// for no real benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize)]
pub enum TreeNode {
    Group(GroupNode),
    Session(SessionSummary),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SessionStatus {
    Active,
    Detached,
    Dead,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Detached => "detached",
            SessionStatus::Dead => "dead",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => SessionStatus::Active,
            "detached" => SessionStatus::Detached,
            _ => SessionStatus::Dead,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SessionOrigin {
    Nexus,
    Scanner,
}

impl SessionOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionOrigin::Nexus => "nexus",
            SessionOrigin::Scanner => "scanner",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "nexus" => SessionOrigin::Nexus,
            _ => SessionOrigin::Scanner,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    TextInput,
    Confirm,
    GroupPicker,
    TypePicker,
    Finder,
}

#[derive(Debug, Clone)]
pub enum InputContext {
    NewSessionName,
    NewSessionCwd {
        name: String,
    },
    RenameSession {
        session_id: String,
    },
    RenameGroup {
        group_id: GroupId,
    },
    NewGroupName,
    NewSessionWorktree {
        name: String,
        cwd: String,
        repo_root: PathBuf,
    },
    ConfirmDeleteSession {
        session_id: String,
        tmux_name: Option<String>,
        worktree: Option<WorktreeInfo>,
    },
    ConfirmDeleteGroup {
        group_id: GroupId,
    },
    MoveSession {
        session_id: String,
    },
    NewSessionGroup {
        name: String,
        cwd: String,
        repo_root: Option<PathBuf>,
    },
    /// Final wizard step: pick which command the session launches.
    NewSessionType {
        name: String,
        cwd: String,
        repo_root: Option<PathBuf>,
        group_id: Option<GroupId>,
    },
    /// Free-text command entry, reached from the "Custom" type choice.
    NewSessionCommand {
        name: String,
        cwd: String,
        repo_root: Option<PathBuf>,
        group_id: Option<GroupId>,
    },
}

/// Nexus commands triggered by Alt+key in the interactor.
#[derive(Debug)]
pub enum NexusCommand {
    CursorDown,
    CursorUp,
    ToggleExpand,
    NewSession,
    DeleteSelected,
    RenameSelected,
    MoveSession,
    NewGroup,
    KillTmux,
    ToggleHelp,
    Quit,
    ToggleDeadSessions,
    NextTheme,
    PrevTheme,
    OpenLazygit,
    OpenEditor,
    OpenFinder,
}

/// Result from routing an event through the interactor.
#[derive(Debug)]
pub enum RouteResult {
    /// Event was handled locally (scroll, tmux forward, paste).
    Handled,
    /// Event is a nexus command that App should dispatch.
    NexusCommand(NexusCommand),
    /// Event was ignored (modal overlay active, unrecognized key).
    Ignored,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub display_name: String,
    pub cwd: Option<PathBuf>,
    pub last_active: String,
    pub is_active: bool,
    pub status: SessionStatus,
    pub tmux_name: Option<String>,
    pub created_by: SessionOrigin,
    pub created_at: String,
    pub claude_session_id: Option<String>,
    pub worktree: Option<WorktreeInfo>,
    /// The command this session launches in tmux (e.g. `claude`, `bash`).
    /// Only `claude` sessions get conversation resume.
    pub launch_command: String,
    #[serde(skip)]
    pub jsonl_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum GroupIcon {
    Root,
    SubGroup,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TmuxSessionInfo {
    pub session_id: SessionId,
    pub window_name: String,
    pub is_active: bool,
    pub status: TmuxSessionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TmuxSessionStatus {
    Running,
    Idle,
}

// ---------------------------------------------------------------------------
// Session content — what the interactor panel displays
// ---------------------------------------------------------------------------

/// Content to display in the session interactor panel.
pub enum SessionContent {
    /// Live terminal content, pre-parsed by capture worker thread.
    Live(Text<'static>),
    /// Pre-rendered conversation log from JSONL for sessions without a tmux pane.
    ConversationLog(Text<'static>),
}

/// A single conversation turn from the JSONL log.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub role: Role,
    pub content: String,
}

/// Role in a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Human,
    Assistant,
}

// ---------------------------------------------------------------------------
// Theme / panel types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)] // Variants used via style_for() match + test coverage
pub enum ThemeElement {
    Background,
    Surface,
    Text,
    Dim,
    Primary,
    Secondary,
    Hazard,
    Accent,
    Border,
    ActiveSession,
    IdleSession,
    SelectedItem,
    FocusedBorder,
    UnfocusedBorder,
    TreeIndent,
    TopBarLabel,
    TopBarValue,
    InteractorTitle,
    ConversationHuman,
    ConversationAssistant,
    LogoAgent,
    LogoNexus,
    WorktreeBranch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelType {
    TopBar,
    SessionTree,
    SessionInteractor,
    Logo,
}

/// Mouse text selection state for the interactor panel.
/// Coordinates are absolute screen positions (col, row).
pub struct TextSelection {
    pub anchor: (u16, u16),
    pub end: (u16, u16),
}

impl TextSelection {
    /// Return (start, end) with start <= end in reading order.
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor.1 < self.end.1
            || (self.anchor.1 == self.end.1 && self.anchor.0 <= self.end.0)
        {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// True if the selection spans at least one character.
    pub fn is_nonempty(&self) -> bool {
        self.anchor != self.end
    }
}
