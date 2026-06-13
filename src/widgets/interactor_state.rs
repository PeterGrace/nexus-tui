use std::sync::mpsc;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::text::{Line, Span, Text};

use crate::capture_worker::LiveFrame;
use crate::conversation;
use crate::theme;
use crate::tmux::{alt_key_to_send_args, key_event_to_send_args, TmuxManager};
use crate::types::*;

/// State for the session interactor panel.
///
/// Owns the capture worker communication channels, current display content,
/// and a cloned TmuxManager for send_keys/resize/paste operations.
pub struct InteractorState {
    tmux: TmuxManager,
    content_rx: mpsc::Receiver<Option<LiveFrame>>,
    session_tx: mpsc::Sender<String>,
    nudge_tx: mpsc::Sender<()>,
    pub current_content: Option<SessionContent>,
    last_resize: (u16, u16),
    pub current_session_name: Option<String>,
    /// The actual tmux session name (sanitized), used for resize_pane/send_keys.
    /// Distinct from `current_session_name` which is the human-readable display name.
    current_tmux_name: Option<String>,
    pub log_scroll_offset: u16,
    pub live_scroll_offset: u16,
}

impl InteractorState {
    pub fn new(
        tmux: TmuxManager,
        content_rx: mpsc::Receiver<Option<LiveFrame>>,
        session_tx: mpsc::Sender<String>,
        nudge_tx: mpsc::Sender<()>,
    ) -> Self {
        Self {
            tmux,
            content_rx,
            session_tx,
            nudge_tx,
            current_content: None,
            last_resize: (0, 0),
            current_session_name: None,
            current_tmux_name: None,
            log_scroll_offset: 0,
            live_scroll_offset: 0,
        }
    }

    /// Poll for new content from the capture worker (non-blocking).
    ///
    /// Returns `true` if content was updated (triggers a redraw).
    pub fn poll_content(&mut self) -> bool {
        let mut updated = false;
        // Drain all pending messages, keep the latest
        while let Ok(msg) = self.content_rx.try_recv() {
            match msg {
                Some((text, cursor)) => {
                    self.current_content = Some(SessionContent::Live { text, cursor });
                    updated = true;
                }
                None => {
                    // Session gone — will transition to conversation log
                    // on next reconciliation cycle
                    self.current_content = None;
                    updated = true;
                }
            }
        }
        updated
    }

    /// Switch to a different session.
    ///
    /// Tells the capture worker which session to capture. If the session has no
    /// tmux pane, loads the conversation log instead.
    pub fn switch_session(&mut self, session: &SessionSummary) {
        self.log_scroll_offset = 0;
        self.live_scroll_offset = 0;
        self.current_session_name = Some(session.display_name.clone());
        self.current_tmux_name = session.tmux_name.clone();
        // Reset last_resize so the next resize_if_needed call fires for the new session
        self.last_resize = (0, 0);

        if session.status == SessionStatus::Active {
            // Active session — tell capture worker to start capturing
            if let Some(ref tmux_name) = session.tmux_name {
                let _ = self.session_tx.send(tmux_name.clone());
            }
        } else {
            // Dead/detached — show conversation log
            self.load_conversation_log(session);
            // Tell capture worker to stop capturing (empty name)
            let _ = self.session_tx.send(String::new());
        }
    }

    /// Clear the interactor (e.g., when a group node is selected).
    pub fn clear(&mut self) {
        self.current_content = None;
        self.current_session_name = None;
        self.current_tmux_name = None;
        self.log_scroll_offset = 0;
        self.live_scroll_offset = 0;
        let _ = self.session_tx.send(String::new());
    }

    /// Resize the tmux pane to match the interactor panel dimensions.
    ///
    /// Only sends the resize command if dimensions actually changed.
    pub fn resize_if_needed(&mut self, cols: u16, rows: u16) {
        if (cols, rows) != self.last_resize && cols > 0 && rows > 0 {
            if let Some(ref name) = self.current_tmux_name {
                let _ = self.tmux.resize_pane(name, cols, rows);
            }
            self.last_resize = (cols, rows);
        }
    }

    /// Load conversation log for a dead/detached session.
    ///
    /// Pre-renders the turns into `Text<'static>` so the render loop
    /// doesn't rebuild styled lines every frame.
    fn load_conversation_log(&mut self, session: &SessionSummary) {
        let turns = session
            .jsonl_path
            .as_ref()
            .map(|path| conversation::parse_conversation(path, 100))
            .unwrap_or_default();
        let text = Self::render_turns_to_text(&turns);
        self.current_content = Some(SessionContent::ConversationLog(text));
    }

