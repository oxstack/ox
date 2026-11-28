//! Bounded, volatile text observations from one authenticated owner event feed.
use ox_host::opencode::{Event, Owner, runtime::RouteRuntime};
use std::{
    collections::{HashMap, HashSet},
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const QUEUE_FRAMES: usize = 128;
const MAX_PART_BYTES: usize = 1024 * 1024;
const MAX_FRAGMENT_BYTES: usize = 32 * 1024;
const MAX_OPEN_PARTS: usize = 128;
const RETRY: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct Observer {
    input: Option<Receiver<Frame>>,
    retry: Option<Instant>,
    parts: HashMap<(Uuid, String), Text>,
}

struct Text {
    message: String,
    bytes: usize,
    unsent: String,
    requested_refresh: bool,
}

enum Change {
    Open,
    Close,
    Fragment(String),
}

struct Frame {
    session: String,
    message: String,
    part: String,
    change: Change,
}

impl Frame {
    fn text(event: &Event) -> Option<Self> {
        let p = &event.properties;
        let session = p["sessionID"].as_str()?;
        let (message, part, change) = match event.name.as_str() {
            "message.part.updated" => {
                let part = &p["part"];
                if part["type"] != "text" || part["synthetic"] == true {
                    return None;
                }
                let change = if part["text"] == "" && part["time"]["end"].is_null() {
                    Change::Open
                } else {
                    Change::Close
                };
                (part["messageID"].as_str()?, part["id"].as_str()?, change)
            }
            "message.part.delta" if p["field"] == "text" => (
                p["messageID"].as_str()?,
                p["partID"].as_str()?,
                Change::Fragment(p["delta"].as_str()?.to_owned()),
            ),
            _ => return None,
        };
        if [session, message, part].iter().any(|id| id.len() > 256) {
            return None;
        }
        Some(Self {
            session: session.into(),
            message: message.into(),
            part: part.into(),
            change,
        })
    }
}

impl Observer {
    pub(super) fn connect(&mut self, runtime: &RouteRuntime, owner: &Owner) {
        if self.input.is_some() || self.retry.is_some_and(|until| until > Instant::now()) {
            return;
        }
        self.retry = Some(Instant::now() + RETRY);
        let Ok(mut stream) = runtime.route_events(owner) else {
            return;
        };
        let (tx, rx) = sync_channel(QUEUE_FRAMES);
        if thread::Builder::new()
            .name("ox-opencode-sse".into())
            .spawn(move || {
                while let Ok(Some(event)) = stream.read_event() {
                    if let Some(frame) = Frame::text(&event)
                        && (matches!(&frame.change, Change::Fragment(text) if text.len() > MAX_FRAGMENT_BYTES)
                            || tx.try_send(frame).is_err())
                    {
                        // A skipped fragment makes every later fragment ambiguous.
                        // Drop the feed and let the owner reconcile a fresh snapshot.
                        return;
                    }
                }
            })
            .is_ok()
        {
            self.input = Some(rx);
        }
    }

    /// Return true if the feed ended or overflowed. Partial text cannot survive
    /// that boundary because the native feed has no replay cursor.
    pub(super) fn poll(
        &mut self,
        runtime: &RouteRuntime,
        owner: &Owner,
        active: &HashSet<Uuid>,
    ) -> bool {
        if self.input.is_none() {
            return false;
        }
        self.parts.retain(|(id, _), _| active.contains(id));
        for _ in 0..=QUEUE_FRAMES {
            match self.input.as_ref().expect("active event stream").try_recv() {
                Ok(event) => self.observe(event, runtime, owner, active),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.input = None;
                    self.parts.clear();
                    return true;
                }
            }
        }
        false
    }

    fn observe(
        &mut self,
        frame: Frame,
        runtime: &RouteRuntime,
        owner: &Owner,
        active: &HashSet<Uuid>,
    ) {
        let Ok(Some(id)) = runtime.opened_session(owner, &frame.session) else {
            return;
        };
        if !active.contains(&id) {
            return;
        }
        let key = (id, frame.part);
        match frame.change {
            Change::Open if self.parts.len() < MAX_OPEN_PARTS => {
                self.parts.entry(key).or_insert(Text {
                    message: frame.message,
                    bytes: 0,
                    unsent: String::new(),
                    requested_refresh: false,
                });
            }
            Change::Open => {}
            Change::Close => {
                self.parts.remove(&key);
            }
            Change::Fragment(fragment) => {
                if let Some(text) = self
                    .parts
                    .get_mut(&key)
                    .filter(|text| text.message == frame.message)
                {
                    if text.bytes.saturating_add(fragment.len()) > MAX_PART_BYTES {
                        self.parts.remove(&key);
                    } else {
                        text.bytes += fragment.len();
                        text.unsent.push_str(&fragment);
                    }
                }
            }
        }
    }

    pub(super) fn unknown_parts(&mut self, known: impl Fn(Uuid, &str) -> bool) -> HashSet<Uuid> {
        self.parts
            .iter_mut()
            .filter_map(|((id, part), text)| {
                if !text.requested_refresh && !text.unsent.is_empty() && !known(*id, part) {
                    text.requested_refresh = true;
                    return Some(*id);
                }
                None
            })
            .collect()
    }

    pub(super) fn flush(
        &mut self,
        known: impl Fn(Uuid, &str) -> bool,
        deliver: impl Fn(Uuid, &str, String),
    ) {
        for ((id, part), text) in &mut self.parts {
            if !text.unsent.is_empty() && known(*id, part) {
                deliver(*id, part, std::mem::take(&mut text.unsent));
            }
        }
    }
}
