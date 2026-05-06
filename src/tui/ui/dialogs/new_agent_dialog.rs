use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, truncate_str, DIM};
use crate::tui::app::{
    dialog::new_agent::{BackgroundTrigger, NewAgentDialog, NewTaskMode, NewTaskType},
    types::App,
};

const TYPE_FIELD: usize = 0;
const INTERACTIVE_MODE_FIELD: usize = 1;
const BACKGROUND_TRIGGER_FIELD: usize = 1;
const TERMINAL_DIR_FIELD: usize = 1;
const TERMINAL_SHELL_FIELD: usize = 2;

const CLI_PICKER_VISIBLE: usize = 6;
const MODEL_PICKER_VISIBLE: usize = 5;
const SESSION_PICKER_VISIBLE: usize = 6;
const DIR_BROWSER_VISIBLE: usize = 10;

#[derive(Clone, Copy)]
struct FieldLayout {
    cli: usize,
    model: usize,
    prompt: usize,
    extra: usize,
    dir: usize,
    yolo: usize,
}

impl FieldLayout {
    fn for_task(task_type: NewTaskType) -> Self {
        match task_type {
            NewTaskType::Interactive => Self {
                cli: 2,
                model: 3,
                prompt: 4,
                extra: 5,
                dir: 3,
                yolo: 4,
            },
            NewTaskType::Terminal => Self {
                cli: 0,
                model: 3,
                prompt: 4,
                extra: 5,
                dir: TERMINAL_DIR_FIELD,
                yolo: 4,
            },
            NewTaskType::Background => Self {
                cli: 2,
                model: 3,
                prompt: 4,
                extra: 5,
                dir: 6,
                yolo: 4,
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

pub fn draw_new_agent_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.new_agent_dialog else {
        return;
    };

    let accent = dialog.selected_accent_color();
    let filtered_clis = dialog.filtered_cli_indices();
    let area = centered_rect(65, dialog_height(dialog, &filtered_clis), frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(dialog_title(dialog))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = build_dialog_lines(dialog, accent, &filtered_clis);
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── Form state ───────────────────────────────────────────────────────────────

fn dialog_height(dialog: &NewAgentDialog, filtered_clis: &[usize]) -> u16 {
    let dir_rows = dir_browser_rows(dialog);
    let base_height = match dialog.task_type {
        NewTaskType::Interactive => 12 + dir_rows,
        NewTaskType::Terminal => 10 + dir_rows,
        NewTaskType::Background => 15 + dir_rows,
    };

    base_height + cli_picker_rows(dialog, filtered_clis.len()) + model_picker_rows(dialog)
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
) -> Vec<Line<'static>> {
    let layout = FieldLayout::for_task(dialog.task_type);
    let mut lines = dialog_header_lines(dialog, accent);

    match dialog.task_type {
        NewTaskType::Interactive => {
            append_interactive_sections(&mut lines, dialog, accent, filtered_clis, layout);
        }
        NewTaskType::Terminal => append_terminal_sections(&mut lines, dialog, accent),
        NewTaskType::Background => {
            append_background_sections(&mut lines, dialog, accent, filtered_clis, layout);
        }
    }

    lines.push(Line::from(Span::styled(
        help_text(dialog),
        Style::default().fg(DIM),
    )));
    lines
}

fn dialog_header_lines(dialog: &NewAgentDialog, accent: Color) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Type:  ", Style::default().fg(DIM)),
            selector_span(
                task_type_label(dialog.task_type),
                TYPE_FIELD,
                dialog.is_edit_mode(),
                accent,
                dialog.field,
            ),
        ]),
        Line::from(""),
    ]
}

