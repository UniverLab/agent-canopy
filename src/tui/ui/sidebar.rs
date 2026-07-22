//! Sidebar rendering — layered: RAG (pinned top) → Live → Automation →
//! Knowledge (projects) → sysinfo (pinned bottom).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{
    last_two_segments, truncate_str, ACCENT, BG_HOVER, BG_SELECTED, BORDER_COLOR, DIM,
    INTERACTIVE_COLOR,
};
use super::{STATUS_DISABLED, STATUS_FAIL, STATUS_OK, STATUS_RUNNING};
use crate::domain::loops::{Loop, LoopStatus};
use crate::tui::agent::AgentStatus;
use crate::tui::app::types::{
    AgentEntry, AgentSectionFocus, App, AutomationKind, Focus, LoopSidebarMeta, SidebarLayer,
};
use ratatui::style::Color;

pub(super) fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    app.sidebar_click_map.clear();
    app.automation_loop_click_map.clear();
    app.project_click_map.clear();
    app.layer_header_click_map.clear();
    app.sidebar_visible_capacity = 0;

    let areas = split_sidebar_content(area, app);

    let show_rag = app.rag_info.has_rag_activity() && areas.content.height >= 6;
    let (rag_area, content_below) = split_top_panel(areas.content, show_rag, 6);

    if let Some(rag_area) = rag_area.filter(|area| area.height >= 3) {
        render_titled_panel(
            frame,
            rag_area,
            rag_info_title(app),
            Style::default().fg(if is_rag_focused(app) { ACCENT } else { DIM }),
            rag_border_style(app),
            |frame, inner| draw_rag_info(frame, inner, app),
        );
    }

    let brain_area = draw_sidebar_layers(frame, content_below, app);
    render_brain_or_graph(frame, brain_area, app);

    render_dashboard_if_present(frame, areas.dashboard, app);

    if let Some(dialog) = app.project_relation_dialog.as_ref() {
        draw_project_relation_dialog(frame, areas.content, app, dialog);
    }
}

#[derive(Clone, Copy)]
struct SidebarContentAreas {
    content: Rect,
    dashboard: Option<Rect>,
}

fn dashboard_height(app: &App) -> u16 {
    // Ask the dashboard how many rows it will actually draw (cpu/mem/load are
    // always present; gpu/pwr/swap only when their data is). Reserving a fixed
    // slot for optional rows — as an earlier version did for the now
    // battery-only `pwr:` row — left a blank line when the row was absent.
    let content_lines = crate::tui::ui::system_dashboard::dashboard_content_line_count(
        &app.system_info,
        app.temperature_unit,
    ) as u16;
    content_lines + 2
}

fn split_sidebar_content(area: Rect, app: &App) -> SidebarContentAreas {
    let height = dashboard_height(app);
    let dashboard = (area.height >= height).then_some(Rect::new(
        area.x,
        area.y + area.height - height,
        area.width,
        height,
    ));
    let content = dashboard.map_or(area, |dashboard| {
        Rect::new(
            area.x,
            area.y,
            area.width,
            area.height.saturating_sub(dashboard.height),
        )
    });
    SidebarContentAreas { content, dashboard }
}

fn split_top_panel(content: Rect, enabled: bool, top_height: u16) -> (Option<Rect>, Rect) {
    if !enabled {
        return (None, content);
    }

    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_height), Constraint::Min(0)]).areas(content);
    (Some(top), bottom)
}

fn section_block<'a>(title: &'a str, title_style: Style, border_style: Style) -> Block<'a> {
    Block::default()
        .title_bottom(
            Line::from(Span::styled(title, title_style))
                .alignment(ratatui::layout::Alignment::Right),
        )
        .borders(Borders::ALL)
        .border_style(border_style)
}

fn render_titled_panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    title_style: Style,
    border_style: Style,
    render_inner: impl FnOnce(&mut Frame, Rect),
) {
    let block = section_block(title, title_style, border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    render_inner(frame, inner);
}

fn take_top(area: &mut Rect, height: u16) -> Option<Rect> {
    if area.height == 0 || height == 0 {
        return None;
    }

    let [top, rest] = Layout::vertical([
        Constraint::Length(height.min(area.height)),
        Constraint::Min(0),
    ])
    .areas(*area);
    *area = rest;
    Some(top)
}

#[derive(Clone, Copy)]
struct ScrollState {
    start: usize,
    max_visible: usize,
    has_up: bool,
    has_down: bool,
}

fn scroll_state(total_items: usize, selected: Option<usize>, max_visible: usize) -> ScrollState {
    scroll_state_with_offset(total_items, selected, max_visible, 0)
}

/// Like [`scroll_state`], but shifts the auto-follow-selection start further
/// down by `manual_offset` rows (mouse-wheel scrolling), clamped so the list
/// never scrolls past its last page.
fn scroll_state_with_offset(
    total_items: usize,
    selected: Option<usize>,
    max_visible: usize,
    manual_offset: usize,
) -> ScrollState {
    let auto_start = selected.map_or(0, |selected| {
        if selected >= max_visible {
            selected.saturating_sub(max_visible - 1)
        } else {
            0
        }
    });
    let max_start = total_items.saturating_sub(max_visible);
    let start = (auto_start + manual_offset).min(max_start);

    ScrollState {
        start,
        max_visible,
        has_up: start > 0,
        has_down: total_items.saturating_sub(start) > max_visible,
    }
}

fn render_brain_if_visible(frame: &mut Frame, area: Rect, app: &App) {
    if area.height < 3 || area.width < 6 {
        return;
    }
    let Some(brain) = app.sidebar_brain.as_ref() else {
        return;
    };
    crate::tui::ui::panel::draw_brians_brain(frame, area, brain);
}

fn render_dashboard_if_present(frame: &mut Frame, area: Option<Rect>, app: &App) {
    let Some(area) = area else {
        return;
    };
    crate::tui::ui::system_dashboard::render_system_dashboard(
        frame,
        area,
        &app.system_info,
        app.temperature_unit,
    );
}

/// Leftover space below the three layers: the project relation graph when
/// there's something to show, else Brian's Brain.
fn render_brain_or_graph(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    if !app.project_graph_trees.is_empty() && area.height >= 4 {
        render_titled_panel(
            frame,
            area,
            " project graph ",
            Style::default().fg(DIM),
            Style::default().fg(DIM),
            |frame, inner| draw_project_graph(frame, inner, app),
        );
        return;
    }
    render_brain_if_visible(frame, area, app);
}

// ── Layer headers ─────────────────────────────────────────────────

fn layer_label(layer: SidebarLayer) -> &'static str {
    match layer {
        SidebarLayer::Live => "Live",
        SidebarLayer::Automation => "Automation",
        SidebarLayer::Knowledge => "Knowledge",
    }
}

