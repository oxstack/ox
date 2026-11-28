//! Find local copies of an asset already selected from official release metadata.

use super::{Asset, BinaryStage};
use crate::provider::RelayError;
use std::env;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(super) fn reuse(
    root: &Path,
    asset: &Asset,
    dest: &Path,
    search_path: &OsStr,
) -> Result<bool, RelayError> {
    for directory in env::split_paths(search_path).filter(|dir| dir.is_absolute()) {
        let Ok(command) = directory.join("agent-browser").canonicalize() else {
            continue;
        };
        let packaged = npm_payloads(&command, asset.name);
        for candidate in std::iter::once(command).chain(packaged.into_iter().flatten()) {
            if stage_candidate(root, asset, dest, &candidate)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn npm_payloads(command: &Path, artifact_name: &str) -> Option<[PathBuf; 3]> {
    let bin = command.parent()?;
    let package = bin.parent()?;
    let modules = package.parent()?;
    if command.file_name()? != "agent-browser.js"
        || bin.file_name()? != "bin"
        || package.file_name()? != "agent-browser"
        || modules.file_name()? != "node_modules"
    {
        return None;
    }
    let target = artifact_name.strip_prefix("agent-browser-")?;
    let libc = if target.starts_with("linux-") {
        "-gnu"
    } else {
        ""
    };
    let native_path =
        PathBuf::from(format!("@agent-browser/cli-{target}{libc}")).join("bin/agent-browser");
    // The package can materialize the native file, or install it as a nested or hoisted dependency.
    Some([
        bin.join(artifact_name),
        package.join("node_modules").join(&native_path),
        modules.join(native_path),
    ])
}

fn open_regular_candidate(path: &Path, size: u64) -> Option<File> {
    let candidate = path.canonicalize().ok()?;
    let metadata = candidate.metadata().ok()?;
    if !metadata.is_file() || metadata.len() != size {
        return None;
    }
    // Recheck the opened file since a package manager may replace it during inspection.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(candidate)
        .ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.len() == size).then_some(file)
}

fn stage_candidate(
    root: &Path,
    asset: &Asset,
    dest: &Path,
    candidate: &Path,
) -> Result<bool, RelayError> {
    let Some(mut source) = open_regular_candidate(candidate, asset.size) else {
        return Ok(false);
    };
    let mut stage = BinaryStage::new(root, asset)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let Ok(read) = source.read(&mut buffer) else {
            return Ok(false);
        };
        if read == 0 {
            return stage.publish(asset, dest);
        }
        if !stage.append(&buffer[..read])? {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::digest;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    const PAYLOAD: &[u8] = b"approved artifact";

    fn asset() -> Asset {
        Asset {
            version: "0.45.0".to_owned(),
            name: "agent-browser-linux-x64",
            size: PAYLOAD.len() as u64,
            sha256: hex::encode(digest::digest(&digest::SHA256, PAYLOAD)),
        }
    }

    #[test]
    fn adopts_a_verified_private_copy_instead_of_following_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let bad = root.join("bad-bin");
        let good = root.join("bin");
        fs::create_dir(&bad).unwrap();
        fs::create_dir(&good).unwrap();
        // This candidate has the right size and a different digest.
        fs::write(bad.join("agent-browser"), b"tampered artifact").unwrap();
        let dest = root.join("installed");
        fs::write(&dest, "previous data").unwrap();
        assert!(!reuse(root, &asset(), &dest, bad.as_os_str()).unwrap());
        assert_eq!(fs::read(&dest).unwrap(), b"previous data");

        let source = root.join("native");
        fs::write(&source, PAYLOAD).unwrap();
        symlink(&source, good.join("agent-browser")).unwrap();
        let search_path = env::join_paths([&bad, &good]).unwrap();
        assert!(reuse(root, &asset(), &dest, &search_path).unwrap());
        let installed = fs::symlink_metadata(&dest).unwrap();
        assert!(installed.is_file());
        assert_eq!(installed.mode() & 0o777, 0o700);
        assert_ne!(installed.ino(), source.metadata().unwrap().ino());
        fs::write(&source, "source updated in place").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), PAYLOAD);
    }

    #[test]
    fn finds_the_npm_native_file_from_a_recognized_launcher_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let command = bin.join("agent-browser");
        let dest = root.join("installed");
        for (directory, should_reuse) in [("unrelated", false), ("node_modules", true)] {
            let modules = root.join(directory);
            let launcher = modules.join("agent-browser/bin/agent-browser.js");
            let native = modules.join("@agent-browser/cli-linux-x64-gnu/bin/agent-browser");
            fs::create_dir_all(launcher.parent().unwrap()).unwrap();
            fs::create_dir_all(native.parent().unwrap()).unwrap();
            fs::write(&launcher, "# a wrapper that must never run").unwrap();
            fs::write(&native, PAYLOAD).unwrap();
            symlink(launcher, &command).unwrap();
            assert_eq!(
                reuse(root, &asset(), &dest, bin.as_os_str()).unwrap(),
                should_reuse
            );
            fs::remove_file(&command).unwrap();
        }
        assert_eq!(fs::read(&dest).unwrap(), PAYLOAD);
    }
}