fn selector_span(
    value: &str,
    field: usize,
    locked: bool,
    accent: Color,
    current_field: usize,
) -> Span<'static> {
    if locked {
        return Span::styled(
            format!("  {value}  "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        );
    }

    Span::styled(
        format!(" ◀ {value} ▶ "),
        focus_style(current_field, field, accent),
    )
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

fn picker_detail_style(selected: bool, selected_style: Style) -> Style {
    if selected {
        selected_style
    } else {
        Style::default().fg(DIM)
    }
}

fn filter_row(prefix: &str, filter: &str, focused: bool, accent: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            prefix.to_string(),
            if focused {
                Style::default().fg(accent)
            } else {
                Style::default().fg(DIM)
            },
        ),
        Span::styled(
            filter_display(filter),
            if filter.is_empty() {
                Style::default().fg(DIM)
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

fn append_interactive_sections(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    layout: FieldLayout,
) {
    if !dialog.is_edit_mode() {
        push_spaced_row(lines, interactive_mode_row(dialog, accent));
    }

    append_cli_section(lines, dialog, accent, filtered_clis, layout.cli);
    append_session_picker_rows(lines, dialog);
    append_yolo_section(lines, dialog, accent, layout.yolo);
    append_directory_section(lines, dialog, accent, layout.dir, false, layout.dir);
}

fn append_terminal_sections(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
) {
    append_directory_section(
        lines,
        dialog,
        accent,
        TERMINAL_DIR_FIELD,
        false,
        TERMINAL_DIR_FIELD,
    );
    push_spaced_row(lines, terminal_shell_row(dialog, accent));
}

fn append_background_sections(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    layout: FieldLayout,
) {
    append_trigger_section(lines, dialog, accent);
    append_cli_section(lines, dialog, accent, filtered_clis, layout.cli);
    append_model_section(lines, dialog, accent, layout.model);
    append_prompt_section(lines, dialog, accent, layout);

    let hide_dir = dialog.background_trigger == BackgroundTrigger::Watch;
    let browser_field = if hide_dir { layout.extra } else { layout.dir };
    append_directory_section(lines, dialog, accent, layout.dir, hide_dir, browser_field);
}

fn interactive_mode_row(dialog: &NewAgentDialog, accent: Color) -> Line<'static> {
    let mut spans = vec![
        Span::styled("  Session:  ", Style::default().fg(DIM)),
        selector_span(
            interactive_mode_label(dialog.task_mode),
            INTERACTIVE_MODE_FIELD,
            false,
            accent,
            dialog.field,
        ),
    ];

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

    Line::from(spans)
}

fn append_trigger_section(lines: &mut Vec<Line<'static>>, dialog: &NewAgentDialog, accent: Color) {
    push_spaced_row(
        lines,
        Line::from(vec![
            Span::styled("  Trigger:", Style::default().fg(DIM)),
            selector_span(
                background_trigger_label(dialog.background_trigger),
                BACKGROUND_TRIGGER_FIELD,
                dialog.is_edit_mode(),
                accent,
                dialog.field,
            ),
        ]),
    );
}

fn append_cli_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    cli_field: usize,
) {
    lines.push(Line::from(vec![
        Span::styled("  Harness: ", Style::default().fg(DIM)),
        Span::styled(
            format!(" {} ", dialog.selected_cli()),
            focus_style(dialog.field, cli_field, accent),
        ),
        Span::styled("  (◂▸ cycle · type/Space pick)", Style::default().fg(DIM)),
    ]));

    append_cli_picker_rows(lines, dialog, accent, filtered_clis, cli_field);
    lines.push(Line::from(""));
}

fn append_cli_picker_rows(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    filtered_clis: &[usize],
    cli_field: usize,
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
    ));

    if filtered_clis.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no matches)",
            Style::default().fg(DIM),
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
    lines.push(Line::from(Span::styled(footer, Style::default().fg(DIM))));
}

fn append_model_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    model_field: usize,
) {
    lines.push(Line::from(vec![
        Span::styled("  Model: ", Style::default().fg(DIM)),
        Span::styled(
            model_value(dialog),
            focus_style(dialog.field, model_field, accent),
        ),
    ]));

    append_model_picker_rows(lines, dialog, accent, model_field);
    lines.push(Line::from(""));
}

fn model_value(dialog: &NewAgentDialog) -> String {
    if dialog.model.is_empty() {
        "(optional — Space to browse)".to_string()
    } else {
        format!("{}▏", dialog.model)
    }
}

fn append_model_picker_rows(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    model_field: usize,
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
                picker_detail_style(is_selected, style),
            ),
        ]));
    }

    if total > MODEL_PICKER_VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("    … {total} models  ↑↓ scroll  → accept  Esc close"),
            Style::default().fg(DIM),
        )));
    }
}

fn append_session_picker_rows(lines: &mut Vec<Line<'static>>, dialog: &NewAgentDialog) {
    if !dialog.session_picker_open {
        return;
    }

    let total = dialog.session_entries.len();
    let window = PickerWindow::new(dialog.session_picker_idx, total, SESSION_PICKER_VISIBLE);

    if total == 0 {
        lines.push(Line::from(Span::styled(
            "    (no sessions found)",
            Style::default().fg(DIM),
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
                picker_detail_style(is_selected, style),
            ),
        ]));
    }

    if total > SESSION_PICKER_VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("    … {total} sessions  ↑↓ scroll  Enter accept  Esc close"),
            Style::default().fg(DIM),
        )));
    }
}

