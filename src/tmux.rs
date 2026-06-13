use std::io::Write;
use std::process::{Command, Stdio};

use color_eyre::eyre::{bail, WrapErr};
use color_eyre::Result;

use crate::types::{CursorPos, TmuxSessionInfo, TmuxSessionStatus};

// ---------------------------------------------------------------------------
// SendKeysArgs — type-safe tmux send-keys arguments
// ---------------------------------------------------------------------------

/// Type-safe tmux send-keys arguments — prevents command injection by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendKeysArgs {
    /// Literal text — always sent with `-l` flag. Safe for any user input.
    Literal(String),
    /// Named tmux key — compile-time constant from match arms on KeyCode.
    /// Injection-safe because values are &'static str from the key_event_to_send_args match.
    Named(&'static str),
}

// ---------------------------------------------------------------------------
// TmuxManager
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TmuxManager {
    socket_name: String,
}

impl TmuxManager {
    /// Scrollback lines captured by [`capture_pane`](Self::capture_pane) (`-S -<N>`).
    /// Also used to map the pane cursor onto the captured text.
    pub const CAPTURE_SCROLLBACK_LINES: u16 = 500;

    pub fn new(socket_name: &str) -> Self {
        Self {
            socket_name: socket_name.to_string(),
        }
    }

    /// Check whether tmux is installed and reachable.
    pub fn is_available(&self) -> bool {
        Command::new("tmux")
            .arg("-V")
            .stderr(Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Launch a new detached tmux session that runs `claude`.
    ///
    /// If `resume_id` is provided, launches `claude --resume <id>` so Claude
    /// Code picks up the previous conversation.
    pub fn launch_claude_session(
        &self,
        name: &str,
        cwd: &str,
        resume_id: Option<&str>,
    ) -> Result<()> {
        Self::validate_target(name)?;
        let mut cmd = Command::new("tmux");
        cmd.args(["-L", &self.socket_name]).args([
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            cwd,
            "claude",
        ]);

        if let Some(id) = resume_id {
            cmd.args(["--resume", id]);
        }

        let status = cmd
            .stderr(Stdio::null())
            .status()
            .wrap_err("failed to run tmux new-session")?;

        if !status.success() {
            bail!(
                "tmux new-session (claude) exited with status {} for session '{}'",
                status,
                name
            );
        }
        Ok(())
    }

    /// List sessions on the nexus socket.
    ///
    /// Parses the output of:
    /// ```text
    /// tmux -L nexus list-sessions -F '#{session_name}:#{window_name}:#{session_attached}:#{pane_current_command}'
    /// ```
    pub fn list_sessions(&self) -> Result<Vec<TmuxSessionInfo>> {
        let output = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args([
                "list-sessions",
                "-F",
                "#{session_name}:#{window_name}:#{session_attached}:#{pane_current_command}",
            ])
            .stderr(Stdio::null())
            .output()
            .wrap_err("failed to run tmux list-sessions")?;

        if !output.status.success() {
            // tmux exits non-zero when there are no sessions — that's fine.
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_list_sessions_output(&stdout))
    }

    /// Rename an existing tmux session.
    pub fn rename_session(&self, old_name: &str, new_name: &str) -> Result<()> {
        Self::validate_target(old_name)?;
        Self::validate_target(new_name)?;
        let status = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["rename-session", "-t", old_name, new_name])
            .stderr(Stdio::null())
            .status()
            .wrap_err("failed to run tmux rename-session")?;

        if !status.success() {
            bail!(
                "tmux rename-session exited with status {} for '{}'",
                status,
                old_name
            );
        }
        Ok(())
    }

    /// Kill a session by name on the nexus socket.
    pub fn kill_session(&self, name: &str) -> Result<()> {
        Self::validate_target(name)?;
        let status = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["kill-session", "-t", name])
            .stderr(Stdio::null())
            .status()
            .wrap_err("failed to run tmux kill-session")?;

        if !status.success() {
            bail!(
                "tmux kill-session exited with status {} for '{}'",
                status,
                name
            );
        }
        Ok(())
    }