fn layer_collapsed(app: &App, layer: SidebarLayer) -> bool {
    match layer {
        SidebarLayer::Live => app.live_collapsed,
        SidebarLayer::Automation => app.automation_collapsed,
        SidebarLayer::Knowledge => app.knowledge_collapsed,
    }
}

fn layer_count(app: &App, layer: SidebarLayer) -> usize {
    match layer {
        SidebarLayer::Live => {
            let (_, interactive, terminal) = agent_indices_by_kind(app);
            interactive.len() + terminal.len() + app.split_groups.len()
        }
        SidebarLayer::Automation => {
            let (background, _, _) = agent_indices_by_kind(app);
            background.len() + app.active_loops().len()
        }
        SidebarLayer::Knowledge => app.projects.len(),
    }
}

fn layer_focused(app: &App, layer: SidebarLayer) -> bool {
    matches!(app.focus, Focus::Home | Focus::Preview)
        && !app.playground_active
        && !app.agents_rag_focused
        && app.sidebar_layer == layer
}

/// Draws a layer's 1-row collapsible header (`▾ Live (3)`), registers it in
/// `layer_header_click_map`, and returns the row it occupies.
fn draw_layer_header(frame: &mut Frame, area: Rect, app: &mut App, layer: SidebarLayer) {
    if area.height == 0 {
        return;
    }
    let collapsed = layer_collapsed(app, layer);
    let arrow = if collapsed { "▸" } else { "▾" };
    let focused = layer_focused(app, layer);
    let fg = if focused { ACCENT } else { Color::White };
    let text = format!(
        " {arrow} {} ({}) ",
        layer_label(layer),
        layer_count(app, layer)
    );
    let bg = if focused { BG_SELECTED } else { Color::Reset };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    app.layer_header_click_map.push((layer, area.y, area.y + 1));
}

// ── Layer bodies ─────────────────────────────────────────────────

fn agent_indices_by_kind(app: &App) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    app.agents.iter().enumerate().fold(
        (Vec::new(), Vec::new(), Vec::new()),
        |mut indices, (index, agent)| {
            match agent {
                AgentEntry::Interactive(_) | AgentEntry::Orphaned(_) => indices.1.push(index),
                AgentEntry::Terminal(_) => indices.2.push(index),
                AgentEntry::Group(_) => {}
                _ => indices.0.push(index),
            }
            indices
        },
    )
}

/// Split `budget` rows across sections with the given `demands` using max-min
/// fair capping. No section is allocated more than it needs; the surplus freed
/// by sections that fit within an equal share is shared among the sections that
/// still want more, proportional to their unsatisfied demand. Sections that end
/// up below their demand scroll internally. Callers use this only when the
/// sections can't all be shown at full height (`sum(demands) > budget`), but the
/// function is correct for any input and always allocates at most `budget` rows.
fn fair_section_heights(demands: &[u16], budget: u16) -> Vec<u16> {
    let n = demands.len();
    let mut alloc = vec![0u16; n];
    let mut capped = vec![false; n];
    let mut remaining_budget = budget;

    loop {
        let active: Vec<usize> = (0..n).filter(|&i| !capped[i]).collect();
        if active.is_empty() || remaining_budget == 0 {
            break;
        }

        // Cap every section whose full remaining demand fits within an equal
        // share of the budget; the rows they don't take are freed for others.
        let share = remaining_budget / active.len() as u16;
        let mut capped_any = false;
        for &i in &active {
            let want = demands[i] - alloc[i];
            if want <= share {
                alloc[i] = demands[i];
                remaining_budget -= want;
                capped[i] = true;
                capped_any = true;
            }
        }
        if capped_any {
            continue;
        }

        // Nobody fully fits: hand out what's left proportional to each still-
        // hungry section's unsatisfied demand, then place the rounding remainder
        // on the hungriest sections one row at a time.
        let total_want: u32 = active.iter().map(|&i| (demands[i] - alloc[i]) as u32).sum();
        if total_want == 0 {
            break;
        }
        let budget_u32 = remaining_budget as u32;
        let mut distributed = 0u16;
        for &i in &active {
            let want = (demands[i] - alloc[i]) as u32;
            let give = (budget_u32 * want / total_want) as u16;
            alloc[i] += give;
            distributed += give;
        }
        let mut leftover = remaining_budget - distributed;
        while leftover > 0 {
            let Some(&i) = active
                .iter()
                .filter(|&&i| alloc[i] < demands[i])
                .max_by_key(|&&i| demands[i] - alloc[i])
            else {
                break;
            };
            alloc[i] += 1;
            leftover -= 1;
        }
        break;
    }

    alloc
}

/// Rows needed for a card-style sub-list (agent cards, loop cards): 0 when
/// empty, else `count*4+2` (3-row cards + 1-row gap + 2-row border).
fn card_list_demand(count: usize) -> u16 {
    if count == 0 {
        0
    } else {
        count as u16 * 4 + 2
    }
}

/// Rows needed for the compact groups list: 0 when empty, else
/// `count*2+2` (1-row entries + 1-row gap + 2-row border, see `draw_groups_list`).
fn groups_list_demand(count: usize) -> u16 {
    if count == 0 {
        0
    } else {
        count as u16 * 2 + 2
    }
}

fn live_body_demand(app: &App, interactive: &[usize], terminal: &[usize]) -> u16 {
    card_list_demand(interactive.len())
        + card_list_demand(terminal.len())
        + groups_list_demand(app.split_groups.len())
}

fn automation_body_demand(app: &App, background: &[usize]) -> u16 {
    card_list_demand(background.len()) + card_list_demand(app.active_loops().len())
}

fn knowledge_body_demand(app: &App) -> u16 {
    if app.projects.is_empty() {
        2
    } else {
        card_list_demand(app.projects.len())
    }
}

/// Lays out and draws the three sidebar layers top-to-bottom, returning the
/// leftover area (for the project graph / Brian's Brain).
fn draw_sidebar_layers(frame: &mut Frame, area: Rect, app: &mut App) -> Rect {
    let (background_indices, interactive_indices, terminal_indices) = agent_indices_by_kind(app);

    let header_budget = 3.min(area.height);
    let body_budget = area.height.saturating_sub(header_budget);
    let demands = [
        if app.live_collapsed {
            0
        } else {
            live_body_demand(app, &interactive_indices, &terminal_indices)
        },
        if app.automation_collapsed {
            0
        } else {
            automation_body_demand(app, &background_indices)
        },
        if app.knowledge_collapsed {
            0
        } else {
            knowledge_body_demand(app)
        },
    ];
    let alloc = fair_section_heights(&demands, body_budget);

    let mut remaining = area;

    if let Some(header) = take_top(&mut remaining, 1) {
        draw_layer_header(frame, header, app, SidebarLayer::Live);
    }
    if let Some(body) = take_top(&mut remaining, alloc[0]) {
        draw_live_body(frame, body, app, &interactive_indices, &terminal_indices);
    }

    if let Some(header) = take_top(&mut remaining, 1) {
        draw_layer_header(frame, header, app, SidebarLayer::Automation);
    }
    if let Some(body) = take_top(&mut remaining, alloc[1]) {
        draw_automation_body(frame, body, app, &background_indices);
    }

    if let Some(header) = take_top(&mut remaining, 1) {
        draw_layer_header(frame, header, app, SidebarLayer::Knowledge);
    }
    if let Some(body) = take_top(&mut remaining, alloc[2]) {
        draw_knowledge_body(frame, body, app);
    }

    remaining
}

