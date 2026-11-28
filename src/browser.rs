//! Explicit, local browser verification for humans and tools in a worktree.

use crate::provider::RelayError;
use reqwest::Url;
use ring::digest;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::net::Ipv6Addr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

mod component;

use component::{Component, Installed};
const LOCAL_HOSTS: &str = "localhost,127.0.0.1,[::1]";
const INSTALL_HINT: &str = "fix: ox browser install";
const DOWNLOAD_ENV: &[&str] = &[
    "SSL_CERT_FILE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

const USAGE: &str = "\
Verify a local app with a shared browser session.

Usage: ox browser [--session NAME] [--allow-host HOST] <command>

  install [--allow-fresh]         Install or refresh the browser component
  open http://localhost:3000      Open your local app
  snapshot [--json]               Read the page and get element references
  fill @e1 TEXT                   Fill a field from the latest snapshot
  click @e2                       Click an element from the latest snapshot
  preview                         Print the live preview link for the same session
  screenshot FILE.png             Save a screenshot to the given path
  close                           Close the browser session

Install requires a stable release and binary published at least 24 hours ago.
Use install --allow-fresh for an urgent stable release. Open sessions keep
their component until closed.

Run commands from the same Git worktree or, outside Git, the same directory.
Use --session NAME to keep two conversations in that worktree separate.
For a trusted development host, pass --allow-host HOST before each command.
Set OX_BROWSER_EXECUTABLE to use a specific Chromium executable.";

pub(crate) fn print_usage() {
    println!("{USAGE}");
}

enum Action<'a> {
    Help,
    Install { allow_fresh: bool },
    Open(Url),
    Snapshot { json: bool },
    Fill(&'a OsStr, &'a str),
    Click(&'a OsStr),
    Screenshot(PathBuf),
    Preview,
    Close,
}

struct Scope<'a> {
    label: &'a str,
    allowed_host: Option<String>,
}

fn parse(mut args: &[OsString]) -> Result<(Scope<'_>, Action<'_>), RelayError> {
    let mut label = None;
    let mut allowed_host = None;
    while let [flag, value, rest @ ..] = args {
        match flag.to_str() {
            Some("--session") if label.is_none() => label = Some(session_label(value)?),
            Some("--allow-host") if allowed_host.is_none() => allowed_host = Some(dev_host(value)?),
            _ => break,
        }
        args = rest;
    }

    let action = match args {
        [] => Action::Help,
        [arg] if matches!(arg.to_str(), Some("-h" | "--help" | "help")) => Action::Help,
        [arg] if arg == "install" => Action::Install { allow_fresh: false },
        [arg, flag] if arg == "install" && flag == "--allow-fresh" => {
            Action::Install { allow_fresh: true }
        }
        [arg, url] if arg == "open" => Action::Open(dev_url(
            url.to_str().ok_or("The URL must be UTF-8.")?,
            allowed_host.as_deref(),
        )?),
        [arg] if arg == "snapshot" => Action::Snapshot { json: false },
        [arg, flag] if arg == "snapshot" && flag == "--json" => Action::Snapshot { json: true },
        [arg, reference, text] if arg == "fill" => Action::Fill(
            element_ref(reference)?,
            text.to_str().ok_or("The field text must be UTF-8.")?,
        ),
        [arg, reference] if arg == "click" => Action::Click(element_ref(reference)?),
        [arg, path]
            if arg == "screenshot" && Path::new(path).extension() == Some(OsStr::new("png")) =>
        {
            Action::Screenshot(env::current_dir()?.join(path))
        }
        [arg] if arg == "preview" => Action::Preview,
        [arg] if arg == "close" => Action::Close,
        _ => return Err("Invalid browser command or arguments. See ox browser --help.".into()),
    };
    Ok((
        Scope {
            label: label.unwrap_or("default"),
            allowed_host,
        },
        action,
    ))
}

fn session_label(value: &OsStr) -> Result<&str, RelayError> {
    let name = value
        .to_str()
        .ok_or("Browser session names must be UTF-8.")?;
    if !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Ok(name);
    }
    Err("Use a browser session name with 1–64 letters, digits, hyphens or underscores.".into())
}

