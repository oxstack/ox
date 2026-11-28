//! Workbench projection for host-owned OpenCode sessions.
use super::{Connection, Profile, Store, ThreadRow, replica};
use crate::{
    actions::Action,
    asks::{Ask, Decision},
    changed::Changed,
    host_protocol::{AskView, SessionView, Timeline},
    links::Link,
    runtime::{Billing, RequestId},
    transcript::Transcript,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::mpsc::SyncSender,
};

const MAX_PROMPT_BYTES: usize = 1024 * 1024;

/// Native I/O stays off the host actor. This transport is internal to the host.
pub enum Command {
    Select(String),
    Send(Option<String>, String),
    LoadOlder(String),
    Answer(String, RequestId, String, usize),
    Stop(String, u64),
}

#[derive(Debug)]
pub enum Event {
    Ready(Vec<SessionView>),
    Snapshot {
        session: Box<SessionView>,
        transcript: Timeline,
        asks: Vec<AskView>,
        sequence: Option<u64>,
    },
    Finished {
        key: String,
        result: Result<Option<String>, String>,
    },
    StreamText {
        thread: String,
        part: String,
        delta: String,
    },
    StreamLost,
    Closed(String),
}

struct Pending {
    sending: Option<Option<String>>,
    submitted: Option<String>,
}

pub type Completion = (String, Result<Option<String>, String>);

pub struct Project {
    tx: SyncSender<(String, Command)>,
    pending: HashMap<String, Pending>,
    completed: Vec<Completion>,
    sessions: HashMap<String, SessionView>,
    timelines: HashMap<String, Timeline>,
    asks: HashMap<String, Vec<AskView>>,
    sequences: HashMap<String, u64>,
    volatile: HashMap<String, HashSet<String>>,
}

impl Store {
    pub fn open_opencode(profile: Profile, tx: SyncSender<(String, Command)>) -> Self {
        Self::with_link(
            profile,
            Link::OpenCode(Box::new(Project {
                tx,
                pending: HashMap::new(),
                completed: Vec::new(),
                sessions: HashMap::new(),
                timelines: HashMap::new(),
                asks: HashMap::new(),
                sequences: HashMap::new(),
                volatile: HashMap::new(),
            })),
        )
    }

    pub(crate) fn is_opencode(&self) -> bool {
        matches!(self.link, Link::OpenCode(_))
    }

    fn opencode(&self) -> &Project {
        let Link::OpenCode(project) = &self.link else {
            unreachable!("OpenCode link required")
        };
        project
    }

    fn opencode_mut(&mut self) -> &mut Project {
        let Link::OpenCode(project) = &mut self.link else {
            unreachable!("OpenCode link required")
        };
        project
    }

    pub fn opencode_has_pending(&self, key: &str) -> bool {
        self.opencode().pending.contains_key(key)
    }

