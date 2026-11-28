//! Opt-in update of the running executable from GitHub Releases.
//!
//! Stable builds follow the latest stable release. Development builds
//! (version `X.Y.Z-dev.N`) follow the rolling pre-release tagged `dev`.

use std::cmp::Ordering;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use ring::digest;
use serde::Deserialize;

use crate::provider::RelayError;

mod desktop;
#[path = "update_preferences.rs"]
mod preferences;
use preferences::Channel;
mod signing;
mod standalone;

const REPOSITORY: &str = "oxstack/ox";
const CURRENT: &str = crate::VERSION;
const DEV_TAG: &str = "dev";
const MAX_RELEASE_BYTES: usize = 512 * 1024;
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const USAGE: &str = "Usage: ox update [--check [--json]] [--dev|--stable]\n       ox update --stage | --apply | --status --json";

/// A release version: `X.Y.Z` or the development form `X.Y.Z-dev.N`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    dev: Option<u64>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.dev, other.dev) {
                (None, None) => Ordering::Equal,
                // A development build comes before the release it leads to.
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(left), Some(right)) => left.cmp(&right),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.core;
        write!(formatter, "{major}.{minor}.{patch}")?;
        match self.dev {
            Some(build) => write!(formatter, "-dev.{build}"),
            None => Ok(()),
        }
    }
}

impl Channel {
    /// The channel a build with this version follows by default.
    fn of(version: Version) -> Self {
        if version.dev.is_some() {
            Self::Dev
        } else {
            Self::Stable
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Self::Stable => "--stable",
            Self::Dev => "--dev",
        }
    }

    fn metadata_url(self) -> String {
        match self {
            Self::Stable => format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
            Self::Dev => {
                format!("https://api.github.com/repos/{REPOSITORY}/releases/tags/{DEV_TAG}")
            }
        }
    }

    fn download_url(self, update: &Update) -> String {
        let tag = match self {
            Self::Stable => format!("v{}", update.version),
            Self::Dev => DEV_TAG.to_owned(),
        };
        format!(
            "https://github.com/{REPOSITORY}/releases/download/{tag}/{}",
            update.asset
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Update {
        check_only: bool,
        json: bool,
        channel: Option<Channel>,
    },
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug)]
struct Update {
    version: String,
    asset: String,
    size: u64,
    sha256: [u8; 32],
}

struct Installation {
    path: PathBuf,
    file: File,
}

impl Installation {
    fn lock(path: &Path) -> Result<Self, RelayError> {
        let path = path.canonicalize()?;
        let file = File::open(&path)?;
        file.try_lock()
            .map_err(|error| format!("could not lock the installed ox executable: {error}"))?;
        Ok(Self { path, file })
    }
}

fn parse_args(args: &[OsString]) -> Result<Command, RelayError> {
    let mut check_only = false;
    let mut json = false;
    let mut channel = None;
    for arg in args {
        match arg.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help),
            Some("--json") if !json => json = true,
            Some("--check") if !check_only => check_only = true,
            Some("--dev") if channel.is_none() => channel = Some(Channel::Dev),
            Some("--stable") if channel.is_none() => channel = Some(Channel::Stable),
            _ => return Err(USAGE.into()),
        }
    }
    if json && !check_only {
        return Err(USAGE.into());
    }
    Ok(Command::Update {
        check_only,
        json,
        channel,
    })
}

fn automatic_enabled() -> bool {
    if std::env::var_os("OX_NO_BACKGROUND_UPDATE").is_some() {
        return false;
    }
    let Some(dir) = crate::desktop_state::directory() else {
        return false;
    };
    match File::open(dir.join("preferences.json")) {
        Ok(file) => serde_json::from_reader::<_, serde_json::Value>(file)
            .is_ok_and(|v| v["background_updates"].as_bool().unwrap_or(true)),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

fn fresh_stamp(path: &Path, age_limit: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < age_limit)
        && std::fs::read_to_string(path).ok().as_deref() == Some(CURRENT)
}

fn recent_check(store: &Path) -> bool {
    fresh_stamp(&store.join("last-check"), Duration::from_secs(6 * 3600))
}
fn retry_later(store: &Path) -> bool {
    fresh_stamp(&store.join("failed-check"), Duration::from_secs(3600))
}

fn record_check<T>(
    store: &Path,
    result: Result<Option<T>, RelayError>,
) -> Result<Option<T>, RelayError> {
    let failed = store.join("failed-check");
    match &result {
        Ok(candidate) => {
            let _ = std::fs::remove_file(failed);
            if candidate.is_none() {
                std::fs::write(store.join("last-check"), CURRENT)?;
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(store.join("last-check"));
            let _ = std::fs::write(failed, CURRENT);
        }
    }
    result
}

fn write_error(store: &Path, error: std::io::Error) -> String {
    if matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    ) {
        format!(
            "Cannot write to {}. Check that your account can update this installation.",
            store.display()
        )
    } else {
        format!("Could not stage the update in {}: {error}", store.display())
    }
}
fn open_lock_file(store: &Path) -> Result<File, RelayError> {
    File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.join("update.lock"))
        .map_err(|e| write_error(store, e).into())
}

