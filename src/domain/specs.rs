use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecSection {
    Objective,
    FunctionalRequirements,
    NonFunctionalRequirements,
    Constraints,
    Guidelines,
    InScope,
    OutOfScope,
}

impl SpecSection {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Objective => "objective",
            Self::FunctionalRequirements => "functional_requirements",
            Self::NonFunctionalRequirements => "non_functional_requirements",
            Self::Constraints => "constraints",
            Self::Guidelines => "guidelines",
            Self::InScope => "in_scope",
            Self::OutOfScope => "out_of_scope",
        }
    }

    pub fn all() -> &'static [SpecSection] {
        &[
            Self::Objective,
            Self::FunctionalRequirements,
            Self::NonFunctionalRequirements,
            Self::Constraints,
            Self::Guidelines,
            Self::InScope,
            Self::OutOfScope,
        ]
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        for section in Self::all() {
            if section.tag() == tag {
                return Some(*section);
            }
        }
        None
    }

    pub fn legacy_heading_patterns(self) -> &'static [&'static str] {
        match self {
            Self::Objective => &["objective", "expected outcome"],
            Self::FunctionalRequirements => &["functional requirements"],
            Self::NonFunctionalRequirements => {
                &["non-functional requirements", "non functional requirements"]
            }
            Self::Constraints => &["constraints", "what to respect", "restrictions"],
            Self::Guidelines => &["guidelines", "guidance", "lineamientos"],
            Self::InScope => &["in scope", "scope in"],
            Self::OutOfScope => &["out of scope", "scope out"],
        }
    }
}

impl fmt::Display for SpecSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tag())
    }
}

#[derive(Debug)]
pub enum SpecParseError {
    MissingTag(String),
    DuplicateTag(String),
    UnexpectedTag(String),
    MalformedClosingTag(String),
    MissingSpecWrapper(String),
    LegacyConversionFailed(String),
}

impl fmt::Display for SpecParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTag(tag) => write!(f, "spec is missing required tag <{tag}>"),
            Self::DuplicateTag(tag) => write!(f, "spec contains duplicate tag <{tag}>"),
            Self::UnexpectedTag(tag) => write!(f, "spec has an unexpected tag <{tag}>"),
            Self::MalformedClosingTag(tag) => write!(f, "spec has malformed closing tag </{tag}>"),
            Self::MissingSpecWrapper(msg) => write!(f, "{msg}"),
            Self::LegacyConversionFailed(msg) => write!(f, "legacy spec conversion failed: {msg}"),
        }
    }
}

impl std::error::Error for SpecParseError {}

#[derive(Debug, Clone)]
pub struct ParsedTaggedSpec {
    pub sections: Vec<(SpecSection, String)>,
}

impl ParsedTaggedSpec {
    pub fn section_body(&self, section: SpecSection) -> Option<&str> {
        self.sections
            .iter()
            .find(|(s, _)| *s == section)
            .map(|(_, body)| body.as_str())
    }
}

#[derive(Debug)]
pub enum ParsedSpecDescription {
    Tagged(ParsedTaggedSpec),
    Legacy(String),
}

#[derive(Debug)]
pub enum SpecSectionResult {
    Tagged { content: String },
    Legacy { content: String },
}

pub fn parse_spec_description(input: &str) -> Result<ParsedSpecDescription, SpecParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(SpecParseError::MissingSpecWrapper(
            "spec description is empty".to_string(),
        ));
    }

    if looks_like_tagged(trimmed) {
        let parsed = parse_tagged_spec(trimmed)?;
        return Ok(ParsedSpecDescription::Tagged(parsed));
    }

    if looks_like_legacy(trimmed) {
        return Ok(ParsedSpecDescription::Legacy(trimmed.to_string()));
    }

    Err(SpecParseError::MissingSpecWrapper(
        "spec is missing required <spec> wrapper tag".to_string(),
    ))
}