fn element_ref(reference: &OsStr) -> Result<&OsStr, RelayError> {
    if reference
        .to_str()
        .and_then(|r| r.strip_prefix("@e"))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    {
        Ok(reference)
    } else {
        Err("Use an element reference such as @e1 from ox browser snapshot.".into())
    }
}

fn dev_host(value: &OsStr) -> Result<String, RelayError> {
    const INVALID: &str =
        "Pass one hostname or IP address to --allow-host, without a scheme, port or wildcard.";
    let value = value.to_str().ok_or(INVALID)?;
    let ip = value
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(value);
    if ip.parse::<Ipv6Addr>().is_ok() {
        let url = Url::parse(&format!("http://[{ip}]/")).map_err(|_| INVALID)?;
        return Ok(url.host_str().ok_or(INVALID)?.to_owned());
    }
    let host = value.to_ascii_lowercase();
    if host
        .strip_suffix('.')
        .unwrap_or(&host)
        .split('.')
        .all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && Url::parse(&format!("http://{host}/"))
            .is_ok_and(|url| url.host_str() == Some(host.as_str()))
    {
        return Ok(host);
    }
    Err(INVALID.into())
}

fn dev_url(input: &str, allowed_host: Option<&str>) -> Result<Url, RelayError> {
    let invalid_url = "Enter an HTTP or HTTPS development URL without credentials, such as http://localhost:3000.";
    let url = Url::parse(input).map_err(|_| invalid_url)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid_url.into());
    }
    let host = url.host_str().ok_or(invalid_url)?;
    if LOCAL_HOSTS.split(',').any(|local| host == local) || Some(host) == allowed_host {
        return Ok(url);
    }
    Err("This URL is outside the browser session's scope. For a trusted development host, use --allow-host HOST before the command.".into())
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn component_dir() -> Result<PathBuf, RelayError> {
    let base = absolute_env("XDG_CACHE_HOME")
        .or_else(|| {
            absolute_env("HOME").map(|home| {
                home.join(if cfg!(target_os = "macos") {
                    "Library/Caches"
                } else {
                    ".cache"
                })
            })
        })
        .ok_or("Set HOME or XDG_CACHE_HOME to an absolute directory.")?;
    Ok(base.join("ox/browser"))
}

fn worktree_root(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(cwd)
}

fn session_name(worktree: &Path, label: &str, allowed_host: Option<&str>) -> String {
    let mut sha = digest::Context::new(&digest::SHA256);
    sha.update(worktree.as_os_str().as_encoded_bytes());
    sha.update(b"\0");
    sha.update(label.as_bytes());
    if let Some(host) = allowed_host {
        // Keep existing loopback identities. An opt-in gets a separate profile.
        sha.update(b"\0");
        sha.update(host.as_bytes());
    }
    format!("ox-{}", hex::encode(&sha.finish().as_ref()[..12]))
}

fn system_browser() -> Result<Option<PathBuf>, RelayError> {
    if let Some(path) = env::var_os("OX_BROWSER_EXECUTABLE").map(PathBuf::from) {
        if path.is_absolute() && is_executable(&path) {
            return Ok(Some(path));
        }
        return Err(
            "OX_BROWSER_EXECUTABLE must point to an absolute, executable Chromium path.".into(),
        );
    }
    if cfg!(target_os = "macos") {
        let path = "Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        let system = Path::new("/").join(path);
        return Ok([
            Some(system),
            absolute_env("HOME").map(|home| home.join(path)),
        ]
        .into_iter()
        .flatten()
        .find(|path| is_executable(path)));
    }
    let Some(path) = env::var_os("PATH") else {
        return Ok(None);
    };
    Ok(env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| {
            [
                "chromium",
                "chromium-browser",
                "google-chrome",
                "google-chrome-stable",
            ]
            .map(|name| dir.join(name))
        })
        .find(|path| is_executable(path)))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o100 != 0)
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(path)
}

fn user_id() -> u32 {
    // SAFETY: geteuid reads the current process user ID.
    unsafe { libc::geteuid() }
}

fn private_directory(path: &Path) -> Result<bool, RelayError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    if !metadata.is_dir() || metadata.uid() != user_id() || metadata.mode() & 0o777 != 0o700 {
        return Err(format!(
            "The OX Browser directory must be private and owned by you: {}",
            path.display()
        )
        .into());
    }
    Ok(true)
}

