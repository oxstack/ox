//! Why the previous host stopped, so the next one can say so.
//!
//! A host that stops writes `last-stop.json` in its state directory: the
//! reason (`upgrade` after a drain, `stop` after SIGTERM), its version and how
//! many turns were still running. The next host consumes it once, when it
//! retires the previous epoch's YOLO requests and unstarted handoffs. No task,
//! prompt or result is recorded. A host that crashed leaves no file, and the
//! retirement says only that the host restarted.
use ox_desk_core::actions::Action;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const FILE: &str = "last-stop.json";
pub(crate) const REFUSAL: &str = "The OX session host is restarting for an update and is not starting new work. Running turns are finishing; try again in a few seconds.";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct LastStop {
    pub reason: String,
    pub version: String,
    pub running: usize,
    pub at: u64,
}

pub(crate) fn version() -> &'static str {
    option_env!("OX_BUILD_VERSION")
        .filter(|s| !s.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Actions that would start a turn, a worker or a handoff.
pub(crate) fn starts_work(action: &Action) -> bool {
    matches!(
        action,
        Action::Submit(_)
            | Action::SendText(..)
            | Action::SendImages(..)
            | Action::StartWorker(_)
            | Action::StartHandoff(_)
            | Action::MoveSession(_)
            | Action::InjectHandoff(..)
            | Action::ExecuteCommand(_)
            | Action::NativeCommand(..)
            | Action::PaletteCommand(..)
    )
}

pub(crate) fn record(dir: &Path, reason: &str, running: usize) -> io::Result<()> {
    let stop = LastStop {
        reason: reason.into(),
        version: version().into(),
        running,
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    };
    let tmp = dir.join(format!("{FILE}.tmp-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(&stop)?)?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(FILE))
}

/// Read and remove the previous host's note. Returns the clause that completes
/// "The OX session host …" in retirement messages.
pub(crate) fn take(dir: &Path) -> String {
    let path = dir.join(FILE);
    let stop: Option<LastStop> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let _ = fs::remove_file(&path);
    cause(stop.as_ref())
}

fn cause(stop: Option<&LastStop>) -> String {
    match stop {
        Some(s) if s.reason == "upgrade" => {
            format!("restarted for an update from {}", s.version)
        }
        Some(s) if s.reason == "stop" => "was stopped and started again".into(),
        _ => "restarted".into(),
    }
}

/// The last stop, without consuming it (for `ox host doctor`).
pub(crate) fn peek(dir: &Path) -> Option<LastStop> {
    fs::read(dir.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_upgrade_names_itself_once_and_a_crash_says_restarted() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(take(dir.path()), "restarted");
        record(dir.path(), "upgrade", 2).unwrap();
        let seen = peek(dir.path()).unwrap();
        assert_eq!((seen.reason.as_str(), seen.running), ("upgrade", 2));
        assert_eq!(
            take(dir.path()),
            format!("restarted for an update from {}", version())
        );
        assert_eq!(take(dir.path()), "restarted");
        record(dir.path(), "stop", 0).unwrap();
        assert_eq!(take(dir.path()), "was stopped and started again");
    }

    #[test]
    fn only_work_starting_actions_are_refused_while_draining() {
        assert!(starts_work(&Action::SendText(None, "hi".into())));
        assert!(starts_work(&Action::Submit(None)));
        assert!(!starts_work(&Action::Stop("t".into())));
        assert!(!starts_work(&Action::SelectThread("t".into())));
        assert!(!starts_work(&Action::AnswerAsk(
            serde_json::from_value(serde_json::json!(1)).unwrap(),
            0
        )));
    }
}
