use std::time::Duration;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::theme;
use crate::types::*;
use crate::widgets;
use tachyonfx::EffectRenderer;

pub fn draw(frame: &mut Frame, app: &mut App, elapsed: Duration) {
    let area = frame.area();

    // Fill background
    frame.render_widget(Block::default().style(Style::new().bg(theme::bg())), area);

    // Terminal too small guard
    if area.width < 80 || area.height < 24 {
        let msg = Paragraph::new("Terminal too small. Minimum: 80x24")
            .style(Style::new().fg(theme::hazard()).bg(theme::bg()));
        frame.render_widget(msg, area);
        app.area_tree = Rect::default();
        app.area_theme_label = Rect::default();
        app.area_logo_border_y = 0;
        return;
    }

    // Top-level: top bar + main area (no bottom strip — activity removed)
    let [top_bar, main_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);

    // Main area: tree (dynamic %) + right column (fills remainder)
    let [left_panel, right_column] = Layout::horizontal([
        Constraint::Percentage(app.tree_width_pct),
        Constraint::Fill(1),
    ])
    .areas(main_area);

    // Store border x-position for drag hit-testing
    app.area_border_x = left_panel.x + left_panel.width;

    // Right column: interactor fills everything
    let interactor_area = right_column;

    // Split left panel: tree + optional logo (dynamic height)
    let logo_h = app.logo_height.min(left_panel.height / 2);
    let (tree_area, logo_area, indicator_area) = if logo_h <= 2 {
        // GoL hidden: 1-row indicator bar at bottom
        if left_panel.height > 1 {
            let [tree, indicator] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(left_panel);
            app.area_logo_border_y = indicator.y;
            (tree, None, Some(indicator))
        } else {
            app.area_logo_border_y = 0;
            (left_panel, None, None)
        }
    } else if left_panel.height >= logo_h + 5 {
        // Enough room for tree + logo
        let [tree, logo] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(logo_h)]).areas(left_panel);
        app.area_logo_border_y = logo.y;
        (tree, Some(logo), None)
    } else if left_panel.height > 1 {
        // Not enough room for full logo — show indicator bar as fallback drag handle
        let [tree, indicator] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(left_panel);
        app.area_logo_border_y = indicator.y;
        (tree, None, Some(indicator))
    } else {
        app.area_logo_border_y = 0;
        (left_panel, None, None)
    };

    // Store tree inner rect for mouse hit-testing
    app.area_tree = Rect {
        x: tree_area.x + 1,
        y: tree_area.y + 1,
        width: tree_area.width.saturating_sub(2),
        height: tree_area.height.saturating_sub(2),
    };

    // Render each zone
    let (session_count, active_count) = app.session_counts();
    app.area_theme_label = widgets::top_bar::render_top_bar(
        frame,
        top_bar,
        session_count,
        active_count,
        app.update_available,
    );

    // Don't show attention pulse for the session currently viewed in the interactor
    let mut visible_attention = app.attention_sessions.clone();
    if let Some(name) = app
        .cached_selected
        .as_ref()
        .and_then(|s| s.tmux_name.as_deref())
    {
        visible_attention.remove(name);
    }

    let attention_rects = widgets::tree::render_tree(
        frame,
        tree_area,
        &app.tree,
        &mut app.tree_state,
        true, // tree is always "focused" now (no focus switching)
        &visible_attention,
    );

    if let Some(logo_area) = logo_area {
        widgets::logo::render_logo(frame, logo_area, &app.logo_state);
    }

    if let Some(indicator) = indicator_area {
        let bar = Paragraph::new(Line::from(Span::styled(
            " ◉ NEXUS ",
            theme::style_for(ThemeElement::LogoNexus),
        )))
        .style(Style::new().bg(theme::surface()));
        frame.render_widget(bar, indicator);
    }

    // Store interactor inner area for mouse text selection hit-testing
    app.area_interactor_inner = Rect {
        x: interactor_area.x + 1,
        y: interactor_area.y + 1,
        width: interactor_area.width.saturating_sub(2),
        height: interactor_area.height.saturating_sub(2),
    };

    // Clamp live_scroll_offset to max scrollable range before reading it
    let inner_height = interactor_area.height.saturating_sub(2); // border top+bottom
    if let Some(ref mut is) = app.interactor_state {
        if let Some(SessionContent::Live(ref text)) = is.current_content {
            let max_offset = (text.lines.len() as u16).saturating_sub(inner_height);
            is.live_scroll_offset = is.live_scroll_offset.min(max_offset);
        }
    }

    // Render the session interactor
    let interactor_content = app
        .interactor_state
        .as_ref()
        .and_then(|is| is.current_content.as_ref());
    let interactor_session_name = app
        .interactor_state
        .as_ref()
        .and_then(|is| is.current_session_name.as_deref());
    let log_scroll = app
        .interactor_state
        .as_ref()
        .map(|is| is.log_scroll_offset)
        .unwrap_or(0);
    let live_scroll = app
        .interactor_state
        .as_ref()
        .map(|is| is.live_scroll_offset)
        .unwrap_or(0);
    widgets::interactor::render_interactor(
        frame,
        interactor_area,
        interactor_content,
        interactor_session_name,
        log_scroll,
        live_scroll,
    );

    // Cache interactor text and render selection highlight
    {
        let inner = app.area_interactor_inner;
        let buf = frame.buffer_mut();

        // Cache cell symbols for text extraction
        if app.text_selection.is_some() {
            app.interactor_rendered_cells.clear();
            for y in inner.y..inner.y + inner.height {
                let mut row = Vec::with_capacity(inner.width as usize);
                for x in inner.x..inner.x + inner.width {
                    row.push(buf[(x, y)].symbol().to_string());
                }
                app.interactor_rendered_cells.push(row);
            }
        }

        // Apply selection highlight (REVERSED modifier)
        if let Some(ref sel) = app.text_selection {
            let (start, end) = sel.normalized();
            for y in start.1.max(inner.y)..=end.1.min(inner.y + inner.height.saturating_sub(1)) {
                let x_start = if y == start.1 {
                    start.0.max(inner.x)
                } else {
                    inner.x
                };
                let x_end = if y == end.1 {
                    end.0.min(inner.x + inner.width.saturating_sub(1))
                } else {
                    inner.x + inner.width.saturating_sub(1)
                };
                for x in x_start..=x_end {
                    let style = buf[(x, y)].style().add_modifier(Modifier::REVERSED);
                    buf[(x, y)].set_style(style);
                }
            }
        }
    }

    // Input prompt overlay (renders over full main area for readability)
    match app.input_mode {
        InputMode::TextInput => {
            render_text_input(frame, main_area, app);
        }
        InputMode::Confirm => {
            render_confirm(frame, main_area, app);
        }
        InputMode::GroupPicker => {
            render_group_picker(frame, main_area, app);
        }
        InputMode::TypePicker => {
            render_type_picker(frame, main_area, app);
        }
        InputMode::Finder => {
            render_finder(frame, area, app);
        }
        InputMode::Normal => {}
    }

    // Status message overlay
    if let Some((ref msg, _)) = app.status_message {
        let msg_width = area.width.min(msg.len() as u16 + 4);
        let msg_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(1),
            width: msg_width,
            height: 1,
        };
        let status = Paragraph::new(format!(" {msg} "))
            .style(Style::new().fg(theme::hazard()).bg(theme::surface()));
        frame.render_widget(status, msg_area);
    }

    // Help overlay
    if app.show_help {
        render_help_overlay(frame, area);
    }

    // Apply TachyonFX attention pulse effects to flagged session rows
    for (tmux_name, rect) in &attention_rects {
        if let Some(effect) = app.attention_effects.get_mut(tmux_name) {
            frame.render_effect(effect, *rect, elapsed.into());
        }
    }

    // Apply TachyonFX boot effects (skip once done)
    if !app.boot_done {
        let zones = [top_bar, left_panel, right_column];
        for (effect, &zone) in app.boot_effects.iter_mut().zip(zones.iter()) {
            frame.render_effect(effect, zone, elapsed.into());
        }
        if app.boot_effects.iter().all(|e| e.done()) {
            app.boot_done = true;
            app.boot_effects.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Text input prompt
// ---------------------------------------------------------------------------

fn render_text_input(frame: &mut Frame, panel_area: Rect, app: &App) {
    let label = match &app.input_context {
        Some(InputContext::NewSessionName) => "Session name",
        Some(InputContext::NewSessionCwd { .. }) => "Working directory",
        Some(InputContext::RenameSession { .. }) => "New name",
        Some(InputContext::RenameGroup { .. }) => "New name",
        Some(InputContext::NewGroupName) => "Group name",
        Some(InputContext::NewSessionCommand { .. }) => "Launch command",
        _ => "Input",
    };

    let is_cwd = matches!(app.input_context, Some(InputContext::NewSessionCwd { .. }));
    let has_suggestions = is_cwd && !app.path_suggestions.is_empty();

    const VISIBLE_MAX: usize = 5;

    let prompt_height = 3u16;
    let (visible_count, scroll_offset) = if has_suggestions {
        let total = app.path_suggestions.len();
        let vis = total.min(VISIBLE_MAX);
        let offset = if total <= VISIBLE_MAX {
            0
        } else {
            app.path_suggestion_cursor
                .saturating_sub(VISIBLE_MAX / 2)
                .min(total - VISIBLE_MAX)
        };
        (vis, offset)
    } else {
        (0, 0)
    };
    let suggestion_height = if has_suggestions {
        visible_count as u16 + 2 // +2 for borders
    } else {
        0
    };
    let total_height = prompt_height + suggestion_height;

    let prompt_area = Rect {
        x: panel_area.x,
        y: panel_area.y + panel_area.height.saturating_sub(total_height),
        width: panel_area.width,
        height: total_height,
    };

    // Render suggestion dropdown above the input prompt
    if has_suggestions {
        let suggestion_area = Rect {
            x: prompt_area.x,
            y: prompt_area.y,
            width: prompt_area.width,
            height: suggestion_height,
        };

        let total = app.path_suggestions.len();
        let can_scroll_up = scroll_offset > 0;
        let can_scroll_down = scroll_offset + visible_count < total;
        let scroll_indicator = match (can_scroll_up, can_scroll_down) {
            (true, true) => " \u{25B2}\u{25BC} ",
            (true, false) => " \u{25B2} ",
            (false, true) => " \u{25BC} ",
            (false, false) => "",
        };

        let mut suggestion_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme::primary()))
            .style(Style::new().bg(theme::surface()));
        if !scroll_indicator.is_empty() {
            suggestion_block = suggestion_block.title(Span::styled(
                scroll_indicator,
                Style::new().fg(theme::dim()),
            ));
        }

        let suggestion_inner = suggestion_block.inner(suggestion_area);
        frame.render_widget(Clear, suggestion_area);
        frame.render_widget(suggestion_block, suggestion_area);

        let visible_slice = &app.path_suggestions[scroll_offset..scroll_offset + visible_count];
        let lines: Vec<Line> = visible_slice
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let global_index = scroll_offset + i;
                let is_selected = global_index == app.path_suggestion_cursor;
                let prefix = if is_selected { "> " } else { "  " };
                let style = if is_selected {
                    Style::new()
                        .fg(theme::primary())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme::text())
                };
                Line::from(Span::styled(format!("{prefix}{path}"), style))
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), suggestion_inner);
    }

    // Render the text input prompt below suggestions
    let input_area = Rect {
        x: prompt_area.x,
        y: prompt_area.y + suggestion_height,
        width: prompt_area.width,
        height: prompt_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::primary()))
        .style(Style::new().bg(theme::surface()));

    let inner = block.inner(input_area);
    frame.render_widget(Clear, input_area);
    frame.render_widget(block, input_area);

    let cursor_char = "\u{2588}"; // █
    let content = Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::new()
                .fg(theme::primary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(&app.input_buffer, Style::new().fg(theme::text())),
        Span::styled(cursor_char, Style::new().fg(theme::primary())),
    ]);

    frame.render_widget(Paragraph::new(content), inner);
}

