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

use std::path::{Path, PathBuf};

const IMPLEMENTER_PRESET: &str = "You are a careful Rust implementer working in this repository.

FIRST, verify the premise of the spec against the actual code: read the relevant files before changing anything. If the premise is wrong — it describes something that isn't true of the current code, or asks for a change that doesn't actually apply — stop and report exactly why instead of forcing a change to fit a wrong premise.

Otherwise, implement exactly this spec, nothing more and nothing less:

{{spec_content}}

Previous feedback (if any): {{previous_feedback}}
If it reads \"(none)\", this is a fresh implementation. Otherwise, address every point of the feedback as part of your changes.

Before reporting, run the checks relevant to what you changed (formatter, linter, tests) locally — never assume they pass without running them.

Do NOT commit your changes under any circumstances. A separate reviewer step reviews your diff and commits it.

If you are genuinely blocked — missing credentials, an unresolvable conflict, a decision only a human can make — make the final line of your report start with \"BLOCKER:\" followed by a one-paragraph explanation of exactly why.
";

const REVIEWER_PRESET: &str = "You are the reviewer and committer for this loop.

Review the current diff strictly against this spec — nothing else:

{{spec_content}}

If the diff correctly and completely implements the spec, make EXACTLY ONE commit covering ONLY this spec's work. Use a concise, descriptive commit message with NO trailers of any kind (no Co-Authored-By, no issue references, no generated-by footers). NEVER push.

If the worktree has no changes at all, FAIL and say so explicitly — do not commit an empty diff, and do not treat \"nothing changed\" as success.

If the diff is wrong, incomplete, or diverges from the spec, FAIL with a specific, actionable list of what's wrong so the implementer can address it directly — vague feedback is not acceptable.
";

const RESILIENCE_PRESET: &str = "# ROLE
You are the on-call medic for this loop. A node just failed or reported a blocker. Your only job is to diagnose why and route to the right next step — you do not fix the underlying work yourself.

# HOW
Read the failure/blocker context below (`{{previous_feedback}}`) and pick exactly ONE diagnosis:

- QUOTA — the failure is a rate limit, quota exhaustion, or \"try again later\" from the platform/API itself. Action: no code changes are needed; report QUOTA and that the node should be retried once quota resets.
- GLITCH — the failure is a transient infrastructure hiccup (network blip, spurious timeout, a flaky check unrelated to the spec) with no sign of a real defect. Action: report GLITCH and recommend a plain retry of the same node.
- OTHER — the failure reflects a genuine problem with the work itself (wrong code, an unmet spec, a missing dependency, a decision only a human can make). Action: report OTHER with a precise, actionable explanation of what's wrong so a human or the next agent can address it.

State your diagnosis and its one action clearly in your report; do not hedge between diagnoses.

# YES (allowed)
- Reading files, logs, git history, and process/job status.
- Checking scheduling/queue state (e.g. `git status`, `git log`, `ps`, reading CI/log output).
- Reporting your diagnosis and recommended action via loop_complete_node / loop_report_blocker.

# NO (forbidden)
- Writing code, or editing any file or config.
- Building or testing the project.
- Committing anything.
- Attempting to verify or finish the spec's actual work — that is not your job here.
";

/// `(name, content)` for every builtin prompt preset — the single source
/// both [`seed_builtin_prompt_presets`] writes to disk and
/// [`resolve_prompt_preset`] falls back to.
pub fn builtin_prompt_preset_specs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("implementer", IMPLEMENTER_PRESET),
        ("reviewer", REVIEWER_PRESET),
        ("resilience", RESILIENCE_PRESET),
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
                .map(|(_, content)| content.to_string())
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
            IMPLEMENTER_PRESET
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

        assert_eq!(resolve_prompt_preset(&prompts, "reviewer"), REVIEWER_PRESET);
    }

    #[test]
    fn resolve_prompt_preset_returns_empty_for_an_unknown_custom_preset_with_no_file() {
        let dir = tempdir().unwrap();
        let prompts = prompts_dir(dir.path());

        assert_eq!(resolve_prompt_preset(&prompts, "totally-custom"), "");
    }
}
