use super::*;

use chrono::Duration;

fn sample_agent(id: &str, trigger: Option<Trigger>) -> Agent {
    Agent {
        id: id.to_string(),
        prompt: "Run tests".to_string(),
        trigger,
        cli: Cli::new("opencode"),
        model: None,
        effort: None,
        working_dir: Some("/tmp/project".to_string()),
        enabled: true,
        enable_at: None,
        created_at: Utc::now(),
        log_path: "/tmp/test.log".to_string(),
        timeout_minutes: 15,
        expires_at: None,
        last_run_at: None,
        last_run_ok: None,
        last_triggered_at: None,
        trigger_count: 0,
    }
}

#[test]
fn test_agent_not_expired_no_expiry() {
    let agent = sample_agent("t1", None);
    assert!(!agent.is_expired());
}

#[test]
fn test_agent_not_expired_future() {
    let mut agent = sample_agent("t2", None);
    agent.expires_at = Some(Utc::now() + Duration::hours(1));
    assert!(!agent.is_expired());
}

#[test]
fn test_agent_expired_past() {
    let mut agent = sample_agent("t3", None);
    agent.created_at = Utc::now() - Duration::hours(2);
    agent.expires_at = Some(Utc::now() - Duration::hours(1));
    assert!(agent.is_expired());
}

#[test]
fn test_agent_trigger_type_labels() {
    let cron_agent = sample_agent(
        "c1",
        Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }),
    );
    assert_eq!(cron_agent.trigger_type_label(), "cron");
    assert!(cron_agent.is_cron());
    assert!(!cron_agent.is_watch());

    let watch_agent = sample_agent(
        "w1",
        Some(Trigger::Watch {
            path: "/tmp".to_string(),
            events: vec![WatchEvent::Create],
            debounce_seconds: 2,
            recursive: false,
        }),
    );
    assert_eq!(watch_agent.trigger_type_label(), "watch");
    assert!(!watch_agent.is_cron());
    assert!(watch_agent.is_watch());

    let manual_agent = sample_agent("m1", None);
    assert_eq!(manual_agent.trigger_type_label(), "manual");
    assert!(!manual_agent.is_cron());
    assert!(!manual_agent.is_watch());
}

#[test]
fn test_agent_accessors() {
    let cron_agent = sample_agent(
        "c1",
        Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }),
    );
    assert_eq!(cron_agent.schedule_expr(), Some("0 9 * * *"));
    assert!(cron_agent.watch_path().is_none());

    let watch_agent = sample_agent(
        "w1",
        Some(Trigger::Watch {
            path: "/tmp/watched".to_string(),
            events: vec![WatchEvent::Create, WatchEvent::Modify],
            debounce_seconds: 5,
            recursive: true,
        }),
    );
    assert_eq!(watch_agent.watch_path(), Some("/tmp/watched"));
    assert!(watch_agent.schedule_expr().is_none());
    let events = watch_agent.watch_events().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.contains(&WatchEvent::Create));
    assert!(events.contains(&WatchEvent::Modify));
}

#[test]
fn test_trigger_type_str() {
    let cron_trigger = Trigger::Cron {
        schedule_expr: "0 9 * * *".to_string(),
    };
    assert_eq!(cron_trigger.type_str(), "cron");

    let watch_trigger = Trigger::Watch {
        path: "/tmp".to_string(),
        events: vec![WatchEvent::Create],
        debounce_seconds: 2,
        recursive: false,
    };
    assert_eq!(watch_trigger.type_str(), "watch");
}

#[test]
fn test_watch_event_from_str() {
    assert_eq!(WatchEvent::from_str("create"), Some(WatchEvent::Create));
    assert_eq!(WatchEvent::from_str("modify"), Some(WatchEvent::Modify));
    assert_eq!(WatchEvent::from_str("delete"), Some(WatchEvent::Delete));
    assert_eq!(WatchEvent::from_str("move"), Some(WatchEvent::Move));
    assert_eq!(WatchEvent::from_str("invalid"), None);
    assert_eq!(WatchEvent::from_str(""), None);
}