pub(crate) fn store_lock(store: &Path, background: bool) -> Result<File, RelayError> {
    let file = open_lock_file(store)?;
    // Manual operations wait for the download (15 minutes) and bounded host
    // drain (10 minutes). Background pollers leave the current owner alone.
    let until = std::time::Instant::now() + Duration::from_secs(30 * 60);
    let mut announced = false;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            Err(std::fs::TryLockError::WouldBlock) if background => {
                return Err("An OX update is already running.".into());
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= until {
                    return Err("Timed out waiting for the other OX update to finish.".into());
                }
                if !announced {
                    eprintln!("Waiting for the current OX update to finish…");
                    announced = true;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Ok(file)
}

pub(crate) fn on_terminal_launch() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let store = crate::managed_install::package_store(&exe).or_else(|| standalone::store(&exe));
    let Some(store) = store else { return };
    if !automatic_enabled() {
        return;
    }
    if let Some(version) = staged_version(&store) {
        eprintln!("OX {version} is downloaded. Run ox update to install it.");
    } else if !recent_check(&store) && !retry_later(&store) {
        // The interactive client starts immediately, even on a slow or offline connection.
        if let Ok(mut child) = Command::new(exe)
            .args(["update", "--background"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn staged_version(store: &Path) -> Option<String> {
    if store.ends_with(".ox-desktop") {
        desktop::pending_version(store)
    } else {
        standalone::pending_version(store)
    }
}

pub(crate) async fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let executable = std::env::current_exe()?;
    if args.len() == 1 && args[0] == "--select-package" {
        return desktop::select_installer_package(&executable);
    }
    let store = crate::managed_install::package_store(&executable);
    if let [command, rest @ ..] = args
        && matches!(
            command.to_str(),
            Some(
                "--stage-desktop"
                    | "--apply-desktop"
                    | "--try-apply-desktop"
                    | "--stage-host-update"
                    | "--try-select-host-update"
            )
        )
    {
        let store = store
            .as_deref()
            .ok_or("This desktop installation is unmanaged. Run install.sh --desktop first.")?;
        return desktop::command(store, command, rest).await;
    }
    let options: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    if matches!(
        options.as_slice(),
        ["--stage" | "--apply" | "--background"] | ["--status", "--json"]
    ) {
        let desktop = store.is_some();
        let store = store
            .or_else(|| standalone::store(&executable))
            .ok_or("Staged updates require an installer-managed OX installation.")?;
        if options[0] == "--status" {
            let staged = staged_version(&store);
            println!(
                "{}",
                serde_json::json!({"installed": CURRENT, "staged": staged})
            );
            return Ok(0);
        }
        if options[0] == "--background" && !automatic_enabled() {
            return Ok(0);
        }
        if desktop {
            return desktop::public_command(&store, options[0]).await;
        }
        return standalone::command(&store, &executable, options[0]).await;
    }
    let (check_only, json, requested) = match parse_args(args)? {
        Command::Help => {
            println!("{USAGE}");
            println!("Update the managed desktop package or standalone CLI.");
            println!("A stable build follows stable releases. A dev build follows the dev");
            println!("pre-release that is rebuilt from every commit on main.");
            println!("  --stage    Download and verify an update without applying it");
            println!("  --apply    Apply the staged update offline");
            println!("  --status --json    Show installed and staged versions offline");
            println!("  --check    Look for a newer release without installing it");
            println!("  --json     Report installed and available versions with --check");
            println!("  --dev      Switch to the dev pre-release");
            println!("  --stable   Switch back to the latest stable release");
            return Ok(0);
        }
        Command::Update {
            check_only,
            channel,
            json,
        } => (check_only, json, channel),
    };

    let target = target_triple(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or("ox releases support Linux and macOS on x86-64 and ARM64")?;
    let running = parse_version(CURRENT).ok_or("this ox build has no update version")?;
    if let Some(store) = store.as_deref() {
        return desktop::update(store, requested, check_only, json).await;
    }
    let channel = requested.unwrap_or(Channel::of(running));
    // An explicit move to the other channel installs that channel's build even
    // when its version number is lower (for example dev back to stable).
    let switching = channel != Channel::of(running);
    if !check_only && executable.with_file_name("ox-desktop").is_file() {
        return Err(
            "To update this desktop installation, run the installer with --desktop.".into(),
        );
    }
    if !check_only
        && !switching
        && let Some(cache) = standalone::store(&executable)
    {
        return standalone::command(&cache, &executable, "update").await;
    }
    let installation = if check_only {
        None
    } else {
        Some(Installation::lock(&executable)?)
    };
    let client = release_client()?;
    let metadata_url = channel.metadata_url();
    let update = latest(&client, &metadata_url, channel, switching, CURRENT, target).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({"installed": CURRENT, "available": update.as_ref().map(|u| &u.version)})
        );
        return Ok(0);
    }
    let Some(update) = update else {
        match channel {
            Channel::Stable => println!("ox {CURRENT} is up to date."),
            Channel::Dev => println!("ox {CURRENT} is up to date on the dev channel."),
        }
        return Ok(0);
    };
    let Some(installation) = installation else {
        let command = if switching {
            format!("ox update {}", channel.flag())
        } else {
            "ox update".to_owned()
        };
        println!(
            "ox {} is available (installed: {CURRENT}). Run {command} to install it.",
            update.version
        );
        return Ok(0);
    };

    let url = channel.download_url(&update);
    println!("Updating ox {CURRENT} to {}…", update.version);
    install(&client, &url, &update, &installation).await?;
    println!(
        "Updated to ox {}. Running sessions keep their current version until restarted.",
        update.version
    );
    if switching && channel == Channel::Dev {
        println!(
            "This build follows the dev channel. Run ox update --stable to return to stable releases."
        );
    }
    Ok(0)
}

fn release_client() -> Result<Client, RelayError> {
    Ok(release_client_builder().build()?)
}

pub(crate) fn release_client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .user_agent(format!("ox/{CURRENT}"))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !release_host(attempt.url()) {
                attempt.error("unexpected GitHub release redirect")
            } else {
                attempt.follow()
            }
        }))
}

