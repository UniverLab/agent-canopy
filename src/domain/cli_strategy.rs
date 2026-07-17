//! Dynamic CLI execution strategy.
//!
//! All CLI definitions come from the registry (platforms.json).
//! Commands are built dynamically based on the saved configuration.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use anyhow::{Context, Result};

/// Strategy for building CLI commands from registry config.
#[derive(Clone)]
pub struct CliStrategy {
    pub binary: String,
    pub headless_mode: String,
    pub model_flag: Option<String>,
    pub supports_working_dir: bool,
    pub working_dir_flag: Option<String>,
    pub env_vars: HashMap<String, String>,
    /// When true, the prompt is delivered via stdin (backed by an anonymous
    /// temp file) instead of argv. See [`CliConfig::prompt_via_stdin`] for
    /// why this must stay opt-in per CLI.
    ///
    /// [`CliConfig::prompt_via_stdin`]: super::cli_config::CliConfig::prompt_via_stdin
    pub prompt_via_stdin: bool,
    /// Flag that sets the session id when spawning a new headless session
    /// (RS1). See [`CliConfig::session_id_set_flag`].
    ///
    /// [`CliConfig::session_id_set_flag`]: super::cli_config::CliConfig::session_id_set_flag
    pub session_id_set_flag: Option<String>,
    /// Subcommand/args to list this platform's sessions, e.g. `"session
    /// list"` or `"ls"`. Drives list-after-run session id capture (RS1
    /// phase 2). See [`CliConfig::session_list_cmd`].
    ///
    /// [`CliConfig::session_list_cmd`]: super::cli_config::CliConfig::session_list_cmd
    pub session_list_cmd: Option<String>,
    /// Extra args that make the session list machine-readable (e.g.
    /// `"--format json"`). See [`CliConfig::session_list_format_args`].
    ///
    /// [`CliConfig::session_list_format_args`]: super::cli_config::CliConfig::session_list_format_args
    pub session_list_format_args: Option<String>,
    /// Regex extracting session ids from the list output. See
    /// [`CliConfig::session_id_pattern`].
    ///
    /// [`CliConfig::session_id_pattern`]: super::cli_config::CliConfig::session_id_pattern
    pub session_id_pattern: Option<String>,
}

/// Resolve the executable path for a CLI's configured `binary`.
///
/// - Absolute paths are used as-is, with no PATH lookup at all (this is how
///   CLIs like mimo are configured in `~/.canopy/config.toml` today).
/// - Bare names are resolved against PATH first.
/// - If PATH resolution fails, falls back to `~/.<binary>/bin/<binary>`,
///   since many CLI installers drop their binary there without ever
///   touching the (often PATH-minimal) systemd user environment.
pub fn resolve_binary(binary: &str) -> Result<PathBuf> {
    resolve_binary_with_home(binary, dirs::home_dir().as_deref())
}

fn resolve_binary_with_home(binary: &str, home: Option<&Path>) -> Result<PathBuf> {
    let path = Path::new(binary);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    if let Ok(resolved) = which::which(binary) {
        return Ok(resolved);
    }

    let Some(home) = home else {
        anyhow::bail!("CLI binary '{binary}' not found in PATH.");
    };

    let fallback = home.join(format!(".{binary}")).join("bin").join(binary);
    if fallback.is_file() {
        return Ok(fallback);
    }

    anyhow::bail!(
        "CLI binary '{binary}' not found. Looked in PATH and in {}",
        fallback.display()
    );
}

impl CliStrategy {
    /// Return a copy of this strategy with `prompt_via_stdin` forced to
    /// `true`. Used by the loop engine when the composed prompt exceeds
    /// the OS argv size limit — delivering via stdin avoids E2BIG
    /// regardless of what the CLI's registered capability says.
    pub fn with_stdin_forced(&self) -> Self {
        Self {
            prompt_via_stdin: true,
            ..self.clone()
        }
    }

