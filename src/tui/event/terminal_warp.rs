use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use std::time::Duration;

use super::search_picker::resolve_cd_path;
use crate::tui::agent::{key_to_bytes, InteractiveAgent};
use crate::tui::app::terminal_search::TerminalSearch;
use crate::tui::app::types::App;

const DIRECT_SYNC_WAIT_MS: u64 = 35;
const TAB_SYNC_WAIT_MS: u64 = 90;
const SCROLL_STEP: usize = 3;
const PAGE_SCROLL_STEP: usize = 15;
const ETX: u8 = 0x03;
const EOT: u8 = 0x04;
const FF: u8 = 0x0c;

// ── Terminal warp-mode key handling ─────────────────────────────────

/// Handle keystrokes for a terminal agent in warp mode.
/// Keys are accumulated in the input buffer and only sent to PTY on Enter.
pub fn sync_terminal_warp_buffer_from_pty(app: &mut App, idx: usize, wait_ms: u64) {
    let agent = &app.terminal_agents[idx];
    if should_skip_warp_sync(agent) {
        return;
    }

    let Some(input) = agent.sync_warp_input_from_pty(Duration::from_millis(wait_ms)) else {
        return;
    };
    let Some(()) = replace_input_buffer(agent, &input) else {
        return;
    };

    app.terminal_agents[idx].warp_cursor = input.len();
    app.terminal_agents[idx].warp_passthrough = true;
}

pub fn handle_terminal_direct_pty_key(
    app: &mut App,
    idx: usize,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<()> {
    let bytes = key_to_bytes(code, modifiers);
    let sensitive_input = {
        let agent = &app.terminal_agents[idx];
        if !bytes.is_empty() {
            let _ = agent.write_to_pty(&bytes);
        }
        agent.is_sensitive_input_active()
    };

    if sensitive_input {
        reset_warp_input(&mut app.terminal_agents[idx]);
        return Ok(());
    }

    if should_sync_direct_input(&app.terminal_agents[idx], code, modifiers, &bytes) {
        sync_terminal_warp_buffer_from_pty(app, idx, wait_ms_for_key(code));
    }

    match code {
        KeyCode::Enter => submit_direct_passthrough(app, idx),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            reset_warp_input(&mut app.terminal_agents[idx]);
        }
        _ => {
            app.terminal_agents[idx].history_index = None;
        }
    }

    Ok(())
}

pub fn handle_terminal_warp_key(
    app: &mut App,
    idx: usize,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<()> {
    if app.terminal_agents[idx].warp_passthrough
        || app.terminal_agents[idx].should_bypass_warp_input()
    {
        return handle_terminal_direct_pty_key(app, idx, code, modifiers);
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        handle_terminal_warp_control_key(app, idx, code);
        return Ok(());
    }

    match code {
        KeyCode::Enter => submit_warp_input(app, idx),
        KeyCode::Tab => return handle_warp_tab(app, idx),
        KeyCode::Char(c) => insert_char_at_cursor(&mut app.terminal_agents[idx], c),
        KeyCode::Backspace => delete_before_cursor(&mut app.terminal_agents[idx]),
        KeyCode::Delete => delete_at_cursor(&mut app.terminal_agents[idx]),
        KeyCode::Left => move_cursor_left(&mut app.terminal_agents[idx]),
        KeyCode::Right => move_cursor_right(&mut app.terminal_agents[idx]),
        KeyCode::Home => app.terminal_agents[idx].warp_cursor = 0,
        KeyCode::End => move_cursor_to_end(&mut app.terminal_agents[idx]),
        KeyCode::Up => handle_warp_up_key(app, idx),
        KeyCode::Down => handle_warp_down_key(app, idx),
        KeyCode::PageUp => scroll_up(&mut app.terminal_agents[idx], PAGE_SCROLL_STEP),
        KeyCode::PageDown => scroll_down(&mut app.terminal_agents[idx], PAGE_SCROLL_STEP),
        _ => {}
    }

    Ok(())
}

fn should_skip_warp_sync(agent: &InteractiveAgent) -> bool {
    agent.in_alternate_screen() || agent.is_sensitive_input_active()
}

fn should_sync_direct_input(
    agent: &InteractiveAgent,
    code: KeyCode,
    modifiers: KeyModifiers,
    bytes: &[u8],
) -> bool {
    if bytes.is_empty() || is_direct_submit(code, modifiers) {
        return false;
    }

    !should_skip_warp_sync(agent)
}

fn is_direct_submit(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Enter)
        || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
        || (code == KeyCode::Char('d') && modifiers.contains(KeyModifiers::CONTROL))
}

