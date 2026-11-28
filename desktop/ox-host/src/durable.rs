use ox_desk_core::host_protocol::{Scope, ScopeState, SessionResult, SessionView};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Owned {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) forwarded: std::collections::BTreeMap<u64, crate::engine::forwarding::Job>,
    pub(crate) scopes: Vec<SavedScope>,
    #[serde(default)]
    pub(crate) worker_notices: crate::worker_notices::Notices,
    pub(crate) reserved: u64,
    /// Sessions whose YOLO grant covers their workers' and handoffs' YOLO starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) grants: Vec<Grant>,
    /// Children waiting for the operator to accept YOLO. Only their stamps: the
    /// task stays in memory, so a new epoch retires them as declined.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) asking: Vec<ox_desk_core::origin::Origin>,
    /// Jobs that never became sessions, so their parent can still read why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) outcomes: Vec<Outcome>,
    /// Handoffs started but not yet confirmed, and recent failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) handoffs: Vec<HandoffRecord>,
    /// Context windows stock clients reported, by route and model, newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) windows: Vec<Window>,
}
impl Owned {
    pub(crate) fn record_outcome(&mut self, outcome: Outcome) {
        self.worker_notices
            .observe(&outcome.origin, &outcome.status, &outcome.message);
        self.outcomes.push(outcome);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) route: ox_desk_core::runtime::Route,
    pub(crate) model: Option<String>,
    pub(crate) tokens: u64,
}
/// At most this many reported windows are kept.
pub(crate) const WINDOWS: usize = 64;

/// Record a reported window, replacing the same route and model and keeping it newest.
pub(crate) fn record_window(windows: &mut Vec<Window>, window: Window) {
    if windows.last() == Some(&window) {
        return;
    }
    windows.retain(|w| !(w.route == window.route && w.model == window.model));
    windows.push(window);
    let excess = windows.len().saturating_sub(WINDOWS);
    windows.drain(..excess);
}

