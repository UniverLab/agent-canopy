//! Sidebar rendering — agent cards split into Background and Interactive groups.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{
    last_two_segments, truncate_str, ACCENT, BG_HOVER, BG_SELECTED, DIM, INTERACTIVE_COLOR,
};
use super::{STATUS_DISABLED, STATUS_FAIL, STATUS_OK, STATUS_RUNNING};
use crate::tui::agent::AgentStatus;
use crate::tui::app::types::{
    AgentEntry, AgentSectionFocus, App, Focus, ProjectsPanelFocus, SidebarMode,
};
use ratatui::style::Color;

pub(super) fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    app.sidebar_click_map.clear();
    app.sidebar_visible_capacity = 0;

    let (background_indices, interactive_indices, terminal_indices) = agent_indices_by_kind(app);
    let areas = split_sidebar_content(area, app);

    if app.sidebar_mode == SidebarMode::Projects {
        draw_projects_sidebar(frame, areas, app);
        return;
    }

    let has_agents = !background_indices.is_empty()
        || !interactive_indices.is_empty()
        || !terminal_indices.is_empty()
        || !app.split_groups.is_empty();
    if !has_agents {
        draw_empty_agents_sidebar(frame, areas, app);
        return;
    }

    draw_agents_sidebar(
        frame,
        areas,
        &background_indices,
        &interactive_indices,
        &terminal_indices,
        app,
    );
}

#[derive(Clone, Copy)]
struct SidebarContentAreas {
    content: Rect,
    dashboard: Option<Rect>,
}

#[derive(Default)]
struct ProjectsLayout {
    projects: Option<Rect>,
    loops: Option<Rect>,
    knowledge: Option<Rect>,
    rag_queue: Option<Rect>,
    brain: Option<Rect>,
}

#[derive(Default)]
struct AgentLayout {
    background: Option<Rect>,
    interactive: Option<Rect>,
    terminal: Option<Rect>,
    groups: Option<Rect>,
    brain: Option<Rect>,
    rag_info: Option<Rect>,
}

#[derive(Clone, Copy, Default)]
struct AgentSectionHeights {
    background: Option<u16>,
    interactive: Option<u16>,
    terminal: Option<u16>,
    groups: Option<u16>,
}

impl AgentSectionHeights {
    fn total(self) -> u16 {
        self.background.unwrap_or(0)
            + self.interactive.unwrap_or(0)
            + self.terminal.unwrap_or(0)
            + self.groups.unwrap_or(0)
    }

    fn count(self) -> u16 {
        self.background.is_some() as u16
            + self.interactive.is_some() as u16
            + self.terminal.is_some() as u16
            + self.groups.is_some() as u16
    }
}

#[derive(Clone, Copy)]
struct ScrollState {
    start: usize,
    max_visible: usize,
    has_up: bool,
    has_down: bool,
}

#[derive(Clone, Copy)]
struct AgentCardMeta<'a> {
    accent: Color,
    status_color: Color,
    agent_type: &'static str,
    type_detail: &'a str,
    work_dir: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct GroupRowStyle {
    bg: Color,
    fg: Color,
    modifier: Modifier,
    prefix_color: Color,
    active_tag: &'static str,
}

fn agent_indices_by_kind(app: &App) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    app.agents.iter().enumerate().fold(
        (Vec::new(), Vec::new(), Vec::new()),
        |mut indices, (index, agent)| {
            match agent {
                AgentEntry::Interactive(_) => indices.1.push(index),
                AgentEntry::Terminal(_) => indices.2.push(index),
                AgentEntry::Group(_) => {}
                _ => indices.0.push(index),
            }
            indices
        },
    )
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