fn wait_ms_for_key(code: KeyCode) -> u64 {
    if code == KeyCode::Tab {
        TAB_SYNC_WAIT_MS
    } else {
        DIRECT_SYNC_WAIT_MS
    }
}

fn handle_warp_tab(app: &mut App, idx: usize) -> Result<()> {
    let input = trimmed_input_text(&app.terminal_agents[idx]);
    if is_cd_picker_request(&input) {
        return open_terminal_suggestion_picker(app, idx);
    }

    let text = input_text(&app.terminal_agents[idx]);
    let _ = app.terminal_agents[idx].write_to_pty(text.as_bytes());
    let _ = app.terminal_agents[idx].write_to_pty(b"\t");
    sync_terminal_warp_buffer_from_pty(app, idx, TAB_SYNC_WAIT_MS);
    Ok(())
}

fn handle_terminal_warp_control_key(app: &mut App, idx: usize, code: KeyCode) {
    match code {
        KeyCode::Char('f') => {
            app.terminal_search = Some(TerminalSearch::new(idx));
        }
        KeyCode::Char('c') => {
            let _ = app.terminal_agents[idx].write_to_pty(&[ETX]);
            reset_warp_input(&mut app.terminal_agents[idx]);
        }
        KeyCode::Char('d') => {
            let _ = app.terminal_agents[idx].write_to_pty(&[EOT]);
            app.terminal_agents[idx].warp_passthrough = false;
        }
        KeyCode::Char('l') => {
            let _ = app.terminal_agents[idx].write_to_pty(&[FF]);
        }
        KeyCode::Char('u') => clear_before_cursor(&mut app.terminal_agents[idx]),
        KeyCode::Char('k') => clear_after_cursor(&mut app.terminal_agents[idx]),
        KeyCode::Char('a') => app.terminal_agents[idx].warp_cursor = 0,
        KeyCode::Char('e') => move_cursor_to_end(&mut app.terminal_agents[idx]),
        _ => {}
    }
}

fn handle_warp_up_key(app: &mut App, idx: usize) {
    let agent = &app.terminal_agents[idx];
    if agent.history_index.is_some() || input_is_blank(agent) {
        browse_history_up(app, idx);
        return;
    }

    scroll_up(&mut app.terminal_agents[idx], SCROLL_STEP);
}

fn handle_warp_down_key(app: &mut App, idx: usize) {
    if app.terminal_agents[idx].history_index.is_some() {
        browse_history_down(app, idx);
        return;
    }

    scroll_down(&mut app.terminal_agents[idx], SCROLL_STEP);
}

fn browse_history_up(app: &mut App, idx: usize) {
    let session_name = app.terminal_agents[idx].name.clone();
    let history_len = app
        .terminal_histories
        .get(&session_name)
        .map(|history| history.commands.len())
        .unwrap_or(0);
    if history_len == 0 {
        return;
    }

    let new_idx = app.terminal_agents[idx]
        .history_index
        .map_or(history_len - 1, |current| current.saturating_sub(1));
    app.terminal_agents[idx].history_index = Some(new_idx);
    load_history_entry(app, idx, &session_name, new_idx);
}

fn browse_history_down(app: &mut App, idx: usize) {
    let Some(current) = app.terminal_agents[idx].history_index else {
        return;
    };

    let session_name = app.terminal_agents[idx].name.clone();
    let history_len = app
        .terminal_histories
        .get(&session_name)
        .map(|history| history.commands.len())
        .unwrap_or(0);

    if current + 1 < history_len {
        app.terminal_agents[idx].history_index = Some(current + 1);
        load_history_entry(app, idx, &session_name, current + 1);
        return;
    }

    app.terminal_agents[idx].history_index = None;
    let _ = clear_input_buffer(&app.terminal_agents[idx]);
    app.terminal_agents[idx].warp_cursor = 0;
}

