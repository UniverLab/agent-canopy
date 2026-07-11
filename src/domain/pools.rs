use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::loops::LoopSpec;

/// An ordered queue of existing specs — the work, decoupled from any one
/// loop (the team that will eventually run it). A spec can be a member of
/// any number of pools; pool membership never mutates the spec itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// A pool together with its members, resolved and ordered by position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolDetails {
    pub pool: Pool,
    pub members: Vec<LoopSpec>,
}