fn private_executable(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_file() && meta.uid() == user_id() && meta.mode() & 0o777 == 0o700),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn socket_dir(
    root: &Path,
    component: &Component,
    create: bool,
) -> Result<Option<PathBuf>, RelayError> {
    // Keep the path below the Unix socket path limit, even in long home paths.
    let uid = user_id();
    let namespace = session_name(root, component.id(), None);
    let dir = Path::new("/tmp").join(format!("ox-browser-{uid}-{namespace}"));
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(private_directory(&dir)?.then_some(dir))
}

fn child_command(root: &Path, component: &Component) -> Command {
    let mut command = Command::new(component.path(root));
    // The browser keeps its own agent-browser settings and inherits no provider credentials.
    command.env_clear().env("HOME", root).current_dir(root);
    for key in ["PATH", "LANG", "LC_ALL", "LC_CTYPE"] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

fn read_private_file(path: &Path) -> Result<Option<String>, RelayError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if !meta.is_file() || meta.uid() != user_id() || meta.mode() & 0o777 != 0o600 {
        return Err(format!(
            "The OX Browser record must be private and owned by you: {}",
            path.display()
        )
        .into());
    }
    // Local records contain one or two generation IDs, each under 100 bytes.
    let mut value = String::new();
    fs::File::open(path)?.take(257).read_to_string(&mut value)?;
    if value.len() > 256 {
        return Err("The OX Browser record is too large.".into());
    }
    Ok(Some(value))
}

fn write_private_file(path: &Path, data: &str) -> Result<(), RelayError> {
    let mut stage = tempfile::NamedTempFile::new_in(path.parent().expect("browser record parent"))?;
    stage.write_all(data.as_bytes())?;
    stage
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    stage.as_file().sync_all()?;
    stage.persist(path)?;
    Ok(())
}

fn lock_scope(states: &Path, session: &str, exclusive: bool) -> Result<fs::File, RelayError> {
    private_dir(states)?;
    if !private_directory(states)? {
        return Err("The OX Browser session directory became unavailable.".into());
    }
    let lock = fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(states.join(format!("{session}.lock")))?;
    if exclusive {
        lock.lock()?;
    } else {
        lock.lock_shared()?;
    }
    Ok(lock)
}

fn active_component(
    root: &Path,
    states: &Path,
    session: &str,
    legacy: Option<&Component>,
) -> Result<Option<(Component, PathBuf)>, RelayError> {
    let recorded = if private_directory(states)? {
        read_private_file(&states.join(session))?
            .as_deref()
            .map(Component::parse)
            .transpose()?
    } else {
        None
    };
    for component in recorded
        .iter()
        .chain(legacy.filter(|legacy| Some(*legacy) != recorded.as_ref()))
    {
        if let Some(dir) = socket_dir(root, component, false)?
            && UnixStream::connect(dir.join(format!("{session}.sock"))).is_ok()
        {
            if !private_executable(&component.path(root))? {
                return Err(format!(
                    "The active OX Browser session component is unavailable.\n  {INSTALL_HINT}"
                )
                .into());
            }
            return Ok(Some((component.clone(), dir)));
        }
    }
    Ok(None)
}