    pub fn take_opencode_completions(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.opencode_mut().completed)
    }

    pub(super) fn opencode_sending(&self, id: Option<&str>) -> bool {
        self.opencode().pending.values().any(|p| {
            p.sending
                .as_ref()
                .is_some_and(|owner| owner.as_deref() == id)
        })
    }

    pub(super) fn opencode_pending_turn(&self) -> bool {
        self.is_opencode()
            && self
                .opencode()
                .pending
                .values()
                .any(|p| p.sending.is_some())
    }

    pub(super) fn opencode_can_send(&self, id: Option<&str>) -> bool {
        self.state == Connection::Ready
            && !self.opencode_sending(id)
            && id.is_none_or(|id| {
                self.transcripts
                    .get(id)
                    .is_some_and(|t| t.ready && t.active_turn().is_none())
                    && self
                        .opencode()
                        .sessions
                        .get(id)
                        .is_some_and(|session| matches!(session.status.as_str(), "idle" | "failed"))
                    && !self
                        .asks
                        .iter()
                        .any(|ask| ask.thread.as_deref() == Some(id))
            })
    }

    pub fn dispatch_opencode(&mut self, key: &str, action: Action) -> io::Result<()> {
        let mut pending = Pending {
            sending: None,
            submitted: None,
        };
        let command = match action {
            Action::NewDraft => {
                self.new_session();
                self.changed.mark(Changed::CONNECTION);
                return Ok(());
            }
            Action::DraftEdit(id, text) => {
                self.edit(id.as_deref(), text);
                return Ok(());
            }
            Action::DismissNotice => {
                self.dismiss_notice();
                return Ok(());
            }
            Action::OpenCatalogRow(row) => Command::Select(row.id),
            Action::SelectThread(id) => Command::Select(id),
            Action::LoadOlder(id) => Command::LoadOlder(id),
            Action::Stop(id) => {
                let sequence = self
                    .opencode()
                    .sequences
                    .get(&id)
                    .copied()
                    .ok_or_else(|| io::Error::other("No OpenCode turn is available to stop"))?;
                Command::Stop(id, sequence)
            }
            Action::AnswerAsk(id, choice) => {
                let (thread, ask) = self
                    .opencode()
                    .asks
                    .iter()
                    .find_map(|(thread, asks)| {
                        asks.iter()
                            .find(|ask| ask.id == id)
                            .map(|ask| (thread, ask))
                    })
                    .ok_or_else(|| io::Error::other("OpenCode approval is no longer pending"))?;
                if choice >= ask.choices.len() {
                    return Err(io::Error::other("Invalid OpenCode approval choice"));
                }
                Command::Answer(thread.clone(), id, ask.token.clone(), choice)
            }
            Action::Submit(id) => {
                let text = self.draft(id.as_deref()).to_owned();
                pending.sending = Some(id.clone());
                pending.submitted = Some(text.clone());
                Command::Send(id, text)
            }
            Action::SendText(id, text) => {
                pending.sending = Some(id.clone());
                Command::Send(id, text)
            }
            _ => {
                return Err(io::Error::other(
                    "This action is unavailable for hosted OpenCode",
                ));
            }
        };
        if self.state != Connection::Ready {
            return Err(io::Error::other("OpenCode is connecting or offline"));
        }
        match &command {
            Command::Send(owner, text) => {
                if !self.can_send(owner.as_deref()) {
                    return Err(io::Error::other(
                        "Open this OpenCode session before sending, or wait for its current turn",
                    ));
                }
                if text.trim().is_empty() || text.len() > MAX_PROMPT_BYTES {
                    return Err(io::Error::other("Invalid OpenCode prompt size"));
                }
            }
            Command::Select(id)
            | Command::LoadOlder(id)
            | Command::Stop(id, _)
            | Command::Answer(id, ..)
                if self.catalog.get(id).is_none() =>
            {
                return Err(io::Error::other(
                    "OpenCode session is absent from this host route",
                ));
            }
            _ => (),
        }
        let select = match &command {
            Command::Select(id) => Some(id.clone()),
            _ => None,
        };
        self.opencode()
            .tx
            .try_send((key.into(), command))
            .map_err(|_| io::Error::other("OpenCode is busy or offline"))?;
        if let Some(id) = select {
            self.selected = Some(id);
        }
        if let Some(owner) = &pending.sending {
            self.opencode_send_changed(owner.as_deref());
        }
        self.opencode_mut().pending.insert(key.into(), pending);
        Ok(())
    }

    fn opencode_send_changed(&mut self, owner: Option<&str>) {
        if let Some(id) = owner {
            self.changed.transcript(id);
        }
        self.changed.mark(Changed::THREADS | Changed::CONNECTION);
    }

    pub(super) fn opencode_event(&mut self, event: Event) -> io::Result<()> {
        match event {
            Event::Ready(rows) => {
                for row in rows {
                    self.opencode_row(row);
                }
                self.state = Connection::Ready;
                self.changed.mark(Changed::CONNECTION);
            }
            Event::Snapshot {
                session,
                transcript,
                asks,
                sequence,
            } => {
                if let Some(sequence) = sequence {
                    self.opencode_mut()
                        .sequences
                        .insert(session.id.clone(), sequence);
                }
                self.opencode_snapshot(*session, transcript, asks)?;
            }
            Event::Finished { key, result } => {
                if let Some(pending) = self.opencode_mut().pending.remove(&key) {
                    if let Some(owner) = pending.sending {
                        if let Some(submitted) = &pending.submitted
                            && result.is_ok()
                            && self.draft(owner.as_deref()) == submitted
                        {
                            self.edit(owner.as_deref(), String::new());
                        }
                        self.opencode_send_changed(owner.as_deref());
                    }
                    if let Ok(Some(id)) = &result {
                        self.selected = Some(id.clone());
                    }
                    self.opencode_mut().completed.push((key, result));
                }
            }
            Event::StreamText {
                thread,
                part,
                delta,
            } => {
                self.opencode_stream_text(thread, part, delta);
            }
            Event::StreamLost => {
                self.opencode_reset_streams()?;
            }
            Event::Closed(reason) => {
                self.opencode_reset_streams()?;
                self.state = Connection::Closed(reason.clone());
                self.asks.clear();
                self.opencode_mut().asks.clear();
                for (id, t) in &mut self.transcripts {
                    t.active_turn = None;
                    t.ready = false;
                    t.verified = false;
                    self.changed.transcript(id);
                }
                let pending = std::mem::take(&mut self.opencode_mut().pending);
                self.opencode_mut()
                    .completed
                    .extend(pending.into_keys().map(|key| (key, Err(reason.clone()))));
                self.changed
                    .mark(Changed::CONNECTION | Changed::THREADS | Changed::ASK);
            }
        }
        Ok(())
    }

    fn opencode_stream_text(&mut self, thread: String, part: String, delta: String) {
        let Some(turn) = self.opencode().timelines.get(&thread).and_then(|timeline| {
            let active = timeline.detail["active"].as_str()?;
            timeline
                .items
                .iter()
                .rev()
                .find(|item| {
                    item.id == part
                        && item.role == "assistant"
                        && item.turn == active
                        && item.text.is_empty()
                })
                .map(|item| item.turn.clone())
        }) else {
            return;
        };
        let Some(transcript) = self.transcripts.get_mut(&thread) else {
            return;
        };
        transcript.observe_delta(
            turn,
            part.clone(),
            crate::transcript::DeltaTarget::Agent,
            delta,
        );
        self.changed.transcript(&thread);
        self.opencode_mut()
            .volatile
            .entry(thread)
            .or_default()
            .insert(part);
    }

    fn opencode_reset_streams(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.opencode_mut().volatile);
        for (thread, parts) in pending {
            self.opencode_reset_parts(&thread, &parts)?;
        }
        Ok(())
    }

    fn opencode_reset_parts(&mut self, thread: &str, parts: &HashSet<String>) -> io::Result<()> {
        let authoritative: Vec<_> = self
            .opencode()
            .timelines
            .get(thread)
            .into_iter()
            .flat_map(|timeline| {
                timeline
                    .items
                    .iter()
                    .filter(|item| parts.contains(&item.id))
                    .cloned()
            })
            .collect();
        let Some(t) = self.transcripts.get_mut(thread) else {
            return Ok(());
        };
        for mut item in authoritative {
            let key = crate::transcript::ItemKey {
                turn: item.turn.clone(),
                item: item.id.clone(),
            };
            if let Some(entry) = t.entry(&key)
                && let crate::transcript::item::Item::AgentMessage { text, .. } = &entry.item
                && text != &item.text
            {
                item.revision = entry.revision + 1;
                replica::apply_item(t, item)?;
                self.changed.transcript(thread);
            }
        }
        Ok(())
    }

    fn opencode_row(&mut self, session: SessionView) {
        let id = session.id.clone();
        if self.opencode().sessions.get(&id) == Some(&session) {
            return;
        }
        self.catalog.insert(ThreadRow {
            id: id.clone(),
            cwd: self.profile.project.clone(),
            model_provider: self.profile.provider.clone(),
            name: Some(session.title.clone()),
            updated_at: session.updated_at,
            model: match &self.profile.route.billing {
                Billing::Api(alias) => Some(alias.clone()),
                Billing::Subscription => None,
            },
            worker: false,
            parent_thread_id: None,
            agent_nickname: None,
            preview: String::new(),
            generated_title: None,
            status: None,
            unread: false,
            activity_order: 0,
        });
        self.opencode_mut().sessions.insert(id, session);
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
    }

    fn opencode_snapshot(
        &mut self,
        session: SessionView,
        timeline: Timeline,
        asks: Vec<AskView>,
    ) -> io::Result<()> {
        let id = session.id.clone();
        if !timeline.detail["active"].is_string()
            && let Some(parts) = self.opencode_mut().volatile.remove(&id)
        {
            self.opencode_reset_parts(&id, &parts)?;
        }
        let last_result = session.last_result();
        self.opencode_row(session);
        let old = self
            .opencode_mut()
            .timelines
            .remove(&id)
            .unwrap_or_default();
        if old != timeline {
            let t = self.transcripts.entry(id.clone()).or_default();
            let same_order = old.items.len() <= timeline.items.len()
                && old
                    .items
                    .iter()
                    .zip(&timeline.items)
                    .all(|(a, b)| a.id == b.id && a.turn == b.turn);
            let old_len = t.len();
            if !same_order {
                *t = Transcript::default();
                self.changed.replace_transcript(&id);
            }
            for (index, item) in timeline.items.iter().enumerate() {
                if !same_order || old.items.get(index) != Some(item) {
                    let mut update = item.clone();
                    let key = crate::transcript::ItemKey {
                        turn: update.turn.clone(),
                        item: update.id.clone(),
                    };
                    update.revision = t.entry(&key).map_or(1, |old| old.revision + 1);
                    replica::apply_item(t, update)?;
                }
            }
            if !same_order {
                t.host_history_replaced(old_len);
            }
            replica::apply_meta(t, timeline.detail.clone());
            t.last_result = last_result;
            self.changed.transcript(&id);
        } else if let Some(t) = self.transcripts.get_mut(&id)
            && t.last_result != last_result
        {
            t.last_result = last_result;
            self.changed.transcript(&id);
        }
        self.opencode_mut().timelines.insert(id.clone(), timeline);
        if self
            .opencode()
            .asks
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            != asks
        {
            self.asks.retain(|ask| ask.thread.as_deref() != Some(&id));
            self.asks.extend(asks.iter().map(as_ask));
            self.opencode_mut().asks.insert(id, asks);
            self.changed.mark(Changed::ASK);
        }
        Ok(())
    }

    pub(super) fn opencode_session_view(&self, row: &ThreadRow) -> SessionView {
        let id = row.id.as_str();
        let mut session = self
            .opencode()
            .sessions
            .get(id)
            .cloned()
            .unwrap_or_else(|| SessionView {
                id: id.to_owned(),
                title: row.title().into(),
                status: "idle".into(),
                parent: None,
                updated_at: row.updated_at,
                capabilities: Vec::new(),
                detail: json!({}),
            });
        session.capabilities.push("open".into());
        if self.can_send(Some(id)) {
            session.capabilities.push("send".into());
        }
        if self.state == Connection::Ready
            && matches!(session.status.as_str(), "running" | "needs_input")
        {
            session.capabilities.push("stop".into());
        }
        session.detail["row"] = json!(row);
        session.detail["route"] = json!(self.profile.route);
        session.detail["options"] = json!(self.options(Some(id)));
        session.detail["sending"] = json!(self.opencode_sending(Some(id)));
        session
    }
}