    /// Convert conversation turns into pre-rendered `Text<'static>`.
    fn render_turns_to_text(turns: &[ConversationTurn]) -> Text<'static> {
        let mut lines: Vec<Line> = Vec::new();

        for turn in turns {
            let (role_label, role_style) = match turn.role {
                Role::Human => ("You", theme::style_for(ThemeElement::ConversationHuman)),
                Role::Assistant => (
                    "Claude",
                    theme::style_for(ThemeElement::ConversationAssistant),
                ),
            };

            lines.push(Line::from(Span::styled(
                format!("--- {role_label} ---"),
                role_style,
            )));

            for content_line in turn.content.lines() {
                lines.push(Line::from(Span::styled(
                    content_line.to_string(),
                    theme::style_for(ThemeElement::Text),
                )));
            }

            lines.push(Line::from(""));
        }

        Text::from(lines)
    }

    /// Scroll conversation log up.
    pub fn scroll_up(&mut self, amount: u16) {
        self.log_scroll_offset = self.log_scroll_offset.saturating_sub(amount);
    }

    /// Scroll conversation log down.
    pub fn scroll_down(&mut self, amount: u16) {
        self.log_scroll_offset = self.log_scroll_offset.saturating_add(amount);
    }

    /// Handle mouse scroll — adjusts the appropriate offset based on content type.
    pub fn handle_mouse_scroll(&mut self, kind: MouseEventKind) {
        let delta: u16 = 3;
        match (&self.current_content, kind) {
            (Some(SessionContent::Live { .. }), MouseEventKind::ScrollUp) => {
                self.live_scroll_offset = self.live_scroll_offset.saturating_add(delta);
            }
            (Some(SessionContent::Live { .. }), MouseEventKind::ScrollDown) => {
                self.live_scroll_offset = self.live_scroll_offset.saturating_sub(delta);
            }
            (Some(SessionContent::ConversationLog(_)), MouseEventKind::ScrollUp) => {
                self.scroll_up(delta);
            }
            (Some(SessionContent::ConversationLog(_)), MouseEventKind::ScrollDown) => {
                self.scroll_down(delta);
            }
            _ => {}
        }
    }

    /// Route an input event: Alt→NexusCommand, forward→tmux, or ignored.
    ///
    /// `current_tmux_name` is the tmux session name to forward keys to.
    /// When `None` (no active tmux pane), non-Alt keys are ignored.
    pub fn route_event(&mut self, event: &Event, current_tmux_name: Option<&str>) -> RouteResult {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // Alt+key → NexusCommand
                if key.modifiers.contains(KeyModifiers::ALT) {
                    // Terminal Ctrl+key collisions: legacy terminals encode
                    // Ctrl+M as 0x0D (Enter) and Ctrl+H as 0x08 (Backspace).
                    // crossterm maps these bytes to named KeyCodes, losing the
                    // Ctrl info. So Ctrl+Alt+M arrives as Alt+Enter and
                    // Ctrl+Alt+H as Alt+Backspace — indistinguishable from
                    // the real keys. We map Enter→MoveSession (matching 'm')
                    // and Backspace→ToggleHelp (matching 'h') to resolve this.
                    // ToggleExpand is rebound to 'e' to avoid the collision.
                    return match key.code {
                        KeyCode::Char('j') => RouteResult::NexusCommand(NexusCommand::CursorDown),
                        KeyCode::Char('k') => RouteResult::NexusCommand(NexusCommand::CursorUp),
                        KeyCode::Char('e') => RouteResult::NexusCommand(NexusCommand::ToggleExpand),
                        KeyCode::Char('n') => RouteResult::NexusCommand(NexusCommand::NewSession),
                        KeyCode::Char('d') => {
                            RouteResult::NexusCommand(NexusCommand::DeleteSelected)
                        }
                        KeyCode::Char('r') => {
                            RouteResult::NexusCommand(NexusCommand::RenameSelected)
                        }
                        KeyCode::Char('m') | KeyCode::Enter => {
                            RouteResult::NexusCommand(NexusCommand::MoveSession)
                        }
                        KeyCode::Char('g') => RouteResult::NexusCommand(NexusCommand::NewGroup),
                        KeyCode::Char('x') => RouteResult::NexusCommand(NexusCommand::KillTmux),
                        KeyCode::Char('h') | KeyCode::Char('?') => {
                            RouteResult::NexusCommand(NexusCommand::ToggleHelp)
                        }
                        KeyCode::Char('q') => RouteResult::NexusCommand(NexusCommand::Quit),
                        KeyCode::Char('H') => {
                            RouteResult::NexusCommand(NexusCommand::ToggleDeadSessions)
                        }
                        KeyCode::Char('t') => RouteResult::NexusCommand(NexusCommand::NextTheme),
                        KeyCode::Char('T') => RouteResult::NexusCommand(NexusCommand::PrevTheme),
                        KeyCode::Char('l') => RouteResult::NexusCommand(NexusCommand::OpenLazygit),
                        KeyCode::Char('v') => RouteResult::NexusCommand(NexusCommand::OpenEditor),
                        KeyCode::Char('p') => RouteResult::NexusCommand(NexusCommand::OpenFinder),
                        other => {
                            // Forward unbound Alt+key to tmux (word-forward, word-backward, etc.)
                            if let Some(tmux_name) = current_tmux_name {
                                if let Some(args) = alt_key_to_send_args(other, key.modifiers) {
                                    let _ = self.tmux.send_keys(tmux_name, &args);
                                    self.live_scroll_offset = 0;
                                    let _ = self.nudge_tx.send(());
                                    return RouteResult::Handled;
                                }
                            }
                            RouteResult::Ignored
                        }
                    };
                }

                // Shift+Arrow/Page → scroll the live view instead of forwarding to tmux
                if current_tmux_name.is_some()
                    && matches!(self.current_content, Some(SessionContent::Live { .. }))
                    && key.modifiers.contains(KeyModifiers::SHIFT)
                {
                    match key.code {
                        KeyCode::Up => {
                            self.live_scroll_offset = self.live_scroll_offset.saturating_add(1);
                            return RouteResult::Handled;
                        }
                        KeyCode::Down => {
                            self.live_scroll_offset = self.live_scroll_offset.saturating_sub(1);
                            return RouteResult::Handled;
                        }
                        KeyCode::PageUp => {
                            self.live_scroll_offset = self.live_scroll_offset.saturating_add(10);
                            return RouteResult::Handled;
                        }
                        KeyCode::PageDown => {
                            self.live_scroll_offset = self.live_scroll_offset.saturating_sub(10);
                            return RouteResult::Handled;
                        }
                        _ => {}
                    }
                }

                // Non-Alt key → forward to tmux if we have an active pane
                if let Some(tmux_name) = current_tmux_name {
                    if let Some(args) = key_event_to_send_args(key) {
                        let _ = self.tmux.send_keys(tmux_name, &args);
                        // Auto-follow: snap to bottom so user sees the response
                        self.live_scroll_offset = 0;
                        // Wake capture worker so display updates immediately
                        let _ = self.nudge_tx.send(());
                        return RouteResult::Handled;
                    }
                }

                // No active tmux pane — handle scroll for conversation log
                if matches!(
                    self.current_content,
                    Some(SessionContent::ConversationLog(_))
                ) {
                    match key.code {
                        KeyCode::Up => {
                            self.scroll_up(1);
                            return RouteResult::Handled;
                        }
                        KeyCode::Down => {
                            self.scroll_down(1);
                            return RouteResult::Handled;
                        }
                        KeyCode::PageUp => {
                            self.scroll_up(10);
                            return RouteResult::Handled;
                        }
                        KeyCode::PageDown => {
                            self.scroll_down(10);
                            return RouteResult::Handled;
                        }
                        _ => {}
                    }
                }

                RouteResult::Ignored
            }
            // Mouse events are handled directly in App::handle_event
            Event::Paste(text) => {
                // Paste → load-buffer + paste-buffer (with 1MB limit)
                if let Some(tmux_name) = current_tmux_name {
                    if text.len() <= 1_048_576 {
                        let _ = self.tmux.load_buffer_and_paste(tmux_name, text);
                        let _ = self.nudge_tx.send(());
                        return RouteResult::Handled;
                    }
                }
                RouteResult::Ignored
            }
            _ => RouteResult::Ignored,
        }
    }
}
