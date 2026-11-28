pub(crate) mod attachments;
mod host_client;
pub(crate) mod hosts;
#[cfg(test)]
pub(crate) use host_client::open_store;
pub(crate) use host_client::{
    CatalogEvent, CatalogWatch, HOST_NOT_RUNNING, HostLink, IdleReservation, ProcessMonitor,
    clean_handoff_pairings, reserve_idle_update,
};
pub(crate) use host_client::{MoveDestination, MovePhase, MoveStatus};
pub mod onboarding;
pub(crate) mod remote;
pub mod versions;
#[cfg(test)]
use ox_desk_core::store::Route;
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;

pub use ox_pty::child_path;

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub(crate) fn find(name: &str, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(name))
        .find(|p| executable(p))
}

/// Capture the companion beside the executable that started this window. On
/// macOS the launch path can pass through the mutable installer symlink.
static PACKAGED_OX: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

fn sibling_ox(executable_path: &Path) -> Option<PathBuf> {
    let sibling = executable_path.canonicalize().ok()?.with_file_name("ox");
    executable(&sibling).then_some(sibling)
}

pub fn init_companion() {
    PACKAGED_OX.get_or_init(|| env::current_exe().ok().and_then(|path| sibling_ox(&path)));
}

pub fn ox() -> PathBuf {
    PACKAGED_OX
        .get()
        .cloned()
        .flatten()
        .or_else(|| find("ox", &child_path()))
        .unwrap_or_else(|| PathBuf::from("ox"))
}

/// Supply the known application icon before AppKit initializes the first window's tabs.
/// Looking it up through IconServices synchronously can block window creation on Intel Macs.
#[cfg(target_os = "macos")]
pub fn set_application_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage, NSImageNameApplicationIcon};
    use objc2_foundation::NSData;

    let main_thread =
        MainThreadMarker::new().expect("configure the application on the main thread");
    let bytes = NSData::with_bytes(include_bytes!("../../packaging/ox-desktop.png"));
    if let Some(icon) = NSImage::initWithData(NSImage::alloc(), &bytes) {
        // AppKit exports this immutable string constant. The setter receives a live,
        // non-null NSImage and the application retains it for its Dock tile.
        unsafe {
            icon.setName(Some(NSImageNameApplicationIcon));
            NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&icon));
        }
    }
}

pub fn launch_claude_login(project: &Path) -> Result<(), String> {
    launch_in_terminal(project, "claude", vec!["login".into(), "claude".into()])
}

pub fn launch_login(runtime: ox_desk_core::runtime::Runtime, project: &Path) -> Result<(), String> {
    use ox_desk_core::runtime::Runtime;
    match runtime {
        Runtime::Codex => launch_chatgpt_login(project),
        Runtime::Claude => launch_claude_login(project),
        Runtime::Antigravity => {
            let path = child_path();
            let program = find(runtime.executable(), &path)
                .ok_or_else(|| "Install Antigravity CLI and add agy to PATH.".to_owned())?;
            open_program_in_terminal(
                project,
                program,
                vec!["acp".into(), "--login".into()],
                &path,
            )
        }
    }
}

pub fn launch_chatgpt_login(project: &Path) -> Result<(), String> {
    launch_in_terminal(project, "codex", vec!["login".into(), "chatgpt".into()])
}

pub fn launch_runtime_terminal(project: &Path, args: Vec<String>) -> Result<(), String> {
    let client = match args.first().map(String::as_str) {
        Some("codex") => "codex",
        Some("claude") => "claude",
        _ => {
            return Err(
                "The host did not supply a native terminal route. Update OX and retry.".into(),
            );
        }
    };
    launch_in_terminal(project, client, args)
}

fn launch_in_terminal(project: &Path, client: &str, args: Vec<String>) -> Result<(), String> {
    let path = child_path();
    let companion = ox();
    if !companion.is_absolute() {
        return Err("Install ox alongside OX Desktop or add ox to PATH.".into());
    }
    if find(client, &path).is_none() {
        return Err(format!("Install {client} and add its executable to PATH."));
    }
    open_program_in_terminal(project, companion, args, &path)
}

