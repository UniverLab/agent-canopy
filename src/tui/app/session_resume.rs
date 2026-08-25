pub(crate) fn args_contain_flag(args: &str, flag: &str) -> bool {
    args.split_whitespace().any(|arg| arg == flag)
}

pub(crate) fn append_flag_if_missing(
    base_args: Option<&str>,
    yolo_flag: Option<&str>,
    should_include_yolo: bool,
) -> Option<String> {
    let base = base_args.map(str::trim).filter(|args| !args.is_empty());

    match (base, yolo_flag, should_include_yolo) {
        (Some(args), Some(flag), true) if !args_contain_flag(args, flag) => {
            Some(format!("{args} {flag}"))
        }
        (Some(args), _, _) => Some(args.to_string()),
        (None, Some(flag), true) => Some(flag.to_string()),
        (None, _, _) => None,
    }
}

fn join_args(base_args: Option<&str>, extra_args: Option<&str>) -> Option<String> {
    match (
        base_args.map(str::trim).filter(|args| !args.is_empty()),
        extra_args.map(str::trim).filter(|args| !args.is_empty()),
    ) {
        (Some(base), Some(extra)) => Some(format!("{base} {extra}")),
        (Some(base), None) => Some(base.to_string()),
        (None, Some(extra)) => Some(extra.to_string()),
        (None, None) => None,
    }
}

fn args_contain_sequence(args: &str, sequence: &str) -> bool {
    let args_tokens: Vec<_> = args.split_whitespace().collect();
    let sequence_tokens: Vec<_> = sequence.split_whitespace().collect();
    !sequence_tokens.is_empty()
        && args_tokens
            .windows(sequence_tokens.len())
            .any(|window| window == sequence_tokens.as_slice())
}

use std::collections::HashSet;

use crate::db::session::InteractiveSession;

/// Cap on the resume picker's candidate list, applied after deduplication
/// (decision 6): the table holds well over a thousand rows and the picker is
/// a recency tool, not an archive browser.
pub(crate) const RESUME_CANDIDATE_CAP: usize = 20;

/// Metadata-only view of a resumable session for the picker: enough to tell
/// rows apart (name, harness, recency) and enough to actually relaunch it
/// (working_dir, original args) without reading a session's full history.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResumableSession {
    pub id: String,
    pub name: String,
    pub cli: String,
    pub last_active: String,
    pub working_dir: String,
    pub args: Option<String>,
}

impl From<&InteractiveSession> for ResumableSession {
    fn from(session: &InteractiveSession) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
            cli: session.cli.clone(),
            last_active: session.started_at.clone(),
            working_dir: session.working_dir.clone(),
            args: session.args.clone(),
        }
    }
}

/// Collapse resumable-session candidates to at most one per (cli,
/// working_dir) — the most recent — then cap the result (decisions 5, 6).
/// `sessions` must already be ordered most-recent-first (as returned by
/// `Database::get_resumable_sessions`), since the first occurrence of each
/// key is the one kept.
pub(crate) fn dedupe_resumable_sessions(
    sessions: Vec<InteractiveSession>,
    cap: usize,
) -> Vec<InteractiveSession> {
    let mut seen = HashSet::new();
    sessions
        .into_iter()
        .filter(|session| seen.insert((session.cli.clone(), session.working_dir.clone())))
        .take(cap)
        .collect()
}

/// What choosing "Resume" should do for the current set of resumable
/// sessions (decisions 3 and 4: a lone candidate resumes directly with no
/// prompt, and zero candidates is reported rather than falling through to
/// starting something new).
pub(crate) enum ResumeChoice {
    /// No resumable sessions.
    None,
    /// Exactly one candidate: resume it directly, no picker.
    Direct(ResumableSession),
    /// More than one candidate, most-recent-first.
    Picker(Vec<ResumableSession>),
}

/// Decide what choosing "Resume" should do, given the sessions eligible for
/// resume. Sorts most-recent-first before deciding, so both the `Direct`
/// candidate and the `Picker` list reflect that order.
pub(crate) fn plan_resume(sessions: &[InteractiveSession]) -> ResumeChoice {
    let mut sorted: Vec<ResumableSession> = sessions.iter().map(ResumableSession::from).collect();
    sorted.sort_by(|a, b| b.last_active.cmp(&a.last_active));

    match sorted.len() {
        0 => ResumeChoice::None,
        1 => ResumeChoice::Direct(sorted.into_iter().next().expect("len == 1 checked above")),
        _ => ResumeChoice::Picker(sorted),
    }
}