/// An API handoff names its model. Subscription handoffs let the stock
/// client select a model, so an observed window can only lower the fallback.
pub(crate) fn handoff_window(
    windows: &[Window],
    route: &ox_desk_core::runtime::Route,
    catalog: Option<u64>,
) -> Option<u64> {
    let reports = windows.iter().filter(|w| &w.route == route && w.tokens > 0);
    if let ox_desk_core::runtime::Billing::Api(alias) = &route.billing {
        let matched = reports
            .clone()
            .filter(|w| w.model.as_ref() == Some(alias))
            .map(|w| w.tokens);
        if let Some(limit) = matched.chain(catalog.filter(|&n| n > 0)).min() {
            return Some(limit);
        }
    }
    reports
        .map(|w| w.tokens)
        .min()
        .map(|limit| limit.min(ox_desk_core::handoff::DEFAULT_CONTEXT))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) scope: Scope,
    pub(crate) session: String,
    /// Unix seconds when the grant was given. Grants saved before this field existed read
    /// as 0 and are therefore expired.
    #[serde(default)]
    pub(crate) granted_at: i64,
}
/// How long a session's YOLO grant lasts.
pub(crate) const GRANT_SECS: i64 = 12 * 60 * 60;
impl Grant {
    pub(crate) fn expires_at(&self) -> i64 {
        self.granted_at.saturating_add(GRANT_SECS)
    }
    pub(crate) fn live(&self, now: i64) -> bool {
        now < self.expires_at()
    }
}
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) origin: ox_desk_core::origin::Origin,
    /// `declined` or `failed`.
    pub(crate) status: String,
    pub(crate) message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct HandoffRecord {
    pub(crate) id: String,
    pub(crate) target: Scope,
    /// The stamp the target receives. References only, no message text.
    pub(crate) origin: ox_desk_core::origin::Origin,
    /// `starting` until the target's first turn is sent, then removed, or `failed`.
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) message: Option<String>,
}
/// At most this many outcomes and failed handoffs are kept.
pub(crate) const KEPT: usize = 256;
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SavedScope {
    pub(crate) scope: Scope,
    pub(crate) sessions: Vec<SavedSession>,
    // Tokens suffice to retire old approvals. No commands or tool arguments on disk.
    pub(crate) pending: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SavedSession {
    id: String,
    title: String,
    parent: Option<String>,
    updated_at: i64,
    active: bool,
    /// The immutable worker stamp. No task, prompt or result is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<ox_desk_core::origin::Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    marks: Option<crate::workers::Marks>,
    /// The access mode a Claude session had, so the send that resumes it after a reap
    /// starts it the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<ox_desk_core::runtime::Access>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_result: Option<SessionResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_attachment: Option<ox_desk_core::memory::Attachment>,
}
impl SavedScope {
    pub(crate) fn from_state(s: &ScopeState) -> Self {
        let mut sessions: Vec<_> = s
            .sessions
            .iter()
            .filter(|r| {
                r.detail["owned"] == true || r.parent.is_some() || !r.detail["origin"].is_null()
            })
            .map(|r| {
                let access = |key: &str| {
                    serde_json::from_value::<Option<ox_desk_core::runtime::Access>>(
                        r.detail[key].clone(),
                    )
                    .ok()
                    .flatten()
                };
                SavedSession {
                    id: r.id.clone(),
                    title: r.title.chars().take(192).collect(),
                    parent: r.parent.clone(),
                    updated_at: r.updated_at,
                    active: matches!(
                        r.status.as_str(),
                        "running" | "needs_input" | "interrupted" | "Last seen running"
                    ),
                    origin: serde_json::from_value(r.detail["origin"].clone()).ok(),
                    marks: serde_json::from_value(r.detail["worker_marks"].clone()).ok(),
                    access: access("access").or_else(|| access("resume_access")),
                    last_result: r.last_result(),
                    memory_attachment: serde_json::from_value(
                        r.detail["memory_attachment"].clone(),
                    )
                    .ok(),
                }
            })
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let mut pending: Vec<_> = s.asks.iter().map(|a| a.token.clone()).collect();
        pending.sort();
        Self {
            scope: s.scope.clone(),
            sessions,
            pending,
        }
    }
    pub(crate) fn origins(
        &self,
    ) -> impl Iterator<Item = (&str, ox_desk_core::origin::Origin, crate::workers::Marks)> {
        self.sessions.iter().filter_map(|s| {
            Some((
                s.id.as_str(),
                s.origin.clone()?,
                s.marks.clone().unwrap_or_default(),
            ))
        })
    }
    pub(crate) fn recover(&self) -> ScopeState {
        ScopeState {
            scope: self.scope.clone(),
            connection: "disconnected".into(),
            capabilities: vec![],
            asks: vec![],
            transcripts: Default::default(),
            detail: serde_json::json!({"retired_approvals":self.pending.len()}),
            sessions: self
                .sessions
                .iter()
                .map(|s| {
                    let mut detail = serde_json::json!({
                        "owned": true,
                        "recovered": true,
                        "worker": s.origin.is_some(),
                        "origin": s.origin,
                        "worker_marks": s.marks,
                        "resume_access": s.access,
                    });
                    if let Some(attachment) = &s.memory_attachment {
                        detail["memory_attachment"] = serde_json::json!(attachment);
                    }
                    if let Some(result) = &s.last_result {
                        detail["last_result"] = serde_json::json!(result);
                    }
                    SessionView {
                        id: s.id.clone(),
                        title: s.title.clone(),
                        parent: s.parent.clone(),
                        updated_at: s.updated_at,
                        status: if s.active { "interrupted" } else { "idle" }.into(),
                        // No process is open, so the thread shows Ended and a send reopens it.
                        capabilities: {
                            let mut caps = vec!["open".into(), "send".into(), "ended".into()];
                            // Images stay attachable to a thread a send reopens.
                            if self.scope.route.runtime == ox_desk_core::runtime::Runtime::Codex {
                                caps.push("attach_image".into());
                            }
                            caps
                        },
                        detail,
                    }
                })
                .collect(),
        }
    }
}

/// The session ledger file in the host state directory.
pub(crate) const LEDGER: &str = "ownership-v1.json";

/// Keep the original ledger readable by a rolled-back host. It ignores the optional
/// extension, and its next write can discard that metadata. Antigravity's session
/// index and its native history live separately and survive such a rollback.
const ANTIGRAVITY: &str = "antigravity";

#[derive(Default, Serialize, Deserialize)]
struct AntigravityRecords {
    /// JSON pointers into the original ledger and the positions of extracted records.
    /// The parent list sorts before paths to lists in its restored records.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    arrays: BTreeMap<String, Vec<(usize, Value)>>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    forwarded: Map<String, Value>,
}

fn references_antigravity(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.get("runtime").and_then(Value::as_str) == Some(ANTIGRAVITY)
                || fields.values().any(references_antigravity)
        }
        Value::Array(values) => values.iter().any(references_antigravity),
        _ => false,
    }
}