fn render_brain_if_visible(frame: &mut Frame, area: Option<Rect>, app: &App) {
    let Some(area) = area.filter(|area| area.height >= 3 && area.width >= 6) else {
        return;
    };
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

fn draw_projects_sidebar(frame: &mut Frame, areas: SidebarContentAreas, app: &App) {
    let rag_items = &app.global_rag_queue;
    let loops = app.visible_loops();
    let show_rag_info = app.rag_info.has_rag_activity() && areas.content.height >= 6;
    // ragInfo sits at the TOP of the projects sidebar so it's always visible.
    let (rag_info_area, content_below) = split_top_panel(areas.content, show_rag_info, 6);
    let (has_projects, has_loops, projects_needed, loops_needed, knowledge_needed, rag_needed) =
        projects_layout_requirements(app, loops.len(), rag_items, content_below.height);
    let layout = layout_projects_sections(
        content_below,
        has_projects,
        has_loops,
        projects_needed,
        loops_needed,
        knowledge_needed,
        rag_needed,
    );

    if let Some(rag_info_area) = rag_info_area.filter(|area| area.height >= 3) {
        render_titled_panel(
            frame,
            rag_info_area,
            " ragInfo ",
            Style::default().fg(DIM),
            projects_panel_border_style(app, ProjectsPanelFocus::RagInfo),
            |frame, inner| draw_rag_info(frame, inner, app),
        );
    }

    if let Some(projects_area) = layout.projects {
        render_titled_panel(
            frame,
            projects_area,
            " projects ",
            Style::default().fg(DIM),
            projects_panel_border_style(app, ProjectsPanelFocus::Projects),
            |frame, inner| draw_projects_list(frame, inner, app),
        );
    }

    if let Some(loops_area) = layout.loops {
        render_titled_panel(
            frame,
            loops_area,
            " loops ",
            Style::default().fg(DIM),
            projects_panel_border_style(app, ProjectsPanelFocus::Loops),
            |frame, inner| draw_loops_list(frame, inner, app),
        );
    }

    if let Some(knowledge_area) = layout.knowledge {
        render_titled_panel(
            frame,
            knowledge_area,
            " knowledge ",
            Style::default().fg(DIM),
            projects_panel_border_style(app, ProjectsPanelFocus::Knowledge),
            |frame, inner| draw_knowledge_list(frame, inner, app),
        );
    }

    if let Some(rag_area) = layout.rag_queue.filter(|area| area.height >= 3) {
        render_titled_panel(
            frame,
            rag_area,
            rag_queue_title(app.rag_paused),
            Style::default().fg(DIM),
            Style::default().fg(DIM),
            |frame, inner| draw_rag_queue(frame, inner, rag_items, app.selected_rag_queue),
        );
    }

    render_brain_if_visible(frame, layout.brain, app);

    // Project graph — show in brain area if we have graph trees
    if !app.project_graph_trees.is_empty() {
        if let Some(graph_area) = layout.brain.filter(|area| area.height >= 4) {
            render_titled_panel(
                frame,
                graph_area,
                " project graph ",
                Style::default().fg(DIM),
                Style::default().fg(DIM),
                |frame, inner| draw_project_graph(frame, inner, app),
            );
        }
    }

    render_dashboard_if_present(frame, areas.dashboard, app);

    // Project relation dialog overlay
    if let Some(dialog) = app.project_relation_dialog.as_ref() {
        draw_project_relation_dialog(frame, areas.content, app, dialog);
    }
}

fn split_top_panel(content: Rect, enabled: bool, top_height: u16) -> (Option<Rect>, Rect) {
    if !enabled {
        return (None, content);
    }

    let [top, bottom] =
        Layout::vertical([Constraint::Length(top_height), Constraint::Min(0)]).areas(content);
    (Some(top), bottom)
}

fn projects_layout_requirements(
    app: &App,
    loop_count: usize,
    rag_items: &[crate::db::project::RagQueueItem],
    content_height: u16,
) -> (bool, bool, u16, u16, u16, u16) {
    let has_projects = !app.projects.is_empty();
    let has_loops = true;
    let projects_needed = if has_projects {
        (app.projects.len() as u16 * 3 + 2).min(content_height)
    } else {
        0
    };
    let loops_needed = if loop_count > 0 {
        (loop_count as u16 * 3 + 2).min(content_height)
    } else {
        4.min(content_height)
    };
    let knowledge_needed = if !app.project_knowledge.is_empty() {
        (app.project_knowledge.len() as u16 * 3 + 2).min(12)
    } else {
        4.min(content_height)
    };
    let rag_needed = if app.playground_active && !rag_items.is_empty() {
        (rag_items.len() as u16 * 2 + 3).min(14)
    } else {
        0
    };

    (
        has_projects,
        has_loops,
        projects_needed,
        loops_needed,
        knowledge_needed,
        rag_needed,
    )
}

fn rag_queue_title(rag_paused: bool) -> &'static str {
    if rag_paused {
        " ragQueue ⏸ "
    } else {
        " ragQueue "
    }
}