fn looks_like_tagged(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.starts_with("<spec>") || trimmed.starts_with("<spec ")
}

fn looks_like_legacy(input: &str) -> bool {
    let lower = input.to_lowercase();
    let mut found = 0;
    for section in SpecSection::all() {
        for pattern in section.legacy_heading_patterns() {
            let p = pattern.to_lowercase();
            if lower.contains(&format!("## {p}"))
                || lower.contains(&format!("{p}:"))
                || lower.contains(&format!("{p}\n"))
            {
                found += 1;
                break;
            }
        }
    }
    found >= 3
}

fn parse_tagged_spec(input: &str) -> Result<ParsedTaggedSpec, SpecParseError> {
    let trimmed = input.trim();

    let inner = extract_spec_wrapper_content(trimmed)?;

    let mut found_sections: Vec<(SpecSection, String)> = Vec::new();
    let mut seen_tags = std::collections::HashSet::new();

    let canonical_tags: Vec<&str> = SpecSection::all().iter().map(|s| s.tag()).collect();

    let mut remaining = inner.as_str();

    while !remaining.trim().is_empty() {
        remaining = remaining.trim_start();
        if remaining.is_empty() {
            break;
        }

        if !remaining.starts_with('<') {
            let non_tag = remaining.trim();
            if !non_tag.is_empty() {
                let snippet = non_tag.chars().take(40).collect::<String>();
                return Err(SpecParseError::UnexpectedTag(format!(
                    "unexpected content before tag: '{}...'",
                    snippet
                )));
            }
            break;
        }

        let (tag_name, is_closing) = parse_opening_tag_name(remaining)?;

        if is_closing {
            return Err(SpecParseError::UnexpectedTag(format!(
                "unexpected closing tag </{}> at top level",
                tag_name
            )));
        }

        if tag_name == "spec" {
            return Err(SpecParseError::UnexpectedTag(
                "nested <spec> tag is not allowed".to_string(),
            ));
        }

        let Some(section) = SpecSection::from_tag(&tag_name) else {
            return Err(SpecParseError::UnexpectedTag(tag_name));
        };

        if seen_tags.contains(&section) {
            return Err(SpecParseError::DuplicateTag(tag_name));
        }
        seen_tags.insert(section);

        let (body, after) = extract_section_body(inner.as_str(), remaining, &tag_name)?;

        check_no_nested_canonical_tags(&body, &canonical_tags, &tag_name)?;

        found_sections.push((section, body));
        remaining = after;
    }

    for section in SpecSection::all() {
        if !seen_tags.contains(section) {
            return Err(SpecParseError::MissingTag(section.tag().to_string()));
        }
    }

    Ok(ParsedTaggedSpec {
        sections: found_sections,
    })
}

fn extract_spec_wrapper_content(input: &str) -> Result<String, SpecParseError> {
    let trimmed = input.trim();

    if !trimmed.starts_with("<spec>") && !trimmed.starts_with("<spec ") {
        return Err(SpecParseError::MissingSpecWrapper(
            "spec is missing required <spec> wrapper tag".to_string(),
        ));
    }

    let after_open = if let Some(stripped) = trimmed.strip_prefix("<spec>") {
        stripped
    } else {
        match trimmed.find('>') {
            Some(pos) => &trimmed[pos + 1..],
            None => {
                return Err(SpecParseError::MissingSpecWrapper(
                    "spec has malformed <spec> opening tag".to_string(),
                ));
            }
        }
    };

    let close_tag = "</spec>";
    let Some(close_pos) = after_open.rfind(close_tag) else {
        return Err(SpecParseError::MissingSpecWrapper(
            "spec is missing closing </spec> tag".to_string(),
        ));
    };

    let inner = &after_open[..close_pos];
    let after_close = &after_open[close_pos + close_tag.len()..];

    if !after_close.trim().is_empty() {
        return Err(SpecParseError::MissingSpecWrapper(
            "spec has content after closing </spec> tag".to_string(),
        ));
    }

    Ok(inner.to_string())
}