fn draw_live_body(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    interactive_indices: &[usize],
    terminal_indices: &[usize],
) {
    let demands = [
        card_list_demand(interactive_indices.len()),
        card_list_demand(terminal_indices.len()),
        groups_list_demand(app.split_groups.len()),
    ];
    let alloc = fair_section_heights(&demands, area.height);
    let mut remaining = area;

    if let Some(sub) = take_top(&mut remaining, alloc[0]) {
        let border_style = agent_section_border_style(app, AgentSectionFocus::Interactive);
        render_agent_list_panel(
            frame,
            Some(sub),
            " interactive ",
            interactive_indices,
            app,
            INTERACTIVE_COLOR,
            border_style,
        );
    }
    if let Some(sub) = take_top(&mut remaining, alloc[1]) {
        let border_style = agent_section_border_style(app, AgentSectionFocus::Terminal);
        render_agent_list_panel(
            frame,
            Some(sub),
            " terminal ",
            terminal_indices,
            app,
            Color::Green,
            border_style,
        );
    }
    if let Some(sub) = take_top(&mut remaining, alloc[2]) {
        render_groups_panel(frame, Some(sub), app, AgentSectionFocus::Groups);
    }
}

fn draw_automation_body(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    background_indices: &[usize],
) {
    let loop_count = app.active_loops().len();
    let demands = [
        card_list_demand(background_indices.len()),
        card_list_demand(loop_count),
    ];
    let alloc = fair_section_heights(&demands, area.height);
    let mut remaining = area;

    if let Some(sub) = take_top(&mut remaining, alloc[0]) {
        let border_style = automation_agents_border_style(app);
        render_agent_list_panel(
            frame,
            Some(sub),
            " agents ",
            background_indices,
            app,
            ACCENT,
            border_style,
        );
    }
    if let Some(sub) = take_top(&mut remaining, alloc[1]) {
        render_titled_panel(
            frame,
            sub,
            " loops ",
            Style::default().fg(DIM),
            automation_border_style(app, AutomationKind::Loop),
            |frame, inner| draw_automation_loops_list(frame, inner, app),
        );
    }
}

fn draw_knowledge_body(frame: &mut Frame, area: Rect, app: &mut App) {
    render_titled_panel(
        frame,
        area,
        " projects ",
        Style::default().fg(DIM),
        knowledge_border_style(app),
        |frame, inner| draw_projects_list(frame, inner, app),
    );
}

// ── Focus/border styling ────────────────────────────────────────────

fn is_rag_focused(app: &App) -> bool {
    matches!(app.focus, Focus::Home | Focus::Preview)
        && app.agents_rag_focused
        && !app.playground_active
}

fn rag_info_title(app: &App) -> &'static str {
    if app.rag_paused {
        " ragInfo ⏸ "
    } else {
        " ragInfo "
    }
}

fn rag_border_style(app: &App) -> Style {
    Style::default().fg(if is_rag_focused(app) {
        ACCENT
    } else {
        BORDER_COLOR
    })
}

fn knowledge_border_style(app: &App) -> Style {
    let focused = layer_focused(app, SidebarLayer::Knowledge);
    Style::default().fg(if focused { ACCENT } else { BORDER_COLOR })
}

fn automation_border_style(app: &App, kind: AutomationKind) -> Style {
    let focused = layer_focused(app, SidebarLayer::Automation) && app.automation_kind == kind;
    Style::default().fg(if focused { ACCENT } else { BORDER_COLOR })
}

fn agent_section_border_style(app: &App, section: AgentSectionFocus) -> Style {
    let focused = if matches!(app.focus, Focus::Home | Focus::Preview) {
        layer_focused(app, SidebarLayer::Live) && app.agent_section_focus == section
    } else if app.focus == Focus::Agent {
        match app.agents.get(app.selected) {
            Some(AgentEntry::Interactive(_) | AgentEntry::Orphaned(_)) => {
                section == AgentSectionFocus::Interactive
            }
            Some(AgentEntry::Terminal(_)) => section == AgentSectionFocus::Terminal,
            Some(AgentEntry::Group(_)) => section == AgentSectionFocus::Groups,
            _ => false,
        }
    } else {
        false
    };

    Style::default().fg(if focused { ACCENT } else { BORDER_COLOR })
}

/// Border style for the Automation layer's `agents` sub-panel — distinct
/// from `agent_section_border_style` (Live's Interactive/Terminal/Groups)
/// since Automation tracks its active sub-list via `automation_kind`.
fn automation_agents_border_style(app: &App) -> Style {
    automation_border_style(app, AutomationKind::Agent)
}

fn render_agent_list_panel(
    frame: &mut Frame,
    area: Option<Rect>,
    title: &str,
    indices: &[usize],
    app: &mut App,
    accent: Color,
    border_style: Style,
) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        title,
        Style::default().fg(DIM),
        border_style,
        |frame, inner| draw_agent_list(frame, inner, indices, app, accent),
    );
}

fn render_groups_panel(
    frame: &mut Frame,
    area: Option<Rect>,
    app: &mut App,
    section: AgentSectionFocus,
) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        " groups ",
        Style::default().fg(DIM),
        agent_section_border_style(app, section),
        |frame, inner| draw_groups_list(frame, inner, app),
    );
}

// ── Knowledge layer: projects list ──────────────────────────────────

fn draw_projects_list(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No registered projects",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let scroll = scroll_state(
        app.projects.len(),
        Some(app.selected_project),
        (area.height / 4).max(1) as usize,
    );
    let panel_focused = knowledge_border_style_is_focused(app);
    let mut y = area.y;
    let row_h = 4u16;

    // Collected up front (rather than iterating `app.projects` directly) so
    // the loop body can also push into `app.project_click_map` — mirrors
    // `draw_automation_loops_list`'s `loop_ids_and_meta` pattern, since both
    // borrow `app` mutably for the click map alongside the data being drawn.
    let visible: Vec<(usize, String, String, String)> = app
        .projects
        .iter()
        .enumerate()
        .skip(scroll.start)
        .take(scroll.max_visible)
        .map(|(idx, project)| {
            (
                idx,
                project.name.clone(),
                project.hash.clone(),
                last_two_segments(&project.path),
            )
        })
        .collect();

    for (idx, name, hash, path) in &visible {
        if y + 3 > area.y + area.height {
            break;
        }
        draw_project_loop_card(
            frame,
            Rect::new(area.x, y, area.width, 3),
            *idx == app.selected_project,
            name,
            hash,
            path,
            panel_focused,
        );
        app.project_click_map.push((*idx, y, y + 3));
        y += row_h;
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down);
}

