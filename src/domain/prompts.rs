//! Prompt presets (P1) — file-backed, user-editable prompt text for builtin
//! agent blueprints. Seeded under `~/.canopy/prompts/` at daemon startup
//! (never overwriting an existing file — user edits are sacred) and resolved
//! at agent SPAWN time, so an edit takes effect on the very next run without
//! a recompile.
//!
//! The engine never special-cases a preset name — "implementer"/"reviewer"/
//! "resilience" are seed data here, exactly like `builtin_blueprint_specs()`
//! in `blueprints.rs`. The hardcoded constants below are both the seed
//! content written to disk *and* the fallback used when a preset file goes
//! missing or unreadable, so the two can never drift apart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The tag vocabulary every builtin preset below is built from, mirroring
/// the `# [NAME]: Description` header + XML-tag convention Canopy's own
/// prompt builder emits (see `tui::app::dialog::prompt`'s
/// `append_instruction_section` and its `push_xml_item`/`build_xml_block`
/// helpers) — a header line, an outer tag, and one indented `<item_tag>`
/// block per item. Defined once here, and referenced by all three presets
/// below, so the four sections every preset shares — role, procedure, what
/// it may use, what it must never do — can't drift into three
/// slightly-different dialects.
struct Section {
    header: &'static str,
    tag: &'static str,
    item_tag: &'static str,
}

const ROLE: Section = Section {
    header: "# [ROLE]: Who You Are\n",
    tag: "role",
    item_tag: "",
};
const PROCEDURE: Section = Section {
    header: "# [INSTRUCTIONS]: Execution Logic\n",
    tag: "instruction_set",
    item_tag: "instruction",
};
const ALLOWED: Section = Section {
    header: "# [ALLOWED]: What You May Use\n",
    tag: "allowed",
    item_tag: "item",
};
const PROHIBITED: Section = Section {
    header: "# [PROHIBITED]: Never Do This\n",
    tag: "prohibited",
    item_tag: "rule",
};

/// Render a single-block section: a header line followed by `content`
/// wrapped directly in `section.tag`, with no per-item indentation. Used
/// only for [`ROLE`], which is always one short paragraph rather than a
/// list.
fn wrapped_section(section: &Section, content: &str) -> String {
    format!(
        "{}<{}>\n{}\n</{}>\n\n",
        section.header, section.tag, content, section.tag
    )
}

/// Render a list section: a header line, an outer `section.tag`, and one
/// `section.item_tag` block per entry in `items` — the same
/// header-then-indented-items shape [`push_xml_item`]/[`build_xml_block`] in
/// `tui::app::dialog::prompt` produce. Used for every preset section except
/// [`ROLE`].
fn itemized_section(section: &Section, items: &[&str]) -> String {
    let mut out = String::new();
    out.push_str(section.header);
    out.push_str(&format!("<{}>\n", section.tag));
    for item in items {
        out.push_str(&format!("  <{}>\n", section.item_tag));
        for line in item.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str(&format!("  </{}>\n\n", section.item_tag));
    }
    out.push_str(&format!("</{}>\n\n", section.tag));
    out
}

fn implementer_preset() -> String {
    let mut out = wrapped_section(
        &ROLE,
        "You are a careful Rust implementer working in this repository.",
    );
    out.push_str(&itemized_section(&PROCEDURE, &[
        "FIRST, verify the premise of the spec against the actual code: read the relevant files before changing anything. If the premise is wrong — it describes something that isn't true of the current code, or asks for a change that doesn't actually apply — stop and report exactly why instead of forcing a change to fit a wrong premise.",
        "Otherwise, implement exactly this spec, nothing more and nothing less:\n\n{{spec_content}}",
        "Previous feedback (if any): {{previous_feedback}}\nIf it reads \"(none)\", this is a fresh implementation. Otherwise, address every point of the feedback as part of your changes.",
        "Before reporting, run the checks relevant to what you changed (formatter, linter, tests) locally — never assume they pass without running them.",
        "If you are genuinely blocked — missing credentials, an unresolvable conflict, a decision only a human can make — make the final line of your report start with \"BLOCKER:\" followed by a one-paragraph explanation of exactly why.",
    ]));
    out.push_str(&itemized_section(&ALLOWED, &[
        "Reading any file in the repository, and running local checks (formatter, linter, tests) to verify your work.",
    ]));
    out.push_str(&itemized_section(&PROHIBITED, &[
        "Committing your changes under any circumstances — a separate reviewer step reviews your diff and commits it.",
    ]));
    out
}

