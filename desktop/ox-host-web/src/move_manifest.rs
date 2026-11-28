//! File boundary shared by the move sender and authenticated staging receiver.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PLAN: &str = "plan.json";
pub const TRANSCRIPT: &str = "transcript.archive";
pub const CHECKOUT: &str = "checkout.bundle";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub bytes: u64,
    pub sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plan: FileSpec,
    pub transcript: FileSpec,
    pub checkout: FileSpec,
}

impl Manifest {
    pub fn sha256(&self) -> serde_json::Result<[u8; 32]> {
        Ok(Sha256::digest(serde_json::to_vec(self)?).into())
    }
}