fn layout_projects_sections(
    content_top: Rect,
    has_projects: bool,
    has_loops: bool,
    projects_needed: u16,
    loops_needed: u16,
    knowledge_needed: u16,
    rag_needed: u16,
) -> ProjectsLayout {
    if (has_projects || has_loops)
        && rag_needed > 0
        && projects_needed + loops_needed + knowledge_needed + rag_needed < content_top.height
    {
        let mut remaining = content_top;
        let projects = take_top(&mut remaining, projects_needed);
        let loops = take_top(&mut remaining, loops_needed);
        let knowledge = take_top(&mut remaining, knowledge_needed);
        let rag_queue = take_top(&mut remaining, rag_needed);

        return ProjectsLayout {
            projects,
            loops,
            knowledge,
            rag_queue,
            brain: Some(remaining),
        };
    }

    if (has_projects || has_loops)
        && projects_needed + loops_needed + knowledge_needed < content_top.height
    {
        let mut remaining = content_top;
        return ProjectsLayout {
            projects: take_top(&mut remaining, projects_needed),
            loops: take_top(&mut remaining, loops_needed),
            knowledge: take_top(&mut remaining, knowledge_needed),
            rag_queue: (rag_needed > 0 && remaining.height >= 3).then_some(remaining),
            brain: None,
        };
    }

    if has_projects || has_loops {
        let mut remaining = content_top;
        return ProjectsLayout {
            projects: take_top(&mut remaining, projects_needed),
            loops: take_top(&mut remaining, loops_needed).or(Some(remaining)),
            knowledge: None,
            ..ProjectsLayout::default()
        };
    }

    if rag_needed > 0 {
        return ProjectsLayout {
            rag_queue: Some(content_top),
            ..ProjectsLayout::default()
        };
    }

    ProjectsLayout {
        brain: Some(content_top),
        ..ProjectsLayout::default()
    }
}

fn draw_empty_agents_sidebar(frame: &mut Frame, areas: SidebarContentAreas, app: &App) {
    let show_rag_info = app.rag_info.has_rag_activity() && areas.content.height >= 9;
    let (brain_area, rag_info_area) = if show_rag_info {
        let [top, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(6)]).areas(areas.content);
        (Some(top), Some(bottom))
    } else {
        (Some(areas.content), None)
    };

    render_brain_if_visible(frame, brain_area, app);
    if let Some(rag_info_area) = rag_info_area {
        draw_agents_rag_info_panel(frame, rag_info_area, app);
    }
    render_dashboard_if_present(frame, areas.dashboard, app);
}

fn draw_agents_sidebar(
    frame: &mut Frame,
    areas: SidebarContentAreas,
    background_indices: &[usize],
    interactive_indices: &[usize],
    terminal_indices: &[usize],
    app: &mut App,
) {
    let show_rag_info = app.rag_info.has_rag_activity() && areas.content.height >= 10;
    // ragInfo sits at the TOP so it's always visible, same as the projects sidebar.
    let (rag_info_area, content_area) = if show_rag_info {
        let [top, bottom] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(areas.content);
        (Some(top), bottom)
    } else {
        (None, areas.content)
    };

    let heights = AgentSectionHeights {
        background: (!background_indices.is_empty())
            .then_some(background_indices.len() as u16 * 4 + 2),
        interactive: (!interactive_indices.is_empty())
            .then_some(interactive_indices.len() as u16 * 4 + 2),
        terminal: (!terminal_indices.is_empty()).then_some(terminal_indices.len() as u16 * 4 + 2),
        groups: (!app.split_groups.is_empty()).then_some(app.split_groups.len() as u16 * 2 + 2),
    };
    let mut layout = layout_agent_sections(content_area, heights);
    layout.rag_info = rag_info_area;

    render_agent_list_panel(
        frame,
        layout.background,
        " background ",
        background_indices,
        app,
        ACCENT,
        AgentSectionFocus::Background,
    );
    render_agent_list_panel(
        frame,
        layout.interactive,
        " interactive ",
        interactive_indices,
        app,
        INTERACTIVE_COLOR,
        AgentSectionFocus::Interactive,
    );
    render_agent_list_panel(
        frame,
        layout.terminal,
        " terminal ",
        terminal_indices,
        app,
        Color::Green,
        AgentSectionFocus::Terminal,
    );
    render_groups_panel(frame, layout.groups, app, AgentSectionFocus::Groups);
    render_brain_if_visible(frame, layout.brain, app);
    if let Some(rag_info_area) = layout.rag_info {
        draw_agents_rag_info_panel(frame, rag_info_area, app);
    }
    render_dashboard_if_present(frame, areas.dashboard, app);
}

