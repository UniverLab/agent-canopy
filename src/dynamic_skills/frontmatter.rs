//! Minimal parser for the one-line description shown in `skill_list`.
//!
//! Reads the `description:` key from a SKILL.md's YAML frontmatter, falling
//! back to the first `#` heading. This is not a general YAML parser — it
//! only covers the single-line and folded/literal block-scalar (`>`/`|`)
//! forms that skills in the wild actually use.

/// Extract a one-line description from SKILL.md/INSTRUCTIONS.md content.
pub fn parse_description(content: &str) -> String {
    if let Some(desc) = frontmatter_description(content) {
        if !desc.is_empty() {
            return desc;
        }
    }
    first_heading(content).unwrap_or_default()
}

fn frontmatter_description(content: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let body: Vec<&str> = lines.by_ref().take_while(|l| l.trim() != "---").collect();

    let mut iter = body.into_iter().peekable();
    while let Some(line) = iter.next() {
        let Some(rest) = line.strip_prefix("description:") else {
            continue;
        };
        let rest = rest.trim();
        if rest.starts_with('>') || rest.starts_with('|') {
            let mut collected = Vec::new();
            while let Some(next) = iter.peek() {
                if next.trim().is_empty() {
                    iter.next();
                    continue;
                }
                let indent = next.len() - next.trim_start().len();
                if indent == 0 {
                    break;
                }
                collected.push(next.trim());
                iter.next();
            }
            return Some(collected.join(" "));
        }
        return Some(rest.trim_matches('"').trim_matches('\'').to_string());
    }
    None
}

fn first_heading(content: &str) -> Option<String> {
    content
        .lines()
        .find(|l| l.trim_start().starts_with('#'))
        .map(|l| l.trim_start().trim_start_matches('#').trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_line_quoted_description() {
        let content = "---\nname: foo\ndescription: \"A quoted one-liner.\"\n---\n# Foo\n";
        assert_eq!(parse_description(content), "A quoted one-liner.");
    }

    #[test]
    fn parses_folded_block_scalar_description() {
        let content = "---\nname: foo\ndescription: >\n  First line of the\n  folded description.\n\n  More detail.\nlicense: MIT\n---\n# Foo\n";
        assert_eq!(
            parse_description(content),
            "First line of the folded description. More detail."
        );
    }

    #[test]
    fn falls_back_to_first_heading_without_frontmatter() {
        let content = "# My Skill\n\nSome body text.\n";
        assert_eq!(parse_description(content), "My Skill");
    }

    #[test]
    fn falls_back_to_heading_when_frontmatter_has_no_description() {
        let content = "---\nname: foo\n---\n# Foo Skill\nbody\n";
        assert_eq!(parse_description(content), "Foo Skill");
    }

    #[test]
    fn no_description_or_heading_yields_empty_string() {
        let content = "just plain text, no heading, no frontmatter";
        assert_eq!(parse_description(content), "");
    }
}
