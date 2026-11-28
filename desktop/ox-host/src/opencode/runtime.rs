//! Route-owned stock OpenCode session state for an OX host I/O worker. The
//! private native session IDs and volatile stream stay on this side of the host wire.
mod ledger;
mod projection;

use super::{Decision, Error, Events, Hosted, Owner, Profile, Result, Session};
use ledger::{Ledger, Record, Turn};
use ox_desk_core::{
    host_protocol::{AskView, SessionOutcome, SessionView, Timeline},
    runtime::RequestId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt as _,
    path::Path,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const RECENT_MESSAGES: usize = 50;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pending,
    Cancelling,
    Completed,
    Failed,
    Stopped,
}

impl Phase {
    fn outcome(self) -> Option<SessionOutcome> {
        match self {
            Self::Completed => Some(SessionOutcome::Completed),
            Self::Failed => Some(SessionOutcome::Failed),
            Self::Stopped => Some(SessionOutcome::Stopped),
            Self::Pending | Self::Cancelling => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Receipt {
    pub sequence: u64,
    pub phase: Phase,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub session: SessionView,
    pub transcript: Timeline,
    pub asks: Vec<AskView>,
    pub before: Option<String>,
    pub receipt: Option<Receipt>,
}

#[derive(Debug, Serialize)]
pub struct TranscriptPage {
    pub transcript: Timeline,
    pub before: Option<String>,
}

pub struct RouteRuntime {
    hosted: Hosted,
    owner: Owner,
    ledger: Ledger,
    sessions: HashMap<Uuid, Live>,
    _lock: File,
}

struct Live {
    native: Session,
    tokens: HashMap<String, Uuid>,
    answered: HashSet<String>,
}

impl Live {
    fn new(native: Session) -> Self {
        Self {
            native,
            tokens: HashMap::new(),
            answered: HashSet::new(),
        }
    }
}

impl RouteRuntime {
    pub fn start(ox: &Path, profile: Profile) -> Result<Self> {
        let ledger_path = profile.state_home().join("ox-host-sessions-v1.json");
        let owner = profile.owner().clone();
        let owner_key = owner.profile_key();
        // Keep registry ownership even if the managed child unexpectedly exits.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(profile.root().join("controller.lock"))?;
        lock.try_lock().map_err(std::io::Error::from)?;
        let hosted = Hosted::start(ox, profile)?;
        let ledger = Ledger::open(ledger_path, owner_key)?;
        Ok(Self {
            hosted,
            owner,
            ledger,
            sessions: HashMap::new(),
            _lock: lock,
        })
    }

    pub fn sessions(&self, owner: &Owner) -> Result<Vec<Uuid>> {
        self.check_owner(owner)?;
        Ok(self.ledger.ids())
    }

    pub fn catalog(&self, owner: &Owner) -> Result<Vec<(Uuid, SessionView, Option<Receipt>)>> {
        self.check_owner(owner)?;
        Ok(self
            .ledger
            .ids()
            .into_iter()
            .filter_map(|id| {
                let record = self.ledger.get(id)?;
                let receipt = record.turn.as_ref().map(Turn::receipt);
                Some((
                    id,
                    projection::session_view(id, record, true, false),
                    receipt,
                ))
            })
            .collect())
    }

    pub fn receipt(&self, owner: &Owner, id: Uuid) -> Result<Option<Receipt>> {
        self.check_owner(owner)?;
        Ok(self.record(id)?.turn.as_ref().map(Turn::receipt))
    }

    /// The OX host allocates this identifier before issuing a create action.
    pub fn create(&mut self, owner: &Owner, id: Uuid, title: &str) -> Result<()> {
        self.check_owner(owner)?;
        if self.ledger.get(id).is_some() {
            return self.open(id).map(|_| ());
        }
        if id.is_nil() || title.is_empty() || title.len() > 1024 {
            return Err(Error::InvalidInput(
                "Invalid OpenCode host session ID or title",
            ));
        }
        let native =
            self.hosted
                .adapter
                .create_with_permissions(owner, title, Some(permissions()))?;
        self.ledger.put(
            id,
            Record {
                native: native.id().to_owned(),
                title: title.to_owned(),
                updated_at: now_millis() / 1000,
                turn: None,
            },
        )?;
        self.sessions.insert(id, Live::new(native));
        Ok(())
    }

    /// Sends are serialized by OX per session. The writer uses the next
    /// sequence for a new operator intent and the same sequence for its retry.
    /// A sequence is fenced on disk before the asynchronous native POST.
    pub fn send(&mut self, owner: &Owner, id: Uuid, sequence: u64, text: &str) -> Result<Receipt> {
        self.check_owner(owner)?;
        if text.trim().is_empty() || text.len() > MAX_PROMPT_BYTES {
            return Err(Error::InvalidInput("Invalid OpenCode prompt size"));
        }
        let native = self.open(id)?.native.clone();
        let old = self.record(id)?.turn.as_ref();
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        if let Some(turn) = old.filter(|turn| turn.sequence == sequence) {
            if turn.digest != digest {
                return Err(Error::ConflictingSend);
            }
            return Ok(turn.receipt());
        }
        let previous = old.map_or(0, |turn| turn.sequence);
        if previous.checked_add(1) != Some(sequence) {
            return Err(Error::ConflictingSend);
        }
        if old.is_some_and(|turn| turn.phase.outcome().is_none())
            || !self.hosted.adapter.is_idle(owner, &native)?
            || !self.hosted.adapter.pending(owner, &native)?.is_empty()
        {
            return Err(Error::TurnInProgress);
        }
        let turn = Turn {
            sequence,
            digest,
            native_message: message_id(),
            phase: Phase::Pending,
        };
        self.store_turn(id, turn.clone())?;
        if let Err(_error) =
            self.hosted
                .adapter
                .push_text(owner, &native, &turn.native_message, text)
        {
            // Stock has no idempotency guarantee for a repeated POST carrying
            // a messageID. Leave the receipt pending for reconciliation/Stop.
            return Err(Error::AmbiguousSend);
        }
        Ok(turn.receipt())
    }

    /// Call after a live event or a lost stream. History and pending asks
    /// replace the host snapshot rather than replaying volatile native deltas.
    pub fn read(&mut self, owner: &Owner, id: Uuid) -> Result<Snapshot> {
        self.check_owner(owner)?;
        let native = self.open(id)?.native.clone();
        let page = self
            .hosted
            .adapter
            .history(owner, &native, RECENT_MESSAGES, None)?;
        let idle = self.hosted.adapter.is_idle(owner, &native)?;
        let mut record = self.record(id)?.clone();
        let phase = record.turn.as_ref().map(|turn| turn.phase);
        let asks = if phase == Some(Phase::Pending) {
            let pending = self.hosted.adapter.pending(owner, &native)?;
            if idle
                && pending.is_empty()
                && let Some(turn) = &mut record.turn
                && let Some(phase) = projection::terminal(&page, &turn.native_message)
            {
                turn.phase = phase;
                self.store_turn(id, turn.clone())?;
                record.updated_at = self.record(id)?.updated_at;
            }
            let live = self.sessions.get_mut(&id).ok_or(Error::UnknownSession)?;
            projection::asks(id, record.turn.as_ref(), live, pending)
        } else {
            Vec::new()
        };
        Ok(projection::snapshot(id, record, page, idle, asks))
    }

    pub fn history(
        &mut self,
        owner: &Owner,
        id: Uuid,
        limit: usize,
        before: Option<&str>,
    ) -> Result<TranscriptPage> {
        self.check_owner(owner)?;
        let native = self.open(id)?.native.clone();
        let page = self.hosted.adapter.history(owner, &native, limit, before)?;
        Ok(TranscriptPage {
            transcript: projection::timeline(&page),
            before: page.before,
        })
    }

    /// Subscribe before the initial read. A stream termination or reconnect
    /// always leads to a fresh read. `Events` belongs on its own I/O worker.
    pub fn events(&mut self, owner: &Owner, id: Uuid) -> Result<Events> {
        self.check_owner(owner)?;
        let native = self.open(id)?.native.clone();
        self.hosted.adapter.events(owner, &native)
    }

    pub fn route_events(&self, owner: &Owner) -> Result<Events> {
        self.check_owner(owner)?;
        self.hosted.adapter.route_events(owner)
    }

    /// Translate an authenticated native event to an already opened host session.
    pub fn opened_session(&self, owner: &Owner, native: &str) -> Result<Option<Uuid>> {
        self.check_owner(owner)?;
        Ok(self
            .sessions
            .iter()
            .find_map(|(id, session)| (session.native.id() == native).then_some(*id)))
    }

    pub fn answer(
        &mut self,
        owner: &Owner,
        id: Uuid,
        request: &RequestId,
        token: &str,
        decision: Decision,
    ) -> Result<()> {
        self.check_owner(owner)?;
        let RequestId::Text(request) = request else {
            return Err(Error::UnknownPermission);
        };
        if self.record(id)?.turn.as_ref().map(|turn| turn.phase) != Some(Phase::Pending) {
            return Err(Error::UnknownPermission);
        }
        let live = self.open(id)?;
        if !Uuid::parse_str(token)
            .ok()
            .is_some_and(|token| live.tokens.get(request) == Some(&token))
        {
            return Err(Error::UnknownPermission);
        }
        let native = live.native.clone();
        live.tokens.remove(request);
        live.answered.insert(request.clone());
        self.hosted
            .adapter
            .answer(owner, &native, request, decision)
    }

    /// Fence the specific turn before interacting with stock. A late Stop
    /// from a replica cannot abort a newer turn in the same session.
    pub fn stop(&mut self, owner: &Owner, id: Uuid, sequence: u64) -> Result<Receipt> {
        self.check_owner(owner)?;
        let native = self.open(id)?.native.clone();
        let mut turn = self
            .record(id)?
            .turn
            .clone()
            .filter(|turn| turn.sequence == sequence)
            .ok_or(Error::ConflictingSend)?;
        if turn.phase.outcome().is_some() {
            return Ok(turn.receipt());
        }
        if turn.phase == Phase::Pending && self.hosted.adapter.is_idle(owner, &native)? {
            let page = self
                .hosted
                .adapter
                .history(owner, &native, RECENT_MESSAGES, None)?;
            if let Some(phase) = projection::terminal(&page, &turn.native_message)
                && self.hosted.adapter.pending(owner, &native)?.is_empty()
            {
                turn.phase = phase;
                self.store_turn(id, turn.clone())?;
                return Ok(turn.receipt());
            }
        }
        turn.phase = Phase::Cancelling;
        self.store_turn(id, turn.clone())?;
        let live = self.sessions.get_mut(&id).ok_or(Error::UnknownSession)?;
        live.tokens.clear();
        live.answered.clear();
        // prompt_async acknowledges a fork, which may not yet have entered
        // the native run-state registry. Abort against an initially idle
        // session can succeed and still let that fork start later.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.hosted.adapter.is_idle(owner, &native)? {
            let page = self
                .hosted
                .adapter
                .history(owner, &native, RECENT_MESSAGES, None)?;
            if projection::started(&page, &turn.native_message) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Error::BlockedByCancellation);
            }
            thread::sleep(Duration::from_millis(25));
        }
        self.hosted.adapter.cancel(owner, &native)?;
        turn.phase = Phase::Stopped;
        self.store_turn(id, turn.clone())?;
        Ok(turn.receipt())
    }