// ---------------------------------------------------------------------------
// Confirm dialog
// ---------------------------------------------------------------------------

fn render_confirm(frame: &mut Frame, panel_area: Rect, app: &App) {
    let message = match &app.input_context {
        Some(InputContext::ConfirmDeleteSession {
            worktree: Some(_), ..
        }) => "Delete session AND worktree? (y/n/s=session only)",
        Some(InputContext::ConfirmDeleteSession { .. }) => "Delete this session? (y/n)",
        Some(InputContext::ConfirmDeleteGroup { .. }) => "Delete this group? (y/n)",
        Some(InputContext::NewSessionWorktree { .. }) => "Isolate in git worktree? (y/n)",
        _ => "Confirm? (y/n)",
    };

    let prompt_height = 3u16;
    let prompt_area = Rect {
        x: panel_area.x,
        y: panel_area.y + panel_area.height.saturating_sub(prompt_height),
        width: panel_area.width,
        height: prompt_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::hazard()))
        .style(Style::new().bg(theme::surface()));

    let inner = block.inner(prompt_area);
    frame.render_widget(Clear, prompt_area);
    frame.render_widget(block, prompt_area);

    let content = Paragraph::new(Span::styled(
        message,
        Style::new()
            .fg(theme::hazard())
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(content, inner);
}

// ---------------------------------------------------------------------------
// Group picker overlay
// ---------------------------------------------------------------------------

fn render_group_picker(frame: &mut Frame, panel_area: Rect, app: &App) {
    let group_count = app.picker_groups.len();
    let picker_height = (group_count as u16 + 2).min(panel_area.height); // +2 for borders

    let picker_area = Rect {
        x: panel_area.x,
        y: panel_area.y + panel_area.height.saturating_sub(picker_height),
        width: panel_area.width,
        height: picker_height,
    };

    let title = if matches!(
        app.input_context,
        Some(InputContext::NewSessionGroup { .. })
    ) {
        " Select group "
    } else {
        " Move to group "
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::new()
                .fg(theme::primary())
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::primary()))
        .style(Style::new().bg(theme::surface()));

    let inner = block.inner(picker_area);
    frame.render_widget(Clear, picker_area);
    frame.render_widget(block, picker_area);

    let lines: Vec<Line> = app
        .picker_groups
        .iter()
        .enumerate()
        .map(|(i, (_gid, name))| {
            let is_selected = i == app.picker_cursor;
            let prefix = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                Style::new()
                    .fg(theme::primary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::text())
            };
            Line::from(Span::styled(format!("{prefix}{name}"), style))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Final wizard step: pick the launch command (Claude / config types / Custom).
fn render_type_picker(frame: &mut Frame, panel_area: Rect, app: &App) {
    let type_count = app.picker_types.len();
    let picker_height = (type_count as u16 + 2).min(panel_area.height); // +2 for borders

    let picker_area = Rect {
        x: panel_area.x,
        y: panel_area.y + panel_area.height.saturating_sub(picker_height),
        width: panel_area.width,
        height: picker_height,
    };

    let block = Block::default()
        .title(Span::styled(
            " Session type ",
            Style::new()
                .fg(theme::primary())
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::primary()))
        .style(Style::new().bg(theme::surface()));

    let inner = block.inner(picker_area);
    frame.render_widget(Clear, picker_area);
    frame.render_widget(block, picker_area);

    let lines: Vec<Line> = app
        .picker_types
        .iter()
        .enumerate()
        .map(|(i, (label, cmd))| {
            let is_selected = i == app.picker_cursor;
            let prefix = if is_selected { "> " } else { "  " };
            let style = if is_selected {
                Style::new()
                    .fg(theme::primary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::text())
            };
            // Show the command alongside the label for concrete types.
            let text = match cmd {
                Some(c) => format!("{prefix}{label}  ({c})"),
                None => format!("{prefix}{label}"),
            };
            Line::from(Span::styled(text, style))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Session finder overlay
// ---------------------------------------------------------------------------

fn render_finder(frame: &mut Frame, area: Rect, app: &App) {
    // Small terminal guard
    if area.width < 40 || area.height < 10 {
        return;
    }

    let result_count = app.finder_state.result_count();
    let max_visible = (area.height * 40 / 100).max(5) as usize;
    let visible_count = result_count.min(max_visible);
    // +4 for borders (2) + input row (1) + hints row (1)
    let content_height = (visible_count as u16 + 4).min(area.height);
    let content_width = (area.width * 60 / 100).max(40).min(area.width);

    let overlay = centered_rect(content_width, content_height, area);

    let block = Block::default()
        .title(Span::styled(
            " SESSION FINDER ",
            Style::new()
                .fg(theme::primary())
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::primary()))
        .style(Style::new().bg(theme::surface()));

    let inner = block.inner(overlay);
    frame.render_widget(Clear, overlay);
    frame.render_widget(block, overlay);

    if inner.height < 2 || inner.width < 10 {
        return;
    }

    // Build lines: input row + results + hints
    let mut lines: Vec<Line> = Vec::new();

    // Input row with cursor
    lines.push(Line::from(vec![
        Span::styled("> ", Style::new().fg(theme::primary())),
        Span::styled(
            app.finder_state.query.clone(),
            Style::new().fg(theme::text()),
        ),
        Span::styled("_", Style::new().fg(theme::primary())),
    ]));

    let results = app.finder_state.results();
    if results.is_empty() {
        // Empty state
        let msg = if app.finder_state.query.is_empty() {
            "No sessions"
        } else {
            "No matching sessions"
        };
        lines.push(Line::from(Span::styled(
            format!("  {msg}"),
            Style::new().fg(theme::dim()),
        )));
    } else {
        // Scrolling: determine visible window around cursor
        let cursor = app.finder_state.cursor;
        let scroll_offset = if cursor >= max_visible {
            cursor - max_visible + 1
        } else {
            0
        };

        let inner_width = inner.width as usize;
        for (i, entry) in results
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(max_visible)
        {
            let is_selected = i == cursor;

            let status_icon = match entry.status {
                SessionStatus::Active => "●",
                SessionStatus::Detached => "○",
                SessionStatus::Dead => "×",
            };

            let status_style = match entry.status {
                SessionStatus::Active => Style::new().fg(theme::secondary()),
                SessionStatus::Detached => Style::new().fg(theme::dim()),
                SessionStatus::Dead => Style::new().fg(theme::dim()),
            };

            // Calculate space for name, group, and cwd
            // Format: " {icon} {name}  {group}  {cwd}"
            let prefix_len = 4; // " X "
            let group_display = if entry.group_name.is_empty() {
                String::new()
            } else {
                format!("  {}", entry.group_name)
            };

            let name = &entry.display_name;
            let remaining =
                inner_width.saturating_sub(prefix_len + name.len() + group_display.len());
            let cwd_display = if !entry.cwd.is_empty() && remaining > 6 {
                let cwd = &entry.cwd;
                let max_cwd = remaining.saturating_sub(2); // "  " prefix
                if cwd.len() > max_cwd {
                    format!("  ...{}", &cwd[cwd.len() - max_cwd + 3..])
                } else {
                    format!("  {cwd}")
                }
            } else {
                String::new()
            };

            let row_style = if is_selected {
                Style::new().bg(theme::bg())
            } else {
                Style::new()
            };

            let name_style = if is_selected {
                Style::new()
                    .fg(theme::primary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::text())
            };

            lines.push(Line::from(vec![
                Span::styled(format!(" {status_icon} "), status_style),
                Span::styled(name.clone(), name_style.patch(row_style)),
                Span::styled(
                    group_display,
                    Style::new().fg(theme::dim()).patch(row_style),
                ),
                Span::styled(cwd_display, Style::new().fg(theme::dim()).patch(row_style)),
            ]));
        }
    }

    // Pad remaining space then add hints at bottom
    let used_lines = lines.len() as u16;
    let available = inner.height;
    if used_lines < available {
        for _ in used_lines..available.saturating_sub(1) {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            " ↑↓ navigate  Enter select  Esc close",
            Style::new().fg(theme::dim()),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Help overlay
// ---------------------------------------------------------------------------

fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let bindings: Vec<(&str, &str)> = vec![
        ("", "-- Nexus Commands (Alt+key) --"),
        ("Alt+q", "Quit Nexus"),
        ("Alt+h / Alt+?", "Toggle this help"),
        ("Alt+j", "Cursor down"),
        ("Alt+k", "Cursor up"),
        ("Alt+Enter", "Toggle expand/collapse"),
        ("Alt+n", "New session"),
        ("Alt+g", "New group"),
        ("Alt+r", "Rename selected item"),
        ("Alt+m", "Move session to group"),
        ("Alt+d", "Delete selected item"),
        ("Alt+x", "Kill tmux (mark detached)"),
        ("Alt+H", "Toggle past/dead sessions"),
        ("Alt+t / Alt+T", "Cycle theme"),
        ("Alt+l", "Open lazygit in session cwd"),
        ("Alt+v", "Open editor in session cwd"),
        ("Alt+p", "Session finder"),
        ("", ""),
        ("", "Click+drag in session panel"),
        ("", "to select and copy text."),
        ("", ""),
        ("", "All other keys are forwarded to"),
        ("", "the embedded Claude Code session."),
        ("", ""),
        ("", "macOS: Enable 'Use Option as Meta"),
        ("", "key' in Terminal/iTerm2 settings."),
    ];

    let content_height = (bindings.len() as u16) + 4;
    let content_width = 52u16;

    let overlay = centered_rect(content_width, content_height, area);

    let block = Block::default()
        .title(Span::styled(
            " KEYBINDINGS ",
            Style::new()
                .fg(theme::primary())
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::primary()))
        .style(Style::new().bg(theme::bg()));

    let inner = block.inner(overlay);
    frame.render_widget(Clear, overlay);
    frame.render_widget(block, overlay);

    let lines: Vec<Line> = bindings
        .iter()
        .map(|(key, desc)| {
            if key.is_empty() {
                Line::from(Span::styled(
                    format!("  {desc}"),
                    Style::new().fg(theme::dim()),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{key:>18}"), Style::new().fg(theme::primary())),
                    Span::styled(format!("  {desc}"), Style::new().fg(theme::text())),
                ])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}
