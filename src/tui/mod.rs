//! TUI Dashboard for Aegis-MCP
//!
//! Provides a terminal-based dashboard for monitoring agents.

mod app;
mod events;
mod ui;

pub use app::App;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io;
use std::time::Duration;

/// Run the TUI dashboard
pub fn run_dashboard(wrapper_pid: u32) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app state
    let app = App::new(wrapper_pid);

    // Run the main loop
    let res = run_app(&mut terminal, app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, mut app: App) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(100);

    loop {
        // Draw UI
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Handle events
        if events::handle_events(&mut app, tick_rate)? {
            return Ok(());
        }

        // Update state
        app.update();
    }
}
