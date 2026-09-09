//! The live loop view: a read-only render of [`LoopLiveState`] for the
//! main panel — header, spec queue, current-spec graph (auto-following the
//! engine's current node, or a manually-highlighted one), and a detail
//! footer. Pure over the snapshot; the only I/O is the `App` glue in
//! [`draw_loop_live_view`], which assembles plain values before handing off
//! to [`render_loop_live_view`].

use std::collections::HashMap;

use chrono::{DateTime, Local, Utc};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::super::theme::Theme;
use super::{compact_cwd, truncate_str};
use crate::domain::loops::{
    LoopEdgeCondition, LoopNode, LoopNodeKind, LoopRunStatus, LoopSpecStatus, LoopStatus,
};
use crate::tui::app::loop_live_state::{
    EnsembleLiveInfo, LoopLiveState, NodeRunInfo, SpecQueueEntry,
};
use crate::tui::app::types::App;
use crate::tui::ui::sidebar::draw_scroll_indicators;

pub(crate) fn draw_loop_live_view(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        app.loop_spec_strip_click_map.clear();
        return;
    }
    let Some(state) = app.loop_live_state.as_ref() else {
        app.loop_spec_strip_click_map.clear();
        frame.render_widget(
            Paragraph::new("No loop selected").style(Style::default().fg(theme.dim_text)),
            area,
        );
        return;
    };

    let blocked = app
        .loop_sidebar_meta
        .get(&state.loop_id)
        .is_some_and(|meta| meta.blocked);
    let highlighted = app.loop_graph_highlighted_node_id().map(str::to_string);
    let node_info = app.loop_graph_highlighted_node_run_info();
    let selected_spec_id = app.loop_spec_strip_selected.clone();
    let spec_scroll = app.loop_spec_strip_scroll;

    let result = render_loop_live_view(
        frame,
        area,
        &LiveViewContext {
            state,
            follow: app.loop_graph_follow,
            follow_anchor: app.loop_graph_follow_anchor.as_deref(),
            highlighted_node_id: highlighted.as_deref(),
            node_info: &node_info,
            blocked,
            now: Utc::now(),
            theme,
            selected_spec_id: selected_spec_id.as_deref(),
            spec_scroll,
            scroll: app.loop_live_view_scroll,
        },
    );

    app.loop_spec_strip_click_map = result.click_map;
    app.loop_spec_strip_capacity = result.capacity;
    app.loop_live_view_total_lines = result.total_lines;
    app.loop_live_view_scroll = result.clamped_scroll;
    app.loop_graph_follow_anchor = result.follow_anchor;

    // CT3: live-tail overlay renders last so it sits above the graph and
    // detail content. Viewer only — no input state is touched here.
    if let Some(dialog) = app.node_tail_dialog.as_ref() {
        super::super::dialogs::draw_node_tail_dialog(frame, area, dialog, theme, Utc::now());
    }
}

/// Everything the pure renderer needs, gathered by [`draw_loop_live_view`]
/// so the render itself stays a plain function of already-computed values
/// (no `Database`/`App` access), which is what makes it testable without a
/// full `App`.
struct LiveViewContext<'a> {
    state: &'a LoopLiveState,
    follow: bool,
    /// CT8: the node id the graph last re-centred on while auto-following
    /// (mirrors `App::loop_graph_follow_anchor`). Compared against
    /// `highlighted_node_id` — which equals the engine's current node id
    /// whenever `follow` is true — to decide whether this frame is a real
    /// node transition that should re-centre the scroll.
    follow_anchor: Option<&'a str>,
    highlighted_node_id: Option<&'a str>,
    node_info: &'a NodeRunInfo,
    blocked: bool,
    now: DateTime<Utc>,
    theme: &'a Theme,
    /// Spec id manually selected in the marker strip, if any — independent
    /// of `highlighted_node_id`/`follow`, which are the *graph's* own
    /// follow/manual state.
    selected_spec_id: Option<&'a str>,
    /// First visible index into `state.spec_queue` for the marker strip.
    spec_scroll: usize,
    scroll: u16,
}

/// What [`render_loop_live_view`] hands back to its `App`-owning caller:
/// where the marker strip's chips actually landed on screen, for mouse
/// hit-testing next frame (mirrors `sidebar_tab_click_map`'s shape).
struct LiveViewRenderResult {
    click_map: Vec<(String, u16, u16, u16)>,
    capacity: usize,
    total_lines: u16,
    clamped_scroll: u16,
    /// CT8: the anchor to persist for next frame. In auto-follow this is the
    /// current node id (adopted whether or not we re-centred this frame); in
    /// manual it is carried through unchanged.
    follow_anchor: Option<String>,
}

fn render_loop_live_view(
    frame: &mut Frame,
    area: Rect,
    ctx: &LiveViewContext,
) -> LiveViewRenderResult {
    let state = ctx.state;
    // CT8: the top row is a persistent mode indicator that never scrolls with
    // the graph content. Everything below runs on `area` minus that row.
    let indicator_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    frame.render_widget(mode_indicator(ctx.follow, ctx.theme), indicator_area);
    let mut lines = header_lines(state, ctx.blocked, ctx.theme);
    lines.push(Line::from(""));

    let chip_row = area.y + lines.len() as u16;
    let strip = spec_strip_layout(
        state,
        ctx.selected_spec_id,
        ctx.spec_scroll,
        area.width,
        area.height,
        ctx.theme,
    );
    lines.extend(strip.lines);

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Graph",
        Style::default().fg(ctx.theme.dim_text),
    )));
    let graph_start_line = lines.len() as u16;
    let graph_result = if state.effective_nodes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no nodes yet)",
            Style::default().fg(ctx.theme.dim_text),
        )));
        None
    } else {
        let result = graph_lines(
            state,
            ctx.highlighted_node_id,
            ctx.follow,
            area.width,
            ctx.theme,
        );
        let offset = result.highlighted_offset;
        lines.extend(result.lines);
        Some(offset)
    };
    lines.push(Line::from(""));
    lines.extend(footer_lines(
        state,
        ctx.highlighted_node_id,
        ctx.node_info,
        ctx.follow,
        ctx.now,
        ctx.theme,
    ));

    let total_lines = lines.len() as u16;
    let max_scroll = total_lines.saturating_sub(area.height);
    let clamped_from_input = ctx.scroll.min(max_scroll);

    // CT8: who owns the scroll.
    //
    // Auto-follow: re-derive the scroll from the highlighted node ONLY on the
    // frame where the current node's identity changed since the last
    // re-centre (`ctx.follow_anchor`). While `follow` is true,
    // `highlighted_node_id` == the engine's current node id, so this fires on
    // a real node-to-node transition and on nothing else — not a status
    // change, not elapsed time, not the user scrolling. Every other redraw
    // keeps `clamped_from_input`, the user's own scroll, so the whole graph
    // of a running loop can be read.
    //
    // Manual navigation: the scroll always chases the selection with the same
    // margin `ensure_visible` uses, so the highlight can never leave the panel.
    let (scroll, next_follow_anchor) = if ctx.follow {
        let node_changed = ctx.highlighted_node_id != ctx.follow_anchor;
        let scroll = if node_changed {
            match graph_result.flatten() {
                Some(offset) => ensure_visible(
                    graph_start_line + offset,
                    3,
                    clamped_from_input,
                    area.height,
                ),
                None => clamped_from_input,
            }
        } else {
            clamped_from_input
        };
        (scroll, ctx.highlighted_node_id.map(str::to_string))
    } else {
        let scroll = match graph_result.flatten() {
            Some(offset) => ensure_visible(
                graph_start_line + offset,
                3,
                clamped_from_input,
                area.height,
            ),
            None => clamped_from_input,
        };
        (scroll, ctx.follow_anchor.map(str::to_string))
    };
    let clamped_scroll = scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((clamped_scroll, 0)),
        area,
    );

    let has_up = clamped_scroll > 0;
    let has_down = total_lines > area.height && clamped_scroll < max_scroll;
    if has_up || has_down {
        draw_scroll_indicators(frame, area, has_up, has_down, ctx.theme);
    }

    LiveViewRenderResult {
        click_map: strip
            .click_map
            .into_iter()
            .map(|(spec_id, row_offset, col_start, col_end)| {
                (
                    spec_id,
                    chip_row + row_offset,
                    area.x + col_start,
                    area.x + col_end,
                )
            })
            .collect(),
        capacity: strip.capacity,
        total_lines,
        clamped_scroll,
        follow_anchor: next_follow_anchor,
    }
}

fn ensure_visible(start: u16, span: u16, scroll: u16, height: u16) -> u16 {
    let end = start.saturating_add(span);
    let view_end = scroll.saturating_add(height);
    if height == 0 {
        return scroll;
    }
    if start < scroll {
        start
    } else if end > view_end {
        end.saturating_sub(height)
    } else {
        scroll
    }
}

/// CT8: the always-visible badge naming the current scroll mode. Rendered on
/// a reserved top row so it never scrolls away with the graph. In manual mode
/// it also names the key (`Esc`) that returns to auto-follow. Both modes are
/// labelled outright — the reader never has to infer the mode from behaviour.
/// The colour fills the whole row (widget `style`, not just the span) so the
/// badge reads as a solid bar at a glance, matching the interactive preview's
/// focus state.
fn mode_indicator(follow: bool, theme: &Theme) -> Paragraph<'static> {
    let (text, bg) = if follow {
        (
            " \u{25cf} AUTO-FOLLOW \u{2014} view tracks the running node ",
            theme.status_running,
        )
    } else {
        (
            " \u{2016} MANUAL \u{2014} press Esc to return to auto-follow ",
            theme.warning,
        )
    };
    let style = Style::default()
        .fg(theme.accent_fg)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    Paragraph::new(Line::from(Span::styled(text.to_string(), style))).style(style)
}

