use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

use crate::db::Database;
use crate::domain::loops::{LoopSpecStatus, SpecAdminStatusOutcome};

#[derive(Subcommand)]
pub enum SpecAction {
    /// Mark a spec as completed.
    Complete {
        /// Spec ID to complete.
        spec_id: String,
        /// Reason for completion.
        #[arg(long)]
        reason: String,
    },
    /// Mark a spec as skipped.
    Skip {
        /// Spec ID to skip.
        spec_id: String,
        /// Reason for skipping.
        #[arg(long)]
        reason: String,
    },
    /// Reopen a completed/skipped spec back to pending.
    Reopen {
        /// Spec ID to reopen.
        spec_id: String,
        /// Reason for reopening.
        #[arg(long)]
        reason: String,
    },
}

pub async fn handle_spec_action(action: SpecAction) -> Result<()> {
    let (spec_id, status, reason) = match action {
        SpecAction::Complete { spec_id, reason } => (spec_id, LoopSpecStatus::Completed, reason),
        SpecAction::Skip { spec_id, reason } => (spec_id, LoopSpecStatus::Skipped, reason),
        SpecAction::Reopen { spec_id, reason } => (spec_id, LoopSpecStatus::Pending, reason),
    };

    let db = Database::new(&ensure_data_dir()?)?;
    match db.set_spec_admin_status(&spec_id, status, &reason)? {
        SpecAdminStatusOutcome::Success => {
            println!(
                "Spec '{}' set to '{}': {}",
                spec_id,
                status.as_str(),
                reason
            );
            Ok(())
        }
        SpecAdminStatusOutcome::NotFound => Err(anyhow::anyhow!("Spec '{}' not found.", spec_id)),
        SpecAdminStatusOutcome::NotStandalone(loop_id) => Err(anyhow::anyhow!(
            "Spec '{}' is bound to loop '{}'; spec_set_status only administers standalone specs.",
            spec_id,
            loop_id
        )),
        SpecAdminStatusOutcome::ActiveRun { loop_id, run_id } => Err(anyhow::anyhow!(
            "Spec '{}' is attached to an active run (loop '{}', run '{}'); \
                 it cannot be administratively transitioned while running.",
            spec_id,
            loop_id,
            run_id
        )),
    }
}

fn ensure_data_dir() -> Result<PathBuf> {
    let base_dir = match std::env::var("CANOPY_DATA_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => {
            let home = std::env::var("HOME")?;
            PathBuf::from(home).join(".canopy")
        }
    };

    if !base_dir.exists() {
        std::fs::create_dir_all(&base_dir)?;
    }

    Ok(base_dir.join("canopy.db"))
}
