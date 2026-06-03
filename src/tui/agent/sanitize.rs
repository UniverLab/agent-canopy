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

pub fn looks_like_shell_prompt(line: &str) -> bool {
    let trimmed = line.trim_start();
    TERMINAL_SHELL_PROMPTS
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
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
    use super::{is_ui_line, line_looks_sensitive_prompt, looks_like_shell_prompt, strip_shell_prompt_prefix};

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
        // When terminal is narrow, the prompt may wrap and the keyword
        // may appear on a line that doesn't end with : or ?
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
    fn detects_shell_prompt_boundaries() {
        assert!(looks_like_shell_prompt("$ git push"));
        assert!(looks_like_shell_prompt("# root command"));
        assert!(looks_like_shell_prompt("❯ zsh prompt"));
        assert!(!looks_like_shell_prompt("Enter passphrase for key"));
        assert!(!looks_like_shell_prompt("Total 5, reused 3"));
        assert!(!looks_like_shell_prompt("  indented text"));
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
    fn keeps_informational_lines_with_text() {
        assert!(!is_ui_line("✓ MCP server github synced"));
        assert!(!is_ui_line("remaining tasks: 2"));
        assert!(!is_ui_line("... waiting for input"));
    }
}