fn status_icon_and_label(
    state: &LoopLiveState,
    blocked: bool,
    theme: &Theme,
) -> (&'static str, String, Color) {
    if let Some(at) = state.autorun_at {
        let local = at.with_timezone(&Local);
        return (
            "⏰",
            format!("autorun {}", local.format("%H:%M")),
            Color::Cyan,
        );
    }
    if blocked {
        return ("⛔", "blocked".to_string(), theme.status_fail);
    }
    match state.loop_status {
        LoopStatus::Running => ("▶", "running".to_string(), theme.status_running),
        LoopStatus::Pausing => ("⏸", "pausing".to_string(), Color::Yellow),
        LoopStatus::Paused => ("⏸", "paused".to_string(), Color::Yellow),
        LoopStatus::Completed => ("✓", "completed".to_string(), theme.status_ok),
        LoopStatus::Failed => ("✗", "failed".to_string(), theme.status_fail),
        LoopStatus::Draft => ("·", "draft".to_string(), theme.dim_text),
    }
}

fn header_lines(state: &LoopLiveState, blocked: bool, theme: &Theme) -> Vec<Line<'static>> {
    let (icon, label, color) = status_icon_and_label(state, blocked, theme);
    vec![
        Line::from(vec![
            Span::styled(icon, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(
                state.loop_name.clone(),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(label, Style::default().fg(color)),
            Span::raw("   "),
            Span::styled(
                format!("{}/{} specs", state.done_count, state.total_count),
                Style::default().fg(theme.dim_text),
            ),
        ]),
        Line::from(Span::styled(
            format!("Workdir: {}", compact_cwd(&state.workdir)),
            Style::default().fg(theme.dim_text),
        )),
    ]
}

/// Chip glyph for a queue entry: the current spec always shows `▶`
/// regardless of its underlying status (running or the next pending one —
/// see `assemble_loop_live_state`'s `current_spec_id` rule); otherwise the
/// glyph reflects the terminal status directly, since that's exactly the
/// case a user goes looking for (which spec failed vs. was skipped).
fn spec_chip(
    entry: &SpecQueueEntry,
    current_spec_id: Option<&str>,
    theme: &Theme,
) -> (&'static str, Color) {
    if Some(entry.spec_id.as_str()) == current_spec_id {
        return ("▶", theme.status_running);
    }
    match entry.status {
        LoopSpecStatus::Pending => ("○", theme.dim_text),
        LoopSpecStatus::Running => ("▶", theme.status_running),
        LoopSpecStatus::Completed => ("✓", theme.status_ok),
        LoopSpecStatus::Failed => ("✗", theme.status_fail),
        LoopSpecStatus::Skipped => ("⊘", theme.status_disabled),
        LoopSpecStatus::Interrupted => ("⚑", theme.status_interrupted),
    }
}

/// A marker chip's fixed on-screen footprint: a 3-column core (bracketed
/// when selected, plain otherwise) plus a 1-column gap, so the strip's
/// column arithmetic never has to special-case which chip is selected.
const SPEC_CHIP_WIDTH: u16 = 4;

/// Columns reserved for the "(a-b of N)" suffix when the strip has to
/// truncate — wide enough for three-digit spec counts on either side.
const SPEC_STRIP_RANGE_SUFFIX_WIDTH: u16 = 14;

/// The marker strip's rendered lines, its click map (spec id + row offset +
/// column span, relative to the strip's own origin — the caller translates
/// to absolute screen coordinates), and how many chips fit in the given
/// width.
struct SpecStripLayout {
    lines: Vec<Line<'static>>,
    click_map: Vec<(String, u16, u16, u16)>,
    capacity: usize,
    #[allow(dead_code)]
    chip_line_count: usize,
}

/// Lay out the spec marker strip: status chips wrapped across multiple lines
/// to fill available height; compressed to a scrollable window with a
/// "(a-b of N)" indicator only when even multi-line wrapping cannot fit all
/// specs — followed by the selected spec's detail, falling back to the
/// running/next-pending spec when nothing is manually selected.
fn spec_strip_layout(
    state: &LoopLiveState,
    selected_spec_id: Option<&str>,
    scroll: usize,
    area_width: u16,
    area_height: u16,
    theme: &Theme,
) -> SpecStripLayout {
    if state.spec_queue.is_empty() {
        return SpecStripLayout {
            lines: vec![Line::from(Span::styled(
                "Queue: (empty)",
                Style::default().fg(theme.dim_text),
            ))],
            click_map: Vec::new(),
            capacity: 0,
            chip_line_count: 1,
        };
    }

    let total = state.spec_queue.len();
    let chips_per_line = (area_width / SPEC_CHIP_WIDTH).max(1) as usize;
    let lines_needed = total.div_ceil(chips_per_line);
    const RESERVED_NON_CHIP_LINES: u16 = 8;
    let max_chip_lines = area_height.saturating_sub(RESERVED_NON_CHIP_LINES).max(1) as usize;
    let chip_lines = lines_needed.min(max_chip_lines);
    let truncated = total > chip_lines * chips_per_line;
    let capacity = if truncated {
        // Round up: reserving 3 whole chips (12 cols) for a 14-col suffix
        // leaves the "(a-b of N)" text spilling past the line and soft-wrapping
        // onto an extra visual row. Reserve the ceiling so it fits.
        let last_line_reserved = SPEC_STRIP_RANGE_SUFFIX_WIDTH
            .div_ceil(SPEC_CHIP_WIDTH)
            .max(1) as usize;
        chip_lines.saturating_sub(1) * chips_per_line
            + chips_per_line.saturating_sub(last_line_reserved)
    } else {
        chip_lines * chips_per_line
    };
    let capacity = capacity.max(1);
    let start = scroll.min(total.saturating_sub(capacity));
    let end = (start + capacity).min(total);

    let mut chip_lines_vec: Vec<Line<'static>> = Vec::new();
    let mut click_map: Vec<(String, u16, u16, u16)> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut col: u16 = 0;
    let mut row_offset: u16 = 0;

    for entry in &state.spec_queue[start..end] {
        if col > 0 && col + SPEC_CHIP_WIDTH > area_width {
            chip_lines_vec.push(Line::from(std::mem::take(&mut current_spans)));
            row_offset += 1;
            col = 0;
        }
        let (icon, color) = spec_chip(entry, state.current_spec_id.as_deref(), theme);
        let selected = Some(entry.spec_id.as_str()) == selected_spec_id;
        let core = if selected {
            format!("[{icon}]")
        } else {
            format!(" {icon} ")
        };
        let style = if selected {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        let core_width = core.chars().count() as u16;
        click_map.push((entry.spec_id.clone(), row_offset, col, col + core_width));
        current_spans.push(Span::styled(core, style));
        current_spans.push(Span::raw(" "));
        col += SPEC_CHIP_WIDTH;
    }
    if truncated {
        current_spans.push(Span::styled(
            format!(" ({}-{} of {})", start + 1, end, total),
            Style::default().fg(theme.dim_text),
        ));
    }
    if !current_spans.is_empty() || chip_lines_vec.is_empty() {
        chip_lines_vec.push(Line::from(current_spans));
    }

    let chip_line_count = chip_lines_vec.len();
    let mut lines = chip_lines_vec;
    lines.extend(spec_detail_lines(state, selected_spec_id, theme));

    SpecStripLayout {
        lines,
        click_map,
        capacity,
        chip_line_count,
    }
}

/// Name, status, and (for a failed/skipped spec) the recorded reason for
/// whichever spec the marker strip is showing: the manual selection if
/// there is one, else the running/next-pending spec — the same fallback
/// the strip's detail line always showed before selection existed.
fn spec_detail_lines(
    state: &LoopLiveState,
    selected_spec_id: Option<&str>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let entry = selected_spec_id
        .and_then(|id| state.spec_queue.iter().find(|e| e.spec_id == id))
        .or_else(|| {
            state
                .spec_queue
                .iter()
                .find(|e| Some(e.spec_id.as_str()) == state.current_spec_id.as_deref())
        });
    let Some(entry) = entry else {
        return Vec::new();
    };

    let (_, color) = spec_chip(entry, state.current_spec_id.as_deref(), theme);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            entry.spec_name.clone(),
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", spec_status_label(entry.status)),
            Style::default().fg(color),
        ),
    ])];
    if let Some(reason) = entry.failure_reason.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("  {reason}"),
            Style::default().fg(theme.dim_text),
        )));
    }
    lines
}

fn spec_status_label(status: LoopSpecStatus) -> &'static str {
    match status {
        LoopSpecStatus::Pending => "pending",
        LoopSpecStatus::Running => "running",
        LoopSpecStatus::Completed => "completed",
        LoopSpecStatus::Failed => "failed",
        LoopSpecStatus::Skipped => "skipped",
        LoopSpecStatus::Interrupted => "interrupted",
    }
}

