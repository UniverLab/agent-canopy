use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, draw_dialog_left_wave, truncate_str};
use crate::tui::app::{
    dialog::new_agent::{BackgroundTrigger, NewAgentDialog, NewTaskMode, NewTaskType, SeedOption},
    types::App,
};
use crate::tui::ui::theme::Theme;

const TYPE_FIELD: usize = 0;
const INTERACTIVE_MODE_FIELD: usize = 1;
const BACKGROUND_TRIGGER_FIELD: usize = 1;
const TERMINAL_DIR_FIELD: usize = 1;
const TERMINAL_SHELL_FIELD: usize = 2;

const CLI_PICKER_VISIBLE: usize = 6;
const MODEL_PICKER_VISIBLE: usize = 5;
const SESSION_PICKER_VISIBLE: usize = 6;
const DIR_BROWSER_VISIBLE: usize = 10;
pub(crate) const SESSION_RESUME_PICKER_VISIBLE: usize = 6;

#[derive(Clone, Copy)]
struct FieldLayout {
    cli: usize,
    identity: usize,
    model: usize,
    prompt: usize,
    extra: usize,
    dir: usize,
    yolo: usize,
    sandbox: usize,
}

impl FieldLayout {
    fn for_task(task_type: NewTaskType) -> Self {
        match task_type {
            NewTaskType::Interactive => Self {
                cli: 2,
                identity: 5,
                model: 3,
                prompt: 4,
                extra: 5,
                dir: 6,
                yolo: 4,
                sandbox: 7,
            },
            NewTaskType::Terminal => Self {
                cli: 0,
                identity: 0,
                model: 3,
                prompt: 4,
                extra: 5,
                dir: TERMINAL_DIR_FIELD,
                yolo: 4,
                sandbox: 0,
            },
            NewTaskType::Background => Self {
                cli: 2,
                identity: 0,
                model: 3,
                prompt: 4,
                extra: 5,
                dir: 6,
                yolo: 4,
                sandbox: 0,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct PickerWindow {
    scroll: usize,
    has_above: bool,
    has_below: bool,
}

impl PickerWindow {
    fn new(selected: usize, total: usize, max_visible: usize) -> Self {
        let scroll = selected.saturating_sub(max_visible.saturating_sub(1));
        Self {
            scroll,
            has_above: scroll > 0,
            has_below: total > 0 && scroll + max_visible < total,
        }
    }
}

pub fn draw_new_agent_dialog(frame: &mut Frame, app: &App, theme: &Theme) {
    let Some(dialog) = &app.new_agent_dialog else {
        return;
    };

    let accent = dialog.selected_accent_color(theme);
    let filtered_clis = dialog.filtered_cli_indices();
    let area = centered_rect(65, dialog_height(dialog, &filtered_clis), frame.area());
    frame.render_widget(Clear, area);
    draw_dialog_left_wave(frame, area, app.animation_tick.into());

    let field_width = prompt_field_width(frame.area());

    let block = Block::default()
        .title(dialog_title(dialog))
        .borders(crate::tui::ui::borders_for(theme))
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(theme.dialog_bg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = build_dialog_lines(dialog, accent, &filtered_clis, field_width, theme);
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── Form state ───────────────────────────────────────────────────────────────

fn dialog_height(dialog: &NewAgentDialog, filtered_clis: &[usize]) -> u16 {
    let dir_rows = dir_browser_rows(dialog);
    let prompt_rows = if matches!(dialog.task_type, NewTaskType::Background) {
        // 1 label row + 3 visible prompt rows
        1 + PROMPT_VISIBLE_ROWS as u16
    } else {
        0
    };
    let base_height = match dialog.task_type {
        NewTaskType::Interactive => 14 + dir_rows,
        NewTaskType::Terminal => 10 + dir_rows,
        NewTaskType::Background => 15 + dir_rows + prompt_rows,
    };

    base_height
        + cli_picker_rows(dialog, filtered_clis.len())
        + model_picker_rows(dialog)
        + session_resume_picker_rows(dialog)
}

fn session_resume_picker_rows(dialog: &NewAgentDialog) -> u16 {
    let Some(picker) = &dialog.session_resume_picker else {
        return 0;
    };

    let visible = picker.sessions.len().min(SESSION_RESUME_PICKER_VISIBLE);
    let overflow_line = usize::from(picker.sessions.len() > SESSION_RESUME_PICKER_VISIBLE);
    (visible + overflow_line) as u16
}

fn cli_picker_rows(dialog: &NewAgentDialog, filtered_clis_len: usize) -> u16 {
    if !dialog.cli_picker_open {
        return 0;
    }

    filtered_clis_len.clamp(1, CLI_PICKER_VISIBLE) as u16 + 2
}

fn model_picker_rows(dialog: &NewAgentDialog) -> u16 {
    if !dialog.model_picker_open || dialog.model_suggestions.is_empty() {
        return 0;
    }

    let visible = dialog.model_suggestions.len().min(MODEL_PICKER_VISIBLE);
    let overflow_line = usize::from(dialog.model_suggestions.len() > MODEL_PICKER_VISIBLE);
    (visible + overflow_line) as u16
}

fn dir_browser_rows(dialog: &NewAgentDialog) -> u16 {
    let filtered_entries = dialog.filtered_dir_entries();
    if filtered_entries.is_empty() && dialog.dir_entries.is_empty() {
        return 0;
    }

    3 + filtered_entries.len().min(DIR_BROWSER_VISIBLE) as u16
}

fn dialog_title(dialog: &NewAgentDialog) -> &'static str {
    if !dialog.is_edit_mode() {
        return " New Agent ";
    }

    match dialog.task_type {
        NewTaskType::Background => " Edit Background ",
        NewTaskType::Interactive => " Edit Agent ",
        NewTaskType::Terminal => " Edit Terminal ",
    }
}

fn task_type_label(task_type: NewTaskType) -> &'static str {
    match task_type {
        NewTaskType::Interactive => "Interactive",
        NewTaskType::Terminal => "Terminal",
        NewTaskType::Background => "Background",
    }
}

fn interactive_mode_label(task_mode: NewTaskMode) -> &'static str {
    match task_mode {
        NewTaskMode::Interactive => "New",
        NewTaskMode::Resume => "Resume",
    }
}

fn background_trigger_label(trigger: BackgroundTrigger) -> &'static str {
    match trigger {
        BackgroundTrigger::Cron => "Cron",
        BackgroundTrigger::Watch => "Watch",
    }
}

fn help_text(dialog: &NewAgentDialog) -> &'static str {
    match dialog.task_type {
        NewTaskType::Interactive => {
            "  ↑↓: fields · Shift+↑↓: navigate  (in dirs: → enter  ← up) · Enter: launch · Esc: cancel"
        }
        NewTaskType::Background => {
            "  ↑↓: fields · Shift+↑↓: navigate  (in dirs: → enter  ← up) · Enter: create · Esc: cancel"
        }
        NewTaskType::Terminal => {
            "  ↑↓: fields · Shift+↑↓: navigate  (in dirs: → enter  ← up) · Enter: launch · Esc: cancel"
        }
    }
}

fn session_picker_label(dialog: &NewAgentDialog) -> String {
    let Some((_, title)) = &dialog.selected_session else {
        return "  ↵ pick session  (latest)".to_string();
    };

    format!("  ↵ pick  [{}]", truncate_with_ellipsis(title, 40))
}

fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let shortened: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

// ── Rendering helpers ────────────────────────────────────────────────────────

fn build_dialog_lines(
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    field_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let layout = FieldLayout::for_task(dialog.task_type);
    let mut lines = dialog_header_lines(dialog, accent, theme);

    match dialog.task_type {
        NewTaskType::Interactive => {
            append_interactive_sections(
                &mut lines,
                dialog,
                accent,
                filtered_clis,
                layout,
                field_width,
                theme,
            );
        }
        NewTaskType::Terminal => {
            append_terminal_sections(&mut lines, dialog, accent, field_width, theme);
        }
        NewTaskType::Background => {
            append_background_sections(
                &mut lines,
                dialog,
                accent,
                filtered_clis,
                layout,
                field_width,
                theme,
            );
        }
    }

    lines.push(Line::from(Span::styled(
        help_text(dialog),
        Style::default().fg(theme.dim_text),
    )));
    lines
}

fn dialog_header_lines(
    dialog: &NewAgentDialog,
    accent: Color,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut type_row = vec![Span::styled(
        "  Type:  ",
        Style::default().fg(theme.dim_text),
    )];
    type_row.extend(selector_spans(
        task_type_label(dialog.task_type),
        TYPE_FIELD,
        dialog.is_edit_mode(),
        accent,
        dialog.field,
        theme,
    ));
    vec![Line::from(""), Line::from(type_row), Line::from("")]
}

fn selector_spans(
    value: &str,
    field: usize,
    locked: bool,
    accent: Color,
    current_field: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if locked {
        return vec![Span::styled(
            format!("  {value}  "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )];
    }

    // Discreet ‹ › lateral selectors, matching the prompt builder's send
    // control: arrows are dim when the field is unfocused and take the accent
    // color when it is focused; the value keeps the field's focus styling.
    let arrow_style = if current_field == field {
        Style::default().fg(accent)
    } else {
        Style::default().fg(theme.dim_text)
    };
    vec![
        Span::styled(" ‹ ", arrow_style),
        Span::styled(value.to_string(), focus_style(current_field, field, accent)),
        Span::styled(" › ", arrow_style),
    ]
}

fn focus_style(current_field: usize, field: usize, accent: Color) -> Style {
    if current_field == field {
        return Style::default()
            .fg(Color::Black)
            .bg(accent)
            .add_modifier(Modifier::BOLD);
    }

    Style::default().fg(Color::White)
}

fn picker_item_style(accent: Color, selected: bool) -> Style {
    if selected {
        return Style::default()
            .fg(Color::Black)
            .bg(accent)
            .add_modifier(Modifier::BOLD);
    }

    Style::default().fg(Color::White)
}

fn picker_detail_style(selected: bool, selected_style: Style, theme: &Theme) -> Style {
    if selected {
        selected_style
    } else {
        Style::default().fg(theme.dim_text)
    }
}

fn filter_row(
    prefix: &str,
    filter: &str,
    focused: bool,
    accent: Color,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            prefix.to_string(),
            if focused {
                Style::default().fg(accent)
            } else {
                Style::default().fg(theme.dim_text)
            },
        ),
        Span::styled(
            filter_display(filter),
            if filter.is_empty() {
                Style::default().fg(theme.dim_text)
            } else {
                Style::default().fg(Color::White)
            },
        ),
    ])
}