fn target_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

fn release_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

async fn latest(
    client: &Client,
    url: &str,
    channel: Channel,
    switching: bool,
    current: &str,
    target: &str,
) -> Result<Option<Update>, RelayError> {
    let release = release_metadata(client, url, channel).await?;
    select_update(release, channel, switching, current, target, None)
}

async fn release_metadata(
    client: &Client,
    url: &str,
    channel: Channel,
) -> Result<Release, RelayError> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| "Could not reach GitHub to check for OX updates. Check your connection and try again.")?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => {
            return Err(match channel {
                Channel::Stable => {
                    "no stable ox release has been published yet. Run ox update --dev to install the latest development build"
                }
                Channel::Dev => "no ox dev build has been published yet",
            }
            .into());
        }
        status => return Err(format!("GitHub returned HTTP {status} while checking for OX updates. Try again in a few minutes.").into()),
    }
    let body = crate::read_upstream(response, MAX_RELEASE_BYTES)
        .await
        .map_err(|_| "GitHub release metadata was too large or unreadable")?;
    let release: Release =
        serde_json::from_slice(&body).map_err(|_| "invalid GitHub release metadata")?;
    Ok(release)
}

fn select_update(
    release: Release,
    channel: Channel,
    switching: bool,
    current: &str,
    target: &str,
    archive: Option<&str>,
) -> Result<Option<Update>, RelayError> {
    let installed = parse_version(current).ok_or("this ox build has no update version")?;
    if release.draft {
        return Err("GitHub did not return a published ox release".into());
    }
    let version = match channel {
        Channel::Stable => {
            if release.prerelease {
                return Err("GitHub did not return a stable ox release".into());
            }
            release
                .tag_name
                .strip_prefix('v')
                .ok_or("invalid ox release tag")?
                .to_owned()
        }
        Channel::Dev => {
            if release.tag_name != DEV_TAG || !release.prerelease {
                return Err("GitHub did not return the ox dev pre-release".into());
            }
            dev_version(&release.assets, target)?
        }
    };
    let newer = parse_version(&version)
        .filter(|parsed| Channel::of(*parsed) == channel)
        .ok_or("invalid ox release version")?;
    let wanted = if switching {
        newer != installed
    } else {
        newer > installed
    };
    if !wanted {
        return Ok(None);
    }

    let name = archive
        .map(str::to_owned)
        .unwrap_or_else(|| format!("ox-{version}-{target}"));
    let max_bytes = if archive.is_some() {
        256 * 1024 * 1024
    } else {
        MAX_BINARY_BYTES
    };
    let mut matching = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == name);
    let asset = matching
        .next()
        .ok_or_else(|| format!("release {version} has no ox binary for {target}"))?;
    if matching.next().is_some() || !(1..=max_bytes).contains(&asset.size) {
        return Err("invalid ox release asset metadata".into());
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or("GitHub did not provide a SHA-256 digest for this ox binary")?;
    let mut sha256 = [0; 32];
    hex::decode_to_slice(digest, &mut sha256)
        .map_err(|_| "GitHub supplied an invalid SHA-256 digest for this ox binary")?;
    Ok(Some(Update {
        version,
        asset: name,
        size: asset.size,
        sha256,
    }))
}

