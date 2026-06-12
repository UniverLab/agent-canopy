use anyhow::Result;
use std::path::Path;

pub(crate) fn upsert_json_key(
    path: &Path,
    keys: &[&str],
    value: &serde_json::Value,
) -> Result<bool> {
    let mut root: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let clean = strip_jsonc_comments(&content);
        serde_json::from_str(&clean).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let mut current = &mut root;
    for &key in &keys[..keys.len() - 1] {
        if !current.get(key).is_some_and(|v| v.is_object()) {
            current[key] = serde_json::json!({});
        }
        current = &mut current[key];
    }

    let leaf = keys[keys.len() - 1];
    current[leaf] = value.clone();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&root)? + "\n")?;
    Ok(true)
}

/// Remove a `[section.entry_key]` TOML block from string content.
pub(crate) fn remove_toml_key_section_str(content: &str, table_header: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_target = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed == table_header {
                in_target = true;
                continue;
            }
            in_target = false;
        }
        if !in_target {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Upsert a TOML config key for platforms like Codex that use config.toml.
///
/// Writes `[{section}.{entry_key}]` with the fields from `value` (a JSON object).
/// Example output: `[mcp_servers.canopy]\nurl = "http://localhost:7755/mcp"\n`
pub(crate) fn upsert_toml_key(
    path: &Path,
    section: &str,
    entry_key: &str,
    value: &serde_json::Value,
) -> Result<bool> {
    let table_header = format!("[{section}.{entry_key}]");

    let content = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    // Remove existing [section.entry_key] (if any) so we always write a fresh one
    let mut base = remove_toml_key_section_str(&content, &table_header);

    // Remove conflicting [[section]] array-of-tables entries (e.g. from old format)
    base = remove_conflicting_toml_arrays(&base, section);

    // Build the TOML fragment from the JSON value
    let mut fragment = format!("\n{table_header}\n");
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            match v {
                serde_json::Value::String(s) => {
                    fragment.push_str(&format!("{k} = \"{s}\"\n"));
                }
                serde_json::Value::Bool(b) => {
                    fragment.push_str(&format!("{k} = {b}\n"));
                }
                serde_json::Value::Number(n) => {
                    fragment.push_str(&format!("{k} = {n}\n"));
                }
                _ => {
                    let toml_val: toml::Value = serde_json::from_value(v.clone())?;
                    fragment.push_str(&format!("{k} = {toml_val}\n"));
                }
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut out = base;
    out.push_str(&fragment);
    std::fs::write(path, out)?;
    Ok(true)
}

pub(crate) fn remove_json_key(path: &Path, parent_key: &str, key: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path)?;
    let clean = strip_jsonc_comments(&content);
    let mut root: serde_json::Value = serde_json::from_str(&clean).unwrap_or(serde_json::json!({}));

    let Some(parent) = root.get_mut(parent_key).and_then(|v| v.as_object_mut()) else {
        return Ok(false);
    };

    if parent.remove(key).is_some() {
        std::fs::write(path, serde_json::to_string_pretty(&root)? + "\n")?;
        return Ok(true);
    }
    Ok(false)
}

/// Upsert a TOML entry using `[[section]]` array-of-tables format (e.g. mistral).
///
/// Each entry is identified by `name = "entry_key"` within the array.
/// Example: `[[mcp_servers]]\nname = "fetch"\ncommand = "uvx"\n`
pub(crate) fn upsert_toml_array(
    path: &Path,
    section: &str,
    entry_key: &str,
    value: &serde_json::Value,
) -> Result<bool> {
    let array_header = format!("[[{section}]]");
    let name_line = format!("name = \"{entry_key}\"");

    let content = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    // Remove existing entry (if any) so we always write a fresh one
    let mut base = if content.contains(&name_line) {
        remove_toml_array_entry_str(&content, &array_header, &name_line)
    } else {
        content
    };

    // Remove any `{section} = []` scalar that conflicts with [[section]] array-of-tables
    let scalar_prefix = format!("{section} =");
    let scalar_prefix2 = format!("{section}=");
    base = base
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with(&scalar_prefix) && !t.starts_with(&scalar_prefix2)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !base.is_empty() && !base.ends_with('\n') {
        base.push('\n');
    }

    // Remove conflicting [section.xxx] key-table entries (left over from old format)
    base = remove_conflicting_toml_tables(&base, section);

    // Remove stray [[section]] headers not followed by a name = "..." line
    base = remove_stray_toml_array_headers(&base, &array_header);

    // Build the TOML fragment
    let mut fragment = format!("\n{array_header}\nname = \"{entry_key}\"\n");
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            match v {
                serde_json::Value::String(s) => {
                    fragment.push_str(&format!("{k} = \"{s}\"\n"));
                }
                serde_json::Value::Bool(b) => {
                    fragment.push_str(&format!("{k} = {b}\n"));
                }
                serde_json::Value::Number(n) => {
                    fragment.push_str(&format!("{k} = {n}\n"));
                }
                _ => {
                    let toml_val: toml::Value = serde_json::from_value(v.clone())?;
                    fragment.push_str(&format!("{k} = {toml_val}\n"));
                }
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut out = base;
    out.push_str(&fragment);
    std::fs::write(path, out)?;
    Ok(true)
}

/// Remove a `[[section]]` array entry from string content.
/// The entry is identified by the line `name = "entry_key"` immediately following the header.
pub(crate) fn remove_toml_array_entry_str(
    content: &str,
    array_header: &str,
    name_line: &str,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_target = false;
    let mut pending_header: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == array_header {
            // A new [[section]] header ends any previous target block
            if in_target {
                in_target = false;
            }
            pending_header = Some(line.to_string());
            continue;
        }

        if let Some(ref header) = pending_header {
            if trimmed == name_line {
                in_target = true;
                pending_header = None;
                continue;
            }
            out.push_str(header);
            out.push('\n');
            pending_header = None;
        }

        if in_target {
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_target = false;
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    // Flush any buffered header not yet written
    if let Some(header) = pending_header {
        out.push_str(&header);
        out.push('\n');
    }

    out
}

/// Remove `[[section]]` header lines that are not followed by a `name = "..."` line.
/// Fixes stray empty headers accumulated from previous corrupt writes.
fn remove_stray_toml_array_headers(content: &str, array_header: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == array_header {
            let next_non_empty = (i + 1..lines.len()).find(|&j| !lines[j].trim().is_empty());
            let is_valid = next_non_empty
                .map(|j| lines[j].trim().starts_with("name ="))
                .unwrap_or(false);
            if is_valid {
                out.push_str(lines[i]);
                out.push('\n');
            }
            // else: stray header without content — drop it
        } else {
            out.push_str(lines[i]);
            out.push('\n');
        }
        i += 1;
    }
    out
}

/// Remove all `[section.xxx]` key-table entries that conflict with `[[section]]` format.
/// This handles migration from the older `[mcp_servers.canopy]` format to `[[mcp_servers]]`.
fn remove_conflicting_toml_tables(content: &str, section: &str) -> String {
    let prefix = format!("[{section}.");
    let mut out = String::with_capacity(content.len());
    let mut in_conflict = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect a conflicting [section.xxx] header (single-bracket, not [[...]])
        if trimmed.starts_with(&prefix)
            && trimmed.ends_with(']')
            && !trimmed.starts_with(&format!("[[{section}."))
        {
            in_conflict = true;
            continue;
        }

        // Any new section header ends the conflicting block
        if in_conflict && trimmed.starts_with('[') {
            in_conflict = false;
        }

        if !in_conflict {
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

/// Remove all `[[section]]` array-of-tables blocks for a given section.
/// Counterpart to `remove_conflicting_toml_tables`: cleans up array entries
/// when switching a platform to `[section.name]` key-table format.
fn remove_conflicting_toml_arrays(content: &str, section: &str) -> String {
    let header = format!("[[{section}]]");
    let mut out = String::with_capacity(content.len());
    let mut in_array = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == header {
            in_array = true;
            continue;
        }

        // Any new section header ends the array block
        if in_array && trimmed.starts_with('[') {
            in_array = false;
        }

        if !in_array {
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

pub fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;

    while let Some(c) = chars.next() {
        if in_string {
            in_string = handle_string_char(c, &mut chars, &mut out);
        } else {
            match c {
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                '/' => handle_comment_or_slash(&mut chars, &mut out),
                _ => out.push(c),
            }
        }
    }
    out
}

/// Handle a character inside a JSON string. Returns `false` when the string ends.
fn handle_string_char(
    c: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    out: &mut String,
) -> bool {
    out.push(c);
    if c == '\\' {
        if let Some(&next) = chars.peek() {
            out.push(next);
            chars.next();
        }
        return true; // still in string
    }
    c != '"' // false = string ended
}

/// Handle `/` outside a string: skip `//` line comment, `/* */` block comment, or emit `/`.
fn handle_comment_or_slash(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String) {
    match chars.peek() {
        Some('/') => {
            for ch in chars.by_ref() {
                if ch == '\n' {
                    out.push('\n');
                    break;
                }
            }
        }
        Some('*') => {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        }
        _ => out.push('/'),
    }
}
