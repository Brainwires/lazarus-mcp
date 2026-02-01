//! UI rendering for the TUI dashboard

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::app::{App, LogLevel, Panel};
use crate::wrapper::AgentState;

/// Draw the entire UI
pub fn draw(f: &mut Frame, app: &mut App) {
    // Main layout: header + body
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(0),    // Body
        ])
        .split(f.area());

    draw_header(f, app, main_chunks[0]);
    draw_body(f, app, main_chunks[1]);

    // Draw help overlay if active
    if app.show_help {
        draw_help_overlay(f);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let status = if let Some(state) = &app.shared_state {
        match state.agent_status {
            AgentState::Running => ("Running", Color::Green),
            AgentState::Starting => ("Starting", Color::Yellow),
            AgentState::Restarting => ("Restarting", Color::Yellow),
            AgentState::Stopped => ("Stopped", Color::Red),
            AgentState::Failed => ("Failed", Color::Red),
        }
    } else {
        ("Unknown", Color::Gray)
    };

    let title = Line::from(vec![
        Span::styled(" AEGIS-MCP ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("| Status: "),
        Span::styled(status.0, Style::default().fg(status.1)),
        Span::raw(" | "),
        Span::raw("[q]uit [h]elp [r]estart [Tab] switch panel"),
    ]);

    let header = Paragraph::new(title)
        .style(Style::default().bg(Color::DarkGray));
    f.render_widget(header, area);
}

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    // Split into left column (agent) and right column (pool + locks + log)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(65),
        ])
        .split(area);

    draw_agent_panel(f, app, body_chunks[0]);

    // Right column: Pool + Locks + Log
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // Pool
            Constraint::Length(4),  // Locks
            Constraint::Min(6),     // Log
        ])
        .split(body_chunks[1]);

    draw_pool_panel(f, app, right_chunks[0]);
    draw_locks_panel(f, app, right_chunks[1]);
    draw_log_panel(f, app, right_chunks[2]);
}

fn draw_agent_panel(f: &mut Frame, app: &App, area: Rect) {
    let selected = app.selected_panel == Panel::Agent;
    let border_style = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let block = Block::default()
        .title(" Primary Agent ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![];

    if let Some(state) = &app.shared_state {
        // Status line with icon
        let (icon, status_color) = match state.agent_status {
            AgentState::Running => ("●", Color::Green),
            AgentState::Starting => ("◐", Color::Yellow),
            AgentState::Restarting => ("↻", Color::Yellow),
            AgentState::Stopped => ("○", Color::Red),
            AgentState::Failed => ("✗", Color::Red),
        };

        lines.push(Line::from(vec![
            Span::raw("Status: "),
            Span::styled(format!("{} {:?}", icon, state.agent_status), Style::default().fg(status_color)),
        ]));

        lines.push(Line::from(format!("Agent: {}", state.agent_name)));

        if let Some(pid) = state.agent_pid {
            lines.push(Line::from(format!("PID: {}", pid)));
        }

        lines.push(Line::from(format!("Wrapper PID: {}", state.wrapper_pid)));
        lines.push(Line::from(format!("Uptime: {}", app.uptime_str())));
        lines.push(Line::from(format!("Restarts: {}", state.restart_count)));
    } else {
        lines.push(Line::from(Span::styled(
            "Waiting for agent data...",
            Style::default().fg(Color::Gray),
        )));
    }

    let content = Paragraph::new(lines).wrap(Wrap { trim: true });
    f.render_widget(content, inner);
}

fn draw_pool_panel(f: &mut Frame, app: &App, area: Rect) {
    let selected = app.selected_panel == Panel::Pool;
    let border_style = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let block = Block::default()
        .title(" Agent Pool ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.pool_agents.is_empty() {
        let content = Paragraph::new(Span::styled(
            "No active pool agents",
            Style::default().fg(Color::Gray),
        ));
        f.render_widget(content, inner);
    } else {
        let items: Vec<ListItem> = app
            .pool_agents
            .iter()
            .map(|agent| {
                let content = format!(
                    "{} {} - {} (iter: {}, {}s)",
                    if agent.status == "Running" { "▶" } else { "✓" },
                    &agent.id[..8.min(agent.id.len())],
                    agent.task,
                    agent.iterations,
                    agent.elapsed_secs
                );
                ListItem::new(content)
            })
            .collect();

        let list = List::new(items);
        f.render_widget(list, inner);
    }
}

fn draw_locks_panel(f: &mut Frame, app: &App, area: Rect) {
    let selected = app.selected_panel == Panel::Locks;
    let border_style = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let block = Block::default()
        .title(" File Locks ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.file_locks.is_empty() {
        let content = Paragraph::new(Span::styled(
            "No file locks held",
            Style::default().fg(Color::Gray),
        ));
        f.render_widget(content, inner);
    } else {
        let items: Vec<ListItem> = app
            .file_locks
            .iter()
            .map(|lock| {
                ListItem::new(format!(
                    "{} ({}) - {}",
                    lock.path, lock.lock_type, lock.agent_id
                ))
            })
            .collect();

        let list = List::new(items);
        f.render_widget(list, inner);
    }
}

fn draw_log_panel(f: &mut Frame, app: &App, area: Rect) {
    let selected = app.selected_panel == Panel::Log;
    let border_style = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let block = Block::default()
        .title(" Log ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    let items: Vec<ListItem> = app
        .logs
        .iter()
        .skip(app.log_scroll)
        .map(|entry| {
            let elapsed = entry.timestamp.elapsed();
            let time_str = format!("{:02}:{:02}", elapsed.as_secs() / 60, elapsed.as_secs() % 60);

            let (prefix, style) = match entry.level {
                LogLevel::Info => ("INFO", Style::default().fg(Color::Green)),
                LogLevel::Error => ("ERR ", Style::default().fg(Color::Red)),
            };

            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", time_str), Style::default().fg(Color::Gray)),
                Span::styled(format!("[{}] ", prefix), style),
                Span::raw(&entry.message),
            ]))
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn draw_help_overlay(f: &mut Frame) {
    let area = centered_rect(60, 50, f.area());

    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::DarkGray));

    let help_text = vec![
        Line::from(Span::styled("Keyboard Shortcuts", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("  q, Esc     Quit dashboard"),
        Line::from("  h, ?       Show this help"),
        Line::from("  Tab        Next panel"),
        Line::from("  Shift+Tab  Previous panel"),
        Line::from("  r          Restart agent"),
        Line::from("  j, Down    Scroll down (in log)"),
        Line::from("  k, Up      Scroll up (in log)"),
        Line::from(""),
        Line::from(Span::styled("Press any key to close", Style::default().fg(Color::Gray))),
    ];

    let paragraph = Paragraph::new(help_text)
        .block(block)
        .wrap(Wrap { trim: true });

    f.render_widget(paragraph, area);
}

/// Helper function to create a centered rect
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