fn parse_opening_tag_name(input: &str) -> Result<(String, bool), SpecParseError> {
    if !input.starts_with('<') {
        return Err(SpecParseError::UnexpectedTag(
            "expected '<' at start of tag".to_string(),
        ));
    }

    let is_closing = input.starts_with("</");
    let after_bracket = if is_closing { &input[2..] } else { &input[1..] };

    let end = after_bracket
        .find(|c: char| c == '>' || c.is_whitespace())
        .unwrap_or(after_bracket.len());

    let tag_name = &after_bracket[..end];
    if tag_name.is_empty() {
        return Err(SpecParseError::UnexpectedTag("empty tag name".to_string()));
    }

    for ch in tag_name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '_' {
            return Err(SpecParseError::UnexpectedTag(format!(
                "invalid character '{}' in tag name",
                ch
            )));
        }
    }

    Ok((tag_name.to_string(), is_closing))
}

fn extract_section_body<'a>(
    full_inner: &'a str,
    from_here: &'a str,
    tag_name: &str,
) -> Result<(String, &'a str), SpecParseError> {
    let open_tag_end = from_here.find('>').ok_or_else(|| {
        SpecParseError::UnexpectedTag(format!("tag <{}> is not closed", tag_name))
    })?;
    let after_open = &from_here[open_tag_end + 1..];

    let close_tag = format!("</{}>", tag_name);
    let close_pos = after_open
        .find(&close_tag)
        .ok_or_else(|| SpecParseError::MalformedClosingTag(tag_name.to_string()))?;

    let body = &after_open[..close_pos];
    let after_close = &after_open[close_pos + close_tag.len()..];

    let _ = full_inner;

    let trimmed_body = body.to_string();

    Ok((trimmed_body, after_close))
}

fn check_no_nested_canonical_tags(
    body: &str,
    canonical_tags: &[&str],
    _parent_tag: &str,
) -> Result<(), SpecParseError> {
    for &tag in canonical_tags {
        let open_pattern = format!("<{}", tag);
        let close_pattern = format!("</{}>", tag);

        for line in body.lines() {
            let trimmed_line = line.trim();
            if trimmed_line.starts_with(&open_pattern)
                && (trimmed_line.len() == open_pattern.len()
                    || trimmed_line.as_bytes().get(open_pattern.len()) == Some(&b'>')
                    || trimmed_line.as_bytes().get(open_pattern.len()) == Some(&b' '))
            {
                return Err(SpecParseError::UnexpectedTag(format!(
                    "nested <{}> tag found in section body",
                    tag
                )));
            }
            if trimmed_line.starts_with(&close_pattern) {
                return Err(SpecParseError::UnexpectedTag(format!(
                    "nested </{}> tag found in section body",
                    tag
                )));
            }
        }
    }
    Ok(())
}

pub fn validate_spec_description_template(description: &str) -> Result<(), String> {
    match parse_spec_description(description) {
        Ok(ParsedSpecDescription::Tagged(_)) => Ok(()),
        Ok(ParsedSpecDescription::Legacy(_)) => Err(
            "spec is in legacy heading format and needs conversion to <spec> tag format. Required tags: <objective>, <functional_requirements>, <non_functional_requirements>, <constraints>, <guidelines>, <in_scope>, <out_of_scope>"
                .to_string(),
        ),
        Err(e) => Err(e.to_string()),
    }
}

pub fn extract_spec_section(
    input: &str,
    section: SpecSection,
) -> Result<SpecSectionResult, SpecParseError> {
    match parse_spec_description(input)? {
        ParsedSpecDescription::Tagged(parsed) => {
            let content = parsed.section_body(section).unwrap_or("").to_string();
            Ok(SpecSectionResult::Tagged { content })
        }
        ParsedSpecDescription::Legacy(body) => {
            let content = extract_legacy_section(&body, section)?;
            Ok(SpecSectionResult::Legacy { content })
        }
    }
}