fn filter_display(filter: &str) -> String {
    if filter.is_empty() {
        "type to filter".to_string()
    } else {
        filter.to_string()
    }
}

fn push_spaced_row(lines: &mut Vec<Line<'static>>, row: Line<'static>) {
    lines.push(row);
    lines.push(Line::from(""));
}

// ── Section builders ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn append_interactive_sections(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    layout: FieldLayout,
    _field_width: usize,
    theme: &Theme,
) {
    if !dialog.is_edit_mode() {
        push_spaced_row(lines, interactive_mode_row(dialog, accent, theme));
        // Sits between choosing Resume and CLI selection: the harness
        // follows from whichever session is picked here.
        append_session_resume_picker_rows(lines, dialog, theme);
    }

    append_cli_section(lines, dialog, accent, filtered_clis, layout.cli, theme);
    append_session_picker_rows(lines, dialog, theme);
    append_identity_section(lines, dialog, accent, layout.identity, theme);
    append_effort_section(lines, dialog, theme);
    append_yolo_section(lines, dialog, accent, layout.yolo, theme);
    append_sandbox_section(lines, dialog, accent, layout.sandbox, theme);
    append_directory_section(lines, dialog, accent, layout.dir, false, layout.dir, theme);
}

fn append_terminal_sections(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    _field_width: usize,
    theme: &Theme,
) {
    append_directory_section(
        lines,
        dialog,
        accent,
        TERMINAL_DIR_FIELD,
        false,
        TERMINAL_DIR_FIELD,
        theme,
    );
    push_spaced_row(lines, terminal_shell_row(dialog, accent, theme));
}

#[allow(clippy::too_many_arguments)]
fn append_background_sections(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    layout: FieldLayout,
    field_width: usize,
    theme: &Theme,
) {
    append_trigger_section(lines, dialog, accent, theme);
    append_cli_section(lines, dialog, accent, filtered_clis, layout.cli, theme);
    append_model_section(lines, dialog, accent, layout.model, theme);
    append_effort_section(lines, dialog, theme);
    append_prompt_section(lines, dialog, accent, layout, field_width, theme);

    let hide_dir = dialog.background_trigger == BackgroundTrigger::Watch;
    let browser_field = if hide_dir { layout.extra } else { layout.dir };
    append_directory_section(
        lines,
        dialog,
        accent,
        layout.dir,
        hide_dir,
        browser_field,
        theme,
    );
}

