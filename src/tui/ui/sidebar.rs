//! Sidebar rendering — RAG (pinned top) → tab bar (Live / Automation /
//! Knowledge, exactly one visible at a time, full remaining height) →
//! sysinfo (pinned bottom).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::theme::Theme;
use super::{borders_for, last_two_segments, truncate_str, BG_HOVER, INTERACTIVE_COLOR};
use super::{STATUS_DISABLED, STATUS_FAIL, STATUS_OK, STATUS_RUNNING};
use crate::domain::loops::{Loop, LoopStatus};
use crate::tui::agent::AgentStatus;
use crate::tui::app::types::{
    AgentEntry, AgentSectionFocus, App, AutomationKind, Focus, LoopSidebarMeta, SidebarLayer,
};
use ratatui::style::Color;

pub(super) fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    app.sidebar_click_map.clear();
    app.automation_loop_click_map.clear();
    app.project_click_map.clear();
    app.sidebar_tab_click_map.clear();
    app.sidebar_visible_capacity = 0;

    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(theme.sidebar_bg)),
        area,
    );

    let areas = split_sidebar_content(area, app, theme);

    let show_rag = app.rag_info.has_rag_activity() && areas.content.height >= 6;
    let (rag_area, content_below) = split_top_panel(areas.content, show_rag, 6);

    if let Some(rag_area) = rag_area.filter(|area| area.height >= 3) {
        render_titled_panel(
            frame,
            rag_area,
            rag_info_title(app),
            Style::default().fg(if is_rag_focused(app) {
                theme.header_color
            } else {
                theme.dim_text
            }),
            rag_border_style(app, theme),
            theme,
            |frame, inner| draw_rag_info(frame, inner, app, theme),
        );
    }

    let brain_area = draw_sidebar_tabs(frame, content_below, app, theme);
    let is_knowledge_tab = app.sidebar_layer == SidebarLayer::Knowledge;
    render_brain_or_graph(frame, brain_area, app, theme, is_knowledge_tab);

    render_dashboard_if_present(frame, areas.dashboard, app, theme);

    if let Some(dialog) = app.project_relation_dialog.as_ref() {
        draw_project_relation_dialog(frame, areas.content, app, dialog, theme);
    }
}

#[derive(Clone, Copy)]
struct SidebarContentAreas {
    content: Rect,
    dashboard: Option<Rect>,
}

fn dashboard_height(app: &App, theme: &Theme) -> u16 {
    // Ask the dashboard how many rows it will actually draw (cpu/mem/load are
    // always present; gpu/pwr/swap only when their data is). Reserving a fixed
    // slot for optional rows — as an earlier version did for the now
    // battery-only `pwr:` row — left a blank line when the row was absent.
    let content_lines = crate::tui::ui::system_dashboard::dashboard_content_line_count(
        &app.system_info,
        app.temperature_unit,
        theme,
    ) as u16;
    content_lines + 2
}

fn split_sidebar_content(area: Rect, app: &App, theme: &Theme) -> SidebarContentAreas {
    let height = dashboard_height(app, theme);
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

fn section_block<'a>(
    title: &'a str,
    title_style: Style,
    border_style: Style,
    theme: &Theme,
) -> Block<'a> {
    Block::default()
        .title_bottom(
            Line::from(Span::styled(title, title_style))
                .alignment(ratatui::layout::Alignment::Right),
        )
        .borders(borders_for(theme))
        .border_style(border_style)
}