fn reviewer_preset() -> String {
    let mut out = wrapped_section(&ROLE, "You are the reviewer and committer for this loop.");
    out.push_str(&itemized_section(&PROCEDURE, &[
        "Review the current diff strictly against this spec — nothing else:\n\n{{spec_content}}",
        "If the diff correctly and completely implements the spec, make EXACTLY ONE commit covering ONLY this spec's work. Use a concise, descriptive commit message with NO trailers of any kind (no Co-Authored-By, no issue references, no generated-by footers).",
        "If the worktree has no changes at all, FAIL and say so explicitly — do not treat \"nothing changed\" as success.",
        "If the diff is wrong, incomplete, or diverges from the spec, FAIL with a specific, actionable list of what's wrong so the implementer can address it directly — vague feedback is not acceptable.",
    ]));
    out.push_str(&itemized_section(&ALLOWED, &[
        "Reading the current diff and this spec, and making exactly one commit when the spec is fully satisfied.",
    ]));
    out.push_str(&itemized_section(
        &PROHIBITED,
        &[
            "Pushing, under any circumstances.",
            "Committing an empty diff.",
        ],
    ));
    out
}

fn resilience_preset() -> String {
    let mut out = wrapped_section(
        &ROLE,
        "You are the on-call medic for this loop. A node just failed or reported a blocker. Your only job is to diagnose why and route to the right next step — you do not fix the underlying work yourself.",
    );
    out.push_str(&itemized_section(&PROCEDURE, &[
        "Read the failure/blocker context below (`{{previous_feedback}}`) and pick exactly ONE diagnosis: QUOTA, GLITCH, or OTHER.",
        "QUOTA — the failure is a rate limit, quota exhaustion, or \"try again later\" from the platform/API itself. Action: call `loop_schedule_autorun` with `loop_id` set to this loop's ID and `quota_reset_message` set to the ENTIRE failure text below, copied verbatim — do NOT extract, truncate, reformat, or translate any part of it; the engine parses the raw text itself. Then report FAIL.",
        "GLITCH — the failure is a transient infrastructure hiccup (network blip, spurious timeout, a flaky check unrelated to the spec) with no sign of a real defect. Action: report GLITCH and recommend a plain retry of the same node.",
        "OTHER — the failure reflects a genuine problem with the work itself (wrong code, an unmet spec, a missing dependency, a decision only a human can make). Action: report OTHER with a precise, actionable explanation of what's wrong so a human or the next agent can address it.",
        "State your diagnosis and its one action clearly in your report; do not hedge between diagnoses.",
    ]));
    out.push_str(&itemized_section(&ALLOWED, &[
        "Reading files, logs, git history, and process/job status.",
        "Checking scheduling/queue state (e.g. `git status`, `git log`, `ps`, reading CI/log output).",
        "Reporting your diagnosis and recommended action via loop_complete_node / loop_report_blocker.",
    ]));
    out.push_str(&itemized_section(
        &PROHIBITED,
        &[
            "Writing code, or editing any file or config.",
            "Building or testing the project.",
            "Committing anything.",
            "Attempting to verify or finish the spec's actual work — that is not your job here.",
        ],
    ));
    out
}

/// `(name, content)` for every builtin prompt preset — the single source
/// both [`seed_builtin_prompt_presets`] writes to disk and
/// [`resolve_prompt_preset`] falls back to. Content is built (not a
/// hardcoded `&'static str`) so all three presets render through the same
/// [`Section`] vocabulary above.
pub fn builtin_prompt_preset_specs() -> Vec<(&'static str, String)> {
    vec![
        ("implementer", implementer_preset()),
        ("reviewer", reviewer_preset()),
        ("resilience", resilience_preset()),
    ]
}

/// `<canopy_dir>/prompts/` — where preset files are seeded to and resolved
/// from.
pub fn prompts_dir(canopy_dir: &Path) -> PathBuf {
    canopy_dir.join("prompts")
}

/// Canopy's home directory, honoring `CANOPY_HOME_OVERRIDE` the same way
/// `Cli::strategy()` does (see `domain::models::Cli::strategy`'s doc comment
/// for why tests swap this env var rather than the real `HOME`). Unset in
/// production, where this is just `~/.canopy`.
pub fn canopy_dir() -> PathBuf {
    std::env::var_os("CANOPY_HOME_OVERRIDE")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".canopy")
}