    /// Build a command using the registry-defined configuration.
    ///
    /// Resolves `self.binary` to an actual executable path first, so a
    /// missing CLI fails with a clear message instead of a bare
    /// `os error 2` once the process is spawned.
    pub fn build_command(
        &self,
        prompt: &str,
        model: Option<&str>,
        working_dir: Option<&str>,
    ) -> Result<Command> {
        self.build_command_with_session(prompt, model, working_dir, None)
    }

    /// [`build_command`], additionally injecting a caller-chosen session id
    /// via the registry's `session_id_set_flag` (RS1 set-at-spawn capture).
    /// The id is silently dropped when the CLI has no such flag — callers
    /// decide whether to mint one by checking `session_id_set_flag` first.
    ///
    /// [`build_command`]: Self::build_command
    pub fn build_command_with_session(
        &self,
        prompt: &str,
        model: Option<&str>,
        working_dir: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Command> {
        let resolved = resolve_binary(&self.binary)?;
        let mut cmd = Command::new(resolved);

        // Make the child its own process-group leader so the engine can
        // `killpg` it (and any helpers it forks) as a unit on timeout/abnormal
        // end (B12), instead of leaving them to keep running past the daemon's
        // control. `kill_on_drop` is a cross-platform safety net for the
        // direct child alone, in case the `Command`/`Child` is ever dropped
        // without an explicit kill.
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);

        // Set environment variables
        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }

        // Add headless mode flags (before prompt)
        for arg in shell_words::split(&self.headless_mode).unwrap_or_default() {
            cmd.arg(arg);
        }

        // Set the session id at spawn time (RS1), when both the id and the
        // CLI's flag for it exist. Before the positional prompt so the id
        // can never be mistaken for it.
        if let Some(sid) = session_id {
            if let Some(ref flag) = self.session_id_set_flag {
                cmd.arg(flag).arg(sid);
            }
        }

        // Deliver the prompt via stdin (backed by an anonymous temp file) or
        // argv, per the CLI's registered capability. argv has an OS-level
        // per-argument/argv size cliff (Linux MAX_ARG_STRLEN, ARG_MAX) that a
        // large composed prompt (e.g. one embedding a prior node's full
        // output) can cross, crashing the spawn with E2BIG. Node outputs are
        // arbitrarily large, so any CLI that can read the prompt from stdin
        // instead should.
        if self.prompt_via_stdin {
            let mut file = tempfile::tempfile().context("failed to create temp file for prompt")?;
            file.write_all(prompt.as_bytes())
                .context("failed to write prompt to temp file")?;
            file.seek(SeekFrom::Start(0))
                .context("failed to rewind prompt temp file")?;
            cmd.stdin(std::process::Stdio::from(file));
        } else {
            cmd.arg(prompt);
            cmd.stdin(std::process::Stdio::null());
        }

        // Add model if specified
        if let Some(m) = model {
            if let Some(ref flag) = self.model_flag {
                cmd.arg(flag).arg(m);
            }
        }

        // Add working directory if supported
        if self.supports_working_dir {
            if let Some(dir) = working_dir {
                if let Some(ref flag) = self.working_dir_flag {
                    cmd.arg(flag).arg(dir);
                }
            }
        }

