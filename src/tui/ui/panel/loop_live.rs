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

use super::{
    compact_cwd, truncate_str, ACCENT, BORDER_COLOR, DIM, STATUS_FAIL, STATUS_OK, STATUS_RUNNING,
};
use crate::domain::loops::{
    LoopEdgeCondition, LoopNode, LoopRunStatus, LoopSpecStatus, LoopStatus,
};
use crate::tui::app::loop_live_state::{
    EnsembleLiveInfo, LoopLiveState, NodeRunInfo, SpecQueueEntry,
};
use crate::tui::app::types::App;

pub(crate) fn draw_loop_live_view(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(state) = app.loop_live_state.as_ref() else {
        frame.render_widget(
            Paragraph::new("No loop selected").style(Style::default().fg(DIM)),
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

    render_loop_live_view(
        frame,
        area,
        &LiveViewContext {
            state,
            follow: app.loop_graph_follow,
            highlighted_node_id: highlighted.as_deref(),
            node_info: &node_info,
            blocked,
            now: Utc::now(),
        },
    );
}

/// Everything the pure renderer needs, gathered by [`draw_loop_live_view`]
/// so the render itself stays a plain function of already-computed values
/// (no `Database`/`App` access), which is what makes it testable without a
/// full `App`.
struct LiveViewContext<'a> {
    state: &'a LoopLiveState,
    follow: bool,
    highlighted_node_id: Option<&'a str>,
    node_info: &'a NodeRunInfo,
    blocked: bool,
    now: DateTime<Utc>,
}

fn render_loop_live_view(frame: &mut Frame, area: Rect, ctx: &LiveViewContext) {
    let state = ctx.state;
    let mut lines = header_lines(state, ctx.blocked);
    lines.push(Line::from(""));
    lines.extend(queue_lines(state));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Graph", Style::default().fg(DIM))));
    if state.effective_nodes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no nodes yet)",
            Style::default().fg(DIM),
        )));
    } else {
        lines.extend(graph_lines(
            state,
            ctx.highlighted_node_id,
            ctx.follow,
            area.width,
        ));
    }
    lines.push(Line::from(""));
    lines.extend(footer_lines(
        state,
        ctx.highlighted_node_id,
        ctx.node_info,
        ctx.follow,
        ctx.now,
    ));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn status_icon_and_label(state: &LoopLiveState, blocked: bool) -> (&'static str, String, Color) {
    if let Some(at) = state.autorun_at {
        let local = at.with_timezone(&Local);
        return (
            "⏰",
            format!("autorun {}", local.format("%H:%M")),
            Color::Cyan,
        );
    }
    if blocked {
        return ("⛔", "blocked".to_string(), STATUS_FAIL);
    }
    match state.loop_status {
        LoopStatus::Running => ("▶", "running".to_string(), STATUS_RUNNING),
        LoopStatus::Paused => ("⏸", "paused".to_string(), Color::Yellow),
        LoopStatus::Completed => ("✓", "completed".to_string(), STATUS_OK),
        LoopStatus::Failed => ("✗", "failed".to_string(), STATUS_FAIL),
        LoopStatus::Draft => ("·", "draft".to_string(), DIM),
    }
}

