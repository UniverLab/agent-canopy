//! Content-hash based overwrite policy for skills sync.
//!
//! An earlier incident showed that mtimes cannot be trusted as a divergence
//! signal: a plain file copy preserves the source mtime, so a stale copy can
//! look "unchanged" even though its content is old. Divergence is therefore
//! always decided by hashing file content, never by timestamps.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncAction {
    /// Destination doesn't exist yet, or `force` is set: write incoming content.
    Write,
    /// Destination already matches incoming content byte-for-byte: nothing to do.
    Unchanged,
    /// Destination diverges from incoming content and `force` is not set: skip.
    Skip,
}

fn hash(content: &[u8]) -> [u8; 32] {
    Sha256::digest(content).into()
}

/// Decide what to do about writing `incoming` over `existing` (the current
/// destination content, or `None` if the destination doesn't exist yet).
///
/// Pure and filesystem-free so the decision matrix can be unit tested directly.
pub(crate) fn decide_sync_action(
    existing: Option<&[u8]>,
    incoming: &[u8],
    force: bool,
) -> SyncAction {
    match existing {
        None => SyncAction::Write,
        Some(_) if force => SyncAction::Write,
        Some(existing) if hash(existing) == hash(incoming) => SyncAction::Unchanged,
        Some(_) => SyncAction::Skip,
    }
}

fn sync_new_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".sync-new");
    dest.with_file_name(name)
}

/// Apply [`decide_sync_action`] against a real file: read `dest` if present,
/// then write, skip-with-warning (leaving a `.sync-new` sidecar for diffing),
/// or leave an already-matching destination untouched.
///
/// `source` identifies where `incoming` came from (e.g. a URL or a source
/// file path) purely for the WARN message — it plays no part in the decision.
///
/// Returns `true` if `dest` was written.
pub(crate) fn sync_write(
    dest: &Path,
    source: &str,
    incoming: &[u8],
    force: bool,
) -> std::io::Result<bool> {
    let existing = if dest.exists() {
        Some(std::fs::read(dest)?)
    } else {
        None
    };

    match decide_sync_action(existing.as_deref(), incoming, force) {
        SyncAction::Write => {
            std::fs::write(dest, incoming)?;
            // Clear a stale sidecar left behind by a previously skipped sync.
            let sidecar = sync_new_path(dest);
            if sidecar.exists() {
                let _ = std::fs::remove_file(&sidecar);
            }
            Ok(true)
        }
        SyncAction::Unchanged => Ok(false),
        SyncAction::Skip => {
            let sidecar = sync_new_path(dest);
            match std::fs::write(&sidecar, incoming) {
                Ok(()) => tracing::warn!(
                    "Skills sync: destination {} diverges from sync source {}; \
                     NOT overwriting (local content differs from incoming content). \
                     Incoming content saved to {} for comparison — \
                     re-run with --force-skills to overwrite.",
                    dest.display(),
                    source,
                    sidecar.display()
                ),
                Err(e) => tracing::warn!(
                    "Skills sync: destination {} diverges from sync source {}; \
                     NOT overwriting (local content differs from incoming content). \
                     Could not write {} for comparison: {e} — \
                     re-run with --force-skills to overwrite.",
                    dest.display(),
                    source,
                    sidecar.display()
                ),
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_when_destination_missing() {
        assert_eq!(decide_sync_action(None, b"new", false), SyncAction::Write);
    }

    #[test]
    fn skips_when_content_diverges() {
        assert_eq!(
            decide_sync_action(Some(b"local v1.7"), b"stale v1.2", false),
            SyncAction::Skip
        );
    }

    #[test]
    fn unchanged_when_content_identical() {
        assert_eq!(
            decide_sync_action(Some(b"same"), b"same", false),
            SyncAction::Unchanged
        );
    }

    #[test]
    fn force_overwrites_diverged_content() {
        assert_eq!(
            decide_sync_action(Some(b"local v1.7"), b"stale v1.2", true),
            SyncAction::Write
        );
    }

    #[test]
    fn force_is_a_noop_when_content_already_matches() {
        // Force still results in a write, which is fine — it's a byte-identical
        // rewrite. Documented here so the behavior doesn't drift silently.
        assert_eq!(
            decide_sync_action(Some(b"same"), b"same", true),
            SyncAction::Write
        );
    }

    #[test]
    fn sync_write_skips_and_creates_sidecar_on_divergence() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("SKILL.md");
        std::fs::write(&dest, b"local v1.7 content").unwrap();

        let wrote = sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"stale v1.2 content",
            false,
        )
        .unwrap();

        assert!(!wrote);
        assert_eq!(std::fs::read(&dest).unwrap(), b"local v1.7 content");
        let sidecar = dir.path().join("SKILL.md.sync-new");
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"stale v1.2 content");
    }

    #[test]
    fn sync_write_leaves_identical_destination_untouched_with_no_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("SKILL.md");
        std::fs::write(&dest, b"same content").unwrap();

        let wrote = sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"same content",
            false,
        )
        .unwrap();

        assert!(!wrote);
        assert_eq!(std::fs::read(&dest).unwrap(), b"same content");
        assert!(!dir.path().join("SKILL.md.sync-new").exists());
    }

    #[test]
    fn sync_write_force_overwrites_diverged_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("SKILL.md");
        std::fs::write(&dest, b"local content").unwrap();

        let wrote = sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"forced content",
            true,
        )
        .unwrap();

        assert!(wrote);
        assert_eq!(std::fs::read(&dest).unwrap(), b"forced content");
    }

    #[test]
    fn sync_write_creates_new_file_when_destination_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("SKILL.md");

        let wrote = sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"brand new",
            false,
        )
        .unwrap();

        assert!(wrote);
        assert_eq!(std::fs::read(&dest).unwrap(), b"brand new");
        assert!(!dir.path().join("SKILL.md.sync-new").exists());
    }

    #[test]
    fn sync_write_clears_stale_sidecar_once_overwrite_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("SKILL.md");
        let sidecar = dir.path().join("SKILL.md.sync-new");
        std::fs::write(&dest, b"local content").unwrap();
        std::fs::write(&sidecar, b"leftover from a previous skipped sync").unwrap();

        let wrote = sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"forced content",
            true,
        )
        .unwrap();

        assert!(wrote);
        assert!(!sidecar.exists());
    }

    #[test]
    fn running_twice_in_a_row_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("SKILL.md");

        assert!(sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"content",
            false
        )
        .unwrap());
        // Second run: destination now matches incoming, so nothing changes.
        assert!(!sync_write(
            &dest,
            "essential-pack:loop-design/SKILL.md",
            b"content",
            false
        )
        .unwrap());
        assert!(!dir.path().join("SKILL.md.sync-new").exists());
    }
}