        Ok(cmd)
    }

    /// Whether list-after-run session id capture (RS1 phase 2) applies to
    /// this platform: it exposes a session-list command AND an id-extraction
    /// pattern, and has NO set-at-spawn flag. Set-at-spawn takes strict
    /// precedence — when it exists the id is known before the process starts,
    /// so the engine must never fall back to diffing session lists.
    pub fn can_capture_session_after_run(&self) -> bool {
        self.session_id_set_flag.is_none()
            && self.session_list_cmd.is_some()
            && self.session_id_pattern.is_some()
    }

    /// Build the registry-defined session-list command, to be run with the
    /// node's workdir as cwd (several CLIs scope their session list to the
    /// current project). Returns `Ok(None)` when the platform has no
    /// `session_list_cmd`. Only listing args are added — never the prompt,
    /// model, headless, or session-id-set flags — so this can never start a
    /// real session or consume model quota.
    pub fn build_session_list_command(&self, working_dir: &str) -> Result<Option<Command>> {
        let Some(list_cmd) = self.session_list_cmd.as_deref() else {
            return Ok(None);
        };
        let resolved = resolve_binary(&self.binary)?;
        let mut cmd = Command::new(resolved);

        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);

        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }
        for arg in shell_words::split(list_cmd).unwrap_or_default() {
            cmd.arg(arg);
        }
        if let Some(fmt) = self.session_list_format_args.as_deref() {
            for arg in shell_words::split(fmt).unwrap_or_default() {
                cmd.arg(arg);
            }
        }
        cmd.current_dir(working_dir);
        cmd.stdin(std::process::Stdio::null());
        Ok(Some(cmd))
    }

    /// Extract the set of session ids from list-command output using the
    /// registry-configured `session_id_pattern`. Capture group 1 is the id
    /// when the pattern has one; otherwise the whole match. Returns an empty
    /// set when no pattern is configured or it fails to compile — capture is
    /// best-effort and never surfaces an error to the run.
    pub fn extract_session_ids(&self, output: &str) -> std::collections::HashSet<String> {
        let mut ids = std::collections::HashSet::new();
        let Some(pattern) = self.session_id_pattern.as_deref() else {
            return ids;
        };
        let Ok(re) = regex::Regex::new(pattern) else {
            return ids;
        };
        for caps in re.captures_iter(output) {
            if let Some(m) = caps.get(1).or_else(|| caps.get(0)) {
                ids.insert(m.as_str().to_string());
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses an absolute (non-existent) path for `binary` so tests don't
    /// depend on any real CLI being installed on the machine running them.
    fn sample_strategy() -> CliStrategy {
        let mut env_vars = HashMap::new();
        env_vars.insert("FOO".to_string(), "bar".to_string());

        CliStrategy {
            binary: "/usr/local/bin/test-cli".to_string(),
            headless_mode: "--headless --quiet".to_string(),
            model_flag: Some("--model".to_string()),
            supports_working_dir: true,
            working_dir_flag: Some("--workdir".to_string()),
            env_vars,
            prompt_via_stdin: false,
            session_id_set_flag: None,
            session_list_cmd: None,
            session_list_format_args: None,
            session_id_pattern: None,
        }
    }

    #[test]
    fn test_build_command_basic() {
        let strategy = sample_strategy();
        let cmd = strategy.build_command("test prompt", None, None).unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("test-cli"));
    }

    #[test]
    fn test_build_command_with_model() {
        let strategy = sample_strategy();
        let cmd = strategy
            .build_command("test prompt", Some("gpt-4"), None)
            .unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("--model"));
        assert!(cmd_str.contains("gpt-4"));
    }

    #[test]
    fn test_build_command_with_working_dir() {
        let strategy = sample_strategy();
        let cmd = strategy
            .build_command("test prompt", None, Some("/tmp/project"))
            .unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("--workdir"));
        assert!(cmd_str.contains("/tmp/project"));
    }

    #[test]
    fn test_build_command_no_working_dir_when_not_supported() {
        let mut strategy = sample_strategy();
        strategy.supports_working_dir = false;

        let cmd = strategy
            .build_command("test prompt", None, Some("/tmp/project"))
            .unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(!cmd_str.contains("--workdir"));
    }

    #[test]
    fn test_build_command_no_model_flag() {
        let mut strategy = sample_strategy();
        strategy.model_flag = None;

        let cmd = strategy
            .build_command("test prompt", Some("gpt-4"), None)
            .unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(!cmd_str.contains("--model"));
    }

    #[test]
    fn test_build_command_empty_headless_mode() {
        let mut strategy = sample_strategy();
        strategy.headless_mode = String::new();

        let cmd = strategy.build_command("test prompt", None, None).unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("test-cli"));
    }

    #[test]
    fn test_with_stdin_forced_overrides_flag() {
        let mut strategy = sample_strategy();
        strategy.prompt_via_stdin = false;
        let forced = strategy.with_stdin_forced();
        assert!(
            forced.prompt_via_stdin,
            "with_stdin_forced must set prompt_via_stdin to true"
        );
        assert!(!strategy.prompt_via_stdin, "original must be unchanged");
        assert_eq!(
            strategy.binary, forced.binary,
            "all other fields must be preserved"
        );
    }

    #[test]
    fn test_build_command_prompt_via_stdin_keeps_prompt_out_of_argv() {
        let mut strategy = sample_strategy();
        strategy.prompt_via_stdin = true;

        let cmd = strategy
            .build_command("this must not appear in argv", None, None)
            .unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(!cmd_str.contains("this must not appear in argv"));
    }

    #[tokio::test]
    async fn test_build_command_prompt_via_stdin_delivers_huge_prompt() {
        // A multi-hundred-KB prompt would blow argv (Linux MAX_ARG_STRLEN is
        // 128KiB) if passed via `cmd.arg`. Piped via stdin it has no
        // input-size cliff: spawn `cat`, which just echoes stdin to stdout.
        let mut strategy = sample_strategy();
        strategy.binary = "/bin/cat".to_string();
        strategy.headless_mode = String::new();
        strategy.model_flag = None;
        strategy.supports_working_dir = false;
        strategy.prompt_via_stdin = true;

        let huge_prompt = "x".repeat(500 * 1024);
        let mut cmd = strategy.build_command(&huge_prompt, None, None).unwrap();
        cmd.stdout(std::process::Stdio::piped());

        let output = cmd.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), huge_prompt);
    }

    #[test]
    fn build_command_with_session_injects_set_flag_and_id() {
        let mut strategy = sample_strategy();
        strategy.session_id_set_flag = Some("--session-id".to_string());
        let cmd = strategy
            .build_command_with_session(
                "p",
                None,
                None,
                Some("11111111-2222-3333-4444-555555555555"),
            )
            .unwrap();
        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("--session-id"));
        assert!(cmd_str.contains("11111111-2222-3333-4444-555555555555"));
    }

    #[test]
    fn build_command_with_session_without_flag_drops_id() {
        // sample_strategy has no session_id_set_flag: the id must be
        // silently dropped, never passed as a stray argument.
        let strategy = sample_strategy();
        let cmd = strategy
            .build_command_with_session("p", None, None, Some("sid-123"))
            .unwrap();
        let cmd_str = format!("{:?}", cmd);
        assert!(!cmd_str.contains("sid-123"));
    }

    #[test]
    fn build_command_never_injects_session_flag_without_id() {
        let mut strategy = sample_strategy();
        strategy.session_id_set_flag = Some("--session-id".to_string());
        let cmd = strategy.build_command("p", None, None).unwrap();
        let cmd_str = format!("{:?}", cmd);
        assert!(!cmd_str.contains("--session-id"));
    }

    #[test]
    fn test_build_command_all_options() {
        let strategy = sample_strategy();
        let cmd = strategy
            .build_command("my prompt", Some("claude-3"), Some("/home/project"))
            .unwrap();

        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("my prompt"));
        assert!(cmd_str.contains("--model"));
        assert!(cmd_str.contains("claude-3"));
        assert!(cmd_str.contains("--workdir"));
        assert!(cmd_str.contains("/home/project"));
    }

    #[test]
    fn can_capture_session_after_run_requires_list_and_pattern_without_set_flag() {
        let mut s = sample_strategy();
        assert!(!s.can_capture_session_after_run(), "nothing configured");

        s.session_list_cmd = Some("session list".to_string());
        assert!(!s.can_capture_session_after_run(), "pattern still missing");

        s.session_id_pattern = Some("\"id\"\\s*:\\s*\"([^\"]+)\"".to_string());
        assert!(s.can_capture_session_after_run(), "list + pattern present");

        // Set-at-spawn takes precedence and disables list-after-run capture.
        s.session_id_set_flag = Some("--session-id".to_string());
        assert!(!s.can_capture_session_after_run(), "set-at-spawn wins");
    }

    #[test]
    fn extract_session_ids_pulls_id_key_from_opencode_family_json() {
        let mut s = sample_strategy();
        s.session_id_pattern = Some("\"id\"\\s*:\\s*\"([^\"]+)\"".to_string());
        // Real opencode/mimo/kilo shape: a JSON array whose objects also carry
        // a `projectId` (which must NOT be mistaken for `id`).
        let output = r#"[
          {"id": "ses_AAA", "projectId": "hexhexhex", "directory": "/x"},
          {"id": "ses_BBB", "projectId": "hexhexhex", "directory": "/y"}
        ]"#;
        let ids = s.extract_session_ids(output);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("ses_AAA"));
        assert!(ids.contains("ses_BBB"));
    }

    #[test]
    fn extract_session_ids_pulls_id_key_from_cn_json() {
        let mut s = sample_strategy();
        s.session_id_pattern = Some("\"id\"\\s*:\\s*\"([^\"]+)\"".to_string());
        // Real cn shape: an object wrapping a `sessions` array.
        let output = r#"{"sessions": [
          {"id": "8ae15a84-fec0-43b4-9cb8-47293662302e", "title": "x"},
          {"id": "633286f6-0820-46a5-8c8f-ed315faa5e49", "title": "y"}
        ]}"#;
        let ids = s.extract_session_ids(output);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("8ae15a84-fec0-43b4-9cb8-47293662302e"));
    }

    #[test]
    fn extract_session_ids_empty_without_pattern() {
        let s = sample_strategy();
        assert!(s.extract_session_ids(r#"[{"id":"ses_X"}]"#).is_empty());
    }

    #[test]
    fn build_session_list_command_none_without_list_cmd() {
        let s = sample_strategy();
        assert!(s.build_session_list_command("/tmp").unwrap().is_none());
    }

    #[test]
    fn build_session_list_command_appends_format_args_and_sets_cwd() {
        let mut s = sample_strategy();
        s.session_list_cmd = Some("session list".to_string());
        s.session_list_format_args = Some("--format json".to_string());
        let cmd = s
            .build_session_list_command("/tmp/project")
            .unwrap()
            .expect("list command must be built");
        let cmd_str = format!("{:?}", cmd);
        assert!(cmd_str.contains("session"));
        assert!(cmd_str.contains("list"));
        assert!(cmd_str.contains("--format"));
        assert!(cmd_str.contains("json"));
        // The prompt/headless/model flags must never appear on a list command.
        assert!(!cmd_str.contains("--headless"));
        assert!(!cmd_str.contains("--model"));
    }

    #[test]
    fn resolve_binary_absolute_path_used_as_is_without_touching_path() {
        // Deliberately a path that does not exist: absolute paths must be
        // returned verbatim, with no PATH lookup and no existence check.
        let resolved = resolve_binary_with_home("/nonexistent/somewhere/mimo", None).unwrap();
        assert_eq!(resolved, PathBuf::from("/nonexistent/somewhere/mimo"));
    }

    #[test]
    fn resolve_binary_bare_name_falls_back_to_dot_dir_bin() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let bin_dir = home.join(".canopy-test-fixture-cli").join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake_binary = bin_dir.join("canopy-test-fixture-cli");
        std::fs::write(&fake_binary, "#!/bin/sh\n").unwrap();

        let resolved = resolve_binary_with_home("canopy-test-fixture-cli", Some(home)).unwrap();
        assert_eq!(resolved, fake_binary);
    }

    #[test]
    fn resolve_binary_not_found_anywhere_names_binary_and_searched_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let err =
            resolve_binary_with_home("canopy-test-fixture-cli-missing", Some(home)).unwrap_err();
        let message = err.to_string();

        assert!(message.contains("canopy-test-fixture-cli-missing"));
        assert!(message.contains("PATH"));
        assert!(message.contains(
            home.join(".canopy-test-fixture-cli-missing")
                .join("bin")
                .join("canopy-test-fixture-cli-missing")
                .to_str()
                .unwrap()
        ));
    }
}
