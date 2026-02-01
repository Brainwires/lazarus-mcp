//! Application state for the TUI dashboard

use std::collections::VecDeque;
use std::time::Instant;

use crate::wrapper::SharedState;

/// Maximum number of log entries to keep
const MAX_LOG_ENTRIES: usize = 100;

/// Application state
pub struct App {
    /// Wrapper PID to load shared state
    pub wrapper_pid: u32,
    /// Cached shared state
    pub shared_state: Option<SharedState>,
    /// Selected panel (for keyboard navigation)
    pub selected_panel: Panel,
    /// Log entries
    pub logs: VecDeque<LogEntry>,
    /// Whether help overlay is shown
    pub show_help: bool,
    /// Last update time
    pub last_update: Instant,
    /// Whether app should quit
    pub should_quit: bool,
    /// Scroll offset for logs
    pub log_scroll: usize,
    /// Pool agents list (cached)
    pub pool_agents: Vec<PoolAgentInfo>,
    /// File locks (cached)
    pub file_locks: Vec<FileLockInfo>,
}

/// Selectable panel
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Agent,
    Pool,
    Locks,
    Log,
}

impl Panel {
    pub fn next(&self) -> Self {
        match self {
            Panel::Agent => Panel::Pool,
            Panel::Pool => Panel::Locks,
            Panel::Locks => Panel::Log,
            Panel::Log => Panel::Agent,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            Panel::Agent => Panel::Log,
            Panel::Pool => Panel::Agent,
            Panel::Locks => Panel::Pool,
            Panel::Log => Panel::Locks,
        }
    }
}

/// Log entry
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: Instant,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Info,
    Error,
}

/// Pool agent info (simplified for display)
#[derive(Debug, Clone)]
pub struct PoolAgentInfo {
    pub id: String,
    pub status: String,
    pub task: String,
    pub iterations: u32,
    pub elapsed_secs: u64,
}

/// File lock info
#[derive(Debug, Clone)]
pub struct FileLockInfo {
    pub path: String,
    pub lock_type: String,
    pub agent_id: String,
}

impl App {
    pub fn new(wrapper_pid: u32) -> Self {
        let now = Instant::now();
        let mut app = Self {
            wrapper_pid,
            shared_state: None,
            selected_panel: Panel::Agent,
            logs: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            show_help: false,
            last_update: now,
            should_quit: false,
            log_scroll: 0,
            pool_agents: Vec::new(),
            file_locks: Vec::new(),
        };

        app.log(LogLevel::Info, "Dashboard started");
        app
    }

    /// Log a message
    pub fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        if self.logs.len() >= MAX_LOG_ENTRIES {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            timestamp: Instant::now(),
            level,
            message: message.into(),
        });
    }

    /// Update state from various sources
    pub fn update(&mut self) {
        // Only update every 500ms to avoid excessive file reads
        if self.last_update.elapsed().as_millis() < 500 {
            return;
        }
        self.last_update = Instant::now();

        // Load shared state from file
        if let Ok(state) = SharedState::load(self.wrapper_pid) {
            self.shared_state = Some(state);
        }

        // Update pool agents
        self.update_pool_agents();

        // Update file locks
        self.update_file_locks();
    }

    fn update_pool_agents(&mut self) {
        // Pool info would need to be exposed via a file or IPC
        // For now, we'll leave this as a placeholder
    }

    fn update_file_locks(&mut self) {
        // File locks would need to be exposed via a file or IPC
        // For now, we'll leave this as a placeholder
    }

    /// Get uptime as formatted string
    pub fn uptime_str(&self) -> String {
        if let Some(state) = &self.shared_state {
            let secs = state.uptime_secs;
            let hours = secs / 3600;
            let mins = (secs % 3600) / 60;
            let secs = secs % 60;
            if hours > 0 {
                format!("{}h {}m {}s", hours, mins, secs)
            } else if mins > 0 {
                format!("{}m {}s", mins, secs)
            } else {
                format!("{}s", secs)
            }
        } else {
            "Unknown".to_string()
        }
    }

    /// Handle key input
    pub fn handle_key(&mut self, key: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;

        if self.show_help {
            self.show_help = false;
            return;
        }

        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('?') | KeyCode::Char('h') => self.show_help = true,
            KeyCode::Tab => self.selected_panel = self.selected_panel.next(),
            KeyCode::BackTab => self.selected_panel = self.selected_panel.prev(),
            KeyCode::Char('r') => {
                // Trigger restart via signal file
                let signal_path = format!("/tmp/lazarus-mcp-{}", self.wrapper_pid);
                let signal = serde_json::json!({
                    "reason": "TUI restart request"
                });
                if std::fs::write(&signal_path, signal.to_string()).is_ok() {
                    self.log(LogLevel::Info, "Restart signal sent");
                } else {
                    self.log(LogLevel::Error, "Failed to send restart signal");
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_panel == Panel::Log {
                    if self.log_scroll < self.logs.len().saturating_sub(1) {
                        self.log_scroll += 1;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_panel == Panel::Log {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}