    fn check_owner(&self, owner: &Owner) -> Result<()> {
        if owner != &self.owner {
            return Err(Error::OwnerMismatch);
        }
        self.ledger.check()
    }

    fn record(&self, id: Uuid) -> Result<&Record> {
        self.ledger.get(id).ok_or(Error::UnknownSession)
    }

    fn store_turn(&mut self, id: Uuid, turn: Turn) -> Result<()> {
        let mut record = self.record(id)?.clone();
        record.updated_at = now_millis() / 1000;
        record.turn = Some(turn);
        self.ledger.put(id, record)
    }

    fn open(&mut self, id: Uuid) -> Result<&mut Live> {
        if !self.sessions.contains_key(&id) {
            let record = self.record(id)?;
            let native = self.hosted.adapter.reopen_with_permissions(
                &self.owner,
                &record.native,
                Some(permissions()),
            )?;
            if let Some(mut turn) = record
                .turn
                .clone()
                .filter(|turn| turn.phase.outcome().is_none())
            {
                // A new managed child cannot resume the old worker turn or
                // inherit its approval. Avoid replaying any uncertain POST.
                let page =
                    self.hosted
                        .adapter
                        .history(&self.owner, &native, RECENT_MESSAGES, None)?;
                turn.phase = if turn.phase == Phase::Cancelling {
                    Phase::Stopped
                } else {
                    projection::terminal(&page, &turn.native_message).unwrap_or(Phase::Stopped)
                };
                self.hosted.adapter.cancel(&self.owner, &native)?;
                self.store_turn(id, turn)?;
            }
            self.sessions.insert(id, Live::new(native));
        }
        self.sessions.get_mut(&id).ok_or(Error::UnknownSession)
    }
}

fn permissions() -> Value {
    json!([
        {"permission":"*", "pattern":"*", "action":"ask"},
        {"permission":"task", "pattern":"*", "action":"deny"},
        {"permission":"question", "pattern":"*", "action":"deny"},
    ])
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis() as i64)
}

fn message_id() -> String {
    // Stable v1 orders same-millisecond records by ascending message ID. Its
    // timestamp occupies the high 36 bits of a six-byte, wrapping prefix.
    let encoded = (now_millis() as u64 * 4096) & 0xffff_ffff_ffff;
    format!(
        "msg_{encoded:012x}{}",
        &Uuid::new_v4().simple().to_string()[..14]
    )
}