fn load_history_entry(app: &mut App, idx: usize, session_name: &str, history_idx: usize) {
    let Some(cmd) = app
        .terminal_histories
        .get(session_name)
        .and_then(|history| history.commands.get(history_idx))
        .map(|entry| entry.cmd.clone())
    else {
        return;
    };

    let Some(()) = replace_input_buffer(&app.terminal_agents[idx], &cmd) else {
        return;
    };
    app.terminal_agents[idx].warp_cursor = cmd.len();
}

fn submit_direct_passthrough(app: &mut App, idx: usize) {
    let captured = trimmed_input_text(&app.terminal_agents[idx]);
    record_terminal_command(app, idx, &captured);
    reset_warp_input(&mut app.terminal_agents[idx]);
}

fn submit_warp_input(app: &mut App, idx: usize) {
    let captured = input_text(&app.terminal_agents[idx]);
    write_line_to_pty(&app.terminal_agents[idx], &captured);
    record_terminal_command(app, idx, captured.trim());
    reset_warp_input(&mut app.terminal_agents[idx]);
}

fn write_line_to_pty(agent: &InteractiveAgent, captured: &str) {
    if captured.is_empty() {
        let _ = agent.write_to_pty(b"\n");
        return;
    }

    let mut bytes = captured.as_bytes().to_vec();
    bytes.push(b'\n');
    let _ = agent.write_to_pty(&bytes);
}

fn reset_warp_input(agent: &mut InteractiveAgent) {
    let _ = clear_input_buffer(agent);
    agent.warp_cursor = 0;
    agent.history_index = None;
    agent.warp_passthrough = false;
}

fn insert_char_at_cursor(agent: &mut InteractiveAgent, c: char) {
    let cursor = agent.warp_cursor;
    let Some(new_cursor) = with_input_buffer(agent, |buf| {
        let pos = cursor.min(buf.len());
        buf.insert(pos, c);
        pos + c.len_utf8()
    }) else {
        return;
    };

    agent.warp_cursor = new_cursor;
    agent.history_index = None;
}

fn delete_before_cursor(agent: &mut InteractiveAgent) {
    let cursor = agent.warp_cursor;
    if cursor == 0 {
        return;
    }

    let Some(new_cursor) = with_input_buffer(agent, |buf| {
        let clamped = cursor.min(buf.len());
        let prev = buf[..clamped]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        buf.remove(prev);
        prev
    }) else {
        return;
    };

    agent.warp_cursor = new_cursor;
}

fn delete_at_cursor(agent: &mut InteractiveAgent) {
    let cursor = agent.warp_cursor;
    let _ = with_input_buffer(agent, |buf| {
        let clamped = cursor.min(buf.len());
        if clamped < buf.len() {
            buf.remove(clamped);
        }
    });
}

fn move_cursor_left(agent: &mut InteractiveAgent) {
    let cursor = agent.warp_cursor;
    if cursor == 0 {
        return;
    }

    let Some(new_pos) = read_input_buffer(agent, |buf| {
        let clamped = cursor.min(buf.len());
        buf[..clamped]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }) else {
        return;
    };

    agent.warp_cursor = new_pos;
}

fn move_cursor_right(agent: &mut InteractiveAgent) {
    let cursor = agent.warp_cursor;
    let Some(new_pos) = read_input_buffer(agent, |buf| {
        let clamped = cursor.min(buf.len());
        if clamped >= buf.len() {
            return clamped;
        }

        buf[clamped..]
            .char_indices()
            .nth(1)
            .map(|(offset, _)| clamped + offset)
            .unwrap_or(buf.len())
    }) else {
        return;
    };

    agent.warp_cursor = new_pos;
}

fn move_cursor_to_end(agent: &mut InteractiveAgent) {
    agent.warp_cursor = buffer_len(agent);
}

fn clear_before_cursor(agent: &mut InteractiveAgent) {
    let cursor = agent.warp_cursor;
    let Some(()) = with_input_buffer(agent, |buf| {
        let clamped = cursor.min(buf.len());
        buf.drain(..clamped);
    }) else {
        return;
    };

    agent.warp_cursor = 0;
}

fn clear_after_cursor(agent: &mut InteractiveAgent) {
    let cursor = agent.warp_cursor;
    let _ = with_input_buffer(agent, |buf| {
        let clamped = cursor.min(buf.len());
        buf.truncate(clamped);
    });
}

fn scroll_up(agent: &mut InteractiveAgent, amount: usize) {
    let max = agent.max_scroll();
    agent.scroll_offset = (agent.scroll_offset + amount).min(max);
}