fn interactive_mode_row(dialog: &NewAgentDialog, accent: Color, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "  Session:  ",
        Style::default().fg(theme.dim_text),
    )];
    spans.extend(selector_spans(
        interactive_mode_label(dialog.task_mode),
        INTERACTIVE_MODE_FIELD,
        false,
        accent,
        dialog.field,
        theme,
    ));

    if dialog.resume_unconfigured() && !dialog.has_session_picker() {
        spans.push(Span::styled(
            "  (not configured — falls back to new)",
            Style::default().fg(Color::Yellow),
        ));
    }

    if matches!(dialog.task_mode, NewTaskMode::Resume) && dialog.has_session_picker() {
        spans.push(Span::styled(
            session_picker_label(dialog),
            Style::default().fg(Color::Cyan),
        ));
    }

    if matches!(dialog.task_mode, NewTaskMode::Resume) {
        spans.push(Span::styled(
            session_resume_label(dialog),
            Style::default().fg(if dialog.resume_sessions_empty {
                Color::Yellow
            } else {
                Color::Cyan
            }),
        ));
    }

    Line::from(spans)
}

/// Status text for the canopy-native resume picker: which session (and
/// harness) is resolved, that none exist, or that the picker is open.
fn session_resume_label(dialog: &NewAgentDialog) -> String {
    if dialog.resume_sessions_empty {
        return "  (no resumable sessions)".to_string();
    }
    if dialog.session_resume_picker.is_some() {
        return "  ↵/↑↓ choose session to resume".to_string();
    }
    match &dialog.selected_resume_session {
        Some(session) => format!(
            "  [{}] on {}",
            truncate_with_ellipsis(&session.name, 32),
            session.cli
        ),
        None => String::new(),
    }
}

fn append_trigger_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    theme: &Theme,
) {
    let mut trigger_row = vec![Span::styled(
        "  Trigger:",
        Style::default().fg(theme.dim_text),
    )];
    trigger_row.extend(selector_spans(
        background_trigger_label(dialog.background_trigger),
        BACKGROUND_TRIGGER_FIELD,
        dialog.is_edit_mode(),
        accent,
        dialog.field,
        theme,
    ));
    push_spaced_row(lines, Line::from(trigger_row));
}

fn append_cli_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    cli_field: usize,
    theme: &Theme,
) {
    lines.push(Line::from(vec![
        Span::styled("  Harness: ", Style::default().fg(theme.dim_text)),
        Span::styled(
            format!(" {} ", dialog.selected_cli()),
            focus_style(dialog.field, cli_field, accent),
        ),
        Span::styled(
            "  (◂▸ cycle · type/Space pick)",
            Style::default().fg(theme.dim_text),
        ),
    ]));

    append_cli_picker_rows(lines, dialog, accent, filtered_clis, cli_field, theme);
    lines.push(Line::from(""));
}

#[allow(clippy::too_many_arguments)]
fn append_cli_picker_rows(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    cli_field: usize,
    theme: &Theme,
) {
    if !dialog.cli_picker_open {
        return;
    }

    let total = dialog.available_clis.len();
    let total_matches = filtered_clis.len();
    let window = PickerWindow::new(dialog.cli_picker_idx, total_matches, CLI_PICKER_VISIBLE);

    lines.push(filter_row(
        "    🔍 ",
        &dialog.cli_picker_filter,
        dialog.field == cli_field,
        accent,
        theme,
    ));

    if filtered_clis.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no matches)",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        for (i, cli_idx) in filtered_clis
            .iter()
            .enumerate()
            .skip(window.scroll)
            .take(CLI_PICKER_VISIBLE)
        {
            let is_selected = i == dialog.cli_picker_idx;
            let style = picker_item_style(accent, is_selected);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {} ", if is_selected { "›" } else { " " }),
                    style,
                ),
                Span::styled(dialog.available_clis[*cli_idx].as_str().to_string(), style),
            ]));
        }
    }

    let footer = if filtered_clis.is_empty() {
        "    Backspace clear  Esc close".to_string()
    } else if total_matches > CLI_PICKER_VISIBLE {
        format!("    … {total_matches}/{total} harnesses  ↑↓ scroll  Enter/Esc close")
    } else {
        format!("    {total_matches}/{total} harnesses  type to filter  Enter/Esc close")
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(theme.dim_text),
    )));
}

fn append_model_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    model_field: usize,
    theme: &Theme,
) {
    lines.push(Line::from(vec![
        Span::styled("  Model: ", Style::default().fg(theme.dim_text)),
        Span::styled(
            model_value(dialog),
            focus_style(dialog.field, model_field, accent),
        ),
    ]));

    append_model_picker_rows(lines, dialog, accent, model_field, theme);
    lines.push(Line::from(""));
}

fn model_value(dialog: &NewAgentDialog) -> String {
    if dialog.model.is_empty() {
        "(optional — Space to browse)".to_string()
    } else {
        format!("{}▏", dialog.model)
    }
}

fn append_effort_section(lines: &mut Vec<Line<'static>>, dialog: &NewAgentDialog, theme: &Theme) {
    let value = if dialog.effort.is_empty() {
        "-".to_string()
    } else {
        dialog.effort.clone()
    };
    lines.push(Line::from(vec![
        Span::styled("  Effort: ", Style::default().fg(theme.dim_text)),
        Span::styled(value, Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(""));
}

fn append_model_picker_rows(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    model_field: usize,
    theme: &Theme,
) {
    if dialog.field != model_field || !dialog.model_picker_open {
        return;
    }
    if dialog.model_suggestions.is_empty() {
        return;
    }

    let total = dialog.model_suggestions.len();
    let window = PickerWindow::new(dialog.model_suggestion_idx, total, MODEL_PICKER_VISIBLE);

    for (i, entry) in dialog
        .model_suggestions
        .iter()
        .enumerate()
        .skip(window.scroll)
        .take(MODEL_PICKER_VISIBLE)
    {
        let is_selected = i == dialog.model_suggestion_idx;
        let style = picker_item_style(accent, is_selected);
        lines.push(Line::from(vec![
            Span::styled(
                format!("    {} ", if is_selected { "›" } else { " " }),
                style,
            ),
            Span::styled(truncate_str(&entry.id, 38), style),
            Span::styled(
                format!(" [{}]", entry.provider),
                picker_detail_style(is_selected, style, theme),
            ),
        ]));
    }

    if total > MODEL_PICKER_VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("    … {total} models  ↑↓ scroll  Enter accept  Esc close"),
            Style::default().fg(theme.dim_text),
        )));
    }
}

