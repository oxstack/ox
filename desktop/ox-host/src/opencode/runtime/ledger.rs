use super::{Phase, Receipt};
use crate::opencode::{Error, Result, check_id};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{self, Read as _, Write as _},
    os::fd::AsRawFd as _,
    path::PathBuf,
};
use uuid::Uuid;

const FORMAT: u32 = 1;
const MAX_BYTES: usize = super::super::MAX_WIRE_BYTES;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Record {
    pub native: String,
    pub title: String,
    pub updated_at: i64,
    pub turn: Option<Turn>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Turn {
    pub sequence: u64,
    pub native_message: String,
    pub digest: String,
    pub phase: Phase,
}

impl Turn {
    pub fn receipt(&self) -> Receipt {
        Receipt {
            sequence: self.sequence,
            phase: self.phase,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct State {
    version: u32,
    owner: String,
    sessions: BTreeMap<Uuid, Record>,
}

pub(super) struct Ledger {
    path: PathBuf,
    state: State,
    writable: bool,
}

impl Ledger {
    pub fn open(path: PathBuf, owner: String) -> Result<Self> {
        let state = match File::open(&path) {
            Ok(file) => {
                let mut data = Vec::new();
                file.take((MAX_BYTES + 1) as u64).read_to_end(&mut data)?;
                if data.len() > MAX_BYTES {
                    return Err(Error::RegistryUnavailable);
                }
                serde_json::from_slice::<State>(&data)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => State {
                version: FORMAT,
                owner: owner.clone(),
                sessions: BTreeMap::new(),
            },
            Err(error) => return Err(error.into()),
        };
        if state.owner != owner {
            return Err(Error::OwnerMismatch);
        }
        if state.version != FORMAT {
            return Err(Error::RegistryUnavailable);
        }
        let mut native_ids = HashSet::new();
        for (id, record) in &state.sessions {
            if id.is_nil()
                || !native_ids.insert(&record.native)
                || check_id(&record.native, "ses_").is_err()
                || record.turn.as_ref().is_some_and(|turn| {
                    turn.sequence == 0 || check_id(&turn.native_message, "msg_").is_err()
                })
            {
                return Err(Error::RegistryUnavailable);
            }
        }
        Ok(Self {
            path,
            state,
            writable: true,
        })
    }

    pub fn check(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::RegistryUnavailable)
        }
    }

    pub fn ids(&self) -> Vec<Uuid> {
        self.state.sessions.keys().copied().collect()
    }

    pub fn get(&self, id: Uuid) -> Option<&Record> {
        self.state.sessions.get(&id)
    }

    pub fn put(&mut self, id: Uuid, record: Record) -> Result<()> {
        self.check()?;
        self.state.sessions.insert(id, record);
        // Rename and directory sync can fail independently. Never issue a
        // native mutation based on a registry of uncertain durability.
        self.writable = false;
        self.save()?;
        self.writable = true;
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let data = serde_json::to_vec(&self.state)?;
        if data.len() > MAX_BYTES {
            return Err(Error::RegistryUnavailable);
        }
        let parent = self.path.parent().ok_or(Error::RegistryUnavailable)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".ox-session-")
            .tempfile_in(parent)?;
        temporary.write_all(&data)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        let dir = File::open(parent)?;
        // SAFETY: dir is an owned, valid directory descriptor; fsync does not
        // take ownership. Darwin's regular-file F_FULLFSYNC cannot cover it.
        if unsafe { libc::fsync(dir.as_raw_fd()) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(native: &str) -> Record {
        Record {
            native: native.into(),
            title: "first".into(),
            updated_at: 10,
            turn: None,
        }
    }

    #[test]
    fn binds_on_disk_records_to_one_route_and_rejects_duplicate_native_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.json");
        let id = Uuid::new_v4();
        let mut ledger = Ledger::open(path.clone(), "route".into()).unwrap();
        ledger.put(id, record("ses_bound")).unwrap();
        assert!(matches!(
            Ledger::open(path.clone(), "different".into()),
            Err(Error::OwnerMismatch)
        ));
        assert_eq!(
            Ledger::open(path.clone(), "route".into()).unwrap().ids(),
            vec![id]
        );
        ledger.put(Uuid::new_v4(), record("ses_bound")).unwrap();
        assert!(matches!(
            Ledger::open(path, "route".into()),
            Err(Error::RegistryUnavailable)
        ));
    }

    #[test]
    fn a_failed_journal_write_fences_actions_for_the_lifetime_of_the_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.json");
        let mut ledger = Ledger::open(path.clone(), "route".into()).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(ledger.put(Uuid::new_v4(), record("ses_bound")).is_err());
        assert!(matches!(ledger.check(), Err(Error::RegistryUnavailable)));
    }
}