impl AntigravityRecords {
    fn extract(
        &mut self,
        records: &mut Vec<Value>,
        path: String,
        matches: impl Fn(&Value) -> bool,
    ) {
        let mut index = 0;
        let mut extracted = Vec::new();
        records.retain_mut(|record| {
            let keep = !matches(record);
            if !keep {
                extracted.push((index, record.take()));
            }
            index += 1;
            keep
        });
        if !extracted.is_empty() {
            self.arrays.insert(path, extracted);
        }
    }

    fn restore(self, data: &mut Value) -> io::Result<()> {
        for (path, records) in self.arrays {
            let list = data
                .pointer_mut(&path)
                .and_then(Value::as_array_mut)
                .ok_or_else(|| io::Error::other("Invalid Antigravity ledger collection"))?;
            for (index, record) in records {
                if index > list.len() {
                    return Err(io::Error::other(
                        "Invalid Antigravity ledger record position",
                    ));
                }
                list.insert(index, record);
            }
        }
        if !self.forwarded.is_empty() {
            let forwarded = data["forwarded"]
                .as_object_mut()
                .ok_or_else(|| io::Error::other("Invalid Antigravity ledger forwarded jobs"))?;
            forwarded.extend(self.forwarded);
        }
        Ok(())
    }
}

fn encode_owned(data: &Owned) -> io::Result<Vec<u8>> {
    let mut json = serde_json::to_value(data)?;
    let mut extra = AntigravityRecords::default();
    let scopes = json["scopes"].as_array_mut().expect("serialized scopes");
    for (index, scope) in scopes.iter_mut().enumerate() {
        if !references_antigravity(&scope["scope"]) {
            extra.extract(
                scope["sessions"]
                    .as_array_mut()
                    .expect("serialized sessions"),
                format!("/scopes/{index}/sessions"),
                references_antigravity,
            );
        }
    }
    extra.extract(scopes, "/scopes".into(), |scope| {
        references_antigravity(&scope["scope"])
    });
    for path in [
        "/grants",
        "/asking",
        "/outcomes",
        "/handoffs",
        "/windows",
        "/worker_notices/pending",
        "/worker_notices/latest",
    ] {
        if let Some(records) = json.pointer_mut(path).and_then(Value::as_array_mut) {
            extra.extract(records, path.into(), references_antigravity);
        }
    }
    if let Some(jobs) = json.get_mut("forwarded").and_then(Value::as_object_mut) {
        jobs.retain(|id, job| {
            if references_antigravity(job) {
                extra.forwarded.insert(id.clone(), job.take());
                false
            } else {
                true
            }
        });
    }
    if !extra.arrays.is_empty() || !extra.forwarded.is_empty() {
        json[ANTIGRAVITY] = serde_json::to_value(extra)?;
    }
    Ok(serde_json::to_vec(&json)?)
}