/// Seed the builtin prompt presets under `<canopy_dir>/prompts/` if missing.
/// Idempotent and reseed-if-missing, mirroring
/// `Database::seed_builtin_blueprints`: a file already present (by name) is
/// left completely untouched — user edits are never overwritten — so this is
/// safe to call on every daemon startup and every `canopy prompts` CLI
/// invocation.
pub fn seed_builtin_prompt_presets(canopy_dir: &Path) -> std::io::Result<()> {
    let dir = prompts_dir(canopy_dir);
    std::fs::create_dir_all(&dir)?;
    for (name, content) in builtin_prompt_preset_specs() {
        let path = dir.join(format!("{name}.md"));
        if path.exists() {
            continue;
        }
        std::fs::write(&path, content)?;
    }
    Ok(())
}

/// Resolve a named preset at agent spawn time: read `<prompts_dir>/<name>.md`.
/// If the file is missing or unreadable, fall back to the hardcoded seed
/// constant for that name (logging a WARN naming the missing file) — or an
/// empty string if `name` isn't a builtin either, since a custom preset with
/// no file has nothing to fall back to.
///
/// Takes `prompts_dir` directly (rather than reading `canopy_dir()` itself)
/// so callers and tests can point it at any directory without touching
/// process-wide env state — see [`canopy_dir`] to derive the production path.
pub fn resolve_prompt_preset(prompts_dir: &Path, name: &str) -> String {
    let path = prompts_dir.join(format!("{name}.md"));
    match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            let fallback = builtin_prompt_preset_specs()
                .into_iter()
                .find(|(preset_name, _)| *preset_name == name)
                .map(|(_, content)| content)
                .unwrap_or_default();
            tracing::warn!(
                preset = name,
                path = %path.display(),
                error = %error,
                "prompt preset file missing or unreadable; falling back to the builtin seed constant"
            );
            fallback
        }
    }
}

/// A `{{name}}` placeholder in a preset's raw content with no bound value
/// supplied by the caller. [`render_preset`] refuses to emit rather than let
/// a literal, unfilled placeholder reach whatever the preset is composed
/// into — an unfilled placeholder is a refusal, not a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingPresetBinding {
    pub preset: String,
    pub binding: String,
}

impl std::fmt::Display for MissingPresetBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "preset '{}' requires '{{{{{}}}}}', which was not supplied",
            self.preset, self.binding
        )
    }
}

impl std::error::Error for MissingPresetBinding {}

