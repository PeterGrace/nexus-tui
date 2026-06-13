use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme;
use crate::types::*;

/// Render the session interactor panel.
///
/// Displays either:
/// - Live terminal content (pre-parsed `Text<'static>` from capture worker)
/// - Conversation log (for sessions without a tmux pane)
/// - Empty state ("Select a session" when no session or group selected)
pub fn render_interactor(
    frame: &mut Frame,
    area: Rect,
    content: Option<&SessionContent>,
    session_name: Option<&str>,
    log_scroll_offset: u16,
    live_scroll_offset: u16,
    show_cursor: bool,
) {
    // Small-area guard: don't render in tiny areas
    if area.width < 10 || area.height < 3 {
        return;
    }

    let title = match session_name {
        Some(name) => format!(" {} ", name),
        None => " SESSION ".to_string(),
    };

    let block = Block::default()
        .title(Span::styled(
            title,
            theme::style_for(ThemeElement::InteractorTitle),
        ))
        .borders(Borders::ALL)
        .border_set(theme::border_for(PanelType::SessionInteractor))
        .border_style(theme::border_style_for(PanelType::SessionInteractor, false))
        .style(theme::style_for(ThemeElement::Surface));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match content {
        Some(SessionContent::Live { text, cursor }) => {
            render_live(frame, inner, text, live_scroll_offset, *cursor, show_cursor);
        }
        Some(SessionContent::ConversationLog(text)) => {
            render_conversation_log(frame, inner, text, log_scroll_offset);
        }
        None => {
            render_empty(frame, inner);
        }
    }
}

/// Render live terminal content from the capture worker.
///
/// Scrolls to the bottom so the input cursor area is always visible.
fn render_live(
    frame: &mut Frame,
    area: Rect,
    text: &Text<'static>,
    user_offset: u16,
    cursor: Option<CursorPos>,
    show_cursor: bool,
) {
    let total_lines = (text.lines.len().min(u16::MAX as usize)) as u16;
    let bottom_pin = total_lines.saturating_sub(area.height);
    let scroll_offset = bottom_pin.saturating_sub(user_offset);
    let paragraph = Paragraph::new(text.clone()).scroll((scroll_offset, 0));
    frame.render_widget(paragraph, area);

    // Place the real terminal cursor on the cell its captured line renders to.
    // `cursor.line` indexes the same text the Paragraph draws, so it stays
    // aligned with the underlying character through any scroll or trailing-blank
    // trimming. Only draw when the target cell is inside the panel.
    if show_cursor {
        if let Some(c) = cursor.filter(|c| c.visible) {
            let row = (c.line as i32) - (scroll_offset as i32);
            let col = c.x as i32;
            if row >= 0 && row < area.height as i32 && col >= 0 && col < area.width as i32 {
                frame.set_cursor_position((area.x + col as u16, area.y + row as u16));
            }
        }
    }
}

/// Render a pre-rendered conversation log for sessions without a tmux pane.
fn render_conversation_log(
    frame: &mut Frame,
    area: Rect,
    text: &Text<'static>,
    scroll_offset: u16,
) {
    if text.lines.is_empty() {
        let msg = Paragraph::new("No conversation data available")
            .style(theme::style_for(ThemeElement::Dim))
            .alignment(Alignment::Center);
        frame.render_widget(msg, area);
        return;
    }

    let paragraph = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));
    frame.render_widget(paragraph, area);
}

/// Render the empty state when no session is selected or a group node is selected.
fn render_empty(frame: &mut Frame, area: Rect) {
    let msg = Paragraph::new("Select a session")
        .style(theme::style_for(ThemeElement::Dim))
        .alignment(Alignment::Center);
    frame.render_widget(msg, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use ratatui::Terminal;

    #[test]
    fn render_interactor_empty_no_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(frame, area, None, None, 0, 0, true);
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_live_no_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let text = Text::raw("Hello from terminal");
        let content = SessionContent::Live { text, cursor: None };
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(
                    frame,
                    area,
                    Some(&content),
                    Some("test-session"),
                    0,
                    0,
                    true,
                );
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_conversation_log_no_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let text = Text::from(vec![
            Line::from("--- You ---"),
            Line::from("Hello"),
            Line::from(""),
            Line::from("--- Claude ---"),
            Line::from("Hi there!"),
            Line::from(""),
        ]);
        let content = SessionContent::ConversationLog(text);
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(frame, area, Some(&content), Some("old-session"), 0, 0, true);
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_empty_conversation_log() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let content = SessionContent::ConversationLog(Text::default());
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(frame, area, Some(&content), None, 0, 0, true);
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_small_area_no_panic() {
        let backend = TestBackend::new(5, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(frame, area, None, None, 0, 0, true);
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_zero_area_no_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 0, 0);
                render_interactor(frame, area, None, None, 0, 0, true);
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_with_scroll_offset() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let lines: Vec<Line> = (0..50)
            .flat_map(|i| {
                let role = if i % 2 == 0 { "You" } else { "Claude" };
                vec![
                    Line::from(format!("--- {role} ---")),
                    Line::from(format!("Turn {i}")),
                    Line::from(""),
                ]
            })
            .collect();
        let content = SessionContent::ConversationLog(Text::from(lines));
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(frame, area, Some(&content), None, 10, 0, true);
            })
            .unwrap();
    }

    #[test]
    fn render_interactor_live_with_scroll_offset() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        // Create content taller than the viewport so offset has effect
        let lines: Vec<&str> = (0..100).map(|_| "line of output").collect();
        let text = Text::raw(lines.join("\n"));
        let content = SessionContent::Live { text, cursor: None };
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_interactor(
                    frame,
                    area,
                    Some(&content),
                    Some("live-session"),
                    0,
                    5,
                    true,
                );
            })
            .unwrap();
    }
}