fn as_ask(view: &AskView) -> Ask {
    Ask {
        command: view.command.clone(),
        id: view.id.clone(),
        thread: view.thread.clone(),
        turn: view.turn.clone(),
        item: view.item.clone(),
        title: view.title.clone(),
        detail: view.detail.clone(),
        questions: view.questions.clone(),
        decisions: view
            .choices
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let mut decision = Decision::new(label.clone(), Value::Null);
                decision.detail = view.choice_details.get(index).cloned().flatten();
                decision
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host_protocol::{Scope, TimelineItem},
        runtime::Runtime,
    };

    #[test]
    fn losing_the_native_feed_discards_only_unpersisted_text() {
        let route = Runtime::OpenCode.route(Some("api/model".into()));
        let scope = Scope {
            project: "/fixture".into(),
            route,
        };
        let (tx, _commands) = std::sync::mpsc::sync_channel(1);
        let mut store = Store::open_opencode(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "opencode".into(),
            },
            tx,
        );
        let snapshot = |text: &str, active: Option<&str>| Event::Snapshot {
            session: Box::new(SessionView {
                id: "thread".into(),
                title: "Thread".into(),
                parent: None,
                status: if active.is_some() { "running" } else { "idle" }.into(),
                updated_at: 1,
                capabilities: Vec::new(),
                detail: json!({ "owned": true }),
            }),
            transcript: Timeline {
                detail: json!({"active": active, "ready": true}),
                items: vec![TimelineItem {
                    turn: "turn".into(),
                    id: "part".into(),
                    role: "assistant".into(),
                    text: text.into(),
                    revision: 0,
                    detail: Value::Null,
                }],
            },
            asks: Vec::new(),
            sequence: Some(1),
        };
        let partial = || Event::StreamText {
            thread: "thread".into(),
            part: "part".into(),
            delta: "first".into(),
        };
        let display = |store: &Store| {
            store.host_snapshot(&scope).transcripts["thread"].items[0]
                .text
                .clone()
        };
        store.opencode_event(snapshot("", Some("turn"))).unwrap();
        store.opencode_event(partial()).unwrap();
        store.opencode_event(snapshot("", Some("turn"))).unwrap();
        assert_eq!(
            display(&store),
            "first",
            "unchanged authority preserves the temporary prefix"
        );
        store.opencode_event(Event::StreamLost).unwrap();
        assert_eq!(display(&store), "");
        store.opencode_event(partial()).unwrap();
        store
            .opencode_event(snapshot("first restored", None))
            .unwrap();
        store.opencode_event(Event::StreamLost).unwrap();
        assert_eq!(display(&store), "first restored");
        store.opencode_event(partial()).unwrap();
        assert_eq!(
            display(&store),
            "first restored",
            "late fragments cannot change a finished turn"
        );
    }
}