fn knowledge_border_style_is_focused(app: &App) -> bool {
    layer_focused(app, SidebarLayer::Knowledge)
}

fn draw_project_loop_card(
    frame: &mut Frame,
    area: Rect,
    selected: bool,
    title: &str,
    meta1: &str,
    meta2: &str,
    panel_focused: bool,
) {
    let bg = if selected { BG_SELECTED } else { Color::Reset };
    let title_style = project_title_style(selected, panel_focused);
    let meta_style = project_meta_style(selected);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(title, area.width as usize),
            title_style,
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(meta1, area.width as usize),
            meta_style,
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(meta2, area.width as usize),
            meta_style,
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );
}

// ── Automation layer: loops sub-list ────────────────────────────────

/// Status icon shown on an active loop's card: running takes priority, then
/// blocked (a paused loop whose latest run recorded a `loop_report_blocker`
/// description), then plain paused, then draft.
fn loop_status_icon(lp: &Loop, meta: LoopSidebarMeta) -> (&'static str, Color) {
    match lp.status {
        LoopStatus::Running => ("▶", STATUS_RUNNING),
        LoopStatus::Paused if meta.blocked => ("⛔", STATUS_FAIL),
        LoopStatus::Paused => ("⏸", Color::Yellow),
        LoopStatus::Draft | LoopStatus::Completed | LoopStatus::Failed => ("·", DIM),
    }
}

fn draw_active_loop_card(
    frame: &mut Frame,
    area: Rect,
    selected: bool,
    lp: &Loop,
    meta: LoopSidebarMeta,
    panel_focused: bool,
) {
    let bg = if selected { BG_SELECTED } else { Color::Reset };
    let title_style = project_title_style(selected, panel_focused);
    let meta_style = project_meta_style(selected);
    let (icon, icon_color) = loop_status_icon(lp, meta);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(icon, Style::default().fg(icon_color)),
            Span::raw(" "),
            Span::styled(
                truncate_str(&lp.name, area.width.saturating_sub(2) as usize),
                title_style,
            ),
        ]))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let progress = format!("{}/{} specs", meta.done, meta.total);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&progress, area.width as usize),
            meta_style,
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&last_two_segments(&lp.workdir), area.width as usize),
            meta_style,
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );
}

fn draw_automation_loops_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let loops = app.active_loops();
    if loops.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No active loops",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let selected_index = app
        .selected_loop_id
        .as_deref()
        .and_then(|id| loops.iter().position(|lp| lp.id == id));
    let scroll = scroll_state(
        loops.len(),
        selected_index,
        ((area.height + 1) / 4).max(1) as usize,
    );
    let panel_focused =
        layer_focused(app, SidebarLayer::Automation) && app.automation_kind == AutomationKind::Loop;
    let mut y = area.y;
    let row_h = 4u16;

    let loop_ids_and_meta: Vec<(String, Loop, LoopSidebarMeta)> = loops
        .iter()
        .copied()
        .skip(scroll.start)
        .take(scroll.max_visible)
        .map(|lp| {
            let meta = app
                .loop_sidebar_meta
                .get(&lp.id)
                .copied()
                .unwrap_or_default();
            (lp.id.clone(), lp.clone(), meta)
        })
        .collect();

    for (id, lp, meta) in &loop_ids_and_meta {
        if y + 3 > area.y + area.height {
            break;
        }
        let card_area = Rect::new(area.x, y, area.width, 3);
        let selected = app.selected_loop_id.as_deref() == Some(id.as_str());
        draw_active_loop_card(frame, card_area, selected, lp, *meta, panel_focused);
        app.automation_loop_click_map.push((id.clone(), y, y + 3));
        y += row_h;
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down);
}

fn project_title_style(selected: bool, panel_focused: bool) -> Style {
    if selected && panel_focused {
        return Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD);
    }
    if selected {
        return Style::default()
            .fg(Color::White)
            .bg(BG_SELECTED)
            .add_modifier(Modifier::BOLD);
    }
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

fn project_meta_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(Color::White).bg(BG_SELECTED)
    } else {
        Style::default().fg(DIM)
    }
}

// ── RAG (pinned top) ─────────────────────────────────────────────────

fn draw_rag_queue(
    frame: &mut Frame,
    area: Rect,
    items: &[crate::db::project::RagQueueItem],
    scroll_pos: usize,
) {
    use crate::tui::ui::ACCENT;

    let mut y = area.y;
    for (idx, item) in items.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let (icon, icon_color) = if item.status == "processing" {
            ("◉", Color::Yellow)
        } else {
            ("·", ACCENT)
        };
        let is_cursor = idx == scroll_pos;
        let prefix = if is_cursor { "›" } else { " " };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(ACCENT)),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::raw(" "),
                Span::styled(
                    truncate_str(&item.source_path, area.width.saturating_sub(3) as usize),
                    Style::default().fg(Color::White),
                ),
            ])),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
        if y < area.y + area.height {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate_str(
                        &last_two_segments(&item.source_path),
                        area.width.saturating_sub(3) as usize,
                    ),
                    Style::default().fg(DIM),
                ))),
                Rect::new(area.x + 2, y, area.width.saturating_sub(2), 1),
            );
            y += 1;
        }
    }
}

fn draw_rag_info(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![
        labeled_kv_line(" chunks: ", &app.rag_info.total_chunks.to_string()),
        labeled_kv_line(" files:  ", &app.rag_info.indexed_files.to_string()),
    ];

    let queue_text = rag_queue_text(app);
    if !queue_text.is_empty() {
        lines.push(labeled_kv_line(" queue:  ", &queue_text));
    }

    lines.push(rag_status_line(app));
    lines.push(Line::from(Span::styled(
        " Enter → playground ",
        Style::default().fg(ACCENT),
    )));

    frame.render_widget(Paragraph::new(lines), area);

    if app.rag_info.total_chunks == 0 && !app.global_rag_queue.is_empty() && area.height > 5 {
        let queue_area = Rect::new(
            area.x,
            area.y + 5,
            area.width,
            area.height.saturating_sub(5),
        );
        draw_rag_queue(
            frame,
            queue_area,
            &app.global_rag_queue,
            app.selected_rag_queue,
        );
    }
}

