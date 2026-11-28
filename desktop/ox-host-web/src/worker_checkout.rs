//! A host-to-host checkout session. Git IDs and archive chunks share the same
//! authenticated connection. The receiver's filesystem paths stay on the receiver.
pub(crate) mod connection;
pub(crate) mod receiver;
mod sender;
pub use sender::{Sender, transfer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, digest::Output};
use std::io;
use tokio::io::AsyncWriteExt;

pub(crate) const CHUNK: usize = 256 * 1024;

pub(crate) struct ArchiveChunks {
    remaining: u64,
    hash: Sha256,
}

impl ArchiveChunks {
    pub(crate) fn new(bytes: u64) -> Self {
        Self {
            remaining: bytes,
            hash: Sha256::new(),
        }
    }

    pub(crate) fn complete(&self) -> bool {
        self.remaining == 0
    }

    pub(crate) async fn write(
        &mut self,
        file: &mut tokio::fs::File,
        chunk: &[u8],
    ) -> io::Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(chunk.len() as u64)
            .filter(|_| !chunk.is_empty() && chunk.len() <= CHUNK)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Invalid archive chunk length")
            })?;
        file.write_all(chunk).await?;
        self.hash.update(chunk);
        Ok(())
    }

    pub(crate) fn digest(self) -> Output<Sha256> {
        self.hash.finalize()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Checkout {
    source: String,
    target: String,
    repository: String,
    job: u64,
    /// An immutable source-generated UUID for this job, reused if its reply is lost.
    request_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Open(Checkout),
    Known { commits: Vec<String> },
    Archive { bytes: u64, commit: String },
    Finish { sha256: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
enum Reply {
    Ready {
        target: String,
        source: String,
        existing: Option<Receipt>,
    },
    Known {
        commits: Vec<String>,
    },
    Installed(Delivery),
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub commit: String,
    pub archive_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Delivery {
    pub receipt: Receipt,
    /// A retry or concurrent request found this job's previously installed snapshot.
    pub recovered: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paired_hosts;
    use std::{env, fs, path::PathBuf};

    #[test]
    #[ignore = "two isolated hosts through the TLS browser fixture"]
    fn linked_checkout() {
        let state = PathBuf::from(env::var_os("OX_TEST_REMOTE_DIR").unwrap());
        let source = fs::read_to_string(state.join("host-id")).unwrap();
        let hosts = paired_hosts::worker_links(&state.join("worker-links.json")).unwrap();
        let send = |request| {
            transfer(
                &hosts[0],
                source.trim(),
                &state.join("inventory-source/ox/nested"),
                &state,
                42,
                request,
            )
        };
        let request = "ddcba183-5073-4e9a-9825-e8730784ce5e";
        if env::var_os("OX_TEST_DENIED_LINK").is_some() {
            assert!(send(request).is_err());
            return;
        }
        let (first, second) = std::thread::scope(|s| {
            let a = s.spawn(|| send(request).unwrap());
            (send(request).unwrap(), a.join().unwrap())
        });
        assert_ne!(first.recovered, second.recovered);
        assert_eq!(first.receipt, second.receipt);
        let receipt = first.receipt;
        assert!(
            receipt.archive_bytes < 1_000_000,
            "the common 2 MiB base should not cross the link"
        );
        let replay = send(request).unwrap();
        assert!(replay.recovered);
        assert_eq!(replay.receipt, receipt);
        assert!(send("3d89eff6-a436-40a4-a9ad-fa294b50c48d").is_err());
        // Drop a connection after freezing the archive but before its chunks leave.
        // A restarted sender must deliver those exact bytes even after another edit.
        let spool = tempfile::tempdir_in(&state).unwrap();
        let project = state.join("inventory-source/ox/nested");
        let frozen = Sender {
            host: &hosts[0],
            source: source.trim(),
            project: &project,
            staging: &state,
            job: 45,
            request_id: "64e47f98-97b7-47c6-8fd4-2d7872f3f79c",
        };
        let stopped = frozen
            .run(Some(spool.path()), || {
                spool.path().join("snapshot").exists()
            })
            .unwrap_err();
        assert_eq!(stopped.kind(), std::io::ErrorKind::Interrupted);
        let path = project.join("tracked");
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"later laptop edit").unwrap();
        let delivered = frozen.run(Some(spool.path()), || false);
        fs::write(&path, original).unwrap();
        assert!(!delivered.unwrap().recovered);

        assert!(
            transfer(
                &hosts[0],
                source.trim(),
                &state.join("another/ox"),
                &state,
                43,
                request
            )
            .is_err()
        );
    }
}
