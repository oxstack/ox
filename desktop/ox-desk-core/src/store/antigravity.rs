//! Antigravity sessions on the official, user-installed ACP subprocess.
mod wire;

use super::{Connection, Profile, Store, ThreadRow, catalog::now_secs};
use crate::{
    asks::{Ask, Decision},
    changed::Changed,
    codex::activity::{Activity, Change},
    links::{Link, LinkEvent, antigravity as pipe},
    runtime::{Access, RequestId},
    slash::Capabilities,
    transcript::{
        DeltaTarget, Transcript,
        item::{Content, Item},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use wire::{SessionOptions, ToolUpdate};

pub const PROVIDER: &str = "antigravity";
const PREFIX: &str = "antigravity:";
const TURN_CONTEXT_URI: &str = "ox://turn-context";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Project {
    path: OsString,
    index: PathBuf,
    deliver: Arc<dyn Fn(LinkEvent) + Send + Sync>,
    sessions: HashMap<i64, Session>,
    permissions: HashMap<RequestId, (i64, RequestId)>,
}

#[derive(Debug)]
pub struct Event {
    generation: i64,
    event: pipe::Event,
}

impl Event {
    pub(crate) fn unblocks(&self) -> bool {
        !matches!(self.event, pipe::Event::Update(_))
    }
}

struct Session {
    link: pipe::Link,
    owner: Option<String>,
    first_prompt: Option<String>,
    start_mode: Option<Access>,
    embedded_context: bool,
    first_context: Option<String>,
    first_attachment: Option<crate::memory::Attachment>,
    handoff: bool,
    pending: Option<(i64, Pending, Option<Instant>)>,
    options: SessionOptions,
    stream: Stream,
}

enum Pending {
    Initialize,
    Open,
    StartMode(Access),
    Mode(Access),
    Model(String),
    Prompt(String, Option<crate::memory::Attachment>),
}

#[derive(Default)]
struct Stream {
    turn: u64,
    turn_floor: u64,
    serial: u64,
    chunk: Option<(Chunk, String)>,
    user: String,
    tools: HashMap<String, ToolUpdate>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Chunk {
    User,
    Agent,
    Thought,
}

fn native_id(id: &str) -> Option<&str> {
    id.strip_prefix(PREFIX).filter(|id| !id.is_empty())
}

/// OX only indexes metadata for sessions it creates. History and account data
/// stay with Antigravity and are accessed via ACP.
pub fn saved_rows(index: &Path) -> io::Result<Vec<ThreadRow>> {
    Ok(saved_sessions(index)?
        .into_iter()
        .map(|saved| saved.row)
        .collect())
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    #[serde(flatten)]
    row: ThreadRow,
    /// Replay can omit a cancelled turn. Reserve its number so the next live
    /// result still has a new identity in the host and clients.
    #[serde(default)]
    turn_floor: u64,
}

fn saved_sessions(index: &Path) -> io::Result<Vec<SavedSession>> {
    match File::open(index) {
        Ok(file) => serde_json::from_reader(file).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e),
    }
}

fn save_row(index: &Path, row: &ThreadRow, turn: u64) -> io::Result<()> {
    crate::state_file::update_preference_at(index, Vec::<SavedSession>::new, |mut rows| {
        if let Some(saved) = rows
            .iter_mut()
            .find(|r| r.row.id == row.id && r.row.cwd == row.cwd)
        {
            saved.row = row.clone();
            saved.turn_floor = saved.turn_floor.max(turn);
        } else {
            rows.push(SavedSession {
                row: row.clone(),
                turn_floor: turn,
            });
        }
        rows
    })
}

impl Session {
    fn request(&mut self, id: i64, kind: Pending, method: &str, params: Value) -> io::Result<()> {
        self.link.request(id, method, params)?;
        let deadline =
            (!matches!(kind, Pending::Prompt(..))).then(|| Instant::now() + CONTROL_TIMEOUT);
        self.pending = Some((id, kind, deadline));
        Ok(())
    }
}

impl Store {
    pub fn open_antigravity(
        profile: Profile,
        path: OsString,
        index: PathBuf,
        deliver: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut store = Self::with_link(
            profile,
            Link::Antigravity(Box::new(Project {
                path,
                index,
                deliver: Arc::new(deliver),
                sessions: HashMap::new(),
                permissions: HashMap::new(),
            })),
        );
        store.state = Connection::Ready;
        store.antigravity_list()?;
        Ok(store)
    }

    fn agy(&self) -> &Project {
        let Link::Antigravity(project) = &self.link else {
            unreachable!("Antigravity store")
        };
        project
    }

    fn agy_mut(&mut self) -> &mut Project {
        let Link::Antigravity(project) = &mut self.link else {
            unreachable!("Antigravity store")
        };
        project
    }

    pub(super) fn is_antigravity(&self) -> bool {
        matches!(self.link, Link::Antigravity(_))
    }

    fn agy_generation(&self, id: &str) -> Option<i64> {
        self.agy()
            .sessions
            .iter()
            .find_map(|(generation, session)| {
                (session.owner.as_deref() == Some(id)).then_some(*generation)
            })
    }

    pub(super) fn antigravity_opening(&self) -> bool {
        self.is_antigravity()
            && self
                .agy()
                .sessions
                .values()
                .any(|s| s.first_prompt.is_some())
    }

    /// A cancelled or expired handoff must not send its delayed first turn.
    pub fn drop_antigravity_handoff(&mut self) {
        if !self.is_antigravity() {
            return;
        }
        let generation = self
            .agy()
            .sessions
            .iter()
            .find_map(|(generation, session)| {
                (session.handoff && session.first_prompt.is_some()).then_some(*generation)
            });
        if let Some(generation) = generation {
            self.antigravity_remove(generation);
            self.changed.mark(Changed::THREADS);
        }
    }

    pub(super) fn antigravity_live(&self) -> Vec<String> {
        self.agy()
            .sessions
            .values()
            .filter_map(|s| s.owner.clone())
            .collect()
    }

    pub(super) fn antigravity_has_live(&self, id: &str) -> bool {
        self.agy_generation(id).is_some()
    }

    pub(super) fn antigravity_catalog(&self) -> io::Result<Vec<ThreadRow>> {
        let mut rows = saved_rows(&self.agy().index)?;
        rows.retain(|row| {
            row.cwd == self.profile.project
                && row.model_provider == PROVIDER
                && native_id(&row.id).is_some()
        });
        rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        Ok(rows)
    }

    pub(super) fn antigravity_list(&mut self) -> io::Result<()> {
        let query = self.query.to_lowercase();
        for row in self.antigravity_catalog()? {
            if self.catalog.get(&row.id).is_none()
                && (query.is_empty()
                    || row.title().to_lowercase().contains(&query)
                    || self.antigravity_has_live(&row.id))
            {
                self.catalog.insert(row);
            }
        }
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    pub(super) fn antigravity_reopen(&mut self) {
        for id in std::mem::take(&mut self.reopen) {
            if let Err(e) = self.antigravity_open(&id) {
                self.transcripts.entry(id.clone()).or_default().error = Some(e.to_string());
                self.changed.transcript(&id);
            }
        }
    }

    pub(super) fn antigravity_open(&mut self, id: &str) -> io::Result<()> {
        if self.antigravity_has_live(id) {
            return Ok(());
        }
        let saved = saved_sessions(&self.agy().index)?
            .into_iter()
            .find(|saved| {
                let row = &saved.row;
                row.id == id && row.cwd == self.profile.project && row.model_provider == PROVIDER
            });
        let saved = saved.filter(|_| native_id(id).is_some()).ok_or_else(|| {
            io::Error::other("This Antigravity session was not saved for this project.")
        })?;
        self.antigravity_spawn(Some(saved), None, self.host_start_access(id), false)?;
        self.mark_loading(id);
        Ok(())
    }

    fn antigravity_spawn(
        &mut self,
        saved: Option<SavedSession>,
        first_prompt: Option<String>,
        start_mode: Option<Access>,
        handoff: bool,
    ) -> io::Result<()> {
        self.seq += 1;
        let generation = self.seq;
        let deliver = Arc::clone(&self.agy().deliver);
        let link = pipe::Link::spawn(
            &self.agy().path,
            &self.profile.project,
            start_mode == Some(Access::Yolo),
            move |event| deliver(LinkEvent::Antigravity(Event { generation, event })),
        )?;
        let (first_context, first_attachment) = if first_prompt.is_some() {
            self.first_turn_context()
        } else {
            (None, None)
        };
        let (owner, turn_floor) =
            saved.map_or((None, 0), |saved| (Some(saved.row.id), saved.turn_floor));
        let mut session = Session {
            link,
            owner,
            first_prompt,
            start_mode,
            handoff,
            embedded_context: false,
            first_context,
            first_attachment,
            pending: None,
            options: SessionOptions::default(),
            stream: Stream {
                turn_floor,
                ..Stream::default()
            },
        };
        session.request(
            generation,
            Pending::Initialize,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name":"ox", "title":"OX", "version":env!("CARGO_PKG_VERSION")}
            }),
        )?;
        self.agy_mut().sessions.insert(generation, session);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    pub(super) fn antigravity_can_submit(&self, owner: Option<&str>) -> bool {
        match owner {
            None => !self.antigravity_opening(),
            Some(id) => self
                .agy_generation(id)
                .is_some_and(|generation| self.agy().sessions[&generation].pending.is_none()),
        }
    }

    pub(super) fn antigravity_handoff(&mut self, text: String) -> io::Result<()> {
        if !self.can_send(None) {
            return Err(io::Error::other("Wait for Antigravity to be ready."));
        }
        self.antigravity_spawn(None, Some(text), self.next_access(), true)?;
        self.next_preset = None;
        Ok(())
    }

    pub(super) fn antigravity_submit(&mut self, owner: Option<&str>) -> io::Result<()> {
        if !self.can_send(owner) {
            return Err(io::Error::other(
                "Wait for the Antigravity session to be ready.",
            ));
        }
        if !self.draft_images(owner).is_empty() {
            return Err(io::Error::other(
                "Remove the attachment before submitting to Antigravity.",
            ));
        }
        let text = self.draft(owner).to_owned();
        if text.trim().is_empty() {
            return Err(io::Error::other("Enter a message"));
        }
        if let Some(id) = owner {
            let generation = self.agy_generation(id).expect("ready session");
            self.antigravity_prompt(generation, text, self.sender_context(), None)?;
            self.restored_drafts.remove(id);
        } else {
            self.antigravity_spawn(None, Some(text), self.next_access(), false)?;
            self.next_preset = None;
        }
        self.edit(owner, String::new());
        Ok(())
    }

    fn antigravity_prompt(
        &mut self,
        generation: i64,
        text: String,
        context: Option<String>,
        attachment: Option<crate::memory::Attachment>,
    ) -> io::Result<()> {
        self.seq += 1;
        let seq = self.seq;
        let session = self
            .agy_mut()
            .sessions
            .get_mut(&generation)
            .expect("active session");
        let id = session.owner.clone().expect("prompt needs a session");
        let mut prompt = Vec::new();
        let mut sent_attachment = None;
        if session.embedded_context
            && let Some(context) = context
        {
            prompt.push(json!({
                "type": "resource",
                "resource": {"uri":TURN_CONTEXT_URI, "mimeType":"text/plain", "text":context},
                "annotations": {"audience":["assistant"]}
            }));
            sent_attachment = attachment;
        }
        prompt.push(json!({"type":"text","text":&text}));
        let params = json!({"sessionId": native_id(&id), "prompt":prompt});
        let number = session
            .stream
            .turn
            .max(session.stream.turn_floor)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Antigravity session turn numbers are exhausted."))?;
        session.request(
            seq,
            Pending::Prompt(text.clone(), sent_attachment),
            "session/prompt",
            params,
        )?;
        session.stream.turn = number;
        session.stream.chunk = None;
        session.stream.tools.clear();
        let turn = number.to_string();
        let transcript = self.transcripts.entry(id.clone()).or_default();
        transcript.error = None;
        transcript.apply_activity(Activity {
            thread: id.clone(),
            turn: turn.clone(),
            change: Change::TurnStarted,
        });
        transcript.observe_item(turn, "user".into(), user_item(text), true, None);
        if let Some(row) = self.catalog.get_mut(&id) {
            row.updated_at = now_secs();
            if let Err(e) = save_row(
                &self.agy().index,
                self.catalog.get(&id).expect("updated row"),
                number,
            ) {
                self.post_notice(format!("Could not save Antigravity session metadata: {e}"));
            }
        }
        self.changed.transcript(&id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    pub(super) fn antigravity_stop(&mut self, id: &str) -> io::Result<()> {
        let generation = self
            .agy_generation(id)
            .ok_or_else(|| io::Error::other("No active Antigravity session"))?;
        let turn = self
            .transcript(id)
            .and_then(|t| t.active_turn())
            .ok_or_else(|| io::Error::other("This session has no active turn"))?
            .to_owned();
        self.agy().sessions[&generation]
            .link
            .notify("session/cancel", json!({"sessionId":native_id(id)}))?;
        // ACP requires pending permission requests to be cancelled with a cancelled prompt.
        self.antigravity_clear_asks(generation, true);
        self.transcripts
            .get_mut(id)
            .expect("active transcript")
            .stop_requested = Some(turn);
        Ok(())
    }

    pub(super) fn antigravity_end(&mut self, id: &str) -> io::Result<()> {
        let generation = self
            .agy_generation(id)
            .ok_or_else(|| io::Error::other("This Antigravity session has no live process"))?;
        if self
            .transcript(id)
            .is_some_and(|t| t.active_turn().is_some())
        {
            let _ = self.antigravity_stop(id);
        }
        self.antigravity_remove(generation);
        self.ended_threads.insert(id.into());
        self.changed.mark(Changed::THREADS);
        self.changed.transcript(id);
        Ok(())
    }

    fn antigravity_remove(&mut self, generation: i64) -> Option<Session> {
        self.antigravity_clear_asks(generation, false);
        let session = self.agy_mut().sessions.remove(&generation)?;
        if let Some(id) = &session.owner
            && let Some(t) = self.transcripts.get_mut(id)
        {
            t.ready = false;
            t.verified = false;
            t.loading = false;
            t.interrupted |= t.active_turn.take().is_some();
        }
        Some(session)
    }

    pub(super) fn antigravity_caps(&self, id: Option<&str>) -> Capabilities {
        id.and_then(|id| self.agy_generation(id))
            .map(|generation| self.agy().sessions[&generation].options.capabilities())
            .unwrap_or_default()
    }

    pub(super) fn antigravity_access(&self, id: Option<&str>) -> Option<Access> {
        let generation = self.agy_generation(id?)?;
        let session = &self.agy().sessions[&generation];
        if session.start_mode == Some(Access::Yolo) {
            return self
                .transcript(id?)
                .filter(|t| t.ready && t.verified)
                .map(|_| Access::Yolo);
        }
        session.options.access()
    }

    pub(super) fn antigravity_access_choices(&self, id: &str) -> Vec<Access> {
        self.agy_generation(id)
            .map(|generation| &self.agy().sessions[&generation])
            .filter(|session| session.start_mode != Some(Access::Yolo))
            .map(|session| session.options.available_access())
            .unwrap_or_default()
    }

    pub(super) fn antigravity_configuring(&self, id: &str) -> bool {
        self.agy_generation(id).is_some_and(|generation| {
            matches!(
                self.agy().sessions[&generation].pending,
                Some((_, Pending::Mode(_) | Pending::Model(_), _))
            )
        })
    }

    pub(super) fn antigravity_set_access(&mut self, id: &str, access: Access) -> io::Result<()> {
        let generation = self
            .agy_generation(id)
            .ok_or_else(|| io::Error::other("Open an Antigravity session first."))?;
        let session = &self.agy().sessions[&generation];
        if access == Access::Yolo || session.start_mode == Some(Access::Yolo) {
            return Err(io::Error::other(
                "Choose YOLO when starting an Antigravity session. End this session to reopen it with regular permissions.",
            ));
        }
        if session.options.access() == Some(access) {
            return Ok(());
        }
        let (method, params) = session
            .options
            .set_access(native_id(id).expect("session ID"), access)
            .ok_or_else(|| io::Error::other("Antigravity did not advertise this mode."))?;
        self.antigravity_request(generation, Pending::Mode(access), method, params)
    }

    pub(super) fn antigravity_choose(
        &mut self,
        owner: Option<&str>,
        choice: super::routes_thread::Choice,
    ) -> io::Result<()> {
        let (Some(id), super::routes_thread::Choice::Model(model)) = (owner, choice) else {
            return Err(io::Error::other(
                "Choose an advertised model in an open Antigravity session.",
            ));
        };
        let generation = self
            .agy_generation(id)
            .ok_or_else(|| io::Error::other("Open an Antigravity session first."))?;
        let (method, params) = self.agy().sessions[&generation]
            .options
            .set_model(native_id(id).expect("session ID"), &model)
            .ok_or_else(|| io::Error::other("Antigravity did not advertise this model."))?;
        self.antigravity_request(generation, Pending::Model(model), method, params)
    }

    fn antigravity_request(
        &mut self,
        generation: i64,
        pending: Pending,
        method: &str,
        params: Value,
    ) -> io::Result<()> {
        self.seq += 1;
        let seq = self.seq;
        self.agy_mut()
            .sessions
            .get_mut(&generation)
            .expect("active session")
            .request(seq, pending, method, params)?;
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    pub(crate) fn antigravity_deadline(&self) -> Option<Instant> {
        self.is_antigravity()
            .then(|| {
                self.agy()
                    .sessions
                    .values()
                    .filter_map(|s| s.pending.as_ref()?.2)
                    .min()
            })
            .flatten()
    }

    pub(super) fn antigravity_tick(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .agy()
            .sessions
            .iter()
            .filter_map(|(generation, s)| (s.pending.as_ref()?.2? <= now).then_some(*generation))
            .collect();
        for generation in expired {
            self.antigravity_failed(
                generation,
                "Antigravity did not answer its control request.".into(),
            );
        }
    }

    fn antigravity_failed(&mut self, generation: i64, why: String) {
        let Some(session) = self.antigravity_remove(generation) else {
            return;
        };
        let closed_owner = session
            .owner
            .as_ref()
            .filter(|_| session.first_prompt.is_none());
        if let Some(id) = closed_owner {
            self.ended_threads.insert(id.clone());
            let t = self.transcripts.entry(id.clone()).or_default();
            t.error = Some(why.clone());
            if self.resume_sends.remove(id) {
                let text = self.draft(Some(id)).to_owned();
                if !text.is_empty() {
                    self.restored_drafts.insert(
                        id.clone(),
                        (format!("antigravity-resume-{generation}"), text),
                    );
                }
            }
            self.resume_senders.remove(id);
            self.changed.transcript(id);
        }
        if let Some(text) = session.first_prompt
            && !session.handoff
        {
            if self.next_preset.is_none() {
                self.next_preset = session.start_mode;
            }
            let newer = self.draft(None);
            let restored = if newer.is_empty() {
                text
            } else {
                format!("{text}\n\n{newer}")
            };
            self.edit(None, restored);
        }
        if why.starts_with("Antigravity ACP error -32000: Authentication required") {
            self.state = Connection::SignInRequired;
            self.changed.mark(Changed::CONNECTION);
            self.post_notice(
                "Sign in on this host with `agy acp --login`, then check the connection.",
            );
        } else {
            self.post_notice(&why);
        }
        self.changed.mark(Changed::THREADS);
    }

    pub(super) fn antigravity_event(
        &mut self,
        Event { generation, event }: Event,
    ) -> io::Result<()> {
        if !self.agy().sessions.contains_key(&generation) {
            return Ok(());
        }
        match event {
            pipe::Event::Closed(reason) => self.antigravity_failed(generation, reason),
            pipe::Event::Reply { id, result } => {
                let session = self
                    .agy_mut()
                    .sessions
                    .get_mut(&generation)
                    .expect("active session");
                if session.pending.as_ref().is_none_or(|p| p.0 != id) {
                    return Ok(());
                }
                let (_, pending, _) = session.pending.take().expect("matched request");
                if let Err(e) = self.antigravity_reply(generation, pending, result) {
                    self.antigravity_failed(generation, e.to_string());
                }
            }
            pipe::Event::Update(params) => self.antigravity_update(generation, params),
            pipe::Event::Permission { id, params } => {
                self.antigravity_permission(generation, id, params)?
            }
        }
        Ok(())
    }

    fn antigravity_reply(
        &mut self,
        generation: i64,
        pending: Pending,
        result: Result<Value, String>,
    ) -> io::Result<()> {
        let get_owner = |store: &Self| store.agy().sessions[&generation].owner.clone();
        let response = match (pending, result) {
            (Pending::Prompt(text, attachment), result) => {
                let id = get_owner(self).expect("prompt session");
                if result.is_ok()
                    && let Some(attachment) = attachment
                {
                    self.remember_memory_attachment(&id, attachment);
                }
                self.antigravity_finish(generation, &id, text, result);
                return Ok(());
            }
            (Pending::Mode(_) | Pending::Model(_), Err(error)) => {
                self.post_notice(&error);
                self.changed.mark(Changed::THREADS);
                return Ok(());
            }
            (pending, result) => (pending, result.map_err(io::Error::other)?),
        };
        let (pending, value) = response;
        match pending {
            Pending::Initialize => {
                if value["protocolVersion"].as_u64() != Some(1) {
                    return Err(io::Error::other(
                        "Antigravity did not negotiate ACP protocol version 1.",
                    ));
                }
                self.agy_mut()
                    .sessions
                    .get_mut(&generation)
                    .expect("active session")
                    .embedded_context =
                    value["agentCapabilities"]["promptCapabilities"]["embeddedContext"] == true;
                let id = get_owner(self);
                let mut params = json!({"cwd": self.profile.project, "mcpServers":[]});
                let method = if let Some(id) = id.as_deref() {
                    if value["agentCapabilities"]["loadSession"] != true {
                        return Err(io::Error::other(
                            "This Antigravity CLI cannot load saved sessions. Update agy and retry.",
                        ));
                    }
                    params["sessionId"] = json!(native_id(id));
                    // session/load streams history before it replies. Reset the old view once
                    // the agent has confirmed it supports replay, so it cannot append duplicates.
                    if let Some(t) = self.transcripts.get_mut(id) {
                        let previous = t.len();
                        *t = Transcript::default();
                        t.loading = true;
                        t.host_history_replaced(previous);
                    }
                    "session/load"
                } else {
                    "session/new"
                };
                self.antigravity_request(generation, Pending::Open, method, params)?;
            }
            Pending::Open => {
                let session = self
                    .agy_mut()
                    .sessions
                    .get_mut(&generation)
                    .expect("active session");
                session.options.apply(&value);
                if session.owner.is_none() {
                    let native = value["sessionId"]
                        .as_str()
                        .filter(|id| !id.trim().is_empty() && !id.chars().any(char::is_control))
                        .ok_or_else(|| {
                            io::Error::other("Antigravity returned no usable session ID.")
                        })?;
                    session.owner = Some(format!("{PREFIX}{native}"));
                }
                if let Some(mode) = session.start_mode
                    && mode != Access::Yolo
                    && session.options.access() != Some(mode)
                {
                    let native = session
                        .owner
                        .as_deref()
                        .and_then(native_id)
                        .expect("known session");
                    let (method, params) = session.options.set_access(native, mode)
                        .ok_or_else(|| io::Error::other("Antigravity does not offer the requested starting mode. Update agy and retry."))?;
                    return self.antigravity_request(
                        generation,
                        Pending::StartMode(mode),
                        method,
                        params,
                    );
                }
                self.antigravity_ready(generation)?;
            }
            Pending::Mode(access) | Pending::StartMode(access) => {
                let session = self
                    .agy_mut()
                    .sessions
                    .get_mut(&generation)
                    .expect("active session");
                session.options.apply(&value);
                if value.get("configOptions").is_none() {
                    session.options.confirm_legacy_access(access);
                }
                if session.options.access() != Some(access) {
                    return Err(io::Error::other(
                        "Antigravity did not confirm the requested mode.",
                    ));
                }
                if matches!(pending, Pending::StartMode(_)) {
                    self.antigravity_ready(generation)?;
                } else if let Some(id) = get_owner(self) {
                    self.changed.transcript(&id);
                }
            }
            Pending::Model(model) => {
                let session = self
                    .agy_mut()
                    .sessions
                    .get_mut(&generation)
                    .expect("active session");
                session.options.apply(&value);
                if value.get("configOptions").is_none() {
                    session.options.confirm_legacy_model(&model);
                }
                let id = session.owner.clone().expect("model session");
                self.antigravity_publish_options(&id);
            }
            Pending::Prompt(..) => unreachable!("processed separately"),
        }
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    fn antigravity_ready(&mut self, generation: i64) -> io::Result<()> {
        let session = self
            .agy_mut()
            .sessions
            .get_mut(&generation)
            .expect("active session");
        let id = session.owner.clone().expect("open session");
        let first = session.first_prompt.clone();
        let first_context = session.first_context.take();
        let first_attachment = session.first_attachment.take();
        session.stream.chunk = None;
        let t = self.transcripts.entry(id.clone()).or_default();
        t.loading = false;
        t.ready = true;
        t.verified = true;
        t.error = None;
        self.ended_threads.remove(&id);
        if let Some(text) = first {
            let request = crate::handoff::split_claude_turn(&text)
                .map_or(text.as_str(), |(_, request)| request);
            let row: ThreadRow = serde_json::from_value(json!({
                "id":id, "cwd":self.profile.project, "modelProvider":PROVIDER,
                "preview":request.lines().find(|s| !s.trim().is_empty()).unwrap_or("").chars().take(120).collect::<String>(),
                "updatedAt":now_secs()
            })).map_err(io::Error::other)?;
            self.catalog.insert(row);
            self.catalog_revision += 1;
            if let Err(err) =
                self.antigravity_prompt(generation, text, first_context, first_attachment)
            {
                self.catalog.remove(&id);
                return Err(err);
            }
            self.agy_mut()
                .sessions
                .get_mut(&generation)
                .expect("active session")
                .first_prompt = None;
            self.host_opened = Some(id.clone());
            self.selected = Some(id.clone());
        }
        self.antigravity_publish_options(&id);
        self.changed.transcript(&id);
        if self.resume_sends.contains(&id) {
            self.flush_resume_sends();
        }
        Ok(())
    }

    fn antigravity_publish_options(&mut self, id: &str) {
        let caps = self.antigravity_caps(Some(id));
        if let Some(t) = self.transcripts.get_mut(id) {
            t.model = (!caps.model.is_empty()).then_some(caps.model);
        }
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
    }

    fn antigravity_finish(
        &mut self,
        generation: i64,
        id: &str,
        text: String,
        result: Result<Value, String>,
    ) {
        self.antigravity_clear_asks(generation, true);
        let session = self
            .agy_mut()
            .sessions
            .get_mut(&generation)
            .expect("active session");
        let turn = session.stream.turn.to_string();
        let (interrupted, error) = match result {
            Ok(value) => match value["stopReason"].as_str() {
                Some("cancelled") => (true, None),
                Some("end_turn" | "max_tokens" | "max_turn_requests") => (false, None),
                Some("refusal") => (false, Some("Antigravity refused the request.".into())),
                _ => (
                    false,
                    Some("Antigravity returned an unknown stop reason.".into()),
                ),
            },
            Err(error) => {
                let restored = self.draft(Some(id));
                let draft = if restored.is_empty() {
                    text.clone()
                } else {
                    format!("{text}\n{restored}")
                };
                self.edit(Some(id), draft);
                self.restored_drafts.insert(
                    id.into(),
                    (format!("antigravity-{generation}-{turn}"), text),
                );
                (false, Some(error))
            }
        };
        if let Some(t) = self.transcripts.get_mut(id) {
            t.apply_activity(Activity {
                thread: id.into(),
                turn: turn.clone(),
                change: Change::TurnCompleted { error, interrupted },
            });
        }
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
    }

    fn antigravity_update(&mut self, generation: i64, params: Value) {
        let Some((native, update)) = wire::update(params) else {
            return;
        };
        let Link::Antigravity(project) = &mut self.link else {
            return;
        };
        let session = project
            .sessions
            .get_mut(&generation)
            .expect("active session");
        let Some(id) = session
            .owner
            .clone()
            .filter(|id| native_id(id) == Some(&native))
        else {
            return;
        };
        let replay = matches!(session.pending, Some((_, Pending::Open, _)))
            || session.first_prompt.is_some();
        match update {
            wire::Update::Options(value) => {
                session.options.apply(&value);
                self.antigravity_publish_options(&id);
            }
            wire::Update::Metadata { title: name } => {
                if let Some(row) = self.catalog.get_mut(&id) {
                    row.name = name;
                    let row = row.clone();
                    if let Err(e) = save_row(&self.agy().index, &row, 0) {
                        self.post_notice(format!(
                            "Could not save Antigravity session metadata: {e}"
                        ));
                    }
                    self.catalog_revision += 1;
                    self.changed.mark(Changed::THREADS);
                }
            }
            wire::Update::Usage { used, size } => {
                let total_tokens = i64::try_from(used).unwrap_or(i64::MAX);
                let tokens = || crate::usage::TokenCounts {
                    total_tokens,
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                    reasoning_output_tokens: 0,
                };
                if let Some(t) = self.transcripts.get_mut(&id) {
                    t.usage = Some(crate::usage::TokenUsage {
                        total: tokens(),
                        last: tokens(),
                        model_context_window: i64::try_from(size).ok(),
                    });
                    if size > 0 {
                        self.context_windows.push((t.model.clone(), size));
                    }
                    self.changed.transcript(&id);
                }
            }
            update => {
                let stream = &mut session.stream;
                if let Some(t) = self.transcripts.get_mut(&id)
                    && (replay || t.active_turn().is_some())
                {
                    stream.observe(t, update, replay);
                    self.changed.transcript(&id);
                }
            }
        }
    }

    fn antigravity_permission(
        &mut self,
        generation: i64,
        id: RequestId,
        params: Value,
    ) -> io::Result<()> {
        let session = &self.agy().sessions[&generation];
        let valid = session.owner.as_deref().filter(|owner| {
            self.transcript(owner)
                .is_some_and(|t| t.active_turn().is_some() && t.stop_requested.is_none())
        });
        let Some((p, owner)) = wire::permission(params)
            .zip(valid)
            .filter(|(p, owner)| native_id(owner) == Some(&p.session) && !p.choices.is_empty())
        else {
            return session.link.reply(&id, cancelled());
        };
        let owner = owner.to_owned();
        let turn = session.stream.turn.to_string();
        self.seq += 1;
        let ask_id = RequestId::Text(format!("antigravity-{}", self.seq));
        let decline = p.choices.iter().position(|choice| choice.reject);
        let command =
            p.command
                .zip(decline)
                .filter(|_| !p.is_question)
                .map(|((command, cwd), decline)| crate::asks::CommandApproval {
                    command,
                    cwd,
                    decline,
                });
        let decisions = p
            .choices
            .into_iter()
            .map(|option| {
                let mut decision = Decision::new(
                    option.label,
                    json!({
                        "outcome": {"outcome":"selected","optionId":option.id}
                    }),
                );
                decision.detail = option.detail;
                decision
            })
            .collect();
        self.asks.push(Ask {
            id: ask_id.clone(),
            thread: Some(owner),
            turn: Some(turn),
            item: Some(p.tool_id),
            title: p.title,
            detail: p.detail,
            command,
            questions: vec![],
            decisions,
        });
        self.agy_mut().permissions.insert(ask_id, (generation, id));
        self.changed.mark(Changed::ASK | Changed::THREADS);
        Ok(())
    }

    pub(super) fn antigravity_answer(&mut self, id: &RequestId, answer: Value) -> io::Result<()> {
        let (generation, wire) = self
            .agy()
            .permissions
            .get(id)
            .ok_or_else(|| io::Error::other("Antigravity permission already resolved"))?;
        self.agy().sessions[generation].link.reply(wire, answer)?;
        self.agy_mut().permissions.remove(id);
        Ok(())
    }

    fn antigravity_clear_asks(&mut self, generation: i64, send_cancel: bool) {
        let ids: Vec<_> = self
            .agy()
            .permissions
            .iter()
            .filter_map(|(key, (owner, wire))| {
                (*owner == generation).then_some((key.clone(), wire.clone()))
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        for (key, wire) in ids {
            if send_cancel && let Some(session) = self.agy().sessions.get(&generation) {
                let _ = session.link.reply(&wire, cancelled());
            }
            self.agy_mut().permissions.remove(&key);
            self.asks.retain(|ask| ask.id != key);
        }
        self.changed.mark(Changed::ASK | Changed::THREADS);
    }
}

fn user_item(text: String) -> Item {
    Item::UserMessage {
        content: vec![Content {
            text: Some(text),
            path: None,
        }],
    }
}

fn cancelled() -> Value {
    json!({"outcome":{"outcome":"cancelled"}})
}

impl Stream {
    fn observe(&mut self, t: &mut Transcript, update: wire::Update, replay: bool) {
        use wire::Update;
        match update {
            Update::UserText { message_id, text } if replay => {
                if !matches!(&self.chunk, Some((Chunk::User, id)) if message_id.as_ref().is_none_or(|message| *id == format!("User:{message}")))
                {
                    self.turn += 1;
                    self.user.clear();
                    self.tools.clear();
                }
                let (turn, item) = self.chunk(Chunk::User, message_id);
                self.user.push_str(&text);
                t.observe_item(turn, item, user_item(self.user.clone()), true, None);
            }
            Update::AgentText { message_id, text } => {
                let (turn, item) = self.chunk(Chunk::Agent, message_id);
                t.observe_delta(turn, item, DeltaTarget::Agent, text);
            }
            Update::Thought { message_id, text } => {
                let (turn, item) = self.chunk(Chunk::Thought, message_id);
                t.observe_delta(turn, item, DeltaTarget::Summary(0), text);
            }
            Update::Plan(text) => self.observe_plan(t, text),
            Update::Tool(update) => {
                self.chunk = None;
                let id = update.id().to_owned();
                let plan = update.plan();
                let tool = match self.tools.entry(id.clone()) {
                    std::collections::hash_map::Entry::Occupied(mut old) => {
                        old.get_mut().merge(update);
                        old.into_mut()
                    }
                    std::collections::hash_map::Entry::Vacant(new) => new.insert(update),
                };
                if tool.is_plan() {
                    if let Some(plan) = plan {
                        self.observe_plan(t, plan);
                    }
                } else {
                    t.observe_item(
                        self.turn.to_string(),
                        format!("tool:{id}"),
                        tool.item(),
                        true,
                        tool.output(),
                    );
                }
            }
            _ => {}
        }
    }

    fn observe_plan(&mut self, t: &mut Transcript, text: String) {
        self.chunk = None;
        t.observe_item(
            self.turn.to_string(),
            "plan".into(),
            Item::Plan { text },
            true,
            None,
        );
    }

    fn chunk(&mut self, kind: Chunk, message_id: Option<String>) -> (String, String) {
        let message_id = message_id.map(|id| format!("{kind:?}:{id}"));
        if self.chunk.as_ref().is_none_or(|(current, id)| {
            *current != kind || message_id.as_ref().is_some_and(|message| message != id)
        }) {
            self.serial += 1;
            self.chunk = Some((
                kind,
                message_id.unwrap_or_else(|| format!("chunk:{}", self.serial)),
            ));
        }
        (
            self.turn.to_string(),
            self.chunk.as_ref().expect("new chunk").1.clone(),
        )
    }
}