/// The dev release is always tagged `dev`, so its version comes from the one
/// asset named `ox-<X.Y.Z-dev.N>-<target>`.
fn dev_version(assets: &[Asset], target: &str) -> Result<String, RelayError> {
    let suffix = format!("-{target}");
    let mut versions = assets.iter().filter_map(|asset| {
        let text = asset.name.strip_prefix("ox-")?.strip_suffix(&suffix)?;
        parse_version(text)
            .filter(|version| version.dev.is_some())
            .map(|_| text.to_owned())
    });
    let version = versions
        .next()
        .ok_or_else(|| format!("the dev release has no ox binary for {target}"))?;
    if versions.next().is_some() {
        return Err("invalid ox release asset metadata".into());
    }
    Ok(version)
}

fn parse_version(text: &str) -> Option<Version> {
    let (core_text, dev) = match text.split_once("-dev.") {
        Some((core_text, build)) => (core_text, Some(parse_number(build)?)),
        None => (text, None),
    };
    let mut components = core_text.split('.');
    let mut core = [0; 3];
    for number in &mut core {
        *number = parse_number(components.next()?)?;
    }
    components.next().is_none().then_some(Version { core, dev })
}

/// A decimal number without a sign or leading zeros.
fn parse_number(part: &str) -> Option<u64> {
    if part.is_empty()
        || (part.len() > 1 && part.starts_with('0'))
        || !part.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    part.parse().ok()
}

async fn download_verified(
    client: &Client,
    url: &str,
    update: &Update,
    file: &mut File,
) -> Result<(), RelayError> {
    use std::io::Write;
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(15 * 60))
        .send()
        .await
        .map_err(|_| "could not download the ox release asset from GitHub")?;
    if response.status() != StatusCode::OK {
        return Err(format!("ox download returned HTTP {}", response.status()).into());
    }
    let mut sha256 = digest::Context::new(&digest::SHA256);
    let mut received = 0_u64;
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        format!(
            "The OX {} download was interrupted. Check your connection and retry.",
            update.version
        )
    })? {
        received += chunk.len() as u64;
        if received > update.size {
            return Err("ox download exceeds its published size".into());
        }
        file.write_all(&chunk)?;
        sha256.update(&chunk);
    }
    if received != update.size || sha256.finish().as_ref() != update.sha256 {
        return Err(
            format!("The OX {} download did not match its published size and SHA-256 digest. It was discarded.", update.version).into(),
        );
    }
    Ok(())
}

#[cfg(unix)]
async fn install(
    client: &Client,
    url: &str,
    update: &Update,
    installation: &Installation,
) -> Result<(), RelayError> {
    let parent = installation
        .path
        .parent()
        .ok_or("Missing CLI install directory.")?;
    let mut stage = tempfile::NamedTempFile::new_in(parent)?;
    download_verified(client, url, update, stage.as_file_mut()).await?;
    replace_cli(stage.path(), installation)
}

