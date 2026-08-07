//! The one place that knows the live database's file name.
//!
//! [`DB_FILE_NAME`] is a legacy name (an earlier product name) kept as-is:
//! renaming it means migrating a live database a running binary may
//! already have open, for a cosmetic gain, and it's out of scope for
//! whatever change you're making unless you're the spec that owns that
//! migration. What every caller — the daemon, the TUI, and every CLI
//! subcommand that touches the database — needs is to agree on where it
//! is, so [`database_path`] is the only place that joins the file name onto
//! a data directory. A literal repeated at every call site is what let one
//! of them (`canopy spec`, before it was rewritten to delegate through the
//! daemon instead of opening the database itself) resolve a different file
//! than everyone else without anything noticing.
//!
//! Lives in `domain` rather than `db` because `domain` is compiled
//! standalone by `examples/rag_search.rs` without the `db` module present;
//! `db` and `daemon` can depend on this, not the other way around.

use std::path::{Path, PathBuf};

/// The live database's file name.
pub const DB_FILE_NAME: &str = "background_agents.db";

/// Resolve the live database's path from the canopy data directory.
pub fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_path_joins_the_canonical_file_name() {
        let data_dir = Path::new("/tmp/some-data-dir");
        assert_eq!(
            database_path(data_dir),
            data_dir.join("background_agents.db")
        );
    }
}
