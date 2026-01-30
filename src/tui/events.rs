//! Event handling for the TUI dashboard

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use std::time::Duration;

use super::app::App;

/// Handle events and return true if should quit
pub fn handle_events(app: &mut App, tick_rate: Duration) -> Result<bool> {
    if event::poll(tick_rate)? {
        if let Event::Key(key) = event::read()? {
            // Only handle key press events, not release
            if key.kind == KeyEventKind::Press {
                app.handle_key(key.code);
            }
        }
    }

    Ok(app.should_quit)
}