#[test]
fn test_watch_event_display() {
    assert_eq!(WatchEvent::Create.to_string(), "create");
    assert_eq!(WatchEvent::Modify.to_string(), "modify");
    assert_eq!(WatchEvent::Delete.to_string(), "delete");
    assert_eq!(WatchEvent::Move.to_string(), "move");
}

#[test]
fn test_cli_from_str() {
    assert_eq!(Cli::from_str("opencode").as_str(), "opencode");
    assert_eq!(Cli::from_str("kiro").as_str(), "kiro");
    assert_eq!(Cli::from_str("gemini").as_str(), "gemini");
    assert_eq!(Cli::from_str("unknown").as_str(), "unknown");
    assert_eq!(Cli::from_str("").as_str(), "opencode");
}

#[test]
fn test_cli_as_str() {
    assert_eq!(Cli::new("opencode").as_str(), "opencode");
    assert_eq!(Cli::new("kiro").as_str(), "kiro");
    assert_eq!(Cli::new("gemini").as_str(), "gemini");
}

#[test]
fn test_cli_display() {
    assert_eq!(format!("{}", Cli::new("opencode")), "opencode");
    assert_eq!(format!("{}", Cli::new("kiro")), "kiro");
    assert_eq!(format!("{}", Cli::new("gemini")), "gemini");
}

#[test]
fn test_cli_resolve_explicit_opencode() {
    assert_eq!(Cli::resolve(Some("opencode")).unwrap().as_str(), "opencode");
}

#[test]
fn test_cli_resolve_explicit_kiro() {
    assert_eq!(Cli::resolve(Some("kiro")).unwrap().as_str(), "kiro");
}

#[test]
fn test_cli_resolve_explicit_gemini() {
    assert_eq!(Cli::resolve(Some("gemini")).unwrap().as_str(), "gemini");
}

#[test]
fn test_cli_resolve_unknown_returns_ok() {
    assert_eq!(Cli::resolve(Some("vim")).unwrap().as_str(), "vim");
}

#[test]
fn test_parse_list_valid_events() {
    let input = vec!["create".to_string(), "modify".to_string()];
    let events = WatchEvent::parse_list(&input).unwrap();
    assert_eq!(events, vec![WatchEvent::Create, WatchEvent::Modify]);
}

#[test]
fn test_parse_list_all_events() {
    let input = vec![
        "create".to_string(),
        "modify".to_string(),
        "delete".to_string(),
        "move".to_string(),
    ];
    let events = WatchEvent::parse_list(&input).unwrap();
    assert_eq!(events.len(), 4);
}

#[test]
fn test_parse_list_invalid_event_returns_error() {
    let input = vec!["create".to_string(), "bogus".to_string()];
    let err = WatchEvent::parse_list(&input).unwrap_err();
    assert!(err.contains("Invalid event type 'bogus'"));
}

#[test]
fn test_parse_list_empty_returns_error() {
    let input: Vec<String> = vec![];
    let err = WatchEvent::parse_list(&input).unwrap_err();
    assert!(err.contains("At least one event type must be specified"));
}

#[test]
fn test_trigger_type_from_str() {
    assert!(matches!(
        TriggerType::from_str("scheduled"),
        TriggerType::Scheduled
    ));
    assert!(matches!(
        TriggerType::from_str("manual"),
        TriggerType::Manual
    ));
    assert!(matches!(TriggerType::from_str("watch"), TriggerType::Watch));
    assert!(matches!(
        TriggerType::from_str("unknown"),
        TriggerType::Scheduled
    ));
}

#[test]
fn test_trigger_type_roundtrip() {
    for tt in [
        TriggerType::Scheduled,
        TriggerType::Manual,
        TriggerType::Watch,
    ] {
        assert!(
            matches!(TriggerType::from_str(tt.as_str()), t if std::mem::discriminant(&t) == std::mem::discriminant(&tt))
        );
    }
}