/// Border/text style and marker glyph for a node box — bold accent with a
/// solid marker while auto-following (the "pulsing current node" cue),
/// plain accent with a `›` marker for a manually-picked node, dim/plain
/// otherwise.
fn node_style(is_highlighted: bool, follow: bool, theme: &Theme) -> (Style, Style, &'static str) {
    if is_highlighted && follow {
        (
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
            "●",
        )
    } else if is_highlighted {
        (
            Style::default().fg(theme.header_color),
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
            "›",
        )
    } else {
        (
            Style::default().fg(theme.border_color),
            Style::default().fg(theme.text_primary),
            " ",
        )
    }
}

fn depth_prefix(depth: usize) -> String {
    "   ".repeat(depth.min(3))
}

fn edge_condition_priority(condition: &LoopEdgeCondition) -> (u8, Option<String>) {
    match condition {
        LoopEdgeCondition::Pass => (0, None),
        LoopEdgeCondition::Fail => (1, None),
        LoopEdgeCondition::Always => (2, None),
        LoopEdgeCondition::Route(label) => (3, Some(label.clone())),
        LoopEdgeCondition::Break => (4, None),
    }
}

fn node_box_lines(
    node: &LoopNode,
    is_highlighted: bool,
    follow: bool,
    inner: usize,
    theme: &Theme,
    depth: usize,
) -> Vec<Line<'static>> {
    let prefix = depth_prefix(depth);
    let (border_style, text_style, marker) = node_style(is_highlighted, follow, theme);
    let kind_tag = format!("[{}]", node.kind.display_str());
    let max_name = inner.saturating_sub(2 + kind_tag.len());
    let name_display = truncate_str(&node.name, max_name);
    let spaces = inner.saturating_sub(2 + name_display.len() + kind_tag.len());
    let kind_tag_style = if node.kind == LoopNodeKind::Router {
        Style::default()
            .fg(theme.kind_router)
            .add_modifier(Modifier::BOLD)
    } else {
        text_style
    };

    vec![
        Line::from(Span::styled(
            format!("{prefix}┌{}┐", "─".repeat(inner)),
            border_style,
        )),
        Line::from(vec![
            Span::styled(
                format!("{prefix}│{marker} {name_display}{}", " ".repeat(spaces)),
                text_style,
            ),
            Span::styled(kind_tag, kind_tag_style),
            Span::styled("│", text_style),
        ]),
        Line::from(Span::styled(
            format!("{prefix}└{}┘", "─".repeat(inner)),
            border_style,
        )),
    ]
}

/// Collapsed box for an ensemble (F1) — folds its N member nodes plus the
/// join into ONE box ("name [N models]") with a live status tag per member,
/// instead of drawing N+1 separate boxes and a fan-out of near-identical
/// edges. Expand-on-inspect isn't a separate view: the member/join ids are
/// still real entries in `state.effective_nodes`, so navigating directly to
/// one (e.g. via a future picker) still resolves correctly — this box is
/// purely a collapsed *rendering*, not a different graph.
fn ensemble_box_lines(
    ensemble: &EnsembleLiveInfo,
    is_highlighted: bool,
    follow: bool,
    inner: usize,
    theme: &Theme,
    depth: usize,
) -> Vec<Line<'static>> {
    let prefix = depth_prefix(depth);
    let (border_style, text_style, marker) = node_style(is_highlighted, follow, theme);
    let title = format!("{} [{} models]", ensemble.name, ensemble.members.len());
    let max_title = inner.saturating_sub(2);
    let title_display = truncate_str(&title, max_title);
    let title_spaces = inner.saturating_sub(2 + title_display.chars().count());

    let mut lines = vec![
        Line::from(Span::styled(
            format!("{prefix}┌{}┐", "─".repeat(inner)),
            border_style,
        )),
        Line::from(Span::styled(
            format!(
                "{prefix}│{} {}{}│",
                marker,
                title_display,
                " ".repeat(title_spaces)
            ),
            text_style,
        )),
    ];
    for member in &ensemble.members {
        let (tag, color) = ensemble_member_status_tag(member.status, theme);
        let label = format!("{} {tag}", member.label);
        let max_label = inner.saturating_sub(4);
        let label_display = truncate_str(&label, max_label);
        let spaces = inner.saturating_sub(4 + label_display.chars().count());
        lines.push(Line::from(Span::styled(
            format!("{prefix}│  {}{}│", label_display, " ".repeat(spaces)),
            Style::default().fg(color),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!("{prefix}└{}┘", "─".repeat(inner)),
        border_style,
    )));
    lines
}

fn ensemble_member_status_tag(
    status: Option<LoopRunStatus>,
    theme: &Theme,
) -> (&'static str, Color) {
    match status {
        Some(LoopRunStatus::Pass) => ("[pass]", theme.status_ok),
        Some(LoopRunStatus::Fail) => ("[fail]", theme.status_fail),
        Some(LoopRunStatus::Interrupted) => ("[interrupted]", theme.status_fail),
        Some(LoopRunStatus::Running) => ("[running]", theme.status_running),
        None => ("[pending]", theme.dim_text),
    }
}

/// Node boxes in DFS tree order starting from the entry node (the node
/// with no incoming edges, or position 0 if every node has one). Each node's
/// children appear immediately after its edge lines, indented one level
/// deeper. Cycles render as a back-reference line with `↩` instead of a
/// second box. An ensemble (F1) renders as one collapsed box at the depth
/// of its first member; other members and the join are skipped.
struct GraphLinesResult<'a> {
    lines: Vec<Line<'a>>,
    highlighted_offset: Option<u16>,
}

