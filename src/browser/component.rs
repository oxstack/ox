//! Select, verify, and activate the optional upstream native component.

use super::{
    DOWNLOAD_ENV, INSTALL_HINT, LOCAL_HOSTS, child_command, exit_code, private_executable,
    read_private_file, write_private_file,
};
use crate::provider::RelayError;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use ring::digest;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
#[path = "../../tests/common/executable.rs"]
mod executable;

const RELEASES_URL: &str = "https://api.github.com/repos/vercel-labs/agent-browser/releases";
const DOWNLOAD_URL: &str = "https://github.com/vercel-labs/agent-browser/releases/download";
const RELEASE_PAGE_SIZE: usize = 30;
const MAX_RELEASE_PAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MIN_RELEASE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const BINARY_PREFIX: &str = "agent-browser-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Component(String);

impl Component {
    pub(super) fn parse(id: &str) -> Result<Self, RelayError> {
        let (version, checksum) = id.split_once('-').unwrap_or((id, ""));
        if parse_version(version).is_some() && (id == version || valid_sha256(checksum)) {
            return Ok(Self(id.to_owned()));
        }
        Err(format!("The OX Browser component record is invalid.\n  {INSTALL_HINT}").into())
    }

    pub(super) fn id(&self) -> &str {
        &self.0
    }

    pub(super) fn path(&self, root: &Path) -> PathBuf {
        root.join(format!("{BINARY_PREFIX}{}", self.id()))
    }
}

pub(super) struct Installed {
    pub(super) current: Component,
    pub(super) legacy: Option<Component>,
}

impl Installed {
    pub(super) fn load(root: &Path) -> Result<Option<Self>, RelayError> {
        if let Some(record) = read_private_file(&root.join("current"))? {
            let (current, legacy) = record
                .split_once('\n')
                .ok_or("The OX Browser installation record is invalid.")?;
            return Ok(Some(Self {
                current: Component::parse(current)?,
                legacy: (!legacy.is_empty())
                    .then(|| Component::parse(legacy))
                    .transpose()?,
            }));
        }
        // Migrate the previous layout. Only the old installer used bare versions.
        let legacy = legacy_component(root)?;
        Ok(legacy.map(|current| Self {
            legacy: Some(current.clone()),
            current,
        }))
    }
}

fn legacy_component(root: &Path) -> Result<Option<Component>, RelayError> {
    let mut chosen = None;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix(BINARY_PREFIX))
        else {
            continue;
        };
        let Some(version) = parse_version(id) else {
            continue;
        };
        if chosen
            .as_ref()
            .is_none_or(|(previous, _)| version > *previous)
            && private_executable(&entry.path())?
        {
            chosen = Some((version, Component::parse(id)?));
        }
    }
    Ok(chosen.map(|(_, component)| component))
}

fn parse_version(input: &str) -> Option<[u32; 3]> {
    let mut parts = input.split('.');
    let mut version = [0; 3];
    for number in &mut version {
        let part = parts.next()?;
        if (part.len() > 1 && part.starts_with('0')) || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *number = part.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

fn valid_sha256(input: &str) -> bool {
    input.len() == 64
        && input
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Deserialize)]
struct UpstreamRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
    assets: Vec<UpstreamAsset>,
}

#[derive(Deserialize)]
struct UpstreamAsset {
    name: String,
    size: u64,
    digest: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct Asset {
    version: String,
    name: &'static str,
    size: u64,
    sha256: String,
}

impl Asset {
    fn component(&self) -> Component {
        Component(format!("{}-{}", self.version, self.sha256))
    }
}

fn platform_asset() -> Result<&'static str, RelayError> {
    let name = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "agent-browser-linux-x64",
        ("linux", "aarch64") => "agent-browser-linux-arm64",
        ("macos", "x86_64") => "agent-browser-darwin-x64",
        ("macos", "aarch64") => "agent-browser-darwin-arm64",
        _ => return Err("OX Browser is available on x86-64 and ARM64 Linux and macOS.".into()),
    };
    Ok(name)
}