    /// Configure the nexus tmux server: true color support + keybindings.
    ///
    /// Sets `default-terminal`, `terminal-overrides`, and `COLORTERM` so
    /// programs inside tmux (Claude Code) detect and use true color.
    /// Also binds `Ctrl+Q` to detach-client for a consistent return path.
    ///
    /// Safe to call multiple times — idempotent. Call after creating the
    /// first session (which starts the server) and at startup if the server
    /// is already running.
    pub fn configure_server(&self) -> Result<()> {
        // 256-color base terminal type for $TERM inside panes
        let _ = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["set-option", "-g", "default-terminal", "tmux-256color"])
            .stderr(Stdio::null())
            .status();

        // True color passthrough: works with any outer terminal (Ghostty, iTerm2, etc.)
        let _ = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["set-option", "-sa", "terminal-overrides", ",*:Tc"])
            .stderr(Stdio::null())
            .status();

        // Propagate COLORTERM=truecolor to programs inside tmux panes
        let _ = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["set-environment", "-g", "COLORTERM", "truecolor"])
            .stderr(Stdio::null())
            .status();

        // Scrollback buffer size — predictable budget for capture-pane -S -500
        let _ = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["set-option", "-g", "history-limit", "2000"])
            .stderr(Stdio::null())
            .status();

        // Ctrl+Q → detach (consistent way to return to Nexus TUI)
        let status = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["bind-key", "-n", "C-q", "detach-client"])
            .stderr(Stdio::null())
            .status()
            .wrap_err("failed to run tmux bind-key")?;

        if !status.success() {
            bail!("tmux bind-key exited with status {}", status);
        }
        Ok(())
    }

    /// Capture the contents of a tmux pane with ANSI escape sequences.
    ///
    /// Uses `-p -e -N` flags: `-p` outputs to stdout, `-e` includes ANSI
    /// escapes, `-N` preserves alternate screen content.
    pub fn capture_pane(&self, session_name: &str) -> Result<String> {
        Self::validate_target(session_name)?;
        let scrollback = format!("-{}", Self::CAPTURE_SCROLLBACK_LINES);
        let output = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args([
                "capture-pane",
                "-t",
                session_name,
                "-p",
                "-e",
                "-N",
                "-S",
                &scrollback,
            ])
            .stderr(Stdio::null())
            .output()
            .wrap_err("failed to run tmux capture-pane")?;

        if !output.status.success() {
            bail!(
                "tmux capture-pane exited with status {} for '{}'",
                output.status,
                session_name
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Read the pane cursor position and map it onto the captured text.
    ///
    /// `capture-pane` omits the cursor, so we query it separately. Returns a
    /// [`CursorPos`] whose `line` is the cursor's index within the text produced
    /// by [`capture_pane`](Self::capture_pane): scrollback rows captured above
    /// the visible pane (`min(CAPTURE_SCROLLBACK_LINES, history_size)`) plus the
    /// cursor's row within the visible pane (`cursor_y`). Anchoring from the top
    /// keeps it correct even when tmux trims trailing blank rows.
    pub fn pane_cursor(&self, session_name: &str) -> Result<CursorPos> {
        Self::validate_target(session_name)?;
        let output = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args([
                "display-message",
                "-p",
                "-t",
                session_name,
                "#{cursor_x} #{cursor_y} #{history_size} #{cursor_flag}",
            ])
            .stderr(Stdio::null())
            .output()
            .wrap_err("failed to run tmux display-message")?;

        if !output.status.success() {
            bail!(
                "tmux display-message exited with status {} for '{}'",
                output.status,
                session_name
            );
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split_whitespace();
        let x: u16 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let cursor_y: u16 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let history: u16 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        // cursor_flag is 1 when the cursor is visible; assume visible if absent.
        let visible = fields.next().map(|v| v == "1").unwrap_or(true);

        let scrollback = Self::CAPTURE_SCROLLBACK_LINES.min(history);
        Ok(CursorPos {
            x,
            line: scrollback.saturating_add(cursor_y),
            visible,
        })
    }

    /// Lightweight text-only capture of the last N lines of a pane.
    ///
    /// No ANSI escapes, no alternate screen — just plain text for pattern
    /// matching. Used by the feedback scanner to detect halt states.
    pub fn capture_pane_tail(&self, session_name: &str, lines: u32) -> Result<String> {
        Self::validate_target(session_name)?;
        let lines_arg = format!("-{lines}");
        let output = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["capture-pane", "-t", session_name, "-p", "-S", &lines_arg])
            .stderr(Stdio::null())
            .output()
            .wrap_err("failed to run tmux capture-pane (tail)")?;

        if !output.status.success() {
            bail!(
                "tmux capture-pane (tail) exited with status {} for '{}'",
                output.status,
                session_name
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Send keys to a tmux session synchronously via `Command::status()`.
    ///
    /// Uses blocking wait to ensure keystroke ordering — rapid typing won't
    /// cause out-of-order delivery. The ~2-5ms overhead per key is acceptable
    /// since we process one event per frame.
    ///
    /// `Literal` args use `-l` flag (safe for any user text).
    /// `Named` args use the key name directly (compile-time constants).
    pub fn send_keys(&self, session_name: &str, args: &SendKeysArgs) -> Result<()> {
        Self::validate_target(session_name)?;
        let mut cmd = Command::new("tmux");
        cmd.args(["-L", &self.socket_name])
            .args(["send-keys", "-t", session_name]);

        match args {
            SendKeysArgs::Literal(text) => {
                cmd.args(["-l", text]);
            }
            SendKeysArgs::Named(key_name) => {
                cmd.arg(key_name);
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .wrap_err("failed to run tmux send-keys")?;

        Ok(())
    }

    /// Resize a tmux session's window and pane to the given dimensions.
    ///
    /// Uses `resize-window` first (required for detached sessions where the
    /// window size constrains the pane), then `resize-pane` as a fallback.
    pub fn resize_pane(&self, session_name: &str, cols: u16, rows: u16) -> Result<()> {
        Self::validate_target(session_name)?;
        let cols_str = cols.to_string();
        let rows_str = rows.to_string();

        // resize-window works for detached sessions (resize-pane alone won't
        // exceed the window dimensions, which default to 80x24 for detached)
        let _ = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args([
                "resize-window",
                "-t",
                session_name,
                "-x",
                &cols_str,
                "-y",
                &rows_str,
            ])
            .stderr(Stdio::null())
            .status();

        let status = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args([
                "resize-pane",
                "-t",
                session_name,
                "-x",
                &cols_str,
                "-y",
                &rows_str,
            ])
            .stderr(Stdio::null())
            .status()
            .wrap_err("failed to run tmux resize-pane")?;

        if !status.success() {
            bail!(
                "tmux resize-pane exited with status {} for '{}'",
                status,
                session_name
            );
        }
        Ok(())
    }

    /// Load text into a named tmux buffer and paste it into a session.
    ///
    /// Uses `tmux load-buffer -b nexus-paste -` (stdin) followed by
    /// `tmux paste-buffer -b nexus-paste -t <session> -d` (auto-cleanup).
    pub fn load_buffer_and_paste(&self, session_name: &str, text: &str) -> Result<()> {
        Self::validate_target(session_name)?;

        // Load into named buffer via stdin
        let mut child = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args(["load-buffer", "-b", "nexus-paste", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .wrap_err("failed to spawn tmux load-buffer")?;

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("tmux load-buffer exited with status {}", status);
        }

        // Paste from named buffer into session (-d deletes buffer after paste)
        let status = Command::new("tmux")
            .args(["-L", &self.socket_name])
            .args([
                "paste-buffer",
                "-b",
                "nexus-paste",
                "-t",
                session_name,
                "-d",
            ])
            .status()
            .wrap_err("failed to run tmux paste-buffer")?;

        if !status.success() {
            bail!("tmux paste-buffer exited with status {}", status);
        }
        Ok(())
    }

    /// Validate that a session name is safe to use as a tmux target.
    ///
    /// Rejects names containing `.` (tmux target separator for session:window.pane)
    /// or any characters outside `[a-zA-Z0-9_-]`.
    fn validate_target(name: &str) -> Result<()> {
        if name.is_empty() {
            bail!("session name cannot be empty");
        }
        if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            Ok(())
        } else {
            bail!(
                "invalid session name '{}': only [a-zA-Z0-9_-] allowed (no '.' — tmux separator)",
                name
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Sanitize a string for use as a tmux session name.
///
/// Replaces any character outside `[a-zA-Z0-9-]` with `-`.
pub fn sanitize_tmux_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Key mapping: crossterm KeyEvent → tmux SendKeysArgs
// ---------------------------------------------------------------------------

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Map a crossterm `KeyEvent` to tmux `SendKeysArgs`.
///
/// Returns `None` for key events that should not be forwarded (e.g., Alt+key
/// combos that are reserved for nexus commands).
pub fn key_event_to_send_args(event: &KeyEvent) -> Option<SendKeysArgs> {
    // Alt+key is reserved for nexus commands — never forward
    if event.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    // Ctrl+key combos
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = event.code {
            if c.is_ascii_alphabetic() {
                let lower = c.to_ascii_lowercase();
                return Some(SendKeysArgs::Named(match lower {
                    'a' => "C-a",
                    'b' => "C-b",
                    'c' => "C-c",
                    'd' => "C-d",
                    'e' => "C-e",
                    'f' => "C-f",
                    'g' => "C-g",
                    'h' => "C-h",
                    'i' => "C-i",
                    'j' => "C-j",
                    'k' => "C-k",
                    'l' => "C-l",
                    'm' => "C-m",
                    'n' => "C-n",
                    'o' => "C-o",
                    'p' => "C-p",
                    'q' => "C-q",
                    'r' => "C-r",
                    's' => "C-s",
                    't' => "C-t",
                    'u' => "C-u",
                    'v' => "C-v",
                    'w' => "C-w",
                    'x' => "C-x",
                    'y' => "C-y",
                    'z' => "C-z",
                    _ => return None,
                }));
            }
            return None;
        }
    }

    match event.code {
        // Printable characters → Literal (safe for any text)
        KeyCode::Char(c) => Some(SendKeysArgs::Literal(c.to_string())),

        // Special keys → Named tmux key names
        KeyCode::Enter => Some(SendKeysArgs::Named("Enter")),
        KeyCode::Backspace => Some(SendKeysArgs::Named("BSpace")),
        KeyCode::Tab => Some(SendKeysArgs::Named("Tab")),
        KeyCode::Esc => Some(SendKeysArgs::Named("Escape")),
        KeyCode::Up => Some(SendKeysArgs::Named("Up")),
        KeyCode::Down => Some(SendKeysArgs::Named("Down")),
        KeyCode::Left => Some(SendKeysArgs::Named("Left")),
        KeyCode::Right => Some(SendKeysArgs::Named("Right")),
        KeyCode::Home => Some(SendKeysArgs::Named("Home")),
        KeyCode::End => Some(SendKeysArgs::Named("End")),
        KeyCode::PageUp => Some(SendKeysArgs::Named("PageUp")),
        KeyCode::PageDown => Some(SendKeysArgs::Named("PageDown")),
        KeyCode::Delete => Some(SendKeysArgs::Named("DC")),
        KeyCode::Insert => Some(SendKeysArgs::Named("IC")),
        KeyCode::BackTab => Some(SendKeysArgs::Named("BTab")),

        // Function keys F1-F12
        KeyCode::F(n) => match n {
            1 => Some(SendKeysArgs::Named("F1")),
            2 => Some(SendKeysArgs::Named("F2")),
            3 => Some(SendKeysArgs::Named("F3")),
            4 => Some(SendKeysArgs::Named("F4")),
            5 => Some(SendKeysArgs::Named("F5")),
            6 => Some(SendKeysArgs::Named("F6")),
            7 => Some(SendKeysArgs::Named("F7")),
            8 => Some(SendKeysArgs::Named("F8")),
            9 => Some(SendKeysArgs::Named("F9")),
            10 => Some(SendKeysArgs::Named("F10")),
            11 => Some(SendKeysArgs::Named("F11")),
            12 => Some(SendKeysArgs::Named("F12")),
            _ => None,
        },

        // Unhandled key codes (Null, CapsLock, etc.) — ignore
        _ => None,
    }
}

/// Map an Alt+key combo to tmux send-keys args.
///
/// Called for Alt+key events that are NOT bound to a Nexus command, so they
/// can be forwarded to the tmux pane as standard terminal navigation
/// (e.g. Alt+f → word-forward, Alt+b → word-backward, Alt+arrows).
pub fn alt_key_to_send_args(code: KeyCode, _modifiers: KeyModifiers) -> Option<SendKeysArgs> {
    match code {
        KeyCode::Char(c) => Some(SendKeysArgs::Named(match c {
            'f' => "M-f",
            'b' => "M-b",
            _ => return None,
        })),
        KeyCode::Left => Some(SendKeysArgs::Named("M-Left")),
        KeyCode::Right => Some(SendKeysArgs::Named("M-Right")),
        KeyCode::Up => Some(SendKeysArgs::Named("M-Up")),
        KeyCode::Down => Some(SendKeysArgs::Named("M-Down")),
        KeyCode::Backspace => Some(SendKeysArgs::Named("M-BSpace")),
        KeyCode::Delete => Some(SendKeysArgs::Named("M-DC")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parse the raw output from `tmux list-sessions -F '...'` into structured
/// window info.
///
/// Expected format per line:
/// ```text
/// session_name:window_name:session_attached:pane_current_command
/// ```
///
/// - `session_attached > 0` -> `is_active = true`
/// - `pane_current_command` non-empty -> `Running`, otherwise `Idle`
fn parse_list_sessions_output(output: &str) -> Vec<TmuxSessionInfo> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(parse_session_line)
        .collect()
}

fn parse_session_line(line: &str) -> Option<TmuxSessionInfo> {
    // Split on ':', expecting at least 4 fields.
    // Session names and commands could theoretically contain colons,
    // so we split into at most 4 parts.
    let mut parts = line.splitn(4, ':');

    let session_name = parts.next()?.to_string();
    let window_name = parts.next()?.to_string();
    let attached_str = parts.next()?;
    let command = parts.next().unwrap_or("");

    let attached: u32 = attached_str.trim().parse().unwrap_or(0);
    let is_active = attached > 0;

    let status = if command.trim().is_empty() {
        TmuxSessionStatus::Idle
    } else {
        TmuxSessionStatus::Running
    };

    Some(TmuxSessionInfo {
        session_id: session_name,
        window_name,
        is_active,
        status,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_line() {
        let output = "my-session:bash:1:vim\n";
        let windows = parse_list_sessions_output(output);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].session_id, "my-session");
        assert_eq!(windows[0].window_name, "bash");
        assert!(windows[0].is_active);
        assert_eq!(windows[0].status, TmuxSessionStatus::Running);
    }

    #[test]
    fn test_parse_detached_idle() {
        let output = "nexus-001:zsh:0:\n";
        let windows = parse_list_sessions_output(output);
        assert_eq!(windows.len(), 1);
        assert!(!windows[0].is_active);
        assert_eq!(windows[0].status, TmuxSessionStatus::Idle);
    }

    #[test]
    fn test_parse_multiple_lines() {
        let output = "\
session-a:win1:1:claude\n\
session-b:win2:0:vim\n\
session-c:win3:0:\n";

        let windows = parse_list_sessions_output(output);
        assert_eq!(windows.len(), 3);

        assert_eq!(windows[0].session_id, "session-a");
        assert!(windows[0].is_active);
        assert_eq!(windows[0].status, TmuxSessionStatus::Running);

        assert_eq!(windows[1].session_id, "session-b");
        assert!(!windows[1].is_active);
        assert_eq!(windows[1].status, TmuxSessionStatus::Running);

        assert_eq!(windows[2].session_id, "session-c");
        assert!(!windows[2].is_active);
        assert_eq!(windows[2].status, TmuxSessionStatus::Idle);
    }

    #[test]
    fn test_parse_empty_output() {
        let windows = parse_list_sessions_output("");
        assert!(windows.is_empty());
    }

    #[test]
    fn test_parse_malformed_line_skipped() {
        let output = "only-one-part\nsession:win:0:cmd\n";
        let windows = parse_list_sessions_output(output);
        // First line has no colons -> skipped, second line is valid
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].session_id, "session");
    }

    #[test]
    fn test_parse_non_numeric_attached() {
        let output = "sess:win:abc:cmd\n";
        let windows = parse_list_sessions_output(output);
        assert_eq!(windows.len(), 1);
        // Non-numeric falls back to 0 -> not active
        assert!(!windows[0].is_active);
    }

    #[test]
    fn test_parse_command_with_colons() {
        // If the command itself contains colons, splitn(4, ':') keeps them
        let output = "sess:win:1:python:3.11:script.py\n";
        let windows = parse_list_sessions_output(output);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].session_id, "sess");
        assert_eq!(windows[0].window_name, "win");
        assert!(windows[0].is_active);
        assert_eq!(windows[0].status, TmuxSessionStatus::Running);
    }

    #[test]
    fn test_validate_target_valid() {
        assert!(TmuxManager::validate_target("my-session").is_ok());
        assert!(TmuxManager::validate_target("test_123").is_ok());
        assert!(TmuxManager::validate_target("ABC-xyz").is_ok());
    }

    #[test]
    fn test_validate_target_rejects_dot() {
        assert!(TmuxManager::validate_target("session.name").is_err());
    }

    #[test]
    fn test_validate_target_rejects_spaces() {
        assert!(TmuxManager::validate_target("my session").is_err());
    }

    #[test]
    fn test_validate_target_rejects_empty() {
        assert!(TmuxManager::validate_target("").is_err());
    }

    #[test]
    fn test_validate_target_rejects_injection() {
        assert!(TmuxManager::validate_target("sess;rm -rf /").is_err());
        assert!(TmuxManager::validate_target("sess:window").is_err());
    }

    #[test]
    fn test_sanitize_tmux_name() {
        assert_eq!(sanitize_tmux_name("hello-world"), "hello-world");
        assert_eq!(sanitize_tmux_name("foo.bar/baz"), "foo-bar-baz");
        assert_eq!(sanitize_tmux_name("a b c"), "a-b-c");
    }

    // -- Integration tests (require tmux installed) ----------------------

    #[test]
    #[ignore]
    fn test_is_available_with_tmux() {
        let mgr = TmuxManager::new("nexus-test");
        assert!(mgr.is_available());
    }

    #[test]
    #[ignore]
    fn test_launch_and_kill_session() {
        let mgr = TmuxManager::new("nexus-test-lk");

        // Launch claude session
        mgr.launch_claude_session("test-sess", "/tmp", None)
            .unwrap();

        // Verify it appears in list
        let sessions = mgr.list_sessions().unwrap();
        assert!(
            sessions.iter().any(|s| s.session_id == "test-sess"),
            "session should appear in list"
        );

        // Kill
        mgr.kill_session("test-sess").unwrap();

        // Verify it's gone
        let sessions = mgr.list_sessions().unwrap();
        assert!(
            !sessions.iter().any(|s| s.session_id == "test-sess"),
            "session should be removed"
        );
    }

    #[test]
    #[ignore]
    fn test_configure_server() {
        let mgr = TmuxManager::new("nexus-test-kb");

        // Need at least one session for server to exist
        mgr.launch_claude_session("kb-test", "/tmp", None).unwrap();
        mgr.configure_server().unwrap();
        mgr.kill_session("kb-test").unwrap();
    }

    #[test]
    #[ignore]
    fn test_capture_pane_returns_content() {
        let mgr = TmuxManager::new("nexus-test-cap");
        mgr.launch_claude_session("cap-test", "/tmp", None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let content = mgr.capture_pane("cap-test").unwrap();
        assert!(!content.is_empty());

        mgr.kill_session("cap-test").unwrap();
    }

    #[test]
    #[ignore]
    fn test_send_keys_reaches_session() {
        let mgr = TmuxManager::new("nexus-test-sk");
        mgr.launch_claude_session("sk-test", "/tmp", None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        mgr.send_keys("sk-test", &SendKeysArgs::Literal("hello".to_string()))
            .unwrap();
        mgr.send_keys("sk-test", &SendKeysArgs::Named("Enter"))
            .unwrap();

        mgr.kill_session("sk-test").unwrap();
    }

    #[test]
    #[ignore]
    fn test_resize_pane() {
        let mgr = TmuxManager::new("nexus-test-rp");
        mgr.launch_claude_session("rp-test", "/tmp", None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        mgr.resize_pane("rp-test", 80, 24).unwrap();

        mgr.kill_session("rp-test").unwrap();
    }

    #[test]
    fn test_send_keys_args_variants() {
        let lit = SendKeysArgs::Literal("hello".to_string());
        let named = SendKeysArgs::Named("Enter");
        assert_ne!(lit, named);
        assert_eq!(lit, SendKeysArgs::Literal("hello".to_string()));
        assert_eq!(named, SendKeysArgs::Named("Enter"));
    }

    // -- Key mapping tests ------------------------------------------------

    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn test_key_mapping_printable_chars() {
        let result = key_event_to_send_args(&make_key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(result, Some(SendKeysArgs::Literal("a".to_string())));

        let result = key_event_to_send_args(&make_key(KeyCode::Char('Z'), KeyModifiers::SHIFT));
        assert_eq!(result, Some(SendKeysArgs::Literal("Z".to_string())));
    }

    #[test]
    fn test_key_mapping_unicode() {
        let result = key_event_to_send_args(&make_key(KeyCode::Char('ñ'), KeyModifiers::NONE));
        assert_eq!(result, Some(SendKeysArgs::Literal("ñ".to_string())));

        let result = key_event_to_send_args(&make_key(KeyCode::Char('日'), KeyModifiers::NONE));
        assert_eq!(result, Some(SendKeysArgs::Literal("日".to_string())));
    }

    #[test]
    fn test_key_mapping_special_keys() {
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Enter"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("BSpace"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Tab, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Tab"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Escape"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Delete, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("DC"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Insert, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("IC"))
        );
    }

    #[test]
    fn test_key_mapping_arrows() {
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Up, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Up"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Down, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Down"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Left, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Left"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Right, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Right"))
        );
    }

    #[test]
    fn test_key_mapping_nav_keys() {
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Home, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("Home"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::End, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("End"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("PageUp"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::PageDown, KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("PageDown"))
        );
    }

    #[test]
    fn test_key_mapping_ctrl_keys() {
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(SendKeysArgs::Named("C-c"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(SendKeysArgs::Named("C-a"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(SendKeysArgs::Named("C-z"))
        );
        // Uppercase Ctrl+key should map to same lowercase
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Char('C'), KeyModifiers::CONTROL)),
            Some(SendKeysArgs::Named("C-c"))
        );
    }

    #[test]
    fn test_key_mapping_function_keys() {
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::F(1), KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("F1"))
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::F(12), KeyModifiers::NONE)),
            Some(SendKeysArgs::Named("F12"))
        );
        // F13+ should be ignored
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::F(13), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn test_key_mapping_alt_returns_none() {
        // Alt+key is reserved for nexus commands
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
        assert_eq!(
            key_event_to_send_args(&make_key(KeyCode::Char('q'), KeyModifiers::ALT)),
            None
        );
    }
}
