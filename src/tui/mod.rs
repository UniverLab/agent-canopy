//! Canopy Agent Hub — TUI for monitoring and managing agents.
//!
//! Reads the daemon's `SQLite` database in read-only mode (WAL allows
//! concurrent readers) and displays background_agents, watchers, and their logs
//! in a card-based sidebar with a live log panel.

mod agent;
mod app;
mod atmosphere;
mod brians_brain;
mod clipboard;
pub(crate) mod context_transfer;
mod event;
mod gamification;
pub(crate) mod mcp_client;
pub(crate) mod prompt_templates;
pub(crate) mod selection;
pub(crate) mod terminal_history;
mod ui;
mod whimsg;

pub(crate) use ui::truncate_str_keep_tail;

use anyhow::{Context, Result};
use ratatui::crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use std::io;
use std::sync::Arc;

use crate::db::Database;
use crate::domain::db_paths::database_path;

use crate::tui::app::types::App;
use event::run_event_loop;

/// Entry point for `canopy tui`.
pub fn run_tui() -> Result<()> {
    crate::domain::notification::register_aumid();
    crate::domain::notification::clear_stale_notifications();
    let data_dir = crate::ensure_data_dir()?;
    let db_path = database_path(&data_dir);

    if !db_path.exists() {
        eprintln!("Daemon not running — starting it automatically…");
        auto_start_daemon(&data_dir)?;
        // Wait briefly for the daemon to create the database
        for _ in 0..20 {
            if db_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if !db_path.exists() {
            anyhow::bail!(
                "Daemon started but database not found at {}.\nCheck logs: canopy daemon logs",
                db_path.display()
            );
        }
    }

    let db = Arc::new(Database::new(&db_path).context("Failed to open database")?);
    let mut app = App::new(Arc::clone(&db), &data_dir)?;

    // Reap bridge sidecars whose owning process died without cleaning up
    app.reconcile_bridge_sessions();
    // Auto-resume previously active interactive sessions
    app.auto_resume_sessions();
    // Load orphaned sessions for TUI visibility
    if let Ok(orphaned) = app.db.get_orphaned_sessions() {
        app.orphaned_sessions = orphaned;
    }
    // Auto-resume previously active terminal sessions
    app.auto_resume_terminal_sessions();
    // Now that sessions are resumed (and their schedules reassigned), open the
    // scheduled-send delivery gate and drop schedules whose session is gone.
    app.restore_scheduled_sends();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    // Enable Kitty keyboard enhancement if supported — allows Shift+Enter
    // disambiguation. Where unsupported, Ctrl+S remains the fallback send key.
    let ke_supported = supports_keyboard_enhancement().unwrap_or(false);
    if ke_supported {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    app.keyboard_enhancement_active = ke_supported;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Run
    let result = run_event_loop(&mut terminal, &mut app);

    // Restore terminal — always, even on error
    disable_raw_mode()?;
    if ke_supported {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    result
}

/// Try to start the daemon process automatically.
fn auto_start_daemon(data_dir: &std::path::Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log_path = data_dir.join("daemon.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve")
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    cmd.spawn().context("Failed to spawn daemon process")?;
    Ok(())
}