fn labeled_kv_line(label: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, Style::default().fg(DIM)),
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn rag_status_line(app: &App) -> Line<'static> {
    use crate::rag::status::{compute_rag_model_status, RagModelStatus};

    match compute_rag_model_status(
        app.rag_paused,
        app.rag_model_loaded,
        app.rag_info.processing_items,
    ) {
        RagModelStatus::Paused => Line::from(Span::styled(
            " ⏸ paused ",
            Style::default().fg(Color::Yellow),
        )),
        RagModelStatus::Ready if app.rag_info.processing_items > 0 => Line::from(Span::styled(
            " ◉ indexing ",
            Style::default().fg(Color::Yellow),
        )),
        RagModelStatus::Ready => Line::from(Span::styled(" ● ready ", Style::default().fg(ACCENT))),
        RagModelStatus::Sleeping => {
            Line::from(Span::styled(" ○ sleeping ", Style::default().fg(DIM)))
        }
    }
}

fn rag_queue_text(app: &App) -> String {
    if app.rag_info.queued_items > 0 {
        format!("{} queued", app.rag_info.queued_items)
    } else {
        String::new()
    }
}

// ── Live/Automation agent cards (shared card renderer) ──────────────

fn draw_agent_list(frame: &mut Frame, area: Rect, indices: &[usize], app: &mut App, accent: Color) {
    let card_h = 3u16;
    let row_h = 4u16;

    if area.height < card_h || indices.is_empty() {
        return;
    }

    let max_visible = ((area.height.saturating_sub(card_h)) / row_h + 1) as usize;
    let selected_local = indices.iter().position(|&idx| idx == app.selected);
    let scroll = scroll_state_with_offset(
        indices.len(),
        selected_local,
        max_visible,
        app.sidebar_scroll_offset,
    );
    app.sidebar_visible_capacity += scroll.max_visible;
    let mut y = area.y;
    let end = indices.len().min(scroll.start + scroll.max_visible + 1);

    for (rel_i, &idx) in indices[scroll.start..end].iter().enumerate() {
        if y + card_h > area.y + area.height {
            break;
        }

        let card_area = Rect::new(area.x, y, area.width, card_h);
        let selected = idx == app.selected && !app.agents_rag_focused;
        let hovered = app.hovered_row == Some(idx) && !selected;
        draw_sidebar_card(
            frame,
            card_area,
            &app.agents[idx],
            app,
            selected,
            hovered,
            accent,
        );
        app.sidebar_click_map.push((idx, y, y + card_h));

        let is_last_visible = scroll.start + rel_i >= indices.len() - 1;
        y += if is_last_visible { card_h } else { row_h };
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down);
}

fn draw_scroll_indicators(frame: &mut Frame, area: Rect, has_up: bool, has_down: bool) {
    if has_up {
        frame.render_widget(
            Paragraph::new("▲").style(Style::default().fg(DIM)),
            Rect::new(area.x + area.width.saturating_sub(2), area.y, 1, 1),
        );
    }
    if has_down {
        frame.render_widget(
            Paragraph::new("▼").style(Style::default().fg(DIM)),
            Rect::new(
                area.x + area.width.saturating_sub(2),
                (area.y + area.height).saturating_sub(1),
                1,
                1,
            ),
        );
    }
}

#[derive(Clone, Copy)]
struct AgentCardMeta<'a> {
    accent: Color,
    status_color: Color,
    agent_type: &'static str,
    type_detail: &'a str,
    work_dir: Option<&'a str>,
}

fn draw_sidebar_card(
    frame: &mut Frame,
    area: Rect,
    agent: &AgentEntry,
    app: &App,
    selected: bool,
    hovered: bool,
    _accent: Color,
) {
    let meta = agent_card_meta(agent, app);
    let status_color = effective_status_color(meta.status_color, agent, app, selected);
    let bg = if selected {
        BG_SELECTED
    } else if hovered {
        BG_HOVER
    } else {
        Color::Reset
    };
    let name = agent.id(app);

    let mut name_spans = vec![Span::styled(
        name,
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(if selected { meta.accent } else { Color::White }),
    )];
    if is_agent_in_group(name, app) {
        name_spans.push(Span::styled(" [▣]", Style::default().fg(DIM)));
    }
    render_sidebar_card_line(frame, area, 0, bg, status_color, name_spans);

    let type_detail = format!(
        "{} · {}",
        meta.agent_type,
        truncate_str(meta.type_detail, area.width.saturating_sub(6) as usize)
    );
    render_sidebar_card_line(
        frame,
        area,
        1,
        bg,
        status_color,
        vec![Span::styled(type_detail, Style::default().fg(DIM))],
    );

    let dir_text = meta
        .work_dir
        .filter(|dir| !dir.is_empty())
        .map(last_two_segments)
        .unwrap_or_else(|| "/".to_string());
    render_sidebar_card_line(
        frame,
        area,
        2,
        bg,
        status_color,
        vec![Span::styled(dir_text, Style::default().fg(DIM))],
    );
}

fn agent_card_meta<'a>(agent: &'a AgentEntry, app: &'a App) -> AgentCardMeta<'a> {
    match agent {
        AgentEntry::Agent(agent) => AgentCardMeta {
            accent: ACCENT,
            status_color: if !agent.enabled {
                STATUS_DISABLED
            } else if app.active_runs.contains_key(&agent.id) {
                STATUS_RUNNING
            } else if agent.last_run_ok == Some(false) {
                STATUS_FAIL
            } else {
                STATUS_OK
            },
            agent_type: agent.trigger_type_label(),
            type_detail: agent.cli.as_str(),
            work_dir: agent.working_dir.as_deref().or_else(|| agent.watch_path()),
        },
        AgentEntry::Interactive(index) => {
            let agent = &app.interactive_agents[*index];
            AgentCardMeta {
                accent: agent.accent_color,
                // Interactive agents pulse on recent output activity (unchanged).
                status_color: pty_session_status_color(
                    false,
                    &agent.status,
                    agent.has_recent_activity(),
                    false,
                    app.animation_tick,
                ),
                agent_type: "pty",
                type_detail: agent.cli.as_str(),
                work_dir: Some(agent.working_dir.as_str()),
            }
        }
        AgentEntry::Terminal(index) => {
            let agent = &app.terminal_agents[*index];
            AgentCardMeta {
                accent: agent.accent_color,
                // Terminal sessions pulse while a foreground command executes
                // and go solid green at the prompt — output activity is
                // deliberately ignored (a scrolled-by finished command must
                // not keep pulsing; a `watch`/`tail -f` still counts as
                // executing). Same source of truth warp uses to gate input.
                status_color: pty_session_status_color(
                    true,
                    &agent.status,
                    false,
                    agent.foreground_app_active(),
                    app.animation_tick,
                ),
                agent_type: "term",
                type_detail: agent.shell.as_str(),
                work_dir: Some(agent.working_dir.as_str()),
            }
        }
        AgentEntry::Orphaned(index) => {
            let session = &app.orphaned_sessions[*index];
            AgentCardMeta {
                accent: ratatui::style::Color::DarkGray,
                status_color: STATUS_FAIL,
                agent_type: "orphan",
                type_detail: session.cli.as_str(),
                work_dir: Some(session.working_dir.as_str()),
            }
        }
        AgentEntry::Group(_) => AgentCardMeta {
            accent: ACCENT,
            status_color: STATUS_OK,
            agent_type: "group",
            type_detail: "",
            work_dir: None,
        },
        AgentEntry::Corrupt(_) => AgentCardMeta {
            accent: ratatui::style::Color::Red,
            status_color: STATUS_FAIL,
            agent_type: "corrupt config",
            type_detail: "",
            work_dir: None,
        },
    }
}