fn render_titled_panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    title_style: Style,
    border_style: Style,
    theme: &Theme,
    render_inner: impl FnOnce(&mut Frame, Rect),
) {
    let block = section_block(title, title_style, border_style, theme);
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
    let auto_start = selected.map_or(0, |sel| {
        crate::tui::selection::clamp_scroll(sel, 0, total_items, max_visible)
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

fn render_dashboard_if_present(frame: &mut Frame, area: Option<Rect>, app: &App, theme: &Theme) {
    let Some(area) = area else {
        return;
    };
    crate::tui::ui::system_dashboard::render_system_dashboard(
        frame,
        area,
        &app.system_info,
        app.temperature_unit,
        theme,
    );
}

/// The project graph's minimum *outer* height — what the split hands out,
/// not what `draw_project_graph` gets to draw into. `render_titled_panel`
/// strips the border before calling in, so with `Borders::ALL` this leaves
/// an inner budget of 2 edge lines; with a borderless theme, all 4.
/// `draw_project_graph` must budget the inner height it receives directly
/// rather than subtracting border rows a second time.
const GRAPH_MIN_HEIGHT: u16 = 4;

/// How the leftover space below the three layers is carved up between the
/// project relation graph and Brian's Brain. When the graph has nothing to
/// show, the brain takes the whole area, as before. When it does, the graph
/// gets its minimum first (it carries information; the brain is atmosphere),
/// then the brain takes whatever remains, if that remainder still meets its
/// own minimum — otherwise the graph draws alone rather than splitting into
/// two broken panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrainOrGraphLayout {
    Neither,
    GraphOnly(Rect),
    BrainOnly(Rect),
    Both { graph: Rect, brain: Rect },
}

fn split_brain_or_graph(area: Rect, graph_has_content: bool) -> BrainOrGraphLayout {
    if area.height == 0 {
        return BrainOrGraphLayout::Neither;
    }

    if !graph_has_content {
        return if area.height >= 3 && area.width >= 6 {
            BrainOrGraphLayout::BrainOnly(area)
        } else {
            BrainOrGraphLayout::Neither
        };
    }

    if area.height < GRAPH_MIN_HEIGHT {
        return BrainOrGraphLayout::Neither;
    }

    let brain_fits = area.width >= 6 && area.height >= GRAPH_MIN_HEIGHT + 3;
    if !brain_fits {
        return BrainOrGraphLayout::GraphOnly(area);
    }

    let graph = Rect::new(area.x, area.y, area.width, GRAPH_MIN_HEIGHT);
    let brain = Rect::new(
        area.x,
        area.y + GRAPH_MIN_HEIGHT,
        area.width,
        area.height - GRAPH_MIN_HEIGHT,
    );
    BrainOrGraphLayout::Both { graph, brain }
}

fn render_graph_panel(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    render_titled_panel(
        frame,
        area,
        " project graph ",
        Style::default().fg(theme.dim_text),
        Style::default().fg(theme.dim_text),
        theme,
        |frame, inner| draw_project_graph(frame, inner, app, theme),
    );
}

/// Whether the shared strip should reserve rows for the project graph
/// instead of handing them all to Brian's Brain: only on the Knowledge tab
/// (C26 decision 1 — projects aren't the subject on Live/Automation), and
/// only when there's an edge to draw (C26 decision 2 — a workspace of
/// unrelated projects still fills `project_graph_trees` with one singleton
/// per project, which is not "content").
fn graph_has_content(edge_count: usize, is_knowledge_tab: bool) -> bool {
    is_knowledge_tab && edge_count > 0
}

fn render_brain_or_graph(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    theme: &Theme,
    is_knowledge_tab: bool,
) {
    let graph_has_content = graph_has_content(app.project_graph_edges.len(), is_knowledge_tab);
    match split_brain_or_graph(area, graph_has_content) {
        BrainOrGraphLayout::Neither => {}
        BrainOrGraphLayout::GraphOnly(graph_area) => {
            render_graph_panel(frame, graph_area, app, theme);
        }
        BrainOrGraphLayout::BrainOnly(brain_area) => {
            render_brain_if_visible(frame, brain_area, app);
        }
        BrainOrGraphLayout::Both { graph, brain } => {
            render_graph_panel(frame, graph, app, theme);
            render_brain_if_visible(frame, brain, app);
        }
    }
}

// ── Layer headers ─────────────────────────────────────────────────

fn layer_label(layer: SidebarLayer) -> &'static str {
    match layer {
        SidebarLayer::Live => "Live",
        SidebarLayer::Automation => "Automation",
        SidebarLayer::Knowledge => "Knowledge",
    }
}

fn layer_focused(app: &App, layer: SidebarLayer) -> bool {
    matches!(app.focus, Focus::Home | Focus::Preview)
        && !app.playground_active
        && !app.agents_rag_focused
        && app.sidebar_layer == layer
}

const SIDEBAR_TABS: [SidebarLayer; 3] = [
    SidebarLayer::Live,
    SidebarLayer::Automation,
    SidebarLayer::Knowledge,
];

/// Centers `label` in `width` columns, truncating it if the cell is too
/// narrow. Tabs carry the label alone: at the real `SIDEBAR_WIDTH` each cell
/// is 11 columns, which fits every full word only once the item count is
/// gone — and a count that forces `"Automa… (12)"` costs more legibility
/// than it buys, since the tab's own body shows the items anyway.
fn tab_cell_text(label: &str, width: usize) -> String {
    let text = truncate_str(label, width);
    let slack = width.saturating_sub(text.chars().count());
    let left_pad = slack / 2;
    let right_pad = slack - left_pad;
    format!("{}{text}{}", " ".repeat(left_pad), " ".repeat(right_pad))
}

/// Draws the sidebar's `Live / Automation / Knowledge` tab strip: one cell
/// per tab, each an equal share of `area.width` (the last cell absorbs the
/// rounding remainder so the three always cover the full row exactly — no
/// gap for the background paint underneath to show through). The active
/// tab's label renders in `theme.header_color` and bold, over the default
/// background; inactive cells are dimmed text with no modifier. Registers
/// each cell's hit box in `sidebar_tab_click_map` for mouse clicks — the
/// hit box always spans the full cell width, regardless of the label's
/// centring.
fn draw_sidebar_tab_bar(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let col_w = area.width / SIDEBAR_TABS.len() as u16;
    let mut x = area.x;
    for (i, &layer) in SIDEBAR_TABS.iter().enumerate() {
        let width = if i + 1 == SIDEBAR_TABS.len() {
            area.x + area.width - x
        } else {
            col_w
        };
        if width == 0 {
            break;
        }

        let active = app.sidebar_layer == layer;
        let text = tab_cell_text(layer_label(layer), width as usize);
        let (fg, modifier) = if active {
            (theme.header_color, Modifier::BOLD)
        } else {
            (theme.dim_text, Modifier::empty())
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(fg).add_modifier(modifier),
            )))
            .style(Style::default().bg(Color::Reset)),
            Rect::new(x, area.y, width, 1),
        );
        app.sidebar_tab_click_map
            .push((layer, area.y, x, x + width));
        x += width;
    }
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
fn fair_section_heights(
    demands: &[u16],
    budget: u16,
    focused: Option<usize>,
    floor: u16,
) -> Vec<u16> {
    let n = demands.len();
    let mut alloc = vec![0u16; n];
    let mut capped = vec![false; n];
    let mut remaining_budget = budget;

    // Guarantee the focused section a floor before fair distribution, if it
    // has any demand at all. This is the fix for "I can navigate them but
    // I can't see them" on short screens: the section with the cursor must
    // always have rows to draw into.
    if let Some(idx) = focused {
        if idx < n && demands[idx] > 0 && floor > 0 && budget > 0 {
            let guaranteed = floor.min(demands[idx]).min(budget);
            alloc[idx] = guaranteed;
            remaining_budget = budget - guaranteed;
            // If the focused section's demand was fully satisfied by the floor,
            // it is done; otherwise the fair pass will allocate any additional
            // share on top of the floor.
            if alloc[idx] >= demands[idx] {
                capped[idx] = true;
            }
        }
    }

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

/// Draws the sidebar's tab strip plus the active tab's body, which fills all
/// remaining height — exactly one tab is visible at a time, so there's no
/// more space-sharing between layers (`fair_section_heights` is still used
/// *within* a tab's own sub-sections, e.g. Live's interactive/terminal/
/// groups panels). Returns the rows the active tab did not claim, which the
/// project graph and Brian's Brain share. `fair_section_heights` caps each
/// sub-section at its own demand, so a tab with few agents genuinely leaves
/// rows over; handing back a hardcoded empty rect here is what made the brain
/// unreachable no matter how that leftover was later divided.
fn draw_sidebar_tabs(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) -> Rect {
    let (background_indices, interactive_indices, terminal_indices) = agent_indices_by_kind(app);

    let mut remaining = area;
    if let Some(bar) = take_top(&mut remaining, 1) {
        draw_sidebar_tab_bar(frame, bar, app, theme);
    }

    match app.sidebar_layer {
        SidebarLayer::Live => draw_live_body(
            frame,
            remaining,
            app,
            &interactive_indices,
            &terminal_indices,
            theme,
        ),
        SidebarLayer::Automation => {
            draw_automation_body(frame, remaining, app, &background_indices, theme)
        }
        SidebarLayer::Knowledge => draw_knowledge_body(frame, remaining, app, theme),
    }
}

fn draw_live_body(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    interactive_indices: &[usize],
    terminal_indices: &[usize],
    theme: &Theme,
) -> Rect {
    let demands = [
        card_list_demand(interactive_indices.len()),
        card_list_demand(terminal_indices.len()),
        groups_list_demand(app.split_groups.len()),
    ];
    let focused_idx = match app.agent_section_focus {
        AgentSectionFocus::Interactive => Some(0),
        AgentSectionFocus::Terminal => Some(1),
        AgentSectionFocus::Groups => Some(2),
        AgentSectionFocus::Brain => None,
    };
    let alloc = fair_section_heights(&demands, area.height, focused_idx, 6);
    let mut remaining = area;

    if let Some(sub) = take_top(&mut remaining, alloc[0]) {
        let border_style = agent_section_border_style(app, AgentSectionFocus::Interactive, theme);
        render_agent_list_panel(
            frame,
            Some(sub),
            " interactive ",
            interactive_indices,
            app,
            INTERACTIVE_COLOR,
            border_style,
            theme,
        );
    }
    if let Some(sub) = take_top(&mut remaining, alloc[1]) {
        let border_style = agent_section_border_style(app, AgentSectionFocus::Terminal, theme);
        render_agent_list_panel(
            frame,
            Some(sub),
            " terminal ",
            terminal_indices,
            app,
            theme.success,
            border_style,
            theme,
        );
    }
    if let Some(sub) = take_top(&mut remaining, alloc[2]) {
        render_groups_panel(frame, Some(sub), app, AgentSectionFocus::Groups, theme);
    }

    remaining
}

fn draw_automation_body(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    background_indices: &[usize],
    theme: &Theme,
) -> Rect {
    let loop_count = app.sidebar_loops().len();
    let demands = [
        card_list_demand(background_indices.len()),
        card_list_demand(loop_count),
    ];
    let focused_idx = match app.automation_kind {
        AutomationKind::Agent => Some(0),
        AutomationKind::Loop => Some(1),
    };
    let alloc = fair_section_heights(&demands, area.height, focused_idx, 6);
    let mut remaining = area;

    if let Some(sub) = take_top(&mut remaining, alloc[0]) {
        let border_style = automation_agents_border_style(app, theme);
        render_agent_list_panel(
            frame,
            Some(sub),
            " agents ",
            background_indices,
            app,
            theme.header_color,
            border_style,
            theme,
        );
    }
    if let Some(sub) = take_top(&mut remaining, alloc[1]) {
        // The archived count is always shown here — even while browsing the
        // main list — so the archive is never an invisible state; see the
        // F4-archive spec's "always-visible count" requirement.
        let title = if app.loop_view_archived {
            format!(" archived loops ({}) ", app.archived_loop_count)
        } else if app.archived_loop_count > 0 {
            format!(" loops · {} archived ", app.archived_loop_count)
        } else {
            " loops ".to_string()
        };
        render_titled_panel(
            frame,
            sub,
            &title,
            Style::default().fg(theme.dim_text),
            automation_border_style(app, AutomationKind::Loop, theme),
            theme,
            |frame, inner| draw_automation_loops_list(frame, inner, app, theme),
        );
    }

    remaining
}

/// Rows needed for the projects panel: 3-row cards + 1-row gap + 2-row
/// border (see `draw_projects_list`), with a 3-row floor so the panel (and
/// its "No registered projects" message) still shows when the list is
/// empty. Like `card_list_demand`, a workspace with few projects leaves
/// rows over for the project graph and Brian's Brain below it (C26) —
/// unlike Knowledge's old behavior of always claiming the whole area,
/// which made the graph panel unreachable no matter how many relations
/// existed.
fn project_list_demand(count: usize) -> u16 {
    if count == 0 {
        3
    } else {
        count as u16 * 4 + 2
    }
}

fn draw_knowledge_body(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) -> Rect {
    let mut remaining = area;
    if let Some(sub) = take_top(&mut remaining, project_list_demand(app.projects.len())) {
        render_titled_panel(
            frame,
            sub,
            " projects ",
            Style::default().fg(theme.dim_text),
            knowledge_border_style(app, theme),
            theme,
            |frame, inner| draw_projects_list(frame, inner, app, theme),
        );
    }

    remaining
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

fn rag_border_style(app: &App, theme: &Theme) -> Style {
    Style::default().fg(if is_rag_focused(app) {
        theme.header_color
    } else {
        theme.border_color
    })
}

fn knowledge_border_style(app: &App, theme: &Theme) -> Style {
    let focused = layer_focused(app, SidebarLayer::Knowledge);
    Style::default().fg(if focused {
        theme.header_color
    } else {
        theme.border_color
    })
}

fn automation_border_style(app: &App, kind: AutomationKind, theme: &Theme) -> Style {
    let focused = layer_focused(app, SidebarLayer::Automation) && app.automation_kind == kind;
    Style::default().fg(if focused {
        theme.header_color
    } else {
        theme.border_color
    })
}

fn agent_section_border_style(app: &App, section: AgentSectionFocus, theme: &Theme) -> Style {
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

    Style::default().fg(if focused {
        theme.header_color
    } else {
        theme.border_color
    })
}

/// Border style for the Automation layer's `agents` sub-panel — distinct
/// from `agent_section_border_style` (Live's Interactive/Terminal/Groups)
/// since Automation tracks its active sub-list via `automation_kind`.
fn automation_agents_border_style(app: &App, theme: &Theme) -> Style {
    automation_border_style(app, AutomationKind::Agent, theme)
}

#[allow(clippy::too_many_arguments)]
fn render_agent_list_panel(
    frame: &mut Frame,
    area: Option<Rect>,
    title: &str,
    indices: &[usize],
    app: &mut App,
    accent: Color,
    border_style: Style,
    theme: &Theme,
) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        title,
        Style::default().fg(theme.dim_text),
        border_style,
        theme,
        |frame, inner| draw_agent_list(frame, inner, indices, app, accent, theme),
    );
}

fn render_groups_panel(
    frame: &mut Frame,
    area: Option<Rect>,
    app: &mut App,
    section: AgentSectionFocus,
    theme: &Theme,
) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        " groups ",
        Style::default().fg(theme.dim_text),
        agent_section_border_style(app, section, theme),
        theme,
        |frame, inner| draw_groups_list(frame, inner, app, theme),
    );
}