/// One lexical piece of a template, as recognised by [`scan_template`] — the
/// single definition of what a `{{...}}` marker and its `\{{...}}` escape
/// look like.
enum TemplatePiece<'a> {
    /// Copy verbatim: ordinary characters, and any backslash that is not
    /// immediately followed by a complete `{{...}}` pair.
    Literal(&'a str),
    /// `\{{inner}}` — renders as the literal `{{inner}}`, never a binding
    /// site, whatever `inner` names.
    Escaped { inner: &'a str },
    /// An unescaped `{{inner}}` binding site; `inner` is the exact inner
    /// bytes, surrounding whitespace and all.
    Marker { inner: &'a str },
}

/// Walk `template` left to right, handing each piece to `visit`.
///
/// This is the single owner of escape handling: it alone decides where a
/// marker begins, that a `\` immediately before `{{...}}` escapes it, and
/// that any other `\` is ordinary text. [`render_template`],
/// [`placeholder_names`] and [`unbindable_placeholders`] are all written
/// against it, so the escape rule can never drift between them.
fn scan_template(template: &str, mut visit: impl FnMut(TemplatePiece<'_>)) {
    let mut i = 0;
    while i < template.len() {
        let rest = &template[i..];
        if let Some(after) = rest.strip_prefix("\\{{") {
            if let Some(end) = after.find("}}") {
                visit(TemplatePiece::Escaped {
                    inner: &after[..end],
                });
                i += "\\{{".len() + end + "}}".len();
                continue;
            }
            // No closing `}}`: the backslash is ordinary text — fall through.
        } else if let Some(after) = rest.strip_prefix("{{") {
            match after.find("}}") {
                Some(end) => {
                    visit(TemplatePiece::Marker {
                        inner: &after[..end],
                    });
                    i += "{{".len() + end + "}}".len();
                }
                None => {
                    // Unterminated marker: the remainder is all literal text.
                    visit(TemplatePiece::Literal(rest));
                    return;
                }
            }
            continue;
        }
        let ch = rest.chars().next().expect("non-empty str has a first char");
        visit(TemplatePiece::Literal(&rest[..ch.len_utf8()]));
        i += ch.len_utf8();
    }
}

/// Render `template` in a single left-to-right pass, resolving each
/// unescaped `{{name}}` marker through `resolve` and emitting escaped
/// `\{{name}}` markers as literal text.
///
/// Contract (CP4):
/// - `\{{name}}` (a backslash immediately before the opening braces, with a
///   closing `}}` ahead) emits `{{name}}` — the backslash removed, exactly
///   once — and never invokes `resolve`, even when `name` is a real binding.
/// - An unescaped `{{raw}}` invokes `resolve(raw)` once with the exact inner
///   bytes; a `Some` replacement is emitted, a `None` leaves the original
///   marker untouched so validation can report it.
/// - A backslash not immediately followed by `{{` is ordinary text and is
///   copied unchanged — prompts full of paths (`C:\temp`) and regexes
///   (`\d+`) must survive byte-for-byte.
/// - The output is never rescanned: a replacement containing `{{...}}` is
///   data, not a new marker.
///
/// Escape recognition is delegated entirely to [`scan_template`], the one
/// place that defines it.
pub fn render_template(template: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len());
    scan_template(template, |piece| match piece {
        TemplatePiece::Literal(text) => out.push_str(text),
        TemplatePiece::Escaped { inner } => {
            out.push_str("{{");
            out.push_str(inner);
            out.push_str("}}");
        }
        TemplatePiece::Marker { inner } => match resolve(inner) {
            Some(replacement) => out.push_str(&replacement),
            None => {
                out.push_str("{{");
                out.push_str(inner);
                out.push_str("}}");
            }
        },
    });
    out
}

/// Every unescaped `{{name}}` placeholder in `content`, first-seen order,
/// deduplicated. Escaped `\{{name}}` pairs are literal prose, not binding
/// sites, and are skipped. Names are trimmed for the dedup key, matching the
/// historical behaviour of this helper.
fn placeholder_names(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    scan_template(content, |piece| {
        if let TemplatePiece::Marker { inner } = piece {
            let name = inner.trim().to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    });
    names
}

/// The `{{name}}` placeholders in `template` that `supported` cannot bind.
///
/// A non-empty result means rendering `template` would leave a literal
/// `{{...}}` in the output: the caller must refuse to emit it — exactly as
/// [`render_preset`] refuses — rather than pass an unfilled template on to
/// whatever composes it into a final prompt, where the marker would be read
/// as an instruction.
///
/// Checked against the *template*, never the rendered output: a bound value
/// (a spec body, prior feedback) may legitimately contain `{{...}}` text of
/// its own, and that is data, not an unfilled placeholder.
///
/// A marker counts as unbindable when its inner text either names nothing in
/// `supported` **or** carries surrounding whitespace (`{{ spec_content }}`):
/// [`render_template`] resolves the *exact* inner bytes, so a padded marker
/// finds no binding and reaches the agent as a literal `{{…}}` just as surely
/// as one naming a binding that does not exist.
///
/// Escaped `\{{...}}` pairs are skipped via [`scan_template`] — the shared
/// definition of the escape — whatever their inner text, even a real binding
/// name.
pub fn unbindable_placeholders(template: &str, supported: &[&str]) -> Vec<String> {
    let mut leftovers: Vec<String> = Vec::new();
    scan_template(template, |piece| {
        let TemplatePiece::Marker { inner } = piece else {
            return;
        };
        let name = inner.trim();
        if name.is_empty() {
            return;
        }
        let bindable = inner == name && supported.contains(&name);
        if !bindable && !leftovers.iter().any(|seen| seen == name) {
            leftovers.push(name.to_string());
        }
    });
    leftovers
}

/// Render a preset's raw content (as returned by [`resolve_prompt_preset`]
/// or read straight from a picker entry) by substituting every `{{name}}`
/// placeholder with `bindings[name]`. A preset is scoped to its own
/// invocation: this only ever touches `content`, never anything composed
/// around it, and returns exactly that preset's body.
///
/// Refuses — returns `Err` naming the preset and the missing binding —
/// rather than emit a literal `{{...}}` sequence when a required binding
/// isn't supplied, so a caller with no bindings to offer (e.g. the TUI
/// composing an ad hoc prompt) never leaks an unfilled template into its
/// output.
pub fn render_preset(
    preset: &str,
    content: &str,
    bindings: &HashMap<String, String>,
) -> Result<String, MissingPresetBinding> {
    for name in placeholder_names(content) {
        if !bindings.contains_key(&name) {
            return Err(MissingPresetBinding {
                preset: preset.to_string(),
                binding: name,
            });
        }
    }

    Ok(render_template(content, |raw| bindings.get(raw).cloned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn seed_builtin_prompt_presets_writes_missing_files_but_leaves_edited_ones_alone() {
        let dir = tempdir().unwrap();
        let canopy_dir = dir.path();

        seed_builtin_prompt_presets(canopy_dir).unwrap();
        let implementer_path = prompts_dir(canopy_dir).join("implementer.md");
        assert_eq!(
            std::fs::read_to_string(&implementer_path).unwrap(),
            implementer_preset()
        );

        // Simulate a user edit, then reseed (as a second daemon startup would).
        std::fs::write(&implementer_path, "my custom implementer prompt").unwrap();
        seed_builtin_prompt_presets(canopy_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(&implementer_path).unwrap(),
            "my custom implementer prompt"
        );
    }

    #[test]
    fn seed_builtin_prompt_presets_creates_all_three_builtins() {
        let dir = tempdir().unwrap();
        seed_builtin_prompt_presets(dir.path()).unwrap();

        for (name, _) in builtin_prompt_preset_specs() {
            assert!(prompts_dir(dir.path()).join(format!("{name}.md")).exists());
        }
    }

    #[test]
    fn resolve_prompt_preset_reads_the_file_when_present() {
        let dir = tempdir().unwrap();
        let prompts = prompts_dir(dir.path());
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("implementer.md"), "edited content").unwrap();

        assert_eq!(
            resolve_prompt_preset(&prompts, "implementer"),
            "edited content"
        );
    }

    #[test]
    fn resolve_prompt_preset_falls_back_to_the_hardcoded_constant_when_file_missing() {
        let dir = tempdir().unwrap();
        let prompts = prompts_dir(dir.path()); // never seeded/created

        assert_eq!(
            resolve_prompt_preset(&prompts, "reviewer"),
            reviewer_preset()
        );
    }

    #[test]
    fn resolve_prompt_preset_returns_empty_for_an_unknown_custom_preset_with_no_file() {
        let dir = tempdir().unwrap();
        let prompts = prompts_dir(dir.path());

        assert_eq!(resolve_prompt_preset(&prompts, "totally-custom"), "");
    }

    #[test]
    fn builtin_prompt_preset_specs_returns_non_empty() {
        let specs = builtin_prompt_preset_specs();
        assert!(!specs.is_empty());
        assert_eq!(specs.len(), 3);
    }

    #[test]
    fn builtin_prompt_preset_specs_has_expected_names() {
        let names: Vec<&str> = builtin_prompt_preset_specs()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(names, vec!["implementer", "reviewer", "resilience"]);
    }

    #[test]
    fn builtin_prompt_preset_specs_have_non_empty_content() {
        for (name, content) in builtin_prompt_preset_specs() {
            assert!(
                !content.is_empty(),
                "preset '{name}' must have non-empty content"
            );
        }
    }

    #[test]
    fn implementer_preset_contains_spec_content_placeholder() {
        let specs = builtin_prompt_preset_specs();
        let (_, content) = specs.iter().find(|(n, _)| *n == "implementer").unwrap();
        assert!(
            content.contains("{{spec_content}}"),
            "implementer preset must contain {{{{spec_content}}}} placeholder"
        );
    }

    #[test]
    fn implementer_preset_contains_previous_feedback_placeholder() {
        let specs = builtin_prompt_preset_specs();
        let (_, content) = specs.iter().find(|(n, _)| *n == "implementer").unwrap();
        assert!(
            content.contains("{{previous_feedback}}"),
            "implementer preset must contain {{{{previous_feedback}}}} placeholder"
        );
    }

    #[test]
    fn reviewer_preset_contains_spec_content_placeholder() {
        let specs = builtin_prompt_preset_specs();
        let (_, content) = specs.iter().find(|(n, _)| *n == "reviewer").unwrap();
        assert!(
            content.contains("{{spec_content}}"),
            "reviewer preset must contain {{{{spec_content}}}} placeholder"
        );
    }

    #[test]
    fn resilience_preset_contains_previous_feedback_placeholder() {
        let specs = builtin_prompt_preset_specs();
        let (_, content) = specs.iter().find(|(n, _)| *n == "resilience").unwrap();
        assert!(
            content.contains("{{previous_feedback}}"),
            "resilience preset must contain {{{{previous_feedback}}}} placeholder"
        );
    }

    /// Asserts `needle` (a placeholder) appears strictly between the open
    /// and close tags of `outer_tag` — i.e. inside that section, not
    /// floating before/after it.
    fn assert_inside_tag(content: &str, needle: &str, outer_tag: &str) {
        let open = format!("<{outer_tag}>");
        let close = format!("</{outer_tag}>");
        let open_at = content
            .find(&open)
            .unwrap_or_else(|| panic!("missing <{outer_tag}> in: {content}"));
        let close_at = content
            .find(&close)
            .unwrap_or_else(|| panic!("missing </{outer_tag}> in: {content}"));
        let needle_at = content
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle} in: {content}"));
        assert!(
            needle_at > open_at && needle_at < close_at,
            "{needle} must sit inside <{outer_tag}>...</{outer_tag}>, not floating between sections"
        );
    }

    #[test]
    fn implementer_preset_placeholders_resolve_inside_the_procedure_section() {
        let content = implementer_preset();
        assert_inside_tag(&content, "{{spec_content}}", "instruction_set");
        assert_inside_tag(&content, "{{previous_feedback}}", "instruction_set");
        assert_inside_tag(&content, "(none)", "instruction_set");
    }

    #[test]
    fn reviewer_preset_placeholder_resolves_inside_the_procedure_section() {
        let content = reviewer_preset();
        assert_inside_tag(&content, "{{spec_content}}", "instruction_set");
    }

    #[test]
    fn resilience_preset_placeholder_resolves_inside_the_procedure_section() {
        let content = resilience_preset();
        assert_inside_tag(&content, "{{previous_feedback}}", "instruction_set");
    }

    #[test]
    fn all_three_builtin_presets_share_the_same_section_skeleton() {
        for (name, content) in builtin_prompt_preset_specs() {
            for tag in [ROLE.tag, PROCEDURE.tag, ALLOWED.tag, PROHIBITED.tag] {
                assert!(
                    content.contains(&format!("<{tag}>")) && content.contains(&format!("</{tag}>")),
                    "preset '{name}' is missing the shared <{tag}> section"
                );
            }
        }
    }

    #[test]
    fn resolve_prompt_preset_returns_an_untagged_pre_migration_file_unchanged() {
        let dir = tempdir().unwrap();
        let prompts = prompts_dir(dir.path());
        std::fs::create_dir_all(&prompts).unwrap();
        let old_style = "You are the reviewer and committer for this loop.\n\n\
             Review the current diff strictly against this spec — nothing else:\n\n{{spec_content}}\n";
        std::fs::write(prompts.join("reviewer.md"), old_style).unwrap();

        let resolved = resolve_prompt_preset(&prompts, "reviewer");
        assert_eq!(resolved, old_style);
        // No tags in this file at all — `resolve_prompt_preset` must not
        // require them to work, and the placeholder must still be present
        // for the node prompt renderer's plain `.replace()` to find.
        assert!(!resolved.contains('<'));
        assert!(resolved.contains("{{spec_content}}"));
    }

    #[test]
    fn seeding_leaves_an_old_untagged_installation_mixed_with_new_tagged_defaults() {
        let dir = tempdir().unwrap();
        let canopy_dir = dir.path();
        let prompts = prompts_dir(canopy_dir);
        std::fs::create_dir_all(&prompts).unwrap();
        // Simulate an existing installation: an operator-edited reviewer.md
        // in the pre-tagged format, predating this change.
        let old_style = "You are the reviewer and committer for this loop.\n{{spec_content}}\n";
        std::fs::write(prompts.join("reviewer.md"), old_style).unwrap();

        // A daemon startup on the new binary reseeds missing files only.
        seed_builtin_prompt_presets(canopy_dir).unwrap();

        // The old file is untouched...
        assert_eq!(
            resolve_prompt_preset(&prompts, "reviewer"),
            old_style,
            "an edited file predating this change must not be overwritten"
        );
        // ...while a preset with no file on disk gets the new tagged form.
        assert_eq!(
            resolve_prompt_preset(&prompts, "implementer"),
            implementer_preset()
        );
        assert!(resolve_prompt_preset(&prompts, "implementer").contains("<role>"));
    }

    // ── render_preset ─────────────────────────────────────────────

    #[test]
    fn render_preset_substitutes_every_bound_placeholder() {
        let mut bindings = HashMap::new();
        bindings.insert("spec_content".to_string(), "do the thing".to_string());
        bindings.insert("previous_feedback".to_string(), "(none)".to_string());

        let rendered = render_preset("implementer", &implementer_preset(), &bindings).unwrap();

        assert!(!rendered.contains("{{"));
        assert!(rendered.contains("do the thing"));
        assert!(rendered.contains("(none)"));
    }

    #[test]
    fn render_preset_refuses_and_names_preset_and_missing_binding() {
        let content = "Body with {{spec_content}} inside.";
        let err = render_preset("implementer", content, &HashMap::new()).unwrap_err();

        assert_eq!(err.preset, "implementer");
        assert_eq!(err.binding, "spec_content");
    }

    #[test]
    fn render_preset_with_no_placeholders_and_no_bindings_returns_content_unchanged() {
        let content = "Just plain preset text, no placeholders at all.";
        let rendered = render_preset("custom", content, &HashMap::new()).unwrap();
        assert_eq!(rendered, content);
    }

    #[test]
    fn render_preset_emits_nothing_when_refused() {
        // A refusal returns Err, never a partially-substituted or
        // placeholder-carrying Ok — the caller cannot accidentally use a
        // half-rendered body.
        let content = "{{a}} and {{b}}";
        let mut bindings = HashMap::new();
        bindings.insert("a".to_string(), "filled".to_string());
        // 'b' deliberately left unbound.
        assert!(render_preset("p", content, &bindings).is_err());
    }

    #[test]
    fn render_preset_property_every_builtin_preset_renders_with_no_placeholder_leftover() {
        let mut bindings = HashMap::new();
        bindings.insert("spec_content".to_string(), "SPEC-BODY".to_string());
        bindings.insert("previous_feedback".to_string(), "FEEDBACK-BODY".to_string());

        for (name, content) in builtin_prompt_preset_specs() {
            let rendered = render_preset(name, &content, &bindings)
                .unwrap_or_else(|e| panic!("preset '{name}' failed to render: {e}"));
            assert!(
                !rendered.contains("{{"),
                "preset '{name}' left an unfilled placeholder: {rendered}"
            );
        }
    }

    #[test]
    fn render_preset_two_presets_in_one_session_stay_isolated() {
        let mut bindings_a = HashMap::new();
        bindings_a.insert("spec_content".to_string(), "ONLY-IN-A".to_string());
        bindings_a.insert("previous_feedback".to_string(), "(none)".to_string());

        let mut bindings_b = HashMap::new();
        bindings_b.insert("spec_content".to_string(), "ONLY-IN-B".to_string());

        let rendered_a = render_preset("implementer", &implementer_preset(), &bindings_a).unwrap();
        let rendered_b = render_preset("reviewer", &reviewer_preset(), &bindings_b).unwrap();

        assert!(rendered_a.contains("ONLY-IN-A"));
        assert!(!rendered_a.contains("ONLY-IN-B"));
        assert!(rendered_b.contains("ONLY-IN-B"));
        assert!(!rendered_b.contains("ONLY-IN-A"));
        assert!(!rendered_a.contains(&rendered_b));
        assert!(!rendered_b.contains(&rendered_a));
    }

    #[test]
    fn unbindable_placeholders_empty_when_every_marker_is_supported() {
        let template = "Do {{spec_content}} then read {{previous_feedback}}";
        assert!(
            unbindable_placeholders(template, &["spec_content", "previous_feedback"]).is_empty()
        );
    }

    #[test]
    fn unbindable_placeholders_names_the_markers_no_binding_covers() {
        let template = "Do {{spec_content}} and also {{custom_thing}}";
        assert_eq!(
            unbindable_placeholders(template, &["spec_content", "previous_feedback"]),
            vec!["custom_thing"]
        );
    }

    #[test]
    fn unbindable_placeholders_ignores_braces_in_a_bound_value_not_the_template() {
        // The template is clean; only the eventual *value* of {{spec_content}}
        // would contain "{{x}}" — that is data, and must not count as unbindable.
        let template = "Implement this: {{spec_content}}";
        assert!(unbindable_placeholders(template, &["spec_content"]).is_empty());
    }

    #[test]
    fn unbindable_placeholders_flags_a_supported_name_padded_with_inner_whitespace() {
        // The renderers substitute the exact `{{spec_content}}` sequence, so
        // `{{ spec_content }}` would survive into the output as a literal
        // marker even though `spec_content` is a supported binding — it must
        // be refused, not emitted.
        let template = "Do {{ spec_content }} now";
        assert_eq!(
            unbindable_placeholders(template, &["spec_content"]),
            vec!["spec_content"]
        );
    }

    #[test]
    fn unbindable_placeholders_ignores_an_escaped_unknown_marker() {
        let template = "Fail when \\{{something}} appears in a touched file.";
        assert!(
            unbindable_placeholders(template, &["spec_content"]).is_empty(),
            "escaped marker must not be reported"
        );
    }

    #[test]
    fn unbindable_placeholders_ignores_an_escaped_valid_binding() {
        // An escaped marker naming a real binding is still literal text.
        assert!(
            unbindable_placeholders("The literal \\{{spec_content}} marker.", &["spec_content"])
                .is_empty(),
            "escaped valid binding must not be reported"
        );
        // Padded so it discriminates: without the escape, `{{ spec_content }}`
        // is reported (the renderer binds only the exact name form), so an
        // empty result here can only mean the `\` was honoured.
        assert!(
            unbindable_placeholders(
                "The literal \\{{ spec_content }} marker.",
                &["spec_content"]
            )
            .is_empty(),
            "escaped padded binding must not be reported"
        );
        assert_eq!(
            unbindable_placeholders("The literal {{ spec_content }} marker.", &["spec_content"]),
            vec!["spec_content"],
            "the same marker unescaped is still unbindable"
        );
    }

    #[test]
    fn render_preset_emits_an_escaped_unknown_marker_as_literal() {
        let template = "Fail when \\{{something}} appears in a touched file.";
        let rendered = render_preset("reviewer", template, &HashMap::new()).unwrap();
        assert_eq!(
            rendered,
            "Fail when {{something}} appears in a touched file."
        );
    }

    #[test]
    fn render_preset_escape_wins_over_a_real_binding_without_rescan() {
        let mut bindings = HashMap::new();
        bindings.insert("spec_content".to_string(), "SPEC-BODY".to_string());
        let rendered = render_preset("p", "Literal \\{{spec_content}} here.", &bindings).unwrap();
        assert_eq!(rendered, "Literal {{spec_content}} here.");
    }

    #[test]
    fn render_preset_leaves_ordinary_backslashes_untouched() {
        let template = "Path C:\\temp and regex \\d+ stay.";
        let rendered = render_preset("p", template, &HashMap::new()).unwrap();
        assert_eq!(rendered, template);
    }

    #[test]
    fn render_template_does_not_rescan_replacement_values() {
        let mut bindings = HashMap::new();
        bindings.insert(
            "spec_content".to_string(),
            "value holding {{inner}} text".to_string(),
        );
        let rendered = render_preset("p", "Body: {{spec_content}}.", &bindings).unwrap();
        assert!(
            rendered.contains("{{inner}}"),
            "replacement text must be emitted as data, not rescanned: {rendered}"
        );
    }

    #[test]
    fn resilience_preset_includes_loop_schedule_autorun_instruction() {
        let specs = builtin_prompt_preset_specs();
        let (_, content) = specs.iter().find(|(n, _)| *n == "resilience").unwrap();
        assert!(
            content.contains("loop_schedule_autorun"),
            "resilience preset must instruct calling loop_schedule_autorun for quota failures"
        );
        assert!(
            content.contains("quota_reset_message"),
            "resilience preset must name the quota_reset_message parameter"
        );
        assert!(
            content.contains("verbatim"),
            "resilience preset must instruct passing the text verbatim, not extracted"
        );
    }
}