fn header_lines(state: &LoopLiveState, blocked: bool) -> Vec<Line<'static>> {
    let (icon, label, color) = status_icon_and_label(state, blocked);
    vec![
        Line::from(vec![
            Span::styled(icon, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(
                state.loop_name.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(label, Style::default().fg(color)),
            Span::raw("   "),
            Span::styled(
                format!("{}/{} specs", state.done_count, state.total_count),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(Span::styled(
            format!("Workdir: {}", compact_cwd(&state.workdir)),
            Style::default().fg(DIM),
        )),
    ]
}

/// Chip glyph for a queue entry: the current spec always shows `▶`
/// regardless of its underlying status (running or the next pending one —
/// see `assemble_loop_live_state`'s `current_spec_id` rule), executed specs
/// (completed/failed/skipped) show `✓`, everything else is still `○`.
fn spec_chip(entry: &SpecQueueEntry, current_spec_id: Option<&str>) -> (&'static str, Color) {
    if Some(entry.spec_id.as_str()) == current_spec_id {
        ("▶", STATUS_RUNNING)
    } else if entry.status == LoopSpecStatus::Pending {
        ("○", DIM)
    } else {
        ("✓", STATUS_OK)
    }
}

fn queue_lines(state: &LoopLiveState) -> Vec<Line<'static>> {
    if state.spec_queue.is_empty() {
        return vec![Line::from(Span::styled(
            "Queue: (empty)",
            Style::default().fg(DIM),
        ))];
    }

    let mut chips: Vec<Span<'static>> = Vec::new();
    for entry in &state.spec_queue {
        let (icon, color) = spec_chip(entry, state.current_spec_id.as_deref());
        chips.push(Span::styled(icon, Style::default().fg(color)));
        chips.push(Span::raw(" "));
    }

    let mut lines = vec![Line::from(chips)];
    if let Some(current) = state
        .spec_queue
        .iter()
        .find(|entry| Some(entry.spec_id.as_str()) == state.current_spec_id.as_deref())
    {
        lines.push(Line::from(Span::styled(
            current.spec_name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

/// Border/text style and marker glyph for a node box — bold accent with a
/// solid marker while auto-following (the "pulsing current node" cue),
/// plain accent with a `›` marker for a manually-picked node, dim/plain
/// otherwise.
fn node_style(is_highlighted: bool, follow: bool) -> (Style, Style, &'static str) {
    if is_highlighted && follow {
        (
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            "●",
        )
    } else if is_highlighted {
        (
            Style::default().fg(ACCENT),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            "›",
        )
    } else {
        (
            Style::default().fg(BORDER_COLOR),
            Style::default().fg(Color::White),
            " ",
        )
    }
}

fn node_box_lines(
    node: &LoopNode,
    is_highlighted: bool,
    follow: bool,
    inner: usize,
) -> Vec<Line<'static>> {
    let (border_style, text_style, marker) = node_style(is_highlighted, follow);
    let kind_tag = format!("[{}]", node.kind.as_str());
    let max_name = inner.saturating_sub(2 + kind_tag.len());
    let name_display = truncate_str(&node.name, max_name);
    let spaces = inner.saturating_sub(2 + name_display.len() + kind_tag.len());

    vec![
        Line::from(Span::styled(
            format!("  ┌{}┐", "─".repeat(inner)),
            border_style,
        )),
        Line::from(Span::styled(
            format!(
                "  │{} {}{}{}│",
                marker,
                name_display,
                " ".repeat(spaces),
                kind_tag
            ),
            text_style,
        )),
        Line::from(Span::styled(
            format!("  └{}┘", "─".repeat(inner)),
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
) -> Vec<Line<'static>> {
    let (border_style, text_style, marker) = node_style(is_highlighted, follow);
    let title = format!("{} [{} models]", ensemble.name, ensemble.members.len());
    let max_title = inner.saturating_sub(2);
    let title_display = truncate_str(&title, max_title);
    let title_spaces = inner.saturating_sub(2 + title_display.chars().count());

    let mut lines = vec![
        Line::from(Span::styled(
            format!("  ┌{}┐", "─".repeat(inner)),
            border_style,
        )),
        Line::from(Span::styled(
            format!(
                "  │{} {}{}│",
                marker,
                title_display,
                " ".repeat(title_spaces)
            ),
            text_style,
        )),
    ];
    for member in &ensemble.members {
        let (tag, color) = ensemble_member_status_tag(member.status);
        let label = format!("{} {tag}", member.label);
        let max_label = inner.saturating_sub(4);
        let label_display = truncate_str(&label, max_label);
        let spaces = inner.saturating_sub(4 + label_display.chars().count());
        lines.push(Line::from(Span::styled(
            format!("  │  {}{}│", label_display, " ".repeat(spaces)),
            Style::default().fg(color),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!("  └{}┘", "─".repeat(inner)),
        border_style,
    )));
    lines
}

fn ensemble_member_status_tag(status: Option<LoopRunStatus>) -> (&'static str, Color) {
    match status {
        Some(LoopRunStatus::Pass) => ("[pass]", STATUS_OK),
        Some(LoopRunStatus::Fail) => ("[fail]", STATUS_FAIL),
        Some(LoopRunStatus::Running) => ("[running]", STATUS_RUNNING),
        None => ("[pending]", DIM),
    }
}

fn edge_lines(edges: &[(String, LoopEdgeCondition)]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (label, condition)) in edges.iter().enumerate() {
        let branch = if i == edges.len() - 1 { "└" } else { "├" };
        lines.push(Line::from(Span::styled(
            format!("   {}─ {} → {}", branch, condition.as_str(), label),
            Style::default().fg(DIM),
        )));
    }
    lines
}

/// Collapse a node's outgoing edge targets for display: consecutive targets
/// that are all members of the same ensemble (F1) become one
/// `"<ensemble name> [N models]"` label instead of N near-identical edges —
/// this is what an ensemble's *entry* fan-out looks like from its
/// predecessor's side. Targets outside any ensemble pass through unchanged.
fn collapse_ensemble_targets(
    edges: &[(&LoopNode, LoopEdgeCondition)],
    ensemble_by_member: &HashMap<&str, &EnsembleLiveInfo>,
) -> Vec<(String, LoopEdgeCondition)> {
    let mut seen_ensembles: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (target, condition) in edges {
        match ensemble_by_member.get(target.id.as_str()) {
            Some(ensemble) => {
                if seen_ensembles.insert(ensemble.ensemble_id.as_str()) {
                    out.push((
                        format!("{} [{} models]", ensemble.name, ensemble.members.len()),
                        *condition,
                    ));
                }
            }
            None => out.push((target.name.clone(), *condition)),
        }
    }
    out
}

/// Node boxes in `position` order (a simple, deterministic layout — see the
/// spec's "layered by graph depth" allowance), each followed by its
/// outgoing edges labeled with their condition. Handles cycles (a node
/// whose edges point back up the list) since edges are rendered as text
/// annotations rather than a 2-D layout.
///
/// An ensemble (F1) renders as one collapsed box (its members + join folded
/// together — see [`ensemble_box_lines`]) at the position of its first
/// member; every other member and the join itself are skipped as individual
/// boxes. Fan-out edges into an ensemble's members are likewise collapsed to
/// one edge (see [`collapse_ensemble_targets`]).
fn graph_lines(
    state: &LoopLiveState,
    highlighted_node_id: Option<&str>,
    follow: bool,
    area_width: u16,
) -> Vec<Line<'static>> {
    let mut outgoing: HashMap<&str, Vec<(&LoopNode, LoopEdgeCondition)>> = HashMap::new();
    for edge in &state.effective_edges {
        if let Some(target) = state
            .effective_nodes
            .iter()
            .find(|node| node.id == edge.to_node)
        {
            outgoing
                .entry(edge.from_node.as_str())
                .or_default()
                .push((target, edge.condition));
        }
    }

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

    let nodes = &state.effective_nodes;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut rendered_ensembles: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (idx, node) in nodes.iter().enumerate() {
        // The join is folded into its ensemble's collapsed box (rendered at
        // the first member's position) — never drawn as its own box.
        if join_node_ids.contains(node.id.as_str()) {
            continue;
        }

        if let Some(ensemble) = ensemble_by_member.get(node.id.as_str()) {
            if !rendered_ensembles.insert(ensemble.ensemble_id.as_str()) {
                continue;
            }
            let is_highlighted = ensemble
                .members
                .iter()
                .any(|m| Some(m.node_id.as_str()) == highlighted_node_id)
                || highlighted_node_id == Some(ensemble.join_node_id.as_str());
            lines.extend(ensemble_box_lines(ensemble, is_highlighted, follow, inner));

            // The collapsed box's own outgoing routing is the join's real
            // outgoing edges (on_pass_to/on_fail_to).
            if let Some(edges) = outgoing.get(ensemble.join_node_id.as_str()) {
                let labeled = collapse_ensemble_targets(edges, &ensemble_by_member);
                lines.extend(edge_lines(&labeled));
                lines.push(Line::from(""));
            } else if idx + 1 < nodes.len() {
                lines.push(Line::from(""));
            }
            continue;
        }

        let is_highlighted = highlighted_node_id == Some(node.id.as_str());
        lines.extend(node_box_lines(node, is_highlighted, follow, inner));

        if let Some(edges) = outgoing.get(node.id.as_str()) {
            let labeled = collapse_ensemble_targets(edges, &ensemble_by_member);
            lines.extend(edge_lines(&labeled));
            lines.push(Line::from(""));
        } else if idx + 1 < nodes.len() {
            lines.push(Line::from(""));
        }
    }
    lines
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

fn run_status_span(status: Option<LoopRunStatus>) -> Span<'static> {
    match status {
        Some(LoopRunStatus::Running) => {
            Span::styled("running", Style::default().fg(STATUS_RUNNING))
        }
        Some(LoopRunStatus::Pass) => Span::styled("pass", Style::default().fg(STATUS_OK)),
        Some(LoopRunStatus::Fail) => Span::styled("fail", Style::default().fg(STATUS_FAIL)),
        None => Span::styled("(no runs yet)", Style::default().fg(DIM)),
    }
}

fn footer_lines(
    state: &LoopLiveState,
    highlighted_node_id: Option<&str>,
    node_info: &NodeRunInfo,
    follow: bool,
    now: DateTime<Utc>,
) -> Vec<Line<'static>> {
    let Some(node_id) = highlighted_node_id else {
        return vec![Line::from(Span::styled(
            "(no node selected)",
            Style::default().fg(DIM),
        ))];
    };
    let Some(node) = state.effective_nodes.iter().find(|n| n.id == node_id) else {
        return vec![Line::from(Span::styled(
            "(node not found)",
            Style::default().fg(DIM),
        ))];
    };

    let mode_label = if follow { "auto-follow" } else { "manual" };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            node.name.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("[{}]", node.kind.as_str()),
            Style::default().fg(DIM),
        ),
        Span::raw("  "),
        Span::styled(format!("({mode_label})"), Style::default().fg(DIM)),
    ])];

    let mut meta = vec![run_status_span(node_info.status)];
    if let Some(started_at) = node_info.started_at {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("elapsed {}", format_elapsed(started_at, now)),
            Style::default().fg(DIM),
        ));
    }
    if let Some(iteration) = node_info.iteration {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("iter {iteration}"),
            Style::default().fg(DIM),
        ));
    }
    lines.push(Line::from(meta));

    if let Some(tail) = node_info.output_tail.as_deref() {
        lines.push(Line::from(""));
        for line in tail.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::White),
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
    use crate::tui::app::types::{App, ProjectsPanelFocus};
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
                },
                SpecQueueEntry {
                    spec_id: "s2".to_string(),
                    spec_name: "U1b live view".to_string(),
                    status: LoopSpecStatus::Running,
                },
                SpecQueueEntry {
                    spec_id: "s3".to_string(),
                    spec_name: "T1 theme".to_string(),
                    status: LoopSpecStatus::Pending,
                },
            ],
            done_count: 1,
            total_count: 3,
            current_spec_id: Some("s2".to_string()),
            effective_nodes: team_nodes(),
            effective_edges: team_edges(),
            ensembles: Vec::new(),
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
        };

        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
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
        };
        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: false,
                    highlighted_node_id: Some("n2"),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                },
            );
        });

        assert!(text.contains("manual"), "{text}");
        assert!(text.contains("reviewed and committed"), "{text}");
        // Manual pick uses the `›` marker, not the follow `●`.
        assert!(text.contains('›'), "expected manual marker in:\n{text}");
    }

    #[test]
    fn esc_restores_follow_via_app_state() {
        let (db, data_dir) = test_db_and_dir();
        seed_running_loop(&db);

        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.projects_panel_focus = ProjectsPanelFocus::Loops;
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
            highlighted_node_id: None,
            node_info: &node_info,
            blocked: false,
            now: Utc::now(),
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
        };

        let text = render_to_text(80, 40, |frame, area| {
            render_loop_live_view(
                frame,
                area,
                &LiveViewContext {
                    state: &state,
                    follow: true,
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
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
                name: "Proposers (join)".to_string(),
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
                    highlighted_node_id: state.current_node_id.as_deref(),
                    node_info: &node_info,
                    blocked: false,
                    now: Utc::now(),
                },
            );
        });

        // Collapsed to one box with the member count, not three separate
        // member boxes or a fourth box for the join.
        assert!(text.contains("Proposers [3 models]"), "{text}");
        assert!(!text.contains("Proposers [1]"), "{text}");
        assert!(!text.contains("Proposers [2]"), "{text}");
        assert!(!text.contains("Proposers (join)"), "{text}");
        // Per-member live status inside the collapsed box.
        assert!(text.contains("[pass]"), "{text}");
        assert!(text.contains("[running]"), "{text}");
        assert!(text.contains("[pending]"), "{text}");
        // The fan-out from kickoff collapses to one edge, and the join's own
        // routing to the arbiter still renders.
        assert!(text.contains("Kickoff"), "{text}");
        assert!(text.contains("Arbiter"), "{text}");
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
            active_run_pool_id: None,
            on_completed: None,
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
            workdir: None,
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
        })
        .unwrap();
    }
}