fn append_session_picker_rows(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    theme: &Theme,
) {
    if !dialog.session_picker_open {
        return;
    }

    let total = dialog.session_entries.len();
    let window = PickerWindow::new(dialog.session_picker_idx, total, SESSION_PICKER_VISIBLE);

    if total == 0 {
        lines.push(Line::from(Span::styled(
            "    (no sessions found)",
            Style::default().fg(theme.dim_text),
        )));
        return;
    }

    for (i, (id, label)) in dialog
        .session_entries
        .iter()
        .enumerate()
        .skip(window.scroll)
        .take(SESSION_PICKER_VISIBLE)
    {
        let is_selected = i == dialog.session_picker_idx;
        let style = picker_item_style(Color::Cyan, is_selected);
        lines.push(Line::from(vec![
            Span::styled(
                format!("    {} ", if is_selected { "›" } else { " " }),
                style,
            ),
            Span::styled(truncate_str(id, 18), style),
            Span::styled(
                format!("  {}", truncate_str(label, 36)),
                picker_detail_style(is_selected, style, theme),
            ),
        ]));
    }

    if total > SESSION_PICKER_VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("    … {total} sessions  ↑↓ scroll  Enter accept  Esc close"),
            Style::default().fg(theme.dim_text),
        )));
    }
}

/// Rows for the canopy-native session-resume picker (C12): each row shows
/// name, harness and last-active, most-recent-first (FR 2, 3).
fn append_session_resume_picker_rows(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    theme: &Theme,
) {
    let Some(picker) = &dialog.session_resume_picker else {
        return;
    };

    let total = picker.sessions.len();
    let window = PickerWindow::new(picker.index, total, SESSION_RESUME_PICKER_VISIBLE);

    for (i, session) in picker
        .sessions
        .iter()
        .enumerate()
        .skip(window.scroll)
        .take(SESSION_RESUME_PICKER_VISIBLE)
    {
        let is_selected = i == picker.index;
        let style = picker_item_style(Color::Cyan, is_selected);
        lines.push(Line::from(vec![
            Span::styled(
                format!("    {} ", if is_selected { "›" } else { " " }),
                style,
            ),
            Span::styled(truncate_str(&session.name, 22), style),
            Span::styled(
                format!("  {}", truncate_str(&session.cli, 12)),
                picker_detail_style(is_selected, style, theme),
            ),
            Span::styled(
                format!("  {}", truncate_str(&session.last_active, 20)),
                picker_detail_style(is_selected, style, theme),
            ),
        ]));
    }

    if total > SESSION_RESUME_PICKER_VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("    … {total} sessions  ↑↓ scroll  Enter resume  Esc cancel"),
            Style::default().fg(theme.dim_text),
        )));
    }
}

fn append_prompt_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    layout: FieldLayout,
    field_width: usize,
    theme: &Theme,
) {
    push_spaced_row(
        lines,
        background_prompt_label(dialog, accent, layout.prompt, theme),
    );
    append_prompt_input_block(lines, dialog, accent, layout.prompt, field_width, theme);
    push_spaced_row(
        lines,
        background_target_row(dialog, accent, layout.extra, theme),
    );
}

fn background_prompt_label(
    dialog: &NewAgentDialog,
    accent: Color,
    prompt_field: usize,
    theme: &Theme,
) -> Line<'static> {
    let focus = focus_style(dialog.field, prompt_field, accent);
    Line::from(vec![
        Span::styled("  Prompt: ", Style::default().fg(theme.dim_text)),
        Span::styled(
            format!(" {} rows ", PROMPT_VISIBLE_ROWS),
            focus.add_modifier(Modifier::DIM),
        ),
    ])
}