#[test]
fn test_run_status_from_str() {
    assert!(matches!(RunStatus::from_str("pending"), RunStatus::Pending));
    assert!(matches!(
        RunStatus::from_str("in_progress"),
        RunStatus::InProgress
    ));
    assert!(matches!(RunStatus::from_str("success"), RunStatus::Success));
    assert!(matches!(RunStatus::from_str("error"), RunStatus::Error));
    assert!(matches!(RunStatus::from_str("timeout"), RunStatus::Timeout));
    assert!(matches!(RunStatus::from_str("missed"), RunStatus::Missed));
    assert!(matches!(RunStatus::from_str("unknown"), RunStatus::Pending));
}

#[test]
fn test_run_status_as_str() {
    assert_eq!(RunStatus::Pending.as_str(), "pending");
    assert_eq!(RunStatus::InProgress.as_str(), "in_progress");
    assert_eq!(RunStatus::Success.as_str(), "success");
    assert_eq!(RunStatus::Error.as_str(), "error");
    assert_eq!(RunStatus::Timeout.as_str(), "timeout");
    assert_eq!(RunStatus::Missed.as_str(), "missed");
}

#[test]
fn test_run_status_is_active() {
    assert!(RunStatus::Pending.is_active());
    assert!(RunStatus::InProgress.is_active());
    assert!(!RunStatus::Success.is_active());
    assert!(!RunStatus::Error.is_active());
    assert!(!RunStatus::Timeout.is_active());
    assert!(!RunStatus::Missed.is_active());
}

#[test]
fn test_run_status_display() {
    assert_eq!(format!("{}", RunStatus::Pending), "pending");
    assert_eq!(format!("{}", RunStatus::Success), "success");
    assert_eq!(format!("{}", RunStatus::Error), "error");
}

#[test]
fn test_watcher_trigger_accessors() {
    let agent = sample_agent(
        "w1",
        Some(Trigger::Watch {
            path: "/tmp".to_string(),
            events: vec![WatchEvent::Create],
            debounce_seconds: 5,
            recursive: false,
        }),
    );
    assert_eq!(agent.trigger_count, 0);
    assert!(agent.last_triggered_at.is_none());
}

#[test]
fn test_corrupt_agent_struct() {
    let corrupt = CorruptAgent {
        id: "test-id".to_string(),
        enabled: true,
        error: "parse error".to_string(),
    };
    assert_eq!(corrupt.id, "test-id");
    assert!(corrupt.enabled);
    assert_eq!(corrupt.error, "parse error");
}

#[test]
fn test_agent_is_cron_and_is_watch() {
    let cron_agent = sample_agent(
        "c1",
        Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }),
    );
    assert!(cron_agent.is_cron());
    assert!(!cron_agent.is_watch());

    let watch_agent = sample_agent(
        "w1",
        Some(Trigger::Watch {
            path: "/tmp".to_string(),
            events: vec![WatchEvent::Create],
            debounce_seconds: 2,
            recursive: false,
        }),
    );
    assert!(!watch_agent.is_cron());
    assert!(watch_agent.is_watch());

    let manual_agent = sample_agent("m1", None);
    assert!(!manual_agent.is_cron());
    assert!(!manual_agent.is_watch());
}

#[test]
fn test_agent_watch_events() {
    let watch_agent = sample_agent(
        "w1",
        Some(Trigger::Watch {
            path: "/tmp".to_string(),
            events: vec![WatchEvent::Create, WatchEvent::Modify],
            debounce_seconds: 2,
            recursive: false,
        }),
    );
    let events = watch_agent.watch_events().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.contains(&WatchEvent::Create));
    assert!(events.contains(&WatchEvent::Modify));

    let cron_agent = sample_agent(
        "c1",
        Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }),
    );
    assert!(cron_agent.watch_events().is_none());
}

#[test]
fn test_split_orientation_as_str() {
    assert_eq!(SplitOrientation::Horizontal.as_str(), "horizontal");
    assert_eq!(SplitOrientation::Vertical.as_str(), "vertical");
}