fn replace_cli(stage: &Path, installation: &Installation) -> Result<(), RelayError> {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // Every path that replaces the CLI ends here: staged background updates,
    // a direct `ox update` and a channel switch.
    if cfg!(target_os = "macos") {
        signing::require(signing::running_release(), "The downloaded ox", &[stage])?;
    }
    let executable = &installation.path;
    let metadata = installation.file.metadata()?;
    let file = File::open(stage)?;
    file.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777))?;
    file.sync_all()?;
    let current = fs::symlink_metadata(executable)?;
    if !current.is_file() || current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(
            "the installed ox executable changed during the update. Run ox update again".into(),
        );
    }
    fs::rename(stage, executable)?;
    let parent = executable
        .parent()
        .ok_or("Missing CLI install directory.")?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(not(unix))]
async fn install(_: &Client, _: &str, _: &Update, _: &Installation) -> Result<(), RelayError> {
    Err("ox updates are supported on Linux and macOS".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tests::TempHome;
    use http_body_util::Full;
    use hyper::Response;
    use hyper::body::Bytes;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use serde_json::{Value, json};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::TcpListener;

    const TARGET: &str = "aarch64-apple-darwin";
    const DOWNLOAD: &[u8] = b"new release binary fixture";

    fn version(text: &str) -> Version {
        parse_version(text).unwrap_or_else(|| panic!("{text} should parse"))
    }

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn asset(name: &str, binary: &[u8]) -> Value {
        json!({
            "name": name,
            "size": binary.len(),
            "digest": format!("sha256:{}", hex::encode(digest::digest(&digest::SHA256, binary)))
        })
    }

    fn stable_release(binary: &[u8]) -> Value {
        json!({
            "tag_name": "v0.2.0",
            "draft": false,
            "prerelease": false,
            "assets": [asset(&format!("ox-0.2.0-{TARGET}"), binary)]
        })
    }

    fn dev_release(binary: &[u8]) -> Value {
        json!({
            "tag_name": "dev",
            "draft": false,
            "prerelease": true,
            "assets": [
                asset("install.sh", b"#!/bin/sh"),
                asset(&format!("ox-{TARGET}.tar.gz"), b"alias archive"),
                asset(&format!("ox-{TARGET}.tar.gz.sha256"), b"alias checksum"),
                asset(&format!("ox-0.2.1-dev.7-{TARGET}"), binary),
                asset(&format!("ox-0.2.1-dev.7-{TARGET}.sha256"), b"checksum"),
                asset(&format!("ox-0.2.1-dev.7-{TARGET}.tar.gz"), b"archive"),
                asset("ox-0.2.1-dev.7-x86_64-unknown-linux-gnu", b"another platform"),
            ]
        })
    }

    fn select(
        release: Value,
        channel: Channel,
        switching: bool,
        current: &str,
    ) -> Result<Option<Update>, RelayError> {
        let release: Release = serde_json::from_value(release).unwrap();
        select_update(release, channel, switching, current, TARGET, None)
    }

    #[test]
    fn failed_checks_back_off_and_recover() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        record_check::<()>(dir, Ok(None)).unwrap();
        assert!(recent_check(dir));
        assert!(record_check::<()>(dir, Err("offline".into())).is_err());
        assert!(!recent_check(dir));
        assert!(retry_later(dir));
        let stamp = File::open(dir.join("failed-check")).unwrap();
        stamp
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(3601))
            .unwrap();
        assert!(!retry_later(dir));
        record_check(dir, Ok(Some(()))).unwrap();
        assert!(!dir.join("failed-check").exists());
    }

    #[test]
    fn manual_update_waits_while_background_checks_yield() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let owner = store_lock(dir, true).unwrap();
        assert!(store_lock(dir, true).is_err());
        std::thread::scope(|scope| {
            let (sent, received) = std::sync::mpsc::channel();
            let waiting = scope.spawn(move || {
                sent.send(()).unwrap();
                store_lock(dir, false).unwrap()
            });
            received.recv().unwrap();
            assert!(!waiting.is_finished());
            drop(owner);
            let _lock = waiting.join().unwrap();
        });
    }

    #[test]
    fn compares_numeric_stable_versions_and_limits_redirect_origins() {
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        for invalid in [
            "1",
            "1.0.0-beta",
            "v1.0.0",
            "1.0.0+extra",
            "01.0.0",
            "1.0.0.0",
        ] {
            assert_eq!(parse_version(invalid), None, "{invalid}");
        }
        for url in [
            "http://github.com/resource",
            "https://github.com:8080/resource",
            "https://github.com.example.org/resource",
            "https://name@release-assets.githubusercontent.com/resource",
        ] {
            assert!(!release_host(&Url::parse(url).unwrap()));
        }
        assert!(release_host(
            &Url::parse("https://release-assets.githubusercontent.com/resource").unwrap()
        ));
    }

    #[test]
    fn parses_and_orders_stable_and_dev_versions() {
        assert_eq!(
            version("0.1.1-dev.38"),
            Version {
                core: [0, 1, 1],
                dev: Some(38)
            }
        );
        assert_eq!(
            version("12.0.3"),
            Version {
                core: [12, 0, 3],
                dev: None
            }
        );
        // Ascending order: every entry is smaller than the next one.
        let ordered = [
            "0.0.1-dev.2",
            "0.0.1-dev.10",
            "0.0.1",
            "0.0.2-dev.1",
            "0.1.0",
            "0.1.1-dev.1",
            "0.10.0",
            "1.0.0-dev.999",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(version(pair[0]) < version(pair[1]), "{pair:?}");
            assert!(version(pair[1]) > version(pair[0]), "{pair:?}");
        }
        for text in ordered {
            assert_eq!(version(text).to_string(), text);
            assert_eq!(version(text), version(text));
        }
        for invalid in [
            "",
            "1.0.0-dev",
            "1.0.0-dev.",
            "1.0.0-dev.01",
            "1.0.0-dev.x",
            "1.0.0-dev.1.2",
            "1.0.0-dev.1-dev.2",
            "1.0.0-rc.1",
            "1.0-dev.1",
            "-dev.1",
            "1.0.0-DEV.1",
            "1.0.0 ",
        ] {
            assert_eq!(parse_version(invalid), None, "{invalid:?}");
        }
        assert_eq!(Channel::of(version("0.1.0")), Channel::Stable);
        assert_eq!(Channel::of(version("0.1.1-dev.4")), Channel::Dev);
        assert!(
            parse_version(CURRENT).is_some(),
            "this build's version must be updatable: {CURRENT}"
        );
    }

    #[test]
    fn parses_update_arguments_and_builds_channel_urls() {
        let update = |check_only, channel| Command::Update {
            check_only,
            json: false,
            channel,
        };
        assert_eq!(parse_args(&[]).unwrap(), update(false, None));
        assert_eq!(
            parse_args(&arguments(&["--check"])).unwrap(),
            update(true, None)
        );
        assert_eq!(
            parse_args(&arguments(&["--dev"])).unwrap(),
            update(false, Some(Channel::Dev))
        );
        assert_eq!(
            parse_args(&arguments(&["--stable", "--check"])).unwrap(),
            update(true, Some(Channel::Stable))
        );
        assert_eq!(
            parse_args(&arguments(&["--check", "--dev"])).unwrap(),
            update(true, Some(Channel::Dev))
        );
        assert_eq!(
            parse_args(&arguments(&["--json", "--check"])).unwrap(),
            Command::Update {
                check_only: true,
                json: true,
                channel: None
            }
        );
        for help in [&["-h"][..], &["--help"], &["--dev", "--help"]] {
            assert_eq!(parse_args(&arguments(help)).unwrap(), Command::Help);
        }
        for invalid in [
            &["--dev", "--stable"][..],
            &["--dev", "--dev"],
            &["--check", "--check"],
            &["--beta"],
            &["--json"],
            &["--json", "--json", "--check"],
            &["dev"],
        ] {
            assert_eq!(
                parse_args(&arguments(invalid)).unwrap_err().to_string(),
                USAGE,
                "{invalid:?}"
            );
        }

        assert_eq!(
            Channel::Stable.metadata_url(),
            "https://api.github.com/repos/oxstack/ox/releases/latest"
        );
        assert_eq!(
            Channel::Dev.metadata_url(),
            "https://api.github.com/repos/oxstack/ox/releases/tags/dev"
        );
        let update = Update {
            version: "0.2.1-dev.7".to_owned(),
            asset: format!("ox-0.2.1-dev.7-{TARGET}"),
            size: 1,
            sha256: [0; 32],
        };
        assert_eq!(
            Channel::Dev.download_url(&update),
            format!("https://github.com/oxstack/ox/releases/download/dev/ox-0.2.1-dev.7-{TARGET}")
        );
        let update = Update {
            version: "0.2.0".to_owned(),
            asset: format!("ox-0.2.0-{TARGET}"),
            size: 1,
            sha256: [0; 32],
        };
        let url = Channel::Stable.download_url(&update);
        assert_eq!(
            url,
            format!("https://github.com/oxstack/ox/releases/download/v0.2.0/ox-0.2.0-{TARGET}")
        );
        assert!(release_host(&Url::parse(&url).unwrap()));
        assert!(release_host(
            &Url::parse(&Channel::Dev.metadata_url()).unwrap()
        ));
    }

    #[test]
    fn selects_updates_by_channel() {
        // Dev channel: the version comes from the asset name, not the tag.
        let update = select(dev_release(DOWNLOAD), Channel::Dev, false, "0.2.1-dev.6")
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "0.2.1-dev.7");
        assert_eq!(update.asset, format!("ox-0.2.1-dev.7-{TARGET}"));
        assert_eq!(update.size, DOWNLOAD.len() as u64);
        for current in ["0.2.1-dev.7", "0.2.1-dev.8", "0.2.1", "0.3.0-dev.1"] {
            assert!(
                select(dev_release(DOWNLOAD), Channel::Dev, false, current)
                    .unwrap()
                    .is_none(),
                "{current}"
            );
        }
        // A stable build that asks for --dev switches even to a lower version.
        for current in ["0.2.0", "0.9.0"] {
            let update = select(dev_release(DOWNLOAD), Channel::Dev, true, current)
                .unwrap()
                .unwrap();
            assert_eq!(update.version, "0.2.1-dev.7", "{current}");
        }
        // A dev build that asks for --stable goes back to the stable release.
        let update = select(
            stable_release(DOWNLOAD),
            Channel::Stable,
            true,
            "0.2.1-dev.7",
        )
        .unwrap()
        .unwrap();
        assert_eq!(update.version, "0.2.0");
        // Without a switch a stable release is only installed when it is newer.
        assert!(
            select(stable_release(DOWNLOAD), Channel::Stable, false, "0.2.0")
                .unwrap()
                .is_none()
        );
        assert!(
            select(stable_release(DOWNLOAD), Channel::Stable, false, "0.1.9")
                .unwrap()
                .is_some()
        );

        // Each channel refuses the other channel's release.
        assert!(select(dev_release(DOWNLOAD), Channel::Stable, false, "0.1.0").is_err());
        assert!(select(stable_release(DOWNLOAD), Channel::Dev, true, "0.1.0").is_err());
        let mut release = dev_release(DOWNLOAD);
        release["prerelease"] = json!(false);
        assert!(select(release, Channel::Dev, false, "0.2.1-dev.6").is_err());
        let mut release = dev_release(DOWNLOAD);
        release["draft"] = json!(true);
        assert!(select(release, Channel::Dev, false, "0.2.1-dev.6").is_err());
        let mut release = stable_release(DOWNLOAD);
        release["tag_name"] = json!("v0.2.1-dev.7");
        assert!(select(release, Channel::Stable, false, "0.1.0").is_err());
        // Two dev versions for one platform are ambiguous.
        let mut release = dev_release(DOWNLOAD);
        release["assets"]
            .as_array_mut()
            .unwrap()
            .push(asset(&format!("ox-0.2.1-dev.8-{TARGET}"), DOWNLOAD));
        assert!(select(release, Channel::Dev, false, "0.2.1-dev.6").is_err());
        // No dev binary for this platform.
        let mut release = dev_release(DOWNLOAD);
        release["assets"] = json!([asset("ox-0.2.1-dev.7-x86_64-unknown-linux-gnu", DOWNLOAD)]);
        assert!(
            select(release, Channel::Dev, false, "0.2.1-dev.6")
                .unwrap_err()
                .to_string()
                .contains("no ox binary")
        );
    }

    #[tokio::test]
    async fn downloads_and_replaces_only_a_verified_release() {
        let home = TempHome::new();
        let executable = home.0.join("ox");
        fs::write(&executable, "original").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o710)).unwrap();
        let installation = Installation::lock(&executable).unwrap();
        assert!(Installation::lock(&executable).is_err());

        let (base, server) = mock_release(DOWNLOAD).await;
        let client = Client::builder().no_proxy().build().unwrap();
        let metadata = format!("{base}/latest");
        assert!(
            latest(&client, &metadata, Channel::Stable, false, "0.2.0", TARGET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            latest(&client, &metadata, Channel::Stable, false, "1.0.0", TARGET)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            latest(
                &client,
                &metadata,
                Channel::Stable,
                false,
                "0.1.0",
                "x86_64-apple-darwin"
            )
            .await
            .is_err()
        );
        let update = latest(&client, &metadata, Channel::Stable, false, "0.1.0", TARGET)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.asset, format!("ox-0.2.0-{TARGET}"));
        install(&client, &format!("{base}/binary"), &update, &installation)
            .await
            .unwrap();
        assert_eq!(fs::read(&executable).unwrap(), DOWNLOAD);
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o710
        );
        assert_eq!(fs::read_dir(&home.0).unwrap().count(), 1);
        // The old lock cannot replace a file already installed by another writer.
        assert!(
            install(&client, &format!("{base}/binary"), &update, &installation)
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn dev_channel_installs_the_dev_build_and_missing_releases_explain_themselves() {
        let home = TempHome::new();
        let executable = home.0.join("ox");
        fs::write(&executable, "original").unwrap();
        let installation = Installation::lock(&executable).unwrap();
        let (base, server) = mock_release(DOWNLOAD).await;
        let client = Client::builder().no_proxy().build().unwrap();

        let metadata = format!("{base}/dev");
        assert!(
            latest(
                &client,
                &metadata,
                Channel::Dev,
                false,
                "0.2.1-dev.7",
                TARGET
            )
            .await
            .unwrap()
            .is_none()
        );
        let update = latest(
            &client,
            &metadata,
            Channel::Dev,
            false,
            "0.2.1-dev.3",
            TARGET,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(update.version, "0.2.1-dev.7");
        install(&client, &format!("{base}/binary"), &update, &installation)
            .await
            .unwrap();
        assert_eq!(fs::read(&executable).unwrap(), DOWNLOAD);

        let missing = format!("{base}/missing");
        let error = latest(&client, &missing, Channel::Stable, false, "0.1.0", TARGET)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("ox update --dev"), "{error}");
        let error = latest(&client, &missing, Channel::Dev, true, "0.1.0", TARGET)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no ox dev build"), "{error}");
        server.abort();
    }

    #[tokio::test]
    async fn bad_digest_and_wrong_size_leave_the_installed_file_intact() {
        let home = TempHome::new();
        let executable = home.0.join("ox");
        fs::write(&executable, "original").unwrap();
        let installation = Installation::lock(&executable).unwrap();
        let (base, server) = mock_release(DOWNLOAD).await;
        let client = Client::builder().no_proxy().build().unwrap();
        let mut update = latest(
            &client,
            &format!("{base}/latest"),
            Channel::Stable,
            false,
            "0.1.0",
            TARGET,
        )
        .await
        .unwrap()
        .unwrap();

        update.sha256 = [0; 32];
        let url = format!("{base}/binary");
        assert!(
            install(&client, &url, &update, &installation)
                .await
                .unwrap_err()
                .to_string()
                .contains("SHA-256")
        );
        update.size -= 1;
        assert!(
            install(&client, &url, &update, &installation)
                .await
                .unwrap_err()
                .to_string()
                .contains("published size")
        );
        assert_eq!(fs::read(&executable).unwrap(), b"original");
        assert_eq!(fs::read_dir(&home.0).unwrap().count(), 1);
        server.abort();
    }

    /// Serves `/latest` (stable metadata), `/dev` (dev metadata), `/missing`
    /// (HTTP 404) and the binary on every other path.
    pub(super) async fn mock_release(
        binary: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let stable = Bytes::from(stable_release(binary).to_string());
        let dev = Bytes::from(dev_release(binary).to_string());
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let stable = stable.clone();
                let dev = dev.clone();
                tokio::spawn(async move {
                    let service =
                        service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                            let (status, body) = match request.uri().path() {
                                "/latest" => (StatusCode::OK, stable.clone()),
                                "/dev" => (StatusCode::OK, dev.clone()),
                                "/missing" => (StatusCode::NOT_FOUND, Bytes::new()),
                                _ => (StatusCode::OK, Bytes::from_static(binary)),
                            };
                            async move {
                                let mut response = Response::new(Full::new(body));
                                *response.status_mut() = status;
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        (format!("http://{address}"), server)
    }
}