fn extract_legacy_section(body: &str, target: SpecSection) -> Result<String, SpecParseError> {
    let sections = parse_legacy_sections(body)?;
    for (section, content) in &sections {
        if *section == target {
            return Ok(content.clone());
        }
    }
    Ok(String::new())
}

fn parse_legacy_sections(body: &str) -> Result<Vec<(SpecSection, String)>, SpecParseError> {
    let lines: Vec<&str> = body.lines().collect();
    let mut sections: Vec<(SpecSection, usize)> = Vec::new();

    for (line_idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim().to_lowercase();
        let stripped = trimmed
            .trim_start_matches('#')
            .trim_start_matches("- ")
            .trim_start_matches("* ")
            .trim()
            .trim_end_matches(':')
            .trim()
            .to_string();

        for section in SpecSection::all() {
            for pattern in section.legacy_heading_patterns() {
                if stripped == *pattern {
                    sections.push((*section, line_idx));
                    break;
                }
            }
        }
    }

    let mut result = Vec::new();
    for (i, (section, start_line)) in sections.iter().enumerate() {
        let end_line = if i + 1 < sections.len() {
            sections[i + 1].1
        } else {
            lines.len()
        };

        let body_lines: Vec<&str> = lines[*start_line + 1..end_line].to_vec();
        let content = body_lines.join("\n").trim().to_string();

        result.push((*section, content));
    }

    Ok(result)
}

