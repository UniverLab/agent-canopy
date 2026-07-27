//! Footer rendering — context-sensitive key hints + version.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::theme::Theme;
use crate::tui::app::types::{AgentEntry, App, Focus, ProjectTab, SidebarLayer};

pub(super) fn draw_footer(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let activity_available = app.activity_panel_available();
    let hints = match app.focus {
        Focus::Home => {
            let mut h = vec![("↑↓", "select"), ("n", "new")];
            if activity_available {
                h.push(("F3", "activity"));
            }
            h.push(("Shift+←→", "panels"));
            h.push(("F10", "preview"));
            h.push(("F1", "stats"));
            h
        }
        Focus::Preview => {
            if app.playground_active {
                vec![
                    ("type", "search"),
                    ("↑↓", "results"),
                    ("Enter", "search/open"),
                    ("Shift+↑↓", "agents"),
                    ("Ctrl+T", "transfer"),
                    ("Esc", "close"),
                ]
            } else if app.sidebar_layer == SidebarLayer::Knowledge {
                let mut h = vec![
                    ("↑↓", "highlight"),
                    ("Enter", "open project"),
                    ("Shift+←→", "tab"),
                ];
                h.push(("Esc", "home"));
                h
            } else {
                let is_bg = matches!(app.selected_agent(), Some(AgentEntry::Agent(_)));
                let mut h = vec![("↑↓", "nav"), ("Enter", "focus"), ("Shift+←→", "tab")];
                if is_bg {
                    h.push(("e", "edit"));
                    h.push(("d", "toggle"));
                    h.push(("F4", "delete"));
                    h.push(("r", "rerun"));
                }
                h.push(("n", "new"));
                if activity_available {
                    h.push(("F3", "activity"));
                }
                h.push(("Esc", "home"));
                h
            }
        }
        Focus::NewAgentDialog => vec![
            ("↑↓", "fields"),
            ("←→", "cycle"),
            ("Space", "pick/enter"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ],
        Focus::LaunchpadDialog => vec![
            ("Tab/←→", "toggle"),
            ("type", "mission"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ],
        Focus::Agent if app.sidebar_layer == SidebarLayer::Knowledge => {
            let mut h = vec![
                ("Tab/]/[", "tab"),
                ("o/b/k/h", "jump tab"),
                ("↑↓", "nav list"),
            ];
            if app.project_focus == Some(ProjectTab::Knowledge) {
                h.push(("/", "filter"));
            }
            h.push(("Esc", "back"));
            h
        }
        Focus::Agent => {
            if app.playground_active {
                return draw_footer_playground(frame, area, app, activity_available, theme);
            }

            let is_pty = matches!(
                app.selected_agent(),
                Some(AgentEntry::Interactive(_))
                    | Some(AgentEntry::Terminal(_))
                    | Some(AgentEntry::Group(_))
            );
            let in_split = app.active_split_id.is_some();
            if is_pty {
                let mut h = vec![
                    ("F10", "preview"),
                    ("Esc", "home"),
                    ("Shift+↑↓", "agents/rag"),
                    ("Ctrl+T", "context"),
                ];
                if in_split {
                    h.push(("F4", "dissolve"));
                    h.push(("Shift+F4", "end"));
                    h.push(("Shift+←→", "split focus"));
                } else {
                    h.push(("F4", "end"));
                }
                if matches!(app.selected_agent(), Some(AgentEntry::Terminal(_))) {
                    h.push(("Tab", "catalog"));
                    h.push(("Ctrl+W", "wrap"));
                }
                if matches!(app.selected_agent(), Some(AgentEntry::Interactive(_))) {
                    h.push(("Ctrl+B", "prompt"));
                }
                if activity_available {
                    h.push(("F3", "activity"));
                }
                h.push(("Ctrl+N", "new"));
                h.push(("F1", "legend"));
                h
            } else {
                let mut h = vec![("F10", "preview"), ("Esc", "home")];
                if !app.agents_rag_focused {
                    h.push(("e", "edit"));
                }
                if activity_available {
                    h.push(("F3", "activity"));
                }
                h.push(("Ctrl+N", "new"));
                h.push(("F1", "legend"));
                h
            }
        }
        Focus::ContextTransfer => vec![
            ("↑↓", "select"),
            ("Tab/Enter", "next step"),
            ("Esc", "cancel"),
        ],
        Focus::RagTransfer => vec![("↑↓", "select"), ("Enter", "transfer"), ("Esc", "cancel")],
        Focus::PromptTemplateDialog => vec![
            ("↑↓", "fields"),
            ("⇧↑↓←→", "cursor"),
            ("Ctrl+S", "send"),
            ("Ctrl+A/X", "add/memory/remove"),
            ("Ctrl+L", "recall last"),
            ("Esc", "cancel"),
        ],
        Focus::LoopEditorDialog => vec![
            ("type", "edit"),
            ("←→", "cursor"),
            ("Enter", "newline"),
            ("Ctrl+S", "save"),
            ("Esc", "cancel"),
        ],
        Focus::LoopFormDialog => vec![
            ("Tab/↑↓", "field"),
            ("←→", "trigger"),
            ("Enter", "save"),
            ("Esc", "cancel"),
        ],
        Focus::ProjectRelationDialog => vec![
            ("↑↓", "select"),
            ("←→", "relation"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ],
        Focus::KnowledgeDialog => vec![
            ("Tab", "field"),
            ("Space", "toggle kind"),
            ("Enter", "next/save"),
            ("Esc", "cancel"),
        ],
    };

    let mut spans = Vec::new();
    spans.push(Span::raw("  "));
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(theme.dim_text)));
    }

    // Show split session names when in split view
    let split_label = if let Some(ref split_id) = app.active_split_id {
        app.split_groups
            .iter()
            .find(|g| g.id == *split_id)
            .map(|g| {
                let left_marker = if app.split_right_focused { " " } else { "●" };
                let right_marker = if app.split_right_focused { "●" } else { " " };
                format!(
                    " {left_marker} {} │ {} {right_marker} ",
                    g.session_a, g.session_b
                )
            })
    } else {
        None
    };

    let version = if app.daemon_version.is_empty() {
        String::new()
    } else {
        format!(" v{} ", app.daemon_version)
    };

    let hints_line = Line::from(spans);
    let hints_p = Paragraph::new(hints_line);
    frame.render_widget(hints_p, area);

    // Render split label + version on the right side
    let right_text = match (&split_label, version.is_empty()) {
        (Some(sl), false) => format!("{sl}{version}"),
        (Some(sl), true) => sl.clone(),
        (None, false) => version.clone(),
        (None, true) => String::new(),
    };
    let right_w = right_text.len() as u16;

    if right_w > 0 && area.width > right_w {
        let right_area = Rect::new(area.x + area.width - right_w, area.y, right_w, 1);

        let mut right_spans = Vec::new();
        if let Some(ref sl) = split_label {
            right_spans.push(Span::styled(
                sl.as_str(),
                Style::default()
                    .fg(theme.header_color)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if !version.is_empty() {
            right_spans.push(Span::styled(
                &version,
                Style::default()
                    .fg(theme.dim_text)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let right_p = Paragraph::new(Line::from(right_spans));
        frame.render_widget(right_p, right_area);
    }
}

fn draw_footer_playground(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    activity_available: bool,
    theme: &Theme,
) {
    let mut hints = vec![
        ("type", "search"),
        ("↑↓", "results"),
        ("Enter", "search/open"),
        ("Shift+↑↓", "agents"),
        ("Ctrl+T", "transfer"),
        ("F10", "preview"),
        ("Esc", "close"),
    ];
    if activity_available {
        hints.push(("F3", "activity"));
    }

    let mut spans = Vec::new();
    spans.push(Span::raw("  "));
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(theme.dim_text)));
    }

    let version = if app.daemon_version.is_empty() {
        String::new()
    } else {
        format!(" v{} ", app.daemon_version)
    };

    let hints_line = Line::from(spans);
    frame.render_widget(Paragraph::new(hints_line), area);

    if !version.is_empty() && area.width > version.len() as u16 {
        let right_w = version.len() as u16;
        let right_area = Rect::new(area.x + area.width - right_w, area.y, right_w, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                version,
                Style::default()
                    .fg(theme.dim_text)
                    .add_modifier(Modifier::BOLD),
            ))),
            right_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::types::{App, Focus, SidebarLayer};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    fn make_app() -> App {
        use crate::db::Database;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).unwrap());
        let data_dir = tempfile::tempdir().unwrap();
        App::new(db, data_dir.path()).unwrap()
    }

    fn render_footer_to_text(width: u16, height: u16, draw: impl FnOnce(&mut ratatui::Frame, Rect)) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| {
            let area = frame.area();
            draw(frame, area);
        }).unwrap();
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
    fn footer_renders_in_home_focus() {
        let mut app = make_app();
        app.focus = Focus::Home;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("select"), "Home footer should show 'select': {text}");
        assert!(text.contains("new"), "Home footer should show 'new': {text}");
    }

    #[test]
    fn footer_renders_in_preview_focus() {
        let mut app = make_app();
        app.focus = Focus::Preview;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("nav"), "Preview footer should show 'nav': {text}");
    }

    #[test]
    fn footer_renders_in_new_agent_dialog() {
        let mut app = make_app();
        app.focus = Focus::NewAgentDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("fields"), "NewAgentDialog footer should show 'fields': {text}");
        assert!(text.contains("cancel"), "NewAgentDialog footer should show 'cancel': {text}");
    }

    #[test]
    fn footer_renders_in_launchpad_dialog() {
        let mut app = make_app();
        app.focus = Focus::LaunchpadDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("mission"), "LaunchpadDialog footer should show 'mission': {text}");
    }

    #[test]
    fn footer_renders_in_context_transfer() {
        let mut app = make_app();
        app.focus = Focus::ContextTransfer;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("select"), "ContextTransfer footer should show 'select': {text}");
    }

    #[test]
    fn footer_renders_in_rag_transfer() {
        let mut app = make_app();
        app.focus = Focus::RagTransfer;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("transfer"), "RagTransfer footer should show 'transfer': {text}");
    }

    #[test]
    fn footer_renders_in_prompt_template_dialog() {
        let mut app = make_app();
        app.focus = Focus::PromptTemplateDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("send"), "PromptTemplateDialog footer should show 'send': {text}");
    }

    #[test]
    fn footer_renders_in_loop_editor_dialog() {
        let mut app = make_app();
        app.focus = Focus::LoopEditorDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("save"), "LoopEditorDialog footer should show 'save': {text}");
    }

    #[test]
    fn footer_renders_in_loop_form_dialog() {
        let mut app = make_app();
        app.focus = Focus::LoopFormDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("field"), "LoopFormDialog footer should show 'field': {text}");
    }

    #[test]
    fn footer_renders_in_project_relation_dialog() {
        let mut app = make_app();
        app.focus = Focus::ProjectRelationDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("relation"), "ProjectRelationDialog footer should show 'relation': {text}");
    }

    #[test]
    fn footer_renders_in_knowledge_dialog() {
        let mut app = make_app();
        app.focus = Focus::KnowledgeDialog;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("toggle"), "KnowledgeDialog footer should show 'toggle': {text}");
    }

    #[test]
    fn footer_renders_in_agent_focus_with_no_pty() {
        let mut app = make_app();
        app.focus = Focus::Agent;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("preview"), "Agent footer should show 'preview': {text}");
    }

    #[test]
    fn footer_renders_in_agent_focus_with_knowledge_layer() {
        let mut app = make_app();
        app.focus = Focus::Agent;
        app.sidebar_layer = SidebarLayer::Knowledge;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("tab"), "Knowledge Agent footer should show 'tab': {text}");
        assert!(text.contains("back"), "Knowledge Agent footer should show 'back': {text}");
    }

    #[test]
    fn footer_renders_with_version() {
        let mut app = make_app();
        app.focus = Focus::Home;
        app.daemon_version = "1.0.0".to_string();
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("v1.0.0"), "Footer should show version: {text}");
    }

    #[test]
    fn footer_renders_without_version() {
        let mut app = make_app();
        app.focus = Focus::Home;
        app.daemon_version = String::new();
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        // Should still render hints even without version
        assert!(text.contains("select"), "Footer should still show hints: {text}");
    }

    #[test]
    fn footer_renders_on_narrow_width() {
        let mut app = make_app();
        app.focus = Focus::Home;
        let theme = Theme::classic();
        // Very narrow width should not panic
        let text = render_footer_to_text(20, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(!text.is_empty());
    }

    #[test]
    fn footer_renders_with_split_view() {
        let mut app = make_app();
        app.focus = Focus::Agent;
        app.active_split_id = Some("test-split".to_string());
        app.split_groups.push(crate::domain::models::SplitGroup {
            id: "test-split".to_string(),
            session_a: "session-a".to_string(),
            session_b: "session-b".to_string(),
            orientation: crate::domain::models::SplitOrientation::Horizontal,
            created_at: chrono::Utc::now(),
        });
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("session-a"), "Split footer should show session-a: {text}");
        assert!(text.contains("session-b"), "Split footer should show session-b: {text}");
    }

    #[test]
    fn footer_renders_in_preview_with_playground_active() {
        let mut app = make_app();
        app.focus = Focus::Preview;
        app.playground_active = true;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("search"), "Playground footer should show 'search': {text}");
    }

    #[test]
    fn footer_renders_in_preview_with_knowledge_layer() {
        let mut app = make_app();
        app.focus = Focus::Preview;
        app.sidebar_layer = SidebarLayer::Knowledge;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("highlight"), "Knowledge preview should show 'highlight': {text}");
    }

    #[test]
    fn footer_renders_in_agent_focus_playground_active() {
        let mut app = make_app();
        app.focus = Focus::Agent;
        app.playground_active = true;
        let theme = Theme::classic();
        let text = render_footer_to_text(80, 1, |frame, area| {
            draw_footer(frame, area, &app, &theme);
        });
        assert!(text.contains("search"), "Agent playground footer should show 'search': {text}");
    }
}
