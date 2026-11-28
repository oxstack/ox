//! Owned runtime connections and events delivered to the workbench.
use crate::codex::frame::Frame;
use std::io;

pub mod antigravity;
pub mod claude;
pub mod codex;

pub enum Link {
    Antigravity(Box<crate::store::antigravity::Project>),
    OpenCode(Box<crate::store::opencode::Project>),
    Host(Box<crate::store::replica::Remote>),
    Codex(codex::Link),
    Claude(Box<crate::store::claude::Project>),
}
impl From<codex::Link> for Link {
    fn from(link: codex::Link) -> Self {
        Self::Codex(link)
    }
}
impl Link {
    pub(crate) fn codex(&self) -> io::Result<&codex::Link> {
        match self {
            Self::Codex(link) => Ok(link),
            Self::Antigravity(_) | Self::Claude(_) | Self::Host(_) | Self::OpenCode(_) => {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "This operation requires a Codex session",
                ))
            }
        }
    }
    pub(crate) fn forget(&self, seq: i64) {
        match self {
            Self::Codex(link) => link.forget(seq),
            Self::Antigravity(_) | Self::Claude(_) | Self::Host(_) | Self::OpenCode(_) => (),
        }
    }
}

#[derive(Debug)]
pub enum LinkEvent {
    Host(Box<crate::host_protocol::Server>),
    Title {
        key: crate::titles::Key,
        name: Option<String>,
    },
    SignInRequired,
    Frame(Frame),
    Antigravity(crate::store::antigravity::Event),
    OpenCode(crate::store::opencode::Event),
    Claude(crate::store::claude::Update),
    Closed {
        reason: String,
        stderr_tail: Vec<String>,
    },
}

/// Why a link closed, for the UI. A process that just ended its output says
/// nothing on its own, so use what it printed on stderr instead, with the
/// launcher's `ox:` prefix removed. A Codex below the floor then surfaces as
/// "Codex X is older than the required Y.", which onboarding offers to update.
pub fn close_reason(reason: &str, stderr_tail: &[String]) -> String {
    if !reason.ends_with("closed its output") {
        return reason.to_owned();
    }
    let lines: Vec<&str> = stderr_tail
        .iter()
        .map(|line| {
            let line = line.trim();
            line.strip_prefix("ox: ").unwrap_or(line)
        })
        .filter(|line| !line.is_empty())
        .collect();
    if let Some(old) = lines
        .iter()
        .find(|line| line.contains("older than the required"))
    {
        return (*old).to_owned();
    }
    if lines.is_empty() {
        reason.to_owned()
    } else {
        lines.join("\n")
    }
}

impl LinkEvent {
    /// Responses and permission requests unblock an interaction. Streaming activity
    /// still waits for the next display frame and retains its original wire order.
    pub fn unblocks_interaction(&self) -> bool {
        if let Self::Host(frame) = self {
            use crate::host_protocol::{Change, Server};
            return !matches!(frame.as_ref(),Server::Delta{changes,..} if changes.iter().all(|c| matches!(c,Change::Append{..}|Change::Items{..}|Change::TimelineState{..})));
        }
        if let Self::Antigravity(event) = self {
            return event.unblocks();
        }
        if let Self::Claude(update) = self {
            return update.unblocks();
        }
        matches!(
            self,
            Self::Frame(Frame::Reply { .. } | Frame::Ask(_))
                | Self::Closed { .. }
                | Self::SignInRequired
        )
    }
}

#[cfg(test)]
mod close_reason_tests {
    use super::close_reason;

    fn tail(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn an_early_exit_shows_the_launcher_error_and_an_old_codex_is_named() {
        let old = tail(&[
            "ox: Codex 0.98.0 is older than the required 0.171.0.",
            "  why: could not read the bundled Codex model catalog; exit status 2",
            "  fix: brew upgrade --cask codex",
        ]);
        assert_eq!(
            close_reason("Codex closed its output", &old),
            "Codex 0.98.0 is older than the required 0.171.0."
        );
        let other = tail(&[
            "ox: could not read the bundled Codex model catalog",
            "",
            "  why: x",
        ]);
        assert_eq!(
            close_reason("Codex closed its output", &other),
            "could not read the bundled Codex model catalog\nwhy: x"
        );
        assert_eq!(
            close_reason("Codex closed its output", &[]),
            "Codex closed its output"
        );
        assert_eq!(close_reason("Broken pipe", &old), "Broken pipe");
    }
}
