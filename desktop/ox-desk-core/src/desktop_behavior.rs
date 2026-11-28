//! User-owned Desktop appearance and input settings shared by native and paired settings clients.
use crate::runtime::{Access, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSource {
    #[default]
    System,
    Light,
    Dark,
}
/// Who answers the approvals and questions of a worker an agent started.
#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerApprovals {
    /// The parent agent answers what the host's policy lets it; the rest waits
    /// for a person.
    #[default]
    Parent,
    /// Every request waits for a person.
    Operator,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Behavior {
    pub enter_sends: bool,
    pub theme: ThemeSource,
    pub font_size: u8,
    pub reduce_motion: bool,
    /// Show the weather from MET Norway on the header horizon.
    pub live_weather: bool,
    pub background_updates: bool,
    pub worker_approvals: WorkerApprovals,
    /// The mode a new Claude Code session starts in. `None` starts it the way
    /// `claude` starts in a terminal: Claude Code's own configured default.
    pub claude_start: Option<Access>,
    /// The preset a new Codex session starts in. `None` uses the Codex config,
    /// as `codex` does in a terminal.
    pub codex_start: Option<Access>,
    /// The mode a new Antigravity session starts in, including startup-only YOLO.
    /// `None` uses Antigravity's configured default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antigravity_start: Option<Access>,
}
impl Default for Behavior {
    fn default() -> Self {
        Self {
            enter_sends: true,
            theme: ThemeSource::System,
            font_size: 16,
            reduce_motion: false,
            live_weather: true,
            background_updates: true,
            worker_approvals: WorkerApprovals::Parent,
            claude_start: None,
            codex_start: None,
            antigravity_start: None,
        }
    }
}
#[derive(Deserialize, Serialize)]
pub struct Snapshot {
    pub revision: String,
    pub behavior: Behavior,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    ChangedOnDisk { revision: String },
}

#[path = "../../../src/desktop_state.rs"]
mod desktop_state;
pub use desktop_state::directory;
pub fn path() -> io::Result<PathBuf> {
    directory()
        .map(|p| p.join("preferences.json"))
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))
}
fn document(path: &Path) -> io::Result<Value> {
    match fs::File::open(path) {
        Ok(file) => serde_json::from_reader(file).map_err(io::Error::from),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e),
    }
}
fn snapshot(value: Value) -> io::Result<Snapshot> {
    let mut behavior: Behavior = serde_json::from_value(value)?;
    behavior.font_size = behavior.font_size.clamp(12, 24);
    Ok(Snapshot {
        revision: revision(behavior),
        behavior,
    })
}
/// The mode a new session of `runtime` starts in when nobody chose one for it:
/// the saved setting when it names a preset of that runtime, else `None`
/// (the runtime's own configured default).
pub fn start_mode(runtime: Runtime) -> Option<Access> {
    let behavior = read().ok()?.behavior;
    let chosen = match runtime {
        Runtime::Claude => behavior.claude_start,
        Runtime::Codex => behavior.codex_start,
        Runtime::Antigravity => behavior.antigravity_start,
        Runtime::OpenCode => None,
    }?;
    Access::choices(runtime).contains(&chosen).then_some(chosen)
}
pub fn revision(value: Behavior) -> String {
    serde_json::to_string(&value).expect("Scalar desktop preferences are serializable")
}
pub fn read() -> io::Result<Snapshot> {
    snapshot(document(&path()?)?)
}
pub fn write(expected: &str, next: Behavior) -> io::Result<Outcome> {
    write_at(&path()?, expected, next)
}
fn write_at(path: &Path, expected: &str, next: Behavior) -> io::Result<Outcome> {
    if !(12..=24).contains(&next.font_size) {
        return Err(io::Error::other("Font size must be between 12 and 24"));
    }
    for (runtime, start) in [
        (Runtime::Claude, next.claude_start),
        (Runtime::Codex, next.codex_start),
        (Runtime::Antigravity, next.antigravity_start),
    ] {
        if start.is_some_and(|a| !Access::choices(runtime).contains(&a)) {
            return Err(io::Error::other(format!(
                "That start mode is not a {} mode",
                runtime.name()
            )));
        }
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))?;
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(".preferences.lock"))?;
    lock.lock()?;
    let mut doc = document(path)?;
    let now = snapshot(doc.clone())?;
    if now.revision != expected {
        return Ok(Outcome::ChangedOnDisk {
            revision: now.revision,
        });
    }
    let object = doc
        .as_object_mut()
        .ok_or_else(|| io::Error::other("Invalid desktop preferences"))?;
    let Value::Object(fields) = serde_json::to_value(next)? else {
        unreachable!()
    };
    object.extend(fields);
    if next.antigravity_start.is_none() {
        object.remove("antigravity_start");
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut temp, &doc)?;
    temp.flush()?;
    temp.persist(path)?;
    Ok(Outcome::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_checked_edits_preserve_extension_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("preferences.json");
        fs::write(&path, r#"{"future":{"tint":"amber"}}"#).unwrap();
        let original = Behavior::default();
        let changed = Behavior {
            font_size: 20,
            ..original
        };
        assert!(matches!(
            write_at(&path, &revision(original), changed).unwrap(),
            Outcome::Applied
        ));
        assert!(matches!(
            write_at(&path, &revision(original), original).unwrap(),
            Outcome::ChangedOnDisk { .. }
        ));
        assert_eq!(document(&path).unwrap()["future"]["tint"], "amber");
    }
    #[test]
    fn start_modes_default_to_the_terminal_and_stay_within_their_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("preferences.json");
        // A file saved before these settings existed starts every runtime in its own default.
        fs::write(&path, r#"{"font_size":18}"#).unwrap();
        let before = snapshot(document(&path).unwrap()).unwrap().behavior;
        assert_eq!((before.claude_start, before.codex_start), (None, None));
        assert_eq!(before.antigravity_start, None);
        assert_eq!(before.worker_approvals, WorkerApprovals::Parent);
        let auto = Behavior {
            claude_start: Some(Access::Auto),
            codex_start: Some(Access::Yolo),
            antigravity_start: Some(Access::Plan),
            ..before
        };
        assert!(matches!(
            write_at(&path, &revision(before), auto).unwrap(),
            Outcome::Applied
        ));
        let saved = document(&path).unwrap();
        assert_eq!(
            (
                saved["claude_start"].as_str(),
                saved["codex_start"].as_str()
            ),
            (Some("auto"), Some("yolo"))
        );
        assert_eq!(saved["antigravity_start"], "plan");
        // A preset of the other runtime is refused.
        let crossed = Behavior {
            codex_start: Some(Access::Plan),
            ..auto
        };
        assert!(write_at(&path, &revision(auto), crossed).is_err());
        let crossed = Behavior {
            antigravity_start: Some(Access::AutoReview),
            ..auto
        };
        assert!(write_at(&path, &revision(auto), crossed).is_err());
        let yolo = Behavior {
            antigravity_start: Some(Access::Yolo),
            ..auto
        };
        assert!(matches!(
            write_at(&path, &revision(auto), yolo).unwrap(),
            Outcome::Applied
        ));
        let reset = Behavior {
            antigravity_start: None,
            ..auto
        };
        assert!(matches!(
            write_at(&path, &revision(yolo), reset).unwrap(),
            Outcome::Applied
        ));
        assert_eq!(snapshot(document(&path).unwrap()).unwrap().behavior, reset);
    }
}
