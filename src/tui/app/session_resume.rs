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

pub(crate) fn build_resumed_session_args(
    session: &crate::db::session::InteractiveSession,
    interactive_args: Option<&str>,
    resume_args: Option<&str>,
    session_resume_cmd: Option<&str>,
    yolo_flag: Option<&str>,
) -> Option<String> {
    let original_args = session
        .args
        .as_deref()
        .map(str::trim)
        .filter(|args| !args.is_empty());
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
