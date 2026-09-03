//! CT3: live-tail dialog renderer — a centered overlay that follows a
//! running check node's streamed output.
//!
//! The acceptance criterion is diagnostic: with the dialog open, a hung
//! node must *look* hung. A `Running` node shows a pulsing `● LIVE` badge
//! with elapsed time and frozen output; a finished node shows a final
//! `■` banner (`passed` / `failed` / `timed out`). Zero-output hangs show
//! an explicit "no output yet" line rather than a blank pane, so silence
//! itself is visible.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::domain::loops::LoopRunStatus;
use crate::tui::app::dialog::NodeTailDialog;
use crate::tui::ui::theme::Theme;

/// Overlay geometry: 80% of the width, 80% of the height, centered.
fn tail_centered(area: Rect) -> Rect {
    let w = (area.width * 80 / 100).max(20).min(area.width);
    let h = (area.height * 80 / 100).max(8).min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

/// Status banner line for the dialog header: what the node's state *looks
/// like* right now. Pure over the dialog snapshot so tests can assert the
/// hung-vs-working distinction without a backend.
pub(crate) fn tail_status_line(
    dialog: &NodeTailDialog,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let elapsed = now
        .signed_duration_since(dialog.started_at)
        .num_seconds()
        .max(0);
    match dialog.status {
        LoopRunStatus::Running => {
            let total = dialog.stdout_lines.len() + dialog.stderr_lines.len();
            if total == 0 {
                format!("● LIVE — running {elapsed}s, no output yet (silent: working or hung)")
            } else {
                format!("● LIVE — running {elapsed}s, {total} lines (following)")
            }
        }
        LoopRunStatus::Pass => "■ finished: passed — Esc/t to close".to_string(),
        LoopRunStatus::Fail if dialog.timed_out => {
            "■ finished: TIMED OUT — output below ends at the kill point".to_string()
        }
        LoopRunStatus::Fail => "■ finished: failed — Esc/t to close".to_string(),
    }
}

/// Body lines for the dialog, capped at `max_body` content rows (the
/// on-screen bound for chatty nodes). Applies `dialog.scroll` (lines held
/// back from the live edge) and always keeps the tail — the newest output.
/// Pure: the cap test feeds 5000 lines and asserts the bound.
pub(crate) fn tail_visible_lines(dialog: &NodeTailDialog, max_body: usize) -> Vec<String> {
    if max_body == 0 {
        return Vec::new();
    }
    let mut all: Vec<String> = Vec::new();
    if !dialog.stdout_lines.is_empty() {
        all.extend(dialog.stdout_lines.iter().cloned());
    }
    if !dialog.stderr_lines.is_empty() {
        if !all.is_empty() {
            all.push("── stderr ──".to_string());
        }
        all.extend(dialog.stderr_lines.iter().cloned());
    }
    if all.is_empty() {
        return Vec::new();
    }
    // Clamp scroll here so over-scrolling back never blanks the pane: at
    // least one line always stays visible no matter how far `scroll` ran.
    let end = all
        .len()
        .saturating_sub(dialog.scroll.min(all.len().saturating_sub(1)));
    let start = end.saturating_sub(max_body);
    all[start..end].to_vec()
}

pub(crate) fn draw_node_tail_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &NodeTailDialog,
    theme: &Theme,
    now: chrono::DateTime<chrono::Utc>,
) {
    let area = tail_centered(area);
    if area.width < 10 || area.height < 6 {
        return;
    }
    frame.render_widget(Clear, area);
    let title = format!(" Tail: {} ", dialog.node_name);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_color))
        .style(Style::default().bg(theme.dialog_bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let status = tail_status_line(dialog, now);
    let status_style = match dialog.status {
        LoopRunStatus::Running => Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD),
        LoopRunStatus::Pass => Style::default().fg(theme.header_color),
        LoopRunStatus::Fail => Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD),
    };
    // Header (status) + footer (hints) reserve 3 rows; the rest is body.
    let max_body = inner.height.saturating_sub(3) as usize;
    let body = tail_visible_lines(dialog, max_body);

    let mut lines = vec![Line::from(Span::styled(status, status_style))];
    if body.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no output captured yet)",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        for line in body {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme.text_primary),
            )));
        }
    }
    let footer = if dialog.scroll > 0 {
        format!(
            " ▲ scrolled {} lines up — ↓ to follow live · t/Esc: close ",
            dialog.scroll
        )
    } else {
        " t/Esc: close · ↑: scroll back ".to_string()
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(theme.dim_text),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_dialog(status: LoopRunStatus, stdout: usize, stderr: usize) -> NodeTailDialog {
        NodeTailDialog {
            run_id: "run1".to_string(),
            node_id: "node1".to_string(),
            node_name: "Node node1".to_string(),
            stdout_lines: (0..stdout).map(|i| format!("out {i}")).collect(),
            stderr_lines: (0..stderr).map(|i| format!("err {i}")).collect(),
            status,
            timed_out: false,
            started_at: Utc::now() - chrono::Duration::seconds(12),
            scroll: 0,
        }
    }

    #[test]
    fn running_with_no_output_looks_hung_not_blank() {
        let dialog = test_dialog(LoopRunStatus::Running, 0, 0);
        let status = tail_status_line(&dialog, Utc::now());
        assert!(status.contains("LIVE"), "status: {status}");
        assert!(status.contains("no output yet"), "status: {status}");
        // And the body renders the explicit empty marker, never a blank pane.
        let body = tail_visible_lines(&dialog, 20);
        assert!(body.is_empty());
    }

    #[test]
    fn running_with_output_shows_live_following() {
        let dialog = test_dialog(LoopRunStatus::Running, 3, 0);
        let status = tail_status_line(&dialog, Utc::now());
        assert!(status.contains("LIVE"), "status: {status}");
        assert!(status.contains("following"), "status: {status}");
    }

    #[test]
    fn timed_out_banner_names_the_kill_point() {
        let mut dialog = test_dialog(LoopRunStatus::Fail, 2, 0);
        dialog.timed_out = true;
        let status = tail_status_line(&dialog, Utc::now());
        assert!(status.contains("TIMED OUT"), "status: {status}");
        assert!(status.contains("kill point"), "status: {status}");
    }

    #[test]
    fn failed_without_timeout_says_failed() {
        let dialog = test_dialog(LoopRunStatus::Fail, 2, 0);
        let status = tail_status_line(&dialog, Utc::now());
        assert!(status.contains("failed"), "status: {status}");
        assert!(!status.contains("TIMED OUT"), "status: {status}");
    }

    #[test]
    fn passed_banner_says_passed() {
        let dialog = test_dialog(LoopRunStatus::Pass, 2, 0);
        let status = tail_status_line(&dialog, Utc::now());
        assert!(status.contains("passed"), "status: {status}");
    }

    #[test]
    fn tail_renders_lines_capped() {
        // 5000 lines of mock output must be bounded on screen: the dialog
        // keeps the newest `max_body` rows, never the whole flood.
        let dialog = test_dialog(LoopRunStatus::Running, 4000, 1000);
        let body = tail_visible_lines(&dialog, 20);
        assert_eq!(body.len(), 20);
        assert_eq!(body[19], "err 999");
    }

    #[test]
    fn tail_without_cap_returns_everything_in_order() {
        let dialog = test_dialog(LoopRunStatus::Running, 2, 1);
        let body = tail_visible_lines(&dialog, 100);
        assert_eq!(body, vec!["out 0", "out 1", "── stderr ──", "err 0"]);
    }

    #[test]
    fn scroll_holds_back_from_live_edge() {
        let mut dialog = test_dialog(LoopRunStatus::Running, 10, 0);
        dialog.scroll = 4;
        let body = tail_visible_lines(&dialog, 100);
        assert_eq!(body.len(), 6);
        assert_eq!(body[5], "out 5");
    }
}