fn scroll_down(agent: &mut InteractiveAgent, amount: usize) {
    agent.scroll_offset = agent.scroll_offset.saturating_sub(amount);
}

fn read_input_buffer<T>(agent: &InteractiveAgent, f: impl FnOnce(&String) -> T) -> Option<T> {
    let Ok(buf) = agent.input_buffer.lock() else {
        return None;
    };
    Some(f(&buf))
}

fn with_input_buffer<T>(agent: &InteractiveAgent, f: impl FnOnce(&mut String) -> T) -> Option<T> {
    let Ok(mut buf) = agent.input_buffer.lock() else {
        return None;
    };
    Some(f(&mut buf))
}

fn input_text(agent: &InteractiveAgent) -> String {
    read_input_buffer(agent, |buf| buf.to_string()).unwrap_or_default()
}

fn trimmed_input_text(agent: &InteractiveAgent) -> String {
    read_input_buffer(agent, |buf| buf.trim().to_string()).unwrap_or_default()
}

fn input_is_blank(agent: &InteractiveAgent) -> bool {
    read_input_buffer(agent, |buf| buf.trim().is_empty()).unwrap_or(true)
}

fn buffer_len(agent: &InteractiveAgent) -> usize {
    read_input_buffer(agent, |buf| buf.len()).unwrap_or(0)
}

fn replace_input_buffer(agent: &InteractiveAgent, text: &str) -> Option<()> {
    with_input_buffer(agent, |buf| {
        buf.clear();
        buf.push_str(text);
    })
}

fn clear_input_buffer(agent: &InteractiveAgent) -> Option<()> {
    with_input_buffer(agent, |buf| buf.clear())
}

fn is_cd_command(input: &str) -> bool {
    input == "cd" || input.starts_with("cd ") || input.starts_with("cd\t")
}

fn is_cd_picker_request(input: &str) -> bool {
    input.is_empty() || is_cd_command(input)
}

fn handle_cd_command(app: &mut App, idx: usize, trimmed: &str) -> bool {
    if !is_cd_command(trimmed) {
        return false;
    }

    let target = if trimmed == "cd" {
        dirs::home_dir()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string())
    } else {
        trimmed[2..].trim().to_string()
    };

    let current_dir = app.terminal_agents[idx].working_dir.clone();
    let Some(abs_path) = resolve_cd_path(&current_dir, &target) else {
        return true;
    };

    app.terminal_agents[idx].update_working_dir(&abs_path.to_string_lossy());
    true
}

/// Record a terminal command to the session history.
pub fn record_terminal_command(app: &mut App, idx: usize, captured: &str) {
    if captured.is_empty() {
        return;
    }

    let trimmed = captured.trim();
    if handle_cd_command(app, idx, trimmed) {
        return;
    }

    let session_name = app.terminal_agents[idx].name.clone();
    let cwd = app.terminal_agents[idx].working_dir.clone();
    let hist = app
        .terminal_histories
        .entry(session_name.clone())
        .or_default();
    hist.record(captured, &cwd);
    crate::tui::terminal_history::save_history(&app.data_dir, &session_name, hist);
    crate::tui::terminal_history::record_global_catalog(&app.data_dir, captured, &cwd);
}

/// Open the suggestion picker for a terminal agent.
pub fn open_terminal_suggestion_picker(app: &mut App, idx: usize) -> Result<()> {
    let (blocked, input, cwd) = {
        let agent = &app.terminal_agents[idx];
        (
            should_skip_warp_sync(agent),
            input_text(agent),
            agent.working_dir.clone(),
        )
    };
    if blocked {
        return Ok(());
    }

    if is_cd_command(input.trim()) {
        let partial = if input.len() > 2 {
            input[3..].trim()
        } else {
            ""
        };
        let global = crate::tui::terminal_history::load_all_histories(&app.data_dir);
        app.suggestion_picker = Some(crate::tui::terminal_history::SuggestionPicker::for_cd(
            partial, &cwd, &global,
        ));
        return Ok(());
    }

    app.suggestion_picker = Some(crate::tui::terminal_history::from_global_catalog(
        &input,
        &app.data_dir,
        &cwd,
    ));
    Ok(())
}

// ── Dialog: new agent creation ──────────────────────────────────────