fn select_release(
    releases: Vec<UpstreamRelease>,
    platform: &'static str,
    now: DateTime<Utc>,
    allow_fresh: bool,
) -> Result<Asset, RelayError> {
    let cutoff = if allow_fresh {
        now
    } else {
        now - MIN_RELEASE_AGE
    };
    let mut eligible: Vec<_> = releases
        .into_iter()
        .filter_map(|release| {
            let published = release.published_at?;
            let version = release.tag_name.strip_prefix('v')?;
            let order = parse_version(version)?;
            (!release.draft && !release.prerelease && published <= cutoff)
                .then_some((published, order, release))
        })
        .collect();
    eligible.sort_unstable_by_key(|(published, version, _)| (*published, *version));
    for (_, _, release) in eligible.into_iter().rev() {
        let version = release.tag_name[1..].to_owned();
        let mut matches = release
            .assets
            .into_iter()
            .filter(|asset| asset.name == platform);
        let Some(asset) = matches.next() else {
            continue;
        };
        if matches.next().is_some() {
            return Err(
                format!("agent-browser {version} has multiple {platform} binaries.").into(),
            );
        }
        if asset.created_at > cutoff || asset.updated_at > cutoff {
            continue;
        }
        let sha256 = asset
            .digest
            .as_deref()
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .filter(|digest| valid_sha256(digest));
        if let Some(sha256) = sha256
            && (1..=MAX_BINARY_BYTES).contains(&asset.size)
        {
            return Ok(Asset {
                version,
                name: platform,
                size: asset.size,
                sha256: sha256.to_owned(),
            });
        }
        return Err(format!(
            "agent-browser {version} has invalid {platform} verification metadata."
        )
        .into());
    }
    if allow_fresh {
        return Err(
            "No published stable agent-browser release has a matching platform binary.".into(),
        );
    }
    Err("No stable agent-browser release and matching platform binary have been published for at least 24 hours.".into())
}

fn download_client() -> Result<reqwest::Client, RelayError> {
    let mut builder = crate::update::release_client_builder();
    // Honor the system CA bundle on managed networks.
    if let Some(pem) = env::var_os("SSL_CERT_FILE").filter(|value| !value.is_empty()) {
        for certificate in reqwest::Certificate::from_pem_bundle(&fs::read(pem)?)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder.build()?)
}

async fn resolve_asset(client: &reqwest::Client, allow_fresh: bool) -> Result<Asset, RelayError> {
    let platform = platform_asset()?;
    let mut releases = Vec::new();
    // GitHub lists releases by creation order. Publication order can differ.
    for page in 1.. {
        let response = client
            .get(format!(
                "{RELEASES_URL}?per_page={RELEASE_PAGE_SIZE}&page={page}"
            ))
            .send()
            .await?
            .error_for_status()?;
        let bytes = crate::read_upstream(response, MAX_RELEASE_PAGE_BYTES)
            .await
            .map_err(|_| "Could not read the GitHub agent-browser release catalog.")?;
        let batch: Vec<UpstreamRelease> = serde_json::from_slice(&bytes)?;
        let last = batch.len() < RELEASE_PAGE_SIZE;
        releases.extend(batch);
        if last {
            break;
        }
    }
    select_release(
        releases,
        platform,
        std::time::SystemTime::now().into(),
        allow_fresh,
    )
}

fn verify_binary(path: &Path, asset: &Asset) -> io::Result<bool> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if file.metadata()?.len() != asset.size {
        return Ok(false);
    }
    let mut sha = digest::Context::new(&digest::SHA256);
    let mut buf = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        sha.update(&buf[..read]);
    }
    Ok(hex::encode(sha.finish()) == asset.sha256)
}

async fn download(
    client: &reqwest::Client,
    root: &Path,
    asset: &Asset,
    dest: &Path,
) -> Result<(), RelayError> {
    println!("Downloading agent-browser {}…", asset.version);
    let url = format!("{DOWNLOAD_URL}/v{}/{}", asset.version, asset.name);
    let mut response = client.get(url).send().await?;
    if response.status() != StatusCode::OK {
        return Err(format!(
            "agent-browser download returned HTTP {}.",
            response.status()
        )
        .into());
    }
    let mut stage = tempfile::NamedTempFile::new_in(root)?;
    let mut sha = digest::Context::new(&digest::SHA256);
    let mut bytes = 0;
    while let Some(chunk) = response.chunk().await? {
        bytes += chunk.len() as u64;
        if bytes > asset.size {
            return Err("agent-browser download exceeds its published size.".into());
        }
        stage.write_all(&chunk)?;
        sha.update(&chunk);
    }
    if bytes != asset.size || hex::encode(sha.finish()) != asset.sha256 {
        return Err("agent-browser download failed SHA-256 verification.".into());
    }
    stage
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    stage.as_file().sync_all()?;
    stage.persist(dest)?;
    Ok(())
}