pub fn convert_legacy_spec_description(input: &str) -> Result<String, SpecParseError> {
    let trimmed = input.trim();

    if looks_like_tagged(trimmed) {
        parse_tagged_spec(trimmed)?;
        return Ok(trimmed.to_string());
    }

    let lines: Vec<&str> = trimmed.lines().collect();
    let mut section_starts: Vec<(SpecSection, usize)> = Vec::new();

    for (line_idx, line) in lines.iter().enumerate() {
        let trimmed_line = line.trim().to_lowercase();
        let stripped = trimmed_line
            .trim_start_matches('#')
            .trim_start()
            .trim_start_matches("- ")
            .trim_start_matches("* ")
            .trim()
            .trim_end_matches(':')
            .trim()
            .to_string();

        for section in SpecSection::all() {
            for pattern in section.legacy_heading_patterns() {
                if stripped == *pattern {
                    section_starts.push((*section, line_idx));
                    break;
                }
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    for (section, _) in &section_starts {
        if !seen.insert(*section) {
            return Err(SpecParseError::LegacyConversionFailed(format!(
                "duplicate heading for section <{}>",
                section.tag()
            )));
        }
    }

    for section in SpecSection::all() {
        if !section_starts.iter().any(|(s, _)| *s == *section) {
            return Err(SpecParseError::LegacyConversionFailed(format!(
                "missing heading for required section <{}>",
                section.tag()
            )));
        }
    }

    section_starts.sort_by_key(|(_, line)| *line);

    let mut output = String::from("<spec>\n");
    for (i, (section, start_line)) in section_starts.iter().enumerate() {
        let end_line = if i + 1 < section_starts.len() {
            section_starts[i + 1].1
        } else {
            lines.len()
        };

        let body_lines: Vec<&str> = lines[*start_line + 1..end_line].to_vec();
        let body = body_lines.join("\n").trim().to_string();

        output.push_str(&format!("  <{}>\n", section.tag()));
        if !body.is_empty() {
            for line in body.lines() {
                output.push_str("  ");
                output.push_str(line);
                output.push('\n');
            }
        }
        output.push_str(&format!("  </{}>\n", section.tag()));
    }
    output.push_str("</spec>");

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_tagged_spec() -> String {
        r#"<spec>
  <objective>Ship the feature.</objective>
  <functional_requirements>Does the thing.</functional_requirements>
  <non_functional_requirements>Is fast.</non_functional_requirements>
  <constraints>None extra.</constraints>
  <guidelines>Follow house style.</guidelines>
  <in_scope>This change.</in_scope>
  <out_of_scope>Everything else.</out_of_scope>
</spec>"#
            .to_string()
    }

    #[test]
    fn parse_valid_tagged_canonical_order() {
        let result = parse_spec_description(&valid_tagged_spec()).unwrap();
        match result {
            ParsedSpecDescription::Tagged(parsed) => {
                assert_eq!(parsed.sections.len(), 7);
                assert_eq!(
                    parsed.section_body(SpecSection::Objective),
                    Some("Ship the feature.")
                );
            }
            _ => panic!("expected tagged"),
        }
    }

    #[test]
    fn parse_valid_tagged_arbitrary_order() {
        let input = r#"<spec>
  <constraints>None extra.</constraints>
  <out_of_scope>Everything else.</out_of_scope>
  <objective>Ship the feature.</objective>
  <guidelines>Follow house style.</guidelines>
  <functional_requirements>Does the thing.</functional_requirements>
  <in_scope>This change.</in_scope>
  <non_functional_requirements>Is fast.</non_functional_requirements>
</spec>"#;
        let result = parse_spec_description(input).unwrap();
        match result {
            ParsedSpecDescription::Tagged(parsed) => {
                assert_eq!(parsed.sections.len(), 7);
            }
            _ => panic!("expected tagged"),
        }
    }

    #[test]
    fn parse_accepts_markdown_in_bodies() {
        let input = r#"<spec>
  <objective>
## Sub heading
- list item
- another

```rust
let x: Vec<T> = vec![1, 2, 3];
if a < b { println!("ok"); }
```

URL: https://example.com/<path>?a=1&b=2
  </objective>
  <functional_requirements>Some **bold** and _italic_ text.</functional_requirements>
  <non_functional_requirements>Latency < 100ms.</non_functional_requirements>
  <constraints>Use `file:line` references.</constraints>
  <guidelines>See RFC 2026-09-01.</guidelines>
  <in_scope>Node abc-123 and spec 42.</in_scope>
  <out_of_scope>Nothing.</out_of_scope>
</spec>"#;
        let result = parse_spec_description(input).unwrap();
        match result {
            ParsedSpecDescription::Tagged(parsed) => {
                let obj = parsed.section_body(SpecSection::Objective).unwrap();
                assert!(obj.contains("## Sub heading"));
                assert!(obj.contains("let x: Vec<T>"));
                assert!(obj.contains("a < b"));
                assert!(obj.contains("https://example.com/"));
            }
            _ => panic!("expected tagged"),
        }
    }

    #[test]
    fn parse_missing_tag_names_it() {
        let input = r#"<spec>
  <objective>Ship.</objective>
  <functional_requirements>Does thing.</functional_requirements>
  <non_functional_requirements>Fast.</non_functional_requirements>
  <constraints>None.</constraints>
  <guidelines>Style.</guidelines>
  <in_scope>This.</in_scope>
</spec>"#;
        let err = parse_spec_description(input).unwrap_err();
        match err {
            SpecParseError::MissingTag(tag) => assert_eq!(tag, "out_of_scope"),
            other => panic!("expected MissingTag, got: {}", other),
        }
    }

    #[test]
    fn parse_misspelled_tag_names_supplied_tag() {
        let input = r#"<spec>
  <objective>Ship.</objective>
  <functional_requirement>Does thing.</functional_requirement>
  <non_functional_requirements>Fast.</non_functional_requirements>
  <constraints>None.</constraints>
  <guidelines>Style.</guidelines>
  <in_scope>This.</in_scope>
  <out_of_scope>Nothing.</out_of_scope>
</spec>"#;
        let err = parse_spec_description(input).unwrap_err();
        match err {
            SpecParseError::UnexpectedTag(tag) => assert_eq!(tag, "functional_requirement"),
            other => panic!("expected UnexpectedTag, got: {}", other),
        }
    }

    #[test]
    fn parse_duplicate_tag_names_it() {
        let input = r#"<spec>
  <objective>Ship.</objective>
  <objective>Ship again.</objective>
  <functional_requirements>Does thing.</functional_requirements>
  <non_functional_requirements>Fast.</non_functional_requirements>
  <constraints>None.</constraints>
  <guidelines>Style.</guidelines>
  <in_scope>This.</in_scope>
  <out_of_scope>Nothing.</out_of_scope>
</spec>"#;
        let err = parse_spec_description(input).unwrap_err();
        match err {
            SpecParseError::DuplicateTag(tag) => assert_eq!(tag, "objective"),
            other => panic!("expected DuplicateTag, got: {}", other),
        }
    }

    #[test]
    fn parse_malformed_closing_tag_names_it() {
        let input = r#"<spec>
  <objective>Ship.</objective>
  <functional_requirements>Does thing.</functional_requirements>
  <non_functional_requirements>Fast.</non_functional_requirements>
  <constraints>None.</constraints>
  <guidelines>Style.</guidelines>
  <in_scope>This.</in_scope>
  <out_of_scope>Nothing.</out_of_scopex>
</spec>"#;
        let err = parse_spec_description(input).unwrap_err();
        match err {
            SpecParseError::MalformedClosingTag(tag) => assert_eq!(tag, "out_of_scope"),
            other => panic!("expected MalformedClosingTag, got: {}", other),
        }
    }

    #[test]
    fn parse_missing_spec_wrapper() {
        let err = parse_spec_description("just some text").unwrap_err();
        match err {
            SpecParseError::MissingSpecWrapper(_) => {}
            other => panic!("expected MissingSpecWrapper, got: {}", other),
        }
    }

    #[test]
    fn parse_missing_close_spec() {
        let input = "<spec><objective>Ship.</objective></specx>";
        let err = parse_spec_description(input).unwrap_err();
        match err {
            SpecParseError::MissingSpecWrapper(_) => {}
            other => panic!("expected MissingSpecWrapper, got: {}", other),
        }
    }

    #[test]
    fn legacy_body_classified_as_legacy() {
        let input = r#"## Objective
Ship the feature.

## Functional Requirements
Does the thing.

## Non-Functional Requirements
Is fast.

## Constraints
None extra.

## Guidelines
Follow house style.

## In Scope
This change.

## Out of Scope
Everything else."#;
        let result = parse_spec_description(input).unwrap();
        match result {
            ParsedSpecDescription::Legacy(_) => {}
            _ => panic!("expected legacy"),
        }
    }

    #[test]
    fn legacy_conversion_preserves_sentinels() {
        let input = r#"## Objective
Ship the feature at src/main.rs:42 on 2026-09-01.

## Functional Requirements
- Node abc-123 does thing
- Value is 42

## Non-Functional Requirements
Latency < 100ms

## Constraints
Use `file:line` references

## Guidelines
Follow house style

## In Scope
This change

## Out of Scope
Everything else"#;
        let converted = convert_legacy_spec_description(input).unwrap();
        assert!(converted.contains("<spec>"));
        assert!(converted.contains("</spec>"));
        assert!(converted.contains("<objective>"));
        assert!(converted.contains("src/main.rs:42"));
        assert!(converted.contains("2026-09-01"));
        assert!(converted.contains("abc-123"));
        assert!(converted.contains("42"));
        assert!(converted.contains("Latency < 100ms"));
        assert!(converted.contains("`file:line`"));

        let reparsed = parse_spec_description(&converted).unwrap();
        match reparsed {
            ParsedSpecDescription::Tagged(parsed) => {
                assert_eq!(parsed.sections.len(), 7);
                let obj = parsed.section_body(SpecSection::Objective).unwrap();
                assert!(obj.contains("src/main.rs:42"));
                assert!(obj.contains("2026-09-01"));
            }
            _ => panic!("expected tagged after conversion"),
        }
    }

    #[test]
    fn legacy_conversion_is_idempotent() {
        let input = r#"## Objective
Ship.

## Functional Requirements
Does thing.

## Non-Functional Requirements
Fast.

## Constraints
None.

## Guidelines
Style.

## In Scope
This.

## Out of Scope
Nothing."#;
        let first = convert_legacy_spec_description(input).unwrap();
        let second = convert_legacy_spec_description(&first).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn legacy_conversion_fails_on_duplicate_heading() {
        let input = r#"## Objective
Ship.

## Objective
Ship again.

## Functional Requirements
Does thing.

## Non-Functional Requirements
Fast.

## Constraints
None.

## Guidelines
Style.

## In Scope
This.

## Out of Scope
Nothing."#;
        let err = convert_legacy_spec_description(input).unwrap_err();
        match err {
            SpecParseError::LegacyConversionFailed(msg) => {
                assert!(msg.contains("duplicate"));
                assert!(msg.contains("objective"));
            }
            other => panic!("expected LegacyConversionFailed, got: {}", other),
        }
    }

    #[test]
    fn legacy_conversion_fails_on_missing_heading() {
        let input = r#"## Objective
Ship.

## Functional Requirements
Does thing.

## Non-Functional Requirements
Fast.

## Constraints
None.

## Guidelines
Style.

## In Scope
This."#;
        let err = convert_legacy_spec_description(input).unwrap_err();
        match err {
            SpecParseError::LegacyConversionFailed(msg) => {
                assert!(msg.contains("missing"));
                assert!(msg.contains("out_of_scope"));
            }
            other => panic!("expected LegacyConversionFailed, got: {}", other),
        }
    }

    #[test]
    fn old_aliases_no_longer_pass_strict_validation() {
        let input = r#"Requerimientos funcionales:
- Crear el flujo

Requerimientos no funcionales:
- Mantener compatibilidad

Objetivo:
- Encadenar agentes

Qué respetar:
- No romper tools actuales

Lineamientos:
- Reusar el daemon

Qué sí:
- Backend y MCP

Qué no:
- TUI nueva"#;
        let result = validate_spec_description_template(input);
        assert!(result.is_err());
    }

    #[test]
    fn extract_section_from_tagged() {
        let input = valid_tagged_spec();
        let result = extract_spec_section(&input, SpecSection::Constraints).unwrap();
        match result {
            SpecSectionResult::Tagged { content } => {
                assert_eq!(content, "None extra.");
            }
            _ => panic!("expected tagged"),
        }
    }

    #[test]
    fn extract_section_from_legacy() {
        let input = r#"## Objective
Ship.

## Functional Requirements
Does thing.

## Non-Functional Requirements
Fast.

## Constraints
None extra.

## Guidelines
Style.

## In Scope
This.

## Out of Scope
Nothing."#;
        let result = extract_spec_section(input, SpecSection::Constraints).unwrap();
        match result {
            SpecSectionResult::Legacy { content } => {
                assert_eq!(content, "None extra.");
            }
            _ => panic!("expected legacy"),
        }
    }

    #[test]
    fn validate_rejects_legacy_format() {
        let input = r#"## Objective
Ship.

## Functional Requirements
Does thing.

## Non-Functional Requirements
Fast.

## Constraints
None.

## Guidelines
Style.

## In Scope
This.

## Out of Scope
Nothing."#;
        let err = validate_spec_description_template(input).unwrap_err();
        assert!(err.contains("legacy"));
    }

    #[test]
    fn validate_accepts_tagged() {
        let input = valid_tagged_spec();
        assert!(validate_spec_description_template(&input).is_ok());
    }

    #[test]
    fn section_from_tag_display() {
        assert_eq!(SpecSection::Objective.tag(), "objective");
        assert_eq!(
            SpecSection::FunctionalRequirements.tag(),
            "functional_requirements"
        );
        assert_eq!(format!("{}", SpecSection::Constraints), "constraints");
    }
}
