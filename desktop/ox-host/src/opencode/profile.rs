use super::{Error, Result};
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, DirBuilder},
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _},
    path::{Path, PathBuf},
};

/// Identity supplied by OX from its own host and billing registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Owner {
    host_id: String,
    project: String,
    billing_alias: String,
}

impl Owner {
    pub fn new(host_id: String, project: &Path, billing_alias: String) -> Result<Self> {
        if host_id.trim().is_empty() || billing_alias.trim().is_empty() {
            return Err(Error::InvalidInput(
                "OpenCode requires a host ID and OX billing alias",
            ));
        }
        let project = project.canonicalize()?;
        if !project.is_dir() {
            return Err(Error::InvalidInput("OpenCode project must be a directory"));
        }
        let project = project
            .into_os_string()
            .into_string()
            .map_err(|_| Error::InvalidInput("OpenCode requires a UTF-8 project path"))?;
        Ok(Self {
            host_id,
            project,
            billing_alias,
        })
    }

    pub fn project(&self) -> &Path {
        Path::new(&self.project)
    }

    pub fn directory(&self) -> &str {
        &self.project
    }

    pub fn billing_alias(&self) -> &str {
        &self.billing_alias
    }

    pub(super) fn profile_key(&self) -> String {
        let mut digest = Sha256::new();
        for field in [&self.host_id, &self.project, &self.billing_alias] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
        format!("{:x}", digest.finalize())
    }
}

/// An OX-managed partition for a single owner. It never opens the user's
/// unwrapped OpenCode database or another billing route's data.
pub struct Profile {
    owner: Owner,
    root: PathBuf,
}

impl Profile {
    pub fn prepare(state_root: &Path, owner: Owner) -> Result<Self> {
        // The host has already prepared and checked its state directory.
        validate_private(state_root)?;
        let base = state_root.canonicalize()?.join("opencode");
        create_private(&base)?;
        let root = base.join(owner.profile_key());
        create_private(&root)?;
        let profile = Self { owner, root };
        for path in [
            profile.cache_home(),
            profile.config_home(),
            profile.data_home(),
            profile.state_home(),
        ] {
            create_private(&path)?;
        }
        Ok(profile)
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_home(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn config_home(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn data_home(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn state_home(&self) -> PathBuf {
        self.root.join("state")
    }
}

fn create_private(path: &Path) -> Result<()> {
    if let Err(error) = DirBuilder::new().mode(0o700).create(path)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(error.into());
    }
    validate_private(path)
}

fn validate_private(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    // geteuid reads the process identity only.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(Error::InvalidInput(
            "OpenCode profile requires an owned private directory",
        ));
    }
    Ok(())
}
