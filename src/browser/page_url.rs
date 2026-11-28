//! Read the active page address using the installed component's stock CLI.

use super::{RelayError, dev_url};
use reqwest::Url;
use serde::Deserialize;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REPLY_BYTES: u64 = 256 * 1024;

#[derive(Deserialize)]
struct ReadReply {
    success: bool,
    data: Option<ReadPage>,
}

#[derive(Deserialize)]
struct ReadPage {
    url: String,
}

pub(super) async fn read(
    command: Command,
    allowed_host: Option<&str>,
) -> Result<Option<Url>, RelayError> {
    // A URL-less `read` observes the existing active tab and never launches Chromium.
    // `get url` can relaunch Chromium when the daemon has lost its browser.
    let mut command = tokio::process::Command::from(command);
    let mut child = command
        .args(["read", "--outline", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut output = Vec::new();
    let status = tokio::time::timeout(READ_TIMEOUT, async {
        let stdout = child
            .stdout
            .take()
            .ok_or("The browser output pipe is unavailable.")?;
        stdout
            .take(MAX_REPLY_BYTES + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() as u64 > MAX_REPLY_BYTES {
            return Err("The browser returned too much data to read the page URL.".into());
        }
        Ok::<_, RelayError>(child.wait().await?)
    })
    .await
    .map_err(|_| "Timed out reading the current browser URL.")??;

    let reply: ReadReply = serde_json::from_slice(&output)
        .map_err(|_| "The browser returned an invalid page URL response.")?;
    if !reply.success {
        return Ok(None);
    }
    let page = reply
        .data
        .filter(|_| status.success())
        .ok_or("The browser returned an invalid page URL response.")?;
    Ok(Some(dev_url(&page.url, allowed_host)?))
}
