use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;
use tachyonfx::Effect;

use crate::capture_worker;
use crate::config::NexusConfig;
use crate::db::Database;
use crate::feedback_scanner;
use crate::git;
use crate::theme;
use crate::tmux::{sanitize_tmux_name, TmuxManager};
use crate::types::*;
use crate::ui;
use crate::update_checker;
use crate::widgets::finder_state::FinderState;
use crate::widgets::interactor_state::InteractorState;
use crate::widgets::logo::LogoState;
use crate::widgets::tree_state::{FlatNodeKind, TreeAction, TreeState};

const TICK_RATE: Duration = Duration::from_millis(16);
const ATTENTION_TICK: Duration = Duration::from_millis(50);
const TMUX_POLL_INTERVAL: Duration = Duration::from_secs(2);
const LOGO_FRAME_INTERVAL: Duration = Duration::from_millis(300);
const TREE_WIDTH_PCT_MIN: u16 = 15;
const TREE_WIDTH_PCT_MAX: u16 = 40;
const TREE_WIDTH_PCT_DEFAULT: u16 = 20;
const LOGO_HEIGHT_MIN: u16 = 1;
const LOGO_HEIGHT_MAX: u16 = 40;
const LOGO_HEIGHT_DEFAULT: u16 = 9;

/// Suspend the TUI (alternate screen, raw mode, mouse/paste), run a closure,
/// then restore the TUI. Used by fullscreen attach and lazygit.
fn with_suspended_tui<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);

    let result = f();

    let _ = crossterm::execute!(std::io::stdout(), EnterAlternateScreen);
    let _ = crossterm::terminal::enable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    result
}

pub struct App {
    pub should_quit: bool,
    pub(crate) boot_done: bool,
    last_tick: Instant,
    pub(crate) boot_effects: Vec<Effect>,
    pub(crate) tree: Vec<TreeNode>,
    pub(crate) tree_state: TreeState,
    pub(crate) selection: SelectionState,
    pub(crate) tmux: TmuxManager,
    pub(crate) tmux_available: bool,
    pub(crate) tmux_sessions: Vec<TmuxSessionInfo>,
    last_tmux_poll: Instant,
    #[allow(dead_code)]
    config: NexusConfig,
    pub(crate) db: Database,
    // Cached values (updated on tmux poll and cursor change)
    pub(crate) cached_counts: (usize, usize),
    pub(crate) cached_selected: Option<SessionSummary>,
    // Status message overlay
    pub(crate) status_message: Option<(String, Instant)>,
    // Input state for CRUD operations
    pub(crate) input_mode: InputMode,
    pub(crate) input_buffer: String,
    pub(crate) input_context: Option<InputContext>,
    pub(crate) show_help: bool,
    pub(crate) show_dead_sessions: bool,
    // Group picker state
    pub(crate) picker_groups: Vec<(GroupId, String)>,
    pub(crate) picker_cursor: usize,
    // Path completion state (active during NewSessionCwd input)
    pub(crate) path_suggestions: Vec<String>,
    pub(crate) path_suggestion_cursor: usize,
    // Set after returning from tmux attach to force a full redraw
    needs_full_redraw: bool,
    // Dirty flag: only render when something changed
    dirty: bool,
    // Session interactor state (None if tmux unavailable)
    pub(crate) interactor_state: Option<InteractorState>,
    // Logo animation state
    pub(crate) logo_state: LogoState,
    logo_last_advance: Instant,
    // Pre-launch JSONL snapshots: nexus session_id → set of JSONL stems before launch
    jsonl_snapshots: HashMap<String, HashSet<String>>,
    // Layout rects for mouse hit-testing (populated during draw)
    pub(crate) area_tree: Rect,
    pub(crate) area_theme_label: Rect,
    // Session finder state
    pub(crate) finder_state: FinderState,
    // Mouse text selection in the interactor panel
    pub(crate) text_selection: Option<TextSelection>,
    pub(crate) area_interactor_inner: Rect,
    pub(crate) interactor_rendered_cells: Vec<Vec<String>>,
    // Draggable tree/interactor border
    pub(crate) tree_width_pct: u16,
    pub(crate) dragging_border: bool,
    pub(crate) area_border_x: u16,
    // Draggable tree/logo border
    pub(crate) logo_height: u16,
    pub(crate) dragging_logo_border: bool,
    pub(crate) area_logo_border_y: u16,
    // Feedback scanner: sessions needing user attention (tmux session names)
    pub(crate) attention_sessions: HashSet<String>,
    feedback_rx: Option<mpsc::Receiver<HashSet<String>>>,
    pub(crate) attention_effects: HashMap<String, Effect>,
    // Update checker: is a newer version available upstream?
    pub(crate) update_available: bool,
    update_rx: Option<mpsc::Receiver<bool>>,
    // Pending background worktree creation
    pending_wt_create: Option<PendingWorktreeCreate>,
    // Pending background worktree teardown
    pending_wt_teardown: Option<PendingWorktreeTeardown>,
}

/// Bundled state for a background worktree creation.
struct PendingWorktreeCreate {
    rx: mpsc::Receiver<color_eyre::Result<()>>,
    ctx: PendingWorktreeCtx,
}

/// Context stored while a worktree is being created on a background thread.
struct PendingWorktreeCtx {
    name: String,
    cwd: String, // the worktree path (will become session cwd)
    repo_root: PathBuf,
    branch: String,
    group_id: Option<GroupId>,
}

/// Bundled state for a background worktree teardown.
struct PendingWorktreeTeardown {
    rx: mpsc::Receiver<color_eyre::Result<()>>,
    session_id: String,
}

impl App {
    pub fn new(
        config: NexusConfig,
        tree: Vec<TreeNode>,
        tmux: TmuxManager,
        tmux_available: bool,
        tmux_sessions: Vec<TmuxSessionInfo>,
        db: Database,
    ) -> Self {
        // Restore persisted theme before fx_boot() so boot animation uses correct colors
        if let Ok(Some(idx_str)) = db.get_setting("theme_index") {
            if let Ok(idx) = idx_str.parse::<usize>() {
                theme::set_theme(idx);
            }
        }

        let tree_width_pct = db
            .get_setting("tree_width_pct")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(TREE_WIDTH_PCT_DEFAULT)
            .clamp(TREE_WIDTH_PCT_MIN, TREE_WIDTH_PCT_MAX);

        let logo_height = db
            .get_setting("logo_height")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(LOGO_HEIGHT_DEFAULT)
            .clamp(LOGO_HEIGHT_MIN, LOGO_HEIGHT_MAX);

        let mut tree_state = TreeState::new(&tree);
        let mut selection = SelectionState::default();

        // Restore last-active session cursor position
        if let Ok(Some(last_id)) = db.get_setting("last_session_id") {
            if tree_state.jump_to_session(&last_id, &tree) {
                selection.selected = Some(SelectionTarget::Session(last_id));
            }
        }
        let cached_counts = count_sessions(&tree);

        // Spawn capture worker and feedback scanner if tmux is available
        let (interactor_state, feedback_rx) = if tmux_available {
            // Configure true color + keybindings (no-op if server not yet started)
            if !tmux_sessions.is_empty() {
                let _ = tmux.configure_server();
            }
            let (session_tx, content_rx, nudge_tx) = capture_worker::spawn(tmux.clone());
            let is = InteractorState::new(tmux.clone(), content_rx, session_tx, nudge_tx);
            let frx = feedback_scanner::spawn(tmux.clone());
            (Some(is), Some(frx))
        } else {
            (None, None)
        };

        // Spawn background update checker
        let update_rx = Some(update_checker::spawn(&config.general.db_path));
        let update_available = db
            .get_setting("update_available")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);

