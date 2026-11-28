//! Capture a frozen source's complete transcript and working tree for staging.
use crate::{
    move_archive::{Archive, checksum},
    move_ledger::Plan,
};
use ox_desk_core::host_protocol::{SessionResult, SessionView, Timeline};
use ox_host_service::checkout::Snapshot;
use ox_host_web::move_manifest::{CHECKOUT, FileSpec, Manifest, PLAN, TRANSCRIPT};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder, File},
    io::{self, Seek, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

const OUTGOING: &str = "outgoing-session-moves";
const MANIFEST: &str = "manifest.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundlePlan {
    pub(crate) plan: Plan,
    pub(crate) checkout_commit: String,
    pub(crate) presentation: Presentation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Presentation {
    pub(crate) title: String,
    pub(crate) updated_at: i64,
    pub(crate) last_result: Option<SessionResult>,
    pub(crate) model_provider: String,
}

impl From<&SessionView> for Presentation {
    fn from(session: &SessionView) -> Self {
        Self {
            title: session.title.chars().take(192).collect(),
            updated_at: session.updated_at,
            last_result: session.last_result(),
            model_provider: session.detail["row"]["modelProvider"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

pub(crate) struct Bundle {
    directory: PathBuf,
    pub(crate) manifest: Manifest,
}

impl Bundle {
    pub(crate) fn capture(
        staging: &Path,
        plan: &Plan,
        presentation: Presentation,
        timeline: &Timeline,
    ) -> io::Result<Self> {
        let spool = staging.join(OUTGOING);
        match DirBuilder::new().mode(0o700).create(&spool) {
            Ok(()) => File::open(staging)?.sync_all()?,
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && spool.symlink_metadata()?.is_dir() => {}
            Err(error) => return Err(error),
        }
        let directory = spool.join(plan.id.to_string());
        match directory.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {
                let bundle = Self::recover(staging, plan)?;
                if bundle.manifest.transcript.sha256 != Archive::fingerprint(plan, timeline)? {
                    return Err(io::Error::other(
                        "The conversation changed after the move was prepared.",
                    ));
                }
                File::open(spool)?.sync_all()?;
                return Ok(bundle);
            }
            Ok(_) => {
                return Err(io::Error::other(
                    "The source staging path is already in use.",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let stage = tempfile::Builder::new()
            .prefix(".move-export-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&spool)?;
        let transcript = stage.path().join(TRANSCRIPT);
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(transcript)?;
        let transcript = FileSpec {
            sha256: Archive::capture(&mut file, plan, timeline)?,
            bytes: file.metadata()?.len(),
        };
        // The destination might need to run without reaching the source origin.
        let snapshot = Snapshot::capture(stage.path(), &plan.source.scope.project, &[])?;
        snapshot.retain_archive(&stage.path().join(CHECKOUT))?;
        let mut archive = snapshot.open()?;
        let checkout = FileSpec {
            sha256: checksum(&mut archive)?,
            bytes: archive.metadata()?.len(),
        };
        let plan = serde_json::to_vec(&BundlePlan {
            plan: plan.clone(),
            checkout_commit: snapshot.commit.clone(),
            presentation,
        })?;
        let mut file = File::create_new(stage.path().join(PLAN))?;
        file.write_all(&plan)?;
        file.sync_all()?;
        let plan = FileSpec {
            sha256: Sha256::digest(&plan).into(),
            bytes: plan.len() as u64,
        };
        drop(snapshot);
        let manifest = Manifest {
            plan,
            transcript,
            checkout,
        };
        let manifest_file = File::create_new(stage.path().join(MANIFEST))?;
        serde_json::to_writer(&manifest_file, &manifest)?;
        manifest_file.sync_all()?;
        File::open(stage.path())?.sync_all()?;
        fs::rename(stage.path(), &directory)?;
        File::open(spool)?.sync_all()?;
        Ok(Self {
            directory,
            manifest,
        })
    }

    fn open(directory: PathBuf, plan: &Plan) -> io::Result<Self> {
        let manifest: Manifest = serde_json::from_reader(File::open(directory.join(MANIFEST))?)?;
        let saved: BundlePlan =
            serde_json::from_reader(verified_file(&directory.join(PLAN), &manifest.plan)?)?;
        if saved.plan != *plan {
            return Err(io::Error::other(
                "These source files belong to another move.",
            ));
        }
        verified_file(&directory.join(TRANSCRIPT), &manifest.transcript)?;
        verified_file(&directory.join(CHECKOUT), &manifest.checkout)?;
        Ok(Self {
            directory,
            manifest,
        })
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn recover(staging: &Path, plan: &Plan) -> io::Result<Self> {
        Self::open(staging.join(OUTGOING).join(plan.id.to_string()), plan)
    }
}

pub(crate) fn verified_file(path: &Path, spec: &FileSpec) -> io::Result<File> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() != spec.bytes || checksum(&mut file)? != spec.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "A move file failed its integrity check.",
        ));
    }
    file.rewind()?;
    Ok(file)
}

pub(crate) fn plan_digest(plan: &Plan) -> io::Result<[u8; 32]> {
    Ok(Sha256::digest(serde_json::to_vec(plan)?).into())
}