pub(crate) async fn run(args: &[OsString]) -> Result<u8, RelayError> {
    let (scope, action) = parse(args)?;
    if matches!(action, Action::Help) {
        print_usage();
        return Ok(0);
    }
    let root = component_dir()?;
    if let Action::Install { allow_fresh } = action {
        private_dir(&root)?;
        if !private_directory(&root)? {
            return Err("The OX Browser directory became unavailable. Retry installation.".into());
        }
        return component::install(&root, allow_fresh).await;
    }
    if !private_directory(&root)? {
        return component_unavailable(&action);
    }
    let cwd = env::current_dir()?.canonicalize()?;
    let session = session_name(
        worktree_root(&cwd),
        scope.label,
        scope.allowed_host.as_deref(),
    );
    let states = root.join("scopes");
    let opening = matches!(action, Action::Open(_));
    // Hold the lock through the subprocess so a queued command cannot revive
    // a scope after close. A passive query can also restart an empty daemon.
    let exclusive = opening || matches!(action, Action::Close);
    let _scope_lock = lock_scope(&states, &session, exclusive)?;
    let Some(installed) = Installed::load(&root)? else {
        return component_unavailable(&action);
    };
    let (component, sockets) =
        match active_component(&root, &states, &session, installed.legacy.as_ref())? {
            Some(active) => active,
            None if !private_executable(&installed.current.path(&root))? => {
                return component_unavailable(&action);
            }
            None if opening => {
                let sockets = socket_dir(&root, &installed.current, true)?
                    .ok_or("The OX Browser socket directory became unavailable.")?;
                (installed.current, sockets)
            }
            None => {
                return match action {
                    Action::Preview => Ok(preview_unavailable()),
                    Action::Close => Ok(0),
                    _ => {
                        Err("Open your app with ox browser open before using this session.".into())
                    }
                };
            }
        };
    let mut command = child_command(&root, &component);
    command.env("AGENT_BROWSER_SOCKET_DIR", sockets);
    let domains = match &scope.allowed_host {
        Some(host) => format!("{LOCAL_HOSTS},{host}"),
        None => LOCAL_HOSTS.to_owned(),
    };
    command.args([
        "--session",
        &session,
        "--allowed-domains",
        &domains,
        "--content-boundaries",
    ]);
    if opening && let Some(browser) = system_browser()? {
        command.arg("--executable-path").arg(
            browser
                .to_str()
                .ok_or("The Chromium executable path must be UTF-8.")?,
        );
    }
    match action {
        Action::Open(url) => {
            write_private_file(&states.join(&session), component.id())?;
            command.args(["open", url.as_str()]);
        }
        Action::Snapshot { json } => {
            command.args(["snapshot", "--compact"]);
            if json {
                command.arg("--json");
            }
        }
        Action::Fill(reference, text) => {
            command.arg("fill").arg(reference).arg("--").arg(text);
        }
        Action::Click(reference) => {
            command.arg("click").arg(reference);
        }
        Action::Screenshot(path) => {
            command
                .arg("screenshot")
                .arg(path.to_str().ok_or("The screenshot path must be UTF-8.")?);
        }
        Action::Preview => return preview(&mut command),
        Action::Close => {
            command.arg("close");
        }
        Action::Help | Action::Install { .. } => unreachable!(),
    }
    let status = command
        .status()
        .map_err(|error| format!("Could not start agent-browser: {error}."))?;
    Ok(exit_code(status))
}

fn preview(command: &mut Command) -> Result<u8, RelayError> {
    let output = command.args(["stream", "status", "--json"]).output()?;
    if !output.status.success() {
        std::io::stderr().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        return Ok(exit_code(output.status));
    }
    let state = stream_state(&output.stdout)?;
    if state.connected && state.port != 0 {
        println!("http://127.0.0.1:{}", state.port);
        return Ok(0);
    }
    Ok(preview_unavailable())
}

