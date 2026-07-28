const TERMINAL_SHELL_PROMPTS: [&str; 5] = ["$ ", "# ", "> ", "% ", "❯ "];
const SENSITIVE_PROMPT_HINTS: [&str; 7] = [
    "passphrase",
    "password",
    "passcode",
    "pin",
    "otp",
    "token",
    "verification code",
];

pub fn sanitize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_escape = false;

    for ch in line.chars() {
        if ch == '\x1b' {
            // ESC — start of ANSI sequence
            in_escape = true;
        } else if in_escape {
            // Inside escape sequence: keep going until we see a letter or ~
            if ch.is_ascii_alphabetic() || ch == '~' || ch == 'K' || ch == 'H' {
                in_escape = false;
            }
            // Drop the escape char and the sequence
        } else if ch.is_control() && ch != '\t' {
            // Drop other control chars except tab
        } else {
            out.push(ch);
        }
    }

    out
}

pub fn line_looks_sensitive_prompt(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    SENSITIVE_PROMPT_HINTS
        .iter()
        .any(|hint| lower.contains(hint))
}

/// Extract the *current command* portion of a line: the text that follows the
/// last shell-prompt marker.
///
/// Real prompts carry the marker mid-line (`user@host:~/path$ cmd`), so this
/// scans for the rightmost `$`, `#`, `%` or `❯` that is a prompt boundary
/// (followed by a space, or the trailing glyph of an otherwise-empty prompt),
/// plus the leading `> ` some password prompts use.
///
/// Returns `None` when the line carries no marker at all — that means the line
/// is program output (e.g. a wrapped `Vault passphrase:` prompt) rather than a
/// shell prompt, so callers can keep walking earlier rows.
pub fn command_after_shell_prompt(line: &str) -> Option<String> {
    let trimmed = line.trim_start();

    // Some password prompts render with a leading `> ` (e.g. `> Passphrase:`).
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return Some(rest.trim().to_string());
    }

    // Rightmost prompt-boundary marker. A boundary is a marker glyph that is
    // either the last visible glyph (empty command) or immediately followed by
    // a space (command text after it). This keeps `100% done` and redirects
    // from being mistaken for prompts mid-word.
    let chars: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut command_start: Option<usize> = None;
    for (i, (byte_idx, ch)) in chars.iter().enumerate() {
        if !matches!(ch, '$' | '#' | '%' | '❯') {
            continue;
        }
        let is_boundary = matches!(chars.get(i + 1).map(|(_, c)| *c), None | Some(' '));
        if is_boundary {
            command_start = Some(byte_idx + ch.len_utf8());
        }
    }

    command_start.map(|idx| trimmed[idx..].trim().to_string())
}

pub fn strip_shell_prompt_prefix(line: &str) -> String {
    let trimmed = line.trim_start();
    for prefix in TERMINAL_SHELL_PROMPTS {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.to_string();
        }
    }

    for marker in ["$ ", "# ", "% ", "❯ "] {
        if let Some((_, rest)) = trimmed.rsplit_once(marker) {
            let candidate = rest.trim_start();
            if !candidate.is_empty() {
                return candidate.to_string();
            }
        }
    }

    trimmed.to_string()
}

/// Returns true if `c` is a box-drawing or block-element character
/// (Unicode ranges: Box Drawing U+2500–257F, Block Elements U+2580–259F).
pub fn is_decoration_char(c: char) -> bool {
    matches!(c,
        // Box Drawing (U+2500–U+257F)
        '─'..='╿'
        // Block Elements (U+2580–U+259F) — includes █ ░ ▒ ▓ and all half/quarter blocks
        | '▀'..='▟'
        // Dashes
        | '‐' | '–' | '—' | '−'
    )
}

/// Detect if a line is UI noise that should be excluded from context transfer.
///
/// Catches: box-drawing borders, block-element bars, CLI prompts,
/// status bars, tool-use indicators, MCP messages, and similar chrome.
/// Lines with box-drawing borders that contain text content between them
/// are NOT treated as UI lines — the content is extracted by `strip_borders`.
pub fn is_ui_line(line: &str) -> bool {
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return true;
    }

    // Lines composed entirely of decoration chars + whitespace
    if trimmed.chars().all(|c| c == ' ' || is_decoration_char(c)) {
        return true;
    }

    // Lines with box-drawing borders: extract inner text and check if it's empty.
    // TUI agents (opencode, claude, copilot) render responses inside │ borders.
    if trimmed.starts_with('│') || trimmed.starts_with('┃') || trimmed.starts_with('║') {
        let inner = strip_borders(trimmed);
        // If inner is empty after stripping, it's a purely decorative border
        return inner.trim().is_empty();
    }

    // Common CLI prompts/status indicators
    if trimmed.starts_with('❯')
        || trimmed.starts_with('$')
        || trimmed.starts_with('#')
        || trimmed.contains("───")
    {
        return true;
    }

    // Bare UI glyph lines with no substantive text.
    if matches!(trimmed, "..." | "●" | "▌" | "▣" | "▹" | "ℹ" | "✓") {
        return true;
    }

    // Status bar / footer patterns
    if trimmed == "Environment"
        || trimmed == "for shortcuts"
        || trimmed == "Shift+Tab"
        || trimmed == "MCP issues"
        || trimmed == "MCP servers"
        || trimmed.starts_with("workspace (")
    {
        return true;
    }

    false
}