/// Wrap the prompt char-accurately at `field_width` and render `PROMPT_VISIBLE_ROWS`
/// lines starting from `dialog.prompt_scroll`, drawing a block cursor at
/// `dialog.prompt_cursor`. Mirrors the math in `prompt_visual_line_count` so
/// scrolling, the input handler, and the render stay in lockstep.
#[allow(clippy::too_many_arguments)]
fn append_prompt_input_block(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    prompt_field: usize,
    field_width: usize,
    theme: &Theme,
) {
    let field_width = field_width.max(1);
    let indent = "  ";
    let is_focused = dialog.field == prompt_field;
    let placeholder = "enter agent prompt…";
    let cursor_style = if is_focused {
        Style::default()
            .fg(Color::Black)
            .bg(accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim_text)
    };
    let base_style = if is_focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(theme.dim_text)
    };

    for row in 0..PROMPT_VISIBLE_ROWS {
        let line_index = dialog.prompt_scroll + row;
        let mut spans: Vec<Span<'static>> =
            vec![Span::styled(indent, Style::default().fg(theme.dim_text))];
        if dialog.prompt.is_empty() {
            // Empty prompt: show placeholder only on the first row, otherwise
            // leave a blank cell so the cursor still has a visible home.
            if line_index == 0 {
                if is_focused && dialog.prompt_cursor == 0 {
                    spans.push(Span::styled(" ", cursor_style));
                } else {
                    spans.push(Span::styled(
                        placeholder.chars().take(field_width).collect::<String>(),
                        Style::default()
                            .fg(theme.dim_text)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
            } else if is_focused {
                spans.push(Span::styled(" ", cursor_style));
            }
            lines.push(Line::from(spans));
            continue;
        }

        let char_range = prompt_visual_line_range(dialog, line_index, field_width);
        let chars_in_row: String = dialog
            .prompt
            .chars()
            .skip(char_range.0)
            .take(char_range.1 - char_range.0)
            .collect();
        let mut col = 0usize;
        let mut char_pos = char_range.0;
        for ch in chars_in_row.chars() {
            if ch == '\n' {
                char_pos += 1;
                continue;
            }
            if col >= field_width {
                break;
            }
            let style = if is_focused && char_pos == dialog.prompt_cursor {
                cursor_style
            } else {
                base_style
            };
            spans.push(Span::styled(ch.to_string(), style));
            col += 1;
            char_pos += 1;
        }
        // Cursor at the end of the last row when it sits on a non-existent
        // cell — draw a highlighted blank so the caret stays visible.
        if is_focused
            && dialog.prompt_cursor == char_pos
            && line_index + 1 >= prompt_visual_line_count(dialog, field_width)
        {
            spans.push(Span::styled(" ", cursor_style));
        }
        lines.push(Line::from(spans));
    }
}

/// Char index range `[start, end)` that maps to `visual_line_index` when the
/// prompt is wrapped at `field_width`. Hard newlines count as line breaks and
/// are skipped by the renderer (which flushes the current line at `\n`).
fn prompt_visual_line_range(
    dialog: &NewAgentDialog,
    visual_line_index: usize,
    field_width: usize,
) -> (usize, usize) {
    let field_width = field_width.max(1);
    let mut line_start = 0usize;
    let mut line_index = 0usize;
    let mut col = 0usize;
    let iter = dialog.prompt.char_indices();
    for (_, ch) in iter {
        if ch == '\n' {
            if line_index == visual_line_index {
                return (line_start, line_start + col);
            }
            line_index += 1;
            line_start += col + 1; // skip the '\n' itself
            col = 0;
            continue;
        }
        if col >= field_width {
            if line_index == visual_line_index {
                return (line_start, line_start + field_width);
            }
            line_index += 1;
            line_start += field_width;
            col = 0;
        }
        col += 1;
    }
    if line_index == visual_line_index {
        return (line_start, line_start + col);
    }
    (dialog.prompt.chars().count(), dialog.prompt.chars().count())
}

/// Number of visual lines the prompt occupies at `field_width`. Empty prompts
/// count as 1 line so the cursor / placeholder always have a home.
pub(crate) fn prompt_visual_line_count(dialog: &NewAgentDialog, field_width: usize) -> usize {
    if dialog.prompt.is_empty() {
        return 1;
    }
    let field_width = field_width.max(1);
    let mut lines = 1usize;
    let mut col = 0usize;
    for ch in dialog.prompt.chars() {
        if ch == '\n' {
            lines += 1;
            col = 0;
            continue;
        }
        if col >= field_width {
            lines += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    lines
}

/// Mirrors the calculation in `event::prompt_template::prompt_field_width` so
/// the renderer's wrap math, the cursor's visual line index, and the input
/// handler's `Up`/`Down` row jumps all agree on narrow terminals.
pub(crate) fn prompt_field_width(frame_area: Rect) -> usize {
    let term_width = frame_area.width.max(1);
    let max_dialog_w = term_width.saturating_sub(2).max(1);
    let preferred_dialog_w = term_width.saturating_mul(65) / 100;
    let min_dialog_w = 40u16.min(max_dialog_w);
    let dialog_width = preferred_dialog_w.clamp(min_dialog_w, max_dialog_w);
    // dialog borders (2) + content indent (2) + 1 cell breathing room
    (dialog_width.saturating_sub(5) as usize).max(10)
}

pub(crate) const PROMPT_VISIBLE_ROWS: usize = 3;

fn background_target_row(
    dialog: &NewAgentDialog,
    accent: Color,
    extra_field: usize,
    theme: &Theme,
) -> Line<'static> {
    match dialog.background_trigger {
        BackgroundTrigger::Cron => Line::from(vec![
            Span::styled("  Cron:  ", Style::default().fg(theme.dim_text)),
            Span::styled(
                cron_value(dialog),
                focus_style(dialog.field, extra_field, accent),
            ),
        ]),
        BackgroundTrigger::Watch => Line::from(vec![
            Span::styled("  Path:  ", Style::default().fg(theme.dim_text)),
            Span::styled(
                truncate_str(&dialog.watch_path, 50),
                focus_style(dialog.field, extra_field, accent),
            ),
        ]),
    }
}

fn cron_value(dialog: &NewAgentDialog) -> String {
    if dialog.cron_expr.is_empty() {
        " * * * * *  (min hr dom mon dow · local time)".to_string()
    } else {
        format!(" {}▏ (local time)", dialog.cron_expr)
    }
}

fn append_identity_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    identity_field: usize,
    theme: &Theme,
) {
    let label = identity_label(dialog);
    let locked = dialog.seed_options.len() <= 1;
    let mut identity_row = vec![Span::styled(
        "  Identity: ",
        Style::default().fg(theme.dim_text),
    )];
    identity_row.extend(selector_spans(
        &label,
        identity_field,
        locked,
        accent,
        dialog.field,
        theme,
    ));
    push_spaced_row(lines, Line::from(identity_row));
}

fn identity_label(dialog: &NewAgentDialog) -> String {
    match dialog.seed_options.get(dialog.seed_index) {
        Some(SeedOption::Seed { name, .. }) => truncate_with_ellipsis(name, 30),
        Some(SeedOption::PlantNewSeed) => "🌱 New".to_string(),
        _ => "None".to_string(),
    }
}

fn append_yolo_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    yolo_field: usize,
    theme: &Theme,
) {
    let has_yolo = dialog.selected_yolo_flag().is_some();
    let checkbox = if dialog.yolo_mode { "◉" } else { "○" };
    let checkbox_style = if dialog.field == yolo_field {
        focus_style(dialog.field, yolo_field, accent)
    } else if dialog.yolo_mode {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let mut spans = vec![
        Span::styled("  Yolo:  ", Style::default().fg(theme.dim_text)),
        Span::styled(format!("{checkbox} Autonomous mode"), checkbox_style),
    ];
    if !has_yolo {
        spans.push(Span::styled(
            "  (not supported by this harness)",
            Style::default().fg(theme.dim_text),
        ));
    } else if dialog.yolo_mode {
        spans.push(Span::styled(
            "  ⚠ agent acts without approval",
            Style::default().fg(Color::Yellow),
        ));
    }

    push_spaced_row(lines, Line::from(spans));
}

fn append_sandbox_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    sandbox_field: usize,
    theme: &Theme,
) {
    if sandbox_field == 0 {
        return;
    }
    let checkbox = if dialog.sandbox_mode { "◉" } else { "○" };
    let checkbox_style = if dialog.field == sandbox_field {
        focus_style(dialog.field, sandbox_field, accent)
    } else if dialog.sandbox_mode {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let mut spans = vec![
        Span::styled("  Sandbox:  ", Style::default().fg(theme.dim_text)),
        Span::styled(format!("{checkbox} Isolated worktree"), checkbox_style),
    ];
    if dialog.sandbox_mode {
        spans.push(Span::styled(
            "  no protocol file in repo",
            Style::default().fg(Color::Cyan),
        ));
    }

    push_spaced_row(lines, Line::from(spans));
}

#[allow(clippy::too_many_arguments)]
fn append_directory_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    dir_field: usize,
    hide_dir: bool,
    browser_field: usize,
    theme: &Theme,
) {
    if !hide_dir {
        push_spaced_row(lines, working_dir_row(dialog, accent, dir_field, theme));
    }
    if dialog.dir_entries.is_empty() {
        return;
    }

    lines.extend(dir_browser_lines(
        dialog,
        accent,
        dialog.field == browser_field,
        theme,
    ));
}

fn working_dir_row(
    dialog: &NewAgentDialog,
    accent: Color,
    dir_field: usize,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled("  Dir:   ", Style::default().fg(theme.dim_text)),
        Span::styled(
            truncate_str(&dialog.working_dir, 50),
            focus_style(dialog.field, dir_field, accent),
        ),
    ])
}

