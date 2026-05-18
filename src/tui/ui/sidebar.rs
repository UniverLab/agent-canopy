//! Sidebar rendering — agent cards split into Background and Interactive groups.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::{last_two_segments, truncate_str, ACCENT, BG_SELECTED, DIM, INTERACTIVE_COLOR};
use super::{STATUS_DISABLED, STATUS_FAIL, STATUS_OK, STATUS_RUNNING};
use crate::tui::agent::AgentStatus;
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectsPanelFocus, SidebarMode};
use ratatui::style::Color;

pub(super) fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    app.sidebar_click_map.clear();

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
    workflows: Option<Rect>,
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
    let mut content_lines = 4u16;
    let gpu_will_show = app.system_info.gpu_info.as_ref().is_some_and(|gpu| {
        let has_vram =
            matches!((gpu.vram_used, gpu.vram_total), (Some(_), Some(total)) if total > 0);
        gpu.usage.is_some() || gpu.temperature.is_some() || has_vram
    });
    if gpu_will_show {
        content_lines += 1;
    }
    if app.system_info.swap_used > 0 {
        content_lines += 1;
    }
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
    let start = selected.map_or(0, |selected| {
        if selected >= max_visible {
            selected.saturating_sub(max_visible - 1)
        } else {
            0
        }
    });

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
    let workflows = app.visible_workflows();
    let show_rag_info = app.rag_info.has_rag_activity() && areas.content.height >= 6;
    // ragInfo sits at the TOP of the projects sidebar so it's always visible.
    let (rag_info_area, content_below) = split_top_panel(areas.content, show_rag_info, 6);
    let (has_projects, has_workflows, projects_needed, workflows_needed, rag_needed) =
        projects_layout_requirements(app, workflows.len(), rag_items, content_below.height);
    let layout = layout_projects_sections(
        content_below,
        has_projects,
        has_workflows,
        projects_needed,
        workflows_needed,
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

    if let Some(workflows_area) = layout.workflows {
        render_titled_panel(
            frame,
            workflows_area,
            " workflows ",
            Style::default().fg(DIM),
            projects_panel_border_style(app, ProjectsPanelFocus::Workflows),
            |frame, inner| draw_workflows_list(frame, inner, app),
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

    render_dashboard_if_present(frame, areas.dashboard, app);
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
    workflow_count: usize,
    rag_items: &[crate::db::project::RagQueueItem],
    content_height: u16,
) -> (bool, bool, u16, u16, u16) {
    let has_projects = !app.projects.is_empty();
    let has_workflows = true;
    let projects_needed = if has_projects {
        (app.projects.len() as u16 * 3 + 2).min(content_height)
    } else {
        0
    };
    let workflows_needed = if workflow_count > 0 {
        (workflow_count as u16 * 3 + 2).min(content_height)
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
        has_workflows,
        projects_needed,
        workflows_needed,
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
    has_workflows: bool,
    projects_needed: u16,
    workflows_needed: u16,
    rag_needed: u16,
) -> ProjectsLayout {
    if (has_projects || has_workflows)
        && rag_needed > 0
        && projects_needed + workflows_needed + rag_needed < content_top.height
    {
        let mut remaining = content_top;
        let projects = take_top(&mut remaining, projects_needed);
        let workflows = take_top(&mut remaining, workflows_needed);
        let rag_queue = take_top(&mut remaining, rag_needed);
        return ProjectsLayout {
            projects,
            workflows,
            rag_queue,
            brain: Some(remaining),
        };
    }

    if (has_projects || has_workflows) && projects_needed + workflows_needed < content_top.height {
        let mut remaining = content_top;
        return ProjectsLayout {
            projects: take_top(&mut remaining, projects_needed),
            workflows: take_top(&mut remaining, workflows_needed),
            rag_queue: (rag_needed > 0 && remaining.height >= 3).then_some(remaining),
            brain: None,
        };
    }

    if has_projects || has_workflows {
        let mut remaining = content_top;
        return ProjectsLayout {
            projects: take_top(&mut remaining, projects_needed),
            workflows: take_top(&mut remaining, workflows_needed).or(Some(remaining)),
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
    );
    render_agent_list_panel(
        frame,
        layout.interactive,
        " interactive ",
        interactive_indices,
        app,
        INTERACTIVE_COLOR,
    );
    render_agent_list_panel(
        frame,
        layout.terminal,
        " terminal ",
        terminal_indices,
        app,
        Color::Green,
    );
    render_groups_panel(frame, layout.groups, app);
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

    let per_section = content_area.height / section_count;
    if heights.background.is_some() {
        layout.background = take_top(&mut remaining, per_section);
    }
    if heights.interactive.is_some() {
        layout.interactive = take_top(&mut remaining, per_section);
    }
    if heights.terminal.is_some() {
        layout.terminal = take_top(&mut remaining, per_section);
    }
    if heights.groups.is_some() && remaining.height > 0 {
        layout.groups = Some(remaining);
    }
    layout
}

fn render_agent_list_panel(
    frame: &mut Frame,
    area: Option<Rect>,
    title: &str,
    indices: &[usize],
    app: &mut App,
    accent: Color,
) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        title,
        Style::default().fg(DIM),
        Style::default().fg(DIM),
        |frame, inner| draw_agent_list(frame, inner, indices, app, accent),
    );
}

fn render_groups_panel(frame: &mut Frame, area: Option<Rect>, app: &mut App) {
    let Some(area) = area else {
        return;
    };
    render_titled_panel(
        frame,
        area,
        " groups ",
        Style::default().fg(DIM),
        Style::default().fg(DIM),
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
        (area.height / 3).max(1) as usize,
    );
    let panel_focused = app.projects_panel_focus == ProjectsPanelFocus::Projects;
    let mut y = area.y;

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
        draw_project_row(
            frame,
            area,
            y,
            project,
            idx == app.selected_project,
            panel_focused,
        );
        y += 3;
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down);
}

fn draw_project_row(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    project: &crate::domain::project::Project,
    selected: bool,
    panel_focused: bool,
) {
    let title_style = project_title_style(selected, panel_focused);
    let meta_style = project_meta_style(selected);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&project.name, area.width as usize),
            title_style,
        ))),
        Rect::new(area.x, y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&project.hash, area.width as usize),
            meta_style,
        ))),
        Rect::new(area.x, y + 1, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&last_two_segments(&project.path), area.width as usize),
            meta_style,
        ))),
        Rect::new(area.x, y + 2, area.width, 1),
    );
}

