//! Shared, content-free accounting of size-based RAG exclusions.
//!
//! `canopy doctor` and `canopy rag report` both need to answer "how many files
//! is the per-file size limit leaving out of the index?". The DB ledger is not
//! a trustworthy source for that number: it goes stale after a config, path, or
//! `ragignore` change, and it says nothing about a large file that was never
//! ingested in the first place. So the count is recomputed here from filesystem
//! metadata alone — the same traversal and filters startup ingestion uses
//! (`ragignore` patterns, then [`chunker::detect_lang`]), and **never** opening
//! a candidate file's contents.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::rag::{chunker, ragignore};

/// Outcome of a metadata-only walk of the configured RAG roots.
#[derive(Debug, Default, Clone)]
pub(crate) struct SizeScan {
    /// Supported, non-ignored files at or under the limit — the corpus that
    /// would actually be indexed.
    pub indexable_files: Vec<PathBuf>,
    /// Supported, non-ignored files whose on-disk size exceeds the limit and
    /// are therefore silently skipped by ingestion.
    pub oversize_files: Vec<PathBuf>,
}

/// Walk each existing root in `roots`, applying the same filters ingestion
/// applies (`ragignore` patterns loaded from `data_dir`, then
/// [`chunker::detect_lang`]), and split the surviving files by whether their
/// metadata length exceeds `max_bytes`. Overlapping roots are de-duplicated so
/// a nested directory does not double-count.
///
/// Only [`walkdir::DirEntry::metadata`] is consulted — no candidate file is
/// opened or read. Walk and metadata errors are propagated with path context
/// rather than silently treated as "no exclusions": a root that cannot be read
/// is a diagnostic the caller must surface.
pub(crate) fn scan(data_dir: &Path, roots: &[String], max_bytes: u64) -> anyhow::Result<SizeScan> {
    let patterns = ragignore::load_patterns(data_dir);
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut scan = SizeScan::default();

    for root in roots {
        let root_path = Path::new(root);
        if !root_path.exists() {
            // A missing directory is reported separately (doctor prints a
            // dedicated "RAG dir missing" line); here it simply contributes
            // nothing to the size accounting.
            continue;
        }
        for entry in walkdir::WalkDir::new(root_path).follow_links(false) {
            let entry = entry.with_context(|| format!("walking RAG directory '{root}'"))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if ragignore::is_ignored(path, root_path, &patterns) {
                continue;
            }
            if chunker::detect_lang(&path.to_string_lossy()).is_none() {
                continue;
            }
            if !seen.insert(path.to_path_buf()) {
                continue;
            }
            let meta = entry
                .metadata()
                .with_context(|| format!("reading metadata for '{}'", path.display()))?;
            if meta.len() > max_bytes {
                scan.oversize_files.push(path.to_path_buf());
            } else {
                scan.indexable_files.push(path.to_path_buf());
            }
        }
    }

    Ok(scan)
}

/// One-line summary of size-based exclusions, shared verbatim by `canopy
/// doctor` and `canopy rag report` so both state the limit identically —
/// including when the count is zero, because the whole point of CB20 is that
/// an excluded file is a visible fact and not an absence nobody notices.
pub(crate) fn exclusion_summary(oversize_count: usize, max_bytes: u64) -> String {
    let cap_mb = max_bytes as f64 / (1024.0 * 1024.0);
    format!(
        "Size exclusions: {oversize_count} file(s) exceed the {cap_mb:.0} MB indexing limit \
         (config.toml: rag_max_file_mb)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_bytes(path: &Path, len: usize) {
        fs::write(path, vec![b'x'; len]).unwrap();
    }

    fn names(paths: &[PathBuf]) -> BTreeSet<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn scan_splits_supported_files_by_metadata_size() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("docs");
        fs::create_dir_all(&root).unwrap();

        write_bytes(&root.join("small.md"), 10);
        write_bytes(&root.join("big.md"), 200);
        write_bytes(&root.join("big.pdf"), 300);
        // Unsupported extension: never counted, over the limit or not.
        write_bytes(&root.join("big.txt"), 500);

        let scan = scan(tmp.path(), &[root.to_string_lossy().to_string()], 100).unwrap();

        assert_eq!(
            names(&scan.oversize_files),
            ["big.md", "big.pdf"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        assert_eq!(
            names(&scan.indexable_files),
            ["small.md"].into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn scan_excludes_ragignored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("docs");
        fs::create_dir_all(&root).unwrap();
        // The default ragignore patterns drop dotfiles.
        write_bytes(&root.join(".hidden-big.md"), 200);
        write_bytes(&root.join("visible-big.md"), 200);

        let scan = scan(tmp.path(), &[root.to_string_lossy().to_string()], 100).unwrap();

        assert_eq!(
            names(&scan.oversize_files),
            BTreeSet::from(["visible-big.md".to_string()])
        );
    }

    #[test]
    fn scan_counts_overlapping_roots_once() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("docs");
        let inner = outer.join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_bytes(&inner.join("big.md"), 200);

        let scan = scan(
            tmp.path(),
            &[
                outer.to_string_lossy().to_string(),
                inner.to_string_lossy().to_string(),
            ],
            100,
        )
        .unwrap();

        assert_eq!(
            scan.oversize_files.len(),
            1,
            "a file reachable from two overlapping roots must count once"
        );
    }

    #[test]
    fn scan_uses_metadata_only_and_never_reads_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("docs");
        fs::create_dir_all(&root).unwrap();

        // A sparse 512 MiB "PDF": `metadata().len()` reports the full length,
        // but not one byte is stored and it is not valid PDF data. Anything
        // that opened and read/parsed its contents here would either churn
        // through half a gigabyte of zeros or fail outright — `scan` does
        // neither, because it only stats.
        let huge = root.join("sparse.pdf");
        let f = fs::File::create(&huge).unwrap();
        f.set_len(512 * 1024 * 1024).unwrap();
        drop(f);

        let scan = scan(
            tmp.path(),
            &[root.to_string_lossy().to_string()],
            10 * 1024 * 1024,
        )
        .unwrap();

        assert_eq!(
            names(&scan.oversize_files),
            BTreeSet::from(["sparse.pdf".to_string()])
        );
        assert!(scan.indexable_files.is_empty());
    }

    #[test]
    fn scan_tolerates_a_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let scan = scan(
            tmp.path(),
            &[tmp.path().join("nope").to_string_lossy().to_string()],
            100,
        )
        .unwrap();
        assert!(scan.oversize_files.is_empty());
        assert!(scan.indexable_files.is_empty());
    }

    #[test]
    fn exclusion_summary_states_the_limit_even_at_zero() {
        assert_eq!(
            exclusion_summary(0, 10 * 1024 * 1024),
            "Size exclusions: 0 file(s) exceed the 10 MB indexing limit \
             (config.toml: rag_max_file_mb)"
        );
        assert!(exclusion_summary(81, 10 * 1024 * 1024)
            .starts_with("Size exclusions: 81 file(s) exceed the 10 MB"));
    }
}