fn append_prompt_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    layout: FieldLayout,
) {
    push_spaced_row(lines, background_prompt_row(dialog, accent, layout.prompt));
    push_spaced_row(lines, background_target_row(dialog, accent, layout.extra));
}

fn background_prompt_row(
    dialog: &NewAgentDialog,
    accent: Color,
    prompt_field: usize,
) -> Line<'static> {
    Line::from(vec![
        Span::styled("  Prompt:", Style::default().fg(DIM)),
        Span::styled(
            prompt_value(dialog),
            focus_style(dialog.field, prompt_field, accent),
        ),
    ])
}

fn prompt_value(dialog: &NewAgentDialog) -> String {
    if dialog.prompt.is_empty() {
        " enter agent prompt...".to_string()
    } else {
        format!(" {}▏", dialog.prompt)
    }
}

fn background_target_row(
    dialog: &NewAgentDialog,
    accent: Color,
    extra_field: usize,
) -> Line<'static> {
    match dialog.background_trigger {
        BackgroundTrigger::Cron => Line::from(vec![
            Span::styled("  Cron:  ", Style::default().fg(DIM)),
            Span::styled(
                cron_value(dialog),
                focus_style(dialog.field, extra_field, accent),
            ),
        ]),
        BackgroundTrigger::Watch => Line::from(vec![
            Span::styled("  Path:  ", Style::default().fg(DIM)),
            Span::styled(
                truncate_str(&dialog.watch_path, 50),
                focus_style(dialog.field, extra_field, accent),
            ),
        ]),
    }
}

fn cron_value(dialog: &NewAgentDialog) -> String {
    if dialog.cron_expr.is_empty() {
        " * * * * *  (min hr dom mon dow)".to_string()
    } else {
        format!(" {}▏", dialog.cron_expr)
    }
}

fn append_yolo_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    yolo_field: usize,
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
        Span::styled("  Yolo:  ", Style::default().fg(DIM)),
        Span::styled(format!("{checkbox} Autonomous mode"), checkbox_style),
    ];
    if !has_yolo {
        spans.push(Span::styled(
            "  (not supported by this harness)",
            Style::default().fg(DIM),
        ));
    } else if dialog.yolo_mode {
        spans.push(Span::styled(
            "  ⚠ agent acts without approval",
            Style::default().fg(Color::Yellow),
        ));
    }

    push_spaced_row(lines, Line::from(spans));
}

fn append_directory_section(
    lines: &mut Vec<Line<'static>>,
    dialog: &NewAgentDialog,
    accent: Color,
    dir_field: usize,
    hide_dir: bool,
    browser_field: usize,
) {
    if !hide_dir {
        push_spaced_row(lines, working_dir_row(dialog, accent, dir_field));
    }
    if dialog.dir_entries.is_empty() {
        return;
    }

    lines.extend(dir_browser_lines(
        dialog,
        accent,
        dialog.field == browser_field,
    ));
}

fn working_dir_row(dialog: &NewAgentDialog, accent: Color, dir_field: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled("  Dir:   ", Style::default().fg(DIM)),
        Span::styled(
            truncate_str(&dialog.working_dir, 50),
            focus_style(dialog.field, dir_field, accent),
        ),
    ])
}

fn terminal_shell_row(dialog: &NewAgentDialog, accent: Color) -> Line<'static> {
    let shell_display = if dialog.available_shells.len() > 1 {
        format!("◂ {} ▸", dialog.selected_shell())
    } else {
        dialog.selected_shell().to_string()
    };

    Line::from(vec![
        Span::styled("  Shell: ", Style::default().fg(DIM)),
        Span::styled(
            format!(" {} ", shell_display),
            focus_style(dialog.field, TERMINAL_SHELL_FIELD, accent),
        ),
    ])
}

// ── Navigation / picker rendering ────────────────────────────────────────────

fn dir_browser_lines(dialog: &NewAgentDialog, accent: Color, focused: bool) -> Vec<Line<'static>> {
    let filtered = dialog.filtered_dir_entries();
    let window = PickerWindow::new(dialog.dir_selected, filtered.len(), DIR_BROWSER_VISIBLE);
    let mut lines = vec![filter_row("  🔍 ", &dialog.dir_filter, focused, accent)];

    if filtered.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no matches)",
            Style::default().fg(DIM),
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
    lines.push(Line::from(Span::styled(footer, Style::default().fg(DIM))));
    lines.push(Line::from(""));
    lines
}