#[test]
fn test_split_orientation_from_str() {
    assert!(matches!(
        SplitOrientation::from_str("vertical"),
        SplitOrientation::Vertical
    ));
    assert!(matches!(
        SplitOrientation::from_str("horizontal"),
        SplitOrientation::Horizontal
    ));
    assert!(matches!(
        SplitOrientation::from_str("anything"),
        SplitOrientation::Horizontal
    ));
    assert!(matches!(
        SplitOrientation::from_str(""),
        SplitOrientation::Horizontal
    ));
}

#[test]
fn test_split_group_creation() {
    let group = SplitGroup {
        id: "sg1".to_string(),
        orientation: SplitOrientation::Horizontal,
        session_a: "sess_a".to_string(),
        session_b: "sess_b".to_string(),
        created_at: Utc::now(),
    };
    assert_eq!(group.id, "sg1");
    assert!(matches!(group.orientation, SplitOrientation::Horizontal));
    assert_eq!(group.session_a, "sess_a");
    assert_eq!(group.session_b, "sess_b");
}

#[test]
fn test_trigger_type_display() {
    assert_eq!(format!("{}", TriggerType::Scheduled), "scheduled");
    assert_eq!(format!("{}", TriggerType::Manual), "manual");
    assert_eq!(format!("{}", TriggerType::Watch), "watch");
}

#[test]
fn test_run_status_all_variants_display() {
    assert_eq!(format!("{}", RunStatus::Pending), "pending");
    assert_eq!(format!("{}", RunStatus::InProgress), "in_progress");
    assert_eq!(format!("{}", RunStatus::Success), "success");
    assert_eq!(format!("{}", RunStatus::Error), "error");
    assert_eq!(format!("{}", RunStatus::Timeout), "timeout");
    assert_eq!(format!("{}", RunStatus::Missed), "missed");
}

#[test]
fn test_cli_resolve_empty_string_returns_err() {
    let result = Cli::resolve(Some(""));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("must not be empty"));
}

#[test]
fn test_default_debounce_returns_2() {
    assert_eq!(default_debounce(), 2);
}

#[test]
fn test_start_run_already_active() {
    let run_log = RunLog {
        id: "run1".to_string(),
        background_agent_id: "agent1".to_string(),
        status: RunStatus::InProgress,
        trigger_type: TriggerType::Scheduled,
        summary: None,
        started_at: Utc::now(),
        finished_at: None,
        exit_code: None,
        timeout_at: None,
    };
    let outcome = StartRunOutcome::AlreadyActive(run_log);
    assert!(matches!(outcome, StartRunOutcome::AlreadyActive(_)));
}

#[test]
fn test_start_run_started() {
    let outcome = StartRunOutcome::Started;
    assert!(matches!(outcome, StartRunOutcome::Started));
}

#[test]
fn test_run_log_creation() {
    let log = RunLog {
        id: "run-1".to_string(),
        background_agent_id: "agent-1".to_string(),
        status: RunStatus::Success,
        trigger_type: TriggerType::Manual,
        summary: Some("done".to_string()),
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        exit_code: Some(0),
        timeout_at: None,
    };
    assert_eq!(log.id, "run-1");
    assert_eq!(log.background_agent_id, "agent-1");
    assert!(matches!(log.status, RunStatus::Success));
    assert!(matches!(log.trigger_type, TriggerType::Manual));
    assert_eq!(log.summary.as_deref(), Some("done"));
    assert_eq!(log.exit_code, Some(0));
}

#[test]
fn test_trigger_watch_all_fields() {
    let trigger = Trigger::Watch {
        path: "/src".to_string(),
        events: vec![WatchEvent::Modify, WatchEvent::Delete],
        debounce_seconds: 10,
        recursive: true,
    };
    assert_eq!(trigger.type_str(), "watch");
    if let Trigger::Watch {
        path,
        events,
        debounce_seconds,
        recursive,
    } = trigger
    {
        assert_eq!(path, "/src");
        assert_eq!(events.len(), 2);
        assert_eq!(debounce_seconds, 10);
        assert!(recursive);
    } else {
        panic!("expected Watch trigger");
    }
}