/// Strip box-drawing border characters from the beginning and end of a line.
/// E.g. `│ Hello world │` → `Hello world`.
pub fn strip_borders(line: &str) -> &str {
    let trimmed = line.trim();
    // Strip leading border char(s) + whitespace
    let start = trimmed
        .char_indices()
        .find(|(_, c)| !is_decoration_char(*c) && *c != ' ')
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    let inner = &trimmed[start..];
    // Strip trailing border char(s) + whitespace
    let end = inner
        .char_indices()
        .rev()
        .find(|(_, c)| !is_decoration_char(*c) && *c != ' ')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    &inner[..end]
}

#[cfg(test)]
mod tests {
    use super::{
        command_after_shell_prompt, is_ui_line, line_looks_sensitive_prompt,
        strip_shell_prompt_prefix,
    };

    #[test]
    fn detects_sensitive_prompts() {
        assert!(line_looks_sensitive_prompt(
            "Enter passphrase for key '/tmp/id_rsa':"
        ));
        assert!(line_looks_sensitive_prompt(
            "Password for https://example.com?"
        ));
        assert!(!line_looks_sensitive_prompt("$ git push"));
    }

    #[test]
    fn detects_sensitive_prompts_without_suffix() {
        assert!(line_looks_sensitive_prompt("Enter passphrase for key"));
        assert!(line_looks_sensitive_prompt("Enter your password"));
        assert!(line_looks_sensitive_prompt(
            "Please enter the verification code"
        ));
    }

    #[test]
    fn strips_common_shell_prompts() {
        assert_eq!(strip_shell_prompt_prefix("$ git status"), "git status");
        assert_eq!(strip_shell_prompt_prefix("# cargo test"), "cargo test");
        assert_eq!(strip_shell_prompt_prefix("plain text"), "plain text");
    }

    #[test]
    fn strips_prompt_suffixes_from_redrawn_shell_lines() {
        assert_eq!(
            strip_shell_prompt_prefix(
                "/mnt/.../harness-canopy ❯ jheisonmblivecom@WORKSTATION:/repo$ ls src"
            ),
            "ls src"
        );
    }

    #[test]
    fn command_after_shell_prompt_extracts_current_command() {
        // Decorated prompt with the marker mid-line.
        assert_eq!(
            command_after_shell_prompt("user@host:~/repo$ git push").as_deref(),
            Some("git push")
        );
        // Fresh empty prompt → empty command (never sensitive).
        assert_eq!(
            command_after_shell_prompt("user@host:~/repo$ ").as_deref(),
            Some("")
        );
        assert_eq!(
            command_after_shell_prompt("user@host:~/repo$").as_deref(),
            Some("")
        );
        // Leading `> ` password prompt.
        assert_eq!(
            command_after_shell_prompt("> Vault passphrase: ").as_deref(),
            Some("Vault passphrase:")
        );
    }

    #[test]
    fn command_after_shell_prompt_ignores_plain_output() {
        // A stale error mentioning passphrase is program output, not a prompt.
        assert_eq!(
            command_after_shell_prompt("Error: Decryption failed — wrong passphrase"),
            None
        );
        assert_eq!(command_after_shell_prompt("Receiving objects: 100"), None);
    }

    #[test]
    fn fresh_prompt_after_error_is_not_sensitive() {
        // Regression: an empty prompt redrawn after a `… wrong passphrase` error
        // must not be treated as a sensitive (hidden) input line.
        let command = command_after_shell_prompt(
            "jheisonmblivecom@WORKSTATION:~/Projects/UniverLab/scripts/ghscaff$ ",
        )
        .unwrap();
        assert!(!line_looks_sensitive_prompt(&command));
    }

    #[test]
    fn keeps_informational_lines_with_text() {
        assert!(!is_ui_line("✓ MCP server github synced"));
        assert!(!is_ui_line("remaining tasks: 2"));
        assert!(!is_ui_line("... waiting for input"));
    }
}