fn layout_agent_sections(content_area: Rect, heights: AgentSectionHeights) -> AgentLayout {
    let total_needed = heights.total();
    let section_count = heights.count();
    let mut layout = AgentLayout::default();
    let mut remaining = content_area;

    if total_needed <= content_area.height || section_count == 1 {
        if let Some(height) = heights.background {
            layout.background = take_top(&mut remaining, height);
        }
        if let Some(height) = heights.interactive {
            layout.interactive = take_top(&mut remaining, height);
        }
        if let Some(height) = heights.terminal {
            layout.terminal = take_top(&mut remaining, height);
        }
        if let Some(height) = heights.groups.filter(|_| remaining.height > 0) {
            layout.groups = take_top(&mut remaining, height);
        }
        if remaining.height > 0 {
            layout.brain = Some(remaining);
        }
        return layout;
    }

    // Overflow: the sections don't all fit. Instead of an equal `height /
    // section_count` slice for everyone — which hands small sections
    // (background/terminal/groups) a fat slice they can't fill, leaving empty
    // gaps while `interactive` scrolls — allocate with max-min fairness so no
    // section ends up taller than its content.
    let mut kinds: Vec<AgentSectionKind> = Vec::new();
    let mut demands: Vec<u16> = Vec::new();
    for (kind, height) in [
        (AgentSectionKind::Background, heights.background),
        (AgentSectionKind::Interactive, heights.interactive),
        (AgentSectionKind::Terminal, heights.terminal),
        (AgentSectionKind::Groups, heights.groups),
    ] {
        if let Some(height) = height {
            kinds.push(kind);
            demands.push(height);
        }
    }

    let allocation = fair_section_heights(&demands, content_area.height);
    for (kind, &height) in kinds.iter().zip(allocation.iter()) {
        let rect = take_top(&mut remaining, height);
        match kind {
            AgentSectionKind::Background => layout.background = rect,
            AgentSectionKind::Interactive => layout.interactive = rect,
            AgentSectionKind::Terminal => layout.terminal = rect,
            AgentSectionKind::Groups => layout.groups = rect,
        }
    }
    layout
}

#[derive(Clone, Copy)]
enum AgentSectionKind {
    Background,
    Interactive,
    Terminal,
    Groups,
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

fn render_agent_list_panel(
    frame: &mut Frame,
    area: Option<Rect>,
    title: &str,
    indices: &[usize],
    app: &mut App,
    accent: Color,
    section: AgentSectionFocus,
) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        title,
        Style::default().fg(DIM),
        agent_section_border_style(app, section),
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

fn draw_projects_list(frame: &mut Frame, area: Rect, app: &App) {
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
        (area.height / 4).max(1) as usize, // Ajustado para cards de 3 + separación
    );
    let panel_focused = app.projects_panel_focus == ProjectsPanelFocus::Projects;
    let mut y = area.y;
    let row_h = 4u16;

    for (idx, project) in app
        .projects
        .iter()
        .enumerate()
        .skip(scroll.start)
        .take(scroll.max_visible)
    {
        if y + 3 > area.y + area.height {
            break;
        }
        draw_project_loop_card(
            frame,
            Rect::new(area.x, y, area.width, 3),
            idx == app.selected_project,
            &project.name,
            &project.hash,
            &last_two_segments(&project.path),
            panel_focused,
        );
        y += row_h; // card + gap visual
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down);
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

fn draw_knowledge_list(frame: &mut Frame, area: Rect, app: &App) {
    let filtered = app.filtered_knowledge_indices();
    let filter_active = app.knowledge_filter_mode || !app.knowledge_filter.trim().is_empty();
    let list_area = if filter_active && area.height > 1 {
        let filter_text = if app.knowledge_filter_mode {
            format!("Filter: {}_", app.knowledge_filter)
        } else {
            format!("Filter: {}", app.knowledge_filter)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_str(&filter_text, area.width as usize),
                Style::default().fg(Color::Yellow),
            ))),
            Rect::new(area.x, area.y, area.width, 1),
        );
        Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        )
    } else {
        area
    };

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if app.project_knowledge.is_empty() {
                    "No knowledge yet. Agents can add facts/patterns."
                } else {
                    "No knowledge matches the current filter."
                },
                Style::default().fg(Color::DarkGray),
            ))),
            list_area,
        );
        return;
    }

    let scroll = scroll_state(
        filtered.len(),
        app.selected_filtered_knowledge_index(),
        (list_area.height / 3).max(1) as usize,
    );
    let panel_focused = app.projects_panel_focus == ProjectsPanelFocus::Knowledge;
    let mut y = list_area.y;
    let row_h = 3u16;

    for (display_idx, node_idx) in filtered
        .iter()
        .skip(scroll.start)
        .take(scroll.max_visible)
        .enumerate()
    {
        if y + 2 > list_area.y + list_area.height {
            break;
        }
        let node = &app.project_knowledge[*node_idx];
        let selected = Some(display_idx + scroll.start) == app.selected_filtered_knowledge_index();
        draw_knowledge_card(
            frame,
            Rect::new(list_area.x, y, list_area.width, 2),
            selected,
            &node.title,
            &node.kind,
            panel_focused,
        );
        y += row_h;
    }

    draw_scroll_indicators(frame, list_area, scroll.has_up, scroll.has_down);
}