/// Navigation state for the session-resume picker. Index/scroll math is
/// delegated to the shared selection model (`tui::selection`) rather than
/// reimplemented here.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionResumePicker {
    pub sessions: Vec<ResumableSession>,
    pub index: usize,
    pub scroll: usize,
}

impl SessionResumePicker {
    pub fn new(sessions: Vec<ResumableSession>) -> Self {
        Self {
            sessions,
            index: 0,
            scroll: 0,
        }
    }

    pub fn move_selection(&mut self, forward: bool, visible_rows: usize) {
        let (index, scroll) = crate::tui::selection::move_selection(
            self.index,
            self.scroll,
            self.sessions.len(),
            visible_rows,
            forward,
        );
        self.index = index;
        self.scroll = scroll;
    }

    pub fn selected(&self) -> Option<&ResumableSession> {
        self.sessions.get(self.index)
    }
}

pub(crate) fn build_resumed_session_args(
    original_args: Option<&str>,
    interactive_args: Option<&str>,
    resume_args: Option<&str>,
    session_resume_cmd: Option<&str>,
    yolo_flag: Option<&str>,
) -> Option<String> {
    let original_args = original_args.map(str::trim).filter(|args| !args.is_empty());
    let inter_args = interactive_args
        .map(str::trim)
        .filter(|args| !args.is_empty());
    let resume_args = resume_args.map(str::trim).filter(|args| !args.is_empty());
    let session_resume_cmd = session_resume_cmd
        .map(str::trim)
        .filter(|args| !args.is_empty());
    let had_yolo = yolo_flag
        .is_some_and(|flag| original_args.is_some_and(|args| args_contain_flag(args, flag)));

    let already_resume_args = original_args.is_some_and(|args| {
        resume_args.is_some_and(|resume| args_contain_sequence(args, resume))
            || session_resume_cmd.is_some_and(|cmd| args_contain_sequence(args, cmd))
    });

    let base_args = if already_resume_args {
        original_args.map(str::to_string)
    } else if resume_args.is_some() {
        join_args(original_args.or(inter_args), resume_args)
    } else {
        original_args.or(inter_args).map(str::to_string)
    };

    append_flag_if_missing(base_args.as_deref(), yolo_flag, had_yolo)
}

#[cfg(test)]
mod resume_picker_tests {
    use super::*;

    fn session(id: &str, name: &str, cli: &str, started_at: &str) -> InteractiveSession {
        InteractiveSession {
            id: id.to_string(),
            name: name.to_string(),
            cli: cli.to_string(),
            working_dir: "/tmp".to_string(),
            args: None,
            started_at: started_at.to_string(),
            status: "orphaned".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        }
    }

    // ── plan_resume ─────────────────────────────────────────────

    #[test]
    fn plan_resume_zero_sessions_reports_none() {
        assert!(matches!(plan_resume(&[]), ResumeChoice::None));
    }

    #[test]
    fn plan_resume_one_session_resumes_directly() {
        let sessions = vec![session("s1", "alpha", "claude", "2026-08-19T10:00:00Z")];
        match plan_resume(&sessions) {
            ResumeChoice::Direct(session) => assert_eq!(session.id, "s1"),
            _ => panic!("expected Direct"),
        }
    }

    #[test]
    fn plan_resume_multiple_sessions_opens_picker_most_recent_first() {
        let sessions = vec![
            session("s1", "alpha", "claude", "2026-08-19T10:00:00Z"),
            session("s2", "beta", "codex", "2026-08-19T12:00:00Z"),
            session("s3", "gamma", "gemini", "2026-08-19T11:00:00Z"),
        ];
        match plan_resume(&sessions) {
            ResumeChoice::Picker(rows) => {
                let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                assert_eq!(ids, vec!["s2", "s3", "s1"], "must be most-recent-first");
                assert_eq!(rows[0].cli, "codex");
            }
            _ => panic!("expected Picker"),
        }
    }

