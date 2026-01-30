//! UI rendering for the TUI dashboard

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::app::{App, LogLevel, Panel};
use crate::watchdog::ProcessState;
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
    // Split into left column (agent + system) and right column (pool + network + locks + log)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(65),
        ])
        .split(area);

    // Left column: Agent status + System
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(60),
            Constraint::Percentage(40),
        ])
        .split(body_chunks[0]);

    draw_agent_panel(f, app, left_chunks[0]);
    draw_system_panel(f, app, left_chunks[1]);

    // Right column: Pool + Network + Locks + Log
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),  // Pool
            Constraint::Length(8),  // Network
            Constraint::Length(5),  // Locks
            Constraint::Min(5),     // Log
        ])
        .split(body_chunks[1]);

    draw_pool_panel(f, app, right_chunks[0]);
    draw_network_panel(f, app, right_chunks[1]);
    draw_locks_panel(f, app, right_chunks[2]);
    draw_log_panel(f, app, right_chunks[3]);
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

        lines.push(Line::from(format!("Uptime: {}", app.uptime_str())));
        lines.push(Line::from(format!("Restarts: {}", state.restart_count)));

        // Health info
        if let Some(health) = &state.health {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Health:", Style::default().add_modifier(Modifier::BOLD))));

            let (state_icon, state_color) = match health.state {
                ProcessState::Active => ("●", Color::Green),
                ProcessState::Starting => ("◐", Color::Yellow),
                ProcessState::Idle => ("○", Color::Yellow),
                ProcessState::Unresponsive => ("!", Color::Red),
                ProcessState::HighResource => ("▲", Color::Magenta),
                ProcessState::Exited => ("×", Color::Gray),
            };

            lines.push(Line::from(vec![
                Span::raw("  State: "),
                Span::styled(format!("{} {:?}", state_icon, health.state), Style::default().fg(state_color)),
            ]));

            lines.push(Line::from(format!("  Last activity: {}s ago", health.last_activity_secs)));
            lines.push(Line::from(format!("  Memory: {} MB", health.memory_mb)));
            lines.push(Line::from(format!("  CPU: {:.1}%", health.cpu_percent)));

            if health.unresponsive_count > 0 {
                lines.push(Line::from(vec![
                    Span::raw("  Unresponsive: "),
                    Span::styled(
                        format!("{}", health.unresponsive_count),
                        Style::default().fg(Color::Red),
                    ),
                ]));
            }

            if let Some(action) = &health.action_pending {
                lines.push(Line::from(vec![
                    Span::styled("  ⚠ Action: ", Style::default().fg(Color::Yellow)),
                    Span::styled(format!("{:?}", action), Style::default().fg(Color::Yellow)),
                ]));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Waiting for agent data...",
            Style::default().fg(Color::Gray),
        )));
    }

    let content = Paragraph::new(lines).wrap(Wrap { trim: true });
    f.render_widget(content, inner);
}

fn draw_system_panel(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" System ")
        .borders(Borders::ALL);

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![];

    if let Some(state) = &app.shared_state {
        lines.push(Line::from(format!("Wrapper PID: {}", state.wrapper_pid)));

        if let Some(health) = &state.health {
            // Memory bar
            let mem_percent = (health.memory_mb as f64 / 8192.0 * 100.0).min(100.0);
            let bar_width = 20;
            let filled = (mem_percent / 100.0 * bar_width as f64) as usize;
            let empty = bar_width - filled;
            let bar = format!("[{}{}]", "█".repeat(filled), "░".repeat(empty));

            lines.push(Line::from(vec![
                Span::raw("Memory: "),
                Span::styled(bar, Style::default().fg(if mem_percent > 80.0 { Color::Red } else if mem_percent > 60.0 { Color::Yellow } else { Color::Green })),
                Span::raw(format!(" {}MB", health.memory_mb)),
            ]));

            // CPU bar
            let cpu_percent = health.cpu_percent.min(100.0);
            let filled = (cpu_percent / 100.0 * bar_width as f32) as usize;
            let empty = bar_width - filled;
            let bar = format!("[{}{}]", "█".repeat(filled), "░".repeat(empty));

            lines.push(Line::from(vec![
                Span::raw("CPU:    "),
                Span::styled(bar, Style::default().fg(if cpu_percent > 80.0 { Color::Red } else if cpu_percent > 60.0 { Color::Yellow } else { Color::Green })),
                Span::raw(format!(" {:.1}%", cpu_percent)),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No system data",
            Style::default().fg(Color::Gray),
        )));
    }

    let content = Paragraph::new(lines);
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

fn draw_network_panel(f: &mut Frame, app: &App, area: Rect) {
    let selected = app.selected_panel == Panel::Network;
    let border_style = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let block = Block::default()
        .title(" Network Activity ")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(stats) = &app.network_stats {
        let mut lines = vec![
            Line::from(format!("Connections: {} total", stats.total_connections)),
            Line::from(format!(
                "Traffic: ↑ {} | ↓ {}",
                format_bytes(stats.bytes_sent),
                format_bytes(stats.bytes_received)
            )),
        ];

        if !stats.top_targets.is_empty() {
            lines.push(Line::from("Top targets:"));
            for (target, count) in stats.top_targets.iter().take(3) {
                lines.push(Line::from(format!("  {} ({})", target, count)));
            }
        }

        let content = Paragraph::new(lines);
        f.render_widget(content, inner);
    } else {
        let content = Paragraph::new(Span::styled(
            "Network monitoring not active (use --netmon)",
            Style::default().fg(Color::Gray),
        ));
        f.render_widget(content, inner);
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
                LogLevel::Warn => ("WARN", Style::default().fg(Color::Yellow)),
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

/// Format bytes as human-readable string
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