/// Status color for an interactive/terminal session card. Blue is reserved
/// for background agents (see `agent_status` in `panel/details.rs`) — a PTY
/// is either alive (green) or dead (red). A running session pulses between
/// dim and bright green while it's registering activity (see
/// `ACTIVITY_IDLE_THRESHOLD_MS` and `pulse_active` — never blank, B21) and
/// holds solid green — "healthy, available" — once output has been quiet
/// for a while. Any exit, clean or not, means the PTY is dead: red.
fn session_status_color(status: &AgentStatus, pulsing: bool, animation_tick: u32) -> Color {
    match status {
        AgentStatus::Running if pulsing => pulse_active(animation_tick),
        AgentStatus::Running => STATUS_RUNNING,
        AgentStatus::Exited(_) => STATUS_FAIL,
    }
}

/// Pick which signal drives a PTY-backed session's pulse, then defer to
/// [`session_status_color`]. The two session kinds pulse on different truths:
///
/// * Interactive agents pulse on **recent output activity** (`recent_activity`)
///   — the long-standing B21 behavior, kept exactly as is.
/// * Terminal (plain shell) sessions pulse on **command execution**
///   (`command_executing`, from `InteractiveAgent::foreground_app_active` — the
///   same PTY foreground-process-group check warp uses to gate input) and go
///   solid green the moment the shell returns to its prompt, *regardless* of
///   output activity. A finished command whose output just scrolled by stops
///   pulsing immediately; a still-running `watch`/`tail -f` keeps pulsing.
///
/// Whichever signal isn't selected for a kind is ignored, so callers pass
/// `false` for it.
fn pty_session_status_color(
    is_terminal: bool,
    status: &AgentStatus,
    recent_activity: bool,
    command_executing: bool,
    animation_tick: u32,
) -> Color {
    let pulsing = if is_terminal {
        command_executing
    } else {
        recent_activity
    };
    session_status_color(status, pulsing, animation_tick)
}

/// Alternate between `on` and the shared "off" tone on the TUI's existing
/// animation tick — the same cadence pulsing yellow uses — so a blinking
/// indicator never needs its own timer.
fn pulse(on: Color, animation_tick: u32) -> Color {
    if (animation_tick / 10).is_multiple_of(2) {
        on
    } else {
        super::STATUS_WAIT_OFF
    }
}

/// Working-session heartbeat (B21): alternate between an illuminated green
/// and a muted gray-green on the shared animation tick. Unlike [`pulse`],
/// neither phase is blank — the indicator breathes, it never disappears.
fn pulse_active(animation_tick: u32) -> Color {
    if (animation_tick / 10).is_multiple_of(2) {
        super::STATUS_RUNNING_BRIGHT
    } else {
        super::STATUS_RUNNING_DIM
    }
}

fn effective_status_color(base: Color, agent: &AgentEntry, app: &App, selected: bool) -> Color {
    if !agent_is_waiting(agent, app, selected) {
        return base;
    }

    pulse(super::STATUS_WAIT_ON, app.animation_tick)
}

fn agent_is_waiting(agent: &AgentEntry, app: &App, selected: bool) -> bool {
    if selected && matches!(app.focus, Focus::Agent | Focus::Preview) {
        return false;
    }

    match agent {
        AgentEntry::Interactive(index) => app.interactive_agents[*index].is_waiting_for_input(),
        // Terminal sessions always have the cursor at the last row (shell prompt),
        // so is_waiting_for_input would generate constant false positives.
        AgentEntry::Terminal(_) => false,
        _ => false,
    }
}

fn is_agent_in_group(name: &str, app: &App) -> bool {
    app.split_groups
        .iter()
        .any(|group| group.session_a == name || group.session_b == name)
}

fn render_sidebar_card_line<'a>(
    frame: &mut Frame,
    area: Rect,
    line_offset: u16,
    bg: Color,
    status_color: Color,
    mut spans: Vec<Span<'a>>,
) {
    if area.height < line_offset + 1 {
        return;
    }

    spans.insert(0, Span::raw(" "));
    spans.insert(0, Span::styled("▌", Style::default().fg(status_color)));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        Rect::new(area.x, area.y + line_offset, area.width, 1),
    );
}

fn group_agent_indices(app: &App) -> Vec<usize> {
    app.agents
        .iter()
        .enumerate()
        .filter(|(_, agent)| matches!(agent, AgentEntry::Group(_)))
        .map(|(index, _)| index)
        .collect()
}

#[derive(Clone, Copy)]
struct GroupRowStyle {
    bg: Color,
    fg: Color,
    modifier: Modifier,
    prefix_color: Color,
    active_tag: &'static str,
}

fn group_row_style(is_selected: bool, is_active: bool) -> GroupRowStyle {
    GroupRowStyle {
        bg: if is_selected {
            BG_SELECTED
        } else {
            Color::Reset
        },
        fg: if is_selected {
            ACCENT
        } else if is_active {
            Color::Green
        } else {
            Color::White
        },
        modifier: if is_active || is_selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        },
        prefix_color: if is_active { Color::Green } else { DIM },
        active_tag: if is_active { " ●" } else { "" },
    }
}

fn draw_groups_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let group_agent_indices = group_agent_indices(app);
    let mut y = area.y;

    for (position, (&agent_idx, group)) in group_agent_indices
        .iter()
        .zip(app.split_groups.iter())
        .enumerate()
    {
        if y >= area.y + area.height {
            break;
        }

        let is_selected = agent_idx == app.selected && !app.agents_rag_focused;
        let is_active = app
            .active_split_id
            .as_deref()
            .is_some_and(|id| id == group.id);
        let style = group_row_style(is_selected, is_active);
        let label = format!("{} · {}", group.session_a, group.session_b);
        let text = format!(
            "{}{}",
            truncate_str(&label, area.width.saturating_sub(6) as usize),
            style.active_tag
        );
        let line = Line::from(vec![
            Span::styled("▌ ", Style::default().fg(style.prefix_color).bg(style.bg)),
            Span::styled(
                text,
                Style::default()
                    .fg(style.fg)
                    .bg(style.bg)
                    .add_modifier(style.modifier),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x, y, area.width, 1));
        app.sidebar_click_map.push((agent_idx, y, y + 1));

        y += if position < group_agent_indices.len() - 1 {
            2
        } else {
            1
        };
    }
}