async fn check_output(command: std::process::Command) -> Result<std::process::Output, RelayError> {
    let mut command = tokio::process::Command::from(command);
    let output = tokio::time::timeout(Duration::from_secs(10), command.kill_on_drop(true).output())
        .await??;
    if !output.status.success() {
        return Err(format!(
            "agent-browser compatibility check exited with status {}.",
            output.status
        )
        .into());
    }
    Ok(output)
}

async fn check_contract(
    root: &Path,
    component: &Component,
    version: &str,
) -> Result<(), RelayError> {
    let mut command = child_command(root, component);
    command.arg("--version");
    let output = check_output(command).await?;
    if std::str::from_utf8(&output.stdout)?.trim() != format!("agent-browser {version}") {
        return Err(
            format!("The downloaded agent-browser does not report version {version}.").into(),
        );
    }

    let mut command = child_command(root, component);
    command.arg("--help");
    let output = check_output(command).await?;
    let help = std::str::from_utf8(&output.stdout)?;
    for flag in [
        "--session",
        "--executable-path",
        "--content-boundaries",
        "--allowed-domains",
        "--idle-timeout",
    ] {
        if !help.split_whitespace().any(|word| word == flag) {
            return Err(format!(
                "agent-browser {version} does not support the required {flag} flag."
            )
            .into());
        }
    }

    // This probe starts an isolated daemon and checks the stream response shape without launching Chromium.
    let sockets = tempfile::Builder::new()
        .prefix("ox-bc-")
        .tempdir_in("/tmp")?;
    fs::set_permissions(sockets.path(), fs::Permissions::from_mode(0o700))?;
    let probe = || {
        let mut command = child_command(root, component);
        command
            .env("HOME", sockets.path())
            .current_dir(sockets.path())
            .env("AGENT_BROWSER_SOCKET_DIR", sockets.path())
            .args([
                "--session",
                "probe",
                "--allowed-domains",
                LOCAL_HOSTS,
                "--idle-timeout",
                "10s",
                "--content-boundaries",
            ]);
        command
    };
    let mut command = probe();
    command.args(["stream", "status", "--json"]);
    let status = check_output(command).await;
    let mut command = probe();
    command.arg("close");
    check_output(command).await?;
    let state = super::stream_state(&status?.stdout)?;
    if state.connected
        || state.port == 0
        || state
            .lifecycle
            .is_none_or(|lifecycle| lifecycle.effective_launch.browser_launched)
    {
        return Err(format!(
            "agent-browser {version} returned an incompatible browser stream status."
        )
        .into());
    }
    Ok(())
}

pub(super) async fn install(root: &Path, allow_fresh: bool) -> Result<u8, RelayError> {
    let _lock = crate::update::store_lock(root, false)?;
    let browser = super::system_browser()?;
    if browser.is_none() && cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        return Err(
            "Install Chromium from your system package manager, then run ox browser install again."
                .into(),
        );
    }
    let legacy = legacy_component(root)?;
    let client = download_client()?;
    println!("Checking for an eligible agent-browser release…");
    let asset = resolve_asset(&client, allow_fresh)
        .await
        .map_err(|error| format!("Could not select an agent-browser release: {error}"))?;
    let component = asset.component();
    let dest = component.path(root);
    if !private_executable(&dest)? || !verify_binary(&dest, &asset)? {
        download(&client, root, &asset, &dest).await?;
    }
    check_contract(root, &component, &asset.version).await?;

    let browser = if let Some(browser) = browser {
        browser.display().to_string()
    } else {
        println!("Installing Chrome for Testing…");
        let mut command = child_command(root, &component);
        for key in DOWNLOAD_ENV {
            if let Some(value) = env::var_os(key) {
                command.env(key, value);
            }
        }
        let status = command.arg("install").status()?;
        if !status.success() {
            return Ok(exit_code(status));
        }
        "Chrome for Testing".to_owned()
    };
    activate(root, &component, legacy.as_ref())?;
    println!("agent-browser {} is ready with {browser}.", asset.version);
    Ok(0)
}

