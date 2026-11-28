//! Typed companion operations for the local browser.
use std::{
    path::Path,
    process::{Command, Output},
};

pub(crate) enum PreviewPort {
    Ready(u16),
    NeedsInstall,
}

fn parse_port(endpoint: &str) -> Option<u16> {
    let line = endpoint
        .strip_suffix("\r\n")
        .or_else(|| endpoint.strip_suffix('\n'))
        .unwrap_or(endpoint);
    let port = line.strip_prefix("http://127.0.0.1:")?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

fn command(project: &Path, label: &str) -> Command {
    let mut command = Command::new(super::ox());
    command
        .args(["browser", "--session", label])
        .current_dir(project);
    command
}

pub(crate) fn open(project: &Path, label: &str, url: &str) -> Result<(), String> {
    let output = command(project, label)
        .args(["open", url])
        .output()
        .map_err(|error| format!("Could not open the local browser. {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(command_error(output, "Could not open the local browser."))
}

pub(crate) fn close(project: &Path, label: &str) -> Result<(), String> {
    let output = command(project, label)
        .arg("close")
        .output()
        .map_err(|error| format!("Could not stop the browser session. {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(output, "Could not stop the browser session."))
    }
}

pub(crate) fn install() -> Result<(), String> {
    let output = Command::new(super::ox())
        .args(["browser", "install"])
        .env("PATH", super::child_path())
        .output()
        .map_err(|error| format!("Could not install the browser component. {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(
            output,
            "Could not install the browser component.",
        ))
    }
}

fn command_error(output: Output, fallback: &str) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .next()
        .map(|line| line.strip_prefix("ox: ").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| line.chars().take(200).collect())
        .unwrap_or_else(|| fallback.into())
}

pub(crate) fn preview_port(project: &Path, label: &str) -> Result<PreviewPort, String> {
    let output = command(project, label)
        .arg("preview")
        .output()
        .map_err(|error| format!("Could not find OX’s browser component: {error}"))?;
    match output.status.code() {
        Some(0) => std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(parse_port)
            .map(PreviewPort::Ready)
            .ok_or_else(|| "OX returned an invalid local browser address.".to_owned()),
        Some(2) => Ok(PreviewPort::NeedsInstall),
        Some(3) => {
            Err("Enter your dev server’s URL or connect to a browser your agent started.".into())
        }
        _ => Err("Could not find the local browser session.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_port;

    #[test]
    fn accepts_only_the_companion_loopback_origin() {
        assert_eq!(parse_port("http://127.0.0.1:5678\n"), Some(5678));
        for value in [
            "http://127.0.0.1:0",
            "http://127.0.0.1:+5678",
            "http://127.0.0.1:5678/",
            "http://127.0.0.1:5678\n\n",
            "http://127.0.0.1:5678\nextra",
            "http://127.0.0.1:5678@external.test",
            "http://localhost:5678",
            "https://127.0.0.1:5678",
        ] {
            assert!(parse_port(value).is_none(), "{value}");
        }
    }
}