// ── Knowledge layer: projects list ──────────────────────────────────

fn draw_projects_list(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    if app.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No registered projects",
                Style::default().fg(theme.muted_text),
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
            theme,
        );
        app.project_click_map.push((*idx, y, y + 3));
        y += row_h;
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down, theme);
}

fn knowledge_border_style_is_focused(app: &App) -> bool {
    layer_focused(app, SidebarLayer::Knowledge)
}

#[allow(clippy::too_many_arguments)]
fn draw_project_loop_card(
    frame: &mut Frame,
    area: Rect,
    selected: bool,
    title: &str,
    meta1: &str,
    meta2: &str,
    panel_focused: bool,
    theme: &Theme,
) {
    let bg = if selected {
        theme.selected_bg
    } else {
        Color::Reset
    };
    let title_style = project_title_style(selected, panel_focused, theme);
    let meta_style = project_meta_style(selected, theme);
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

/// Status icon shown on a loop's card. Every one of the five statuses gets
/// its own icon/color pair so a listed loop is identifiable at a glance
/// without hiding any of them (running/paused/draft/failed/completed are
/// listed side by side now that the sidebar no longer filters by status);
/// `blocked` is a `Paused` sub-state (its latest run recorded a
/// `loop_report_blocker` description) that borrows `Failed`'s color to flag
/// it needs the same attention, distinguished from `Failed` by icon.
fn loop_status_icon(lp: &Loop, meta: &LoopSidebarMeta, theme: &Theme) -> (&'static str, Color) {
    match lp.status {
        LoopStatus::Running => ("▶", STATUS_RUNNING),
        LoopStatus::Paused if meta.blocked => ("⛔", STATUS_FAIL),
        LoopStatus::Paused => ("⏸", theme.warning),
        LoopStatus::Draft => ("○", theme.dim_text),
        LoopStatus::Completed => ("✓", STATUS_OK),
        LoopStatus::Failed => ("✗", STATUS_FAIL),
    }
}

fn draw_active_loop_card(
    frame: &mut Frame,
    area: Rect,
    selected: bool,
    lp: &Loop,
    meta: &LoopSidebarMeta,
    panel_focused: bool,
    theme: &Theme,
) {
    let bg = if selected {
        theme.selected_bg
    } else {
        Color::Reset
    };
    let title_style = project_title_style(selected, panel_focused, theme);
    let meta_style = project_meta_style(selected, theme);
    let (icon, icon_color) = loop_status_icon(lp, meta, theme);

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

    let last_run_style = if lp.status == LoopStatus::Running {
        Style::default().fg(STATUS_RUNNING)
    } else {
        meta_style
    };
    let last_run_text = match &meta.autorun_label {
        Some(autorun) => format!("{} · {autorun}", meta.last_run_label),
        None => meta.last_run_label.clone(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&last_run_text, area.width as usize),
            last_run_style,
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

fn draw_automation_loops_list(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let loops = app.sidebar_loops();
    if loops.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No loops",
                Style::default().fg(theme.muted_text),
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
                .cloned()
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
        draw_active_loop_card(frame, card_area, selected, lp, meta, panel_focused, theme);
        app.automation_loop_click_map.push((id.clone(), y, y + 3));
        y += row_h;
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down, theme);
}

fn project_title_style(selected: bool, panel_focused: bool, theme: &Theme) -> Style {
    if selected && panel_focused {
        return Style::default()
            .fg(theme.accent_fg)
            .bg(theme.header_color)
            .add_modifier(Modifier::BOLD);
    }
    if selected {
        return Style::default()
            .fg(theme.text_primary)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD);
    }
    Style::default()
        .fg(theme.header_color)
        .add_modifier(Modifier::BOLD)
}

fn project_meta_style(selected: bool, theme: &Theme) -> Style {
    if selected {
        Style::default()
            .fg(theme.text_primary)
            .bg(theme.selected_bg)
    } else {
        Style::default().fg(theme.dim_text)
    }
}

// ── RAG (pinned top) ─────────────────────────────────────────────────

fn draw_rag_queue(
    frame: &mut Frame,
    area: Rect,
    items: &[crate::db::project::RagQueueItem],
    scroll_pos: usize,
    theme: &Theme,
) {
    let mut y = area.y;
    for (idx, item) in items.iter().enumerate() {
        if y >= area.y + area.height {
            break;
        }
        let (icon, icon_color) = if item.status == "processing" {
            ("◉", theme.warning)
        } else {
            ("·", theme.header_color)
        };
        let is_cursor = idx == scroll_pos;
        let prefix = if is_cursor { "›" } else { " " };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, Style::default().fg(theme.header_color)),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::raw(" "),
                Span::styled(
                    truncate_str(&item.source_path, area.width.saturating_sub(3) as usize),
                    Style::default().fg(theme.text_primary),
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
                    Style::default().fg(theme.dim_text),
                ))),
                Rect::new(area.x + 2, y, area.width.saturating_sub(2), 1),
            );
            y += 1;
        }
    }
}