fn activate(
    root: &Path,
    component: &Component,
    legacy: Option<&Component>,
) -> Result<(), RelayError> {
    let legacy = legacy.map_or("", Component::id);
    write_private_file(
        &root.join("current"),
        &format!("{}\n{legacy}", component.id()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{active_component, private_dir, session_name, socket_dir};
    use std::os::unix::net::UnixListener;

    const PLATFORM: &str = "agent-browser-linux-x64";
    const HASH: &str = "1ce4572138ddacf54f7b7834f96aef9b61cc975676daa26fef2fbdf5c7a2d4bf";

    fn now() -> DateTime<Utc> {
        "2026-11-28T12:00:00Z".parse().unwrap()
    }

    fn fixture(tag: &str, age_minutes: u64) -> UpstreamRelease {
        let published = now() - Duration::from_secs(age_minutes * 60);
        UpstreamRelease {
            tag_name: tag.to_owned(),
            draft: false,
            prerelease: false,
            published_at: Some(published),
            assets: vec![UpstreamAsset {
                name: PLATFORM.to_owned(),
                size: 16,
                digest: Some(format!("sha256:{HASH}")),
                created_at: published,
                updated_at: published,
            }],
        }
    }

    #[test]
    fn resolves_newest_stable_publication_for_platform_after_exact_cutoff() {
        let mut draft = fixture("v0.55.0", 1440);
        draft.draft = true;
        let mut prerelease = fixture("v0.54.0", 1440);
        prerelease.prerelease = true;
        let mut fresh_asset = fixture("v0.52.0", 1440);
        fresh_asset.assets[0].updated_at = now() - Duration::from_secs(1439 * 60);
        let mut fresh_upload = fixture("v0.51.0", 1440);
        fresh_upload.assets[0].created_at = now() - Duration::from_secs(1439 * 60);
        let mut wrong_platform = fixture("v0.50.0", 1440);
        wrong_platform.assets[0].name = "agent-browser-linux-x64-musl".to_owned();

        let releases = vec![
            fixture("v0.49.0", 1500), // A higher version published earlier.
            fixture("v0.59.0", 1439),
            draft,
            fixture("v0.53.0-rc.1", 1440),
            prerelease,
            fresh_asset,
            fresh_upload,
            wrong_platform,
            fixture("v0.46.0", 1440),
            fixture("v0.45.0", 2800),
        ];
        let selected = select_release(releases, PLATFORM, now(), false).unwrap();
        assert_eq!(selected.version, "0.46.0");
        assert_eq!(selected.sha256, HASH);
        assert_eq!(selected.size, 16);
    }

    #[test]
    fn allow_fresh_skips_only_the_release_and_asset_wait() {
        for replaced_asset in [false, true] {
            for (allow_fresh, expected) in [(false, "0.45.0"), (true, "0.46.0")] {
                let mut candidate = fixture("v0.46.0", if replaced_asset { 1440 } else { 1439 });
                if replaced_asset {
                    candidate.assets[0].updated_at = now() - Duration::from_secs(60);
                }
                let mut draft = fixture("v0.49.0", 5);
                draft.draft = true;
                let mut prerelease = fixture("v0.48.0", 10);
                prerelease.prerelease = true;
                let mut wrong_platform = fixture("v0.47.0", 15);
                wrong_platform.assets[0].name = "agent-browser-linux-x64-musl".to_owned();
                let older = fixture("v0.45.0", 2880);
                let releases = vec![draft, prerelease, wrong_platform, candidate, older];
                let selected = select_release(releases, PLATFORM, now(), allow_fresh).unwrap();
                assert_eq!(selected.version, expected);
            }
        }
    }

    #[test]
    fn refuses_newest_matching_asset_with_invalid_verification_metadata() {
        for allow_fresh in [false, true] {
            for (size, digest) in [
                (0, Some(format!("sha256:{HASH}"))),
                (MAX_BINARY_BYTES + 1, Some(format!("sha256:{HASH}"))),
                (16, None),
                (16, Some(format!("sha512:{HASH}"))),
                (16, Some("sha256:not-a-checksum".to_owned())),
            ] {
                let mut latest = fixture("v0.46.0", if allow_fresh { 10 } else { 1440 });
                latest.assets[0].size = size;
                latest.assets[0].digest = digest;
                let older = fixture("v0.45.0", 2800);
                let error = select_release(vec![older, latest], PLATFORM, now(), allow_fresh)
                    .err()
                    .unwrap();
                assert!(error.to_string().contains("invalid"));
            }
        }
    }

    #[test]
    fn existing_component_must_match_the_size_and_hash_at_install() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("browser");
        let asset = select_release(vec![fixture("v0.45.0", 1440)], PLATFORM, now(), false).unwrap();
        assert!(!verify_binary(&path, &asset).unwrap());
        fs::write(&path, b"verified release").unwrap();
        assert!(verify_binary(&path, &asset).unwrap());
        fs::write(&path, b"tampered release").unwrap();
        assert!(!verify_binary(&path, &asset).unwrap());
    }

    #[test]
    fn selects_each_live_scope_after_refresh_and_migrates_legacy_scopes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let states = root.join("scopes");
        private_dir(&states).unwrap();

        let legacy = Component::parse("0.44.0").unwrap();
        let older = fixture("v0.45.0", 2880);
        let newer = fixture("v0.46.0", 1440);
        let current = select_release(vec![older], PLATFORM, now(), false)
            .unwrap()
            .component();
        let replacement = select_release(vec![newer], PLATFORM, now(), false)
            .unwrap()
            .component();
        for component in [&legacy, &current, &replacement] {
            let path = component.path(&root);
            fs::write(&path, "component").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(Installed::load(&root).unwrap().unwrap().current, legacy);
        let worktree = temp.path().join("worktree");
        let default = session_name(&worktree, "default", None);
        let custom = session_name(&worktree, "default", Some("devbox.test"));
        let legacy_dir = socket_dir(&root, &legacy, true).unwrap().unwrap();
        let old_dir = socket_dir(&root, &current, true).unwrap().unwrap();
        let legacy_socket = UnixListener::bind(legacy_dir.join(format!("{default}.sock"))).unwrap();
        let socket = UnixListener::bind(old_dir.join(format!("{custom}.sock"))).unwrap();
        activate(&root, &current, Some(&legacy)).unwrap();
        write_private_file(&states.join(&custom), current.id()).unwrap();
        activate(&root, &replacement, Some(&legacy)).unwrap();

        let installed = Installed::load(&root).unwrap().unwrap();
        assert_eq!(installed.current, replacement);
        assert_eq!(
            active_component(&root, &states, &custom, installed.legacy.as_ref())
                .unwrap()
                .unwrap()
                .0,
            current
        );
        assert_eq!(
            active_component(&root, &states, &default, installed.legacy.as_ref())
                .unwrap()
                .unwrap()
                .0,
            legacy
        );
        drop(socket);
        assert!(
            active_component(&root, &states, &custom, installed.legacy.as_ref())
                .unwrap()
                .is_none()
        );
        drop(legacy_socket);
        fs::remove_dir_all(&legacy_dir).unwrap();
        fs::remove_dir_all(&old_dir).unwrap();
    }

    #[tokio::test]
    async fn incompatible_component_cannot_change_current_selection() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let current = select_release(vec![fixture("v0.45.0", 2880)], PLATFORM, now(), false)
            .unwrap()
            .component();
        activate(root, &current, None).unwrap();
        let candidate = select_release(vec![fixture("v0.46.0", 1440)], PLATFORM, now(), false)
            .unwrap()
            .component();
        let path = candidate.path(root);
        executable::write(&path, "#!/bin/sh\necho agent-browser 0.44.0\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

        let error = check_contract(root, &candidate, "0.46.0")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not report version 0.46.0"));
        assert_eq!(Installed::load(root).unwrap().unwrap().current, current);
    }
}