        let mut app = Self {
            should_quit: false,
            boot_done: false,
            last_tick: Instant::now(),
            boot_effects: theme::fx_boot(),
            tree,
            tree_state,
            selection,
            tmux,
            tmux_available,
            tmux_sessions,
            last_tmux_poll: Instant::now(),
            config,
            db,
            cached_counts,
            cached_selected: None,
            status_message: None,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            input_context: None,
            show_help: false,
            show_dead_sessions: false,
            picker_groups: Vec::new(),
            picker_cursor: 0,
            path_suggestions: Vec::new(),
            path_suggestion_cursor: 0,
            needs_full_redraw: false,
            dirty: true,
            interactor_state,
            logo_state: LogoState::new(),
            logo_last_advance: Instant::now(),
            jsonl_snapshots: HashMap::new(),
            area_tree: Rect::default(),
            area_theme_label: Rect::default(),
            finder_state: FinderState::new(),
            text_selection: None,
            area_interactor_inner: Rect::default(),
            interactor_rendered_cells: Vec::new(),
            tree_width_pct,
            dragging_border: false,
            area_border_x: 0,
            logo_height,
            dragging_logo_border: false,
            area_logo_border_y: 0,
            attention_sessions: HashSet::new(),
            feedback_rx,
            attention_effects: HashMap::new(),
            pending_wt_create: None,
            pending_wt_teardown: None,
            update_available,
            update_rx,
        };

        // Wire up interactor to the restored session (if any)
        app.refresh_cached_selected();
        app.sync_interactor_to_selection();