fn open_program_in_terminal(
    project: &Path,
    program: PathBuf,
    args: Vec<String>,
    path: &OsStr,
) -> Result<(), String> {
    let project = project
        .canonicalize()
        .map_err(|err| format!("Open project: {err}"))?;
    if !project.is_dir() {
        return Err("Select a project folder first.".into());
    }
    let mut command: Vec<OsString> = vec!["/usr/bin/env".into()];
    // A Terminal login shell can export a different Codex home even when the
    // desktop uses the default. Preserve absence as well as explicit paths.
    let profile_vars = [
        "HOME",
        "CODEX_HOME",
        "CODEX_SQLITE_HOME",
        "CLAUDE_CONFIG_DIR",
    ];
    for key in profile_vars {
        command.extend(["-u", key].map(OsString::from));
    }
    let mut path_var = OsString::from("PATH=");
    path_var.push(path);
    command.push(path_var);
    // Terminal services may have a different environment. Preserve explicitly
    // selected profile paths without exposing provider credentials in their argv.
    for key in profile_vars {
        if let Some(value) = env::var_os(key) {
            let mut assignment = OsString::from(key);
            assignment.push("=");
            assignment.push(value);
            command.push(assignment);
        }
    }
    command.push(program.into());
    command.extend(args.into_iter().map(OsString::from));
    open_terminal(&project, &command, path)
}

#[cfg(target_os = "macos")]
// Finder arguments are passed as data. Terminal's shell receives shell-quoted paths.
const MAC_TERMINAL_SCRIPT: &str = r#"on run argv
    set projectDir to quoted form of item 1 of argv
    set commandLine to "cd " & projectDir & " && exec"
    repeat with i from 2 to (count of argv)
        set commandLine to commandLine & " " & quoted form of (item i of argv)
    end repeat
    tell application "Terminal"
        activate
        do script commandLine
    end tell
end run"#;