fn draw_rag_info(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let mut lines = vec![
        labeled_kv_line(" chunks: ", &app.rag_info.total_chunks.to_string(), theme),
        labeled_kv_line(" files:  ", &app.rag_info.indexed_files.to_string(), theme),
    ];

    let queue_text = rag_queue_text(app);
    if !queue_text.is_empty() {
        lines.push(labeled_kv_line(" queue:  ", &queue_text, theme));
    }

    lines.push(rag_status_line(app, theme));
    lines.push(Line::from(Span::styled(
        " Enter → playground ",
        Style::default().fg(theme.header_color),
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
            theme,
        );
    }
}

fn labeled_kv_line(label: &'static str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(label, Style::default().fg(theme.dim_text)),
        Span::styled(
            value.to_string(),
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn rag_status_line(app: &App, theme: &Theme) -> Line<'static> {
    use crate::rag::status::{compute_rag_status, RagModelStatus};

    match compute_rag_status(
        &app.rag_embeddings_model,
        app.rag_paused,
        app.rag_model_loaded,
        app.rag_info.processing_items,
        app.rag_acquisition_state.clone(),
    ) {
        RagModelStatus::Unavailable(_) => Line::from(Span::styled(
            " ✗ unavailable ",
            Style::default().fg(theme.error),
        )),
        RagModelStatus::DownloadFailed(_) => Line::from(Span::styled(
            " ✗ download failed ",
            Style::default().fg(theme.error),
        )),
        RagModelStatus::Downloading { .. } => Line::from(Span::styled(
            " ⬇ downloading ",
            Style::default().fg(theme.warning),
        )),
        RagModelStatus::Preparing { .. } => Line::from(Span::styled(
            " ⚙ preparing ",
            Style::default().fg(theme.warning),
        )),
        RagModelStatus::Paused => Line::from(Span::styled(
            " ⏸ paused ",
            Style::default().fg(theme.warning),
        )),
        RagModelStatus::Ready if app.rag_info.processing_items > 0 => Line::from(Span::styled(
            " ◉ indexing ",
            Style::default().fg(theme.warning),
        )),
        RagModelStatus::Ready => Line::from(Span::styled(
            " ● ready ",
            Style::default().fg(theme.header_color),
        )),
        RagModelStatus::Sleeping => Line::from(Span::styled(
            " ○ sleeping ",
            Style::default().fg(theme.dim_text),
        )),
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

fn draw_agent_list(
    frame: &mut Frame,
    area: Rect,
    indices: &[usize],
    app: &mut App,
    accent: Color,
    theme: &Theme,
) {
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
            theme,
        );
        app.sidebar_click_map.push((idx, y, y + card_h));

        let is_last_visible = scroll.start + rel_i >= indices.len() - 1;
        y += if is_last_visible { card_h } else { row_h };
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down, theme);
}

pub(crate) fn draw_scroll_indicators(
    frame: &mut Frame,
    area: Rect,
    has_up: bool,
    has_down: bool,
    theme: &Theme,
) {
    if has_up {
        frame.render_widget(
            Paragraph::new("▲").style(Style::default().fg(theme.dim_text)),
            Rect::new(area.x + area.width.saturating_sub(2), area.y, 1, 1),
        );
    }
    if has_down {
        frame.render_widget(
            Paragraph::new("▼").style(Style::default().fg(theme.dim_text)),
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

#[allow(clippy::too_many_arguments)]
fn draw_sidebar_card(
    frame: &mut Frame,
    area: Rect,
    agent: &AgentEntry,
    app: &App,
    selected: bool,
    hovered: bool,
    _accent: Color,
    theme: &Theme,
) {
    let meta = agent_card_meta(agent, app, theme);
    let status_color = effective_status_color(meta.status_color, agent, app, selected);
    let bg = if selected {
        theme.selected_bg
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
            .fg(if selected {
                meta.accent
            } else {
                theme.text_primary
            }),
    )];
    if is_agent_in_group(name, app) {
        name_spans.push(Span::styled(" [▣]", Style::default().fg(theme.dim_text)));
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
        vec![Span::styled(
            type_detail,
            Style::default().fg(theme.dim_text),
        )],
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
        vec![Span::styled(dir_text, Style::default().fg(theme.dim_text))],
    );
}

fn agent_card_meta<'a>(agent: &'a AgentEntry, app: &'a App, theme: &Theme) -> AgentCardMeta<'a> {
    match agent {
        AgentEntry::Agent(agent) => AgentCardMeta {
            accent: theme.header_color,
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
                accent: theme.muted_text,
                status_color: STATUS_FAIL,
                agent_type: "orphan",
                type_detail: session.cli.as_str(),
                work_dir: Some(session.working_dir.as_str()),
            }
        }
        AgentEntry::Group(_) => AgentCardMeta {
            accent: theme.header_color,
            status_color: STATUS_OK,
            agent_type: "group",
            type_detail: "",
            work_dir: None,
        },
        AgentEntry::Corrupt(_) => AgentCardMeta {
            accent: theme.error,
            status_color: STATUS_FAIL,
            agent_type: "corrupt config",
            type_detail: "",
            work_dir: None,
        },
    }
}

/// Status color for an interactive/terminal session card. A PTY is either
/// alive (green) or dead — and dead now splits into two: a session that
/// exited 0 reads as successful (`STATUS_OK`, blue), any other exit code
/// reads as failed (`STATUS_FAIL`, red). A running session pulses between
/// dim and bright green while it's registering activity (see
/// `ACTIVITY_IDLE_THRESHOLD_MS` and `pulse_active` — never blank, B21) and
/// holds solid green — "healthy, available" — once output has been quiet
/// for a while.
///
/// "Exit 0 means success" is scoped to *this* rendering decision only. It's
/// a safe read for a user-ended interactive session, but it is not a
/// general-purpose success signal — this project has been bitten before by
/// CLIs that print an error and still exit 0. Do not lift this assumption
/// into exit-code handling elsewhere without re-litigating it there.
fn session_status_color(status: &AgentStatus, pulsing: bool, animation_tick: u32) -> Color {
    match status {
        AgentStatus::Running if pulsing => pulse_active(animation_tick),
        AgentStatus::Running => STATUS_RUNNING,
        AgentStatus::Exited(0) => STATUS_OK,
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

fn group_row_style(is_selected: bool, is_active: bool, theme: &Theme) -> GroupRowStyle {
    GroupRowStyle {
        bg: if is_selected {
            theme.selected_bg
        } else {
            Color::Reset
        },
        fg: if is_selected {
            theme.header_color
        } else if is_active {
            theme.success
        } else {
            theme.text_primary
        },
        modifier: if is_active || is_selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        },
        prefix_color: if is_active {
            theme.success
        } else {
            theme.dim_text
        },
        active_tag: if is_active { " ●" } else { "" },
    }
}

fn draw_groups_list(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
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
        let style = group_row_style(is_selected, is_active, theme);
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

/// How many edge lines fit in the panel's inner area. `inner_height` is
/// already the post-border rect `render_titled_panel` hands to its
/// callback — budget it directly rather than subtracting border rows a
/// second time, which used to compute 0 at the panel's minimum height.
fn graph_edge_row_budget(inner_height: u16, edge_count: usize) -> usize {
    edge_count.min(inner_height as usize)
}

fn draw_project_graph(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if app.project_graph_trees.is_empty() || app.project_graph_edges.is_empty() {
        let msg = if app.projects.len() <= 1 {
            "No relationships yet. Press Enter on a project to link."
        } else {
            "No relationships yet. Press Enter to link projects."
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.muted_text),
            ))),
            area,
        );
        return;
    }

    let edge_count = graph_edge_row_budget(area.height, app.project_graph_edges.len());

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
                // THEME-EXEMPT: single-use graph-edge cyan — not a recurring role.
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
    theme: &Theme,
) {
    // Center the dialog in the screen area
    let dialog_w = 50u16.min(frame.area().width.saturating_sub(4));
    let dialog_h = 14u16.min(frame.area().height.saturating_sub(2));
    let x = frame.area().x + (frame.area().width.saturating_sub(dialog_w)) / 2;
    let y = frame.area().y + (frame.area().height.saturating_sub(dialog_h)) / 2;
    let area = Rect::new(x, y, dialog_w, dialog_h);

    let block = Block::default()
        .title(format!(" Link: {} ", dialog.from_name))
        .borders(borders_for(theme))
        .border_style(Style::default().fg(theme.header_color));
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
            Style::default()
                .fg(theme.header_color)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    if let Some(ref error) = dialog.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_str(error, inner.width as usize),
                Style::default().fg(theme.error),
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
            Style::default().fg(theme.warning),
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
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted_text)
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
                Style::default().fg(theme.muted_text),
            ))),
            Rect::new(inner.x, list_start_y, inner.width, 1),
        );
    }

    // Footer
    let footer_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " Enter confirm  ·  ←→ relation  ·  Esc cancel  ·  type filter ",
            Style::default().fg(theme.dim_text),
        ))),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn needed_agents(count: u16) -> u16 {
        count * 4 + 2
    }

    /// Builds an App backed by a fresh temp DB with `project_count` registered
    /// projects and one loop named "Probe Loop", then renders the sidebar into
    /// a `width`x`height` TestBackend and returns the screen contents as a
    /// flat string for substring assertions. The `Live` tab is active by
    /// default (matching `App::new`) — use `render_sidebar_text_on_tab` to
    /// assert on Automation's or Knowledge's body, since only the active
    /// tab renders.
    fn render_sidebar_text(project_count: usize, width: u16, height: u16) -> String {
        render_sidebar_text_themed(project_count, width, height, &Theme::classic())
    }

    fn render_sidebar_text_themed(
        project_count: usize,
        width: u16,
        height: u16,
        theme: &Theme,
    ) -> String {
        render_sidebar_text_on_tab(project_count, width, height, theme, SidebarLayer::Live)
    }

    fn render_sidebar_text_on_tab(
        project_count: usize,
        width: u16,
        height: u16,
        theme: &Theme,
        active_layer: SidebarLayer,
    ) -> String {
        render_sidebar_text_with(project_count, width, height, theme, active_layer, |_| {})
    }

    /// Like [`render_sidebar_text_on_tab`], but runs `prepare` on the App after
    /// it is built and before the sidebar is drawn — for state the constructor
    /// does not set up, such as an installed Brian's Brain.
    fn render_sidebar_text_with(
        project_count: usize,
        width: u16,
        height: u16,
        theme: &Theme,
        active_layer: SidebarLayer,
        prepare: impl FnOnce(&mut App),
    ) -> String {
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
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
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
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.sidebar_layer = active_layer;
        assert!(
            !app.sidebar_loops().is_empty(),
            "loop should be loaded from db"
        );
        prepare(&mut app);

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, theme);
            })
            .unwrap();

        buffer_to_text(terminal.backend().buffer())
    }

    /// Flattens a rendered `TestBackend` buffer into a plain string (row by
    /// row, no trailing per-cell styling) for substring assertions.
    fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    /// A brain whose every cell is On, so its glyphs are deterministic instead
    /// of depending on the automaton's random seed.
    fn solid_brain(rows: usize, cols: usize) -> crate::tui::brians_brain::BriansBrain {
        use crate::tui::brians_brain::CellState;
        let mut brain = crate::tui::brians_brain::BriansBrain::new(rows, cols, 60);
        for row in brain.grid.iter_mut() {
            for cell in row.iter_mut() {
                *cell = CellState::On;
            }
        }
        for row in brain.green_grid.iter_mut() {
            for green in row.iter_mut() {
                *green = 255;
            }
        }
        brain
    }

    /// The active tab's sub-sections are each capped at their own demand, so a
    /// tall sidebar holding one loop and no sessions leaves rows unclaimed.
    /// Those rows belong to the brain. `draw_sidebar_tabs` used to hand back a
    /// hardcoded zero-height rect instead, which made the brain unreachable no
    /// matter how `split_brain_or_graph` later divided it.
    #[test]
    fn unclaimed_tab_rows_are_handed_to_the_brain() {
        let text = render_sidebar_text_with(
            0,
            30,
            60,
            &Theme::classic(),
            SidebarLayer::Automation,
            |app| app.sidebar_brain = Some(solid_brain(40, 28)),
        );

        assert!(
            text.contains('\u{2588}') || text.contains('\u{28ff}'),
            "expected Brian's Brain glyphs in the sidebar's unclaimed rows, got:\n{text}"
        );
    }

    #[test]
    fn tab_bar_renders_every_layer_label() {
        // Wide enough that no tab cell needs to shorten its label to fit
        // (see `tab_cell_text`'s own narrow-width tests below).
        let text = render_sidebar_text(2, 45, 40);
        assert!(text.contains("Live"), "expected Live tab label");
        assert!(text.contains("Automation"), "expected Automation tab label");
        assert!(text.contains("Knowledge"), "expected Knowledge tab label");
        assert!(
            !text.contains("Knowledge (2)"),
            "tabs carry the label alone, no item count: {text}"
        );
    }

    #[test]
    fn every_tab_label_fits_uncut_at_the_real_sidebar_width() {
        // The reason the count is gone: 33 columns / 3 tabs leaves 11 per
        // cell, which fits "Automation" and "Knowledge" but not either of
        // them followed by a count.
        let cell = super::super::SIDEBAR_WIDTH as usize / SIDEBAR_TABS.len();
        for layer in SIDEBAR_TABS {
            let label = layer_label(layer);
            assert!(
                !tab_cell_text(label, cell).contains('…'),
                "{label} must not be truncated at {cell} columns"
            );
        }
    }

    #[test]
    fn automation_layer_shows_running_loop() {
        let text =
            render_sidebar_text_on_tab(1, 45, 40, &Theme::classic(), SidebarLayer::Automation);
        assert!(text.contains("Probe Loop"), "expected loop name visible");
    }

    // ── Last-run sidebar meta (replaces the old done/total spec count) ──

    /// Builds an App around a caller-supplied `Loop` plus whatever the `seed`
    /// closure inserts into the same DB, renders the Automation tab into a
    /// TestBackend, and returns the screen text. Unlike
    /// `render_sidebar_text_on_tab` (a fixed `Running`, spec-less "Probe
    /// Loop"), this lets each last-run test control the loop's status and
    /// `loop_runs` history directly.
    fn render_automation_text_for(
        lp: &crate::domain::loops::Loop,
        seed: impl FnOnce(&crate::db::Database),
    ) -> String {
        use crate::db::Database;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        db.insert_loop(lp).unwrap();
        seed(&db);

        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.sidebar_layer = SidebarLayer::Automation;

        let backend = TestBackend::new(50, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, &Theme::classic());
            })
            .unwrap();

        buffer_to_text(terminal.backend().buffer())
    }

    fn bare_loop(id: &str, status: LoopStatus) -> Loop {
        Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
            id: id.to_string(),
            name: format!("Loop {id}"),
            description: None,
            workdir: "/tmp/probe".to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    /// Records one node run against `loop_id` via a queue-driven spec — the
    /// spec's own `loop_id` is `None` (never bound to this loop, so
    /// `list_loop_specs(loop_id)` stays empty, exactly like a queue-driven
    /// run), but `loop_runs.loop_id` is set, which is what the sidebar's
    /// last-run query reads.
    fn seed_queue_driven_run(
        db: &crate::db::Database,
        loop_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        output: Option<serde_json::Value>,
    ) {
        use crate::domain::loops::{
            LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus,
        };

        let spec_id = format!("{loop_id}-spec");
        let node_id = format!("{loop_id}-node");
        db.insert_loop_spec(&LoopSpec {
            id: spec_id.clone(),
            loop_id: None,
            name: "queued spec".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Completed,
            started_at: Some(started_at),
            completed_at: Some(started_at),
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: node_id.clone(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "node".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 0,
            created_at: started_at,
        })
        .unwrap();
        db.insert_loop_run(&LoopNodeRun {
            id: format!("{loop_id}-run"),
            loop_id: loop_id.to_string(),
            spec_id,
            node_id,
            status: LoopRunStatus::Pass,
            input: None,
            output,
            started_at,
            completed_at: Some(started_at),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();
    }

    #[test]
    fn queue_driven_loop_shows_last_run_time_not_zero_zero() {
        let started_at = chrono::Utc::now() - chrono::Duration::minutes(2);
        let text = render_automation_text_for(&bare_loop("q1", LoopStatus::Draft), |db| {
            seed_queue_driven_run(db, "q1", started_at, None);
        });
        assert!(
            !text.contains("0/0"),
            "queue-driven loop must not fall back to the old done/total count: {text}"
        );
        assert!(
            text.contains("2m"),
            "expected the last run's relative time (2m) in the sidebar: {text}"
        );
    }

    #[test]
    fn never_run_loop_shows_plainly_not_blank_or_zero() {
        let text = render_automation_text_for(&bare_loop("never1", LoopStatus::Draft), |_db| {});
        assert!(
            !text.contains("0/0"),
            "a never-run loop must not show a misleading zero: {text}"
        );
        assert!(
            text.contains("never"),
            "a never-run loop must say so plainly: {text}"
        );
    }

    /// Scans row `y` for the first cell matching one of `loop_status_icon`'s
    /// glyphs, returning its `(symbol, fg color)` — used to check that every
    /// status renders a visually distinct card without depending on the
    /// inner panel's exact border offset.
    fn status_icon_cell(buffer: &ratatui::buffer::Buffer, y: u16) -> (String, Color) {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if matches!(cell.symbol(), "▶" | "⏸" | "⛔" | "○" | "✓" | "✗") {
                return (cell.symbol().to_string(), cell.fg);
            }
        }
        panic!("no status icon glyph found in row {y}");
    }

    #[test]
    fn sidebar_lists_every_status_ordered_by_recency_and_visually_distinguishable() {
        use crate::db::Database;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());

        // Staggered `created_at`, oldest first, so recency ordering is
        // unambiguous once sorted (none of these loops have run, so
        // `last_activity` falls back to `created_at`).
        let base = chrono::Utc::now() - chrono::Duration::hours(10);
        let mut draft = bare_loop("s-draft", LoopStatus::Draft);
        draft.created_at = base;
        db.insert_loop(&draft).unwrap();

        let mut failed = bare_loop("s-failed", LoopStatus::Failed);
        failed.created_at = base + chrono::Duration::minutes(10);
        failed.autorun_at = Some(chrono::Utc::now() + chrono::Duration::minutes(20));
        db.insert_loop(&failed).unwrap();

        let mut completed = bare_loop("s-completed", LoopStatus::Completed);
        completed.created_at = base + chrono::Duration::minutes(20);
        db.insert_loop(&completed).unwrap();

        let mut paused = bare_loop("s-paused", LoopStatus::Paused);
        paused.created_at = base + chrono::Duration::minutes(30);
        db.insert_loop(&paused).unwrap();

        let mut running = bare_loop("s-running", LoopStatus::Running);
        running.created_at = base + chrono::Duration::minutes(40);
        db.insert_loop(&running).unwrap();

        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.sidebar_layer = SidebarLayer::Automation;
        app.automation_kind = AutomationKind::Loop;

        let theme = Theme::classic();
        let backend = TestBackend::new(50, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = buffer_to_text(&buffer);

        for name in [
            "Loop s-draft",
            "Loop s-failed",
            "Loop s-completed",
            "Loop s-paused",
            "Loop s-running",
        ] {
            assert!(text.contains(name), "expected {name} listed: {text}");
        }

        let click_map = app.automation_loop_click_map.clone();
        let ids: Vec<&str> = click_map.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "s-running",
                "s-paused",
                "s-completed",
                "s-failed",
                "s-draft",
            ],
            "every status must be listed, ordered by last activity most recent first"
        );

        let icons: Vec<(String, Color)> = click_map
            .iter()
            .map(|(_, y_start, _)| status_icon_cell(&buffer, *y_start))
            .collect();
        let unique: std::collections::HashSet<_> = icons.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            icons.len(),
            "each of the five statuses must render a distinct icon/color pair: {icons:?}"
        );

        assert!(
            text.contains("resumes"),
            "a pending autorun must be visible on its loop's sidebar entry: {text}"
        );
    }

    #[test]
    fn running_loop_shows_running_not_a_relative_time() {
        let started_at = chrono::Utc::now() - chrono::Duration::minutes(2);
        let text = render_automation_text_for(&bare_loop("run1", LoopStatus::Running), |db| {
            seed_queue_driven_run(db, "run1", started_at, None);
        });
        assert!(
            text.contains("running"),
            "an actively-running loop must say 'running', not its stale last-run time: {text}"
        );
    }

    #[test]
    fn blocked_indicator_still_renders_for_paused_loop_with_blocker() {
        let started_at = chrono::Utc::now() - chrono::Duration::minutes(5);
        let text = render_automation_text_for(&bare_loop("blocked1", LoopStatus::Paused), |db| {
            seed_queue_driven_run(
                db,
                "blocked1",
                started_at,
                Some(serde_json::json!({"blocker": "waiting on human input"})),
            );
        });
        assert!(
            text.contains('⛔'),
            "a paused loop with a reported blocker must still show the blocked icon: {text}"
        );
    }

    #[test]
    fn inactive_tabs_bodies_are_not_rendered() {
        // The whole point of tabs over stacked layers: exactly one body is
        // visible at a time. A project ("only-in-knowledge") and the
        // always-present "Probe Loop" (Automation) must not leak into the
        // Live tab's render, and vice versa.
        let live_text =
            render_sidebar_text_on_tab(1, 45, 40, &Theme::classic(), SidebarLayer::Live);
        assert!(
            !live_text.contains("Probe Loop"),
            "Automation's body must not render while Live is active"
        );
        assert!(
            !live_text.contains("project0"),
            "Knowledge's body must not render while Live is active"
        );

        let knowledge_text =
            render_sidebar_text_on_tab(1, 45, 40, &Theme::classic(), SidebarLayer::Knowledge);
        assert!(
            knowledge_text.contains("project0"),
            "Knowledge's body must render while Knowledge is active"
        );
        assert!(
            !knowledge_text.contains("Probe Loop"),
            "Automation's body must not render while Knowledge is active"
        );
    }

    #[test]
    fn modern_theme_drops_border_glyphs_where_classic_shows_them() {
        // T5: a modern-rendered sidebar must have no box-drawing border glyphs,
        // while the classic-rendered sidebar must still show them — the
        // background contrast alone is expected to separate panels.
        let border_glyphs = ['─', '│', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼'];
        let assert_no_borders = |label: &str, text: &str| {
            for glyph in border_glyphs {
                assert!(
                    !text.contains(glyph),
                    "{label} render unexpectedly contains border glyph {glyph:?}\n--- text ---\n{text}"
                );
            }
        };

        let classic_text = render_sidebar_text_themed(1, 45, 40, &Theme::classic());
        assert!(
            border_glyphs.iter().any(|g| classic_text.contains(*g)),
            "classic render should still draw box-drawing borders\n--- text ---\n{classic_text}"
        );

        let modern_text = render_sidebar_text_themed(1, 45, 40, &Theme::modern());
        assert_no_borders("modern", &modern_text);
        // And the labels that the modern sidebar still owes the user must
        // survive (sanity: we didn't accidentally turn the whole area blank).
        assert!(
            modern_text.contains("Live"),
            "modern must keep the Live label"
        );
        assert!(
            modern_text.contains("Automation"),
            "modern must keep the Automation label"
        );
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
        // Only the active tab's body renders now — Knowledge must be active
        // for its project rows to draw at all.
        app.sidebar_layer = SidebarLayer::Knowledge;

        let backend = TestBackend::new(34, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, &Theme::classic());
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

        let alloc = fair_section_heights(&demands, 30, None, 0);

        // No section is ever allocated more than it needs.
        for (got, want) in alloc.iter().zip(demands.iter()) {
            assert!(got <= want, "section {got} exceeded its demand {want}");
        }
        // The two small sections get exactly what they need (no empty gap).
        assert_eq!(alloc[0], demands[0]);
        assert_eq!(alloc[2], demands[2]);
        // Interactive absorbs all the surplus and scrolls internally.
        assert_eq!(alloc[1], 30 - demands[0] - demands[2]);
        // Demand exceeds the budget here, so every row is spoken for and
        // nothing is left over for the brain.
        assert_eq!(alloc.iter().sum::<u16>(), 30);
    }

    #[test]
    fn fair_section_heights_never_exceeds_demand_with_four_sections() {
        let demands = [needed_agents(1), needed_agents(8), needed_agents(1), 4];
        let budget = 24;
        assert!(demands.iter().sum::<u16>() > budget);

        let alloc = fair_section_heights(&demands, budget, None, 0);

        for (got, want) in alloc.iter().zip(demands.iter()) {
            assert!(got <= want, "section {got} exceeded its demand {want}");
        }
        assert_eq!(alloc.iter().sum::<u16>(), budget);
    }

    #[test]
    fn fair_section_heights_fits_everyone_when_budget_is_ample() {
        let demands = [needed_agents(1), needed_agents(2)];
        let alloc = fair_section_heights(&demands, 100, None, 0);
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
    fn exit_clean_is_success_blue_regardless_of_activity() {
        assert_eq!(
            session_status_color(&AgentStatus::Exited(0), true, 0),
            STATUS_OK
        );
        assert_eq!(
            session_status_color(&AgentStatus::Exited(0), false, 0),
            STATUS_OK
        );
    }

    #[test]
    fn exit_nonzero_is_failure_red_regardless_of_activity() {
        assert_eq!(
            session_status_color(&AgentStatus::Exited(7), true, 0),
            STATUS_FAIL
        );
        assert_eq!(
            session_status_color(&AgentStatus::Exited(7), false, 0),
            STATUS_FAIL
        );
    }

    #[test]
    fn exited_0_and_exited_1_sidebar_cards_render_different_colors() {
        // The point of the fix is contrast: a clean exit and a failed exit
        // sitting side by side must be visually distinguishable, not just
        // individually "correct" in isolation.
        use crate::db::Database;
        use crate::tui::agent::InteractiveAgent;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();

        let mut ok_agent = InteractiveAgent::spawn_terminal(
            "cat",
            "/tmp",
            80,
            24,
            Some("ok-exit"),
            &[],
            ratatui::style::Color::White,
        )
        .expect("spawn ok-exit agent");
        ok_agent.status = AgentStatus::Exited(0);
        app.interactive_agents.push(ok_agent);

        let mut fail_agent = InteractiveAgent::spawn_terminal(
            "cat",
            "/tmp",
            80,
            24,
            Some("fail-exit"),
            &[],
            ratatui::style::Color::White,
        )
        .expect("spawn fail-exit agent");
        fail_agent.status = AgentStatus::Exited(1);
        app.interactive_agents.push(fail_agent);

        app.agents.push(AgentEntry::Interactive(0));
        app.agents.push(AgentEntry::Interactive(1));

        let theme = Theme::classic();
        let backend = TestBackend::new(50, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, &theme);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = buffer_to_text(&buffer);

        // Scan the row containing each agent's name for the "▌" status
        // gutter glyph, wherever the card's left border happens to sit.
        let gutter_color_for = |name: &str| -> Color {
            let y = text
                .lines()
                .position(|line| line.contains(name))
                .unwrap_or_else(|| panic!("expected {name} listed: {text}"));
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y as u16)];
                if cell.symbol() == "▌" {
                    return cell.fg;
                }
            }
            panic!("no status gutter glyph found on {name}'s row");
        };

        let ok_color = gutter_color_for("ok-exit");
        let fail_color = gutter_color_for("fail-exit");
        assert_eq!(ok_color, STATUS_OK, "clean exit must render success blue");
        assert_eq!(fail_color, STATUS_FAIL, "failed exit must render fail red");
        assert_ne!(
            ok_color, fail_color,
            "clean and failed exits must be visually distinguishable"
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
        // Exited status ignores the pulsing signal either way, but now
        // depends on the exit code: clean exit is blue, not red.
        assert_eq!(
            pty_session_status_color(false, &AgentStatus::Exited(0), true, false, 0),
            STATUS_OK
        );
        assert_eq!(
            pty_session_status_color(false, &AgentStatus::Exited(1), true, false, 0),
            STATUS_FAIL
        );
    }

    #[test]
    fn layer_label_all_variants() {
        assert_eq!(layer_label(SidebarLayer::Live), "Live");
        assert_eq!(layer_label(SidebarLayer::Automation), "Automation");
        assert_eq!(layer_label(SidebarLayer::Knowledge), "Knowledge");
    }

    #[test]
    fn tab_cell_text_centers_the_label_when_there_is_room() {
        // 20 columns, 10-char label → 5 columns of padding on each side.
        assert_eq!(tab_cell_text("Automation", 20), "     Automation     ");
    }

    #[test]
    fn tab_cell_text_odd_slack_gives_the_extra_column_to_the_right() {
        // 11 columns, 4-char label ("Live") → 7 slack columns, split 3/4.
        assert_eq!(tab_cell_text("Live", 11), "   Live    ");
    }

    #[test]
    fn tab_cell_text_truncates_a_label_too_wide_for_its_cell() {
        let text = tab_cell_text("Automation", 6);
        assert_eq!(text, "Autom…");
    }

    #[test]
    fn tab_cell_text_extreme_narrow_width_still_fits_exactly() {
        let text = tab_cell_text("Knowledge", 3);
        assert_eq!(text.chars().count(), 3);
    }

    #[test]
    fn draw_sidebar_tab_bar_registers_three_equal_click_cells_spanning_the_full_width() {
        use crate::db::Database;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::sync::Arc;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();

        let backend = TestBackend::new(33, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, &Theme::classic());
            })
            .unwrap();

        assert_eq!(app.sidebar_tab_click_map.len(), 3);
        let layers: Vec<SidebarLayer> = app
            .sidebar_tab_click_map
            .iter()
            .map(|&(layer, _, _, _)| layer)
            .collect();
        assert_eq!(
            layers,
            vec![
                SidebarLayer::Live,
                SidebarLayer::Automation,
                SidebarLayer::Knowledge
            ]
        );
        // The three cells must tile the row with no gap and no overlap.
        let first_col = app.sidebar_tab_click_map[0].2;
        let last_col_end = app.sidebar_tab_click_map[2].3;
        assert_eq!(last_col_end - first_col, 33);
        for pair in app.sidebar_tab_click_map.windows(2) {
            assert_eq!(pair[0].3, pair[1].2, "cells must be contiguous");
        }
    }

    #[test]
    fn draw_sidebar_tab_bar_marks_the_active_tab_with_accent_bold_and_no_fill() {
        use crate::db::Database;
        use crate::tui::app::App;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::sync::Arc;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.sidebar_layer = SidebarLayer::Live;

        let theme = Theme::classic();
        let backend = TestBackend::new(33, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_sidebar(frame, area, &mut app, &theme);
            })
            .unwrap();

        let (_, row, start, end) = app.sidebar_tab_click_map[0];
        let buffer = terminal.backend().buffer().clone();
        let mut saw_accent_glyph = false;
        for x in start..end {
            let cell = &buffer[(x, row)];
            assert_eq!(
                cell.bg,
                Color::Reset,
                "active tab cell must not fill its background"
            );
            if cell.symbol() != " " {
                assert_eq!(cell.fg, theme.header_color);
                assert!(cell.modifier.contains(Modifier::BOLD));
                saw_accent_glyph = true;
            }
        }
        assert!(saw_accent_glyph, "expected the active label to be drawn");

        let (_, row, start, end) = app.sidebar_tab_click_map[1];
        for x in start..end {
            let cell = &buffer[(x, row)];
            assert_eq!(cell.bg, Color::Reset);
            if cell.symbol() != " " {
                assert_eq!(cell.fg, theme.dim_text);
                assert!(!cell.modifier.contains(Modifier::BOLD));
            }
        }
    }

    #[test]
    fn fair_section_heights_empty() {
        assert_eq!(fair_section_heights(&[], 20, None, 0), Vec::<u16>::new());
    }

    #[test]
    fn fair_section_heights_single_section() {
        assert_eq!(fair_section_heights(&[10], 20, None, 0), vec![10]);
    }

    #[test]
    fn fair_section_heights_budget_exactly_matches_demand() {
        assert_eq!(fair_section_heights(&[5, 5, 5], 15, None, 0), vec![5, 5, 5]);
    }

    #[test]
    fn fair_section_heights_budget_exceeds_demand() {
        assert_eq!(fair_section_heights(&[3, 3], 20, None, 0), vec![3, 3]);
    }

    #[test]
    fn fair_section_heights_budget_falls_short() {
        let result = fair_section_heights(&[10, 10], 10, None, 0);
        assert_eq!(result.iter().sum::<u16>(), 10);
    }

    #[test]
    fn fair_section_heights_one_small_one_large() {
        let result = fair_section_heights(&[2, 20], 12, None, 0);
        assert_eq!(result[0], 2);
        assert_eq!(result[1], 10);
        assert_eq!(result.iter().sum::<u16>(), 12);
    }

    #[test]
    fn fair_section_heights_budget_zero() {
        assert_eq!(fair_section_heights(&[10, 10], 0, None, 0), vec![0, 0]);
    }

    #[test]
    fn fair_section_heights_many_sections() {
        let result = fair_section_heights(&[5, 5, 5, 5, 5], 10, None, 0);
        assert_eq!(result.iter().sum::<u16>(), 10);
        for &v in &result {
            assert!(v <= 5);
        }
    }

    #[test]
    fn fair_section_heights_focused_section_gets_floor_on_short_screen() {
        // Short screen: three sections, each wanting far more than a fair
        // share of the budget. Plain fair distribution splits it roughly
        // evenly and leaves every section below the floor needed to show
        // the cursor -- "I can navigate them but I can't see them".
        let demands = [needed_agents(5), needed_agents(5), needed_agents(5)];
        let budget = 15;

        // Precondition: without a focused section the section the user is
        // looking at is starved below the floor. If this ever stops being
        // true the test below no longer proves the floor does anything.
        let fair = fair_section_heights(&demands, budget, None, 0);
        assert!(
            fair[0] < 6,
            "precondition: fair split should starve the section, got {}",
            fair[0]
        );

        // Focusing that section guarantees it the floor before fair
        // distribution runs for the rest.
        let alloc = fair_section_heights(&demands, budget, Some(0), 6);
        assert!(
            alloc[0] >= 6,
            "focused section got {} rows, expected >= 6",
            alloc[0]
        );

        // Still within budget, still nobody over their demand.
        assert!(alloc.iter().sum::<u16>() <= budget);
        for (got, want) in alloc.iter().zip(demands.iter()) {
            assert!(got <= want, "section got {got} exceeded demand {want}");
        }
    }

    #[test]
    fn fair_section_heights_floor_respects_demand() {
        let demands = [4, 20, 20];
        let alloc = fair_section_heights(&demands, 30, Some(0), 6);
        assert_eq!(alloc[0], 4);
        assert!(alloc.iter().sum::<u16>() <= 30);
    }

    #[test]
    fn fair_section_heights_floor_respects_budget() {
        let demands = [20, 20];
        let alloc = fair_section_heights(&demands, 3, Some(0), 6);
        assert!(alloc[0] <= 3);
        assert!(alloc.iter().sum::<u16>() <= 3);
    }

    #[test]
    fn fair_section_heights_no_focus_unchanged() {
        // With no focused section and a zero floor the function must
        // behave exactly as it did before the floor was added: the small
        // sections cap at their demand, the large one takes the rest.
        let demands = [needed_agents(6), needed_agents(1), groups_list_demand(1)];
        assert_eq!(fair_section_heights(&demands, 20, None, 0), vec![10, 6, 4]);
    }

    #[test]
    fn fair_section_heights_focus_no_effect_when_budget_is_ample() {
        // Tall screen: everything fits. The focused-section floor must not
        // change what any section gets -- no visible difference from the
        // pre-floor behaviour on screens with sufficient height.
        let demands = [needed_agents(3), needed_agents(2), groups_list_demand(2)];
        let ample = demands.iter().sum::<u16>() + 10;
        assert_eq!(
            fair_section_heights(&demands, ample, Some(0), 6),
            fair_section_heights(&demands, ample, None, 0),
        );
        assert_eq!(
            fair_section_heights(&demands, ample, Some(0), 6),
            demands.to_vec(),
        );
    }

    #[test]
    fn card_list_demand_empty() {
        assert_eq!(card_list_demand(0), 0);
    }

    #[test]
    fn card_list_demand_one() {
        assert_eq!(card_list_demand(1), 6);
    }

    #[test]
    fn card_list_demand_many() {
        assert_eq!(card_list_demand(5), 22);
    }

    #[test]
    fn groups_list_demand_empty() {
        assert_eq!(groups_list_demand(0), 0);
    }

    #[test]
    fn groups_list_demand_one() {
        assert_eq!(groups_list_demand(1), 4);
    }

    #[test]
    fn groups_list_demand_many() {
        assert_eq!(groups_list_demand(3), 8);
    }

    #[test]
    fn scroll_state_no_items() {
        let s = scroll_state(0, None, 5);
        assert_eq!(s.start, 0);
        assert!(!s.has_up);
        assert!(!s.has_down);
    }

    #[test]
    fn scroll_state_fewer_items_than_visible() {
        let s = scroll_state(3, None, 10);
        assert_eq!(s.start, 0);
        assert!(!s.has_up);
        assert!(!s.has_down);
    }

    #[test]
    fn scroll_state_selected_below_visible() {
        let s = scroll_state(20, Some(15), 5);
        assert_eq!(s.start, 11);
        assert!(s.has_up);
        assert!(s.has_down);
    }

    #[test]
    fn scroll_state_selected_at_top() {
        let s = scroll_state(20, Some(0), 5);
        assert_eq!(s.start, 0);
        assert!(!s.has_up);
        assert!(s.has_down);
    }

    #[test]
    fn scroll_state_selected_at_end() {
        let s = scroll_state(20, Some(19), 5);
        assert_eq!(s.start, 15);
        assert!(s.has_up);
        assert!(!s.has_down);
    }

    #[test]
    fn scroll_state_with_offset_shifts_start() {
        let s = scroll_state_with_offset(20, Some(0), 5, 3);
        assert_eq!(s.start, 3);
        assert!(s.has_up);
    }

    #[test]
    fn scroll_state_with_offset_clamped_to_max() {
        let s = scroll_state_with_offset(20, Some(19), 5, 100);
        assert_eq!(s.start, 15);
    }

    #[test]
    fn render_sidebar_card_line_short_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(20, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, 1);
                render_sidebar_card_line(
                    frame,
                    area,
                    0,
                    Color::Reset,
                    STATUS_OK,
                    vec![Span::raw("test")],
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, 0)].symbol());
        }
        assert!(text.contains("test"));
    }

    #[test]
    fn render_sidebar_card_line_beyond_height() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(20, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 20, 2);
                // line_offset=2 is beyond height, should not panic
                render_sidebar_card_line(
                    frame,
                    area,
                    2,
                    Color::Reset,
                    STATUS_OK,
                    vec![Span::raw("test")],
                );
            })
            .unwrap();
    }

    #[test]
    fn group_row_style_selected_and_active() {
        let theme = Theme::classic();
        let style = group_row_style(true, true, &theme);
        assert_eq!(style.bg, theme.selected_bg);
        assert_eq!(style.fg, theme.header_color);
        assert!(style.modifier.contains(Modifier::BOLD));
        assert_eq!(style.prefix_color, Color::Green);
        assert_eq!(style.active_tag, " ●");
    }

    #[test]
    fn group_row_style_not_selected_not_active() {
        let theme = Theme::classic();
        let style = group_row_style(false, false, &theme);
        assert_eq!(style.bg, Color::Reset);
        assert_eq!(style.fg, Color::White);
        assert!(style.modifier.is_empty());
        assert_eq!(style.prefix_color, theme.dim_text);
        assert_eq!(style.active_tag, "");
    }

    #[test]
    fn group_row_style_selected_not_active() {
        let theme = Theme::classic();
        let style = group_row_style(true, false, &theme);
        assert_eq!(style.bg, theme.selected_bg);
        assert_eq!(style.fg, theme.header_color);
        assert!(style.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn group_row_style_active_not_selected() {
        let theme = Theme::classic();
        let style = group_row_style(false, true, &theme);
        assert_eq!(style.bg, Color::Reset);
        assert_eq!(style.fg, Color::Green);
        assert!(style.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn project_title_style_selected_and_focused() {
        let theme = Theme::classic();
        let style = project_title_style(true, true, &theme);
        assert_eq!(style.bg, Some(theme.header_color));
        assert_eq!(style.fg, Some(Color::Black));
    }

    #[test]
    fn project_title_style_selected_not_focused() {
        let theme = Theme::classic();
        let style = project_title_style(true, false, &theme);
        assert_eq!(style.bg, Some(theme.selected_bg));
        assert_eq!(style.fg, Some(Color::White));
    }

    #[test]
    fn project_title_style_not_selected() {
        let theme = Theme::classic();
        let style = project_title_style(false, false, &theme);
        assert_eq!(style.bg, None);
        assert_eq!(style.fg, Some(theme.header_color));
    }

    #[test]
    fn project_meta_style_selected() {
        let theme = Theme::classic();
        let style = project_meta_style(true, &theme);
        assert_eq!(style.fg, Some(Color::White));
        assert_eq!(style.bg, Some(theme.selected_bg));
    }

    #[test]
    fn project_meta_style_not_selected() {
        let theme = Theme::classic();
        let style = project_meta_style(false, &theme);
        assert_eq!(style.fg, Some(theme.dim_text));
    }

    #[test]
    fn rag_info_title_when_paused() {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.rag_paused = true;
        assert_eq!(rag_info_title(&app), " ragInfo ⏸ ");
    }

    #[test]
    fn rag_info_title_when_not_paused() {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.rag_paused = false;
        assert_eq!(rag_info_title(&app), " ragInfo ");
    }

    #[test]
    fn rag_queue_text_with_items() {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.rag_info.queued_items = 5;
        assert_eq!(rag_queue_text(&app), "5 queued");
    }

    #[test]
    fn rag_queue_text_zero_items() {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.rag_info.queued_items = 0;
        assert_eq!(rag_queue_text(&app), "");
    }

    #[test]
    fn pulse_active_cycles_through_phases() {
        let a = pulse_active(0);
        let b = pulse_active(10);
        assert_ne!(a, b);
        assert_ne!(a, super::super::STATUS_WAIT_OFF);
        assert_ne!(b, super::super::STATUS_WAIT_OFF);
    }

    #[test]
    fn pulse_cycles_through_phases() {
        let a = pulse(Color::Red, 0);
        let b = pulse(Color::Red, 10);
        assert_ne!(a, b);
    }

    #[test]
    fn draw_sidebar_renders_without_panic() {
        use crate::db::Database;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        let backend = TestBackend::new(33, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_sidebar(frame, frame.area(), &mut app, &Theme::classic());
            })
            .unwrap();
    }

    #[test]
    fn draw_sidebar_modern_theme_no_panic() {
        use crate::db::Database;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        let backend = TestBackend::new(33, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_sidebar(frame, frame.area(), &mut app, &Theme::modern());
            })
            .unwrap();
    }

    #[test]
    fn rag_status_line_shows_downloading_not_ready_even_when_loaded_flag_is_stale() {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.rag_embeddings_model = "text-embedding-3-small".to_string();
        app.rag_model_loaded = true;
        app.rag_acquisition_state =
            Some(crate::rag::status::AcquisitionState::Downloading { started_at: 0 });

        let theme = Theme::classic();
        let line = rag_status_line(&app, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("downloading"), "got: {text:?}");
    }

    #[test]
    fn rag_status_line_shows_download_failed() {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Arc::clone(&db), data_dir.path()).unwrap();
        app.rag_embeddings_model = "text-embedding-3-small".to_string();
        app.rag_acquisition_state = Some(crate::rag::status::AcquisitionState::Failed {
            reason: "boom".to_string(),
        });

        let theme = Theme::classic();
        let line = rag_status_line(&app, &theme);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("download failed"), "got: {text:?}");
    }

    #[test]
    fn split_shares_space_when_both_fit() {
        let area = Rect::new(0, 0, 30, 20);
        match split_brain_or_graph(area, true) {
            BrainOrGraphLayout::Both { graph, brain } => {
                assert!(graph.height >= GRAPH_MIN_HEIGHT, "graph below its minimum");
                assert!(brain.height >= 3, "brain below its minimum");
                assert_eq!(
                    graph.height + brain.height,
                    area.height,
                    "the two sub-areas should cover the whole leftover area"
                );
                assert_eq!(
                    brain.y,
                    graph.y + graph.height,
                    "brain should sit below the graph"
                );
            }
            other => panic!("expected Both, got {other:?}"),
        }
    }

    #[test]
    fn split_gives_graph_alone_when_only_it_fits() {
        // Tall enough for the graph (>=4) but not for graph + brain (>=7).
        let area = Rect::new(0, 0, 30, 5);
        match split_brain_or_graph(area, true) {
            BrainOrGraphLayout::GraphOnly(graph) => assert_eq!(graph, area),
            other => panic!("expected GraphOnly, got {other:?}"),
        }
    }

    #[test]
    fn split_gives_brain_alone_when_graph_is_empty() {
        // Below the graph's own minimum, but that's moot since it has nothing to show.
        let area = Rect::new(0, 0, 30, 3);
        match split_brain_or_graph(area, false) {
            BrainOrGraphLayout::BrainOnly(brain) => assert_eq!(brain, area),
            other => panic!("expected BrainOnly, got {other:?}"),
        }
    }

    #[test]
    fn split_yields_neither_below_both_minimums() {
        let area = Rect::new(0, 0, 30, 2);
        assert_eq!(
            split_brain_or_graph(area, true),
            BrainOrGraphLayout::Neither
        );
    }

    #[test]
    fn split_yields_neither_on_zero_height() {
        let area = Rect::new(0, 0, 30, 0);
        assert_eq!(
            split_brain_or_graph(area, true),
            BrainOrGraphLayout::Neither
        );
        assert_eq!(
            split_brain_or_graph(area, false),
            BrainOrGraphLayout::Neither
        );
    }

    // ── C26: graph_has_content / graph_edge_row_budget ────────────────

    #[test]
    fn graph_has_content_regression_defect_1_trees_but_no_edges() {
        // 39 singleton trees, 0 edges: `project_graph_trees` is never empty
        // (defect 1's false-positive signal), but with no edges there's
        // nothing to draw, so content must read false and the split must
        // hand the whole area to the brain.
        assert!(!graph_has_content(0, true));

        let area = Rect::new(0, 0, 30, 20);
        match split_brain_or_graph(area, graph_has_content(0, true)) {
            BrainOrGraphLayout::BrainOnly(brain) => assert_eq!(brain, area),
            other => panic!("expected BrainOnly, got {other:?}"),
        }
    }

    #[test]
    fn graph_has_content_true_with_at_least_one_edge_on_knowledge_tab() {
        assert!(graph_has_content(1, true));
    }

    #[test]
    fn graph_has_content_gated_to_knowledge_layer() {
        // Same edge count, same area — only the tab differs.
        let edge_count = 3;
        assert!(
            graph_has_content(edge_count, true),
            "Knowledge should show it"
        );
        assert!(
            !graph_has_content(edge_count, false),
            "Live/Automation should not show it"
        );
    }

    #[test]
    fn graph_edge_row_budget_regression_defect_2() {
        // GRAPH_MIN_HEIGHT is the *outer* height the split hands the panel;
        // render_titled_panel strips Borders::ALL's 2 border rows before
        // draw_project_graph ever sees the area. The old code subtracted
        // another 2 from that already-inner height, computing 0 for a
        // panel that has exactly one edge to show.
        let inner_height = GRAPH_MIN_HEIGHT - 2;
        assert_eq!(graph_edge_row_budget(inner_height, 1), 1);
    }

    #[test]
    fn graph_edge_row_budget_truncates_to_available_rows() {
        assert_eq!(graph_edge_row_budget(2, 5), 2);
    }

    #[test]
    fn graph_edge_row_budget_draws_all_edges_when_rows_have_slack() {
        assert_eq!(graph_edge_row_budget(10, 3), 3);
    }

    #[test]
    fn knowledge_tab_with_an_edge_draws_the_graph_panel() {
        let text = render_sidebar_text_with(
            2,
            34,
            40,
            &Theme::classic(),
            SidebarLayer::Knowledge,
            |app| {
                app.project_graph_edges
                    .push(crate::tui::app::types::ProjectGraphEdge {
                        from_name: "project0".to_string(),
                        to_name: "project1".to_string(),
                        from_hash: "hash0".to_string(),
                        to_hash: "hash1".to_string(),
                        relation: "depends_on".to_string(),
                    });
            },
        );
        assert!(
            text.contains("project graph"),
            "graph panel title missing: {text}"
        );
        assert!(
            text.contains("project0") && text.contains("project1"),
            "edge label missing: {text}"
        );
    }

    #[test]
    fn live_tab_never_shows_the_graph_panel_even_with_edges() {
        let text =
            render_sidebar_text_with(2, 34, 40, &Theme::classic(), SidebarLayer::Live, |app| {
                app.project_graph_edges
                    .push(crate::tui::app::types::ProjectGraphEdge {
                        from_name: "project0".to_string(),
                        to_name: "project1".to_string(),
                        from_hash: "hash0".to_string(),
                        to_hash: "hash1".to_string(),
                        relation: "depends_on".to_string(),
                    });
            });
        assert!(
            !text.contains("project graph"),
            "graph panel must not render on Live: {text}"
        );
    }

    #[test]
    fn registered_projects_with_zero_relations_do_not_claim_graph_height() {
        // Spec CB17: registered projects with zero relations make
        // `refresh_project_graph` fill `trees` with one singleton per project
        // while leaving `edges` empty. The old `!trees.is_empty()` height-claim
        // check fired on that, stealing GRAPH_MIN_HEIGHT rows to show only
        // "No relationships yet". The fixed criterion is the same one the
        // drawer uses — it requires an edge — so with none the panel claims
        // nothing and the strip goes to Brian's Brain.
        //
        // Guard: `knowledge_tab_with_an_edge_draws_the_graph_panel` renders
        // with this exact (project_count, width, height, tab) and asserts the
        // panel *does* appear once an edge is present. The only difference
        // here is the relation count, so a pass proves the criterion — not a
        // lack of room — is what withholds the rows.
        let text = render_sidebar_text_with(
            2,
            34,
            40,
            &Theme::classic(),
            SidebarLayer::Knowledge,
            |app| {
                assert!(
                    !app.project_graph_trees.is_empty(),
                    "precondition: singleton trees should be present"
                );
                assert!(
                    app.project_graph_edges.is_empty(),
                    "precondition: no relations were registered"
                );
            },
        );
        assert!(
            !text.contains("project graph"),
            "graph panel must not claim height with zero relations: {text}"
        );
        assert!(
            !text.contains("No relationships yet"),
            "empty message must not be drawn when panel has no content: {text}"
        );
    }
}