#[derive(serde::Deserialize)]
struct StreamState {
    connected: bool,
    port: u16,
    lifecycle: Option<StreamLifecycle>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamLifecycle {
    effective_launch: StreamLaunch,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamLaunch {
    browser_launched: bool,
}

fn stream_state(output: &[u8]) -> Result<StreamState, RelayError> {
    #[derive(serde::Deserialize)]
    struct Response {
        success: bool,
        data: Option<StreamState>,
    }

    let response: Response = serde_json::from_slice(output)?;
    if response.success
        && let Some(state) = response.data
    {
        return Ok(state);
    }
    Err("agent-browser returned an invalid stream status.".into())
}

fn preview_unavailable() -> u8 {
    eprintln!("ox: Open your app with ox browser open before opening its preview.");
    3
}

fn component_unavailable(action: &Action<'_>) -> Result<u8, RelayError> {
    let message = format!("OX Browser is not installed.\n  {INSTALL_HINT}");
    if matches!(action, Action::Preview) {
        eprintln!("ox: {message}");
        return Ok(2);
    }
    Err(message.into())
}

fn exit_code(status: ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_release_override_requires_an_explicit_install_flag() {
        let default = [OsString::from("install")];
        assert!(matches!(
            parse(&default).unwrap().1,
            Action::Install { allow_fresh: false }
        ));
        let override_args = [OsString::from("install"), OsString::from("--allow-fresh")];
        assert!(matches!(
            parse(&override_args).unwrap().1,
            Action::Install { allow_fresh: true }
        ));
        let invalid = [OsString::from("preview"), OsString::from("--allow-fresh")];
        assert!(parse(&invalid).is_err());
    }

    #[test]
    fn dev_urls_are_http_loopback_without_credentials() {
        for url in [
            "http://localhost:3000/path",
            "https://127.0.0.1:8443",
            "http://[::1]:8000",
        ] {
            assert!(dev_url(url, None).is_ok(), "{url}");
        }
        for url in [
            "localhost:3000",
            "https://localhost.example",
            "http://127.0.0.2:3000",
            "https://user:pass@localhost",
            "ftp://localhost/",
            "file:///tmp/page.html",
        ] {
            assert!(dev_url(url, None).is_err(), "{url}");
        }
    }

    #[test]
    fn override_accepts_one_exact_development_host() {
        let host = dev_host(OsStr::new("Devbox.Test")).unwrap();
        assert_eq!(host, "devbox.test");
        assert!(dev_url("http://devbox.test:3000", Some(&host)).is_ok());
        assert!(dev_url("http://localhost:3000", Some(&host)).is_ok());
        assert!(dev_url("http://sub.devbox.test", Some(&host)).is_err());
        assert!(dev_url("http://user@devbox.test", Some(&host)).is_err());

        let v4 = dev_host(OsStr::new("192.168.1.22")).unwrap();
        assert!(dev_url("http://192.168.1.22:3000", Some(&v4)).is_ok());
        let v6 = dev_host(OsStr::new("[fd00::abcd]")).unwrap();
        assert!(dev_url("http://[fd00::abcd]:3000", Some(&v6)).is_ok());
        assert_eq!(dev_host(OsStr::new("fd00::abcd")).unwrap(), v6);

        for bad in [
            "devbox.test,example.test",
            "*.devbox.test",
            "devbox.test:80",
            "http://devbox.test",
            "user@devbox.test",
            "devbox.test/path",
            "127.1",
            "[fd00::abcd]:80",
            " devbox.test",
        ] {
            assert!(dev_host(OsStr::new(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn scope_options_can_appear_in_either_order_once() {
        let args = [
            "--allow-host",
            "Devbox.Test",
            "--session",
            "review",
            "preview",
        ]
        .map(OsString::from);
        let (scope, action) = parse(&args).unwrap();
        assert_eq!(scope.label, "review");
        assert_eq!(scope.allowed_host.as_deref(), Some("devbox.test"));
        assert!(matches!(action, Action::Preview));
        let duplicate = [
            "--allow-host",
            "first.test",
            "--allow-host",
            "second.test",
            "preview",
        ]
        .map(OsString::from);
        assert!(parse(&duplicate).is_err());
    }

    #[test]
    fn scopes_follow_worktrees_and_keep_named_sessions_separate() {
        let root = tempfile::tempdir().unwrap();
        let left = root.path().join("left");
        let right = root.path().join("right");
        let child = left.join("src");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&right).unwrap();
        fs::write(left.join(".git"), "gitdir: /elsewhere/worktrees/left").unwrap();
        fs::create_dir(right.join(".git")).unwrap();

        assert_eq!(worktree_root(&child), left);
        assert_eq!(worktree_root(&right), right);
        assert_ne!(
            session_name(&left, "default", None),
            session_name(&right, "default", None),
        );
        assert_ne!(
            session_name(&left, "default", None),
            session_name(&left, "review", None),
        );
        assert_ne!(
            session_name(&left, "default", None),
            session_name(&left, "default", Some("devbox.test")),
        );
        assert_ne!(
            session_name(&left, "default", Some("devbox.test")),
            session_name(&left, "default", Some("second.test")),
        );
    }

    #[test]
    fn managed_paths_must_be_private_and_exclude_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        assert!(!private_directory(&root).unwrap());
        private_dir(&root).unwrap();
        assert!(private_directory(&root).unwrap());
        let binary = Component::parse("0.45.0").unwrap().path(&root);
        fs::write(&binary, "component").unwrap();
        assert!(!private_executable(&binary).unwrap());
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(private_executable(&binary).unwrap());
        let link = root.join("link");
        std::os::unix::fs::symlink(binary, &link).unwrap();
        assert!(!private_executable(&link).unwrap());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&root).is_err());
    }
}
