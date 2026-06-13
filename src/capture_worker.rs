use std::sync::mpsc;
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::style::Color;
use ratatui::text::Text;

use crate::ansi::sanitize_ansi;
use crate::tmux::TmuxManager;
use crate::types::CursorPos;

/// A captured frame: the parsed terminal text plus the cursor position, if known.
pub type LiveFrame = (Text<'static>, Option<CursorPos>);

/// Spawn the capture worker thread.
///
/// Returns channels for communicating with the worker:
/// - `session_tx`: send session names to capture (empty string = stop)
/// - `content_rx`: receive a parsed [`LiveFrame`] (None = session gone)
/// - `nudge_tx`: send after forwarding a key to wake the worker immediately
pub fn spawn(
    tmux: TmuxManager,
) -> (
    mpsc::Sender<String>,
    mpsc::Receiver<Option<LiveFrame>>,
    mpsc::Sender<()>,
) {
    let (session_tx, session_rx) = mpsc::channel::<String>();
    let (content_tx, content_rx) = mpsc::channel::<Option<LiveFrame>>();
    let (nudge_tx, nudge_rx) = mpsc::channel::<()>();

    std::thread::Builder::new()
        .name("nexus-capture".to_string())
        .spawn(move || {
            capture_loop(tmux, session_rx, content_tx, nudge_rx);
        })
        .expect("failed to spawn capture worker thread");

    (session_tx, content_rx, nudge_tx)
}

fn capture_loop(
    tmux: TmuxManager,
    session_rx: mpsc::Receiver<String>,
    content_tx: mpsc::Sender<Option<LiveFrame>>,
    nudge_rx: mpsc::Receiver<()>,
) {
    let mut current_session = String::new();
    let mut last_raw: Vec<u8> = Vec::new();
    let mut last_text: Text<'static> = Text::default();
    let mut last_cursor: Option<CursorPos> = None;
    let mut poll_interval = Duration::from_millis(30);

    loop {
        // Check for session change (non-blocking)
        while let Ok(new_session) = session_rx.try_recv() {
            current_session = new_session;
            last_raw.clear();
            poll_interval = Duration::from_millis(30);
        }

        if current_session.is_empty() {
            // Block until a session is selected — no CPU burn
            match session_rx.recv() {
                Ok(new_session) => {
                    current_session = new_session;
                    last_raw.clear();
                    poll_interval = Duration::from_millis(30);
                }
                Err(_) => return, // Channel closed — main thread exiting
            }
            continue;
        }

        // Capture
        match tmux.capture_pane(&current_session) {
            Ok(raw) => {
                let raw_bytes = raw.as_bytes();
                let raw_changed = raw_bytes != last_raw.as_slice();
                if raw_changed {
                    last_raw = raw_bytes.to_vec();
                    // Parse ANSI on the worker thread — main thread receives ready-to-render Text
                    let sanitized = sanitize_ansi(raw_bytes);
                    let mut parsed: Text<'static> = sanitized.into_text().unwrap_or_default();
                    normalize_resets(&mut parsed);
                    last_text = parsed;
                }

                // The cursor isn't part of the capture; query it separately so the
                // preview can show where the user is editing. A bare cursor move
                // (arrow keys) leaves the captured text unchanged, so resend the
                // cached text when only the cursor moved.
                let cursor = tmux.pane_cursor(&current_session).ok();
                let cursor_changed = cursor != last_cursor;

                if raw_changed || cursor_changed {
                    last_cursor = cursor;
                    if content_tx.send(Some((last_text.clone(), cursor))).is_err() {
                        return; // Main thread dropped receiver
                    }
                    poll_interval = Duration::from_millis(30); // active: fast polling
                } else {
                    // Nothing changed — back off
                    poll_interval = (poll_interval * 2).min(Duration::from_millis(500));
                }
            }
            Err(_) => {
                // Session gone — notify main thread
                if content_tx.send(None).is_err() {
                    return;
                }
                // Stop capturing this session — wait for a new one
                current_session.clear();
                last_cursor = None;
                continue;
            }
        }

        // Sleep until poll interval expires or a nudge arrives (key forwarded).
        // Nudge resets backoff so the next capture happens immediately.
        match nudge_rx.recv_timeout(poll_interval) {
            Ok(()) => {
                // Drain any extra nudges that piled up
                while nudge_rx.try_recv().is_ok() {}
                poll_interval = Duration::from_millis(30);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Replace `Color::Reset` with `None` in all spans.
///
/// `ansi-to-tui` maps SGR resets (\e[0m, \e[39m, \e[49m) to `Color::Reset`,
/// which tells ratatui to send an explicit SGR reset to the terminal. This
/// overrides the nexus panel's themed background/foreground with the terminal's
/// default colors, causing a visible color mismatch.
///
/// Setting these to `None` instead means "inherit from parent widget", so the
/// panel's theme colors show through for any text without explicit colors.
fn normalize_resets(text: &mut Text<'static>) {
    for line in &mut text.lines {
        for span in &mut line.spans {
            if span.style.fg == Some(Color::Reset) {
                span.style.fg = None;
            }
            if span.style.bg == Some(Color::Reset) {
                span.style.bg = None;
            }
        }
    }
}