// ── Project Graph ────────────────────────────────────────────────

fn draw_project_graph(frame: &mut Frame, area: Rect, app: &App) {
    if app.project_graph_trees.is_empty() || app.project_graph_edges.is_empty() {
        let msg = if app.projects.len() <= 1 {
            "No relationships yet. Press Enter on a project to link."
        } else {
            "No relationships yet. Press Enter to link projects."
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let edge_count = app
        .project_graph_edges
        .len()
        .min(area.height.saturating_sub(2) as usize);

    for (i, edge) in app.project_graph_edges.iter().take(edge_count).enumerate() {
        let y = area.y + i as u16;
        if y + 1 > area.y + area.height {
            break;
        }
        let relation = match edge.relation.as_str() {
            "depends_on" => "(depends)",
            "complements" => "(complements)",
            _ => "",
        };
        let label = format!(
            "{} → {} {}",
            truncate_str(&edge.from_name, 14),
            truncate_str(&edge.to_name, 14),
            relation,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_str(&label, area.width.saturating_sub(2) as usize),
                Style::default().fg(Color::Cyan),
            ))),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

// ── Project Relation Dialog ──────────────────────────────────────

fn draw_project_relation_dialog(
    frame: &mut Frame,
    _area: Rect,
    _app: &App,
    dialog: &crate::tui::app::types::ProjectRelationDialog,
) {
    // Center the dialog in the screen area
    let dialog_w = 50u16.min(frame.area().width.saturating_sub(4));
    let dialog_h = 14u16.min(frame.area().height.saturating_sub(2));
    let x = frame.area().x + (frame.area().width.saturating_sub(dialog_w)) / 2;
    let y = frame.area().y + (frame.area().height.saturating_sub(dialog_h)) / 2;
    let area = Rect::new(x, y, dialog_w, dialog_h);

    let block = Block::default()
        .title(format!(" Link: {} ", dialog.from_name))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT));
    frame.render_widget(block, area);

    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );

    // Relation type selector
    let rel_line = format!(
        "Relation: ◀ {} ▶",
        dialog.relation_types[dialog.relation_idx]
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rel_line,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    if let Some(ref error) = dialog.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_str(error, inner.width as usize),
                Style::default().fg(Color::Red),
            ))),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }

    // Search/filter field
    let filter_label = if dialog.filter_buffer.is_empty() {
        "filter: _"
    } else {
        &dialog.filter_buffer
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{}|", filter_label),
            Style::default().fg(Color::Yellow),
        ))),
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
    );

    // Project list
    let list_start_y = inner.y + 3;
    let max_items = inner
        .height
        .saturating_sub(4)
        .min(dialog.filtered.len() as u16);

    for i in 0..max_items {
        let idx = dialog.filtered[i as usize];
        let project = &dialog.available[idx];
        let name = format!(
            "{}  {}",
            if i as usize == dialog.selected_idx {
                "▶"
            } else {
                " "
            },
            truncate_str(&project.title, inner.width.saturating_sub(4) as usize),
        );
        let style = if i as usize == dialog.selected_idx {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(name, style))),
            Rect::new(inner.x, list_start_y + i, inner.width, 1),
        );
    }

    if dialog.filtered.is_empty() && dialog.filter_buffer.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No other projects indexed.",
                Style::default().fg(Color::DarkGray),
            ))),
            Rect::new(inner.x, list_start_y, inner.width, 1),
        );
    }

    // Footer
    let footer_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Enter confirm  ·  ←→ relation  ·  Esc cancel  ·  type filter ",
            Style::default().fg(DIM),
        ))),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needed_agents(count: u16) -> u16 {
        count * 4 + 2
    }

    /// Builds an App backed by a fresh temp DB with `project_count` registered
    /// projects and one loop named "Probe Loop", then renders the sidebar into
    /// a `width`x`height` TestBackend and returns the screen contents as a
    /// flat string for substring assertions.
    fn render_sidebar_text(project_count: usize, width: u16, height: u16) -> String {
        use crate::db::Database;
        use crate::domain::loops::{Loop, LoopStatus};
        use crate::domain::project::Project;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::sync::Arc;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        for i in 0..project_count {
            db.upsert_project(&Project {
                hash: format!("hash{i}"),
                path: format!("/tmp/project{i}"),
                name: format!("project{i}"),
                description: None,
                tags: None,
                indexed_at: None,
                created_at: 0,
            })
            .unwrap();
        }
        db.insert_loop(&Loop {
            id: "wf-probe".to_string(),
            name: "Probe Loop".to_string(),
            description: None,
            workdir: "/tmp/probe".to_string(),
            status: LoopStatus::Running,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        assert!(
            !app.active_loops().is_empty(),
            "loop should be loaded from db"
        );

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app);
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

    #[test]
    fn layer_headers_render_with_counts() {
        let text = render_sidebar_text(2, 34, 40);
        assert!(text.contains("Live"), "expected Live layer header");
        assert!(
            text.contains("Automation"),
            "expected Automation layer header"
        );
        assert!(
            text.contains("Knowledge (2)"),
            "expected Knowledge layer header with count"
        );
    }

    #[test]
    fn automation_layer_shows_running_loop() {
        let text = render_sidebar_text(1, 34, 40);
        assert!(text.contains("Probe Loop"), "expected loop name visible");
    }

    #[test]
    fn old_top_level_backlog_knowledge_history_sections_are_gone() {
        // T-regression: these used to be top-level sidebar sections; they now
        // only exist inside a project's Focus tab bar.
        let text = render_sidebar_text(1, 34, 40);
        assert!(
            !text.contains(" backlog "),
            "backlog must not be a top-level section"
        );
        assert!(
            !text.contains(" history "),
            "history must not be a top-level section"
        );
    }

    #[test]
    fn collapsed_layer_hides_its_body() {
        use crate::db::Database;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::sync::Arc;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        db.upsert_project(&crate::domain::project::Project {
            hash: "hash0".to_string(),
            path: "/tmp/project0".to_string(),
            name: "project0".to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: 0,
        })
        .unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.knowledge_collapsed = true;

        let backend = TestBackend::new(34, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app);
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

        assert!(text.contains("Knowledge (1)"), "header still shows count");
        assert!(!text.contains("project0"), "collapsed layer hides its body");
    }

    #[test]
    fn drawing_the_knowledge_layer_populates_project_click_map() {
        // T-regression: `draw_projects_list`/`draw_knowledge_body` used to
        // take `&App`, so nothing ever pushed into `project_click_map` and a
        // mouse click on a sidebar project row silently did nothing —
        // functional requirement 4 requires project rows to be clickable.
        use crate::db::Database;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::sync::Arc;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        for i in 0..2 {
            db.upsert_project(&crate::domain::project::Project {
                hash: format!("hash{i}"),
                path: format!("/tmp/project{i}"),
                name: format!("project{i}"),
                description: None,
                tags: None,
                indexed_at: None,
                created_at: 0,
            })
            .unwrap();
        }

        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        assert_eq!(app.projects.len(), 2, "projects should be loaded from db");

        let backend = TestBackend::new(34, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app);
            })
            .unwrap();

        assert_eq!(
            app.project_click_map.len(),
            2,
            "each rendered project row must register a click region: {:?}",
            app.project_click_map
        );
        let indices: Vec<usize> = app
            .project_click_map
            .iter()
            .map(|&(idx, _, _)| idx)
            .collect();
        assert!(indices.contains(&0));
        assert!(indices.contains(&1));
    }

    #[test]
    fn fair_section_heights_caps_small_sections_at_their_demand() {
        // background: 1 agent (6), interactive: 6 agents (26), terminal: 1 (6).
        let demands = [needed_agents(1), needed_agents(6), needed_agents(1)];
        let total: u16 = demands.iter().sum();
        assert_eq!(total, 38);

        let alloc = fair_section_heights(&demands, 30);

        // No section is ever allocated more than it needs.
        for (got, want) in alloc.iter().zip(demands.iter()) {
            assert!(got <= want, "section {got} exceeded its demand {want}");
        }
        // The two small sections get exactly what they need (no empty gap).
        assert_eq!(alloc[0], demands[0]);
        assert_eq!(alloc[2], demands[2]);
        // Interactive absorbs all the surplus and scrolls internally.
        assert_eq!(alloc[1], 30 - demands[0] - demands[2]);
        // The whole budget is used — no leftover rows leaking to a brain gap.
        assert_eq!(alloc.iter().sum::<u16>(), 30);
    }

    #[test]
    fn fair_section_heights_never_exceeds_demand_with_four_sections() {
        let demands = [needed_agents(1), needed_agents(8), needed_agents(1), 4];
        let budget = 24;
        assert!(demands.iter().sum::<u16>() > budget);

        let alloc = fair_section_heights(&demands, budget);

        for (got, want) in alloc.iter().zip(demands.iter()) {
            assert!(got <= want, "section {got} exceeded its demand {want}");
        }
        assert_eq!(alloc.iter().sum::<u16>(), budget);
    }

    #[test]
    fn fair_section_heights_fits_everyone_when_budget_is_ample() {
        let demands = [needed_agents(1), needed_agents(2)];
        let alloc = fair_section_heights(&demands, 100);
        assert_eq!(alloc[0], demands[0]);
        assert_eq!(alloc[1], demands[1]);
    }

    #[test]
    fn running_session_with_recent_activity_is_working_green() {
        assert_eq!(
            session_status_color(&AgentStatus::Running, true, 0),
            pulse_active(0)
        );
        // Neither pulse phase may be blank — the indicator must never
        // disappear (B21).
        assert_ne!(pulse_active(0), super::super::STATUS_WAIT_OFF);
        assert_ne!(pulse_active(10), super::super::STATUS_WAIT_OFF);
        assert_ne!(pulse_active(0), pulse_active(10));
    }

    #[test]
    fn running_session_gone_quiet_is_healthy_idle_green() {
        assert_eq!(
            session_status_color(&AgentStatus::Running, false, 0),
            STATUS_RUNNING
        );
    }

    #[test]
    fn exit_error_wins_over_activity() {
        assert_eq!(
            session_status_color(&AgentStatus::Exited(1), true, 0),
            STATUS_FAIL
        );
        assert_eq!(
            session_status_color(&AgentStatus::Exited(1), false, 0),
            STATUS_FAIL
        );
    }

    #[test]
    fn exit_clean_or_not_is_dead_pty_red() {
        assert_eq!(
            session_status_color(&AgentStatus::Exited(0), true, 0),
            STATUS_FAIL
        );
        assert_eq!(
            session_status_color(&AgentStatus::Exited(0), false, 0),
            STATUS_FAIL
        );
    }

    #[test]
    fn terminal_running_a_command_pulses_never_blank() {
        // A foreground command is executing → pulse through both phases,
        // ignoring output activity entirely.
        let bright = pty_session_status_color(true, &AgentStatus::Running, false, true, 0);
        let dim = pty_session_status_color(true, &AgentStatus::Running, false, true, 10);
        assert_eq!(bright, super::super::STATUS_RUNNING_BRIGHT);
        assert_eq!(dim, super::super::STATUS_RUNNING_DIM);
        // Never blank across the cycle (B21).
        assert_ne!(bright, super::super::STATUS_WAIT_OFF);
        assert_ne!(dim, super::super::STATUS_WAIT_OFF);
        assert_ne!(bright, dim);
    }

    #[test]
    fn terminal_idle_at_prompt_is_solid_even_right_after_output() {
        // No foreground command, but output just scrolled by (recent_activity
        // true): a terminal must NOT keep pulsing — it's solid green.
        assert_eq!(
            pty_session_status_color(true, &AgentStatus::Running, true, false, 0),
            STATUS_RUNNING
        );
        assert_eq!(
            pty_session_status_color(true, &AgentStatus::Running, true, false, 10),
            STATUS_RUNNING
        );
    }

    #[test]
    fn terminal_ignores_output_activity_command_execution_wins() {
        // Command executing but no recent output (e.g. a blocking `sleep`):
        // still pulsing. Output present but command finished: solid.
        assert_eq!(
            pty_session_status_color(true, &AgentStatus::Running, false, true, 0),
            pulse_active(0)
        );
        assert_eq!(
            pty_session_status_color(true, &AgentStatus::Running, true, false, 0),
            STATUS_RUNNING
        );
    }

    #[test]
    fn interactive_agent_still_pulses_on_output_activity() {
        // Interactive kind keeps the activity-based behavior unchanged: the
        // command_executing signal is ignored for it.
        assert_eq!(
            pty_session_status_color(false, &AgentStatus::Running, true, false, 0),
            pulse_active(0)
        );
        assert_eq!(
            pty_session_status_color(false, &AgentStatus::Running, false, true, 0),
            STATUS_RUNNING
        );
        // Exited stays red regardless of signals.
        assert_eq!(
            pty_session_status_color(false, &AgentStatus::Exited(0), true, false, 0),
            STATUS_FAIL
        );
    }
}