#[cfg(target_os = "macos")]
fn open_terminal(project: &Path, command: &[OsString], _path: &OsStr) -> Result<(), String> {
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", MAC_TERMINAL_SCRIPT])
        .arg(project)
        .args(command)
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "Could not launch Terminal: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_terminal(project: &Path, command: &[OsString], path: &OsStr) -> Result<(), String> {
    // The terminal might be a D-Bus service with an unrelated working directory.
    // Set cwd and PATH in the actual terminal child, passing all values as argv.
    let terminals = [
        ("xdg-terminal-exec", &[][..]),
        ("x-terminal-emulator", &["-e"][..]),
        ("gnome-terminal", &["--"][..]),
        ("konsole", &["-e"][..]),
        ("ghostty", &["-e"][..]),
        ("kitty", &[][..]),
        ("foot", &[][..]),
        ("alacritty", &["-e"][..]),
        ("xterm", &["-e"][..]),
    ];
    let (terminal, args) = terminals
        .into_iter()
        .find_map(|(name, args)| find(name, path).map(|exe| (exe, args)))
        .ok_or("Install a terminal emulator or configure xdg-terminal-exec.")?;
    let mut child = Command::new(terminal)
        .args(args)
        .args([
            "/bin/sh",
            "-c",
            r#"cd "$1" && shift && exec "$@""#,
            "ox-desktop",
        ])
        .arg(project)
        .args(command)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|err| format!("Launch terminal: {err}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Try to convert a HEIC/HEIF image to PNG bytes using `sips` (macOS), `heif-convert`, or `magick`/`ffmpeg`.
pub fn convert_heic_to_png(path: &Path) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let out = dir.path().join("converted.png");
    let candidates: &[(&str, &[&str])] = &[
        ("sips", &["-s", "format", "png"]),
        ("heif-convert", &[]),
        ("magick", &[]),
        ("ffmpeg", &["-y", "-i"]),
    ];
    for (bin, prefix) in candidates {
        let mut cmd = Command::new(bin);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match *bin {
            "sips" => {
                cmd.args(*prefix).arg(path).arg("--out").arg(&out);
            }
            "ffmpeg" => {
                cmd.args(*prefix).arg(path).arg(&out);
            }
            _ => {
                cmd.arg(path).arg(&out);
            }
        }
        if cmd.status().is_ok_and(|s| s.success())
            && let Ok(bytes) = std::fs::read(&out)
            && !bytes.is_empty()
        {
            return Ok(bytes);
        }
    }
    Err(format!(
        "Could not convert {} from HEIC to PNG. Convert it to PNG or JPEG first.",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launched_companion_stays_with_its_package_after_a_switch() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("current");
        for package in ["first", "second"] {
            let path = root.path().join(package);
            std::fs::create_dir(&path).unwrap();
            for name in ["ox", "ox-desktop"] {
                let file = path.join(name);
                std::fs::write(&file, "").unwrap();
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        std::os::unix::fs::symlink("first", &current).unwrap();
        let pinned = sibling_ox(&current.join("ox-desktop")).unwrap();
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink("second", &current).unwrap();
        assert_eq!(pinned, root.path().join("first/ox").canonicalize().unwrap());
        assert_ne!(Some(pinned), sibling_ox(&current.join("ox-desktop")));
    }

    #[test]
    fn lookup_skips_relative_paths_and_nonexecutables() {
        let dir = env::temp_dir().join(format!("ox-desktop-platform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("ox");
        std::fs::write(&binary, b"").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).unwrap();
        let path = env::join_paths([Path::new("."), &dir]).unwrap();
        assert_eq!(find("ox", &path), None);
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(find("ox", &path), Some(binary));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn terminal_route_uses_the_same_codex_profile_as_desktop() {
        assert_eq!(
            ox_desk_core::codex::launch_args(&Route::CODEX_SUBSCRIPTION, "openai"),
            [
                "codex",
                "--chatgpt",
                "-c",
                "forced_login_method=\"chatgpt\""
            ]
        );
        assert_eq!(
            ox_desk_core::codex::launch_args(&Route::codex_api("test/model".into()), "custom"),
            [
                "codex",
                "--model",
                "test/model",
                "-c",
                "model_provider=\"custom\"",
                "-c",
                "model_providers.custom.name=\"custom\""
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn terminal_script_compiles_with_stock_terminal_dictionary() {
        let dir = env::temp_dir().join(format!("ox-desktop-applescript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = Command::new("/usr/bin/osacompile")
            .arg("-o")
            .arg(dir.join("launch.scpt"))
            .args(["-e", MAC_TERMINAL_SCRIPT])
            .output()
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminal_preserves_project_and_executable_as_literal_paths() {
        let dir = env::temp_dir().join(format!("ox-desktop-clau'de-{}", std::process::id()));
        let project = dir.join("sample $(ignored) & project");
        std::fs::create_dir_all(&project).unwrap();
        let terminal = dir.join("xdg-terminal-exec");
        let ox = dir.join("my ox");
        crate::executable::write(&terminal, "#!/bin/sh\nexec \"$@\"\n");
        crate::executable::write(
            &ox,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\" > launch.txt\n",
        );
        open_terminal(
            &project,
            &[ox.into(), "claude".into(), "--subscription".into()],
            dir.as_os_str(),
        )
        .unwrap();
        let capture = project.join("launch.txt");
        for _ in 0..100 {
            if std::fs::read_to_string(&capture)
                .is_ok_and(|value| value == "claude\n--subscription\n")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(capture).unwrap(),
            "claude\n--subscription\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

pub mod layout_probe;
pub mod probe;
pub mod restore_probe;
#[cfg(target_os = "linux")]
pub mod theme;