fn graph_lines(
    state: &LoopLiveState,
    highlighted_node_id: Option<&str>,
    follow: bool,
    area_width: u16,
    theme: &Theme,
) -> GraphLinesResult<'static> {
    let box_width = (area_width as usize).saturating_sub(4).clamp(22, 48);
    let inner = box_width.saturating_sub(2);

    let join_node_ids: std::collections::HashSet<&str> = state
        .ensembles
        .iter()
        .map(|e| e.join_node_id.as_str())
        .collect();
    let ensemble_by_member: HashMap<&str, &EnsembleLiveInfo> = state
        .ensembles
        .iter()
        .flat_map(|e| e.members.iter().map(move |m| (m.node_id.as_str(), e)))
        .collect();
    let ensemble_by_join: HashMap<&str, &EnsembleLiveInfo> = state
        .ensembles
        .iter()
        .map(|e| (e.join_node_id.as_str(), e))
        .collect();

    // --- Collapsed node map (key -> renderable) ---
    enum Collapsed<'a> {
        Node(&'a LoopNode),
        Ensemble(&'a EnsembleLiveInfo),
    }
    // Position for ordering fallback entry selection.
    let mut collapsed_nodes: HashMap<String, Collapsed> = HashMap::new();
    let mut collapsed_pos: HashMap<String, i64> = HashMap::new();
    let mut seen_ens: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for node in &state.effective_nodes {
        if join_node_ids.contains(node.id.as_str()) {
            continue;
        }
        if let Some(ens) = ensemble_by_member.get(node.id.as_str()) {
            if seen_ens.insert(ens.ensemble_id.as_str()) {
                // position is the first member's position
                collapsed_nodes.insert(ens.ensemble_id.clone(), Collapsed::Ensemble(ens));
                collapsed_pos.insert(ens.ensemble_id.clone(), node.position);
            }
        } else {
            collapsed_nodes.insert(node.id.clone(), Collapsed::Node(node));
            collapsed_pos.insert(node.id.clone(), node.position);
        }
    }
    if collapsed_nodes.is_empty() {
        return GraphLinesResult {
            lines: Vec::new(),
            highlighted_offset: None,
        };
    }

    // --- Collapsed edges (deduped, sorted later per DFS) ---
    let mut collapsed_edges: HashMap<String, Vec<(String, LoopEdgeCondition)>> = HashMap::new();
    for edge in &state.effective_edges {
        let from_raw = edge.from_node.as_str();
        let to_raw = edge.to_node.as_str();
        // internal ensemble member -> join edge
        if let Some(ens) = ensemble_by_member.get(from_raw) {
            if ens.join_node_id.as_str() == to_raw {
                continue;
            }
        }
        let from_key = if let Some(ens) = ensemble_by_join.get(from_raw) {
            Some(ens.ensemble_id.clone())
        } else if let Some(ens) = ensemble_by_member.get(from_raw) {
            Some(ens.ensemble_id.clone())
        } else if collapsed_nodes.contains_key(from_raw) {
            Some(from_raw.to_string())
        } else {
            None
        };
        let to_key = if let Some(ens) = ensemble_by_join.get(to_raw) {
            Some(ens.ensemble_id.clone())
        } else if let Some(ens) = ensemble_by_member.get(to_raw) {
            Some(ens.ensemble_id.clone())
        } else if collapsed_nodes.contains_key(to_raw) {
            Some(to_raw.to_string())
        } else {
            None
        };
        if let (Some(fk), Some(tk)) = (from_key, to_key) {
            if fk == tk {
                // self-loop: keep as edge but DFS will treat it as back-ref
            }
            let entry = collapsed_edges.entry(fk).or_default();
            if !entry
                .iter()
                .any(|(ek, ec)| ek == &tk && ec == &edge.condition)
            {
                entry.push((tk, edge.condition.clone()));
            }
        }
    }
    // Sort each adjacency by pass > fail > always > route(alpha) > break
    for edges in collapsed_edges.values_mut() {
        edges.sort_by(|a, b| {
            let (pa, la) = edge_condition_priority(&a.1);
            let (pb, lb) = edge_condition_priority(&b.1);
            pa.cmp(&pb).then_with(|| la.cmp(&lb))
        });
    }

    // --- Find entry (node with no incoming collapsed edge) ---
    let mut incoming: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for tos in collapsed_edges.values() {
        for (tk, _) in tos {
            incoming.insert(tk.as_str());
        }
    }
    let entry_key = collapsed_nodes
        .keys()
        .find(|k| !incoming.contains(k.as_str()))
        .cloned()
        .or_else(|| {
            // fallback to smallest position
            collapsed_nodes
                .keys()
                .min_by_key(|k| collapsed_pos.get(k.as_str()).copied().unwrap_or(i64::MAX))
                .cloned()
        })
        .unwrap();

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut highlighted_offset: Option<u16> = None;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::needless_pass_by_value)]
    fn dfs_visit(
        key: &str,
        depth: usize,
        collapsed_nodes: &HashMap<String, Collapsed>,
        collapsed_edges: &HashMap<String, Vec<(String, LoopEdgeCondition)>>,
        state: &LoopLiveState,
        follow: bool,
        inner: usize,
        theme: &Theme,
        lines: &mut Vec<Line<'static>>,
        highlighted_offset: &mut Option<u16>,
        visited: &mut std::collections::HashSet<String>,
        highlighted_node_id: Option<&str>,
    ) {
        if visited.contains(key) {
            return;
        }
        visited.insert(key.to_string());
        let depth_clamped = depth.min(3);
        let is_hl = if let Some(Collapsed::Ensemble(ens)) = collapsed_nodes.get(key) {
            ens.members
                .iter()
                .any(|m| Some(m.node_id.as_str()) == highlighted_node_id)
                || highlighted_node_id == Some(ens.join_node_id.as_str())
        } else {
            highlighted_node_id == Some(key)
        };
        if is_hl && highlighted_offset.is_none() {
            *highlighted_offset = Some(lines.len() as u16);
        }
        match collapsed_nodes.get(key) {
            Some(Collapsed::Node(node)) => {
                lines.extend(node_box_lines(
                    node,
                    is_hl,
                    follow,
                    inner,
                    theme,
                    depth_clamped,
                ));
            }
            Some(Collapsed::Ensemble(ens)) => {
                lines.extend(ensemble_box_lines(
                    ens,
                    is_hl,
                    follow,
                    inner,
                    theme,
                    depth_clamped,
                ));
            }
            None => return,
        }
        if let Some(edges) = collapsed_edges.get(key) {
            let prefix = depth_prefix(depth_clamped);
            // Determine taken route for router nodes (only regular nodes)
            let taken_route = if let Some(Collapsed::Node(node)) = collapsed_nodes.get(key) {
                state
                    .router_taken_routes
                    .get(node.id.as_str())
                    .map(String::as_str)
            } else {
                None
            };
            for (i, (to_key, cond)) in edges.iter().enumerate() {
                let branch = if i == edges.len() - 1 { "└" } else { "├" };
                let is_back = visited.contains(to_key.as_str());
                let target_label = match collapsed_nodes.get(to_key.as_str()) {
                    Some(Collapsed::Ensemble(ens)) => {
                        format!("{} [{} models]", ens.name, ens.members.len())
                    }
                    Some(Collapsed::Node(n)) => n.name.clone(),
                    None => to_key.clone(),
                };
                let suffix = if is_back { "  ↩" } else { "" };
                match cond.route_label() {
                    Some(route) => {
                        let taken = taken_route == Some(route);
                        let (marker, style) = if taken {
                            (
                                "✓",
                                Style::default()
                                    .fg(theme.status_ok)
                                    .add_modifier(Modifier::BOLD),
                            )
                        } else {
                            (" ", Style::default().fg(theme.dim_text))
                        };
                        lines.push(Line::from(Span::styled(
                            format!(
                                "{prefix}   {branch}─{marker} {route} → {target_label}{suffix}"
                            ),
                            style,
                        )));
                    }
                    None => {
                        let label = format!("{target_label}{suffix}");
                        lines.push(Line::from(Span::styled(
                            format!("{prefix}   {}─ {} → {}", branch, cond.as_str(), label),
                            Style::default().fg(theme.dim_text),
                        )));
                    }
                }
            }
            lines.push(Line::from(""));
            // Recurse into unvisited children in priority order
            for (to_key, _) in edges {
                if !visited.contains(to_key.as_str()) {
                    dfs_visit(
                        to_key,
                        depth + 1,
                        collapsed_nodes,
                        collapsed_edges,
                        state,
                        follow,
                        inner,
                        theme,
                        lines,
                        highlighted_offset,
                        visited,
                        highlighted_node_id,
                    );
                }
            }
        } else {
            // leaf node still needs spacing before next sibling at same depth
            // Only add blank if not last overall? DFS already pushes blanks per node with edges;
            // for leaves, add a blank line to separate siblings visually unless at end.
            // We'll add a blank line for consistency with old rendering.
            lines.push(Line::from(""));
        }
    }

    // Start DFS from entry
    dfs_visit(
        &entry_key,
        0,
        &collapsed_nodes,
        &collapsed_edges,
        state,
        follow,
        inner,
        theme,
        &mut lines,
        &mut highlighted_offset,
        &mut visited,
        highlighted_node_id,
    );
    // Visit any disconnected components not reached from entry
    let mut remaining: Vec<String> = collapsed_nodes
        .keys()
        .filter(|k| !visited.contains(k.as_str()))
        .cloned()
        .collect();
    remaining.sort_by_key(|k| collapsed_pos.get(k.as_str()).copied().unwrap_or(i64::MAX));
    for rk in &remaining {
        dfs_visit(
            rk,
            0,
            &collapsed_nodes,
            &collapsed_edges,
            state,
            follow,
            inner,
            theme,
            &mut lines,
            &mut highlighted_offset,
            &mut visited,
            highlighted_node_id,
        );
    }
    // Trim trailing blank lines
    while lines.last().is_some_and(|l| l.width() == 0) {
        lines.pop();
    }
    GraphLinesResult {
        lines,
        highlighted_offset,
    }
}

fn format_elapsed(started_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - started_at).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn run_status_span(status: Option<LoopRunStatus>, theme: &Theme) -> Span<'static> {
    match status {
        Some(LoopRunStatus::Running) => {
            Span::styled("running", Style::default().fg(theme.status_running))
        }
        Some(LoopRunStatus::Pass) => Span::styled("pass", Style::default().fg(theme.status_ok)),
        Some(LoopRunStatus::Fail) => Span::styled("fail", Style::default().fg(theme.status_fail)),
        Some(LoopRunStatus::Interrupted) => {
            Span::styled("interrupted", Style::default().fg(theme.status_fail))
        }
        None => Span::styled("(no runs yet)", Style::default().fg(theme.dim_text)),
    }
}