fn draw_workflows_list(frame: &mut Frame, area: Rect, app: &App) {
    let workflows = app.visible_workflows();
    if workflows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No workflows yet",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }

    let selected_index = app.selected_workflow().and_then(|workflow| {
        workflows
            .iter()
            .position(|candidate| candidate.id == workflow.id)
    });
    let scroll = scroll_state(
        workflows.len(),
        selected_index,
        (area.height / 3).max(1) as usize,
    );
    let panel_focused = app.projects_panel_focus == ProjectsPanelFocus::Workflows;
    let mut y = area.y;

    for (idx, workflow) in workflows
        .iter()
        .enumerate()
        .skip(scroll.start)
        .take(scroll.max_visible)
    {
        if y + 3 > area.y + area.height {
            break;
        }
        draw_workflow_row(
            frame,
            area,
            y,
            workflow,
            app.selected_workflow_id.as_deref() == Some(workflow.id.as_str()),
            panel_focused,
        );
        let _ = idx;
        y += 3;
    }

    draw_scroll_indicators(frame, area, scroll.has_up, scroll.has_down);
}

fn draw_workflow_row(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    workflow: &crate::domain::workflow::Workflow,
    selected: bool,
    panel_focused: bool,
) {
    let title_style = project_title_style(selected, panel_focused);
    let meta_style = project_meta_style(selected);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&workflow.name, area.width as usize),
            title_style,
        ))),
        Rect::new(area.x, y, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(
                &workflow.status.as_str().to_uppercase(),
                area.width as usize,
            ),
            meta_style,
        ))),
        Rect::new(area.x, y + 1, area.width, 1),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_str(&last_two_segments(&workflow.workdir), area.width as usize),
            meta_style,
        ))),
        Rect::new(area.x, y + 2, area.width, 1),
    );
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

fn draw_agent_list(frame: &mut Frame, area: Rect, indices: &[usize], app: &mut App, accent: Color) {
    let card_h = 3u16;
    let row_h = 4u16;

    if area.height < card_h || indices.is_empty() {
        return;
    }

    let max_visible = ((area.height.saturating_sub(card_h)) / row_h + 1) as usize;
    let selected_local = indices.iter().position(|&idx| idx == app.selected);
    let scroll = scroll_state(indices.len(), selected_local, max_visible);
    let mut y = area.y;
    let end = indices.len().min(scroll.start + scroll.max_visible + 1);

    for (rel_i, &idx) in indices[scroll.start..end].iter().enumerate() {
        if y + card_h > area.y + area.height {
            break;
        }

        let card_area = Rect::new(area.x, y, area.width, card_h);
        let selected = idx == app.selected && !app.agents_rag_focused;
        draw_sidebar_card(frame, card_area, &app.agents[idx], app, selected, accent);
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
                area.y + area.height - 1,
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
    _accent: Color,
) {
    let meta = agent_card_meta(agent, app);
    let status_color = effective_status_color(meta.status_color, agent, app, selected);
    let bg = if selected { BG_SELECTED } else { Color::Reset };
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