    // ── SessionResumePicker ─────────────────────────────────────

    #[test]
    fn picker_selecting_second_row_yields_its_session_id() {
        let sessions = vec![
            session("s1", "alpha", "claude", "2026-08-19T12:00:00Z"),
            session("s2", "beta", "codex", "2026-08-19T11:00:00Z"),
            session("s3", "gamma", "gemini", "2026-08-19T10:00:00Z"),
        ];
        let ResumeChoice::Picker(rows) = plan_resume(&sessions) else {
            panic!("expected Picker");
        };
        let mut picker = SessionResumePicker::new(rows);

        picker.move_selection(true, 6);

        assert_eq!(picker.selected().map(|s| s.id.as_str()), Some("s2"));
    }

    #[test]
    fn picker_navigation_wraps_and_never_panics() {
        let rows = vec![
            ResumableSession {
                id: "s1".into(),
                name: "alpha".into(),
                cli: "claude".into(),
                last_active: "2026-08-19T12:00:00Z".into(),
                working_dir: "/tmp".into(),
                args: None,
            },
            ResumableSession {
                id: "s2".into(),
                name: "beta".into(),
                cli: "codex".into(),
                last_active: "2026-08-19T11:00:00Z".into(),
                working_dir: "/tmp".into(),
                args: None,
            },
        ];
        let mut picker = SessionResumePicker::new(rows);

        picker.move_selection(false, 6);
        assert_eq!(picker.selected().map(|s| s.id.as_str()), Some("s2"));

        picker.move_selection(true, 6);
        assert_eq!(picker.selected().map(|s| s.id.as_str()), Some("s1"));
    }

    // ── dedupe_resumable_sessions ───────────────────────────────

    fn session_in(id: &str, cli: &str, started_at: &str, working_dir: &str) -> InteractiveSession {
        InteractiveSession {
            id: id.to_string(),
            name: id.to_string(),
            cli: cli.to_string(),
            working_dir: working_dir.to_string(),
            args: None,
            started_at: started_at.to_string(),
            status: "completed".to_string(),
            session_type: "interactive".to_string(),
            pid: None,
            boot_id: None,
        }
    }

    #[test]
    fn dedupe_collapses_same_cli_and_dir_to_most_recent() {
        // Already most-recent-first, as the DB query returns it.
        let sessions = vec![
            session_in("s3", "claude", "2026-08-19T12:00:00Z", "/proj"),
            session_in("s2", "claude", "2026-08-19T11:00:00Z", "/proj"),
            session_in("s1", "claude", "2026-08-19T10:00:00Z", "/proj"),
        ];

        let deduped = dedupe_resumable_sessions(sessions, 20);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].id, "s3", "must keep the most recent row");
    }

    #[test]
    fn dedupe_keeps_same_cli_different_dirs() {
        let sessions = vec![
            session_in("s1", "claude", "2026-08-19T10:00:00Z", "/proj-a"),
            session_in("s2", "claude", "2026-08-19T09:00:00Z", "/proj-b"),
        ];

        let deduped = dedupe_resumable_sessions(sessions, 20);

        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedupe_keeps_different_clis_same_dir() {
        let sessions = vec![
            session_in("s1", "claude", "2026-08-19T10:00:00Z", "/proj"),
            session_in("s2", "codex", "2026-08-19T09:00:00Z", "/proj"),
        ];

        let deduped = dedupe_resumable_sessions(sessions, 20);

        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedupe_truncates_to_cap_keeping_most_recent() {
        let sessions: Vec<InteractiveSession> = (0..25)
            .map(|i| {
                session_in(
                    &format!("s{i}"),
                    "claude",
                    &format!("2026-08-19T10:{:02}:00Z", 59 - i),
                    &format!("/proj-{i}"),
                )
            })
            .collect();

        let deduped = dedupe_resumable_sessions(sessions, RESUME_CANDIDATE_CAP);

        assert_eq!(deduped.len(), RESUME_CANDIDATE_CAP);
        assert_eq!(
            deduped[0].id, "s0",
            "most recent row must survive truncation"
        );
        assert_eq!(deduped[19].id, "s19");
    }
}