fn footer_lines(
    state: &LoopLiveState,
    highlighted_node_id: Option<&str>,
    node_info: &NodeRunInfo,
    follow: bool,
    now: DateTime<Utc>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some(node_id) = highlighted_node_id else {
        return vec![Line::from(Span::styled(
            "(no node selected)",
            Style::default().fg(theme.dim_text),
        ))];
    };
    let Some(node) = state.effective_nodes.iter().find(|n| n.id == node_id) else {
        return vec![Line::from(Span::styled(
            "(node not found)",
            Style::default().fg(theme.dim_text),
        ))];
    };

    let mode_label = if follow { "auto-follow" } else { "manual" };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            node.name.clone(),
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", node.kind.display_str()),
            Style::default().fg(theme.dim_text),
        ),
        Span::raw("  "),
        Span::styled(
            format!("({mode_label})"),
            Style::default().fg(theme.dim_text),
        ),
    ])];

    let mut meta = vec![run_status_span(node_info.status, theme)];
    if let Some(started_at) = node_info.started_at {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("elapsed {}", format_elapsed(started_at, now)),
            Style::default().fg(theme.dim_text),
        ));
    }
    if let Some(iteration) = node_info.iteration {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("iter {iteration}"),
            Style::default().fg(theme.dim_text),
        ));
    }
    if let Some(route) = node_info.chosen_route.as_deref() {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("route → {route}"),
            Style::default()
                .fg(theme.status_ok)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(meta));

    if let Some(tail) = node_info.output_tail.as_deref() {
        lines.push(Line::from(""));
        for line in tail.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(theme.text_primary),
            )));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::loops::{
        LoopEdge, LoopNode, LoopNodeKind, LoopSpecStatus, LoopStatus as DomainLoopStatus,
    };
    use crate::tui::app::types::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;
    use std::sync::Arc;

    fn render_to_text(width: u16, height: u16, draw: impl FnOnce(&mut Frame, Rect)) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn team_nodes() -> Vec<LoopNode> {
        let kinds = [
            ("Implement", LoopNodeKind::Agent),
            ("Cargo gates", LoopNodeKind::Check),
            ("Review + commit", LoopNodeKind::Agent),
            ("Check committed", LoopNodeKind::Check),
            ("Resilience", LoopNodeKind::Agent),
        ];
        kinds
            .iter()
            .enumerate()
            .map(|(i, (name, kind))| LoopNode {
                id: format!("n{i}"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: name.to_string(),
                kind: *kind,
                config: json!({}),
                position: i as i64,
                created_at: Utc::now(),
            })
            .collect()
    }

    fn team_edges() -> Vec<LoopEdge> {
        // Mirrors the real 5-node team graph: cycles back to Implement (n0)
        // on failure at any later stage, and Resilience (n4) loops back to
        // Implement on its own pass.
        vec![
            ("n0", "n1", LoopEdgeCondition::Pass),
            ("n0", "n4", LoopEdgeCondition::Fail),
            ("n4", "n0", LoopEdgeCondition::Pass),
            ("n1", "n2", LoopEdgeCondition::Pass),
            ("n1", "n0", LoopEdgeCondition::Fail),
            ("n2", "n3", LoopEdgeCondition::Pass),
            ("n2", "n0", LoopEdgeCondition::Fail),
            ("n3", "n2", LoopEdgeCondition::Fail),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (from, to, condition))| LoopEdge {
            id: format!("e{i}"),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        })
        .collect()
    }

    fn running_state() -> LoopLiveState {
        LoopLiveState {
            loop_id: "lp1".to_string(),
            loop_name: "canopy-ux-notifications".to_string(),
            loop_status: DomainLoopStatus::Running,
            workdir: "/home/user/Projects/harness-canopy".to_string(),
            trigger_type: "manual".to_string(),
            schedule_expr: None,
            watch_path: None,
            autorun_at: None,
            spec_queue: vec![
                SpecQueueEntry {
                    spec_id: "s1".to_string(),
                    spec_name: "B1 fix".to_string(),
                    status: LoopSpecStatus::Completed,
                    failure_reason: None,
                },
                SpecQueueEntry {
                    spec_id: "s2".to_string(),
                    spec_name: "U1b live view".to_string(),
                    status: LoopSpecStatus::Running,
                    failure_reason: None,
                },
                SpecQueueEntry {
                    spec_id: "s3".to_string(),
                    spec_name: "T1 theme".to_string(),
                    status: LoopSpecStatus::Pending,
                    failure_reason: None,
                },
            ],
            done_count: 1,
            total_count: 3,
            current_spec_id: Some("s2".to_string()),
            effective_nodes: team_nodes(),
            effective_edges: team_edges(),
            ensembles: Vec::new(),
            router_taken_routes: HashMap::new(),
            current_node_id: Some("n0".to_string()),
            current_node_status: Some(LoopRunStatus::Running),
            current_node_started_at: Some(Utc::now() - chrono::Duration::seconds(75)),
            current_node_iteration: Some(2),
            current_node_output_tail: Some("implementing the graph render...".to_string()),
        }
    }

    #[test]
    fn running_loop_renders_header_queue_graph_and_footer_with_current_node_highlighted() {
        let state = running_state();
        let node_info = NodeRunInfo {
            status: state.current_node_status,
            started_at: state.current_node_started_at,
            iteration: state.current_node_iteration,
            output_tail: state.current_node_output_tail.clone(),
            chosen_route: None,
        };

        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        // Header.
        assert!(text.contains("canopy-ux-notifications"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("1/3 specs"), "{text}");
        assert!(text.contains("harness-canopy"), "{text}");
        // Queue.
        assert!(text.contains("U1b live view"), "{text}");
        // Graph — all five team nodes present.
        for name in [
            "Implement",
            "Cargo gates",
            "Review + commit",
            "Check committed",
            "Resilience",
        ] {
            assert!(text.contains(name), "missing node {name} in:\n{text}");
        }
        assert!(text.contains("pass →"), "{text}");
        assert!(text.contains("fail →"), "{text}");
        // The auto-followed current node (Implement/n0) uses the solid
        // follow marker.
        assert!(text.contains("●"), "expected follow marker in:\n{text}");
        // Footer.
        assert!(text.contains("iter 2"), "{text}");
        assert!(text.contains("elapsed"), "{text}");
        assert!(text.contains("implementing the graph render"), "{text}");
    }

    #[test]
    fn manual_selection_switches_footer_to_the_picked_node() {
        let state = running_state();

        let node_info = NodeRunInfo {
            status: Some(LoopRunStatus::Pass),
            started_at: Some(Utc::now() - chrono::Duration::seconds(10)),
            iteration: Some(1),
            output_tail: Some("reviewed and committed".to_string()),
            chosen_route: None,
        };
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    follow_anchor: None,
                    highlighted_node_id: Some("n2"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        assert!(text.contains("manual"), "{text}");
        assert!(text.contains("reviewed and committed"), "{text}");
        // Manual pick uses the `›` marker, not the follow `●`.
        assert!(text.contains('›'), "expected manual marker in:\n{text}");
    }

    #[test]
    fn spec_strip_shows_distinct_markers_and_selected_detail_with_failure_reason() {
        let mut state = running_state();
        // s1 stays Completed (✓); make s3 Failed with a recorded reason and
        // add a fourth, Skipped spec — before this, Failed/Skipped/Completed
        // all rendered the same `✓`.
        state.spec_queue[2].status = LoopSpecStatus::Failed;
        state.spec_queue[2].failure_reason = Some("cargo test failed: 3 tests failing".to_string());
        state.spec_queue.push(SpecQueueEntry {
            spec_id: "s4".to_string(),
            spec_name: "T2 skipped thing".to_string(),
            status: LoopSpecStatus::Skipped,
            failure_reason: Some("superseded by s5".to_string()),
        });

        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: None,
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: Some("s3"),
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        // Distinct glyphs for completed (s1), failed (s3), and skipped (s4).
        assert!(text.contains('✓'), "{text}");
        assert!(text.contains('✗'), "{text}");
        assert!(text.contains('⊘'), "{text}");
        // The selected marker (s3, failed) is bracketed, distinct from an
        // unselected chip and from the graph's `●` follow marker.
        assert!(
            text.contains("[✗]"),
            "expected bracketed selection marker in:\n{text}"
        );
        // Selecting a spec shows its name, status, and the recorded failure
        // reason — not just the running spec's name shown pre-selection.
        assert!(text.contains("T1 theme"), "{text}");
        assert!(text.contains("[failed]"), "{text}");
        assert!(
            text.contains("cargo test failed: 3 tests failing"),
            "{text}"
        );
    }

    #[test]
    fn spec_strip_wraps_across_multiple_lines_when_many_specs_fit() {
        let mut state = running_state();
        state.spec_queue = (0..20)
            .map(|i| SpecQueueEntry {
                spec_id: format!("s{i}"),
                spec_name: format!("Spec {i}"),
                status: LoopSpecStatus::Pending,
                failure_reason: None,
            })
            .collect();
        state.current_spec_id = None;
        state.total_count = 20;

        let node_info = NodeRunInfo::default();
        let text = render_to_text(40, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: None,
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        assert!(
            !text.contains("of 20"),
            "with tall area all 20 chips should wrap without truncation, but got:\n{text}"
        );
        // All 20 markers present — each ○ occupies a chip, so at least 20 appear.
        assert!(
            text.matches('○').count() >= 20,
            "expected all 20 spec glyphs visible across wrapped lines in:\n{text}"
        );
    }

    #[test]
    fn spec_strip_wraps_21_specs_across_rows_and_click_map_tracks_row() {
        let mut state = running_state();
        state.spec_queue = (0..21)
            .map(|i| SpecQueueEntry {
                spec_id: format!("s{i}"),
                spec_name: format!("Spec {i}"),
                status: LoopSpecStatus::Pending,
                failure_reason: None,
            })
            .collect();
        state.current_spec_id = None;
        state.total_count = 21;

        let theme = Theme::classic();
        // Direct layout check with narrow but tall area: 40 wide -> 10 per line, tall enough for all.
        let layout = spec_strip_layout(&state, None, 0, 40, 40, &theme);
        assert_eq!(
            layout.click_map.len(),
            21,
            "all 21 chips must be present when height allows"
        );
        assert_eq!(
            layout.chip_line_count, 3,
            "21 chips at 10/line needs 3 lines"
        );
        // First 10 on row 0, next 10 on row 1, last on row 2.
        for i in 0..10 {
            assert_eq!(layout.click_map[i].1, 0, "spec s{i} should be on row 0");
        }
        for i in 10..20 {
            assert_eq!(layout.click_map[i].1, 1, "spec s{i} should be on row 1");
        }
        assert_eq!(layout.click_map[20].1, 2);

        // Clicking spec at index 15 (row 1) returns correct spec id.
        let (row, col) = (layout.click_map[15].1, layout.click_map[15].2);
        let hit = layout
            .click_map
            .iter()
            .find(|(_, r, c0, c1)| *r == row && *c0 <= col && col < *c1)
            .map(|(id, _, _, _)| id.as_str());
        assert_eq!(hit, Some("s15"));

        // Also visible in rendered text with no range indicator.
        let node_info = NodeRunInfo::default();
        let text = render_to_text(40, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: None,
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            !text.contains("of 21"),
            "all chips visible so no range suffix:\n{text}"
        );
        assert!(
            text.matches('○').count() >= 21,
            "expected 21 glyphs in:\n{text}"
        );
    }

    #[test]
    fn spec_strip_falls_back_to_scroll_when_height_is_tight() {
        let mut state = running_state();
        state.spec_queue = (0..21)
            .map(|i| SpecQueueEntry {
                spec_id: format!("s{i}"),
                spec_name: format!("Spec {i}"),
                status: LoopSpecStatus::Pending,
                failure_reason: None,
            })
            .collect();
        state.current_spec_id = None;
        state.total_count = 21;

        let theme = Theme::classic();
        // Height 10 -> max_chip_lines = 2, so only 2 lines fit -> truncated.
        let layout = spec_strip_layout(&state, None, 0, 40, 10, &theme);
        assert!(
            layout.capacity < 21,
            "capacity {} should be < 21 when height is tight",
            layout.capacity
        );
        assert_eq!(layout.chip_line_count, 2);

        let node_info = NodeRunInfo::default();
        let text = render_to_text(40, 10, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: None,
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        // The suffix "(1-17 of 21)" may wrap across two buffer rows when the
        // area is narrow; normalize whitespace so the assertion is not fragile
        // to buffer padding while still requiring the indicator to be present.
        let flat = text.replace('\n', " ");
        let normalized = flat.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("of 21"),
            "expected range indicator when height is tight in:\n{text}"
        );
    }

    #[test]
    fn spec_strip_single_line_when_few_specs() {
        let state = running_state();
        // running_state has 3 specs.
        let theme = Theme::classic();
        let layout = spec_strip_layout(&state, None, 0, 80, 40, &theme);
        assert_eq!(layout.chip_line_count, 1);
        assert!(
            !layout.lines.iter().any(|l| l.to_string().contains("of ")),
            "no range indicator with few specs"
        );
        assert_eq!(layout.click_map.len(), 3);
        assert!(layout.click_map.iter().all(|(_, row, _, _)| *row == 0));

        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: None,
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(!text.contains("of "), "no range with few specs in:\n{text}");
    }

    #[test]
    fn spec_strip_click_map_matches_rendered_chip_positions() {
        let state = running_state();
        let node_info = NodeRunInfo::default();
        let backend = TestBackend::new(80, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut result_holder = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                result_holder = Some(render_loop_live_view(
                    frame,
                    area,
                    &LiveViewContext {
                        state: &state,
                        follow: true,
                        follow_anchor: None,
                        highlighted_node_id: None,
                        node_info: &node_info,
                        blocked: false,
                        now: Utc::now(),
                        theme: &Theme::classic(),
                        selected_spec_id: None,
                        spec_scroll: 0,
                        scroll: 0,
                    },
                ));
            })
            .unwrap();
        let result = result_holder.unwrap();

        assert_eq!(result.click_map.len(), 3);
        assert_eq!(result.capacity, (80 / SPEC_CHIP_WIDTH) as usize);
        let ids: Vec<&str> = result
            .click_map
            .iter()
            .map(|(id, _, _, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["s1", "s2", "s3"]);
        // All three chips render on the same row.
        let row = result.click_map[0].1;
        assert!(result.click_map.iter().all(|&(_, r, _, _)| r == row));
        // Columns are ordered and non-overlapping.
        assert!(result.click_map[0].2 < result.click_map[1].2);
        assert!(result.click_map[1].2 < result.click_map[2].2);
    }

    #[test]
    fn esc_restores_follow_via_app_state() {
        let (db, data_dir) = test_db_and_dir();
        seed_running_loop(&db);

        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        let auto_node = app.loop_graph_highlighted_node_id().map(str::to_string);
        assert!(app.loop_graph_follow);

        app.loop_graph_move_highlight(true);
        assert!(!app.loop_graph_follow);
        let manual_node = app.loop_graph_highlighted_node_id().map(str::to_string);
        assert_ne!(auto_node, manual_node);

        app.loop_graph_reset_follow();
        assert!(app.loop_graph_follow);
        assert_eq!(
            app.loop_graph_highlighted_node_id().map(str::to_string),
            auto_node
        );
    }

    #[test]
    fn narrow_width_render_does_not_panic() {
        let state = running_state();
        let node_info = NodeRunInfo::default();
        let ctx = LiveViewContext {
            state: &state,
            follow: true,
            follow_anchor: None,
            highlighted_node_id: None,
            node_info: &node_info,
            blocked: false,
            now: Utc::now(),
            theme: &Theme::classic(),
            selected_spec_id: None,
            spec_scroll: 0,
            scroll: 0,
        };
        render_to_text(1, 5, |frame, area| {
            render_loop_live_view(frame, area, &ctx);
        });
        render_to_text(0, 0, |frame, area| {
            render_loop_live_view(frame, area, &ctx);
        });
    }

    #[test]
    fn completed_loop_renders_statically_with_completed_icon() {
        let mut state = running_state();
        state.loop_status = DomainLoopStatus::Completed;
        state.current_node_status = Some(LoopRunStatus::Pass);
        state.done_count = 3;
        state.current_spec_id = None;

        let node_info = NodeRunInfo {
            status: state.current_node_status,
            started_at: state.current_node_started_at,
            iteration: state.current_node_iteration,
            output_tail: state.current_node_output_tail.clone(),
            chosen_route: None,
        };

        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        assert!(text.contains("completed"), "{text}");
        assert!(text.contains("3/3 specs"), "{text}");
    }

    fn ensemble_live_fixture() -> EnsembleLiveInfo {
        EnsembleLiveInfo {
            ensemble_id: "ens1".to_string(),
            name: "Proposers".to_string(),
            join_node_id: "join1".to_string(),
            members: vec![
                crate::tui::app::loop_live_state::EnsembleMemberLiveInfo {
                    node_id: "m1".to_string(),
                    label: "openrouter/deepseek".to_string(),
                    status: Some(LoopRunStatus::Pass),
                },
                crate::tui::app::loop_live_state::EnsembleMemberLiveInfo {
                    node_id: "m2".to_string(),
                    label: "openrouter/qwen".to_string(),
                    status: Some(LoopRunStatus::Running),
                },
                crate::tui::app::loop_live_state::EnsembleMemberLiveInfo {
                    node_id: "m3".to_string(),
                    label: "openrouter/llama".to_string(),
                    status: None,
                },
            ],
        }
    }

    #[test]
    fn ensemble_renders_as_one_collapsed_box_with_per_member_status() {
        let mut state = running_state();
        // kickoff -> {m1, m2, m3} -> join -> arbiter, replacing the plain
        // team graph so the ensemble is the only thing on screen.
        state.effective_nodes = vec![
            LoopNode {
                id: "kickoff".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Kickoff".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Proposers [1]".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Proposers [2]".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 2,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "m3".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Proposers [3]".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 3,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Proposers (quorum)".to_string(),
                kind: LoopNodeKind::Join,
                config: json!({}),
                position: 4,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "arbiter".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Arbiter".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 5,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![
            ("kickoff", "m1", LoopEdgeCondition::Always),
            ("kickoff", "m2", LoopEdgeCondition::Always),
            ("kickoff", "m3", LoopEdgeCondition::Always),
            ("m1", "join1", LoopEdgeCondition::Always),
            ("m2", "join1", LoopEdgeCondition::Always),
            ("m3", "join1", LoopEdgeCondition::Always),
            ("join1", "arbiter", LoopEdgeCondition::Pass),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (from, to, condition))| LoopEdge {
            id: format!("ee{i}"),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        })
        .collect();
        state.ensembles = vec![ensemble_live_fixture()];
        state.current_node_id = Some("kickoff".to_string());

        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        // Collapsed to one box with the member count, not three separate
        // member boxes or a fourth box for the join.
        assert!(text.contains("Proposers [3 models]"), "{text}");
        assert!(!text.contains("Proposers [1]"), "{text}");
        assert!(!text.contains("Proposers [2]"), "{text}");
        assert!(!text.contains("Proposers (quorum)"), "{text}");
        // Per-member live status inside the collapsed box.
        assert!(text.contains("[pass]"), "{text}");
        assert!(text.contains("[running]"), "{text}");
        assert!(text.contains("[pending]"), "{text}");
        // The fan-out from kickoff collapses to one edge, and the join's own
        // routing to the arbiter still renders.
        assert!(text.contains("Kickoff"), "{text}");
        assert!(text.contains("Arbiter"), "{text}");
    }

    #[test]
    fn router_node_renders_distinctly_with_every_route_legible_at_real_width() {
        let mut state = running_state();
        state.effective_nodes = vec![
            LoopNode {
                id: "router".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Classify request".to_string(),
                kind: LoopNodeKind::Router,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "billing".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Billing specialist".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "technical".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Technical specialist".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 2,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "sales".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Sales specialist".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 3,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "escalation".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Human escalation".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 4,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![
            (
                "router",
                "billing",
                LoopEdgeCondition::Route("billing".to_string()),
            ),
            (
                "router",
                "technical",
                LoopEdgeCondition::Route("technical".to_string()),
            ),
            (
                "router",
                "sales",
                LoopEdgeCondition::Route("sales".to_string()),
            ),
            (
                "router",
                "escalation",
                LoopEdgeCondition::Route("escalation".to_string()),
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (from, to, condition))| LoopEdge {
            id: format!("re{i}"),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        })
        .collect();
        state.router_taken_routes = [("router".to_string(), "technical".to_string())]
            .into_iter()
            .collect();
        state.current_node_id = Some("router".to_string());

        let node_info = NodeRunInfo {
            chosen_route: Some("technical".to_string()),
            ..NodeRunInfo::default()
        };
        // A "real" panel width — wider than the box's own clamp — so a
        // router with 4 routes has plenty of room; nothing here should ever
        // need to wrap or truncate.
        let text = render_to_text(100, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });

        // Router carries its own kind tag alongside the agent boxes it
        // routes to.
        assert!(text.contains("[router]"), "{text}");
        assert!(text.contains("[agent]"), "{text}");
        // Every one of the 4 declared routes is fully legible: its label,
        // arrow, and target name all appear intact — none truncated with
        // "…" or split by an unwanted wrap.
        for (route, target) in [
            ("billing", "Billing specialist"),
            ("technical", "Technical specialist"),
            ("sales", "Sales specialist"),
            ("escalation", "Human escalation"),
        ] {
            let expected = format!("{route} → {target}");
            assert!(text.contains(&expected), "missing {expected:?} in:\n{text}");
        }
        // The route the completed run actually took is marked distinctly
        // from the other three.
        assert!(
            text.contains("✓ technical → Technical specialist"),
            "{text}"
        );
        assert!(text.contains("route → technical"), "{text}");
    }

    fn test_db_and_dir() -> (Arc<Database>, tempfile::TempDir) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        (db, data_dir)
    }

    fn seed_running_loop(db: &Database) {
        use crate::domain::loops::{Loop, LoopNodeRun, LoopSpec};

        let lp = Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: "lp1".to_string(),
            name: "team loop".to_string(),
            description: None,
            workdir: "/tmp/test".to_string(),
            status: DomainLoopStatus::Running,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            hooks: std::collections::BTreeMap::new(),
        };
        db.insert_loop(&lp).unwrap();

        db.insert_loop_spec(&LoopSpec {
            id: "s1".to_string(),
            loop_id: Some("lp1".to_string()),
            name: "spec one".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Running,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();

        for node in team_nodes() {
            db.insert_loop_node(&LoopNode {
                spec_id: Some("s1".to_string()),
                ..node
            })
            .unwrap();
        }
        for edge in team_edges() {
            db.insert_loop_edge(&LoopEdge {
                spec_id: Some("s1".to_string()),
                ..edge
            })
            .unwrap();
        }

        db.insert_loop_run(&LoopNodeRun {
            id: "run1".to_string(),
            loop_id: "lp1".to_string(),
            spec_id: "s1".to_string(),
            node_id: "n0".to_string(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();
    }

    fn many_nodes(count: usize) -> Vec<LoopNode> {
        (0..count)
            .map(|i| LoopNode {
                id: format!("m{i}"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: format!("Node {i}"),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: i as i64,
                created_at: Utc::now(),
            })
            .collect()
    }

    fn many_edges(count: usize) -> Vec<LoopEdge> {
        (0..count.saturating_sub(1))
            .map(|i| LoopEdge {
                id: format!("me{i}"),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: format!("m{i}"),
                to_node: format!("m{}", i + 1),
                condition: LoopEdgeCondition::Pass,
            })
            .collect()
    }

    #[test]
    fn graph_taller_than_panel_is_clipped_with_indicators() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(12);
        state.effective_edges = many_edges(12);
        state.current_node_id = Some("m0".to_string());

        let node_info = NodeRunInfo::default();

        // At top, only ▼ should show.
        let text_top = render_to_text(80, 15, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    follow_anchor: None,
                    highlighted_node_id: Some("m0"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            !text_top.contains('▲'),
            "no ▲ when at top, but got:\n{text_top}"
        );
        assert!(
            text_top.contains('▼'),
            "expected ▼ when content overflows below at top:\n{text_top}"
        );

        // Scrolled mid-way, both indicators.
        let text_mid = render_to_text(80, 15, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    follow_anchor: None,
                    highlighted_node_id: Some("m0"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 10,
                },
            );
        });
        assert!(
            text_mid.contains('▲'),
            "expected ▲ when scrolled down:\n{text_mid}"
        );
        assert!(
            text_mid.contains('▼'),
            "expected ▼ when not at bottom:\n{text_mid}"
        );
    }

    #[test]
    fn graph_that_fits_has_no_indicators() {
        let state = running_state();
        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 60, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(!text.contains('▲'), "no ▲ when graph fits:\n{text}");
        assert!(!text.contains('▼'), "no ▼ when graph fits:\n{text}");
    }

    #[test]
    fn scroll_clamped_to_valid_range() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(12);
        state.effective_edges = many_edges(12);
        let node_info = NodeRunInfo::default();
        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut clamped = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                let result = render_loop_live_view(
                    frame,
                    area,
                    &LiveViewContext {
                        state: &state,
                        follow: false,
                        follow_anchor: None,
                        highlighted_node_id: None,
                        node_info: &node_info,
                        blocked: false,
                        now: Utc::now(),
                        theme: &Theme::classic(),
                        selected_spec_id: None,
                        spec_scroll: 0,
                        scroll: 1000,
                    },
                );
                clamped = Some((result.clamped_scroll, result.total_lines));
            })
            .unwrap();
        let (clamped_scroll, total_lines) = clamped.unwrap();
        // The indicator row consumes 1 row of the 15-row viewport, so the
        // renderer's internal max_scroll is total_lines - 14.
        let max = total_lines.saturating_sub(14);
        assert_eq!(
            clamped_scroll, max,
            "scroll must clamp to total_lines - height ({max}), got {clamped_scroll} with total {total_lines}"
        );
    }

    #[test]
    fn auto_follow_keeps_highlighted_node_visible() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(15);
        state.effective_edges = many_edges(15);
        state.current_node_id = Some("m14".to_string());
        let node_info = NodeRunInfo {
            status: Some(LoopRunStatus::Running),
            ..NodeRunInfo::default()
        };
        // Render with scroll=0 but follow=true — the highlighted last node
        // must be auto-scrolled into the 15-row viewport.
        let text = render_to_text(80, 15, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: Some("m14"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            text.contains("Node 14"),
            "auto-follow must keep highlighted node visible, missing Node 14 in:\n{text}"
        );
    }
    #[test]
    fn dfs_layout_renders_children_immediately_after_edges() {
        // A pass-> B pass-> C : DFS order should be A,B,C with B box immediately after A's edges
        let mut state = running_state();
        state.effective_nodes = vec![
            LoopNode {
                id: "a".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "A".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "b".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "B".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "c".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "C".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 2,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![
            LoopEdge {
                id: "e0".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "a".to_string(),
                to_node: "b".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
            LoopEdge {
                id: "e1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "b".to_string(),
                to_node: "c".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        state.ensembles = Vec::new();
        state.current_node_id = Some("a".to_string());
        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        // Ensure B appears after A and C after B
        // For robustness, just check order of node names in rendered text
        let ai = text.find("A").unwrap();
        let bi = text.find("B").unwrap();
        let ci = text.find("C").unwrap();
        assert!(
            ai < bi,
            "A should appear before B in DFS layout, got ai={ai} bi={bi} text:\n{text}"
        );
        assert!(
            bi < ci,
            "B should appear before C in DFS layout, got bi={bi} ci={ci} text:\n{text}"
        );
        // Also ensure B's box appears directly after A's edge lines (no other box between)
        let pass_a = text.find("pass → B").unwrap();
        assert!(
            pass_a < bi,
            "B box should appear after its incoming edge, pass_a={pass_a} bi={bi}"
        );
    }

    #[test]
    fn dfs_layout_shows_back_reference_for_cycles() {
        let mut state = running_state();
        state.effective_nodes = vec![
            LoopNode {
                id: "a".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "A".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "b".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "B".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![
            LoopEdge {
                id: "e0".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "a".to_string(),
                to_node: "b".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
            LoopEdge {
                id: "e1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "b".to_string(),
                to_node: "a".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        state.ensembles = Vec::new();
        state.current_node_id = Some("a".to_string());
        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            text.contains("↩"),
            "cycle back-edge should render with ↩ marker, got:\n{text}"
        );
        // A's box should appear only once (the back-reference is a line, not a box)
        let box_count = text.matches("┌").count();
        assert_eq!(box_count, 2, "expected 2 boxes (A and B), back-reference should not add a third, got {box_count} in:\n{text}");
    }

    #[test]
    fn dfs_layout_pass_before_fail() {
        let mut state = running_state();
        state.effective_nodes = vec![
            LoopNode {
                id: "a".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "A".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "b".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "B".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "c".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "C".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 2,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![
            LoopEdge {
                id: "e0".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "a".to_string(),
                to_node: "c".to_string(),
                condition: LoopEdgeCondition::Fail,
            },
            LoopEdge {
                id: "e1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "a".to_string(),
                to_node: "b".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        state.ensembles = Vec::new();
        state.current_node_id = Some("a".to_string());
        let node_info = NodeRunInfo::default();
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        let pass_idx = text.find("pass → B").expect("pass edge missing");
        let fail_idx = text.find("fail → C").expect("fail edge missing");
        assert!(
            pass_idx < fail_idx,
            "pass should appear before fail, pass={pass_idx} fail={fail_idx} text:\n{text}"
        );
    }

    #[test]
    fn graph_colors_come_from_theme() {
        // Drive the real render path (`graph_lines`), not a helper, so this
        // actually guards the requirement that the drawing routes colors
        // through `Theme`.
        let theme = Theme {
            status_ok: Color::Rgb(1, 2, 3),
            kind_router: Color::Rgb(4, 5, 6),
            ..Theme::classic()
        };
        let mut state = running_state();
        state.effective_nodes = vec![
            LoopNode {
                id: "r".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Router".to_string(),
                kind: LoopNodeKind::Router,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "t".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "Target".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![LoopEdge {
            id: "e0".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            from_node: "r".to_string(),
            to_node: "t".to_string(),
            condition: LoopEdgeCondition::Route("myroute".to_string()),
        }];
        state.ensembles = Vec::new();
        state.router_taken_routes = [("r".to_string(), "myroute".to_string())]
            .into_iter()
            .collect();

        let taken = graph_lines(&state, None, true, 80, &theme);
        let route_line = taken
            .lines
            .iter()
            .find(|l| l.to_string().contains("myroute → Target"))
            .expect("route edge line missing");
        assert_eq!(
            route_line.spans[0].style.fg,
            Some(Color::Rgb(1, 2, 3)),
            "taken route edge should use theme.status_ok"
        );
        assert!(
            taken.lines.iter().any(|line| line
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Rgb(4, 5, 6)))),
            "router box should use theme.kind_router"
        );

        // With no route recorded as taken, the branch falls back to dim_text.
        state.router_taken_routes = HashMap::new();
        let untaken = graph_lines(&state, None, true, 80, &theme);
        let route_line = untaken
            .lines
            .iter()
            .find(|l| l.to_string().contains("myroute → Target"))
            .expect("route edge line missing");
        assert_eq!(
            route_line.spans[0].style.fg,
            Some(theme.dim_text),
            "non-taken route edge should use theme.dim_text"
        );
    }

    #[test]
    fn back_reference_uses_theme_dim_text() {
        let mut state = running_state();
        state.effective_nodes = vec![
            LoopNode {
                id: "a".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "A".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 0,
                created_at: Utc::now(),
            },
            LoopNode {
                id: "b".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "B".to_string(),
                kind: LoopNodeKind::Agent,
                config: json!({}),
                position: 1,
                created_at: Utc::now(),
            },
        ];
        state.effective_edges = vec![
            LoopEdge {
                id: "e0".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "a".to_string(),
                to_node: "b".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
            LoopEdge {
                id: "e1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "b".to_string(),
                to_node: "a".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        state.ensembles = Vec::new();
        let theme = Theme::classic();
        // Verify graph_lines returns lines where the back-reference edge uses dim_text
        let result = graph_lines(&state, Some("a"), true, 80, &theme);
        let back_line = result
            .lines
            .iter()
            .find(|l| l.to_string().contains("↩"))
            .expect("back ref line missing");
        let fg = back_line.spans[0].style.fg;
        assert_eq!(
            fg,
            Some(theme.dim_text),
            "back-reference should use theme.dim_text, got {:?}",
            fg
        );
    }

    // ---- CT8: scroll ownership + mode indicator ---------------------------

    #[test]
    fn ct8_auto_follow_user_scroll_survives_when_current_node_unchanged() {
        // The measured bug: reading a big graph of a running loop, scrolling
        // down, and being yanked back to the running node every frame.
        let mut state = running_state();
        state.effective_nodes = many_nodes(30);
        state.effective_edges = many_edges(30);
        state.current_node_id = Some("m1".to_string());
        let node_info = NodeRunInfo::default();

        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        // Frame 1: anchor is None -> renderer adopts "m1" as the anchor.
        let mut anchor: Option<String> = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                let r = render_loop_live_view(
                    frame,
                    area,
                    &LiveViewContext {
                        state: &state,
                        follow: true,
                        follow_anchor: None,
                        highlighted_node_id: Some("m1"),
                        node_info: &node_info,
                        blocked: false,
                        now: Utc::now(),
                        theme: &Theme::classic(),
                        selected_spec_id: None,
                        spec_scroll: 0,
                        scroll: 0,
                    },
                );
                anchor = r.follow_anchor;
            })
            .unwrap();
        assert_eq!(
            anchor.as_deref(),
            Some("m1"),
            "renderer must adopt the current node as the anchor"
        );

        // Frame 2: user has scrolled to 10, current node is still "m1".
        let mut clamped = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                let r = render_loop_live_view(
                    frame,
                    area,
                    &LiveViewContext {
                        state: &state,
                        follow: true,
                        follow_anchor: anchor.as_deref(),
                        highlighted_node_id: Some("m1"),
                        node_info: &node_info,
                        blocked: false,
                        now: Utc::now(),
                        theme: &Theme::classic(),
                        selected_spec_id: None,
                        spec_scroll: 0,
                        scroll: 10,
                    },
                );
                clamped = Some(r.clamped_scroll);
            })
            .unwrap();
        assert_eq!(
            clamped,
            Some(10),
            "auto-follow must NOT override the user's scroll when the current node is unchanged"
        );
    }
    // Breaks if: the `if node_changed` guard is removed and auto-follow
    // re-centres every frame again.

    #[test]
    fn ct8_auto_follow_recentres_when_current_node_changes() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(30);
        state.effective_edges = many_edges(30);
        state.current_node_id = Some("m25".to_string());
        let node_info = NodeRunInfo::default();

        // User had scrolled far away (anchor still on "m0"); the engine's
        // current node has moved to "m25". The view must jump to it.
        let text = render_to_text(80, 20, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: Some("m0"),
                    highlighted_node_id: Some("m25"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            text.contains("Node 25"),
            "a real node transition must re-centre the view on the new current node:\n{text}"
        );
    }
    // Breaks if: auto-follow stops re-centring on node change (e.g. the
    // node-change branch always returns `clamped_from_input`).

    #[test]
    fn ct8_auto_follow_ignores_status_or_elapsed_change_for_same_node() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(30);
        state.effective_edges = many_edges(30);
        state.current_node_id = Some("m1".to_string());

        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        let running = NodeRunInfo {
            status: Some(LoopRunStatus::Running),
            started_at: Some(Utc::now() - chrono::Duration::seconds(5)),
            ..NodeRunInfo::default()
        };
        // Frame 1 adopts the anchor "m1".
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_loop_live_view(
                    frame,
                    area,
                    &LiveViewContext {
                        state: &state,
                        follow: true,
                        follow_anchor: None,
                        highlighted_node_id: Some("m1"),
                        node_info: &running,
                        blocked: false,
                        now: Utc::now(),
                        theme: &Theme::classic(),
                        selected_spec_id: None,
                        spec_scroll: 0,
                        scroll: 0,
                    },
                );
            })
            .unwrap();

        // Frame 2: same node "m1", user scrolled to 12, but the node's status
        // flipped to Pass and elapsed time advanced. Scroll must not move.
        let passed = NodeRunInfo {
            status: Some(LoopRunStatus::Pass),
            started_at: Some(Utc::now() - chrono::Duration::seconds(600)),
            ..NodeRunInfo::default()
        };
        let mut clamped = None;
        terminal
            .draw(|frame| {
                let area = frame.area();
                let r = render_loop_live_view(
                    frame,
                    area,
                    &LiveViewContext {
                        state: &state,
                        follow: true,
                        follow_anchor: Some("m1"),
                        highlighted_node_id: Some("m1"),
                        node_info: &passed,
                        blocked: false,
                        now: Utc::now(),
                        theme: &Theme::classic(),
                        selected_spec_id: None,
                        spec_scroll: 0,
                        scroll: 12,
                    },
                );
                clamped = Some(r.clamped_scroll);
            })
            .unwrap();
        assert_eq!(
            clamped,
            Some(12),
            "a status/elapsed change for the same current node must not move the scroll"
        );
    }
    // Breaks if: re-centring keys off node status/elapsed instead of node id.

    #[test]
    fn ct8_manual_navigation_keeps_selection_below_viewport_on_screen() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(30);
        state.effective_edges = many_edges(30);
        state.current_node_id = Some("m0".to_string());
        let node_info = NodeRunInfo::default();

        // Manual mode, selection near the bottom, user scroll still at the top.
        let text = render_to_text(80, 20, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    follow_anchor: None,
                    highlighted_node_id: Some("m26"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            text.contains("Node 26"),
            "manual navigation must scroll a below-viewport selection into view:\n{text}"
        );
    }
    // Breaks if: the manual branch returns `clamped_from_input` without
    // calling `ensure_visible`.

    #[test]
    fn ct8_manual_navigation_keeps_selection_above_viewport_on_screen() {
        let mut state = running_state();
        state.effective_nodes = many_nodes(30);
        state.effective_edges = many_edges(30);
        state.current_node_id = Some("m0".to_string());
        let node_info = NodeRunInfo::default();

        // Manual mode, selection back at the top, but user scroll left far down.
        let text = render_to_text(80, 20, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    follow_anchor: None,
                    highlighted_node_id: Some("m0"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 250,
                },
            );
        });
        assert!(
            text.contains("Node 0"),
            "manual navigation must scroll an above-viewport selection back into view:\n{text}"
        );
    }
    // Breaks if: the manual branch stops calling `ensure_visible`.

    #[test]
    fn ct8_mode_indicator_present_in_both_modes_and_names_the_key() {
        let state = running_state();
        let node_info = NodeRunInfo::default();

        let follow_text = render_to_text(80, 30, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    follow_anchor: None,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            follow_text.contains("AUTO-FOLLOW"),
            "auto-follow mode must render a labelled indicator:\n{follow_text}"
        );

        let manual_text = render_to_text(80, 30, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    follow_anchor: None,
                    highlighted_node_id: Some("n2"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                    theme: &Theme::classic(),
                    selected_spec_id: None,
                    spec_scroll: 0,
                    scroll: 0,
                },
            );
        });
        assert!(
            manual_text.contains("MANUAL"),
            "manual mode must render a labelled indicator:\n{manual_text}"
        );
        assert!(
            manual_text.contains("Esc"),
            "the manual indicator must name the key that restores auto-follow:\n{manual_text}"
        );
    }
    // Breaks if: `mode_indicator_line` is removed, stops being rendered, or
    // drops the "Esc" hint from the manual variant.
}