fn terminal_shell_row(dialog: &NewAgentDialog, accent: Color, theme: &Theme) -> Line<'static> {
    let shell_display = if dialog.available_shells.len() > 1 {
        format!("◂ {} ▸", dialog.selected_shell())
    } else {
        dialog.selected_shell().to_string()
    };

    Line::from(vec![
        Span::styled("  Shell: ", Style::default().fg(theme.dim_text)),
        Span::styled(
            format!(" {} ", shell_display),
            focus_style(dialog.field, TERMINAL_SHELL_FIELD, accent),
        ),
    ])
}

// ── Navigation / picker rendering ────────────────────────────────────────────

fn dir_browser_lines(
    dialog: &NewAgentDialog,
    accent: Color,
    focused: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let filtered = dialog.filtered_dir_entries();
    let window = PickerWindow::new(dialog.dir_selected, filtered.len(), DIR_BROWSER_VISIBLE);
    let mut lines = vec![filter_row(
        "  🔍 ",
        &dialog.dir_filter,
        focused,
        accent,
        theme,
    )];

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no matches)",
            Style::default().fg(theme.dim_text),
        )));
    } else {
        for (i, entry) in filtered
            .iter()
            .enumerate()
            .skip(window.scroll)
            .take(DIR_BROWSER_VISIBLE)
        {
            let style = picker_item_style(accent, i == dialog.dir_selected);
            lines.push(Line::from(Span::styled(format!("    {entry}"), style)));
        }
    }

    let footer = if filtered.is_empty() {
        "    0 items".to_string()
    } else {
        let up = if window.has_above { "↑ " } else { "  " };
        let down = if window.has_below { " ↓" } else { "  " };
        format!(
            "    {up}{}/{}{down}",
            dialog.dir_selected + 1,
            filtered.len()
        )
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(theme.dim_text),
    )));
    lines.push(Line::from(""));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::dialog::new_agent::NewAgentDialog;

    fn dialog_with(prompt: &str) -> NewAgentDialog {
        let mut d = NewAgentDialog::new(Some("."));
        d.prompt = prompt.to_string();
        d.prompt_cursor = prompt.chars().count();
        d
    }

    #[test]
    fn empty_prompt_counts_as_one_visual_line() {
        let d = dialog_with("");
        assert_eq!(prompt_visual_line_count(&d, 30), 1);
    }

    #[test]
    fn single_short_line_counts_as_one_visual_line() {
        let d = dialog_with("hello world");
        assert_eq!(prompt_visual_line_count(&d, 30), 1);
    }

    #[test]
    fn hard_newline_breaks_visual_line() {
        let d = dialog_with("first\nsecond");
        assert_eq!(prompt_visual_line_count(&d, 30), 2);
    }

    #[test]
    fn overflow_wraps_at_field_width() {
        let d = dialog_with("abcdefghij"); // 10 chars
        assert_eq!(prompt_visual_line_count(&d, 4), 3); // 4 + 4 + 2
    }

    #[test]
    fn visual_line_range_respects_hard_newline_boundary() {
        let d = dialog_with("abc\ndef");
        assert_eq!(prompt_visual_line_range(&d, 0, 30), (0, 3));
        assert_eq!(prompt_visual_line_range(&d, 1, 30), (4, 7));
    }

    #[test]
    fn visual_line_range_respects_soft_wrap_boundary() {
        let d = dialog_with("abcdefghij");
        assert_eq!(prompt_visual_line_range(&d, 0, 4), (0, 4));
        assert_eq!(prompt_visual_line_range(&d, 1, 4), (4, 8));
        assert_eq!(prompt_visual_line_range(&d, 2, 4), (8, 10));
    }

    #[test]
    fn out_of_range_line_returns_past_end() {
        let d = dialog_with("abc");
        assert_eq!(prompt_visual_line_range(&d, 5, 30), (3, 3));
    }

    #[test]
    fn prompt_field_width_clamps_to_terminal_width() {
        use ratatui::layout::Rect;
        // Tiny terminal: 30 cols → preferred 19, min 30, clamp 30. minus 5 = 25.
        let w = prompt_field_width(Rect::new(0, 0, 30, 20));
        assert!(w <= 25);
        assert!(w >= 10);
    }

    #[test]
    fn prompt_field_width_grows_with_terminal() {
        use ratatui::layout::Rect;
        let w_small = prompt_field_width(Rect::new(0, 0, 80, 20));
        let w_large = prompt_field_width(Rect::new(0, 0, 200, 20));
        assert!(w_large > w_small);
    }

    #[test]
    fn background_dialog_height_includes_prompt_block() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Background;
        let h = dialog_height(&d, &[]);
        // base 15 + 1 label row + 3 input rows = 19 when no pickers / no dir entries
        assert!(h >= 19, "expected >= 19, got {h}");
    }

    fn spans_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn selector_uses_discreet_angle_glyphs_not_solid_triangles() {
        // Focused and unfocused variants both use the ‹ › glyphs, never ◀ ▶.
        for current in [TYPE_FIELD, INTERACTIVE_MODE_FIELD] {
            let text = spans_text(&selector_spans(
                "Background",
                TYPE_FIELD,
                false,
                Color::Cyan,
                current,
                &Theme::classic(),
            ));
            assert!(text.contains('‹') && text.contains('›'), "got {text:?}");
            assert!(!text.contains('◀') && !text.contains('▶'), "got {text:?}");
            assert!(text.contains("Background"));
        }
    }

    #[test]
    fn locked_selector_has_no_lateral_arrows() {
        let text = spans_text(&selector_spans(
            "Interactive",
            TYPE_FIELD,
            true,
            Color::Cyan,
            TYPE_FIELD,
            &Theme::classic(),
        ));
        assert!(!text.contains('‹') && !text.contains('›'));
        assert!(!text.contains('◀') && !text.contains('▶'));
    }

    #[test]
    fn dialog_title_new_agent() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Interactive;
        assert_eq!(dialog_title(&d), " New Agent ");
    }

    #[test]
    fn dialog_title_edit_background() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Background;
        d.edit_id = Some("test-id".to_string());
        assert_eq!(dialog_title(&d), " Edit Background ");
    }

    #[test]
    fn dialog_title_edit_interactive() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Interactive;
        d.edit_id = Some("test-id".to_string());
        assert_eq!(dialog_title(&d), " Edit Agent ");
    }

    #[test]
    fn dialog_title_edit_terminal() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Terminal;
        d.edit_id = Some("test-id".to_string());
        assert_eq!(dialog_title(&d), " Edit Terminal ");
    }

    #[test]
    fn task_type_label_all_variants() {
        assert_eq!(task_type_label(NewTaskType::Interactive), "Interactive");
        assert_eq!(task_type_label(NewTaskType::Terminal), "Terminal");
        assert_eq!(task_type_label(NewTaskType::Background), "Background");
    }

    #[test]
    fn interactive_mode_label_all_variants() {
        assert_eq!(interactive_mode_label(NewTaskMode::Interactive), "New");
        assert_eq!(interactive_mode_label(NewTaskMode::Resume), "Resume");
    }

    #[test]
    fn background_trigger_label_all_variants() {
        assert_eq!(background_trigger_label(BackgroundTrigger::Cron), "Cron");
        assert_eq!(background_trigger_label(BackgroundTrigger::Watch), "Watch");
    }

    #[test]
    fn filter_display_empty() {
        assert_eq!(filter_display(""), "type to filter");
    }

    #[test]
    fn filter_display_non_empty() {
        assert_eq!(filter_display("claude"), "claude");
    }

    #[test]
    fn truncate_with_ellipsis_short() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_exact() {
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_long() {
        let result = truncate_with_ellipsis("hello world", 5);
        assert!(result.contains("…"));
        assert!(result.chars().count() <= 6); // 5 + ellipsis
    }

    #[test]
    fn cron_value_empty() {
        let mut d = NewAgentDialog::new(Some("."));
        d.cron_expr = String::new();
        let val = cron_value(&d);
        assert!(val.contains("* * * * *"));
    }

    #[test]
    fn cron_value_with_expr() {
        let mut d = NewAgentDialog::new(Some("."));
        d.cron_expr = "30 9 * * 1-5".to_string();
        let val = cron_value(&d);
        assert!(val.contains("30 9 * * 1-5"));
    }

    #[test]
    fn help_text_interactive() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Interactive;
        let text = help_text(&d);
        assert!(text.contains("launch"));
    }

    #[test]
    fn help_text_background() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Background;
        let text = help_text(&d);
        assert!(text.contains("create"));
    }

    #[test]
    fn help_text_terminal() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_type = NewTaskType::Terminal;
        let text = help_text(&d);
        assert!(text.contains("launch"));
    }

    #[test]
    fn model_value_empty() {
        let mut d = NewAgentDialog::new(Some("."));
        d.model = String::new();
        let val = model_value(&d);
        assert!(val.contains("optional"));
    }

    #[test]
    fn model_value_with_model() {
        let mut d = NewAgentDialog::new(Some("."));
        d.model = "claude-3-opus".to_string();
        let val = model_value(&d);
        assert!(val.contains("claude-3-opus"));
    }

    #[test]
    fn identity_label_with_seed() {
        use crate::tui::app::dialog::new_agent::SeedOption;
        let mut d = NewAgentDialog::new(Some("."));
        d.seed_options = vec![SeedOption::Seed {
            name: "My Seed".to_string(),
            id: "seed-1".to_string(),
        }];
        let label = identity_label(&d);
        assert_eq!(label, "My Seed");
    }

    #[test]
    fn identity_label_plant_new() {
        use crate::tui::app::dialog::new_agent::SeedOption;
        let mut d = NewAgentDialog::new(Some("."));
        d.seed_options = vec![SeedOption::PlantNewSeed];
        let label = identity_label(&d);
        assert!(label.contains("New"));
    }

    #[test]
    fn identity_label_none() {
        let mut d = NewAgentDialog::new(Some("."));
        d.seed_options = vec![];
        let label = identity_label(&d);
        assert_eq!(label, "None");
    }

    #[test]
    fn field_layout_for_interactive() {
        let layout = FieldLayout::for_task(NewTaskType::Interactive);
        assert_eq!(layout.cli, 2);
        assert_eq!(layout.identity, 5);
    }

    #[test]
    fn field_layout_for_terminal() {
        let layout = FieldLayout::for_task(NewTaskType::Terminal);
        assert_eq!(layout.cli, 0);
        assert_eq!(layout.identity, 0);
    }

    #[test]
    fn field_layout_for_background() {
        let layout = FieldLayout::for_task(NewTaskType::Background);
        assert_eq!(layout.cli, 2);
        assert_eq!(layout.identity, 0);
    }

    #[test]
    fn picker_window_basic() {
        let w = PickerWindow::new(5, 20, 6);
        assert_eq!(w.scroll, 0);
        assert!(!w.has_above);
        assert!(w.has_below);
    }

    #[test]
    fn picker_window_scrolled() {
        let w = PickerWindow::new(15, 20, 6);
        assert!(w.scroll > 0);
        assert!(w.has_above);
    }

    #[test]
    fn picker_window_at_end() {
        let w = PickerWindow::new(19, 20, 6);
        assert!(!w.has_below);
    }

    #[test]
    fn picker_window_empty() {
        let w = PickerWindow::new(0, 0, 6);
        assert!(!w.has_above);
        assert!(!w.has_below);
    }

    #[test]
    fn prompt_field_width_very_small_terminal() {
        let w = prompt_field_width(ratatui::layout::Rect::new(0, 0, 10, 10));
        assert!(w >= 10);
    }

    #[test]
    fn prompt_field_width_large_terminal() {
        let w = prompt_field_width(ratatui::layout::Rect::new(0, 0, 200, 40));
        assert!(w > 50);
    }

    #[test]
    fn prompt_visual_line_count_mixed_wrap_and_newlines() {
        let d = dialog_with("abcde\n12345678");
        // "abcde" = 1 line, "12345678" at width 4 = 2 lines (4+4)
        // But hard newline counts as a break point too
        let count = prompt_visual_line_count(&d, 4);
        assert!(count >= 3, "expected >= 3, got {count}");
    }

    #[test]
    fn prompt_visual_line_range_at_boundary() {
        let d = dialog_with("abcdefghij");
        // At width 4: line 0 = [0,4), line 1 = [4,8), line 2 = [8,10)
        assert_eq!(prompt_visual_line_range(&d, 2, 4), (8, 10));
    }

    #[test]
    fn session_picker_label_no_selection() {
        let mut d = NewAgentDialog::new(Some("."));
        d.selected_session = None;
        let label = session_picker_label(&d);
        assert!(label.contains("pick session"));
    }

    #[test]
    fn session_picker_label_with_selection() {
        let mut d = NewAgentDialog::new(Some("."));
        d.selected_session = Some(("id-1".to_string(), "My Session".to_string()));
        let label = session_picker_label(&d);
        assert!(label.contains("My Session"));
    }

    #[test]
    fn focus_style_focused() {
        let style = focus_style(0, 0, Color::Cyan);
        assert_eq!(style.bg, Some(Color::Cyan));
    }

    #[test]
    fn focus_style_unfocused() {
        let style = focus_style(1, 0, Color::Cyan);
        assert_eq!(style.fg, Some(Color::White));
    }

    #[test]
    fn picker_item_style_selected() {
        let style = picker_item_style(Color::Cyan, true);
        assert_eq!(style.bg, Some(Color::Cyan));
    }

    #[test]
    fn picker_item_style_not_selected() {
        let style = picker_item_style(Color::Cyan, false);
        assert_eq!(style.fg, Some(Color::White));
    }

    #[test]
    fn picker_detail_style_selected() {
        let selected_style = Style::default().fg(Color::Black).bg(Color::Cyan);
        let style = picker_detail_style(true, selected_style, &Theme::classic());
        assert_eq!(style.bg, Some(Color::Cyan));
    }

    #[test]
    fn picker_detail_style_not_selected() {
        let selected_style = Style::default().fg(Color::Black).bg(Color::Cyan);
        let theme = Theme::classic();
        let style = picker_detail_style(false, selected_style, &theme);
        assert_eq!(style.fg, Some(theme.dim_text));
    }

    #[test]
    fn session_picker_label_truncates_long_title() {
        let mut d = NewAgentDialog::new(Some("."));
        d.selected_session = Some(("id".to_string(), "A".repeat(100)));
        let label = session_picker_label(&d);
        assert!(label.chars().count() < 60);
    }

    #[test]
    fn interactive_mode_row_resume_unconfigured() {
        let mut d = NewAgentDialog::new(Some("."));
        d.task_mode = NewTaskMode::Resume;
        d.session_entries = vec![];
        d.cli_configs = vec![None];
        d.cli_index = 0;
        let line = interactive_mode_row(&d, Color::Cyan, &Theme::classic());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("not configured"));
    }

    #[test]
    fn push_spaced_row_adds_empty_line() {
        let mut lines = Vec::new();
        push_spaced_row(&mut lines, Line::from("test"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].spans.is_empty());
    }

    /// task_type_label returns correct labels for all task types
    #[test]
    fn task_type_label_interactive() {
        assert_eq!(task_type_label(NewTaskType::Interactive), "Interactive");
    }

    #[test]
    fn task_type_label_terminal() {
        assert_eq!(task_type_label(NewTaskType::Terminal), "Terminal");
    }

    #[test]
    fn task_type_label_background() {
        assert_eq!(task_type_label(NewTaskType::Background), "Background");
    }

    /// interactive_mode_label returns correct labels for modes
    #[test]
    fn interactive_mode_label_new() {
        assert_eq!(interactive_mode_label(NewTaskMode::Interactive), "New");
    }

    #[test]
    fn interactive_mode_label_resume() {
        assert_eq!(interactive_mode_label(NewTaskMode::Resume), "Resume");
    }

    /// background_trigger_label returns correct labels for triggers
    #[test]
    fn background_trigger_label_cron() {
        assert_eq!(background_trigger_label(BackgroundTrigger::Cron), "Cron");
    }

    #[test]
    fn background_trigger_label_watch() {
        assert_eq!(background_trigger_label(BackgroundTrigger::Watch), "Watch");
    }

    /// truncate_with_ellipsis doesn't truncate short strings
    #[test]
    fn truncate_with_ellipsis_short_string() {
        let result = truncate_with_ellipsis("hello", 10);
        assert_eq!(result, "hello");
    }

    /// truncate_with_ellipsis adds ellipsis when truncating
    #[test]
    fn truncate_with_ellipsis_long_string() {
        let result = truncate_with_ellipsis("hello world test", 5);
        assert_eq!(result, "hello…");
    }

    /// truncate_with_ellipsis exact length has no ellipsis
    #[test]
    fn truncate_with_ellipsis_exact_length() {
        let result = truncate_with_ellipsis("hello", 5);
        assert_eq!(result, "hello");
    }

    /// truncate_with_ellipsis handles empty string
    #[test]
    fn truncate_with_ellipsis_empty_string() {
        let result = truncate_with_ellipsis("", 5);
        assert_eq!(result, "");
    }

    /// truncate_with_ellipsis handles zero max_chars
    #[test]
    fn truncate_with_ellipsis_zero_max_chars() {
        let result = truncate_with_ellipsis("hello", 0);
        assert_eq!(result, "…");
    }

    /// truncate_with_ellipsis handles unicode characters correctly by char count
    #[test]
    fn truncate_with_ellipsis_unicode() {
        // "こんにちは" is 5 chars; taking 3 gives "こんに" + ellipsis
        let result = truncate_with_ellipsis("こんにちは世界", 3);
        assert_eq!(result, "こんに…");
    }

    /// filter_display handles special characters
    #[test]
    fn filter_display_special_chars() {
        assert_eq!(filter_display("test@#$%"), "test@#$%");
    }
}