        app
    }

    pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        // Enable bracketed paste so crossterm emits Event::Paste
        // Enable mouse capture so we receive scroll wheel events
        let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);

        let result = self.event_loop(&mut terminal);

        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture
        );
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_tick);
            self.last_tick = now;

            // Poll capture worker for new content
            if let Some(ref mut is) = self.interactor_state {
                if is.poll_content() {
                    self.dirty = true;
                    self.text_selection = None;
                }
            }

            // Poll feedback scanner for attention state changes
            let new_attention = self.feedback_rx.as_ref().and_then(|rx| {
                let mut latest = None;
                while let Ok(set) = rx.try_recv() {
                    latest = Some(set);
                }
                latest
            });
            if let Some(set) = new_attention {
                if set != self.attention_sessions {
                    self.attention_sessions = set;
                    self.rebuild_attention_effects();
                    self.dirty = true;
                }
            }

            // Poll update checker
            if let Some(ref rx) = self.update_rx {
                if let Ok(available) = rx.try_recv() {
                    self.update_available = available;
                    self.dirty = true;
                }
            }

            // Poll background worktree operations
            self.poll_worktree_pending();
            self.poll_worktree_teardown();

            // Poll tmux for active sessions periodically
            if self.tmux_available && now.duration_since(self.last_tmux_poll) >= TMUX_POLL_INTERVAL
            {
                self.tmux_sessions = self.tmux.list_sessions().unwrap_or_default();
                self.reconcile_tmux_state();
                self.detect_claude_session_ids();
                self.last_tmux_poll = now;
                self.dirty = true;
            }

            // Advance logo animation frame (skip when GoL hidden)
            if self.logo_height > 2
                && now.duration_since(self.logo_last_advance) >= LOGO_FRAME_INTERVAL
            {
                let term_size = terminal.size()?;
                let logo_w =
                    (term_size.width * self.tree_width_pct / 100).saturating_sub(2) as usize;
                let logo_h = (self.logo_height as usize).saturating_sub(2);
                self.logo_state.advance(logo_w, logo_h);
                self.logo_last_advance = now;
                self.dirty = true;
            }

            // Auto-clear status message after 5 seconds
            if let Some((_, ts)) = &self.status_message {
                if ts.elapsed() >= Duration::from_secs(5) {
                    self.status_message = None;
                    self.dirty = true;
                }
            }

            if self.needs_full_redraw {
                terminal.clear()?;
                self.needs_full_redraw = false;
                self.dirty = true;
            }

            // Only render when something changed (or effects are animating)
            let attention_active = !self.attention_sessions.is_empty();
            if self.dirty || !self.boot_done || attention_active {
                terminal.draw(|frame| ui::draw(frame, self, elapsed))?;
                self.dirty = false;
            }

            let poll_timeout = if self.boot_done {
                if self.dirty {
                    TICK_RATE
                } else if attention_active {
                    ATTENTION_TICK
                } else {
                    Duration::from_millis(100)
                }
            } else {
                TICK_RATE
                    .saturating_sub(now.elapsed())
                    .max(Duration::from_millis(1))
            };

            if event::poll(poll_timeout)? {
                let ev = event::read()?;
                let skip_redraw = matches!(
                    &ev,
                    Event::Mouse(m) if !matches!(
                        m.kind,
                        MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                            | MouseEventKind::Down(_)
                            | MouseEventKind::Drag(crossterm::event::MouseButton::Left)
                            | MouseEventKind::Up(crossterm::event::MouseButton::Left)
                    )
                );
                self.handle_event(ev);
                if !skip_redraw {
                    self.dirty = true;
                }
            }
        }

        Ok(())
    }

    fn handle_event(&mut self, event: Event) {
        // Mouse events — handle directly, regardless of mode.
        if let Event::Mouse(mouse) = &event {
            let col = mouse.column;
            let row = mouse.row;
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    self.text_selection = None;
                    if let Some(ref mut is) = self.interactor_state {
                        is.handle_mouse_scroll(mouse.kind);
                    }
                }
                MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                    // Check if click is on/near the vertical border (±1 col tolerance)
                    let on_border = col >= self.area_border_x.saturating_sub(1)
                        && col <= self.area_border_x + 1
                        && row >= 3; // below top_bar
                    if on_border {
                        self.dragging_border = true;
                        return;
                    }

                    // Check if click is on/near the horizontal logo border (±1 row tolerance)
                    let on_logo_border = self.area_logo_border_y > 0
                        && row >= self.area_logo_border_y.saturating_sub(1)
                        && row <= self.area_logo_border_y + 1
                        && col < self.area_border_x;
                    if on_logo_border {
                        self.dragging_logo_border = true;
                        return;
                    }

                    let inner = self.area_interactor_inner;
                    if self.input_mode == InputMode::Normal
                        && !self.show_help
                        && inner.width > 0
                        && col >= inner.x
                        && col < inner.x + inner.width
                        && row >= inner.y
                        && row < inner.y + inner.height
                    {
                        self.text_selection = Some(TextSelection {
                            anchor: (col, row),
                            end: (col, row),
                        });
                    } else {
                        self.text_selection = None;
                        self.handle_mouse_click(col, row);
                    }
                }
                MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                    if self.dragging_border {
                        if let Ok((term_cols, _)) = crossterm::terminal::size() {
                            if term_cols > 0 {
                                let pct = ((col as u32) * 100 / term_cols as u32) as u16;
                                self.tree_width_pct =
                                    pct.clamp(TREE_WIDTH_PCT_MIN, TREE_WIDTH_PCT_MAX);
                            }
                        }
                        return;
                    }
                    if self.dragging_logo_border {
                        if let Ok((_, term_rows)) = crossterm::terminal::size() {
                            let left_panel_height = term_rows.saturating_sub(3);
                            let max_logo = (left_panel_height / 2).min(LOGO_HEIGHT_MAX);
                            let new_height = term_rows.saturating_sub(row);
                            self.logo_height = new_height.clamp(LOGO_HEIGHT_MIN, max_logo);
                        }
                        return;
                    }
                    if let Some(ref mut sel) = self.text_selection {
                        let inner = self.area_interactor_inner;
                        sel.end = (
                            col.max(inner.x)
                                .min(inner.x + inner.width.saturating_sub(1)),
                            row.max(inner.y)
                                .min(inner.y + inner.height.saturating_sub(1)),
                        );
                    }
                }
                MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                    if self.dragging_border {
                        self.dragging_border = false;
                        let _ = self
                            .db
                            .set_setting("tree_width_pct", &self.tree_width_pct.to_string());
                        self.sync_interactor_size();
                        return;
                    }
                    if self.dragging_logo_border {
                        self.dragging_logo_border = false;
                        let _ = self
                            .db
                            .set_setting("logo_height", &self.logo_height.to_string());
                        return;
                    }
                    if let Some(ref sel) = self.text_selection {
                        if sel.is_nonempty() {
                            let text = self.extract_selection_text();
                            if !text.is_empty() {
                                self.copy_to_clipboard(&text);
                            }
                        } else {
                            self.text_selection = None;
                        }
                    }
                }
                _ => {} // drop mouse-move / right-click — avoids unnecessary redraws
            }
            return;
        }

        // Any key press clears text selection
        if matches!(&event, Event::Key(k) if k.kind == KeyEventKind::Press) {
            self.text_selection = None;
        }

        // Modal overlays intercept key events directly (not forwarded to tmux)
        if self.input_mode != InputMode::Normal {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press {
                    match self.input_mode {
                        InputMode::TextInput => self.handle_text_input_key(key),
                        InputMode::Confirm => self.handle_confirm_key(key),
                        InputMode::GroupPicker => self.handle_group_picker_key(key),
                        InputMode::Finder => self.handle_finder_key(key),
                        InputMode::Normal => unreachable!(),
                    }
                }
            }
            return;
        }

        // Help overlay — any key dismisses
        if self.show_help {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press {
                    self.show_help = false;
                }
            }
            return;
        }

        // Resize events — update interactor pane geometry
        if matches!(event, Event::Resize(_, _)) {
            self.sync_interactor_size();
            return;
        }

        // Get the current tmux name for forwarding
        let current_tmux_name = self
            .cached_selected
            .as_ref()
            .filter(|s| s.status == SessionStatus::Active)
            .and_then(|s| s.tmux_name.clone());

        // Delegate to interactor's route_event
        if let Some(ref mut is) = self.interactor_state {
            let result = is.route_event(&event, current_tmux_name.as_deref());
            match result {
                RouteResult::Handled => return,
                RouteResult::NexusCommand(cmd) => {
                    self.dispatch_nexus_command(cmd);
                    return;
                }
                RouteResult::Ignored => {}
            }
        }

        // Fallback for when interactor_state is None (tmux unavailable):
        // handle keys directly for tree navigation and CRUD
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                self.handle_fallback_key(key);
            }
        }
    }

    fn dispatch_nexus_command(&mut self, cmd: NexusCommand) {
        match cmd {
            NexusCommand::CursorDown => {
                if let Some(action) = self.tree_state.handle_key(
                    KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE),
                    &self.tree,
                ) {
                    self.handle_tree_action(action);
                }
            }
            NexusCommand::CursorUp => {
                if let Some(action) = self.tree_state.handle_key(
                    KeyEvent::new(KeyCode::Up, crossterm::event::KeyModifiers::NONE),
                    &self.tree,
                ) {
                    self.handle_tree_action(action);
                }
            }
            NexusCommand::ToggleExpand => {
                if let Some(action) = self.tree_state.handle_key(
                    KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
                    &self.tree,
                ) {
                    self.handle_tree_action(action);
                }
            }
            NexusCommand::NewSession => self.start_new_session(),
            NexusCommand::DeleteSelected => self.start_delete(),
            NexusCommand::RenameSelected => self.start_rename(),
            NexusCommand::MoveSession => self.start_move_session(),
            NexusCommand::NewGroup => self.start_new_group(),
            NexusCommand::KillTmux => self.kill_tmux_session(),
            NexusCommand::ToggleHelp => {
                self.show_help = !self.show_help;
            }
            NexusCommand::Quit => {
                if let Some(SelectionTarget::Session(ref id)) = self.selection.selected {
                    let _ = self.db.set_setting("last_session_id", id);
                }
                self.should_quit = true;
            }
            NexusCommand::ToggleDeadSessions => {
                self.show_dead_sessions = !self.show_dead_sessions;
                self.refresh_tree();
            }
            NexusCommand::NextTheme => {
                theme::next_theme();
                self.rebuild_attention_effects();
                self.status_message =
                    Some((format!("Theme: {}", theme::current_name()), Instant::now()));
                self.persist_theme();
            }
            NexusCommand::PrevTheme => {
                theme::prev_theme();
                self.rebuild_attention_effects();
                self.status_message =
                    Some((format!("Theme: {}", theme::current_name()), Instant::now()));
                self.persist_theme();
            }
            NexusCommand::OpenLazygit => self.open_lazygit(),
            NexusCommand::OpenEditor => self.open_editor(),
            NexusCommand::OpenFinder => self.start_finder(),
        }
    }

    /// Fullscreen attach: suspend nexus TUI and attach to the selected tmux session.
    /// Fallback key handler when interactor_state is None (tmux unavailable).
    fn handle_fallback_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if let Some(SelectionTarget::Session(ref id)) = self.selection.selected {
                    let _ = self.db.set_setting("last_session_id", id);
                }
                self.should_quit = true;
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            KeyCode::Char('h') => {
                self.show_dead_sessions = !self.show_dead_sessions;
                self.refresh_tree();
            }
            KeyCode::Char('n') => self.start_new_session(),
            KeyCode::Char('G') => self.start_new_group(),
            KeyCode::Char('r') => self.start_rename(),
            KeyCode::Char('m') => self.start_move_session(),
            KeyCode::Char('d') => self.start_delete(),
            KeyCode::Char('x') => self.kill_tmux_session(),
            KeyCode::Char('t') => self.dispatch_nexus_command(NexusCommand::NextTheme),
            KeyCode::Char('T') => self.dispatch_nexus_command(NexusCommand::PrevTheme),
            KeyCode::Char('p') => self.start_finder(),
            _ => {
                if let Some(action) = self.tree_state.handle_key(key, &self.tree) {
                    self.handle_tree_action(action);
                }
            }
        }
    }

    fn handle_mouse_click(&mut self, col: u16, row: u16) {
        if self.input_mode != InputMode::Normal || self.show_help {
            return;
        }

        // Top bar: theme label
        let r = self.area_theme_label;
        if r.width > 0 && row >= r.y && row < r.y + r.height && col >= r.x && col < r.x + r.width {
            self.dispatch_nexus_command(NexusCommand::NextTheme);
            return;
        }

        // Tree panel
        let inner = self.area_tree;
        if inner.width > 0
            && col >= inner.x
            && col < inner.x + inner.width
            && row >= inner.y
            && row < inner.y + inner.height
        {
            self.handle_tree_click(row, inner);
        }
    }

    fn handle_tree_click(&mut self, row: u16, inner: Rect) {
        let flat = self.tree_state.visible_nodes(&self.tree);
        if flat.is_empty() {
            return;
        }

        let viewport_h = inner.height as usize;
        let start = self.tree_state.scroll_offset;
        let end = (start + viewport_h).min(flat.len());

        // Mirror tree.rs scroll indicator logic
        let content_start = if start > 0 { 1usize } else { 0 };
        let content_end_budget = if end < flat.len() { 1usize } else { 0 };
        let content_slots = viewport_h.saturating_sub(content_start + content_end_budget);

        let rel_row = (row - inner.y) as usize;

        // Ignore clicks on scroll indicator rows
        if start > 0 && rel_row < content_start {
            return;
        }
        if end < flat.len() && rel_row >= content_start + content_slots {
            return;
        }

        // Walk flat nodes counting display lines to find which node was clicked.
        // Sessions with worktrees render as 2 display lines but remain 1 flat node.
        let click_row = rel_row - content_start;
        let mut display_row = 0usize;
        let mut clicked_flat_idx = None;

        for (i, fnode) in flat.iter().enumerate().skip(start) {
            if display_row >= content_slots {
                break;
            }

            let node_height = match &fnode.node {
                FlatNodeKind::Session { summary } if summary.worktree.is_some() => {
                    if display_row + 1 < content_slots {
                        2
                    } else {
                        1
                    }
                }
                _ => 1,
            };

            if click_row >= display_row && click_row < display_row + node_height {
                clicked_flat_idx = Some(i);
                break;
            }
            display_row += node_height;
        }

        let Some(flat_idx) = clicked_flat_idx else {
            return;
        };

        // Move cursor and dispatch via existing handle_tree_action flow
        self.tree_state.cursor_index = flat_idx;

        match &flat[flat_idx].node {
            FlatNodeKind::Group { id, .. } => {
                let gid = *id;
                self.handle_tree_action(TreeAction::Select(SelectionTarget::Group(gid)));
                self.tree_state.toggle_expand(gid);
            }
            FlatNodeKind::Session { summary } => {
                let sid = summary.session_id.clone();
                self.handle_tree_action(TreeAction::Select(SelectionTarget::Session(sid)));
            }
        }
    }

    fn extract_selection_text(&self) -> String {
        let sel = match &self.text_selection {
            Some(s) => s,
            None => return String::new(),
        };
        let inner = self.area_interactor_inner;
        let (start, end) = sel.normalized();
        let mut result = String::new();

        for y in start.1..=end.1 {
            let row_idx = (y - inner.y) as usize;
            if row_idx >= self.interactor_rendered_cells.len() {
                continue;
            }
            let row = &self.interactor_rendered_cells[row_idx];

            let x_start = if y == start.1 {
                (start.0 - inner.x) as usize
            } else {
                0
            };
            let x_end = if y == end.1 {
                (end.0 - inner.x) as usize + 1
            } else {
                row.len()
            };
            let x_start = x_start.min(row.len());
            let x_end = x_end.min(row.len());

            let line: String = row[x_start..x_end].concat();
            result.push_str(line.trim_end());
            if y < end.1 {
                result.push('\n');
            }
        }
        result
    }

    fn copy_to_clipboard(&mut self, text: &str) {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let osc = format!("\x1b]52;c;{encoded}\x07");
        let _ = std::io::Write::write_all(&mut std::io::stdout(), osc.as_bytes());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let chars = text.len();
        self.status_message = Some((format!("Copied {chars} chars"), Instant::now()));
    }

    fn handle_tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Select(target) => {
                if matches!(target, SelectionTarget::Session(_)) {
                    self.selection.selected = Some(target);
                    self.refresh_cached_selected();
                    self.sync_interactor_to_selection();
                    self.ensure_session_launched();
                } else {
                    self.selection.selected = Some(target);
                    self.refresh_cached_selected();
                    // Group node selected — clear interactor
                    if let Some(ref mut is) = self.interactor_state {
                        is.clear();
                    }
                }
            }
            TreeAction::ToggleExpand(_) => {}
            TreeAction::ScrollDown | TreeAction::ScrollUp => {
                if let Some(target) = self.tree_state.selected_target(&self.tree) {
                    self.selection.selected = Some(target);
                    self.refresh_cached_selected();
                    self.sync_interactor_to_selection();
                }
            }
        }
    }

    /// Sync interactor state to the currently selected session.
    ///
    /// Also triggers an immediate tmux pane resize so the embedded session
    /// renders at the correct dimensions before the first capture arrives.
    fn sync_interactor_to_selection(&mut self) {
        if let Some(ref session) = self.cached_selected {
            if let Some(ref mut is) = self.interactor_state {
                is.switch_session(session);
            }
            self.sync_interactor_size();
        } else if let Some(ref mut is) = self.interactor_state {
            is.clear();
        }
    }

    /// Resize the interactor's tmux pane to match current terminal + tree width.
    fn sync_interactor_size(&mut self) {
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            let (ic, ir) = interactor_inner_size(cols, rows, self.tree_width_pct);
            if let Some(ref mut is) = self.interactor_state {
                is.resize_if_needed(ic, ir);
            }
        }
    }

    fn persist_theme(&self) {
        let _ = self
            .db
            .set_setting("theme_index", &theme::current_index().to_string());
    }

    // -----------------------------------------------------------------------
    // Text input handling
    // -----------------------------------------------------------------------

    fn handle_text_input_key(&mut self, key: KeyEvent) {
        let is_cwd = matches!(self.input_context, Some(InputContext::NewSessionCwd { .. }));

        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
                self.input_context = None;
                self.path_suggestions.clear();
                self.path_suggestion_cursor = 0;
            }
            KeyCode::Tab if is_cwd && !self.path_suggestions.is_empty() => {
                // Accept highlighted suggestion
                if let Some(suggestion) = self
                    .path_suggestions
                    .get(self.path_suggestion_cursor)
                    .cloned()
                {
                    self.input_buffer = suggestion;
                    if crate::path_complete::is_directory(&self.input_buffer)
                        && !self.input_buffer.ends_with('/')
                    {
                        self.input_buffer.push('/');
                    }
                }
                self.refresh_path_suggestions();
            }
            // Only arrow keys navigate the suggestion list — j/k would be
            // swallowed from a path the user is literally typing.
            KeyCode::Up if is_cwd && !self.path_suggestions.is_empty() => {
                if self.path_suggestion_cursor == 0 {
                    self.path_suggestion_cursor = self.path_suggestions.len() - 1;
                } else {
                    self.path_suggestion_cursor -= 1;
                }
            }
            KeyCode::Down if is_cwd && !self.path_suggestions.is_empty() => {
                self.path_suggestion_cursor =
                    (self.path_suggestion_cursor + 1) % self.path_suggestions.len();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
                if is_cwd {
                    self.refresh_path_suggestions();
                }
            }
            KeyCode::Enter => {
                let mut buffer = self.input_buffer.clone();
                if buffer.trim().is_empty() {
                    return;
                }
                // Expand ~ before passing to process_text_input
                if is_cwd && (buffer == "~" || buffer.starts_with("~/")) {
                    if let Some(home) = dirs::home_dir() {
                        buffer = format!("{}{}", home.display(), &buffer[1..]);
                    }
                }
                self.path_suggestions.clear();
                self.path_suggestion_cursor = 0;
                self.process_text_input(buffer);
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
                if is_cwd {
                    self.refresh_path_suggestions();
                }
            }
            _ => {}
        }
    }

    fn refresh_path_suggestions(&mut self) {
        self.path_suggestions = crate::path_complete::complete_path(&self.input_buffer);
        if self.path_suggestion_cursor >= self.path_suggestions.len() {
            self.path_suggestion_cursor = 0;
        }
    }

    fn process_text_input(&mut self, buffer: String) {
        let ctx = match self.input_context.take() {
            Some(c) => c,
            None => {
                self.input_mode = InputMode::Normal;
                return;
            }
        };

        match ctx {
            InputContext::NewSessionName => {
                // Move to CWD step, prefilled with current dir
                let default_cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.input_buffer = default_cwd;
                self.input_context = Some(InputContext::NewSessionCwd { name: buffer });
                self.refresh_path_suggestions();
            }
            InputContext::NewSessionCwd { name } => {
                // If CWD is a git repo, offer worktree isolation
                if let Some(repo) = git::detect_repo(&buffer) {
                    self.input_mode = InputMode::Confirm;
                    self.input_context = Some(InputContext::NewSessionWorktree {
                        name,
                        cwd: buffer,
                        repo_root: repo.root,
                    });
                } else {
                    self.transition_to_group_or_create(name, buffer, None);
                }
            }
            InputContext::RenameSession { session_id } => {
                let new_tmux_name = sanitize_tmux_name(&buffer);
                let new_tmux_name = self
                    .db
                    .next_unique_tmux_name(&new_tmux_name, Some(&session_id))
                    .unwrap_or(new_tmux_name);
                // Rename the live tmux session if it exists
                if let Some(old_tmux) = find_session_in_tree(&self.tree, &session_id)
                    .and_then(|s| s.tmux_name.as_deref())
                {
                    if self.tmux_available {
                        let _ = self.tmux.rename_session(old_tmux, &new_tmux_name);
                    }
                }
                if let Err(e) = self
                    .db
                    .update_session_name(&session_id, &buffer, &new_tmux_name)
                {
                    self.status_message = Some((format!("rename failed: {e}"), Instant::now()));
                }
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
                self.refresh_tree();
            }
            InputContext::RenameGroup { group_id } => {
                if let Err(e) = self.db.rename_group(group_id, &buffer) {
                    self.status_message = Some((format!("rename failed: {e}"), Instant::now()));
                }
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
                self.refresh_tree();
            }
            InputContext::NewGroupName => {
                match self.db.create_group(&buffer, "") {
                    Ok(_) => {}
                    Err(e) => {
                        self.status_message =
                            Some((format!("create group failed: {e}"), Instant::now()));
                    }
                }
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
                self.refresh_tree();
            }
            _ => {
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
            }
        }
    }

    /// Transition from CWD step to group picker or direct creation.
    fn transition_to_group_or_create(
        &mut self,
        name: String,
        cwd: String,
        repo_root: Option<PathBuf>,
    ) {
        match self.db.get_all_groups() {
            Ok(groups) if !groups.is_empty() => {
                let mut picker = vec![(0i64, "Ungrouped".to_string())];
                picker.extend(groups);
                self.picker_cursor = self.hovered_group_picker_index(&picker);
                self.picker_groups = picker;
                self.input_mode = InputMode::GroupPicker;
                self.input_context = Some(InputContext::NewSessionGroup {
                    name,
                    cwd,
                    repo_root,
                });
            }
            _ => {
                self.create_session_maybe_worktree(&name, &cwd, None, repo_root);
                self.input_mode = InputMode::Normal;
                self.input_buffer.clear();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Confirm handling
    // -----------------------------------------------------------------------

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        // Worktree confirm: y = with worktree, n = without worktree, Esc = cancel
        if matches!(
            self.input_context,
            Some(InputContext::NewSessionWorktree { .. })
        ) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(InputContext::NewSessionWorktree {
                        name,
                        cwd,
                        repo_root,
                    }) = self.input_context.take()
                    {
                        self.input_mode = InputMode::Normal;
                        self.transition_to_group_or_create(name, cwd, Some(repo_root));
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    if let Some(InputContext::NewSessionWorktree { name, cwd, .. }) =
                        self.input_context.take()
                    {
                        self.input_mode = InputMode::Normal;
                        self.transition_to_group_or_create(name, cwd, None);
                    }
                }
                KeyCode::Esc => {
                    self.input_mode = InputMode::Normal;
                    self.input_context = None;
                }
                _ => {}
            }
            return;
        }
        // Worktree delete: 's' = session only (strip worktree, fall through to confirm)
        if let KeyCode::Char('s') | KeyCode::Char('S') = key.code {
            if matches!(
                self.input_context,
                Some(InputContext::ConfirmDeleteSession {
                    worktree: Some(_),
                    ..
                })
            ) {
                // Strip worktree from context, then fall through to 'y' handler
                if let Some(InputContext::ConfirmDeleteSession {
                    session_id,
                    tmux_name,
                    ..
                }) = self.input_context.take()
                {
                    let ctx = InputContext::ConfirmDeleteSession {
                        session_id,
                        tmux_name,
                        worktree: None,
                    };
                    self.input_mode = InputMode::Normal;
                    self.process_confirm(ctx);
                }
                return;
            }
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let ctx = self.input_context.take();
                self.input_mode = InputMode::Normal;
                if let Some(ctx) = ctx {
                    self.process_confirm(ctx);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input_context = None;
            }
            _ => {}
        }
    }

    fn process_confirm(&mut self, ctx: InputContext) {
        match ctx {
            InputContext::ConfirmDeleteSession {
                session_id,
                tmux_name,
                worktree,
            } => {
                // Kill tmux if active
                if let Some(ref name) = tmux_name {
                    let _ = self.tmux.kill_session(name);
                }
                // If worktree exists, remove on background thread, then delete session
                if let Some(wt) = worktree {
                    let cwd = self
                        .db
                        .get_session_cwd(&session_id)
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let wt_path = std::path::PathBuf::from(&cwd);
                    if wt_path.exists() {
                        self.status_message =
                            Some(("Removing worktree...".to_string(), Instant::now()));
                        self.dirty = true;
                        let repo_root = wt.repo_root.clone();
                        let branch = wt.branch.clone();
                        let teardown_hook = git::resolve_hook_path(
                            &repo_root,
                            "on-worktree-teardown",
                            self.config.worktree.on_teardown.as_deref(),
                        );
                        let (tx, rx) = mpsc::channel();
                        std::thread::Builder::new()
                            .name("nexus-wt-teardown".to_string())
                            .spawn(move || {
                                let result = git::remove_worktree(
                                    &repo_root,
                                    &wt_path,
                                    &branch,
                                    teardown_hook.as_deref(),
                                );
                                let _ = tx.send(result);
                            })
                            .expect("thread spawn");
                        self.pending_wt_teardown = Some(PendingWorktreeTeardown { rx, session_id });
                        return;
                    }
                    // Worktree dir doesn't exist — clear columns and proceed
                    let _ = self.db.clear_worktree_columns(&session_id);
                }
                if let Err(e) = self.db.delete_session(&session_id) {
                    self.status_message = Some((format!("delete failed: {e}"), Instant::now()));
                }
                self.jsonl_snapshots.remove(&session_id);
                self.refresh_tree();
            }
            InputContext::ConfirmDeleteGroup { group_id } => {
                if let Err(e) = self.db.delete_group(group_id) {
                    self.status_message = Some((format!("delete failed: {e}"), Instant::now()));
                }
                self.refresh_tree();
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Group picker handling
    // -----------------------------------------------------------------------

    fn handle_group_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input_context = None;
                self.picker_groups.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.picker_groups.is_empty() {
                    self.picker_cursor = (self.picker_cursor + 1) % self.picker_groups.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.picker_groups.is_empty() {
                    if self.picker_cursor == 0 {
                        self.picker_cursor = self.picker_groups.len() - 1;
                    } else {
                        self.picker_cursor -= 1;
                    }
                }
            }
            KeyCode::Enter => {
                let gid = self.picker_groups.get(self.picker_cursor).map(|(g, _)| *g);
                match self.input_context.take() {
                    Some(InputContext::MoveSession { session_id }) => {
                        if let Some(gid) = gid {
                            if let Err(e) = self.db.move_session_to_group(&session_id, gid) {
                                self.status_message =
                                    Some((format!("move failed: {e}"), Instant::now()));
                            }
                        }
                    }
                    Some(InputContext::NewSessionGroup {
                        name,
                        cwd,
                        repo_root,
                    }) => {
                        // gid 0 is the "Ungrouped" sentinel
                        let group = gid.filter(|&id| id != 0);
                        self.create_session_maybe_worktree(&name, &cwd, group, repo_root);
                    }
                    other => {
                        self.input_context = other;
                    }
                }
                self.input_mode = InputMode::Normal;
                self.input_context = None;
                self.picker_groups.clear();
                self.refresh_tree();
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Session finder
    // -----------------------------------------------------------------------

    fn start_finder(&mut self) {
        self.finder_state.open(&self.tree, self.show_dead_sessions);
        self.input_mode = InputMode::Finder;
    }

    fn handle_finder_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Up => {
                self.finder_state.cursor_up();
            }
            KeyCode::Down => {
                self.finder_state.cursor_down();
            }
            KeyCode::Enter => {
                if let Some(entry) = self.finder_state.selected() {
                    let session_id = entry.session_id.clone();
                    self.input_mode = InputMode::Normal;
                    // Navigate tree to the selected session
                    if self.tree_state.jump_to_session(&session_id, &self.tree) {
                        // Update selection and sync interactor
                        self.selection.selected = Some(SelectionTarget::Session(session_id));
                        self.refresh_cached_selected();
                        self.sync_interactor_to_selection();
                        // Auto-resume detached sessions
                        self.ensure_session_launched();
                    }
                }
            }
            KeyCode::Backspace => {
                self.finder_state.query.pop();
                self.finder_state.refilter();
            }
            KeyCode::Char(c) => {
                self.finder_state.query.push(c);
                self.finder_state.refilter();
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // CRUD action starters
    // -----------------------------------------------------------------------

    fn start_new_session(&mut self) {
        self.input_mode = InputMode::TextInput;
        self.input_context = Some(InputContext::NewSessionName);
        self.input_buffer.clear();
    }

    fn start_new_group(&mut self) {
        self.input_mode = InputMode::TextInput;
        self.input_context = Some(InputContext::NewGroupName);
        self.input_buffer.clear();
    }

    fn start_rename(&mut self) {
        let target = self.tree_state.selected_target(&self.tree);
        match target {
            Some(SelectionTarget::Session(id)) => {
                let current_name = find_session_in_tree(&self.tree, &id)
                    .map(|s| s.display_name.clone())
                    .unwrap_or_default();
                self.input_mode = InputMode::TextInput;
                self.input_context = Some(InputContext::RenameSession { session_id: id });
                self.input_buffer = current_name;
            }
            Some(SelectionTarget::Group(gid)) => {
                let current_name = find_group_in_tree(&self.tree, gid)
                    .map(|g| g.name.clone())
                    .unwrap_or_default();
                self.input_mode = InputMode::TextInput;
                self.input_context = Some(InputContext::RenameGroup { group_id: gid });
                self.input_buffer = current_name;
            }
            None => {}
        }
    }

    /// Return the picker index matching the group the cursor currently sits in
    /// (either a group node itself, or the parent group of a session).  Falls
    /// back to 0 (the "Ungrouped" sentinel when present, or first entry).
    fn hovered_group_picker_index(&mut self, picker: &[(GroupId, String)]) -> usize {
        let target = self.tree_state.selected_target(&self.tree);
        let group_id = match target {
            Some(SelectionTarget::Group(gid)) => Some(gid),
            Some(SelectionTarget::Session(ref sid)) => {
                // Walk the tree to find the parent group of this session
                self.tree.iter().find_map(|node| {
                    if let TreeNode::Group(g) = node {
                        let has_child = g
                            .children
                            .iter()
                            .any(|c| matches!(c, TreeNode::Session(s) if s.session_id == *sid));
                        if has_child {
                            Some(g.id)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            }
            None => None,
        };
        match group_id {
            Some(gid) => picker.iter().position(|(id, _)| *id == gid).unwrap_or(0),
            None => 0,
        }
    }

    fn start_move_session(&mut self) {
        let target = self.tree_state.selected_target(&self.tree);
        if let Some(SelectionTarget::Session(id)) = target {
            match self.db.get_all_groups() {
                Ok(groups) if !groups.is_empty() => {
                    self.picker_groups = groups;
                    self.picker_cursor = 0;
                    self.input_mode = InputMode::GroupPicker;
                    self.input_context = Some(InputContext::MoveSession { session_id: id });
                }
                Ok(_) => {
                    self.status_message = Some((
                        "No groups available. Create one with G".to_string(),
                        Instant::now(),
                    ));
                }
                Err(e) => {
                    self.status_message =
                        Some((format!("failed to load groups: {e}"), Instant::now()));
                }
            }
        }
    }

    fn start_delete(&mut self) {
        let target = self.tree_state.selected_target(&self.tree);
        match target {
            Some(SelectionTarget::Session(id)) => {
                let session = find_session_in_tree(&self.tree, &id);
                let tmux_name = session.and_then(|s| s.tmux_name.clone());
                let worktree = session.and_then(|s| s.worktree.clone());
                self.input_mode = InputMode::Confirm;
                self.input_context = Some(InputContext::ConfirmDeleteSession {
                    session_id: id,
                    tmux_name,
                    worktree,
                });
            }
            Some(SelectionTarget::Group(gid)) => {
                if gid == 0 {
                    return; // can't delete Ungrouped
                }
                self.input_mode = InputMode::Confirm;
                self.input_context = Some(InputContext::ConfirmDeleteGroup { group_id: gid });
            }
            None => {}
        }
    }

    fn kill_tmux_session(&mut self) {
        let target = self.tree_state.selected_target(&self.tree);
        if let Some(SelectionTarget::Session(id)) = target {
            if let Some(session) = find_session_in_tree(&self.tree, &id) {
                if session.status == SessionStatus::Active {
                    if let Some(ref tmux_name) = session.tmux_name {
                        if let Err(e) = self.tmux.kill_session(tmux_name) {
                            self.status_message =
                                Some((format!("kill failed: {e}"), Instant::now()));
                            return;
                        }
                    }
                    let _ = self.db.update_session_status(&id, SessionStatus::Detached);
                    self.refresh_tree();
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Session creation + launch
    // -----------------------------------------------------------------------

    fn create_session(&mut self, name: &str, cwd: &str, group_id: Option<GroupId>) {
        self.finalize_session_creation(name, cwd, group_id, None);
    }

    /// Shared session creation logic for both plain and worktree sessions.
    fn finalize_session_creation(
        &mut self,
        name: &str,
        cwd: &str,
        group_id: Option<GroupId>,
        worktree: Option<&WorktreeInfo>,
    ) {
        let tmux_name = sanitize_tmux_name(name);
        let tmux_name = self
            .db
            .next_unique_tmux_name(&tmux_name, None)
            .unwrap_or(tmux_name);
        let snapshot = snapshot_jsonl_stems(cwd);
        match self
            .db
            .create_nexus_session(name, cwd, &tmux_name, worktree)
        {
            Ok(id) => {
                self.jsonl_snapshots.insert(id.clone(), snapshot);
                if let Some(gid) = group_id {
                    if let Err(e) = self.db.assign_session_to_group(&id, gid) {
                        self.status_message =
                            Some((format!("group assign failed: {e}"), Instant::now()));
                    }
                }
                if self.tmux_available {
                    if let Err(e) = self.tmux.launch_claude_session(&tmux_name, cwd, None) {
                        self.status_message =
                            Some((format!("tmux launch failed: {e}"), Instant::now()));
                        self.refresh_tree();
                        return;
                    }
                    let _ = self.tmux.configure_server();
                }
                // Select the new session in the tree so the interactor picks it up
                self.selection.selected = Some(SelectionTarget::Session(id));
                self.refresh_tree();
                self.sync_interactor_to_selection();
                self.ensure_session_launched();
            }
            Err(e) => {
                self.status_message = Some((format!("create failed: {e}"), Instant::now()));
            }
        }
    }

    /// Create a session, optionally with worktree isolation.
    /// If `repo_root` is provided, spawns a background thread for git worktree creation.
    fn create_session_maybe_worktree(
        &mut self,
        name: &str,
        cwd: &str,
        group_id: Option<GroupId>,
        repo_root: Option<PathBuf>,
    ) {
        let repo_root = match repo_root {
            Some(r) => r,
            None => {
                self.create_session(name, cwd, group_id);
                return;
            }
        };

        // Guard: only one worktree operation at a time
        if self.pending_wt_create.is_some() || self.pending_wt_teardown.is_some() {
            self.status_message = Some((
                "Worktree operation already in progress".to_string(),
                Instant::now(),
            ));
            return;
        }

        let prefix =
            git::resolve_branch_prefix(&repo_root, self.config.worktree.branch_prefix.as_deref());
        let branch = git::sanitize_branch_name(name, &prefix);
        if git::branch_exists(&repo_root, &branch) {
            self.status_message = Some((
                format!("Branch '{}' already exists", branch),
                Instant::now(),
            ));
            return;
        }

        let sanitized_dir = branch.replace('/', "-");
        let worktree_path = repo_root.join(".worktrees").join(&sanitized_dir);

        self.status_message = Some(("Creating worktree...".to_string(), Instant::now()));
        self.dirty = true;

        // Resolve create hook via config priority chain
        let create_hook = git::resolve_hook_path(
            &repo_root,
            "on-worktree-create",
            self.config.worktree.on_create.as_deref(),
        );

        // Spawn named background thread for worktree creation
        let root_clone = repo_root.clone();
        let session_name = name.to_string();
        let wt_path = worktree_path.clone();
        let branch_clone = branch.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("nexus-wt-create".to_string())
            .spawn(move || {
                let result = git::create_worktree(
                    &root_clone,
                    &session_name,
                    &wt_path,
                    &branch_clone,
                    create_hook.as_deref(),
                );
                let _ = tx.send(result);
            })
            .expect("thread spawn");

        self.pending_wt_create = Some(PendingWorktreeCreate {
            rx,
            ctx: PendingWorktreeCtx {
                name: name.to_string(),
                cwd: worktree_path.to_string_lossy().to_string(),
                repo_root,
                branch,
                group_id,
            },
        });
    }

    /// Called from event loop to check if background worktree creation finished.
    fn poll_worktree_pending(&mut self) {
        let result = match self
            .pending_wt_create
            .as_ref()
            .and_then(|p| p.rx.try_recv().ok())
        {
            Some(r) => r,
            None => return,
        };

        let ctx = self.pending_wt_create.take().unwrap().ctx;

        match result {
            Ok(()) => {
                let wt_info = WorktreeInfo {
                    branch: ctx.branch,
                    repo_root: ctx.repo_root,
                };
                self.status_message = Some(("Worktree created".to_string(), Instant::now()));
                self.finalize_session_creation(&ctx.name, &ctx.cwd, ctx.group_id, Some(&wt_info));
            }
            Err(e) => {
                self.status_message = Some((format!("worktree failed: {e}"), Instant::now()));
            }
        }
        self.dirty = true;
    }

    /// Called from event loop to check if background worktree teardown finished.
    fn poll_worktree_teardown(&mut self) {
        let result = match self
            .pending_wt_teardown
            .as_ref()
            .and_then(|p| p.rx.try_recv().ok())
        {
            Some(r) => r,
            None => return,
        };

        let session_id = self.pending_wt_teardown.take().unwrap().session_id;

        match result {
            Ok(()) => {
                if let Err(e) = self.db.delete_session(&session_id) {
                    self.status_message = Some((format!("delete failed: {e}"), Instant::now()));
                } else {
                    self.status_message =
                        Some(("Session and worktree deleted".to_string(), Instant::now()));
                }
                self.jsonl_snapshots.remove(&session_id);
                self.refresh_tree();
            }
            Err(e) => {
                self.status_message =
                    Some((format!("worktree removal failed: {e}"), Instant::now()));
            }
        }
        self.dirty = true;
    }

    /// Launch a detached tmux session in the background for the currently
    /// selected session so the interactor can capture it.
    fn ensure_session_launched(&mut self) {
        if !self.tmux_available {
            return;
        }

        let session = match self.cached_selected.as_ref() {
            Some(s) => s.clone(),
            None => return,
        };

        match session.status {
            SessionStatus::Active => {
                // Already running — capture worker handles display
            }
            SessionStatus::Detached => {
                let cwd = match session
                    .cwd
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                {
                    Some(c) => c,
                    None => return,
                };
                let tmux_name = match session.tmux_name.as_ref() {
                    Some(n) => n.clone(),
                    None => sanitize_tmux_name(&session.session_id),
                };

                // Snapshot before fresh launch so we can detect the new JSONL
                if session.claude_session_id.is_none() {
                    self.jsonl_snapshots
                        .insert(session.session_id.clone(), snapshot_jsonl_stems(&cwd));
                }

                if let Err(e) = self.tmux.launch_claude_session(
                    &tmux_name,
                    &cwd,
                    session.claude_session_id.as_deref(),
                ) {
                    self.status_message =
                        Some((format!("tmux launch failed: {e}"), Instant::now()));
                    return;
                }
                let _ = self.tmux.configure_server();
                let _ = self
                    .db
                    .update_session_status(&session.session_id, SessionStatus::Active);
                self.refresh_tree();
                // Re-sync so interactor switches from conversation log to live capture
                self.refresh_cached_selected();
                self.sync_interactor_to_selection();
            }
            SessionStatus::Dead => {
                self.status_message = Some((
                    "Dead session (not resumable). Only Nexus-created sessions can be resumed."
                        .to_string(),
                    Instant::now(),
                ));
            }
        }
    }

    /// Get the selected session's CWD, or set a status error and return `None`.
    fn selected_cwd(&mut self) -> Option<PathBuf> {
        match self.cached_selected.as_ref().and_then(|s| s.cwd.as_ref()) {
            Some(c) => Some(c.clone()),
            None => {
                self.status_message = Some((
                    "No session selected or no cwd available".into(),
                    Instant::now(),
                ));
                None
            }
        }
    }

    /// Suspend the TUI, open lazygit in the selected session's cwd, then restore.
    fn open_lazygit(&mut self) {
        let Some(cwd) = self.selected_cwd() else {
            return;
        };

        let result = with_suspended_tui(|| {
            std::process::Command::new("lazygit")
                .arg("-p")
                .arg(&cwd)
                .status()
        });
        self.needs_full_redraw = true;

        if let Err(e) = result {
            self.status_message = Some((format!("Failed to launch lazygit: {e}"), Instant::now()));
        }
    }

    /// Suspend the TUI, open the user's editor in the selected session's cwd, then restore.
    ///
    /// Editor resolution: `$EDITOR` → `nvim` → `vim`. Supports `$EDITOR` values
    /// with arguments (e.g. `code --wait`).
    fn open_editor(&mut self) {
        let Some(cwd) = self.selected_cwd() else {
            return;
        };

        let editor_raw = std::env::var("EDITOR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if Self::command_exists("nvim") {
                    "nvim".into()
                } else {
                    "vim".into()
                }
            });

        let mut parts = editor_raw.split_whitespace();
        let program = parts.next().unwrap_or("vim");
        let args: Vec<&str> = parts.collect();

        let result = with_suspended_tui(|| {
            std::process::Command::new(program)
                .args(&args)
                .current_dir(&cwd)
                .status()
        });
        self.needs_full_redraw = true;

        if let Err(e) = result {
            self.status_message =
                Some((format!("Failed to launch {program}: {e}"), Instant::now()));
        }
    }

    /// Check if a command exists on $PATH.
    fn command_exists(name: &str) -> bool {
        std::process::Command::new("which")
            .arg(name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // tmux reconciliation
    // -----------------------------------------------------------------------

    fn reconcile_tmux_state(&mut self) {
        let active_names: HashSet<&str> = self
            .tmux_sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();

        let mut changed = false;
        reconcile_recursive(&mut self.tree, &active_names, &self.db, &mut changed);

        if changed {
            self.tree_state.invalidate_cache();
            // Re-sync interactor — selected session may have changed status
            // (e.g., Active → Detached triggers conversation log transition)
            self.refresh_cached_selected();
            self.sync_interactor_to_selection();
        }

        self.cached_counts = count_sessions(&self.tree);

        // Clean up attention for sessions that no longer have live tmux panes
        let before = self.attention_sessions.len();
        self.attention_sessions.retain(|n| {
            self.tmux_sessions
                .iter()
                .any(|s| s.session_id.as_str() == n.as_str())
        });
        if self.attention_sessions.len() != before {
            self.rebuild_attention_effects();
        }
    }

    /// Scan active sessions that lack a `claude_session_id` and attempt to
    /// detect it from `~/.claude/projects/<project_dir>/`.
    fn detect_claude_session_ids(&mut self) {
        let needs_detection: Vec<(String, String)> = collect_sessions_needing_detection(&self.tree);

        if needs_detection.is_empty() {
            return;
        }

        let mut found_any = false;
        for (session_id, cwd) in &needs_detection {
            let snapshot = self.jsonl_snapshots.get(session_id);
            if let Some(claude_id) = detect_claude_session_id(cwd, snapshot) {
                let _ = self.db.set_claude_session_id(session_id, &claude_id);
                self.jsonl_snapshots.remove(session_id);
                found_any = true;
            }
        }

        if found_any {
            self.refresh_tree();
        }
    }

    // -----------------------------------------------------------------------
    // Tree refresh
    // -----------------------------------------------------------------------

    pub(crate) fn refresh_tree(&mut self) {
        if let Ok(tree) = self.db.get_visible_tree(self.show_dead_sessions) {
            self.tree = tree;
            self.tree_state.invalidate_cache();
            self.cached_counts = count_sessions(&self.tree);
            self.refresh_cached_selected();
            self.dirty = true;
        }
    }

    /// Rebuild attention effects with the current theme's hazard color.
    fn rebuild_attention_effects(&mut self) {
        self.attention_effects.clear();
        for name in &self.attention_sessions {
            self.attention_effects
                .insert(name.clone(), theme::fx_attention_pulse());
        }
    }

    fn refresh_cached_selected(&mut self) {
        self.cached_selected = match self.selection.selected.as_ref() {
            Some(SelectionTarget::Session(id)) => find_session_in_tree(&self.tree, id).cloned(),
            _ => None,
        };
    }

    pub(crate) fn session_counts(&self) -> (usize, usize) {
        self.cached_counts
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Compute the exact inner dimensions of the interactor panel.
///
/// Mirrors the layout in `ui.rs`: top_bar (3 rows) + main area, right column
/// fills the remainder after the tree panel. Subtract 2 for borders.
fn interactor_inner_size(term_cols: u16, term_rows: u16, tree_pct: u16) -> (u16, u16) {
    let main_height = term_rows.saturating_sub(3); // top_bar = Length(3)
    let right_width = term_cols * (100 - tree_pct) / 100;
    let inner_cols = right_width.saturating_sub(2);
    let inner_rows = main_height.saturating_sub(2);
    (inner_cols, inner_rows)
}

/// Collect `(session_id, cwd)` pairs for active sessions that lack a Claude session ID.
fn collect_sessions_needing_detection(tree: &[TreeNode]) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for node in tree {
        match node {
            TreeNode::Session(s) => {
                if s.claude_session_id.is_none() && s.status != SessionStatus::Dead {
                    if let Some(cwd) = &s.cwd {
                        result.push((s.session_id.clone(), cwd.to_string_lossy().to_string()));
                    }
                }
            }
            TreeNode::Group(g) => {
                result.extend(collect_sessions_needing_detection(&g.children));
            }
        }
    }
    result
}

/// Snapshot the set of `.jsonl` file stems in a project's Claude directory.
/// Used before launching a fresh Claude session so we can later identify
/// which JSONL file the new session created.
fn snapshot_jsonl_stems(cwd: &str) -> HashSet<String> {
    let project_dir_name = cwd.replace(['/', '.'], "-");
    let Some(project_dir) =
        dirs::home_dir().map(|h| h.join(".claude/projects").join(&project_dir_name))
    else {
        return HashSet::new();
    };
    std::fs::read_dir(&project_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .collect()
}

/// Detect the Claude Code session ID for a project by scanning
/// `~/.claude/projects/<project_dir>/` for `.jsonl` files.
///
/// When `pre_launch` is provided, returns the first (newest) file NOT in the
/// snapshot — i.e., the file created by the launch we're tracking.
/// When `pre_launch` is `None`, falls back to "most recently modified" for
/// backward compatibility (e.g., sessions restored from DB without a snapshot).
fn detect_claude_session_id(cwd: &str, pre_launch: Option<&HashSet<String>>) -> Option<String> {
    let project_dir_name = cwd.replace(['/', '.'], "-");
    let project_dir = dirs::home_dir()?
        .join(".claude/projects")
        .join(&project_dir_name);

    let mut entries: Vec<_> = std::fs::read_dir(&project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();

    entries.sort_by(|a, b| {
        let ta = a
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let tb = b
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        tb.cmp(&ta) // newest first
    });

    if let Some(snapshot) = pre_launch {
        // Find the first (newest) file NOT in the pre-launch snapshot
        entries
            .iter()
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
            })
            .find(|stem| !snapshot.contains(stem))
    } else {
        // Fallback: most recently modified
        entries.first().and_then(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
    }
}

fn find_session_in_tree<'a>(tree: &'a [TreeNode], session_id: &str) -> Option<&'a SessionSummary> {
    for node in tree {
        match node {
            TreeNode::Session(s) => {
                if s.session_id == session_id {
                    return Some(s);
                }
            }
            TreeNode::Group(g) => {
                if let Some(s) = find_session_in_tree(&g.children, session_id) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn find_group_in_tree(tree: &[TreeNode], group_id: GroupId) -> Option<&GroupNode> {
    for node in tree {
        if let TreeNode::Group(g) = node {
            if g.id == group_id {
                return Some(g);
            }
            if let Some(found) = find_group_in_tree(&g.children, group_id) {
                return Some(found);
            }
        }
    }
    None
}

fn count_sessions(tree: &[TreeNode]) -> (usize, usize) {
    let mut total = 0;
    let mut active = 0;
    for node in tree {
        match node {
            TreeNode::Session(s) => {
                total += 1;
                if s.is_active {
                    active += 1;
                }
            }
            TreeNode::Group(g) => {
                let (t, a) = count_sessions(&g.children);
                total += t;
                active += a;
            }
        }
    }
    (total, active)
}

fn reconcile_recursive(
    tree: &mut [TreeNode],
    active_names: &HashSet<&str>,
    db: &Database,
    changed: &mut bool,
) {
    for node in tree.iter_mut() {
        match node {
            TreeNode::Session(s) => {
                let tmux_name = s.tmux_name.as_deref().unwrap_or("");
                let is_in_tmux = !tmux_name.is_empty() && active_names.contains(tmux_name);

                if is_in_tmux && s.status != SessionStatus::Active {
                    s.status = SessionStatus::Active;
                    s.is_active = true;
                    let _ = db.update_session_status(&s.session_id, SessionStatus::Active);
                    *changed = true;
                } else if !is_in_tmux && s.status == SessionStatus::Active {
                    s.status = SessionStatus::Detached;
                    s.is_active = false;
                    let _ = db.update_session_status(&s.session_id, SessionStatus::Detached);
                    *changed = true;
                } else {
                    s.is_active = is_in_tmux;
                }
            }
            TreeNode::Group(g) => {
                reconcile_recursive(&mut g.children, active_names, db, changed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_tmux_name_ascii() {
        assert_eq!(sanitize_tmux_name("hello-world"), "hello-world");
        assert_eq!(sanitize_tmux_name("a1b2c3"), "a1b2c3");
    }

    #[test]
    fn test_sanitize_tmux_name_special_chars() {
        assert_eq!(sanitize_tmux_name("foo.bar/baz"), "foo-bar-baz");
        assert_eq!(sanitize_tmux_name("a b c"), "a-b-c");
    }

    #[test]
    fn test_sanitize_tmux_name_preserves_full_id() {
        let id = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let sanitized = sanitize_tmux_name(id);
        assert_eq!(sanitized, id);
    }

    #[test]
    fn test_count_sessions_empty() {
        let tree: Vec<TreeNode> = vec![];
        assert_eq!(count_sessions(&tree), (0, 0));
    }

    #[test]
    fn test_count_sessions_nested() {
        use crate::mock;
        let tree = mock::mock_tree();
        let (total, active) = count_sessions(&tree);
        assert_eq!(total, 5);
        assert_eq!(active, 2);
    }

    #[test]
    fn test_find_session_in_tree() {
        use crate::mock;
        let tree = mock::mock_tree();

        let found = find_session_in_tree(&tree, "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
        assert!(found.is_some());
        assert_eq!(found.unwrap().display_name, "feat/scanner");

        let not_found = find_session_in_tree(&tree, "nonexistent");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_find_group_in_tree() {
        use crate::mock;
        let tree = mock::mock_tree();

        let found = find_group_in_tree(&tree, 1);
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "nexus");

        let not_found = find_group_in_tree(&tree, 999);
        assert!(not_found.is_none());
    }

    #[test]
    fn test_reconcile_marks_detached() {
        use crate::mock;
        let mut tree = mock::mock_tree();
        let db = Database::open_in_memory().unwrap();
        let active_names: HashSet<&str> = HashSet::new(); // no tmux sessions
        let mut changed = false;

        reconcile_recursive(&mut tree, &active_names, &db, &mut changed);

        // Sessions that were Active should now be Detached
        let s = find_session_in_tree(&tree, "a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();
        assert_eq!(s.status, SessionStatus::Detached);
        assert!(!s.is_active);
        assert!(changed);
    }
}