fn decode_owned(reader: impl io::Read) -> io::Result<Owned> {
    let mut json: Value = serde_json::from_reader(reader)?;
    if let Some(extra) = json
        .as_object_mut()
        .and_then(|data| data.remove(ANTIGRAVITY))
    {
        serde_json::from_value::<AntigravityRecords>(extra)?.restore(&mut json)?;
    }
    Ok(serde_json::from_value(json)?)
}

pub(crate) struct Ledger {
    failed: bool,
    path: PathBuf,
    pub(crate) data: Owned,
}
impl Ledger {
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join(LEDGER);
        let data = match File::open(&path) {
            Ok(f) => decode_owned(BufReader::new(f))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Owned::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path,
            data,
            failed: false,
        })
    }
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }
    pub(crate) fn begin(&mut self, seq: u64) -> io::Result<()> {
        for scope in &mut self.data.scopes {
            scope.pending.clear();
        }
        self.reserve(seq)
    }
    pub(crate) fn next_sequence(&mut self, current: u64) -> io::Result<u64> {
        let next = current
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Host sequence exhausted"))?;
        self.reserve(next)?;
        Ok(next)
    }
    fn reserve(&mut self, seq: u64) -> io::Result<()> {
        if self.data.reserved <= seq {
            let old = self.data.reserved;
            self.data.reserved = seq
                .checked_add(4096)
                .ok_or_else(|| io::Error::other("Sequence exhausted"))?;
            if let Err(e) = self.save() {
                self.data.reserved = old;
                self.failed = true;
                return Err(e);
            }
        }
        Ok(())
    }
    /// Change the host-level records atomically, restoring them if the write fails.
    pub(crate) fn change(&mut self, f: impl FnOnce(&mut Owned)) -> io::Result<()> {
        let before = self.data.clone();
        f(&mut self.data);
        if self.data == before {
            return Ok(());
        }
        let trim = |n: usize| n.saturating_sub(KEPT);
        let drop = trim(self.data.outcomes.len());
        self.data.outcomes.drain(..drop);
        let failed = self
            .data
            .handoffs
            .iter()
            .filter(|h| h.status != "starting")
            .count();
        let mut excess = trim(failed);
        self.data.handoffs.retain(|h| {
            if excess > 0 && h.status != "starting" {
                excess -= 1;
                false
            } else {
                true
            }
        });
        if let Err(e) = self.save() {
            self.data = before;
            self.failed = true;
            return Err(e);
        }
        Ok(())
    }
    /// A new epoch never replays a launch: pending YOLO requests become declined
    /// and handoffs that had not started their first turn become failed.
    /// `cause` completes "The OX session host …", e.g. "restarted for an update".
    pub(crate) fn retire(&mut self, cause: &str) -> io::Result<()> {
        self.change(|data| {
            for origin in std::mem::take(&mut data.asking) {
                data.record_outcome(Outcome {
                    origin,
                    status: "declined".into(),
                    message: format!("The OX session host {cause} before the operator answered, so the YOLO request was declined. Start the worker again if it is still needed."),
                });
            }
            for h in &mut data.handoffs {
                if h.status == "starting" {
                    h.status = "failed".into();
                    h.message = Some(format!(
                        "The OX session host {cause} before the new session's first turn. Nothing was retried. Start the handoff again."
                    ));
                }
            }
        })
    }
    pub(crate) fn update(&mut self, state: SavedScope) -> io::Result<()> {
        let index = self.data.scopes.iter().position(|e| e.scope == state.scope);
        let previous = match index {
            Some(i) if self.data.scopes[i] == state => return Ok(()),
            Some(i) => Some(std::mem::replace(&mut self.data.scopes[i], state)),
            None => {
                self.data.scopes.push(state);
                None
            }
        };
        if let Err(e) = self.save() {
            if let (Some(i), Some(prior)) = (index, previous) {
                self.data.scopes[i] = prior;
            } else {
                self.data.scopes.pop();
            }
            self.failed = true;
            return Err(e);
        }
        Ok(())
    }
    fn save(&self) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        let bytes = encode_owned(&self.data)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(tmp, &self.path)?;
        // fsync is defined for directory descriptors on supported local filesystems.
        // Rust may use Darwin F_FULLFSYNC for File::sync_all, intended for regular files.
        let dir = File::open(self.path.parent().expect("state directory"))?;
        if unsafe { libc::fsync(std::os::fd::AsRawFd::as_raw_fd(&dir)) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn handoffs_match_the_destination_and_bound_an_unknown_model_conservatively() {
        use ox_desk_core::runtime::Runtime;
        let sub = Runtime::Claude.route(None);
        let api = Runtime::Codex.route(Some("fixture".into()));
        let mut windows = Vec::new();
        // Nothing seen and no catalog entry: the selection's 64k default applies.
        assert_eq!(handoff_window(&windows, &sub, None), None);
        assert_eq!(
            ox_desk_core::handoff::Budget::new(handoff_window(&windows, &sub, None), "").window,
            ox_desk_core::handoff::DEFAULT_CONTEXT
        );
        // The catalog alone is used for an API alias.
        assert_eq!(handoff_window(&windows, &api, Some(128_000)), Some(128_000));
        record_window(
            &mut windows,
            Window {
                route: sub.clone(),
                model: Some("opus".into()),
                tokens: 200_000,
            },
        );
        assert_eq!(handoff_window(&windows, &sub, None), Some(64_000));
        assert_eq!(
            handoff_window(&windows, &api, None),
            None,
            "windows are per route"
        );
        // A subscription can launch yet another model, independent of report order.
        record_window(
            &mut windows,
            Window {
                route: sub.clone(),
                model: Some("sonnet".into()),
                tokens: 1_000_000,
            },
        );
        record_window(
            &mut windows,
            Window {
                route: sub.clone(),
                model: Some("opus".into()),
                tokens: 180_000,
            },
        );
        assert_eq!(windows.len(), 2);
        assert_eq!(handoff_window(&windows, &sub, None), Some(64_000));
        // Reported and catalog windows together: the tighter one.
        record_window(
            &mut windows,
            Window {
                route: api.clone(),
                model: Some("fixture".into()),
                tokens: 272_000,
            },
        );
        assert_eq!(handoff_window(&windows, &api, Some(128_000)), Some(128_000));
        // A secondary model reported on this same route never overrides the destination.
        record_window(
            &mut windows,
            Window {
                route: api.clone(),
                model: Some("helper".into()),
                tokens: 1_000_000,
            },
        );
        assert_eq!(handoff_window(&windows, &api, None), Some(272_000));
        record_window(
            &mut windows,
            Window {
                route: api.clone(),
                model: Some("helper".into()),
                tokens: 8_000,
            },
        );
        assert_eq!(handoff_window(&windows, &api, Some(128_000)), Some(128_000));
        record_window(
            &mut windows,
            Window {
                route: sub.clone(),
                model: None,
                tokens: 32_000,
            },
        );
        assert_eq!(handoff_window(&windows, &sub, None), Some(32_000));
        for n in 0..100 {
            record_window(
                &mut windows,
                Window {
                    route: sub.clone(),
                    model: Some(format!("m{n}")),
                    tokens: 1,
                },
            );
        }
        assert_eq!(windows.len(), WINDOWS);
    }

    use super::*;
    use ox_desk_core::{
        host_protocol::{AskView, SessionOutcome, Timeline, TimelineItem},
        modes::Level,
        origin::{Origin, ParentRef, Relation, WorkerMode},
        runtime::{RequestId, Route, Runtime},
    };
    #[test]
    fn an_old_host_can_read_and_write_the_ledger_after_antigravity_use() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Ledger::open(tmp.path()).unwrap();
        let scope = |runtime: Runtime| Scope {
            project: "/project".into(),
            route: runtime.route(None),
        };
        let agy = scope(Runtime::Antigravity);
        let codex = scope(Runtime::Codex);
        let claude = scope(Runtime::Claude);
        let origin = Origin {
            linked: None,
            relation: Relation::Handoff,
            parent: ParentRef {
                project: agy.project.clone(),
                scope: Some(agy.clone()),
                session: Some("antigravity:native".into()),
                runtime: Some(Runtime::Antigravity),
            },
            launch: "operator".into(),
            by_operator: true,
            runtime: Runtime::Codex,
            route: codex.route.clone(),
            mode: WorkerMode::Session,
            job_id: 0,
            requested: Level::Read,
            ceiling: Level::Read,
            source: None,
            started: None,
            decided: None,
            yolo: None,
            handoff: None,
            requested_model: None,
            pinned_model: None,
        };
        let session = |id: &str, origin| SavedSession {
            id: id.into(),
            title: id.into(),
            parent: None,
            updated_at: 42,
            active: false,
            origin,
            marks: None,
            access: None,
            last_result: Some(SessionResult {
                turn: format!("{id}-turn"),
                completed_at: 41,
                outcome: SessionOutcome::Completed,
            }),
            memory_attachment: None,
        };
        let saved = |scope: &Scope, sessions| SavedScope {
            scope: scope.clone(),
            sessions,
            pending: vec![],
        };
        let codex_row = session("existing-codex", None);
        let claude_row = session("existing-claude", None);
        for state in [
            saved(&agy, vec![session("antigravity:native", None)]),
            saved(
                &codex,
                vec![
                    session("continued", Some(origin.clone())),
                    codex_row.clone(),
                ],
            ),
            saved(&claude, vec![claude_row.clone()]),
        ] {
            db.update(state).unwrap();
        }
        db.change(|data| {
            data.handoffs.push(HandoffRecord {
                id: "from-antigravity".into(),
                target: codex.clone(),
                origin,
                status: "failed".into(),
                message: None,
            });
            for scope in [&codex, &agy, &claude] {
                data.windows.push(Window {
                    route: scope.route.clone(),
                    model: None,
                    tokens: 64_000,
                });
            }
        })
        .unwrap();
        db.reserve(80).unwrap();
        let initial = db.data.clone();
        let path = tmp.path().join(LEDGER);
        let raw = fs::read(&path).unwrap();
        let mut legacy_json: Value = serde_json::from_slice(&raw).unwrap();
        assert!(
            legacy_json
                .as_object_mut()
                .unwrap()
                .remove(ANTIGRAVITY)
                .is_some()
        );
        assert!(!references_antigravity(&legacy_json));
        // The old ledger deserializes exactly these fields and ignores the extension.
        let mut legacy: Owned = serde_json::from_slice(&raw).unwrap();
        assert_eq!(legacy.reserved, initial.reserved);
        assert_eq!(legacy.scopes.len(), 2);
        assert!(legacy.scopes[0] == saved(&codex, vec![codex_row]));
        assert!(legacy.scopes[1] == saved(&claude, vec![claude_row]));
        assert_eq!(
            legacy.windows,
            [initial.windows[0].clone(), initial.windows[2].clone()]
        );
        assert!(legacy.handoffs.is_empty());
        assert!(Ledger::open(tmp.path()).unwrap().data == initial);

        // A write from the rolled-back binary can drop the optional metadata.
        // The next upgrade still sees the updated legacy watermark and rows.
        legacy.reserved += 4096;
        fs::write(path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(Ledger::open(tmp.path()).unwrap().data == legacy);
    }
    #[test]
    fn a_publication_at_the_exact_block_boundary_survives_host_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Ledger::open(tmp.path()).unwrap();
        db.begin(0).unwrap();
        let last = db.next_sequence(db.data.reserved - 1).unwrap();
        let mut restarted = Ledger::open(tmp.path()).unwrap();
        let initial = restarted.data.reserved;
        assert!(initial > last);
        restarted.begin(initial).unwrap();
        assert!(restarted.next_sequence(initial).unwrap() > initial);
        assert!(restarted.next_sequence(u64::MAX).is_err());
    }
    #[test]
    fn a_failed_reservation_never_advances_the_published_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = Ledger::open(tmp.path()).unwrap();
        db.begin(0).unwrap();
        let previous = db.data.reserved;
        fs::create_dir(db.path.with_extension("tmp")).unwrap();
        assert!(db.reserve(previous).is_err());
        assert!(db.failed());
        assert_eq!(db.data.reserved, previous);
        assert_eq!(Ledger::open(tmp.path()).unwrap().data.reserved, previous);
    }
    #[test]
    fn restart_retires_effects_and_preserves_only_metadata_and_a_monotonic_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: "/test".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let result = SessionResult {
            turn: "previous-turn".into(),
            completed_at: 2,
            outcome: SessionOutcome::Completed,
        };
        let old: SavedSession = serde_json::from_value(serde_json::json!({
            "id": "old", "title": "Before result metadata", "parent": null,
            "updated_at": 1, "active": false,
        }))
        .unwrap();
        assert!(old.last_result.is_none());
        let row = SessionView {
            id: "worker".into(),
            title: "inspect tests".into(),
            parent: Some("parent".into()),
            status: "running".into(),
            updated_at: 3,
            capabilities: vec!["stop".into()],
            detail: serde_json::json!({"owned":true,"authorization":"live-token","last_result":result}),
        };
        let ask = AskView {
            command: None,
            id: RequestId::Number(22),
            turn: None,
            item: None,
            choice_details: vec![],
            token: "ask-token".into(),
            thread: Some("worker".into()),
            title: "Review command".into(),
            detail: "sensitive command".into(),
            choices: vec![],
            questions: vec![],
        };
        let timeline = Timeline {
            items: vec![TimelineItem {
                id: "item".into(),
                turn: "turn".into(),
                role: "user".into(),
                text: "private transcript".into(),
                revision: 1,
                detail: serde_json::Value::Null,
            }],
            detail: serde_json::Value::Null,
        };
        let view = ScopeState {
            scope,
            connection: "ready".into(),
            sessions: vec![row],
            asks: vec![ask],
            transcripts: [("worker".into(), timeline)].into(),
            capabilities: vec![],
            detail: serde_json::Value::Null,
        };
        let mut db = Ledger::open(tmp.path()).unwrap();
        db.reserve(42).unwrap();
        db.update(SavedScope::from_state(&view)).unwrap();
        drop(db);
        let raw = fs::read_to_string(tmp.path().join("ownership-v1.json")).unwrap();
        for private in ["private transcript", "sensitive command", "live-token"] {
            assert!(!raw.contains(private));
        }
        let db = Ledger::open(tmp.path()).unwrap();
        assert!(db.data.reserved > 42);
        let restored = db.data.scopes[0].recover();
        assert_eq!(restored.sessions[0].status, "interrupted");
        assert_eq!(restored.sessions[0].last_result(), Some(result));
        assert_eq!(restored.sessions[0].parent.as_deref(), Some("parent"));
        // A recovered row has no process: it shows Ended and a send reopens it, and a Codex
        // thread keeps accepting image attachments for that send.
        assert_eq!(
            restored.sessions[0].capabilities,
            ["open", "send", "ended", "attach_image"]
        );
        assert!(restored.asks.is_empty());
        assert!(restored.transcripts.is_empty());
        assert_eq!(restored.detail["retired_approvals"], 1);
    }
}