fn draw_knowledge_card(
    frame: &mut Frame,
    area: Rect,
    selected: bool,
    title: &str,
    kind: &str,
    panel_focused: bool,
) {
    let bg = if selected { BG_SELECTED } else { Color::Reset };
    let kind_color = if kind == "fact" {
        Color::Cyan
    } else {
        Color::Magenta
    };
    let title_style = if selected && panel_focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

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
            format!("[{}]", kind),
            Style::default().fg(kind_color),
        )))
        .style(Style::default().bg(bg)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
}

fn draw_loops_list(frame: &mut Frame, area: Rect, app: &App) {
    let loops = app.visible_loops();
    if loops.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No loops yet",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let selected_index = app
        .selected_loop()
        .and_then(|lp| loops.iter().position(|candidate| candidate.id == lp.id));
    let scroll = scroll_state(
        loops.len(),
        selected_index,
        (area.height / 4).max(1) as usize, // Ajustado para cards
    );
    let panel_focused = app.projects_panel_focus == ProjectsPanelFocus::Loops;
    let mut y = area.y;
    let row_h = 4u16;

    for (_idx, lp) in loops
        .iter()
        .enumerate()
        .skip(scroll.start)
        .take(scroll.max_visible)
    {
        if y + 3 > area.y + area.height {
            break;
        }
        draw_project_loop_card(
            frame,
            Rect::new(area.x, y, area.width, 3),
            app.selected_loop_id.as_deref() == Some(lp.id.as_str()),
            &lp.name,
            &lp.status.as_str().to_uppercase(),
            &last_two_segments(&lp.workdir),
            panel_focused,
        );
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
    if app.rag_paused {
        return Line::from(Span::styled(
            " ⏸ paused ",
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.rag_info.processing_items > 0 {
        return Line::from(Span::styled(
            " ◉ indexing ",
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.rag_info.queued_items > 0 {
        return Line::from(Span::styled(
            " ⏳ pending ",
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(Span::styled(" ✓ ready ", Style::default().fg(ACCENT)))
}

fn rag_queue_text(app: &App) -> String {
    if app.rag_info.queued_items > 0 {
        format!("{} queued", app.rag_info.queued_items)
    } else {
        String::new()
    }
}

fn is_agents_rag_info_focused(app: &App) -> bool {
    app.sidebar_mode == SidebarMode::Agents
        && matches!(app.focus, Focus::Home | Focus::Preview)
        && app.agents_rag_focused
        && !app.playground_active
}

fn agents_rag_info_title(app: &App) -> &'static str {
    if app.rag_paused {
        " ragInfo ⏸ "
    } else {
        " ragInfo "
    }
}

fn agents_rag_info_border_style(app: &App) -> Style {
    Style::default().fg(if is_agents_rag_info_focused(app) {
        ACCENT
    } else {
        DIM
    })
}

fn draw_agents_rag_info_panel(frame: &mut Frame, area: Rect, app: &App) {
    let title_style = Style::default().fg(if is_agents_rag_info_focused(app) {
        ACCENT
    } else {
        DIM
    });
    render_titled_panel(
        frame,
        area,
        agents_rag_info_title(app),
        title_style,
        agents_rag_info_border_style(app),
        |frame, inner| {
            if inner.height >= 1 {
                draw_rag_info(frame, inner, app);
            }
        },
    );
}

fn projects_panel_border_style(app: &App, panel: ProjectsPanelFocus) -> Style {
    let focused = app.sidebar_mode == SidebarMode::Projects
        && matches!(app.focus, Focus::Home | Focus::Preview)
        && app.projects_panel_focus == panel
        && !app.playground_active;
    Style::default().fg(if focused { ACCENT } else { DIM })
}

fn agent_section_border_style(app: &App, section: AgentSectionFocus) -> Style {
    let in_agents_mode = app.sidebar_mode == SidebarMode::Agents;
    let not_playground = !app.playground_active;

    let focused = if matches!(app.focus, Focus::Home | Focus::Preview) {
        in_agents_mode && not_playground && app.agent_section_focus == section
    } else if app.focus == Focus::Agent {
        in_agents_mode && not_playground && {
            match app.agents.get(app.selected) {
                Some(AgentEntry::Agent(_) | AgentEntry::Group(_)) => {
                    section == AgentSectionFocus::Background
                }
                Some(AgentEntry::Interactive(_)) => section == AgentSectionFocus::Interactive,
                Some(AgentEntry::Terminal(_)) => section == AgentSectionFocus::Terminal,
                None => false,
            }
        }
    } else {
        false
    };

    Style::default().fg(if focused { ACCENT } else { DIM })
}

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
                status_color: session_status_color(&agent.status),
                agent_type: "pty",
                type_detail: agent.cli.as_str(),
                work_dir: Some(agent.working_dir.as_str()),
            }
        }
        AgentEntry::Terminal(index) => {
            let agent = &app.terminal_agents[*index];
            AgentCardMeta {
                accent: agent.accent_color,
                status_color: session_status_color(&agent.status),
                agent_type: "term",
                type_detail: agent.shell.as_str(),
                work_dir: Some(agent.working_dir.as_str()),
            }
        }
        AgentEntry::Group(_) => AgentCardMeta {
            accent: ACCENT,
            status_color: STATUS_OK,
            agent_type: "group",
            type_detail: "",
            work_dir: None,
        },
    }
}

fn session_status_color(status: &AgentStatus) -> Color {
    match status {
        AgentStatus::Running => STATUS_RUNNING,
        AgentStatus::Exited(0) => STATUS_OK,
        AgentStatus::Exited(_) => STATUS_FAIL,
    }
}

fn effective_status_color(base: Color, agent: &AgentEntry, app: &App, selected: bool) -> Color {
    if !agent_is_waiting(agent, app, selected) {
        return base;
    }

    if (app.animation_tick / 10).is_multiple_of(2) {
        super::STATUS_WAIT_ON
    } else {
        super::STATUS_WAIT_OFF
    }
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
    fn layout_agent_sections_overflow_keeps_small_sections_within_needed() {
        let content = Rect::new(0, 0, 33, 30);
        let heights = AgentSectionHeights {
            background: Some(needed_agents(1)),
            interactive: Some(needed_agents(6)),
            terminal: Some(needed_agents(1)),
            groups: None,
        };
        assert!(
            heights.total() > content.height,
            "test must exercise overflow"
        );

        let layout = layout_agent_sections(content, heights);

        let background = layout.background.expect("background rect");
        let interactive = layout.interactive.expect("interactive rect");
        let terminal = layout.terminal.expect("terminal rect");

        // Small sections never get a slice taller than their content.
        assert!(background.height <= needed_agents(1));
        assert!(terminal.height <= needed_agents(1));
        // Interactive takes the bulk of the space and scrolls.
        assert!(interactive.height > background.height);
        assert!(interactive.height > terminal.height);
        // Sections tile the content area top-to-bottom with no gaps.
        assert_eq!(background.y, content.y);
        assert_eq!(interactive.y, background.y + background.height);
        assert_eq!(terminal.y, interactive.y + interactive.height);
        assert_eq!(
            background.height + interactive.height + terminal.height,
            content.height
        );
    }

    #[test]
    fn layout_agent_sections_fits_all_when_room_available() {
        let content = Rect::new(0, 0, 33, 60);
        let heights = AgentSectionHeights {
            background: Some(needed_agents(1)),
            interactive: Some(needed_agents(2)),
            terminal: Some(needed_agents(1)),
            groups: None,
        };
        assert!(heights.total() <= content.height);

        let layout = layout_agent_sections(content, heights);

        assert_eq!(layout.background.unwrap().height, needed_agents(1));
        assert_eq!(layout.interactive.unwrap().height, needed_agents(2));
        assert_eq!(layout.terminal.unwrap().height, needed_agents(1));
        // Leftover space becomes the brain panel.
        assert!(layout.brain.is_some());
    }
}
