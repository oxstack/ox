use crate::{
    durable::{Ledger, SavedScope},
    receipts::{Receipts, digest},
    replay::Replay,
    runtime::{RuntimeConfig, canonical_scope},
    transport::{Peer, encode, encode_for},
    workers::{Caller, Registry},
};
use ox_desk_core::{
    actions::Action,
    changed::Changed,
    handoff::HandoffStart,
    host_policy::HostPolicy,
    host_protocol::{self as wire, Change, Client, Cursor, ProcessView, Scope, ScopeState, Server},
    inbox::Inbox,
    links::LinkEvent,
    modes::{Level, YoloAllowance},
    origin::{Origin, ParentCall, WorkerMode, WorkerStart},
    runtime::{RequestId, Runtime},
    store::{Connection, Store},
    surface::Sender,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender},
    thread,
    time::{Duration, Instant},
};

pub(crate) enum Message {
    Connect(u64, Peer),
    Disconnect(u64),
    Request(u64, Box<Client>),
    Runtime(Scope, u64),
    Schema(Scope, u64, Box<ox_desk_core::slash::Capabilities>),
    Discovered(u64, Box<Server>),
    ProjectDirectory(wire::ProjectDirectory),
    HandoffTarget(u64, String, Result<Value, String>),
    HostsListed(PathBuf, Result<Value, String>),
    Outbound(Box<forwarding::Event>),
    WorkerResult(u64, Box<Result<ox_host_web::worker_result::Report, String>>),
    LinkedJob(Box<ox_host_web::linked_jobs::VerifiedJob>, Replier),
    Gateway {
        caller: Caller,
        request: Box<Gateway>,
        reply: Replier,
    },
    Stop,
    Replace(PathBuf, std::os::unix::net::UnixStream),
    /// Upgrade handover: refuse new turns, let running turns finish until the
    /// deadline, then stop. The next host reads why the previous one stopped.
    Drain(Duration),
}
/// A handoff target waiting for its first turn to be accepted.
pub(crate) struct Confirming {
    thread: String,
    deadline: Instant,
    launch: Launching,
}

/// Whether a new session's first turn was accepted: `Some(Ok)` once nothing is
/// left in flight, `Some(Err)` if the runtime refused part of it (such as
/// thread/inject_items) or it took too long, `None` while it is still being sent.
fn first_turn(store: &Store, thread: &str, deadline: Instant) -> Option<Result<(), String>> {
    if let Some(error) = store.transcript(thread).and_then(|t| t.error()) {
        return Some(Err(format!(
            "The new session was created, but its carried history or first turn was refused: {error}"
        )));
    }
    if !store.sending(Some(thread)) {
        return Some(Ok(()));
    }
    (deadline <= Instant::now()).then(|| {
        Err("The new session was created, but its carried history and first turn were not confirmed in time.".to_owned())
    })
}

/// A launch that ended, successfully or not.
pub(crate) struct Finished {
    reply: Replier,
    result: Result<(), String>,
    thread: Option<String>,
    origin: Origin,
    record: Option<String>,
    detached: bool,
    /// Retire the new session at once: its parent ended while the launch was in flight.
    retire: bool,
}
impl Finished {
    fn new(l: Launching, result: Result<(), String>, thread: Option<String>) -> Self {
        Self {
            retire: l.abandoned && result.is_ok() && thread.is_some(),
            record: match &l.payload {
                Payload::Handoff { record, .. } => Some(record.clone()),
                Payload::Worker(_) => None,
            },
            reply: l.reply,
            result,
            thread,
            origin: l.origin,
            detached: l.detached,
        }
    }
}
pub(crate) enum Accepted {
    Launch(Scope, Launching),
    Ask(Asking),
}
pub(crate) enum Gateway {
    Launch(WorkerStart),
    Call(ParentCall),
}
/// Where a worker launch or parent call is answered.
pub(crate) enum Replier {
    /// A hosted gateway waiting on its private lifetime endpoint.
    Channel(SyncSender<Result<Value, String>>),
    /// A same-user gateway or operator client on the main socket.
    Client(u64, String),
}
#[derive(PartialEq)]
enum Stage {
    Opening,
    AccessRequested,
    Sent,
}
/// What a launch sends once its runtime is ready.
pub(crate) enum Payload {
    Worker(WorkerStart),
    /// A handoff: the first turn (for Claude, with its context block) and the
    /// Codex items injected before it. `record` names the ledger entry.
    Handoff {
        text: String,
        items: Vec<Value>,
        record: String,
        /// The surface of the client that requested the handoff, if it declared one.
        from: Option<ox_desk_core::surface::Surface>,
    },
}
impl Payload {
    /// Who the first turn comes from: the launching agent's task for a worker, or the
    /// person who requested a handoff.
    fn sender(&self) -> Sender {
        match self {
            Self::Worker(_) => Sender::WorkerTask,
            Self::Handoff { from, .. } => Sender::Handoff { from: *from },
        }
    }
}
/// A worker launch or handoff accepted by policy, waiting for its runtime to create the session.
pub(crate) struct Launching {
    origin: Origin,
    payload: Payload,
    reply: Replier,
    stage: Stage,
    deadline: Instant,
    /// The launcher already has its answer (a YOLO request the operator accepted).
    /// A failure is recorded as the job's outcome instead.
    detached: bool,
    /// Optional pre-filtered Tier-1 memory brief delivered with a cross-host worker start.
    pub(crate) memory_brief: Option<String>,
    /// The parent was force-ended after the first message was already sent, so the
    /// launch cannot be recalled. The session it opens is retired on arrival.
    abandoned: bool,
}
/// A child waiting for the operator to accept YOLO.
pub(crate) struct Asking {
    pub(crate) origin: Origin,
    pub(crate) start: WorkerStart,
    pub(crate) target: Scope,
    pub(crate) parent_title: String,
    pub(crate) memory_brief: Option<String>,
}
/// A handoff waiting for its source's older history to load.
struct Preparing {
    export_to: Option<(String, uuid::Uuid)>,
    source: Scope,
    start: HandoffStart,
    reply: Replier,
    device: String,
    /// The requesting client's declared surface.
    surface: Option<ox_desk_core::surface::Surface>,
    deadline: Instant,
    /// The target's context window, looked up off the engine thread. The outer
    /// None means the lookup is still running.
    window: std::sync::Arc<std::sync::Mutex<Option<Option<u64>>>>,
    window_deadline: Instant,
    mode_approval: Option<YoloAllowance>,
}
/// A read_ox_thread call waiting for its source transcript to load.
struct Reading {
    frozen: bool,
    scope: Scope,
    session: String,
    source: Box<ox_desk_core::handoff::HandoffSource>,
    args: ReadArgs,
    reply: Replier,
    deadline: Instant,
    selected: bool,
}
#[derive(Clone, Debug)]
struct ReadArgs {
    item: Option<String>,
    after: Option<String>,
    limit: usize,
}
struct ClientState {
    update_lease: Option<tokio::sync::RwLockWriteGuard<'static, ()>>,
    peer: Peer,
    version: Option<u32>,
    prepared: Option<crate::runtime::Prepared>,
    subscription: Option<Option<Scope>>,
    catalog: bool,
    watch_resources: bool,
    /// A desktop window that may take a terminal handoff held for its scope.
    terminal: bool,
    /// The interface this connection declared after its hello. None for an older
    /// client or a relay-only consumer: their sends are stamped `Sender::Unknown`.
    surface: Option<ox_desk_core::surface::Surface>,
}
impl ClientState {
    fn supports_scope(&self, scope: &Scope) -> bool {
        self.version
            .is_some_and(|version| wire::supports_route(version, &scope.route))
            && (scope.route.runtime != Runtime::OpenCode
                || self.subscription.as_ref().and_then(Option::as_ref) == Some(scope))
    }
}
struct Live {
    launching: Option<Launching>,
    fork_origin: Option<Origin>,
    /// A Codex handoff target that exists but whose carried history and first
    /// turn are not confirmed yet: thread/inject_items and turn/start follow
    /// thread/start. Its launcher hears the outcome only once they do.
    confirming: Option<Confirming>,
    opening: Option<(u64, String)>,
    /// Actions waiting for the owning runtime, keyed by action ID.
    awaiting_input: HashMap<String, AwaitingInput>,
    terminal: Option<(u64, String)>,
    schema_started: bool,
    recovered: Vec<wire::SessionView>,
    store: Store,
    inbox: Inbox<LinkEvent>,
    generation: u64,
    state: ScopeState,
    tokens: HashMap<RequestId, String>,
    /// When each live process started and was last used, for the process list and the idle reaper.
    tracker: Tracker,
    /// A terminal handoff whose requesting client was gone when it was ready. The
    /// next scoped subscriber (or a replayed request) receives it until the
    /// deadline, after which it is dropped so the runtime can be reaped.
    held_handoff: Option<(String, Server, Instant)>,
}

struct AwaitingInput {
    client: u64,
    codex_thread: Option<String>,
    digest: [u8; 32],
}

/// How long a terminal handoff waits for a client to take it before it is dropped.
const HELD_HANDOFF_TTL: Duration = Duration::from_secs(600);

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
/// Start and last-use times of one process (a Claude session or a Codex scope).
#[derive(Clone, Copy)]
struct Seen {
    started: i64,
    last: Instant,
    last_unix: i64,
}
impl Seen {
    fn now() -> Self {
        Self {
            started: now_unix(),
            last: Instant::now(),
            last_unix: now_unix(),
        }
    }
}
pub(crate) struct Tracker {
    scope: Seen,
    threads: BTreeMap<String, Seen>,
}
impl Tracker {
    pub(crate) fn new() -> Self {
        Self {
            scope: Seen::now(),
            threads: BTreeMap::new(),
        }
    }
    fn touch(&mut self, thread: &str) {
        let now = Seen::now();
        self.scope.last = now.last;
        self.scope.last_unix = now.last_unix;
        let seen = self.threads.entry(thread.to_owned()).or_insert(now);
        seen.last = now.last;
        seen.last_unix = now.last_unix;
    }
    /// Track exactly the threads that have a live process.
    fn sync(&mut self, live: &[String]) {
        self.threads.retain(|t, _| live.contains(t));
        for t in live {
            self.threads.entry(t.clone()).or_insert_with(Seen::now);
        }
    }
}

/// How long a Claude session or empty Codex scope may sit unused before the
/// host ends it. Zero disables the reaper. `OX_HOST_IDLE_MINUTES` overrides it.
pub(crate) const DEFAULT_IDLE_MINUTES: u64 = 30;
pub(crate) fn idle_timeout(setting: Option<&str>) -> Duration {
    let minutes = setting
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_MINUTES);
    Duration::from_secs(minutes.saturating_mul(60))
}
/// Whether the reaper ends a process: it has sat unused for the whole timeout and
/// ending it would lose nothing (no running turn, pending approval, or worker).
pub(crate) fn reapable(idle: Duration, timeout: Duration, busy: bool) -> bool {
    !timeout.is_zero() && !busy && idle >= timeout
}
/// Messages the main loop reads between timer passes while a timer is overdue.
const OVERDUE_MAILBOX_BURST: u32 = 32;
const REAP_EVERY: Duration = Duration::from_secs(60);

pub(crate) struct Engine {
    pub(crate) replacement: Option<PathBuf>,
    config: RuntimeConfig,
    schema_reader: std::sync::Arc<ox_desk_core::codex::installed::SchemaReader>,
    host_id: uuid::Uuid,
    resources: crate::resources::Resources,
    next_resources: Instant,
    host_name: String,
    subscription_limits: ox_desk_core::usage::reported::ReportedLimits,
    ledger: Ledger,
    replay: Replay,
    tx: SyncSender<Message>,
    clients: BTreeMap<u64, ClientState>,
    running: HashMap<Scope, Live>,
    cold: HashMap<Scope, ScopeState>,
    launched: u64,
    workers: Registry,
    result_maintenance: Option<thread::JoinHandle<()>>,
    worker_results: HashMap<u64, linked::ResultState>,
    receipts: Receipts,
    notice_attempts: HashMap<u64, Instant>,
    notice_history: HashMap<u64, notices::HistoryPage>,
    /// Last project default read from the settings snapshot, kept for a failed read.
    yolo_projects: Vec<PathBuf>,
    /// Projects whose YOLO default was already announced since the host started.
    yolo_announced: HashSet<PathBuf>,
    /// Launch cautions by job number, recorded on the worker when it is stamped.
    launch_warnings: HashMap<u64, String>,
    project_queries: Vec<(u64, String)>,
    discovered: HashMap<(Scope, String), ox_desk_core::store::ThreadRow>,
    asking: BTreeMap<u64, Asking>,
    mode_requests: BTreeMap<u64, mode_requests::ModeRequest>,
    preparing: Vec<Preparing>,
    reading: Vec<Reading>,
    relays: flows::history::Relays,
    forward: forwarding::State,
    host_queries: HashMap<PathBuf, Vec<HostQuery>>,
    in_flows: bool,
    draining: Option<Instant>,
    state_dir: PathBuf,
    idle: Duration,
    next_reap: Instant,
    last_mode_policy: Option<HostPolicy>,
}
mod flows;
mod forward_queue;
pub(crate) mod forwarding;
mod linked;
mod placement;
use placement::HostQuery;
mod mode_requests;
mod notices;
mod opencode;

impl Drop for Engine {
    fn drop(&mut self) {
        self.schema_reader.shutdown();
    }
}

fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
impl Engine {
    pub(crate) fn open(
        config: RuntimeConfig,
        dir: &Path,
        tx: SyncSender<Message>,
    ) -> io::Result<Self> {
        let mut ledger = Ledger::open(dir)?;
        let host_id = crate::identity::load(dir)?;
        let seq = ledger.data.reserved;
        let mut workers = Registry::default();
        for saved in &ledger.data.scopes {
            for (id, origin, marks) in saved.origins() {
                workers.restore(&saved.scope, id, origin, marks);
            }
        }
        forwarding::recover(&ledger, dir)?;
        notices::recover(&mut ledger, &workers)?;
        ledger.retire(&crate::handover::take(dir))?;
        let cold = ledger
            .data
            .scopes
            .iter()
            .map(|saved| {
                let mut state = saved.recover();
                ledger.data.worker_notices.apply_to(&mut state);
                (saved.scope.clone(), state)
            })
            .collect();
        // Capture retired approval counts for the recovery snapshot before the
        // new epoch discards the previous runtime's tokens.
        ledger.begin(seq)?;
        Ok(Self {
            replacement: None,
            host_id,
            resources: Default::default(),
            next_resources: Instant::now(),
            subscription_limits: Default::default(),
            host_name: sysinfo::System::host_name().unwrap_or_else(|| host_id.to_string()),
            config,
            ledger,
            replay: Replay::new(seq, nonce()?),
            tx,
            clients: Default::default(),
            running: Default::default(),
            cold,
            launched: 0,
            workers,
            result_maintenance: None,
            worker_results: Default::default(),
            receipts: Receipts::default(),
            notice_attempts: HashMap::new(),
            notice_history: HashMap::new(),
            yolo_projects: Vec::new(),
            yolo_announced: HashSet::new(),
            launch_warnings: HashMap::new(),
            project_queries: Vec::new(),
            discovered: Default::default(),
            asking: Default::default(),
            mode_requests: Default::default(),
            preparing: Default::default(),
            reading: Default::default(),
            relays: Default::default(),
            forward: Default::default(),
            host_queries: Default::default(),
            in_flows: false,
            draining: None,
            state_dir: dir.to_owned(),
            idle: idle_timeout(std::env::var("OX_HOST_IDLE_MINUTES").ok().as_deref()),
            next_reap: Instant::now() + REAP_EVERY,
            last_mode_policy: HostPolicy::read(dir).ok(),
            schema_reader: Default::default(),
        })
    }
    pub(crate) fn serve(&mut self, rx: Receiver<Message>) -> io::Result<()> {
        self.recover_worker_results();
        // Mailbox messages served in a row while a timer was already overdue.
        let mut overdue_streak = 0u32;
        loop {
            if let Some(at) = self.draining {
                let running = self.running_turns();
                if running == 0 || Instant::now() >= at {
                    crate::handover::record(&self.state_dir, "upgrade", running)?;
                    break;
                }
            }
            let deadline = self
                .running
                .values()
                .flat_map(|r| {
                    r.store
                        .next_deadline()
                        .into_iter()
                        .chain(r.launching.as_ref().map(|l| l.deadline))
                        .chain(r.confirming.as_ref().map(|c| c.deadline))
                })
                .chain(self.preparing.iter().map(|p| p.deadline))
                .chain(
                    self.preparing
                        .iter()
                        .map(|p| p.window_deadline)
                        .filter(|at| *at > Instant::now()),
                )
                .chain(self.reading.iter().map(|r| r.deadline))
                .chain(self.relays.deadline())
                .chain(self.draining)
                .chain(
                    self.clients
                        .values()
                        .any(|c| c.watch_resources)
                        .then_some(self.next_resources),
                )
                .chain(self.forward_deadline())
                .chain(Some(self.next_reap))
                .min();
            let msg = if deadline.is_some_and(|at| at <= Instant::now()) {
                // An overdue timer must not starve the mailbox: a reply, ListProcesses or
                // Stop may be waiting. Read it first, but let the timers run after a bounded
                // run of messages so a busy mailbox cannot starve them either.
                if overdue_streak < OVERDUE_MAILBOX_BURST {
                    match rx.try_recv() {
                        Ok(msg) => {
                            overdue_streak += 1;
                            Some(msg)
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => None,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    }
                } else {
                    None
                }
            } else if let Some(deadline) = deadline {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                Some(match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                })
            };
            if msg.is_none() || deadline.is_none_or(|at| at > Instant::now()) {
                overdue_streak = 0;
            }
            match msg {
                None => {
                    let now = Instant::now();
                    let due: Vec<_> = self
                        .running
                        .iter()
                        .filter(|(_, s)| {
                            s.store.next_deadline().is_some_and(|t| t <= now)
                                || s.launching.as_ref().is_some_and(|l| l.deadline <= now)
                                || s.confirming.as_ref().is_some_and(|c| c.deadline <= now)
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    for scope in due {
                        // An earlier pass in this loop may have ended the scope.
                        if self.running.contains_key(&scope) {
                            self.process(&scope, vec![], false)?;
                        }
                    }
                    if now >= self.next_resources
                        && self.clients.values().any(|c| c.watch_resources)
                    {
                        self.next_resources = now + crate::resources::INTERVAL;
                        let frame = encode(&Server::HostStatus {
                            host: self.host_info(),
                        })?;
                        self.clients.retain(|_, c| {
                            !c.watch_resources || c.peer.send_for(frame.clone(), "host resources")
                        });
                    }
                    if now >= self.next_reap {
                        self.next_reap = now + REAP_EVERY;
                        self.refresh_mode_policy()?;
                        self.recover_worker_results();
                        self.maintain_worker_results();
                        self.reap_idle(now)?;
                    }
                    self.poll_forwarded(now)?;
                    self.advance_flows()?;
                }
                Some(Message::Replace(binary, _reservation)) => {
                    crate::handover::record(&self.state_dir, "upgrade", 0)?;
                    self.replacement = Some(binary);
                    break;
                }
                Some(Message::Stop) => {
                    crate::handover::record(&self.state_dir, "stop", self.running_turns())?;
                    break;
                }
                Some(Message::Drain(bound)) => {
                    let at = Instant::now()
                        .checked_add(bound)
                        .unwrap_or_else(|| Instant::now() + crate::control::MAX_BOUND);
                    self.draining = Some(self.draining.map_or(at, |d| d.min(at)));
                    eprintln!(
                        "OX host: draining for an update, {} running turn(s), at most {}s",
                        self.running_turns(),
                        bound.as_secs()
                    );
                }
                Some(Message::Schema(scope, generation, caps)) => {
                    if let Some(live) = self.running.get_mut(&scope)
                        && live.generation == generation
                        && live.store.state() == &Connection::Ready
                    {
                        let _ = live.store.install_command_schema(*caps);
                        self.process(&scope, vec![], true)?;
                    }
                }
                Some(Message::HostsListed(project, result)) => {
                    self.hosts_listed(project, result)?
                }
                Some(Message::Outbound(event)) => self.outbound(*event)?,
                Some(Message::WorkerResult(id, result)) => {
                    let state = match *result {
                        Ok(report) => linked::ResultState::Ready(report),
                        Err(error) => linked::ResultState::Retry {
                            error,
                            after: Instant::now() + Duration::from_secs(60),
                        },
                    };
                    self.worker_results.insert(id, state);
                }
                Some(Message::LinkedJob(job, reply)) => self.linked_job(*job, reply)?,
                Some(Message::Gateway {
                    caller,
                    request,
                    reply,
                }) => match *request {
                    Gateway::Launch(_) if self.draining.is_some() => {
                        self.answer(reply, Err(crate::handover::REFUSAL.into()))?
                    }
                    Gateway::Launch(start) => self.launch(caller, start, reply)?,
                    Gateway::Call(ParentCall::ListHosts) => self.list_hosts(&caller, reply)?,
                    Gateway::Call(call @ ParentCall::ReadThread { .. }) => {
                        self.read_thread(&caller, call, reply)?
                    }
                    Gateway::Call(call) => self.dispatch_parent_call(&caller, call, reply)?,
                },
                Some(Message::Connect(id, peer)) => {
                    self.clients.insert(
                        id,
                        ClientState {
                            update_lease: None,
                            peer,
                            version: None,
                            prepared: None,
                            subscription: None,
                            catalog: false,
                            watch_resources: false,
                            terminal: false,
                            surface: None,
                        },
                    );
                }
                Some(Message::Disconnect(id)) => {
                    self.clients.remove(&id);
                    self.drop_history_relay(id)?;
                    self.withdraw_activations(|request| matches!(
                        &request.activation, mode_requests::Activation::RemoteHandoff { client, .. } if *client == id
                    ), "The desktop disconnected before approving the handoff.")?;
                }
                Some(Message::Request(id, request)) => {
                    if let Err(e) = self.request(id, *request) {
                        if self.ledger.failed() {
                            return Err(e);
                        }
                        self.reply(
                            id,
                            Server::Error {
                                id: None,
                                message: e.to_string(),
                            },
                        )?;
                    }
                }
                Some(Message::Runtime(scope, gen_id)) => {
                    if let Some(s) = self.running.get(&scope)
                        && s.generation == gen_id
                    {
                        let batch = s.inbox.take();
                        self.process(&scope, batch, false)?;
                    }
                }
                Some(Message::HandoffTarget(id, key, result)) => {
                    self.answer(Replier::Client(id, key), result)?
                }
                Some(Message::ProjectDirectory(directory)) => {
                    for (client, key) in std::mem::take(&mut self.project_queries) {
                        self.reply(
                            client,
                            Server::DiscoveredEnd {
                                id: key,
                                errors: Vec::new(),
                                directory: Some(directory.clone()),
                            },
                        )?;
                    }
                }
                Some(Message::Discovered(id, frame)) => {
                    if let Server::Discovered { scope, rows, .. } = frame.as_ref() {
                        for entry in rows {
                            if let Ok(mut row) =
                                serde_json::from_value::<ox_desk_core::store::ThreadRow>(
                                    entry.detail["row"].clone(),
                                )
                            {
                                row.worker = entry.detail["worker"].as_bool().unwrap_or(false);
                                self.discovered
                                    .insert((scope.clone(), entry.id.clone()), row);
                            }
                        }
                    }
                    self.reply(id, *frame)?;
                }
            }
        }
        self.running.clear();
        Ok(())
    }
    /// The stamp for a person's send on connection `id`: its declared surface, or an
    /// unknown OX client when it declared none.
    fn sender(&self, id: u64) -> Sender {
        Sender::from_client(self.clients.get(&id).and_then(|c| c.surface))
    }
    fn reply(&mut self, id: u64, frame: Server) -> io::Result<()> {
        if let Some(client) = self.clients.get(&id)
            && let Some(packet) = encode_for(frame, client.version.unwrap_or(wire::MIN_VERSION))?
            && !client.peer.send(packet)
        {
            self.clients.remove(&id);
        }
        Ok(())
    }
    fn request(&mut self, id: u64, message: Client) -> io::Result<()> {
        let Some(client) = self.clients.get(&id) else {
            return Ok(());
        };
        if let Some(version) = client.version {
            if !message.supported_by(version) {
                return Err(io::Error::other(
                    "The requested runtime is unavailable on this OX host protocol version",
                ));
            }
        } else if !matches!(message, Client::Hello { .. }) {
            return Err(io::Error::other("Send a protocol hello first"));
        }
        match message {
            Client::ReserveIdleUpdate => {
                self.refresh_mode_policy()?;
                let ready = self.draining.is_none()
                    && !self.update_reserved()
                    && self.running_turns() == 0
                    && self.asking.is_empty()
                    && self.mode_requests.is_empty()
                    && self.running.values().all(|live| {
                        !live.store.has_pending_turn()
                            && live.store.asks().is_empty()
                            && live.opening.is_none()
                            && live.terminal.is_none()
                    });
                let lease = ready
                    .then(|| ox_host_web::host_updates::TERMINALS.try_write().ok())
                    .flatten();
                let ready = lease.is_some();
                if let Some(lease) = lease {
                    self.clients
                        .get_mut(&id)
                        .expect("connected client")
                        .update_lease = Some(lease);
                }
                self.reply(id, Server::IdleUpdate { ready })
            }
            Client::Hello { version } => {
                if client.version.is_some() {
                    return Err(io::Error::other(
                        "This connection already sent a protocol hello",
                    ));
                }
                let Some(version) = wire::negotiate(version) else {
                    let message = wire::version_mismatch(wire::VERSION, version);
                    let _ = self.reply(
                        id,
                        Server::Error {
                            id: None,
                            message: message.clone(),
                        },
                    );
                    return Err(io::Error::other(message));
                };
                let prepared = self.config.prepare().ok();
                let routes = prepared
                    .as_ref()
                    .map(|s| s.routes.clone())
                    .unwrap_or_default();
                let mut capabilities: Vec<String> = [
                    "snapshot",
                    "resume",
                    "actions",
                    "approvals",
                    "workers",
                    "live_catalog",
                    "project_directory",
                    "host_status",
                    "host_mode_policy",
                    "end",
                    "processes",
                    wire::IDLE_UPDATE,
                    ox_desk_core::surface::CAPABILITY,
                ]
                .map(str::to_owned)
                .into();
                capabilities.push("worker_inventory".into());
                capabilities.push("worker_codex_subscription".into());
                capabilities.push("worker_placement".into());
                capabilities.push("remote_handoff".into());
                capabilities.push("handoff_history".into());
                capabilities.push("linked_read_workers".into());
                capabilities.push("linked_write_workers".into());
                capabilities.push("worker_memory_brief".into());
                if prepared.is_some() {
                    capabilities.push("worker_routes_known".into());
                }
                capabilities.extend(
                    self.config
                        .installed_runtimes()
                        .iter()
                        .filter(|runtime| runtime.supports_workers())
                        .map(|runtime| format!("worker_runtime_{}", runtime.executable())),
                );
                if prepared.as_ref().is_some_and(|s| s.has_api_provider()) {
                    capabilities.push("worker_api_provider".into());
                }
                let client = self.clients.get_mut(&id).unwrap();
                client.version = Some(version);
                client.prepared = prepared;
                let projects =
                    crate::projects::known(&self.config, &self.state_dir, self.scope_projects());
                let host = Some(self.host_info());
                self.reply(
                    id,
                    Server::Hello {
                        version,
                        capabilities,
                        routes,
                        projects,
                        host,
                    },
                )
            }
            Client::TerminalPreview {
                id: key,
                scope,
                thread,
            } => {
                let scope = canonical_scope(scope)?;
                let result=self.running.get(&scope)
                    .filter(|_| scope.route.runtime != Runtime::OpenCode)
                    .filter(|_|client.subscription.as_ref()==Some(&Some(scope.clone())) && !key.is_empty() && key.len()<=256)
                    .filter(|l|l.store.can_send(Some(&thread)) && !l.store.in_terminal(&thread)
                        && l.store.transcript(&thread).is_some_and(|t|t.active_turn().is_none())
                        && !l.store.asks().iter().any(|a|a.thread.as_deref()==Some(&thread))
                        && l.store.catalog_row(&thread).is_some_and(|r|!r.worker && r.parent_thread_id.is_none()))
                    .ok_or_else(||io::Error::other("Open an idle parent session in its verified project and billing route."))
                    .and_then(|l|l.store.handoff_argv(Some(&thread)));
                match result {
                    Ok(argv) => self.reply(
                        id,
                        Server::TerminalPreview {
                            id: key,
                            scope,
                            thread,
                            argv,
                        },
                    ),
                    Err(e) => self.reply(
                        id,
                        Server::Error {
                            id: Some(key),
                            message: e.to_string(),
                        },
                    ),
                }
            }
            Client::Surface { surface } => {
                let client = self.clients.get_mut(&id).expect("connected");
                match client.surface {
                    None => {
                        client.surface = Some(surface);
                        Ok(())
                    }
                    Some(declared) if declared == surface => Ok(()),
                    Some(_) => Err(io::Error::other(
                        "This connection already declared a different surface",
                    )),
                }
            }
            Client::WorkerHosts { id: key, project } => {
                let reply = Replier::Client(id, key);
                if !key_ok(&reply) {
                    return self.answer(reply, Err("Invalid worker inventory request.".into()));
                }
                match project.canonicalize() {
                    Ok(project) => self.query_hosts(project, HostQuery::List(reply)),
                    Err(e) => self.answer(reply, Err(e.to_string())),
                }
            }
            Client::ListProcesses { id: key } => {
                let processes = self.processes();
                self.reply(id, Server::Processes { id: key, processes })
            }
            Client::WatchHost => {
                if !self.clients.values().any(|c| c.watch_resources) {
                    self.next_resources = Instant::now() + crate::resources::INTERVAL;
                }
                self.clients
                    .get_mut(&id)
                    .expect("connected")
                    .watch_resources = true;
                let host = self.host_info();
                self.reply(id, Server::HostStatus { host })
            }
            Client::WatchCatalog => {
                let client = self.clients.get_mut(&id).expect("connected");
                client.catalog = true;
                client.subscription = None;
                let scopes = self
                    .running
                    .values()
                    .map(|live| &live.state)
                    .chain(self.cold.values())
                    .filter(|state| state.scope.route.runtime != Runtime::OpenCode)
                    .map(catalog_snapshot)
                    .collect();
                self.reply(
                    id,
                    Server::Snapshot {
                        cursor: self.replay.cursor.clone(),
                        scopes,
                    },
                )
            }
            Client::Subscribe {
                scope,
                after,
                terminal,
            } => {
                if let Some(client) = self.clients.get_mut(&id) {
                    client.terminal = terminal;
                }
                self.subscribe(id, scope, after)
            }
            Client::WatchHandoffReads { proof } => {
                if self
                    .clients
                    .get(&id)
                    .is_some_and(|c| c.surface == Some(ox_desk_core::surface::Surface::Desktop))
                {
                    self.relays.watch(id, &proof);
                }
                Ok(())
            }
            Client::ReadHandoffPage {
                id: key,
                broker,
                source_host,
                target_host,
                scope,
                transfer,
                page,
            } => self.serve_history(
                Replier::Client(id, key),
                &source_host,
                &target_host,
                &scope,
                &transfer,
                page,
                broker.as_deref(),
            ),
            Client::FinishHandoffRead { token, result } => self.finish_history(id, &token, result),
            Client::InspectHandoffTarget { id: key, scope } => {
                self.inspect_handoff_target(id, key, scope);
                Ok(())
            }
            Client::PrepareRemoteHandoff {
                id: key,
                scope,
                start,
                target_host,
                window,
            } => {
                let result = self.export_handoff(id, &key, scope, start, target_host, window);
                if let Err(error) = result {
                    self.answer(Replier::Client(id, key), Err(error.to_string()))?;
                }
                Ok(())
            }
            Client::ReceiveRemoteHandoff {
                id: key,
                scope,
                packet,
            } => self.receive_handoff(id, key, scope, packet, None),
            Client::WorkerLaunch {
                id: key,
                mut caller,
                start,
            } => {
                let reply = Replier::Client(id, key);
                match caller.project.canonicalize() {
                    Ok(project) if key_ok(&reply) => {
                        caller.project = project;
                        self.launch(Caller::External(caller), start, reply)
                    }
                    _ => self.answer(
                        reply,
                        Err("Run the worker from a project directory.".into()),
                    ),
                }
            }
            Client::WorkerCall {
                id: key,
                mut caller,
                call,
            } => {
                let reply = Replier::Client(id, key);
                if matches!(call, ParentCall::ReadThread { .. }) {
                    return self.read_thread(&Caller::External(caller), call, reply);
                }
                match caller.project.canonicalize() {
                    Ok(project) => caller.project = project,
                    Err(e) => return self.answer(reply, Err(e.to_string())),
                }
                let caller = Caller::External(caller);
                if matches!(call, ParentCall::ListHosts) {
                    return self.list_hosts(&caller, reply);
                }
                self.dispatch_parent_call(&caller, call, reply)
            }
            Client::Act {
                id: key,
                scope,
                action,
                approval,
            } => {
                if key.is_empty() || key.len() > 256 {
                    return Err(io::Error::other("Invalid action request ID"));
                }
                let fail = |message: String| Server::Error {
                    id: Some(key.clone()),
                    message,
                };
                let scope = match canonical_scope(scope) {
                    Ok(s) => s,
                    Err(e) => return self.reply(id, fail(e.to_string())),
                };
                let subscribed = self.clients[&id].subscription.as_ref();
                if !matches!(subscribed,Some(Some(s)) if s==&scope)
                    && (scope.route.runtime == Runtime::OpenCode
                        || !matches!(subscribed, Some(None)))
                {
                    return self.reply(
                        id,
                        fail("Subscribe to the target scope before sending actions".into()),
                    );
                }
                // Ending a thread on a route the host closed needs no process: nothing
                // there is running, so reopening one only to tell it so would start a runtime
                // the user just asked to be rid of. The workers and requests under it are
                // still ended or withdrawn.
                if scope.route.runtime != Runtime::OpenCode
                    && !self.running.contains_key(&scope)
                    && self.cold.contains_key(&scope)
                    && let Action::EndSession(thread) | Action::ForceEndSession(thread) = &action
                {
                    let thread = thread.clone();
                    if matches!(action, Action::EndSession(_)) && self.thread_busy(&scope, &thread)
                    {
                        return self.reply(
                            id,
                            fail("This session is busy. Confirm ending it, which cancels its running work.".into()),
                        );
                    }
                    self.drop_grant(&scope, &thread)?;
                    self.end_children(&scope, &thread)?;
                    return self.reply(id, Server::Ack { id: key });
                }
                // A scope the host dropped while idle reopens for a client still subscribed to it.
                if !self.running.contains_key(&scope)
                    && self.cold.contains_key(&scope)
                    && let Err(e) = self.ensure(&scope, self.config.prepare().ok())
                {
                    return self.reply(id, fail(e.to_string()));
                }
                let Some(generation) = self.running.get(&scope).map(|live| live.generation) else {
                    return self.reply(
                        id,
                        fail("Open this project and billing route before sending actions".into()),
                    );
                };
                let hash = digest(&action, &approval)?;
                if let Some(input) = self
                    .running
                    .get_mut(&scope)
                    .unwrap()
                    .awaiting_input
                    .get_mut(&key)
                {
                    if input.digest != hash {
                        return self.reply(
                            id,
                            fail("Action ID was reused for a different request".into()),
                        );
                    }
                    // Connections are numbered in accept order. The retry can arrive
                    // before the old socket's Disconnect, or an old write can arrive late.
                    if id > input.client || !self.clients.contains_key(&input.client) {
                        input.client = id;
                    }
                    return Ok(());
                }
                if (self.draining.is_some() || self.update_reserved())
                    && crate::handover::starts_work(&action)
                {
                    return self.reply(id, fail(crate::handover::REFUSAL.into()));
                }
                match self
                    .receipts
                    .get(&scope, &key, generation, hash)
                    .map(|entry| entry.cloned())
                {
                    Err(e) => return self.reply(id, fail(e.to_string())),
                    Ok(Some(receipt)) => {
                        if let Some(owner) = self.waiting_open_client(&scope, &key) {
                            *owner = id;
                            return Ok(());
                        }
                        if let Some(thread) = receipt.opened {
                            self.reply(
                                id,
                                Server::Opened {
                                    id: key.clone(),
                                    scope: scope.clone(),
                                    thread,
                                },
                            )?;
                        } else {
                            let live = self.running.get_mut(&scope).unwrap();
                            if let Some((client, opening)) = &mut live.opening
                                && opening == &key
                                && !self.clients.contains_key(client)
                            {
                                *client = id;
                            }
                            // The same for a terminal handoff the old connection never received.
                            if let Some((client, terminal)) = &mut live.terminal
                                && terminal == &key
                                && !self.clients.contains_key(client)
                            {
                                *client = id;
                            }
                            if live
                                .held_handoff
                                .as_ref()
                                .is_some_and(|(k, _, _)| k == &key)
                                && let Some((_, frame, _)) = live.held_handoff.take()
                            {
                                self.reply(id, frame)?;
                            }
                        }
                        return self.reply(id, receipt.result);
                    }
                    Ok(None) => (),
                }
                let ends = matches!(action, Action::EndSession(_) | Action::ForceEndSession(_));
                let input_thread = match &action {
                    Action::SendText(Some(thread), _) | Action::SendImages(Some(thread), _, _)
                        if scope.route.runtime == ox_desk_core::runtime::Runtime::Codex =>
                    {
                        Some(thread.clone())
                    }
                    _ => None,
                };
                let result = match self.act(id, &key, &scope, action, approval) {
                    Ok(()) => Server::Ack { id: key.clone() },
                    Err(e) => fail(e.to_string()),
                };
                let accepted = matches!(result, Server::Ack { .. });
                // Only accepted actions are receipts. A refusal is often transient, so a corrected
                // retry with the same key must run again rather than replay the refusal.
                let awaiting_input = accepted
                    && (scope.route.runtime == Runtime::OpenCode || input_thread.is_some())
                    && self.running.get_mut(&scope).is_some_and(|live| {
                        let waiting = if scope.route.runtime == Runtime::OpenCode {
                            live.store.opencode_has_pending(&key)
                        } else {
                            input_thread
                                .as_deref()
                                .is_some_and(|thread| live.store.track_codex_input(thread))
                        };
                        if waiting {
                            live.awaiting_input.insert(
                                key.clone(),
                                AwaitingInput {
                                    client: id,
                                    codex_thread: input_thread,
                                    digest: hash,
                                },
                            );
                        }
                        waiting
                    });
                let waiting = awaiting_input
                    || (accepted && self.waiting_open_client(&scope, &key).is_some());
                // Keep in-flight input outside the bounded completed-receipt cache.
                if accepted && !awaiting_input {
                    self.receipts
                        .insert(scope.clone(), key, generation, hash, result.clone());
                }
                if accepted && self.running.contains_key(&scope) {
                    self.process(&scope, vec![], true)?;
                    if ends {
                        self.reap_empty(&scope)?;
                    }
                }
                if waiting {
                    return Ok(());
                }
                self.reply(id, result)
            }
            Client::Discover {
                id: key,
                projects: Some(query),
            } => {
                let checkout = match query {
                    wire::ProjectQuery::List => {
                        self.project_queries.push((id, key.clone()));
                        if self.project_queries.len() > 1 {
                            return Ok(());
                        }
                        None
                    }
                    wire::ProjectQuery::Checkout(path) => Some(path),
                };
                let mut scopes = self.scope_projects();
                scopes.extend(
                    self.discovered
                        .keys()
                        .map(|(scope, _)| scope.project.clone()),
                );
                let config = self.config.clone();
                let state_dir = self.state_dir.clone();
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let directory = crate::projects::directory(
                        &config,
                        &state_dir,
                        scopes,
                        checkout.as_deref(),
                    );
                    let message = if checkout.is_some() {
                        Message::Discovered(
                            id,
                            Box::new(Server::DiscoveredEnd {
                                id: key,
                                errors: Vec::new(),
                                directory: Some(directory),
                            }),
                        )
                    } else {
                        Message::ProjectDirectory(directory)
                    };
                    let _ = tx.send(message);
                });
                Ok(())
            }
            Client::Discover {
                id: key,
                projects: None,
            } => {
                let cfg = self.config.clone();
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let errors = ox_desk_core::native_catalog::discover(
                        &cfg.ox,
                        &cfg.path,
                        &cfg.desktop.join("claude-routes.json"),
                        &cfg.desktop.join("titles"),
                        |scope, rows| {
                            tx.send(Message::Discovered(
                                id,
                                Box::new(Server::Discovered {
                                    id: key.clone(),
                                    scope,
                                    rows,
                                }),
                            ))
                            .is_ok()
                        },
                    );
                    let _ = tx.send(Message::Discovered(
                        id,
                        Box::new(Server::DiscoveredEnd {
                            id: key,
                            errors,
                            directory: None,
                        }),
                    ));
                });
                Ok(())
            }
        }
    }
    fn subscribe(
        &mut self,
        id: u64,
        scope: Option<Scope>,
        after: Option<Cursor>,
    ) -> io::Result<()> {
        let scope = scope.map(canonical_scope).transpose()?;
        let client = self.clients.get_mut(&id).expect("connected client");
        let old = client.subscription.take();
        let same_subscription = old.as_ref().is_none_or(|previous| previous == &scope);
        let prepared = client.prepared.take();
        let fresh = if let Some(scope) = &scope {
            match self.ensure(scope, prepared) {
                Ok(fresh) => fresh,
                Err(e) => {
                    self.clients
                        .get_mut(&id)
                        .expect("connected client")
                        .subscription = old;
                    return Err(e);
                }
            }
        } else {
            false
        };
        let client = self.clients.get_mut(&id).expect("connected client");
        client.catalog = false;
        client.subscription = Some(scope.clone());
        let mut replayed = None;
        if !fresh
            && same_subscription
            && let Some(cursor) = after
            && let Some(mut frames) = self.replay.after(
                scope.as_ref(),
                &cursor,
                client.version.expect("negotiated subscription"),
            )
        {
            replayed = Some(frames.all(|p| client.peer.send_for(p, "replay")));
        }
        match replayed {
            Some(true) => return self.deliver_held_handoff(id, scope.as_ref()),
            Some(false) => {
                self.clients.remove(&id);
                return Ok(());
            }
            None => (),
        }
        let scopes = self
            .running
            .iter()
            .map(|(s, l)| (s, &l.state))
            .chain(self.cold.iter())
            .filter(|(s, _)| match scope.as_ref() {
                Some(wanted) => wanted == *s,
                None => s.route.runtime != Runtime::OpenCode,
            })
            .map(|(_, s)| s.clone())
            .collect();
        self.reply(
            id,
            Server::Snapshot {
                cursor: self.replay.cursor.clone(),
                scopes,
            },
        )?;
        self.deliver_held_handoff(id, scope.as_ref())
    }
    /// Give a terminal handoff whose requester disconnected to the next subscriber of its scope.
    fn deliver_held_handoff(&mut self, id: u64, scope: Option<&Scope>) -> io::Result<()> {
        let Some(scope) = scope else {
            return Ok(());
        };
        // A phone, push or operator gateway subscribed to the scope must never consume a
        // desktop's terminal handoff.
        if !self.clients.get(&id).is_some_and(|c| c.terminal) {
            return Ok(());
        }
        if let Some(frame) = self
            .running
            .get_mut(scope)
            .and_then(|l| l.held_handoff.take())
            .filter(|(_, _, deadline)| *deadline > Instant::now())
            .map(|(_, frame, _)| frame)
        {
            self.reply(id, frame)?;
        }
        Ok(())
    }
    fn ensure(
        &mut self,
        scope: &Scope,
        prepared: Option<crate::runtime::Prepared>,
    ) -> io::Result<bool> {
        if self.running.get(scope).is_some_and(|live| {
            !matches!(
                live.store.state(),
                Connection::Closed(_) | Connection::SignInRequired
            )
        }) {
            return Ok(false);
        }
        self.launched += 1;
        let generation = self.launched;
        let notify = self.tx.clone();
        let src = scope.clone();
        let inbox = Inbox::new(move || {
            let event = Message::Runtime(src.clone(), generation);
            // Store operations may deliver a reply inline on the actor thread.
            // Never block that thread behind its own full mailbox.
            if let Err(std::sync::mpsc::TrySendError::Full(event)) = notify.try_send(event) {
                let tx = notify.clone();
                thread::spawn(move || {
                    let _ = tx.send(event);
                });
            }
        });
        let into = inbox.clone();
        let opencode = scope.route.runtime == Runtime::OpenCode;
        let mut store = if opencode {
            opencode::open(
                scope,
                self.host_id.to_string(),
                &self.state_dir,
                self.config.ox.clone(),
                move |event| into.push(event),
            )?
        } else {
            self.config
                .open(scope, prepared, move |event| into.push(event))?
        };
        // Opening the route also subscribes it for stop, approval and recovery.
        // Actual launches read the policy again and report an unreadable file.
        if !opencode {
            store.use_host_policy(
                HostPolicy::read(&self.state_dir).unwrap_or(HostPolicy::AlwaysRead),
            );
            store.enable_memory(
                ox_desk_core::memory::store_path_for(&self.state_dir),
                self.host_id.to_string(),
            );
        }
        let previous = self.running.get_mut(scope);
        let prior = previous
            .as_ref()
            .map(|live| SavedScope::from_state(&live.state).recover());
        let prior_transcripts = previous
            .as_ref()
            .map(|live| live.state.transcripts.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(previous) = previous {
            store.recover_from(&mut previous.store);
        }
        annotate(
            &self.host_id.to_string(),
            &self.workers,
            &self.ledger.data,
            scope,
            &mut store,
        );
        let recovered = self
            .cold
            .remove(scope)
            .or(prior)
            .filter(|_| !opencode)
            .map(|old| old.sessions)
            .unwrap_or_default();
        // Threads the host had no process for stay resumable by a send.
        // The ended thread keeps a row for the send that resumes it even when discovery
        // has not listed it, and the access mode its Claude session had.
        for saved in &recovered {
            let access = serde_json::from_value(saved.detail["resume_access"].clone())
                .ok()
                .flatten();
            store.restore_ended(&saved.id, &saved.title, saved.updated_at, access);
            if let Ok(attachment) =
                serde_json::from_value(saved.detail["memory_attachment"].clone())
            {
                store.remember_memory_attachment(&saved.id, attachment);
            }
        }
        let mut state = store.host_snapshot(scope);
        restore(&mut state.sessions, &recovered);
        let mut changes = vec![
            Change::State {
                connection: state.connection.clone(),
                capabilities: state.capabilities.clone(),
                detail: state.detail.clone(),
            },
            Change::Sessions {
                sessions: state.sessions.clone(),
            },
            Change::Asks { asks: vec![] },
        ];
        // Other subscribers retain this scope across native reconnection. Reset
        // their old timelines, then publish the replacement runtime's baseline.
        for thread in prior_transcripts {
            if !state.transcripts.contains_key(&thread) {
                changes.push(Change::Retired { thread });
            }
        }
        changes.extend(
            state
                .transcripts
                .iter()
                .map(|(thread, timeline)| Change::Transcript {
                    thread: thread.clone(),
                    timeline: timeline.clone(),
                }),
        );
        let _ = store.drain(vec![], Instant::now());
        self.withdraw_activations(|r| &r.scope == scope, "The session host closed this route.")?;
        self.running.insert(
            scope.clone(),
            Live {
                launching: None,
                fork_origin: None,
                confirming: None,
                opening: None,
                awaiting_input: Default::default(),
                terminal: None,
                schema_started: false,
                recovered,
                store,
                inbox,
                generation,
                state,
                tokens: Default::default(),
                tracker: Tracker::new(),
                held_handoff: None,
            },
        );
        self.broadcast(scope, changes)?;
        Ok(true)
    }
    fn act(
        &mut self,
        id: u64,
        key: &str,
        scope: &Scope,
        action: Action,
        approval: Option<String>,
    ) -> io::Result<()> {
        self.refresh_mode_policy()?;
        self.act_with_mode_approval(id, key, scope, action, approval, None)
    }
    fn act_with_mode_approval(
        &mut self,
        id: u64,
        key: &str,
        scope: &Scope,
        action: Action,
        approval: Option<String>,
        approved_mode: Option<(ox_desk_core::modes::Level, Sender)>,
    ) -> io::Result<()> {
        let live = self.running.get_mut(scope).expect("validated live scope");
        if let Action::AnswerAsk(ask, _) | Action::AnswerQuestions(ask, _) = &action
            && (approval.is_none() || live.tokens.get(ask) != approval.as_ref())
        {
            return Err(io::Error::other(
                "Approval was already resolved or belongs to an earlier runtime",
            ));
        }
        if scope.route.runtime == Runtime::OpenCode {
            return live.store.dispatch_opencode(key, action);
        }
        if let Action::AnswerAsk(ask, choice) = &action
            && let Some(job) = crate::yolo::job_of(ask)
        {
            if self.mode_requests.contains_key(&job) {
                let result = self.answer_activation(id, scope, job, *choice);
                if result.is_err() {
                    self.process(scope, vec![], true)?;
                }
                return result;
            }
            return self.answer_yolo(id, scope, job, *choice);
        }
        if let Action::AnswerQuestions(ask, _) = &action
            && crate::yolo::job_of(ask).is_some()
        {
            return Err(io::Error::other(
                "Answer a YOLO request with one of its choices.",
            ));
        }
        match action {
            Action::EndSession(thread) => return self.end_thread(scope, &thread, false),
            Action::ForceEndSession(thread) => return self.end_thread(scope, &thread, true),
            Action::StartHandoff(start) => return self.start_handoff(id, key, scope, start),
            Action::YoloGrant(session, on) => return self.set_grant(scope, &session, on),
            Action::InjectHandoff(..) => {
                return Err(io::Error::other(
                    "Start a handoff with its source and request.",
                ));
            }
            _ => (),
        }
        if let Action::StartWorker(start) = action {
            let caller = Caller::Operator {
                scope: scope.clone(),
                parent: start.parent.clone(),
                device: format!("client-{id}"),
            };
            return self
                .launch(caller, start, Replier::Client(id, key.to_owned()))
                .map(|_| ());
        }
        if let Action::SetAccess(target, _) = &action
            && self.running.values().any(|l| {
                l.launching.as_ref().is_some_and(|w| {
                    w.origin.parent.scope.as_ref() == Some(scope)
                        && w.origin.parent.session.as_deref() == Some(target)
                })
            })
        {
            return Err(io::Error::other("A worker is starting on this parent."));
        }
        let target = match &action {
            Action::SetAccess(t, _)
            | Action::ForkThread(t)
            | Action::Stop(t)
            | Action::SendText(Some(t), _)
            | Action::SendImages(Some(t), _, _)
            | Action::Submit(Some(t))
            | Action::NativeCommand(Some(t), _)
            | Action::PaletteCommand(Some(t), _)
            | Action::ExecuteCommand(Some(t)) => Some(t.clone()),
            _ => None,
        };
        let origin = target
            .as_deref()
            .and_then(|t| self.workers.get(scope, t))
            .map(|e| e.origin.clone());
        let active_workers = target.as_deref().is_some_and(|t| {
            self.workers.children_of(scope, t).any(|(s, child)| {
                self.running.get(s).is_some_and(|l| {
                    l.store
                        .transcript(child)
                        .is_some_and(|t| t.active_turn().is_some())
                })
            })
        });
        let live = self.running.get_mut(scope).expect("validated live scope");
        let from_catalog = |key: &str| {
            self.discovered
                .get(&(scope.clone(), key.to_owned()))
                .filter(|r| r.cwd == scope.project)
                .cloned()
        };
        let action = match action {
            Action::SelectThread(key) if live.store.catalog_row(&key).is_none() => {
                Action::OpenCatalogRow(Box::new(from_catalog(&key).ok_or_else(|| {
                    io::Error::other("The thread is outside the project catalog")
                })?))
            }
            Action::OpenCatalogRow(row) => Action::OpenCatalogRow(Box::new(
                live.store
                    .catalog_row(&row.id)
                    .cloned()
                    .or_else(|| from_catalog(&row.id))
                    .ok_or_else(|| io::Error::other("The thread is outside the project catalog"))?,
            )),
            Action::ConfirmAliasAndResume(row) => {
                let saved = live
                    .store
                    .catalog_row(&row.id)
                    .cloned()
                    .or_else(|| {
                        self.discovered
                            .iter()
                            .find(|((s, id), r)| {
                                s.project == scope.project
                                    && s.route.runtime == scope.route.runtime
                                    && s.route.is_api()
                                    && id == &row.id
                                    && r.needs_claude_alias()
                            })
                            .map(|(_, r)| r.clone())
                    })
                    .ok_or_else(|| {
                        io::Error::other("The saved billing route cannot be verified")
                    })?;
                Action::ConfirmAliasAndResume(Box::new(saved))
            }
            action => action,
        };
        crate::policy::check(&live.store, &action, origin.as_ref(), active_workers)?;
        let parsed = match &action {
            Action::PaletteCommand(owner, text) | Action::NativeCommand(owner, text) => {
                live.store.parse_command(owner.as_deref(), text)
            }
            Action::ExecuteCommand(owner) => live
                .store
                .parse_command(owner.as_deref(), live.store.draft(owner.as_deref())),
            _ => None,
        };
        let opens = matches!(
            action,
            Action::SendText(None, _)
                | Action::SendImages(None, _, _)
                | Action::Submit(None)
                | Action::ForkThread(_)
        ) || (matches!(
            action,
            Action::PaletteCommand(None, _)
                | Action::NativeCommand(None, _)
                | Action::ExecuteCommand(None)
        ) && parsed
            .as_ref()
            .is_some_and(|command| command.starts_thread()));
        let pending_open = self.mode_requests.values().any(|r| {
            &r.scope == scope && matches!(&r.activation, mode_requests::Activation::Open {source, ..} if source == &target)
        });
        if opens && (live.opening.is_some() || live.launching.is_some() || pending_open) {
            return Err(io::Error::other(
                "Another session is opening on this route.",
            ));
        }
        let terminal = matches!(action, Action::Terminal(_) | Action::TerminalText(..))
            || (!matches!(action, Action::NativeCommand(..))
                && matches!(
                    parsed,
                    Some(
                        ox_desk_core::slash::Action::Terminal
                            | ox_desk_core::slash::Action::DiscoverClaudeCommand
                    )
                ));
        if terminal && live.terminal.is_some() {
            return Err(io::Error::other(
                "Another device is opening the terminal on this route.",
            ));
        }
        // The operator's own turns on a worker are reported to its parent agent.
        let running = target.as_deref().is_some_and(|t| {
            live.store
                .transcript(t)
                .is_some_and(|t| t.active_turn().is_some())
        });
        let operator_turn = matches!(
            action,
            Action::SendText(Some(_), _)
                | Action::SendImages(Some(_), _, _)
                | Action::Submit(Some(_))
        );
        // Stock Claude may start a saved process to execute a slash command, just as for a send.
        let resume_command = matches!(
            &action,
            Action::PaletteCommand(Some(_), _)
                | Action::NativeCommand(Some(_), _)
                | Action::ExecuteCommand(Some(_))
        ) && parsed
            .as_ref()
            .is_some_and(|command| command.starts_thread());
        let access = match &action {
            Action::SetAccess(_, mode) => Some(*mode),
            _ => None,
        };
        let fork = matches!(action, Action::ForkThread(_));
        // A send to a thread this reopened route has not listed yet: add its row first, so
        // the store can resume the thread and deliver the message.
        if operator_turn
            && let Some(t) = &target
            && !live.store.has_live_session(t)
            && live.store.catalog_row(t).is_none()
            && let Some(row) = from_catalog(t)
        {
            live.store.adopt_catalog_row(row)?;
        }
        // The stamp describes who sent this message. It is fixed here, at acceptance, and
        // travels with the message; it never enters the digest or an authorization check.
        if scope.route.runtime != ox_desk_core::runtime::Runtime::Antigravity
            && let Action::SetAccess(owner, mode) = &action
            && self.stage_mode_change(scope, owner, *mode, approved_mode.is_some())?
        {
            return Ok(());
        }
        let may_open = opens
            || operator_turn
            || matches!(
                &action,
                Action::SelectThread(_)
                    | Action::OpenCatalogRow(_)
                    | Action::ConfirmAliasAndResume(_)
                    | Action::PaletteCommand(..)
                    | Action::NativeCommand(..)
                    | Action::ExecuteCommand(_)
            );
        let sender = approved_mode
            .as_ref()
            .map_or_else(|| self.sender(id), |(_, sender)| sender.clone());
        let live = self.running.get_mut(scope).expect("validated live scope");
        let mut fork_restore = None;
        if may_open {
            let policy = HostPolicy::read(&self.state_dir)?;
            live.store.use_host_policy(policy);
            let resume = target.as_deref().filter(|t| {
                (operator_turn || resume_command)
                    && live.store.is_session_scoped()
                    && !live.store.has_live_session(t)
            });
            if (opens || resume.is_some()) && policy == HostPolicy::YoloAsksEveryTime {
                let level = if let Some(t) = resume {
                    live.store
                        .claude_resume_access(t)
                        .and_then(|mode| ox_desk_core::modes::Level::of(scope.route.runtime, mode))
                } else if fork {
                    (!live.store.is_session_scoped())
                        .then(|| live.store.applied_ceiling(target.as_deref()))
                        .flatten()
                } else {
                    live.store
                        .next_access()
                        .and_then(|mode| ox_desk_core::modes::Level::of(scope.route.runtime, mode))
                };
                if let Some((granted, _)) = approved_mode {
                    if let Some(t) = resume {
                        live.store
                            .choose_resume_access(t, granted.preset(scope.route.runtime));
                    } else if fork {
                        live.store.set_host_start(Some(granted));
                        fork_restore = Some(policy.starting_level());
                    } else {
                        live.store
                            .set_next_access(granted.preset(scope.route.runtime));
                    }
                } else if level == Some(ox_desk_core::modes::Level::Yolo) {
                    let action = match action {
                        Action::Submit(owner) => Action::SendImages(
                            owner.clone(),
                            live.store.draft(owner.as_deref()).to_owned(),
                            live.store.draft_images(owner.as_deref()).to_vec(),
                        ),
                        Action::ExecuteCommand(owner) => {
                            let command = live.store.draft(owner.as_deref()).to_owned();
                            Action::PaletteCommand(owner, command)
                        }
                        other => other,
                    };
                    return self.ask_activation(
                        scope,
                        mode_requests::Activation::Open {
                            client: id,
                            key: key.into(),
                            action: Box::new(action),
                            sender,
                            source: target.clone(),
                        },
                    );
                }
            }
        }
        let sent = live.store.dispatch_as(action, sender);
        if let Some(before) = fork_restore {
            live.store.set_host_start(before);
        }
        sent?;
        if fork && let (Some(origin), Some(source)) = (&origin, &target) {
            live.fork_origin = Some(origin.fork(source));
        }
        if let Some(t) = &target
            && let Some(entry) = self.workers.get_mut(scope, t)
        {
            if operator_turn && running {
                entry.marks.steered += 1;
            } else if operator_turn {
                entry.marks.continued += 1;
                entry.marks.cancelled = false;
                // A new run: the earlier answer is superseded and the runtime budget
                // starts over, so an old finished worker is not expired at once.
                entry.marks.expired = false;
                entry.marks.last = None;
                entry.marks.run_started_at = crate::durable::unix_now();
            }
            if access.is_some() {
                entry.marks.access = access;
            }
        }
        let live = self.running.get_mut(scope).expect("validated live scope");
        if let Some(t) = &target {
            live.tracker.touch(t);
        }
        if terminal {
            live.terminal = Some((id, key.into()));
        }
        if opens {
            live.opening = Some((id, key.into()));
        }
        Ok(())
    }
    fn process(
        &mut self,
        scope: &Scope,
        events: Vec<LinkEvent>,
        from_action: bool,
    ) -> io::Result<()> {
        self.process_scope(scope, events, from_action)?;
        self.apply_notice_receipts(scope)?;
        self.advance_flows()
    }
    fn process_scope(
        &mut self,
        scope: &Scope,
        events: Vec<LinkEvent>,
        from_action: bool,
    ) -> io::Result<()> {
        let live = self.running.get_mut(scope).expect("live scope");
        // A stock request ID may be cancelled and reused in one input batch.
        // Retire the old incarnation before considering the batch's resulting asks.
        for event in events {
            let previous = live.store.asks().len();
            live.store.apply(event);
            if let Some(report) = live.store.usage.worker_report.take() {
                self.subscription_limits.record(&scope.route, report);
            }
            if !live.tokens.is_empty() && live.store.asks().len() < previous {
                live.tokens
                    .retain(|id, _| live.store.asks().iter().any(|a| &a.id == id));
            }
        }
        // A read worker asking to leave Plan mode has finished its plan, and no
        // person could grant the exit within its ceiling. Answer it here so the
        // worker reports instead of waiting for an operator.
        let plan_exits: Vec<_> = live
            .store
            .asks()
            .iter()
            .filter(|ask| {
                live.store
                    .claude_ask_tool(&ask.id)
                    .is_some_and(|(owner, tool)| {
                        tool == "ExitPlanMode"
                            && self
                                .workers
                                .get(scope, owner)
                                .is_some_and(|e| crate::workers::declines_plan_exit(&e.origin))
                    })
            })
            .map(|ask| ask.id.clone())
            .collect();
        for id in plan_exits {
            live.tokens.remove(&id);
            if let Err(e) = live.store.deny_claude_ask(&id, crate::workers::PLAN_REPORT) {
                eprintln!("ox host: could not answer a read worker's plan exit: {e}");
            }
        }
        let reported = live.store.take_context_windows();
        if !reported.is_empty() {
            let route = scope.route.clone();
            self.ledger.change(|d| {
                for (model, tokens) in reported {
                    crate::durable::record_window(
                        &mut d.windows,
                        crate::durable::Window {
                            route: route.clone(),
                            model,
                            tokens,
                        },
                    );
                }
            })?;
        }
        let launch_failure = advance_launch(live);
        let mut dirty = live.store.drain(vec![], Instant::now());
        self.mode_requests.retain(|_, request| {
            &request.scope != scope || !matches!(&request.activation, mode_requests::Activation::Resume(id) if !live.store.resume_held(id))
        });
        // Record archival even if result preparation is still running. A restarted
        // host can subsequently retire the published ref from this durable marker.
        for (id, entry) in self.workers.in_scope(scope) {
            if let Some(link) = &entry.origin.linked
                && entry.origin.requested != Level::Read
                && live.store.archived_threads().contains(id)
            {
                let job = self
                    .state_dir
                    .join("linked-work/jobs")
                    .join(&link.host)
                    .join(entry.origin.job_id.to_string());
                if let Err(e) = ox_host_web::worker_result::archive(&job) {
                    eprintln!(
                        "ox host: could not record worker archival for job {}: {e}",
                        entry.origin.job_id
                    );
                }
            }
        }
        let archived: HashSet<_> = self
            .mode_requests
            .values()
            .filter(|r| &r.scope == scope)
            .filter_map(|r| r.view.thread.as_ref())
            .filter(|t| live.store.archived_threads().contains(*t))
            .cloned()
            .collect();
        let resumes = live.store.new_resume_reviews();
        if !archived.is_empty() {
            self.withdraw_activations(
                |r| {
                    &r.scope == scope
                        && r.view
                            .thread
                            .as_ref()
                            .is_some_and(|id| archived.contains(id))
                },
                "The session was archived.",
            )?;
        }
        for session in resumes.into_iter().filter(|id| !archived.contains(id)) {
            self.ask_activation(scope, mode_requests::Activation::Resume(session))?;
        }
        let live = self.running.get_mut(scope).expect("live scope");
        if from_action {
            dirty.mark(Changed::THREADS | Changed::ASK | Changed::NOTICE);
            if let Some(id) = live.store.selected() {
                dirty.transcript(id);
            }
        }
        if !live.schema_started
            && scope.route.runtime == ox_desk_core::runtime::Runtime::Codex
            && live.store.state() == &Connection::Ready
        {
            live.schema_started = true;
            let tx = self.tx.clone();
            let path = self.config.path.clone();
            let scope = scope.clone();
            let gen_id = live.generation;
            let reader = self.schema_reader.clone();
            thread::spawn(move || {
                if let Ok(caps) = reader.read(&path) {
                    let _ = tx.send(Message::Schema(scope, gen_id, Box::new(caps)));
                }
            });
        }
        let handoff = live.store.take_handoff().and_then(|handoff| {
            live.terminal.take().map(|(id, key)| {
                (
                    id,
                    Server::Handoff {
                        argv: handoff.argv,
                        id: key,
                        scope: scope.clone(),
                        thread: handoff.thread,
                        command: handoff.command,
                    },
                )
            })
        });
        // The requesting client may have gone while the handoff was prepared. Keep
        // it for the next subscriber instead of losing it with the terminal mode set.
        let handoff = match handoff {
            Some((client, frame)) if !self.clients.contains_key(&client) => {
                let key = match &frame {
                    Server::Handoff { id, .. } => id.clone(),
                    _ => String::new(),
                };
                live.held_handoff = Some((key, frame, Instant::now() + HELD_HANDOFF_TTL));
                None
            }
            other => other,
        };
        if live.terminal.is_some() && !live.store.opening_terminal() {
            live.terminal = None;
        }
        let opened = live.store.take_opened();
        // A new session from a worker launch or a worker fork receives its stamp
        // before it is first published, so no client sees it unstamped.
        let mut launched = None;
        if let Some(thread) = &opened {
            if live.opening.is_some()
                && let Some(origin) = live.fork_origin.take()
            {
                self.workers.stamp(scope, thread, origin);
            } else if live
                .launching
                .as_ref()
                .is_some_and(|l| l.stage == Stage::Sent)
            {
                let l = live.launching.take().expect("launching");
                let allowed = live
                    .store
                    .applied_access(Some(thread))
                    .is_none_or(|a| l.origin.ceiling.allows(l.origin.runtime, a));
                if !allowed {
                    // The runtime did not confirm a mode inside the ceiling. Never let it run.
                    let _ = live.store.stop(thread);
                    launched = Some(Finished::new(l, Err("The runtime did not confirm a permission mode within the granted ceiling.".to_owned()), None));
                } else if matches!(l.payload, Payload::Handoff { .. })
                    && l.origin.runtime == ox_desk_core::runtime::Runtime::Codex
                {
                    // Stamp it now so no client sees it unstamped, but report the
                    // handoff only once the carried history and request are in.
                    self.workers.stamp(scope, thread, l.origin.clone());
                    live.confirming = Some(Confirming {
                        thread: thread.clone(),
                        deadline: Instant::now() + Duration::from_secs(60),
                        launch: l,
                    });
                } else {
                    self.workers.stamp(scope, thread, l.origin.clone());
                    if let Payload::Worker(w) = &l.payload
                        && let Some(entry) = self.workers.get_mut(scope, thread)
                    {
                        entry.marks.max_minutes = w.max_minutes.or_else(|| {
                            crate::workers::env_limit("OX_WORKER_MAX_MINUTES")
                                .map(|m| m.min(u64::from(u32::MAX)) as u32)
                        });
                        entry.marks.warning = self.launch_warnings.remove(&l.origin.job_id);
                    }
                    launched = Some(Finished::new(l, Ok(()), Some(thread.clone())));
                }
            }
        }
        if launched.is_none()
            && let Some(c) = &live.confirming
            && let Some(result) = first_turn(&live.store, &c.thread, c.deadline)
        {
            let c = live.confirming.take().expect("confirming");
            launched = Some(Finished::new(c.launch, result, Some(c.thread)));
        }
        let launch_failure = launch_failure.or_else(|| {
            let l = live.launching.as_ref()?;
            match live.store.state() {
                Connection::Closed(e) => Some(e.clone()),
                Connection::SignInRequired => {
                    Some("Sign in to this worker's billing route first.".into())
                }
                _ if l.deadline <= Instant::now() => {
                    Some("The worker runtime did not start a session in time.".into())
                }
                _ if l.stage == Stage::Sent && !live.store.opening_thread() => live
                    .store
                    .notice()
                    .map(str::to_owned)
                    .or(Some("The worker session could not be started.".into())),
                _ => None,
            }
        });
        if let Some(error) = launch_failure
            && let Some(l) = live.launching.take()
        {
            live.store.drop_claude_worker();
            live.store.drop_antigravity_handoff();
            launched = Some(Finished::new(l, Err(error), None));
        }
        annotate(
            &self.host_id.to_string(),
            &self.workers,
            &self.ledger.data,
            scope,
            &mut live.store,
        );
        let previous_yolo: Vec<wire::AskView> = {
            let (yolo, native): (Vec<_>, Vec<_>) = std::mem::take(&mut live.state.asks)
                .into_iter()
                .partition(crate::yolo::is_request);
            live.state.asks = native;
            yolo
        };
        dirty.merge(live.store.drain(vec![], Instant::now()));
        for thread in dirty.dirty_threads() {
            live.tracker.touch(thread);
        }
        let processes = live.store.live_threads();
        for thread in &processes {
            if live.store.thread_busy(thread) {
                live.tracker.touch(thread);
            }
        }
        live.tracker.sync(&processes);
        let mut changes = live.store.host_project(scope, &dirty, &mut live.state);
        crate::yolo::merge(
            &self.asking,
            self.mode_requests
                .values()
                .filter(|r| &r.scope == scope)
                .map(|r| r.view.clone()),
            scope,
            previous_yolo,
            &mut live.state.asks,
            &mut changes,
        );
        for change in &mut changes {
            if live.recovered.is_empty() {
                break;
            }
            match change {
                Change::Sessions { sessions } => {
                    live.recovered.retain(|old| {
                        !sessions
                            .iter()
                            .any(|r| r.id == old.id && r.detail["owned"] == true)
                    });
                    restore(sessions, &live.recovered);
                    live.state.sessions.clone_from(sessions);
                }
                Change::Session { session } => {
                    if session.detail["owned"] == true {
                        live.recovered.retain(|old| old.id != session.id);
                    } else if let Some(old) = live.recovered.iter().find(|old| old.id == session.id)
                    {
                        restore_row(session, old);
                        if let Some(row) =
                            live.state.sessions.iter_mut().find(|r| r.id == session.id)
                        {
                            row.clone_from(session);
                        }
                    }
                }
                _ => {}
            }
        }
        if live
            .state
            .asks
            .iter()
            .any(|a| !live.tokens.contains_key(&a.id))
            && !changes.iter().any(|c| matches!(c, Change::Asks { .. }))
        {
            changes.push(Change::Asks {
                asks: live.state.asks.clone(),
            });
        }
        let mut created = Vec::new();
        if scope.route.runtime == Runtime::OpenCode {
            for (key, result) in live.store.take_opencode_completions() {
                let Some(input) = live.awaiting_input.remove(&key) else {
                    continue;
                };
                let (thread, frame) = match result {
                    Ok(thread) => (thread, Server::Ack { id: key.clone() }),
                    Err(message) => (
                        None,
                        Server::Error {
                            id: Some(key.clone()),
                            message,
                        },
                    ),
                };
                self.receipts.insert(
                    scope.clone(),
                    key.clone(),
                    live.generation,
                    input.digest,
                    frame.clone(),
                );
                if let Some(thread) = thread {
                    self.receipts.opened(scope, &key, &thread);
                    created.push((
                        input.client,
                        Server::Opened {
                            id: key,
                            scope: scope.clone(),
                            thread,
                        },
                    ));
                }
                created.push((input.client, frame));
            }
        } else {
            live.awaiting_input.retain(|key, input| {
                let Some(thread) = input.codex_thread.as_deref() else {
                    return true;
                };
                let Some(result) = live.store.take_codex_input_delivery(thread) else {
                    return true;
                };
                let frame = match result {
                    Ok(()) => Server::Ack { id: key.clone() },
                    Err(message) => Server::Error {
                        id: Some(key.clone()),
                        message,
                    },
                };
                self.receipts.insert(
                    scope.clone(),
                    key.clone(),
                    live.generation,
                    input.digest,
                    frame.clone(),
                );
                created.push((input.client, frame));
                false
            });
        }
        let antigravity = scope.route.runtime == ox_desk_core::runtime::Runtime::Antigravity;
        if let Some(thread) = opened
            && let Some((id, key)) = live.opening.take()
        {
            self.receipts.opened(scope, &key, &thread);
            if antigravity {
                created.push((id, Server::Ack { id: key.clone() }));
            }
            created.push((
                id,
                Server::Opened {
                    id: key,
                    scope: scope.clone(),
                    thread,
                },
            ));
        }
        if !live.store.opening_thread()
            && let Some((id, key)) = live.opening.take()
            && antigravity
        {
            let frame = Server::Error {
                id: Some(key.clone()),
                message: live
                    .store
                    .notice()
                    .unwrap_or("Antigravity could not open a session.")
                    .into(),
            };
            self.receipts.finish(scope, &key, frame.clone());
            created.push((id, frame));
        }
        if live.opening.is_none() {
            live.fork_origin = None;
        }
        if changes.is_empty() {
            if let Some((id, frame)) = handoff {
                self.reply(id, frame)?;
            }
            for (id, frame) in created {
                self.reply(id, frame)?;
            }
            return self.finish_launch(scope, launched);
        }
        if changes.iter().any(|c| matches!(c, Change::Asks { .. })) {
            let mut tokens = HashMap::new();
            for ask in &mut live.state.asks {
                let token = match live.tokens.remove(&ask.id) {
                    Some(existing) => existing,
                    None => nonce()?,
                };
                ask.token.clone_from(&token);
                tokens.insert(ask.id.clone(), token);
            }
            live.tokens = tokens;
            for change in &mut changes {
                if let Change::Asks { asks } = change {
                    asks.clone_from(&live.state.asks);
                }
            }
        }
        if changes.iter().any(|c| {
            matches!(
                c,
                Change::Asks { .. }
                    | Change::Session { .. }
                    | Change::Sessions { .. }
                    | Change::Retired { .. }
            )
        }) {
            self.ledger.update(SavedScope::from_state(&live.state))?;
        }
        let worker_state_changed = changes.iter().any(|c| {
            matches!(
                c,
                Change::State { .. }
                    | Change::Session { .. }
                    | Change::Sessions { .. }
                    | Change::TimelineState { .. }
                    | Change::Transcript { .. }
            )
        });
        self.broadcast(scope, changes)?;
        if let Some((id, frame)) = handoff {
            self.reply(id, frame)?;
        }
        for (id, frame) in created {
            self.reply(id, frame)?;
        }
        self.finish_launch(scope, launched)?;
        // A worker's run just ended (or restarted): carry its final answer in the saved
        // rows so it outlives the process and the host.
        let finals_changed = self.capture_finals(scope);
        if worker_state_changed || finals_changed {
            self.observe_worker_notices(scope)?;
        }
        if finals_changed {
            self.process(scope, vec![], true)?;
        }
        Ok(())
    }
    fn answer(&mut self, reply: Replier, result: Result<Value, String>) -> io::Result<()> {
        match reply {
            Replier::Channel(tx) => {
                let _ = tx.send(result);
                Ok(())
            }
            Replier::Client(id, key) => {
                let (result, error) = match result {
                    Ok(v) => (Some(v), None),
                    Err(e) => (None, Some(e)),
                };
                self.reply(
                    id,
                    Server::WorkerReply {
                        id: key,
                        result,
                        error,
                    },
                )
            }
        }
    }
    fn finish_launch(&mut self, scope: &Scope, launched: Option<Finished>) -> io::Result<()> {
        let Some(f) = launched else {
            return Ok(());
        };
        if let Some(record) = &f.record {
            let error = f.result.as_ref().err().cloned();
            self.ledger.change(|d| match error {
                None => d.handoffs.retain(|h| &h.id != record),
                Some(e) => {
                    if let Some(h) = d.handoffs.iter_mut().find(|h| &h.id == record) {
                        h.status = "failed".into();
                        h.message = Some(e);
                    }
                }
            })?;
            if !f.origin.launch.starts_with("handoff:")
                && let Some(source) = f.origin.parent.scope.clone()
                && self.running.contains_key(&source)
                && &source != scope
            {
                self.process(&source, vec![], true)?;
            }
            let result = f
                .result
                .map(|()| json!({"scope": scope, "session": f.thread, "relation": "handoff"}));
            return self.answer(f.reply, result);
        }
        if f.retire
            && let Some(thread) = f.thread.clone()
        {
            if let Some(entry) = self.workers.get_mut(scope, &thread) {
                entry.marks.cancelled = true;
            }
            if let Some(live) = self.running.get_mut(scope) {
                let _ = live.store.stop(&thread);
            }
            let _ = self.end_thread(scope, &thread, true);
            if self.running.contains_key(scope) {
                self.process(scope, vec![], true)?;
                self.reap_empty(scope)?;
            }
            return self.answer(f.reply, Err(flows::ENDED_PARENT.into()));
        }
        if (f.detached || f.origin.linked.is_some())
            && let Err(e) = &f.result
        {
            let origin = f.origin.clone();
            let message = e.clone();
            self.ledger.change(|d| {
                d.record_outcome(crate::durable::Outcome {
                    origin,
                    status: "failed".into(),
                    message,
                })
            })?;
        }
        let result = f.result.and_then(|()| {
            let thread = f.thread.expect("launched session");
            let job = self
                .workers
                .get(scope, &thread)
                .map(|e| e.origin.job_id)
                .ok_or("The worker stamp was not recorded.")?;
            self.job_status(job)
        });
        self.answer(f.reply, result)
    }
    fn update_reserved(&self) -> bool {
        self.clients
            .values()
            .any(|client| client.update_lease.is_some())
    }

    fn scope_projects(&self) -> std::collections::BTreeSet<PathBuf> {
        self.running
            .keys()
            .chain(self.cold.keys())
            .map(|s| s.project.clone())
            .collect()
    }
    fn host_info(&mut self) -> wire::HostInfo {
        let profile =
            ox_host_web::host_profile::HostProfile::read(&self.state_dir).unwrap_or_default();
        wire::HostInfo {
            subscription_limits: self.subscription_limits.current(now_unix()),
            mode_policy: HostPolicy::read(&self.state_dir).ok(),
            id: Some(self.host_id.to_string()),
            name: Some(if profile.name.is_empty() {
                self.host_name.clone()
            } else {
                profile.name
            }),
            labels: profile.labels,
            resources: Some(self.host_resources()),
            version: crate::handover::version().into(),
            epoch: self.replay.cursor.epoch.clone(),
        }
    }
    fn host_resources(&mut self) -> wire::HostResources {
        let workers = self
            .running
            .iter()
            .map(|(scope, live)| {
                live.store
                    .active_thread_ids()
                    .filter(|id| {
                        self.workers.get(scope, id).is_some_and(|e| {
                            e.origin.relation == ox_desk_core::origin::Relation::Worker
                        })
                    })
                    .count()
            })
            .sum();
        self.resources.sample(workers)
    }
    /// Work a drain waits for: turns in progress in any loaded thread, worker
    /// launches and handoff targets not yet confirmed, and handoffs still
    /// preparing or reading their source.
    fn running_turns(&self) -> usize {
        self.running
            .values()
            .map(|live| {
                live.store.active_turns()
                    + usize::from(live.launching.is_some())
                    + usize::from(live.confirming.is_some())
            })
            .sum::<usize>()
            + self.preparing.len()
            + self.reading.len()
            + self
                .ledger
                .data
                .forwarded
                .values()
                .filter(|j| j.preparing())
                .count()
    }
    fn start_accepted(
        &mut self,
        accepted: Result<Accepted, String>,
        reply: Replier,
    ) -> io::Result<()> {
        match accepted {
            Ok(Accepted::Launch(scope, launching)) => {
                let live = self.running.get_mut(&scope).expect("ensured worker scope");
                live.launching = Some(Launching { reply, ..launching });
                self.process(&scope, vec![], false)
            }
            Ok(Accepted::Ask(asking)) => self.ask_yolo(asking, reply),
            Err(e) => self.answer(reply, Err(e)),
        }
    }
    fn new_job_id(&self) -> Result<u64, String> {
        loop {
            let candidate = getrandom::u64().map_err(|e| e.to_string())? & ((1u64 << 52) - 1);
            if candidate > 0 && !self.job_in_use(candidate) {
                return Ok(candidate);
            }
        }
    }
    fn job_in_use(&self, candidate: u64) -> bool {
        self.ledger.data.forwarded.contains_key(&candidate)
            || self.workers.job_ids().any(|j| j == candidate)
            || self
                .ledger
                .data
                .worker_notices
                .observed
                .contains_key(&candidate)
            || self.asking.contains_key(&candidate)
            || self.mode_requests.contains_key(&candidate)
            || self
                .ledger
                .data
                .outcomes
                .iter()
                .any(|o| o.origin.job_id == candidate)
            || self.running.values().any(|l| {
                l.launching
                    .as_ref()
                    .is_some_and(|job| job.origin.job_id == candidate)
                    || l.confirming
                        .as_ref()
                        .is_some_and(|c| c.launch.origin.job_id == candidate)
            })
    }
    fn worker_source(
        &self,
        caller: &Caller,
    ) -> Result<(PathBuf, Option<String>, Option<Level>, bool), String> {
        let (project, parent, ceiling, parent_origin, external, parent_scope) = match caller {
            Caller::Hosted { scope, hint, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("This OX helper's route is not open on the session host.")?;
                let parent = crate::policy::single_parent(&live.store, hint.as_deref())?;
                let ceiling = live.store.applied_ceiling(Some(&parent));
                let origin = self.workers.get(scope, &parent).map(|e| e.origin.clone());
                (
                    scope.project.clone(),
                    Some(parent),
                    ceiling,
                    origin,
                    false,
                    Some(scope),
                )
            }
            Caller::Operator { scope, parent, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("Open the parent session's route first.")?;
                if live.store.catalog_row(parent).is_none()
                    || live.store.transcript(parent).is_none()
                {
                    return Err("Open the parent session before starting a worker from it.".into());
                }
                let ceiling = live.store.applied_ceiling(Some(parent));
                let origin = self.workers.get(scope, parent).map(|e| e.origin.clone());
                (
                    scope.project.clone(),
                    Some(parent.clone()),
                    ceiling,
                    origin,
                    false,
                    Some(scope),
                )
            }
            Caller::Linked { project, .. } => (project.clone(), None, None, None, false, None),
            Caller::External(c) => (c.project.clone(), None, None, None, true, None),
        };
        let source = match caller {
            Caller::Linked { stamp, .. } => Some(stamp.verified_level.min(stamp.link_ceiling)),
            _ => parent_scope
                .and_then(|_| crate::workers::source_level(ceiling, parent_origin.as_ref())),
        };
        Ok((project, parent, source, external))
    }
    fn accept_launch(&mut self, caller: &Caller, start: WorkerStart) -> Result<Accepted, String> {
        start.validate()?;
        let (project, parent, source, external) = self.worker_source(caller)?;
        let resolved =
            crate::workers::grant(source, external, start.mode_request(), start.runtime)?;
        let policy = HostPolicy::read(&self.state_dir)
            .map_err(|e| format!("Could not read the host mode policy: {e}"))?;
        let ceiling = source.unwrap_or(crate::workers::TERMINAL_CEILING);
        let requested = resolved.level;
        let resolved = policy.child_start(resolved, start.mode_request(), ceiling, start.runtime);
        let this_parent = crate::workers::parent_ref(caller, parent.clone());
        self.check_worker_cap(&this_parent)?;
        let project = ox_desk_core::worker_checkout::resolve(
            &project,
            start.checkout.as_deref(),
            source == Some(ox_desk_core::modes::Level::Yolo),
        )?;
        let scope = canonical_scope(Scope {
            project,
            route: start.route(),
        })
        .map_err(|e| e.to_string())?;
        let job_id = match caller {
            Caller::Linked { job, .. } => *job,
            _ => self.new_job_id()?,
        };
        let mut origin = crate::workers::origin(caller, parent.clone(), &start, job_id, resolved);
        origin.ceiling = requested.max(resolved.level);
        if let Some(warning) = self.write_overlap(&scope, resolved.level) {
            self.launch_warnings.insert(job_id, warning.clone());
            if let Some(live) = caller_scope(caller).and_then(|s| self.running.get_mut(s)) {
                live.store.post_notice(warning);
            }
        }
        if resolved.level.needs_person() {
            match caller {
                _ if policy == HostPolicy::AlwaysYolo => {
                    origin.yolo = Some(YoloAllowance::HostPolicy)
                }
                _ if policy == HostPolicy::YoloAsksEveryTime => {
                    return self.worker_ask(origin, start, scope);
                }
                Caller::Operator { device, .. } if start.yolo_confirmed => {
                    origin.yolo = Some(YoloAllowance::Operator {
                        device: device.clone(),
                    });
                }
                Caller::Operator { .. } => {
                    return Err("Confirm YOLO for this worker first. It would run without permission prompts.".into());
                }
                Caller::Hosted { scope: ps, .. } => {
                    let session = parent.as_deref().expect("hosted parent");
                    match self.yolo_grant(ps, session) {
                        Some(grant) => origin.yolo = Some(grant),
                        None if start.mode_request()
                            == ox_desk_core::modes::ModeRequest::Inherit =>
                        {
                            origin.yolo = Some(YoloAllowance::Inherited {
                                session: session.to_owned(),
                            });
                        }
                        None => return self.worker_ask(origin, start, scope),
                    }
                }
                Caller::Linked { .. } => {
                    return Err(
                        "Cross-host YOLO starts require a target-side operator request.".into(),
                    );
                }
                Caller::External(_) => {
                    return Err("A terminal launch cannot start YOLO workers.".into());
                }
            }
        }
        let launching = self.prepare_launch(&scope, origin, Payload::Worker(start))?;
        Ok(Accepted::Launch(scope, launching))
    }
    fn worker_ask(
        &self,
        origin: Origin,
        start: WorkerStart,
        target: Scope,
    ) -> Result<Accepted, String> {
        let session = origin
            .parent
            .session
            .as_deref()
            .ok_or("A YOLO request needs an open parent session.")?;
        let parent_title = origin
            .parent
            .scope
            .as_ref()
            .and_then(|scope| self.running.get(scope))
            .and_then(|live| live.store.catalog_row(session))
            .map(|row| row.title().to_owned())
            .unwrap_or_else(|| "OX session".to_owned());
        Ok(Accepted::Ask(Asking {
            origin,
            start,
            target,
            parent_title,
            memory_brief: None,
        }))
    }
    /// Open the child's route and check that nothing else is opening there.
    fn prepare_launch(
        &mut self,
        scope: &Scope,
        origin: Origin,
        payload: Payload,
    ) -> Result<Launching, String> {
        self.ensure(scope, None).map_err(|e| e.to_string())?;
        let live = &self.running[scope];
        if live.opening.is_some() || live.launching.is_some() {
            return Err("Another session is opening on the target route. Retry shortly.".into());
        }
        Ok(Launching {
            origin,
            payload,
            reply: Replier::Channel(std::sync::mpsc::sync_channel(1).0),
            stage: Stage::Opening,
            deadline: Instant::now() + Duration::from_secs(60),
            detached: false,
            abandoned: false,
            memory_brief: None,
        })
    }
    fn job_status(&self, job_id: u64) -> Result<Value, String> {
        let (scope, id, entry) = self
            .workers
            .job(job_id)
            .ok_or("Unknown worker job on this session host.")?;
        let live = self.running.get(scope);
        let state = live.map(|l| &l.state).or_else(|| self.cold.get(scope));
        let view = state.and_then(|s| s.sessions.iter().find(|v| v.id == id));
        let timeline = state.and_then(|s| s.transcripts.get(id));
        let native = live
            .and_then(|l| l.store.terminal_session_id(id).ok())
            .unwrap_or_else(|| id.rsplit(':').next().unwrap_or(id));
        let ready = live.is_some_and(|l| l.store.state() == &Connection::Ready);
        let mut status =
            crate::workers::status(entry, id, native, view, timeline, ready, self.operators());
        status["checkout"] = json!(scope.project);
        status["parent"] = json!(entry.origin.parent);
        if let Some(link) = &entry.origin.linked {
            status["parent"] = json!(link.parent);
            status["linked_parent"] = json!(link);
        }
        let requests = self.worker_requests(scope, id, &entry.origin);
        if !requests.is_empty() {
            let answerable = requests.iter().any(|r| r["parent_may_answer"] == true);
            if answerable && status["waiting_for_operator"].is_object() {
                status["waiting_for_operator"]["parent_may_answer"] = json!(true);
                status["waiting_for_operator"]["message"] = json!(format!(
                    "The worker is waiting on a request you may answer: read requests, then call answer_ox_worker with job_id {job_id} and its request_id. It is also shown to the user in OX Desktop and on paired phones; whoever answers first decides."
                ));
            }
            status["requests"] = json!(requests);
        }
        Ok(status)
    }
    /// The verified level of the hosted session that started this worker, bounded
    /// by its own stamp. `None` when it is a terminal launch, closed or unverified.
    pub(super) fn parent_level(&self, origin: &Origin) -> Option<Level> {
        let scope = origin.parent.scope.as_ref()?;
        let session = origin.parent.session.as_deref()?;
        let live = self.running.get(scope)?;
        crate::workers::source_level(
            live.store.applied_ceiling(Some(session)),
            self.workers.get(scope, session).map(|e| &e.origin),
        )
    }
    /// The open requests of one worker session, as its parent agent sees them.
    pub(super) fn worker_requests(&self, scope: &Scope, id: &str, origin: &Origin) -> Vec<Value> {
        let Some(live) = self.running.get(scope) else {
            return vec![];
        };
        let level = self.parent_level(origin);
        let setting = worker_approvals();
        worker_asks(&live.store, id)
            .map(|ask| {
                crate::workers::request_view(
                    ask,
                    crate::workers::parent_may_answer(origin, level, ask, setting),
                )
            })
            .collect()
    }
    /// Resolve the caller to exactly one parent identity, or refuse. Every
    /// parent operation uses this one result.
    fn resolve(&self, caller: &Caller) -> Result<crate::workers::Resolved, String> {
        use crate::workers::Resolved;
        match caller {
            Caller::Hosted { scope, hint, .. } => {
                let live = self
                    .running
                    .get(scope)
                    .ok_or("This OX helper's route is not open on the session host.")?;
                let session = crate::policy::single_parent(&live.store, hint.as_deref())?;
                Ok(Resolved::Hosted {
                    scope: scope.clone(),
                    session,
                })
            }
            Caller::Linked { stamp, .. } => Ok(Resolved::Linked {
                source: stamp.host.clone(),
                device: stamp.device.clone(),
                request: stamp.request_id.clone(),
            }),
            Caller::External(c) => Ok(Resolved::Terminal {
                project: c.project.clone(),
                launch: crate::workers::launch_digest("terminal", &c.launch),
                session: c
                    .session
                    .clone()
                    .filter(|s| crate::workers::valid_session(s)),
            }),
            Caller::Operator { .. } => Err("Operators act on worker sessions directly.".into()),
        }
    }
    /// Publish and save changed worker marks for a scope, live or cold.
    fn persist_marks(&mut self, scope: &Scope) -> io::Result<()> {
        if self.running.contains_key(scope) {
            return self.process(scope, vec![], true);
        }
        let Some(cold) = self.cold.get_mut(scope) else {
            return Ok(());
        };
        for row in &mut cold.sessions {
            if let Some(entry) = self.workers.get(scope, &row.id) {
                row.detail["worker_marks"] = json!(entry.marks);
            }
        }
        self.ledger.update(SavedScope::from_state(cold))
    }
    /// Desktop windows and phones subscribed to the host right now.
    fn operators(&self) -> crate::workers::Operators {
        crate::workers::Operators {
            connected: self
                .clients
                .values()
                .filter(|c| c.catalog || c.subscription.is_some())
                .count(),
        }
    }
    /// The parent agent's four tools. Every answer comes from host session state.
    fn parent_call(&mut self, caller: &Caller, call: ParentCall) -> Result<Value, String> {
        let me = self.resolve(caller)?;
        self.call_as(me, call)
    }
    fn call_as(&mut self, me: crate::workers::Resolved, call: ParentCall) -> Result<Value, String> {
        if self.update_reserved()
            && matches!(call, ParentCall::Steer { .. } | ParentCall::Answer { .. })
        {
            return Err(crate::handover::REFUSAL.into());
        }
        let owned = |e: &crate::workers::Entry| crate::workers::owns(&me, e);
        let job_id = match &call {
            ParentCall::Bind { session } => {
                let crate::workers::Resolved::Terminal { launch, .. } = &me else {
                    return Err("Only a terminal OX launch binds its session.".into());
                };
                if !crate::workers::valid_session(session) {
                    return Err("Invalid native session ID.".into());
                }
                // Write-once, and only on workers this exact launch started.
                let mut scopes = Vec::new();
                for (scope, entry) in self.workers.entries_mut() {
                    if &entry.origin.launch == launch
                        && entry.origin.parent.session.is_none()
                        && entry.marks.parent_session.is_none()
                    {
                        entry.marks.parent_session = Some(session.clone());
                        scopes.push(scope.clone());
                    }
                }
                let mut remote_bound = 0;
                self.ledger
                    .change(|d| {
                        for j in d.forwarded.values_mut().filter(|j| {
                            j.origin.launch == *launch && j.origin.parent.session.is_none()
                        }) {
                            j.origin.parent.session = Some(session.clone());
                            remote_bound += 1;
                        }
                    })
                    .map_err(|e| e.to_string())?;
                let bound = scopes.len() + remote_bound;
                scopes.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                scopes.dedup();
                for scope in scopes {
                    self.persist_marks(&scope).map_err(|e| e.to_string())?;
                }
                return Ok(json!({ "bound": bound }));
            }
            ParentCall::ListHosts => unreachable!("host inventory is answered asynchronously"),
            ParentCall::List => {
                let mut jobs: Vec<u64> = self
                    .workers
                    .jobs()
                    .filter(|(_, _, e)| owned(e))
                    .map(|(_, _, e)| e.origin.job_id)
                    .collect();
                jobs.sort_unstable();
                let unstarted = self.unstarted_jobs(&me);
                let jobs = jobs
                    .into_iter()
                    .filter_map(|j| self.job_status(j).ok())
                    .chain(
                        unstarted
                            .into_iter()
                            .filter_map(|j| self.unstarted_job(&me, j)),
                    )
                    .map(|mut v| {
                        v.as_object_mut().map(|o| o.remove("messages"));
                        v
                    })
                    .chain(
                        self.ledger
                            .data
                            .forwarded
                            .values()
                            .filter(|j| j.owned(&me))
                            .map(|j| j.summary()),
                    )
                    .collect::<Vec<_>>();
                return Ok(json!({ "jobs": jobs }));
            }
            ParentCall::Status { job_id }
            | ParentCall::Steer { job_id, .. }
            | ParentCall::Answer { job_id, .. }
            | ParentCall::Cancel { job_id } => *job_id,
            ParentCall::ReadThread { .. } => {
                return Err("read_ox_thread is answered separately.".into());
            }
        };
        if self.workers.job(job_id).is_none()
            && let Some(status) = self.unstarted_job(&me, job_id)
        {
            return match call {
                ParentCall::Steer { .. } | ParentCall::Answer { .. } => Err(format!(
                    "This worker has not started: {}.",
                    status["status"].as_str().unwrap_or("waiting")
                )),
                ParentCall::Cancel { .. } => {
                    if let Some(done) = self.withdraw(job_id).map_err(|e| e.to_string())? {
                        Ok(done)
                    } else if let Some(done) =
                        self.abort_launch(job_id).map_err(|e| e.to_string())?
                    {
                        Ok(done)
                    } else {
                        Ok(status)
                    }
                }
                _ => Ok(status),
            };
        }
        let (scope, id) = self
            .workers
            .job(job_id)
            .filter(|(_, _, e)| owned(e))
            .map(|(s, id, _)| (s.clone(), id.to_owned()))
            .ok_or("Unknown worker job for this parent session.")?;
        match call {
            ParentCall::List
            | ParentCall::ListHosts
            | ParentCall::Bind { .. }
            | ParentCall::Status { .. }
            | ParentCall::ReadThread { .. } => self.job_status(job_id),
            ParentCall::Answer {
                request,
                choice,
                answers,
                ..
            } => self.parent_answer(&scope, &id, request, choice, answers),
            ParentCall::Steer { text, .. } => {
                let text = text.trim();
                if text.is_empty() || text.len() > 24 * 1024 {
                    return Err("Send a nonempty worker message of at most 24 KiB.".into());
                }
                let status = self.job_status(job_id)?;
                let live = self
                    .running
                    .get_mut(&scope)
                    .ok_or("This worker is not accepting input.")?;
                // Text from the parent never answers an open request.
                if worker_asks(&live.store, &id).next().is_some() {
                    return Err(
                        "The worker is waiting on an open request. Answer it with answer_ox_worker if you may, or wait for the operator."
                            .into(),
                    );
                }
                if status["can_steer"] != true {
                    return Err("This worker is not accepting input.".into());
                }
                live.store
                    .dispatch_as(
                        Action::SendText(Some(id), text.to_owned()),
                        Sender::ParentAgent,
                    )
                    .map_err(|e| e.to_string())?;
                self.process(&scope, vec![], true)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"accepted": true}))
            }
            ParentCall::Cancel { .. } => {
                let status = self.job_status(job_id)?;
                if matches!(
                    status["status"].as_str(),
                    Some("running" | "waiting_for_operator")
                ) {
                    if let Some(live) = self.running.get_mut(&scope) {
                        let _ = live.store.stop(&id);
                    }
                    if let Some(entry) = self.workers.get_mut(&scope, &id) {
                        entry.marks.cancelled = true;
                    }
                    if self.running.contains_key(&scope) {
                        self.process(&scope, vec![], true)
                            .map_err(|e| e.to_string())?;
                    }
                }
                self.job_status(job_id)
            }
        }
    }
    /// A parent agent answers one of its worker's open requests, where policy lets it.
    fn parent_answer(
        &mut self,
        scope: &Scope,
        id: &str,
        request: RequestId,
        choice: Option<usize>,
        answers: Option<Vec<String>>,
    ) -> Result<Value, String> {
        let origin = self
            .workers
            .get(scope, id)
            .map(|e| e.origin.clone())
            .ok_or("Unknown worker job for this parent session.")?;
        let level = self.parent_level(&origin);
        let setting = worker_approvals();
        let live = self
            .running
            .get_mut(scope)
            .ok_or("This worker is not running, so it has no open request.")?;
        let ask = worker_asks(&live.store, id)
            .find(|a| a.id == request)
            .ok_or("This worker has no open request with that request_id. Read its requests with ox_worker_status.")?;
        crate::workers::parent_may_answer(&origin, level, ask, setting).map_err(str::to_owned)?;
        let result = match (choice, answers) {
            (Some(choice), None) => {
                crate::workers::parent_may_choose(ask, choice).map_err(str::to_owned)?;
                live.store.answer(&request, choice)
            }
            (None, Some(answers)) if !ask.questions.is_empty() => {
                live.store.answer_questions(&request, &answers)
            }
            _ => {
                return Err(
                    "Answer an approval with choice, or a question form with answers in order."
                        .into(),
                );
            }
        };
        result.map_err(|e| e.to_string())?;
        live.tokens.remove(&request);
        self.process(scope, vec![], true)
            .map_err(|e| e.to_string())?;
        Ok(json!({"answered": true, "request_id": request}))
    }
    /// Cancel a worker whose runtime is still opening: nothing was sent to it yet, so
    /// the launch is abandoned, its launcher hears "cancelled", and the worker's
    /// scope is closed again if nothing else uses it. A launch whose first message is
    /// already on its way cannot be recalled and keeps reporting `starting`.
    fn abort_launch(&mut self, job_id: u64) -> io::Result<Option<Value>> {
        let Some(scope) = self
            .running
            .iter()
            .find(|(_, live)| {
                live.launching.as_ref().is_some_and(|l| {
                    job_id != 0 && l.origin.job_id == job_id && l.stage != Stage::Sent
                })
            })
            .map(|(scope, _)| scope.clone())
        else {
            return Ok(None);
        };
        let l = self
            .running
            .get_mut(&scope)
            .and_then(|live| {
                live.store.drop_claude_worker();
                live.launching.take()
            })
            .expect("found launching");
        const MESSAGE: &str = "The worker launch was cancelled before it started.";
        if l.detached {
            let origin = l.origin.clone();
            self.ledger.change(|d| {
                d.record_outcome(crate::durable::Outcome {
                    origin,
                    status: "cancelled".into(),
                    message: MESSAGE.into(),
                })
            })?;
        }
        if !l.detached {
            self.ledger
                .change(|d| d.worker_notices.observe(&l.origin, "cancelled", MESSAGE))?;
        }
        self.answer(l.reply, Err(MESSAGE.into()))?;
        self.process(&scope, vec![], false)?;
        self.reap_empty(&scope)?;
        Ok(Some(json!({"job_id": job_id, "status": "cancelled"})))
    }
    /// The live processes this host runs: one per Claude session, one per Codex scope.
    fn processes(&self) -> Vec<ProcessView> {
        let mut out = Vec::new();
        for (scope, live) in &self.running {
            // Include a worker still starting on its dedicated process.
            let threads = live.store.process_threads();
            if threads.is_empty() {
                continue;
            }
            out.extend(self.scope_rows(scope, live, threads));
        }
        out.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.threads.cmp(&b.threads))
        });
        out
    }
    /// The process rows for one route's live threads: one per Claude session, one per Codex scope.
    fn scope_rows(&self, scope: &Scope, live: &Live, threads: Vec<String>) -> Vec<ProcessView> {
        let mut out = Vec::new();
        let runtime = scope.route.runtime;
        let groups: Vec<Vec<String>> = if runtime != ox_desk_core::runtime::Runtime::Codex {
            threads.into_iter().map(|t| vec![t]).collect()
        } else {
            vec![threads]
        };
        for group in groups {
            let seen = |t: &String| live.tracker.threads.get(t).copied();
            let started = group
                .iter()
                .filter_map(seen)
                .map(|s| s.started)
                .min()
                .unwrap_or(live.tracker.scope.started);
            let last = group
                .iter()
                .filter_map(seen)
                .map(|s| s.last_unix)
                .max()
                .unwrap_or(live.tracker.scope.last_unix);
            let worker = group.iter().all(|t| self.workers.get(scope, t).is_some());
            out.push(ProcessView {
                kind: if worker {
                    "worker"
                } else {
                    match runtime {
                        ox_desk_core::runtime::Runtime::Codex => "codex",
                        ox_desk_core::runtime::Runtime::Claude => "claude",
                        ox_desk_core::runtime::Runtime::Antigravity => "antigravity",
                        ox_desk_core::runtime::Runtime::OpenCode => "opencode",
                    }
                }
                .into(),
                scope: scope.clone(),
                project: scope.project.clone(),
                route: scope.route.label(),
                model: group
                    .first()
                    .map(|t| live.store.options(Some(t)).model)
                    .filter(|m| !m.is_empty()),
                started_at: started,
                last_activity: last,
                running: group.iter().any(|t| self.thread_busy(scope, t)),
                draft: group.iter().all(|t| live.store.is_draft(t)),
                threads: group,
            });
        }
        out
    }
    /// Whether ending `thread` in `scope` would interrupt work: its own turn, approval,
    /// sign-in or terminal, a launch still opening for it, or a busy host-owned worker
    /// it started on any scope, at any depth. The reaper and the process list share this.
    fn thread_busy(&self, scope: &Scope, thread: &str) -> bool {
        let mut stack = vec![(scope.clone(), thread.to_owned())];
        let mut seen = HashSet::new();
        while let Some((s, t)) = stack.pop() {
            if !seen.insert((s.clone(), t.clone())) {
                continue;
            }
            let launching = self.running.values().any(|l| {
                l.launching.as_ref().is_some_and(|launch| {
                    launch.origin.parent.scope.as_ref() == Some(&s)
                        && launch.origin.parent.session.as_deref() == Some(t.as_str())
                })
            });
            if launching {
                return true;
            }
            // A YOLO request held for the operator belongs to its parent: ending or
            // reaping the parent would silently drop a decision a person still owes.
            if self
                .mode_requests
                .values()
                .any(|r| r.scope == s && r.view.thread.as_deref() == Some(&t))
            {
                return true;
            }
            if self.asking.values().any(|a| {
                a.origin.parent.scope.as_ref() == Some(&s)
                    && a.origin.parent.session.as_deref() == Some(t.as_str())
            }) {
                return true;
            }
            if let Some(live) = self.running.get(&s)
                && (live.store.thread_busy(&t)
                    || live.store.in_terminal(&t)
                    || live.launching.is_some()
                    || live.confirming.is_some())
            {
                return true;
            }
            stack.extend(
                self.workers
                    .children_of(&s, &t)
                    .map(|(cs, cid)| (cs.clone(), cid.to_owned())),
            );
        }
        false
    }
    /// End one thread's process and the host-owned workers it started. The thread
    /// stays in the catalog; selecting it again resumes it.
    ///
    /// A busy thread is refused unless `force` is set, so a stale window or remote client
    /// cannot kill a running turn and its workers by accident.
    fn end_thread(&mut self, scope: &Scope, thread: &str, force: bool) -> io::Result<()> {
        if !self.running.contains_key(scope) {
            return Err(io::Error::other(
                "This route is not open on the session host.",
            ));
        }
        if !force && self.thread_busy(scope, thread) {
            return Err(io::Error::other(
                "This session is busy. Confirm ending it, which cancels its running work.",
            ));
        }
        let live = self
            .running
            .get(scope)
            .ok_or_else(|| io::Error::other("This route is not open on the session host."))?;
        if !live.store.has_live_session(thread) {
            return Err(io::Error::other("This session has no live process to end."));
        }
        // End the parent first: if the store refuses (not ready, sign-in needed),
        // its workers must not already be cancelled.
        let live = self.running.get_mut(scope).expect("checked above");
        let busy = live.store.thread_busy(thread);
        self.capture_finals(scope);
        let live = self.running.get_mut(scope).expect("checked above");
        live.store.end_session(thread)?;
        live.tracker.threads.remove(thread);
        self.drop_grant(scope, thread)?;
        if busy && let Some(entry) = self.workers.get_mut(scope, thread) {
            entry.marks.cancelled = true;
        }
        self.withdraw_activations(
            |r| &r.scope == scope && r.view.thread.as_deref() == Some(thread),
            "The session ended before its pending mode request was answered.",
        )?;
        self.end_children(scope, thread)?;
        Ok(())
    }
    /// Ending a parent ends its host-owned workers: launches still opening are
    /// aborted and running ones are cancelled and ended.
    fn end_children(&mut self, scope: &Scope, thread: &str) -> io::Result<()> {
        self.withdraw_asks_under(scope, thread)?;
        // The whole lineage from the durable registry, not only live children: a middle
        // parent that already exited must not hide its own running workers.
        let mut tree: Vec<(Scope, String)> = vec![(scope.clone(), thread.to_owned())];
        let mut next = 0;
        while next < tree.len() {
            let (s, t) = tree[next].clone();
            let kids: Vec<(Scope, String)> = self
                .workers
                .children_of(&s, &t)
                .map(|(cs, cid)| (cs.clone(), cid.to_owned()))
                .collect();
            for kid in kids {
                if !tree.contains(&kid) {
                    tree.push(kid);
                }
            }
            next += 1;
        }
        let launches: Vec<u64> = self
            .running
            .values()
            .filter_map(|l| l.launching.as_ref())
            .filter(|l| {
                l.origin.job_id != 0
                    && tree.iter().any(|(s, t)| {
                        l.origin.parent.scope.as_ref() == Some(s)
                            && l.origin.parent.session.as_deref() == Some(t.as_str())
                    })
            })
            .map(|l| l.origin.job_id)
            .collect();
        for job in launches {
            self.abort_launch(job)?;
        }
        // A launch whose first message is already on its way cannot be recalled. Mark
        // it, so the session it opens is retired instead of running without a parent.
        for live in self.running.values_mut() {
            if let Some(l) = live.launching.as_mut()
                && l.stage == Stage::Sent
                && matches!(l.payload, Payload::Worker(_))
                && tree.iter().any(|(s, t)| {
                    l.origin.parent.scope.as_ref() == Some(s)
                        && l.origin.parent.session.as_deref() == Some(t.as_str())
                })
            {
                l.abandoned = true;
            }
        }
        for (child_scope, child) in tree.into_iter().skip(1) {
            let live = self
                .running
                .get(&child_scope)
                .is_some_and(|l| l.store.has_live_session(&child));
            if live
                && self.end_thread(&child_scope, &child, true).is_ok()
                && self.running.contains_key(&child_scope)
            {
                self.process(&child_scope, vec![], true)?;
                self.reap_empty(&child_scope)?;
            }
        }
        Ok(())
    }
    /// Close a route's runtime once it serves nothing: no live process or
    /// subscription, no turn, approval or launch. Dropping its store kills the
    /// app-server (or the idle Claude draft process). The saved sessions stay listed
    /// and the next subscriber or action reopens the route.
    fn reap_empty(&mut self, scope: &Scope) -> io::Result<()> {
        let Some(live) = self.running.get(scope) else {
            return Ok(());
        };
        let idle = live.store.live_threads().is_empty()
            && live.store.active_turns() == 0
            && live.store.asks().is_empty()
            && !live.store.opening_thread()
            && !live.store.opening_terminal()
            && live.launching.is_none()
            && live.confirming.is_none()
            && live.opening.is_none()
            && live.awaiting_input.is_empty()
            && live.terminal.is_none()
            && live.held_handoff.is_none()
            && !self.mode_requests.values().any(|r| &r.scope == scope);
        if !idle {
            return Ok(());
        }
        // Keep each worker's final answer: the process is about to close.
        self.capture_finals(scope);
        self.withdraw_activations(|r| &r.scope == scope, "The session host closed this route.")?;
        let live = self.running.remove(scope).expect("checked above");
        let mut recovered = SavedScope::from_state(&live.state).recover();
        self.ledger.data.worker_notices.apply_to(&mut recovered);
        drop(live);
        let changes = vec![
            Change::State {
                connection: recovered.connection.clone(),
                capabilities: vec![],
                detail: live_detail_after_drop(&recovered),
            },
            Change::Sessions {
                sessions: recovered.sessions.clone(),
            },
            Change::Asks { asks: vec![] },
        ];
        self.cold.insert(scope.clone(), recovered);
        // Carry the workers' marks, final answers included, into the saved rows.
        self.persist_marks(scope)?;
        self.broadcast(scope, changes)
    }
    /// End sessions and empty routes unused for the idle timeout. The sessions stay resumable.
    fn reap_idle(&mut self, now: Instant) -> io::Result<()> {
        let mut due: Vec<(Scope, String)> = Vec::new();
        let mut empty: Vec<Scope> = Vec::new();
        for (scope, live) in &mut self.running {
            if live
                .held_handoff
                .as_ref()
                .is_some_and(|(_, _, deadline)| *deadline <= now)
            {
                live.held_handoff = None;
                empty.push(scope.clone());
            }
        }
        for (scope, live) in &self.running {
            let threads = live.store.live_threads();
            if threads.is_empty() {
                if reapable(
                    now.saturating_duration_since(live.tracker.scope.last),
                    self.idle,
                    live.store.active_turns() > 0,
                ) {
                    empty.push(scope.clone());
                }
                continue;
            }
            for thread in threads {
                let Some(seen) = live.tracker.threads.get(&thread) else {
                    continue;
                };
                let busy = self.thread_busy(scope, &thread);
                if reapable(now.saturating_duration_since(seen.last), self.idle, busy) {
                    due.push((scope.clone(), thread));
                }
            }
        }
        for (scope, thread) in due {
            if self.end_thread(&scope, &thread, false).is_ok() && self.running.contains_key(&scope)
            {
                self.process(&scope, vec![], true)?;
            }
            self.reap_empty(&scope)?;
        }
        for scope in empty {
            self.reap_empty(&scope)?;
        }
        self.expire_workers()
    }
    fn broadcast(&mut self, scope: &Scope, changes: Vec<Change>) -> io::Result<()> {
        self.replay.cursor.seq = self.ledger.next_sequence(self.replay.cursor.seq)?;
        if self.clients.values().any(catalog_only) {
            let mut catalog: Vec<_> = changes
                .iter()
                .filter_map(|change| match change {
                    Change::Session { session } if session.detail["owned"] == true => {
                        Some(Change::Session {
                            session: catalog_row(session),
                        })
                    }
                    Change::Sessions { sessions } => Some(Change::Sessions {
                        sessions: sessions
                            .iter()
                            .filter(|s| s.detail["owned"] == true)
                            .map(catalog_row)
                            .collect(),
                    }),
                    _ => None,
                })
                .collect();
            if changes
                .iter()
                .any(|c| matches!(c, Change::State { .. } | Change::Asks { .. }))
                && let Some(live) = self.running.get(scope)
            {
                catalog.push(Change::State {
                    detail: catalog_detail(&live.state),
                    connection: String::new(),
                    capabilities: vec![],
                });
            }
            // Web push subscribes without a scope and needs notifications, which
            // carry no transcript content.
            catalog.extend(
                changes
                    .iter()
                    .filter(|c| matches!(c, Change::Notification { .. }))
                    .cloned(),
            );
            if !catalog.is_empty() {
                let packet = encode(&Server::Delta {
                    cursor: self.replay.cursor.clone(),
                    scope: scope.clone(),
                    changes: catalog,
                })?;
                self.clients.retain(|_, c| {
                    !catalog_only(c)
                        || !c.supports_scope(scope)
                        || c.peer.send_for(packet.clone(), "catalog watch")
                });
            }
        }
        let packet = encode(&Server::Delta {
            cursor: self.replay.cursor.clone(),
            scope: scope.clone(),
            changes,
        })?;
        self.replay.append(scope.clone(), packet.clone());
        let what = format!("{} {}", scope.project.display(), scope.route.label());
        self.clients.retain(|_, c| {
            !c.supports_scope(scope)
                || match c.subscription.as_ref() {
                    Some(Some(s)) if s == scope => c.peer.send_for(packet.clone(), &what),
                    Some(None) if !c.catalog => c.peer.send_for(packet.clone(), &what),
                    _ => true,
                }
        });
        Ok(())
    }
}

/// Clients that receive navigation metadata only: the catalog watch and scope-less subscriptions.
// Only catalog watchers are filtered. A scope-less Subscribe means "every scope"
// (the phone and web push rely on asks and notifications from it), as on main.
/// The open requests of one worker session, including those of its subagents.
fn worker_asks<'a>(
    store: &'a Store,
    id: &'a str,
) -> impl Iterator<Item = &'a ox_desk_core::asks::Ask> + 'a {
    store.asks().iter().filter(move |a| {
        a.thread.as_deref() == Some(id)
            || store
                .claude_ask_tool(&a.id)
                .is_some_and(|(owner, _)| owner == id)
    })
}

/// Who answers worker requests, from the shared Desktop preferences. A missing or
/// unreadable file means the default.
fn worker_approvals() -> ox_desk_core::desktop_behavior::WorkerApprovals {
    ox_desk_core::desktop_behavior::read()
        .map(|s| s.behavior.worker_approvals)
        .unwrap_or_default()
}

fn catalog_only(client: &ClientState) -> bool {
    client.catalog
}

// Sidebar subscribers need just the navigation metadata. Never copy a transcript,
// command schema, approval payload, or worker snapshot into this stream.
fn catalog_row(view: &wire::SessionView) -> wire::SessionView {
    let detail = [
        "row",
        "worker",
        "order",
        "generated",
        "unread",
        "owned",
        "route",
        "origin",
        "worker_marks",
        "worker_model",
        "worker_notice",
        "last_result",
    ]
    .into_iter()
    .filter_map(|k| Some((k.to_owned(), view.detail.get(k)?.clone())))
    .collect::<serde_json::Map<_, _>>()
    .into();
    wire::SessionView {
        id: view.id.clone(),
        title: view.title.clone(),
        parent: view.parent.clone(),
        status: view.status.clone(),
        updated_at: view.updated_at,
        capabilities: view
            .capabilities
            .iter()
            .filter(|c| matches!(c.as_str(), "stop" | "steer"))
            .cloned()
            .collect(),
        detail,
    }
}
fn catalog_detail(state: &ScopeState) -> serde_json::Value {
    let requests: std::collections::BTreeSet<_> = state
        .asks
        .iter()
        .filter_map(|ask| ask.thread.as_deref())
        .collect();
    let drafts: Vec<_> = state
        .asks
        .iter()
        .filter(|ask| ask.thread.is_none())
        .map(|ask| &ask.id)
        .collect();
    serde_json::json!({"archived":state.detail["archived"],"request_threads":requests,"draft_requests":drafts})
}
fn catalog_snapshot(state: &ScopeState) -> ScopeState {
    ScopeState {
        scope: state.scope.clone(),
        connection: String::new(),
        capabilities: vec![],
        sessions: state
            .sessions
            .iter()
            .filter(|v| v.detail["owned"] == true)
            .map(catalog_row)
            .collect(),
        asks: vec![],
        transcripts: BTreeMap::new(),
        detail: catalog_detail(state),
    }
}

/// Move a policy-approved launch forward once its runtime can accept it.
/// Returns an error to report to the launcher.
fn advance_launch(live: &mut Live) -> Option<String> {
    let l = live.launching.as_mut()?;
    if live.store.state() != &Connection::Ready {
        return None;
    }
    let preset = l
        .origin
        .started
        .unwrap_or_else(|| l.origin.requested.preset(l.origin.runtime));
    // Give each Claude launch its own process so it cannot consume the operator's
    // draft or announce a worker before stock has verified its account and mode.
    match live.store.claude_worker_state() {
        ox_desk_core::store::claude::WorkerBlank::None
            if l.origin.runtime == ox_desk_core::runtime::Runtime::Claude
                && l.stage == Stage::Opening =>
        {
            let contract = l.origin.claude_contract(l.origin.mode);
            let model = match &l.payload {
                Payload::Worker(start) => start.subscription_model.clone(),
                Payload::Handoff { .. } => None,
            };
            return live
                .store
                .stage_claude_worker(contract, model, preset)
                .err()
                .map(|e| e.to_string());
        }
        ox_desk_core::store::claude::WorkerBlank::None => (),
        ox_desk_core::store::claude::WorkerBlank::Starting => return None,
        ox_desk_core::store::claude::WorkerBlank::Lost => {
            live.store.drop_claude_worker();
            return Some("The worker's Claude process did not start.".into());
        }
        ox_desk_core::store::claude::WorkerBlank::Ready => {
            if l.stage == Stage::Sent {
                return None;
            }
            if live.store.claude_worker_access() != Some(preset) {
                // Some stock versions omit the mode from initialize. Ask for the
                // requested mode once and wait for the same confirmation a live edit uses.
                if l.stage == Stage::Opening {
                    l.stage = Stage::AccessRequested;
                    return live
                        .store
                        .set_claude_worker_access(preset)
                        .err()
                        .map(|e| e.to_string());
                }
                return Some(
                    live.store
                        .notice()
                        .unwrap_or("Claude did not confirm the requested worker permission mode.")
                        .into(),
                );
            }
            if !l.origin.ceiling.allows(l.origin.runtime, preset) {
                return Some(
                    "The runtime did not confirm a permission mode within the granted ceiling."
                        .into(),
                );
            }
            // Check the pinned model against the picker of the very process that will run
            // it, before the first inference. An unoffered model stops the launch here, so
            // nothing runs or bills on another model or route.
            if let Payload::Worker(start) = &l.payload
                && let Some(requested) = &start.subscription_model
            {
                let pinned = live
                    .store
                    .claude_worker_picker()
                    .ok_or_else(|| {
                        "Claude Code did not report its model picker, so the model cannot be pinned. Nothing was billed.".to_owned()
                    })
                    .and_then(|picker| ox_desk_core::claude_models::resolve(&picker, requested));
                match pinned {
                    Ok(pin) => l.origin.pinned_model = Some(pin.exact),
                    Err(e) => {
                        live.store.drop_claude_worker();
                        return Some(e);
                    }
                }
            }
            l.stage = Stage::Sent;
            let sender = l.payload.sender();
            let action = match &mut l.payload {
                Payload::Worker(start) => Action::SendText(None, start.prompt()),
                Payload::Handoff { text, items, .. } => {
                    Action::InjectHandoff(std::mem::take(text), std::mem::take(items))
                }
            };
            live.store.set_memory_brief_override(l.memory_brief.clone());
            let result = live
                .store
                .send_on_worker_blank(action, sender)
                .err()
                .map(|e| e.to_string());
            live.store.set_memory_brief_override(None);
            return result;
        }
    }
    let goal = matches!(&l.payload, Payload::Worker(w) if w.mode == WorkerMode::Goal);
    match l.stage {
        Stage::Opening if live.store.can_send(None) => {
            l.stage = Stage::AccessRequested;
            if let Err(e) = live.store.dispatch(Action::SetNextAccess(preset)) {
                return Some(e.to_string());
            }
            advance_launch(live)
        }
        Stage::AccessRequested
            if goal && (!live.store.has_command_schema() || live.store.commands_loading()) =>
        {
            None
        }
        Stage::AccessRequested if goal && !live.store.command_caps(None).supports_goal() => {
            Some("Codex Goal mode is not enabled on the worker's route.".into())
        }
        Stage::AccessRequested
            if live.store.can_send(None)
                && (live.store.next_access() == Some(preset)
                    || live.store.applied_access(None) == Some(preset)) =>
        {
            l.stage = Stage::Sent;
            let sender = l.payload.sender();
            let action = match &mut l.payload {
                Payload::Worker(start) if start.mode == WorkerMode::Goal => {
                    Action::NativeCommand(None, start.prompt())
                }
                Payload::Worker(start) => Action::SendText(None, start.prompt()),
                Payload::Handoff { text, items, .. } => {
                    Action::InjectHandoff(std::mem::take(text), std::mem::take(items))
                }
            };
            live.store.set_memory_brief_override(l.memory_brief.clone());
            let result = live
                .store
                .dispatch_as(action, sender)
                .err()
                .map(|e| e.to_string());
            live.store.set_memory_brief_override(None);
            result
        }
        _ => None,
    }
}

/// Publish each session's stamp with its row. A hosted parent nests the worker
/// across scopes through `origin.parent.scope`; a handoff target does not nest.
/// A handoff source lists where it was handed off to, and a session with a YOLO
/// grant lists the running children that grant allowed.
fn annotate(
    host_id: &str,
    workers: &Registry,
    data: &crate::durable::Owned,
    scope: &Scope,
    store: &mut Store,
) {
    use ox_desk_core::origin::Relation;
    type Notes = BTreeMap<String, (Option<String>, serde_json::Map<String, Value>)>;
    let mut out: Notes = BTreeMap::new();
    for (id, entry) in workers.in_scope(scope) {
        let o = &entry.origin;
        let nests = o.relation != Relation::Handoff && o.handoff.is_none();
        let e = out.entry(id.to_owned()).or_default();
        if nests {
            e.0 = o.parent.scope.as_ref().and(o.parent.session.clone());
        }
        e.1.insert("origin".into(), json!(o));
        e.1.insert("worker_marks".into(), json!(entry.marks));
        e.1.insert("worker_model".into(), crate::workers::model_report(entry));
        e.1.insert("worker".into(), nests.into());
    }
    for summary in data
        .worker_notices
        .latest
        .iter()
        .filter(|s| &s.scope == scope)
    {
        out.entry(summary.session.clone())
            .or_default()
            .1
            .insert("worker_notice".into(), summary.detail());
    }
    let mut push = |id: &str, key: &str, value: Value| {
        let e = out.entry(id.to_owned()).or_default();
        let list = e.1.entry(key.to_owned()).or_insert_with(|| json!([]));
        if let Some(list) = list.as_array_mut() {
            list.push(value);
        }
    };
    let source_here = |source: &&ox_desk_core::handoff::HandoffSource| {
        &source.scope == scope && source.host.as_ref().is_none_or(|h| h.id == host_id)
    };
    for (s, id, entry) in workers.all() {
        let o = &entry.origin;
        if o.relation == Relation::Handoff
            && let Some(h) = o.handoff.as_deref().filter(source_here)
        {
            push(
                &h.session,
                "handed_off_to",
                json!({"scope": s, "session": id, "runtime": o.runtime, "route": o.route, "label": o.route.label()}),
            );
        }
    }
    for h in &data.handoffs {
        if let Some(src) = h.origin.handoff.as_deref().filter(source_here) {
            push(
                &src.session,
                "handoff_attempts",
                json!({"id": h.id, "scope": h.target, "status": h.status, "message": h.message, "label": h.target.route.label()}),
            );
        }
    }
    let now = crate::durable::unix_now();
    for g in data
        .grants
        .iter()
        .filter(|g| &g.scope == scope && g.live(now))
    {
        let children: Vec<Value> = workers
            .all()
            .filter(|(_, _, e)| {
                matches!(&e.origin.yolo, Some(YoloAllowance::Grant { session: Some(s), .. }) if s == &g.session)
            })
            .map(|(s, id, _)| json!({"scope": s, "session": id}))
            .collect();
        let e = out.entry(g.session.clone()).or_default();
        e.1.insert("yolo_grant".into(), true.into());
        e.1.insert("yolo_grant_expires".into(), g.expires_at().into());
        e.1.insert("yolo_children".into(), children.into());
    }
    for (id, entry) in workers.in_scope(scope) {
        if let Some(ceiling) = entry.origin.limits_children() {
            let started = entry
                .origin
                .started
                .and_then(|mode| ox_desk_core::modes::Level::of(scope.route.runtime, mode))
                .unwrap_or(entry.origin.requested);
            store.restore_worker_ceiling(id, ceiling.min(started));
        }
        if let Some(contract) = entry.origin.claude_contract(entry.origin.mode) {
            store.restore_worker_contract(id, contract);
        }
        if let Some(model) = &entry.origin.pinned_model {
            store.restore_worker_model(id, model);
        }
    }
    for (id, (parent, detail)) in out {
        store.annotate_session(&id, parent, Value::Object(detail));
    }
}
fn caller_scope(caller: &Caller) -> Option<&Scope> {
    match caller {
        Caller::Hosted { scope, .. } | Caller::Operator { scope, .. } => Some(scope),
        Caller::External(_) | Caller::Linked { .. } => None,
    }
}
fn key_ok(reply: &Replier) -> bool {
    match reply {
        Replier::Client(_, key) => !key.is_empty() && key.len() <= 256,
        Replier::Channel(_) => true,
    }
}

/// What a scope shows its subscribers after the host closed its idle runtime.
fn live_detail_after_drop(recovered: &ScopeState) -> Value {
    let mut detail = recovered.detail.clone();
    detail["notice"] = json!("Idle session ended. Select a thread to resume it.");
    detail
}

fn restore(rows: &mut Vec<wire::SessionView>, recovered: &[wire::SessionView]) {
    for saved in recovered {
        if let Some(row) = rows.iter_mut().find(|r| r.id == saved.id) {
            restore_row(row, saved);
        } else {
            rows.push(saved.clone());
        }
    }
}

// Until the runtime owns the session again, its catalog only describes the journal.
// Keep the interrupted run and its launch metadata in snapshots and single-row updates.
fn restore_row(row: &mut wire::SessionView, saved: &wire::SessionView) {
    if row.detail["owned"] != true && saved.status == "interrupted" {
        row.status.clone_from(&saved.status);
    }
    row.detail["owned"] = true.into();
    row.detail["recovered"] = true.into();
    for key in [
        "resume_access",
        "worker_marks",
        "origin",
        "worker_notice",
        "last_result",
        "memory_attachment",
    ] {
        if row.detail[key].is_null() && !saved.detail[key].is_null() {
            row.detail[key] = saved.detail[key].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::flows::ENDED_PARENT;
    use super::*;
    use ox_desk_core::{codex::frame::Frame, links::codex, runtime::Route, store::Profile};

    #[test]
    fn resolving_and_reusing_a_request_id_in_one_batch_retires_the_old_token() {
        let root = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: root.path().to_owned(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        // Input is supplied below. The inert stdio endpoint only consumes the store's initialization message.
        let link =
            codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
        let store = Store::open(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        engine.running.insert(
            scope.clone(),
            Live {
                state: store.host_snapshot(&scope),
                store,
                launching: None,
                fork_origin: None,
                confirming: None,
                opening: None,
                awaiting_input: Default::default(),
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
            },
        );
        let read = |line: &[u8]| LinkEvent::Frame(Frame::decode(line, |_| None).unwrap());
        let ask = || {
            read(br#"{"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"a","turnId":"t","itemId":"i","command":"test"}}"#)
        };
        engine.process(&scope, vec![ask()], false).unwrap();
        let old = engine.running[&scope].state.asks[0].token.clone();
        engine
            .process(
                &scope,
                vec![
                    read(br#"{"method":"serverRequest/resolved","params":{"requestId":7}}"#),
                    ask(),
                ],
                false,
            )
            .unwrap();
        let new = &engine.running[&scope].state.asks[0].token;
        assert!(!new.is_empty());
        assert_ne!(new, &old);
        assert!(
            engine
                .act(
                    0,
                    "stale",
                    &scope,
                    Action::AnswerAsk(RequestId::Number(7), 0),
                    Some(old)
                )
                .is_err()
        );
    }

    #[test]
    fn a_slow_native_read_in_one_scope_does_not_stall_the_host() {
        let root = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        let live = |dir: &str, hold: bool| {
            let project = root.path().join(dir);
            std::fs::create_dir_all(&project).unwrap();
            let scope = Scope {
                project,
                route: Route::CODEX_SUBSCRIPTION,
            };
            let link =
                codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
            let mut store = Store::open(
                Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: "openai".into(),
                },
                link,
            )
            .unwrap();
            if hold {
                // The runtime never answers this read: it is "slow" for the whole test.
                store.hold_native_read_for_test("thread-native");
            }
            (
                scope.clone(),
                Live {
                    state: store.host_snapshot(&scope),
                    store,
                    launching: None,
                    fork_origin: None,
                    confirming: None,
                    opening: None,
                    awaiting_input: Default::default(),
                    terminal: None,
                    schema_started: true,
                    recovered: vec![],
                    inbox: Inbox::new(|| {}),
                    generation: 1,
                    tokens: HashMap::new(),
                    tracker: Tracker::new(),
                    held_handoff: None,
                },
            )
        };
        for (scope, live) in [live("slow", true), live("quick", false)] {
            engine.running.insert(scope, live);
        }
        let (peer, frames) = crate::transport::test_peer();
        engine.clients.insert(
            1,
            ClientState {
                update_lease: None,
                peer,
                version: Some(wire::VERSION),
                prepared: None,
                subscription: None,
                catalog: false,
                watch_resources: false,
                terminal: false,
                surface: None,
            },
        );
        let (mailbox, rx) = std::sync::mpsc::sync_channel(64);
        let host = std::thread::spawn(move || {
            engine.serve(rx).unwrap();
        });
        std::thread::sleep(Duration::from_millis(300));
        let asked = Instant::now();
        mailbox
            .send(Message::Request(
                1,
                Box::new(Client::ListProcesses { id: "p".into() }),
            ))
            .unwrap();
        let packet = frames
            .recv_timeout(Duration::from_secs(2))
            .expect("ListProcesses is answered while a native read is outstanding");
        assert!(matches!(
            serde_json::from_slice::<Server>(&packet).unwrap(),
            Server::Processes { .. }
        ));
        assert!(asked.elapsed() < Duration::from_secs(1));
        mailbox.send(Message::Stop).unwrap();
        let stopped = Instant::now();
        while !host.is_finished() && stopped.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(host.is_finished(), "Stop is serviced promptly");
        host.join().unwrap();
    }

    #[test]
    fn a_refused_action_retried_with_the_same_key_runs_once_it_is_valid() {
        let root = tempfile::tempdir().unwrap();
        // Actions are checked against the canonical scope (macOS temp dirs sit behind a symlink).
        let scope = Scope {
            project: root.path().canonicalize().unwrap(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        let link =
            codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
        let store = Store::open(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        engine.running.insert(
            scope.clone(),
            Live {
                state: store.host_snapshot(&scope),
                store,
                launching: None,
                fork_origin: None,
                confirming: None,
                opening: None,
                awaiting_input: Default::default(),
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
            },
        );
        let ask = LinkEvent::Frame(
            Frame::decode(
                br#"{"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"a","turnId":"t","itemId":"i","command":"test"}}"#,
                |_| None,
            )
            .unwrap(),
        );
        engine.process(&scope, vec![ask], false).unwrap();
        let token = engine.running[&scope].state.asks[0].token.clone();
        let (peer, frames) = crate::transport::test_peer();
        engine.clients.insert(
            1,
            ClientState {
                update_lease: None,
                peer,
                version: Some(wire::VERSION),
                prepared: None,
                subscription: Some(Some(scope.clone())),
                catalog: false,
                watch_resources: false,
                terminal: false,
                surface: None,
            },
        );
        let act = |approval: Option<String>| Client::Act {
            id: "retry".into(),
            scope: scope.clone(),
            action: Action::AnswerAsk(RequestId::Number(7), 0),
            approval,
        };
        // The first attempt is refused for lack of approval; the same key with the approval must run.
        engine.request(1, act(None)).unwrap();
        engine.request(1, act(Some(token))).unwrap();
        let sent: Vec<Server> = frames
            .try_iter()
            .map(|p| serde_json::from_slice(&p).unwrap())
            .filter(|f| !matches!(f, Server::Delta { .. }))
            .collect();
        assert!(
            matches!(sent.as_slice(), [Server::Error { .. }, Server::Ack { .. }]),
            "{sent:?}"
        );
    }

    #[test]
    fn pending_opencode_actions_survive_completed_receipt_eviction_and_device_reconnect() {
        use ox_desk_core::store::opencode::{Command, Event};
        let root = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: root.path().canonicalize().unwrap(),
            route: Runtime::OpenCode.route(Some("fixture/model".into())),
        };
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        let (send, commands) = std::sync::mpsc::sync_channel(2);
        let mut store = Store::open_opencode(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "opencode".into(),
            },
            send,
        );
        store.apply(LinkEvent::OpenCode(Event::Ready(vec![])));
        engine.running.insert(
            scope.clone(),
            Live {
                state: store.host_snapshot(&scope),
                store,
                launching: None,
                fork_origin: None,
                confirming: None,
                opening: None,
                awaiting_input: Default::default(),
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
            },
        );
        let mut connect = |id| {
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                id,
                ClientState {
                    update_lease: None,
                    peer,
                    version: Some(wire::VERSION),
                    prepared: None,
                    subscription: Some(Some(scope.clone())),
                    catalog: false,
                    watch_resources: false,
                    terminal: false,
                    surface: None,
                },
            );
            frames
        };
        let old = connect(1);
        let newer = connect(2);
        let act = |text: &str| Client::Act {
            id: "send".into(),
            scope: scope.clone(),
            action: Action::SendText(None, text.into()),
            approval: None,
        };
        engine.request(1, act("original")).unwrap();
        for i in 0..2049 {
            let key = i.to_string();
            engine.receipts.insert(
                scope.clone(),
                key.clone(),
                1,
                [0; 32],
                Server::Ack { id: key },
            );
        }
        engine.request(2, act("original")).unwrap();
        engine.request(1, act("original")).unwrap();
        engine.request(2, act("different")).unwrap();
        let (key, cmd) = commands.try_recv().unwrap();
        assert_eq!(key, "send");
        assert!(matches!(cmd, Command::Send(None, text) if text == "original"));
        assert!(commands.try_recv().is_err());
        engine
            .process(
                &scope,
                vec![LinkEvent::OpenCode(Event::Finished {
                    key,
                    result: Ok(Some("session".into())),
                })],
                false,
            )
            .unwrap();
        let replies = |frames: Receiver<crate::transport::Packet>| {
            frames
                .try_iter()
                .map(|p| serde_json::from_slice::<Server>(&p).unwrap())
                .filter(|f| !matches!(f, Server::Delta { .. }))
                .collect::<Vec<_>>()
        };
        assert!(
            replies(old).is_empty(),
            "the earlier device cannot retake the receipt"
        );
        let sent = replies(newer);
        assert!(
            matches!(sent.as_slice(), [
            Server::Error { message, .. },
            Server::Opened { thread, .. },
            Server::Ack { id },
        ] if message.contains("different request") && thread == "session" && id == "send"),
            "{sent:?}"
        );
    }

    #[test]
    fn v1_subscriptions_keep_codex_and_claude_across_antigravity_activity() {
        use ox_desk_core::runtime::Runtime;
        let root = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        let mut listeners = vec![];
        for (id, version) in [(1, 1), (2, wire::VERSION)] {
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                id,
                ClientState {
                    update_lease: None,
                    peer,
                    version: None,
                    prepared: None,
                    subscription: None,
                    catalog: false,
                    watch_resources: false,
                    terminal: false,
                    surface: None,
                },
            );
            engine.request(id, Client::Hello { version }).unwrap();
            let hello = serde_json::from_slice::<Server>(&frames.try_recv().unwrap()).unwrap();
            assert!(matches!(hello, Server::Hello { version: got, .. } if got == version));
            engine
                .request(
                    id,
                    Client::Subscribe {
                        scope: None,
                        after: None,
                        terminal: false,
                    },
                )
                .unwrap();
            let snapshot = serde_json::from_slice::<Server>(&frames.try_recv().unwrap()).unwrap();
            let Server::Snapshot { cursor, .. } = snapshot else {
                panic!("expected snapshot")
            };
            listeners.push((id, cursor, frames));
        }
        for runtime in [Runtime::Codex, Runtime::Antigravity, Runtime::Claude] {
            let scope = Scope {
                project: root.path().into(),
                route: runtime.route(None),
            };
            engine.broadcast(&scope, vec![]).unwrap();
        }
        for (id, cursor, frames) in listeners {
            let expected = if id == 1 {
                vec![Runtime::Codex, Runtime::Claude]
            } else {
                vec![Runtime::Codex, Runtime::Antigravity, Runtime::Claude]
            };
            let received = || {
                frames
                    .try_iter()
                    .map(|packet| {
                        let frame = serde_json::from_slice::<Server>(&packet).unwrap();
                        let Server::Delta { scope, .. } = frame else {
                            panic!("expected delta")
                        };
                        scope.route.runtime
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(received(), expected);
            engine
                .request(
                    id,
                    Client::Subscribe {
                        scope: None,
                        after: Some(cursor),
                        terminal: false,
                    },
                )
                .unwrap();
            assert_eq!(received(), expected);
        }
    }

    #[test]
    fn a_connection_declares_its_surface_once_after_a_hello_that_offers_it() {
        use ox_desk_core::surface::{CAPABILITY, Surface};
        let root = tempfile::tempdir().unwrap();
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        let mut frames = vec![];
        for id in [1, 2] {
            let (peer, rx) = crate::transport::test_peer();
            engine.clients.insert(
                id,
                ClientState {
                    update_lease: None,
                    peer,
                    version: None,
                    prepared: None,
                    subscription: None,
                    catalog: false,
                    watch_resources: false,
                    terminal: false,
                    surface: None,
                },
            );
            frames.push(rx);
        }
        let declare = |surface| Client::Surface { surface };
        // Nothing is accepted before the hello.
        assert!(engine.request(1, declare(Surface::Phone)).is_err());
        engine
            .request(
                1,
                Client::Hello {
                    version: wire::VERSION + 1,
                },
            )
            .unwrap();
        let hello: Server = serde_json::from_slice(&frames[0].try_recv().unwrap()).unwrap();
        assert!(
            matches!(&hello, Server::Hello { version: wire::VERSION, capabilities, .. } if capabilities.iter().any(|c| c == CAPABILITY)),
            "{hello:?}"
        );
        engine.request(1, declare(Surface::Phone)).unwrap();
        engine.request(1, declare(Surface::Phone)).unwrap();
        assert!(
            engine.request(1, declare(Surface::Desktop)).is_err(),
            "a connection cannot change who it speaks for"
        );
        assert_eq!(
            engine.sender(1),
            Sender::Person {
                surface: Surface::Phone
            }
        );
        // An older client never declares: generic OX guidance, no surface claim.
        engine
            .request(
                2,
                Client::Hello {
                    version: wire::VERSION,
                },
            )
            .unwrap();
        assert_eq!(engine.sender(2), Sender::Unknown);
        assert!(
            serde_json::from_str::<Client>(r#"{"type":"surface","surface":"desktop","x":1}"#)
                .is_err(),
            "the wire still rejects unknown fields"
        );
    }

    #[test]
    fn internal_dispatch_paths_name_their_own_sender() {
        let worker = Payload::Worker(WorkerStart {
            host: None,
            needs: None,
            placement: None,
            parent: "p".into(),
            subscription_model: None,
            checkout: None,
            runtime: ox_desk_core::runtime::Runtime::Codex,
            model: None,
            mode: WorkerMode::Session,
            task: "review".into(),
            write: None,
            permission: None,
            yolo_confirmed: false,
            max_minutes: None,
        });
        assert_eq!(worker.sender(), Sender::WorkerTask);
        let handoff = Payload::Handoff {
            text: String::new(),
            items: vec![],
            record: "r".into(),
            from: Some(ox_desk_core::surface::Surface::Desktop),
        };
        assert_eq!(
            handoff.sender(),
            Sender::Handoff {
                from: Some(ox_desk_core::surface::Surface::Desktop)
            }
        );
    }

    #[test]
    fn catalog_watchers_get_catalog_deltas_and_all_scope_subscribers_get_everything() {
        let root = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: root.path().to_owned(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        let mut client = |id: u64, subscription: Option<Option<Scope>>, catalog: bool| {
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                id,
                ClientState {
                    update_lease: None,
                    peer,
                    version: Some(wire::VERSION),
                    prepared: None,
                    subscription,
                    catalog,
                    watch_resources: false,
                    terminal: false,
                    surface: None,
                },
            );
            frames
        };
        let legacy = client(1, Some(None), false);
        let watcher = client(2, None, true);
        let scoped = client(3, Some(Some(scope.clone())), false);
        let result = json!({"turn":"turn-1","completed_at":1,"outcome":"completed"});
        let session = wire::SessionView {
            id: "t".into(),
            title: "Title".into(),
            parent: None,
            status: "idle".into(),
            updated_at: 1,
            capabilities: vec![],
            detail: json!({"owned": true, "row": true, "last_result": result}),
        };
        let transcript = Change::Transcript {
            thread: "t".into(),
            timeline: wire::Timeline {
                items: vec![],
                detail: Value::Null,
            },
        };
        engine
            .broadcast(
                &scope,
                vec![
                    Change::Sessions {
                        sessions: vec![session],
                    },
                    transcript,
                    Change::Notification {
                        thread: "t".into(),
                        title: "Title".into(),
                        event: ox_desk_core::host_protocol::NotificationKind::Finished,
                        goal_reached: false,
                    },
                ],
            )
            .unwrap();
        let decode =
            |frames: std::sync::mpsc::Receiver<crate::transport::Packet>| -> Vec<Vec<Change>> {
                frames
                    .try_iter()
                    .map(|p| match serde_json::from_slice(&p).unwrap() {
                        Server::Delta { changes, .. } => changes,
                        other => panic!("unexpected frame {other:?}"),
                    })
                    .collect()
            };
        // A scope-less Subscribe means every scope: the phone and web push need its
        // asks and notifications, so it receives the full packet like on main.
        let all = decode(legacy);
        assert_eq!(all.len(), 1);
        assert!(
            all[0]
                .iter()
                .any(|c| matches!(c, Change::Transcript { .. }))
        );
        assert!(
            all[0]
                .iter()
                .any(|c| matches!(c, Change::Notification { .. }))
        );
        {
            let frames = decode(watcher);
            assert_eq!(frames.len(), 1);
            assert!(
                frames[0]
                    .iter()
                    .all(|c| !matches!(c, Change::Transcript { .. })),
                "{frames:?}"
            );
            let [Change::Sessions { sessions }, Change::Notification { .. }] = frames[0].as_slice()
            else {
                panic!("unexpected catalog changes: {frames:?}");
            };
            assert_eq!(sessions[0].detail["last_result"], result);
        }
        let scoped = decode(scoped);
        assert_eq!(scoped.len(), 1);
        assert!(
            scoped[0]
                .iter()
                .any(|c| matches!(c, Change::Transcript { .. }))
        );
    }

    #[test]
    fn a_failed_settings_read_keeps_the_last_project_default() {
        let root = tempfile::tempdir().unwrap();
        let scope = Scope {
            project: root.path().to_owned(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: root.path().into(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config, root.path(), tx).unwrap();
        // Every settings read fails here, so no default has ever been seen.
        assert!(engine.yolo_grant(&scope, "session").is_none());
        // Once a read has succeeded, a later failure keeps honouring that default.
        engine.yolo_projects = vec![scope.project.clone()];
        assert!(matches!(
            engine.yolo_grant(&scope, "session"),
            Some(YoloAllowance::Grant { session: None, .. })
        ));
    }

    mod workers {
        use super::*;
        use ox_desk_core::{
            host_protocol::{AskView, Timeline},
            origin::{Ceiling, ExternalCaller, ParentRef, Relation},
            runtime::{Access, Runtime},
            store::ThreadRow,
        };
        use std::sync::{Arc, Mutex};

        type Log = Arc<Mutex<Vec<(Action, Option<String>)>>>;

        fn config(root: &Path) -> RuntimeConfig {
            RuntimeConfig {
                ox: "/usr/bin/false".into(),
                path: "/bin".into(),
                desktop: root.into(),
            }
        }
        fn view(scope: &Scope, id: &str, status: &str, access: &str) -> wire::SessionView {
            let row = ThreadRow {
                worker: false,
                parent_thread_id: None,
                agent_nickname: None,
                id: id.into(),
                cwd: scope.project.clone(),
                model_provider: "openai".into(),
                model: None,
                name: Some(id.into()),
                preview: String::new(),
                generated_title: None,
                updated_at: 1,
                status: None,
                unread: false,
                activity_order: 0,
            };
            wire::SessionView {
                id: id.into(),
                title: id.into(),
                parent: None,
                status: status.into(),
                updated_at: 1,
                capabilities: ["open", "send", "steer", "stop", "access"]
                    .map(String::from)
                    .into(),
                detail: json!({"row": row, "access": access, "owned": true}),
            }
        }
        /// A runtime stand-in: a host-client replica whose actions are recorded.
        fn live(
            scope: &Scope,
            sessions: Vec<wire::SessionView>,
            active: &[&str],
            asks: Vec<AskView>,
        ) -> (Live, Log) {
            let transcripts = active
                .iter()
                .map(|id| {
                    (
                        *id,
                        Timeline {
                            items: vec![],
                            detail: json!({"active": "t1", "ready": true}),
                        },
                    )
                })
                .collect();
            live_with(scope, sessions, transcripts, asks)
        }
        /// A runtime stand-in with the given transcripts.
        fn live_with(
            scope: &Scope,
            sessions: Vec<wire::SessionView>,
            transcripts: Vec<(&str, Timeline)>,
            asks: Vec<AskView>,
        ) -> (Live, Log) {
            let log: Log = Default::default();
            let record = log.clone();
            let mut store = Store::remote_client(
                ox_desk_core::store::Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: "openai".into(),
                },
                move |action, approval| {
                    record.lock().unwrap().push((action, approval));
                    Ok("sent".into())
                },
            );
            store.apply(LinkEvent::Host(Box::new(Server::Snapshot {
                cursor: Cursor {
                    epoch: "e".into(),
                    seq: 1,
                },
                scopes: vec![ScopeState {
                    scope: scope.clone(),
                    connection: "ready".into(),
                    capabilities: vec!["start".into()],
                    sessions,
                    asks,
                    transcripts: transcripts
                        .into_iter()
                        .map(|(id, t)| (id.to_owned(), t))
                        .collect(),
                    detail: json!({}),
                }],
            })));
            let _ = store.drain(vec![], Instant::now());
            (
                Live {
                    state: store.host_snapshot(scope),
                    store,
                    launching: None,
                    fork_origin: None,
                    confirming: None,
                    opening: None,
                    awaiting_input: Default::default(),
                    terminal: None,
                    schema_started: true,
                    recovered: vec![],
                    inbox: Inbox::new(|| {}),
                    generation: 1,
                    tokens: HashMap::new(),
                    tracker: Tracker::new(),
                    held_handoff: None,
                },
                log,
            )
        }
        fn origin(parent: ParentRef, ceiling: Ceiling) -> Origin {
            Origin {
                linked: None,
                relation: Relation::Worker,
                parent,
                launch: "helper:test".into(),
                by_operator: false,
                runtime: Runtime::Codex,
                route: Route::codex_api("fixture".into()),
                mode: WorkerMode::Session,
                job_id: 41,
                requested: Ceiling::Read,
                ceiling,
                source: None,
                started: None,
                decided: None,
                yolo: None,
                handoff: None,
                requested_model: None,
                pinned_model: None,
            }
        }
        fn scopes(root: &Path) -> (Scope, Scope) {
            let parent = Scope {
                project: root.canonicalize().unwrap(),
                route: Route::CODEX_SUBSCRIPTION,
            };
            let worker = Scope {
                route: Route::codex_api("fixture".into()),
                ..parent.clone()
            };
            (parent, worker)
        }
        fn hosted(scope: &Scope) -> Caller {
            Caller::Hosted {
                scope: scope.clone(),
                hint: None,
                launch: "helper".into(),
            }
        }
        fn start(write: bool) -> WorkerStart {
            WorkerStart {
                host: None,
                needs: None,
                placement: None,
                parent: String::new(),
                runtime: Runtime::Codex,
                model: Some("fixture".into()),
                mode: WorkerMode::Session,
                task: "check".into(),
                write: Some(write),
                permission: None,
                checkout: None,
                yolo_confirmed: false,
                max_minutes: None,
                subscription_model: None,
            }
        }

        #[test]
        fn ambiguous_parents_are_refused_before_any_runtime_starts() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let rows = vec![
                view(&parent, "a", "running", "workspace"),
                view(&parent, "b", "running", "workspace"),
            ];
            engine
                .running
                .insert(parent.clone(), live(&parent, rows, &["a", "b"], vec![]).0);
            let refused = engine.accept_launch(&hosted(&parent), start(false));
            assert!(refused.err().unwrap().contains("Several sessions"));
            assert!(
                !engine.running.contains_key(&worker),
                "no worker runtime was opened"
            );
            assert!(
                engine
                    .parent_call(&hosted(&parent), ParentCall::List)
                    .unwrap_err()
                    .contains("Several sessions")
            );
            // Without the calling thread, every parent operation is refused, not
            // only listing: status, steer and cancel are never looser than list.
            let o = origin(workers_parent(&parent, "a"), Ceiling::Read);
            engine.workers.stamp(&worker, "w", o);
            for call in [
                ParentCall::Status { job_id: 41 },
                ParentCall::Steer {
                    job_id: 41,
                    text: "x".into(),
                },
                ParentCall::Cancel { job_id: 41 },
            ] {
                let err = engine.parent_call(&hosted(&parent), call).unwrap_err();
                assert!(err.contains("Several sessions"), "{err}");
            }
            // With Codex's `_meta.threadId`, the caller is exact. "b" is a sibling
            // worker on the same app-server: it sees none of a's jobs.
            let thread = |id: &str| Caller::Hosted {
                scope: parent.clone(),
                hint: Some(id.into()),
                launch: "helper".into(),
            };
            assert_eq!(
                engine.parent_call(&thread("a"), ParentCall::List).unwrap()["jobs"][0]["job_id"],
                41
            );
            assert_eq!(
                engine
                    .parent_call(&thread("a"), ParentCall::Status { job_id: 41 })
                    .unwrap()["job_id"],
                41
            );
            assert_eq!(
                engine.parent_call(&thread("b"), ParentCall::List).unwrap()["jobs"],
                json!([])
            );
            for call in [
                ParentCall::Status { job_id: 41 },
                ParentCall::Steer {
                    job_id: 41,
                    text: "x".into(),
                },
                ParentCall::Cancel { job_id: 41 },
            ] {
                assert!(engine.parent_call(&thread("b"), call).is_err());
            }
            assert!(!engine.workers.get(&worker, "w").unwrap().marks.cancelled);
            // A thread ID that is not an active session on this route is refused.
            assert!(
                engine
                    .parent_call(&thread("forged"), ParentCall::List)
                    .is_err()
            );
            // A hint from a session-scoped helper resolves the ambiguity, and the
            // parent's read-only mode still caps the request before anything starts.
            let mut rows = vec![
                view(&parent, "a", "running", "read_only"),
                view(&parent, "b", "running", "workspace"),
            ];
            rows.truncate(2);
            engine
                .running
                .insert(parent.clone(), live(&parent, rows, &["a", "b"], vec![]).0);
            let hinted = Caller::Hosted {
                scope: parent.clone(),
                hint: Some("a".into()),
                launch: "h".into(),
            };
            assert!(
                engine
                    .accept_launch(&hinted, start(true))
                    .err()
                    .unwrap()
                    .contains("above the source")
            );
            assert!(!engine.running.contains_key(&worker));
        }

        fn workers_parent(scope: &Scope, id: &str) -> ParentRef {
            ParentRef {
                project: scope.project.clone(),
                scope: Some(scope.clone()),
                session: Some(id.into()),
                runtime: Some(Runtime::Codex),
            }
        }

        #[test]
        fn the_operator_cannot_raise_a_worker_above_its_ceiling() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let (l, log) = live(
                &worker,
                vec![view(&worker, "w", "idle", "read_only")],
                &[],
                vec![],
            );
            engine.running.insert(worker.clone(), l);
            engine.workers.stamp(
                &worker,
                "w",
                origin(workers_parent(&parent, "p"), Ceiling::Read),
            );
            let err = engine
                .act(
                    1,
                    "k1",
                    &worker,
                    Action::SetAccess("w".into(), Access::Workspace),
                    None,
                )
                .unwrap_err();
            assert!(err.to_string().contains("ceiling"), "{err}");
            assert!(
                engine
                    .act(
                        1,
                        "k2",
                        &worker,
                        Action::SetAccess("w".into(), Access::Yolo),
                        None
                    )
                    .is_err()
            );
            assert!(
                log.lock().unwrap().is_empty(),
                "nothing reached the runtime"
            );
            // Within the ceiling the operator's change goes through and is reported to the parent.
            engine.workers.get_mut(&worker, "w").unwrap().origin.ceiling = Ceiling::Write;
            engine
                .act(
                    1,
                    "k3",
                    &worker,
                    Action::SetAccess("w".into(), Access::Workspace),
                    None,
                )
                .unwrap();
            assert!(matches!(
                log.lock().unwrap()[0].0,
                Action::SetAccess(_, Access::Workspace)
            ));
            assert_eq!(
                engine.workers.get(&worker, "w").unwrap().marks.access,
                Some(Access::Workspace)
            );
        }

        #[test]
        fn a_parent_that_runs_without_a_person_answers_its_workers_requests() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let ask = |id: i64, thread: &str| AskView {
                command: None,
                id: RequestId::Number(id),
                token: "native".into(),
                thread: Some(thread.into()),
                turn: Some("t1".into()),
                item: None,
                choice_details: vec![None, None],
                title: "Run command".into(),
                detail: "cargo test".into(),
                choices: vec!["Allow".into(), "Decline".into()],
                questions: vec![],
            };
            let (l, log) = live(
                &worker,
                vec![
                    view(&worker, "w", "needs_input", "workspace"),
                    view(&worker, "r", "needs_input", "read_only"),
                ],
                &["w", "r"],
                vec![ask(9, "w"), ask(10, "r")],
            );
            engine.running.insert(worker.clone(), l);
            with_parent(&mut engine, &parent, "yolo");
            engine.workers.stamp(
                &worker,
                "w",
                origin(workers_parent(&parent, "a"), Ceiling::Write),
            );
            let mut read = origin(workers_parent(&parent, "a"), Ceiling::Read);
            read.job_id = 42;
            engine.workers.stamp(&worker, "r", read);
            engine.process(&worker, vec![], true).unwrap();
            let me = thread(&parent, "a");
            let status = engine
                .parent_call(&me, ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(status["status"], "waiting_for_operator");
            assert_eq!(status["waiting_for_operator"]["parent_may_answer"], true);
            assert_eq!(status["requests"][0]["request_id"], 9);
            assert_eq!(
                status["requests"][0]["choices"],
                json!(["Allow", "Decline"])
            );
            let answer = |job_id, request| ParentCall::Answer {
                job_id,
                request: RequestId::Number(request),
                choice: Some(0),
                answers: None,
            };
            // A read worker's approval stays with the operator, even under a YOLO parent.
            let status = engine
                .parent_call(&me, ParentCall::Status { job_id: 42 })
                .unwrap();
            assert_eq!(status["requests"][0]["parent_may_answer"], false);
            assert!(engine.parent_call(&me, answer(42, 10)).is_err());
            // Its question is an answer, not a grant: the parent may fill it in, unless secret.
            let question = |secret| ox_desk_core::asks::Question {
                id: "q".into(),
                question: "Which file?".into(),
                header: "File".into(),
                is_other: true,
                is_secret: secret,
                multi_select: false,
                options: None,
            };
            for (id, secret) in [(11, false), (12, true)] {
                let mut q = ask(id, "r");
                q.choices = vec![];
                q.choice_details = vec![];
                q.questions = vec![question(secret)];
                engine
                    .running
                    .get_mut(&worker)
                    .unwrap()
                    .store
                    .apply(LinkEvent::Host(Box::new(Server::Delta {
                        cursor: Cursor {
                            epoch: "e".into(),
                            seq: id as u64,
                        },
                        scope: worker.clone(),
                        changes: vec![Change::Asks {
                            asks: vec![ask(9, "w"), q],
                        }],
                    })));
                let status = engine
                    .parent_call(&me, ParentCall::Status { job_id: 42 })
                    .unwrap();
                assert_eq!(
                    status["requests"][0]["parent_may_answer"], !secret,
                    "{status}"
                );
            }
            // An approval too long to show in full is never the parent's to answer.
            let mut long = ask(13, "w");
            long.detail = "x".repeat(crate::workers::REQUEST_DETAIL_LIMIT + 1);
            engine
                .running
                .get_mut(&worker)
                .unwrap()
                .store
                .apply(LinkEvent::Host(Box::new(Server::Delta {
                    cursor: Cursor {
                        epoch: "e".into(),
                        seq: 5,
                    },
                    scope: worker.clone(),
                    changes: vec![Change::Asks {
                        asks: vec![ask(9, "w"), ask(10, "r"), long],
                    }],
                })));
            let status = engine
                .parent_call(&me, ParentCall::Status { job_id: 41 })
                .unwrap();
            let shown = &status["requests"][1];
            assert_eq!(shown["request_id"], 13);
            assert_eq!(shown["parent_may_answer"], false);
            assert!(
                shown["detail"]
                    .as_str()
                    .unwrap()
                    .ends_with("the operator answers this request]")
            );
            assert!(engine.parent_call(&me, answer(41, 13)).is_err());
            // A choice that sets a lasting rule shows its scope and stays with the operator.
            let mut rule = ask(14, "w");
            rule.choices = vec!["Allow once".into(), "Always allow this prefix".into()];
            rule.choice_details = vec![None, Some("\"cargo\"".into())];
            engine
                .running
                .get_mut(&worker)
                .unwrap()
                .store
                .apply(LinkEvent::Host(Box::new(Server::Delta {
                    cursor: Cursor {
                        epoch: "e".into(),
                        seq: 6,
                    },
                    scope: worker.clone(),
                    changes: vec![Change::Asks {
                        asks: vec![ask(9, "w"), ask(10, "r"), rule],
                    }],
                })));
            let status = engine
                .parent_call(&me, ParentCall::Status { job_id: 41 })
                .unwrap();
            let shown = &status["requests"][1];
            assert_eq!(shown["choice_details"], json!([null, "\"cargo\""]));
            assert_eq!(shown["parent_choices"], json!([0]));
            let lasting = ParentCall::Answer {
                job_id: 41,
                request: RequestId::Number(14),
                choice: Some(1),
                answers: None,
            };
            assert!(
                engine
                    .parent_call(&me, lasting)
                    .unwrap_err()
                    .contains("lasting rule")
            );
            // A request of another worker is not this job's to answer.
            assert!(engine.parent_call(&me, answer(41, 10)).is_err());
            assert!(log.lock().unwrap().is_empty());
            // A parent whose own mode asks a person cannot approve for its worker.
            with_parent(&mut engine, &parent, "workspace");
            let refused = engine.parent_call(&me, answer(41, 9)).unwrap_err();
            assert!(refused.contains("person"), "{refused}");
            assert!(log.lock().unwrap().is_empty());
            with_parent(&mut engine, &parent, "yolo");
            assert_eq!(
                engine.parent_call(&me, answer(41, 9)).unwrap()["answered"],
                true
            );
            assert!(matches!(
                log.lock().unwrap()[0].0,
                Action::AnswerAsk(RequestId::Number(9), 0)
            ));
        }

        #[test]
        fn worker_approvals_belong_to_operators_and_survive_a_revoked_device() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let ask = AskView {
                command: None,
                id: RequestId::Number(9),
                token: "native".into(),
                thread: Some("w".into()),
                turn: Some("t1".into()),
                item: None,
                choice_details: vec![None, None],
                title: "Run command".into(),
                detail: "cargo test".into(),
                choices: vec!["Allow".into(), "Decline".into()],
                questions: vec![],
            };
            let (l, log) = live(
                &worker,
                vec![view(&worker, "w", "needs_input", "workspace")],
                &["w"],
                vec![ask],
            );
            engine.running.insert(worker.clone(), l);
            let caller = Caller::External(ExternalCaller {
                project: worker.project.clone(),
                runtime: Runtime::Claude,
                session: Some("s1".into()),
                launch: "l1".into(),
            });
            let mut o = origin(workers_parent(&parent, "p"), Ceiling::Write);
            o.parent.scope = None;
            o.parent.session = Some("s1".into());
            engine.workers.stamp(&worker, "w", o);
            engine.process(&worker, vec![], true).unwrap();
            let token = engine.running[&worker].state.asks[0].token.clone();
            assert!(!token.is_empty());
            // The parent agent has no answer operation and cannot steer past the request.
            let status = engine
                .parent_call(&caller, ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(
                (status["status"].as_str(), status["can_steer"].as_bool()),
                (Some("waiting_for_operator"), Some(false))
            );
            assert!(status["waiting_for_operator"]["message"].is_string());
            let steer = engine.parent_call(
                &caller,
                ParentCall::Steer {
                    job_id: 41,
                    text: "approve it".into(),
                },
            );
            assert!(steer.unwrap_err().contains("operator"));
            assert!(
                crate::workers::parent_call(json!({"op":"answer","job_id":41,"choice":0})).is_err()
            );
            // A phone and a desktop are subscribed. Revoking the phone closes its stream.
            for id in [1, 2] {
                let (peer, _) = crate::transport::test_peer();
                engine.clients.insert(
                    id,
                    ClientState {
                        update_lease: None,
                        peer,
                        version: Some(wire::VERSION),
                        prepared: None,
                        subscription: Some(Some(worker.clone())),
                        catalog: false,
                        watch_resources: false,
                        terminal: false,
                        surface: None,
                    },
                );
            }
            engine.clients.remove(&1);
            assert_eq!(
                engine.running[&worker].state.asks.len(),
                1,
                "the request stays pending"
            );
            assert_eq!(
                engine.running[&worker].state.asks[0].token, token,
                "and keeps its token"
            );
            assert!(log.lock().unwrap().is_empty());
            // A stale token is refused. The remaining operator answers with the live one.
            assert!(
                engine
                    .act(
                        2,
                        "a1",
                        &worker,
                        Action::AnswerAsk(RequestId::Number(9), 0),
                        Some("stale".into())
                    )
                    .is_err()
            );
            engine
                .act(
                    2,
                    "a2",
                    &worker,
                    Action::AnswerAsk(RequestId::Number(9), 0),
                    Some(token),
                )
                .unwrap();
            assert!(
                matches!(log.lock().unwrap()[0], (Action::AnswerAsk(RequestId::Number(9), 0), Some(ref t)) if t == "native")
            );
        }

        #[test]
        fn terminal_workers_belong_to_their_launch_until_a_session_is_bound() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker) = scopes(root.path());
            let terminal = |session: Option<&str>, launch: &str| {
                Caller::External(ExternalCaller {
                    project: worker.project.clone(),
                    runtime: Runtime::Claude,
                    session: session.map(Into::into),
                    launch: launch.into(),
                })
            };
            let mut bound = origin(workers_parent(&parent, "p"), Ceiling::Read);
            bound.parent.scope = None;
            bound.parent.session = None;
            bound.launch = crate::workers::launch_digest("terminal", "token-one");
            let mut orphan = bound.clone();
            orphan.job_id = 42;
            orphan.launch = crate::workers::launch_digest("terminal", "token-gone");
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                let (l, _) = live(
                    &worker,
                    vec![
                        view(&worker, "w1", "running", "read_only"),
                        view(&worker, "w2", "running", "read_only"),
                    ],
                    &["w1", "w2"],
                    vec![],
                );
                engine.running.insert(worker.clone(), l);
                engine.workers.stamp(&worker, "w1", bound.clone());
                engine.workers.stamp(&worker, "w2", orphan.clone());
                engine.process(&worker, vec![], true).unwrap();
                // A second `ox codex` in the same repository sees and controls nothing.
                let other = terminal(None, "token-two");
                assert_eq!(
                    engine.parent_call(&other, ParentCall::List).unwrap()["jobs"],
                    json!([])
                );
                assert!(
                    engine
                        .parent_call(&other, ParentCall::Cancel { job_id: 41 })
                        .is_err()
                );
                assert!(
                    engine
                        .parent_call(
                            &other,
                            ParentCall::Steer {
                                job_id: 41,
                                text: "x".into()
                            }
                        )
                        .is_err()
                );
                // Its own launch controls it and binds its session once observed.
                let own = terminal(None, "token-one");
                assert_eq!(
                    engine
                        .parent_call(&own, ParentCall::Status { job_id: 41 })
                        .unwrap()["job_id"],
                    41
                );
                let reply = engine
                    .parent_call(
                        &own,
                        ParentCall::Bind {
                            session: "s1".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(reply["bound"], 1, "only this launch's workers are bound");
                // Binding is write-once: a later claim cannot move the job.
                engine
                    .parent_call(
                        &own,
                        ParentCall::Bind {
                            session: "s2".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(
                    engine
                        .workers
                        .get(&worker, "w1")
                        .unwrap()
                        .marks
                        .parent_session
                        .as_deref(),
                    Some("s1")
                );
                // A hosted caller has no binding operation.
                let hinted = Caller::Hosted {
                    scope: worker.clone(),
                    hint: Some("w1".into()),
                    launch: "h".into(),
                };
                assert!(
                    engine
                        .parent_call(
                            &hinted,
                            ParentCall::Bind {
                                session: "s1".into()
                            }
                        )
                        .is_err()
                );
            }
            // After a host restart and the original launch's exit, a resume of s1
            // reads its worker. The worker whose launch never learned a session
            // is left to operators only.
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let resumed = terminal(Some("s1"), "token-three");
            assert_eq!(
                engine
                    .parent_call(&resumed, ParentCall::Status { job_id: 41 })
                    .unwrap()["job_id"],
                41
            );
            assert!(
                engine
                    .parent_call(&resumed, ParentCall::Status { job_id: 42 })
                    .is_err()
            );
            assert!(
                engine
                    .parent_call(
                        &terminal(Some("s2"), "token-four"),
                        ParentCall::Status { job_id: 41 }
                    )
                    .is_err()
            );
            assert!(
                engine
                    .parent_call(
                        &terminal(None, "token-four"),
                        ParentCall::Cancel { job_id: 42 }
                    )
                    .is_err()
            );
            assert_eq!(
                engine.parent_call(&resumed, ParentCall::List).unwrap()["jobs"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }

        #[test]
        fn a_resumed_claude_session_reclaims_the_workers_its_new_launch_started() {
            let root = tempfile::tempdir().unwrap();
            let (_, worker) = scopes(root.path());
            // `ox claude` added `--session-id` before the first turn, so the
            // launch names its session when it calls start_ox_worker.
            let session = "6f1c2d3e-4b5a-4c6d-8e7f-90a1b2c3d4e5";
            let terminal = |session: Option<&str>, launch: &str| {
                Caller::External(ExternalCaller {
                    project: worker.project.clone(),
                    runtime: Runtime::Claude,
                    session: session.map(Into::into),
                    launch: launch.into(),
                })
            };
            let launch = terminal(Some(session), "first-launch-token");
            let started = crate::workers::origin(
                &launch,
                None,
                &WorkerStart {
                    host: None,
                    needs: None,
                    placement: None,
                    parent: String::new(),
                    runtime: Runtime::Codex,
                    model: Some("fixture".into()),
                    mode: WorkerMode::Session,
                    task: "fix".into(),
                    write: Some(true),
                    permission: None,
                    checkout: None,
                    yolo_confirmed: false,
                    max_minutes: None,
                    subscription_model: None,
                },
                41,
                ox_desk_core::modes::Resolved {
                    level: Ceiling::Write,
                    access: Access::Workspace,
                    decision: ox_desk_core::modes::Decision::Lowered,
                },
            );
            assert_eq!(
                started.parent.session.as_deref(),
                Some(session),
                "recorded at launch"
            );
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                let (l, _) = live(
                    &worker,
                    vec![view(&worker, "w", "running", "workspace")],
                    &["w"],
                    vec![],
                );
                engine.running.insert(worker.clone(), l);
                engine.workers.stamp(&worker, "w", started.clone());
                engine.process(&worker, vec![], true).unwrap();
                assert_eq!(
                    engine
                        .parent_call(&launch, ParentCall::Status { job_id: 41 })
                        .unwrap()["job_id"],
                    41
                );
                // A later binding from the same launch, e.g. after /clear, changes nothing.
                engine
                    .parent_call(
                        &launch,
                        ParentCall::Bind {
                            session: "after-clear".into(),
                        },
                    )
                    .unwrap();
                assert_eq!(
                    engine
                        .workers
                        .get(&worker, "w")
                        .unwrap()
                        .marks
                        .parent_session,
                    None
                );
                assert_eq!(
                    engine
                        .workers
                        .get(&worker, "w")
                        .unwrap()
                        .origin
                        .parent
                        .session
                        .as_deref(),
                    Some(session)
                );
            }
            // The terminal exits and the host restarts.
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            // Another new `ox claude` in the same project has its own session ID.
            let other = terminal(
                Some("0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d"),
                "second-launch-token",
            );
            assert_eq!(
                engine.parent_call(&other, ParentCall::List).unwrap()["jobs"],
                json!([])
            );
            for call in [
                ParentCall::Status { job_id: 41 },
                ParentCall::Steer {
                    job_id: 41,
                    text: "x".into(),
                },
                ParentCall::Cancel { job_id: 41 },
            ] {
                assert!(engine.parent_call(&other, call).is_err());
            }
            assert!(!engine.workers.get(&worker, "w").unwrap().marks.cancelled);
            // `ox claude --resume <that uuid>` is a new launch naming the same session.
            let resumed = terminal(Some(session), "resume-launch-token");
            assert_eq!(
                engine.parent_call(&resumed, ParentCall::List).unwrap()["jobs"][0]["job_id"],
                41
            );
            assert_eq!(
                engine
                    .parent_call(&resumed, ParentCall::Status { job_id: 41 })
                    .unwrap()["job_id"],
                41
            );
            let cancelled = engine
                .parent_call(&resumed, ParentCall::Cancel { job_id: 41 })
                .unwrap();
            assert_eq!(cancelled["job_id"], 41);
        }

        #[test]
        fn a_host_restart_mid_worker_keeps_its_stamp_and_never_relaunches_it() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker) = scopes(root.path());
            let mut o = origin(workers_parent(&parent, "p"), Ceiling::Write);
            o.parent.scope = None;
            o.parent.session = Some("s1".into());
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                let (l, _) = live(
                    &worker,
                    vec![view(&worker, "w", "running", "workspace")],
                    &["w"],
                    vec![],
                );
                engine.running.insert(worker.clone(), l);
                engine.workers.stamp(&worker, "w", o.clone());
                engine
                    .workers
                    .get_mut(&worker, "w")
                    .unwrap()
                    .marks
                    .continued = 1;
                engine.process(&worker, vec![], true).unwrap();
                let saved = std::fs::read_to_string(root.path().join("ownership-v1.json")).unwrap();
                assert!(saved.contains("\"job_id\":41") && !saved.contains("\"task\""));
            }
            let (tx, rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.running.is_empty(), "nothing is relaunched");
            assert!(rx.try_recv().is_err());
            let entry = engine.workers.get(&worker, "w").unwrap();
            assert_eq!(entry.origin, o, "the stamp is unchanged");
            assert_eq!(entry.marks.continued, 1);
            let row = &engine.cold[&worker].sessions[0];
            assert_eq!(row.status, "interrupted");
            assert_eq!(row.detail["origin"]["job_id"], 41);
            assert_eq!(catalog_row(row).detail["origin"]["ceiling"], "write");
            let caller = Caller::External(ExternalCaller {
                project: worker.project.clone(),
                runtime: Runtime::Claude,
                session: Some("s1".into()),
                launch: "resumed".into(),
            });
            let status = engine
                .parent_call(&caller, ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(status["status"], "interrupted");
            assert_eq!(status["operator"]["continued"], 1);
            let other = Caller::External(ExternalCaller {
                project: worker.project.clone(),
                runtime: Runtime::Claude,
                session: Some("s2".into()),
                launch: "other".into(),
            });
            assert!(
                engine
                    .parent_call(&other, ParentCall::Status { job_id: 41 })
                    .is_err()
            );
            assert_eq!(
                engine.parent_call(&caller, ParentCall::List).unwrap()["jobs"][0]["job_id"],
                41
            );
        }

        fn timeline(items: &[(&str, &str, &str)], active: bool) -> Timeline {
            Timeline {
                items: items
                    .iter()
                    .map(|(id, role, text)| wire::TimelineItem {
                        turn: format!("turn-{id}"),
                        id: (*id).into(),
                        role: (*role).into(),
                        text: (*text).into(),
                        revision: 1,
                        detail: match *role {
                            "user" => json!({"UserMessage": {"content": [{"text": text}]}}),
                            "thinking" => json!({"Reasoning": {"summary": [text]}}),
                            "tool" => json!({"item": {"CommandExecution": {
                                "command": text, "status": "completed",
                                "aggregated_output": "ok", "exit_code": 0}}, "exit": 0}),
                            _ => json!({"update": false}),
                        },
                    })
                    .collect(),
                detail: if active {
                    json!({"active": "t1", "ready": true})
                } else {
                    json!({"ready": true})
                },
            }
        }
        fn inherit(runtime: Runtime) -> WorkerStart {
            WorkerStart {
                runtime,
                write: None,
                ..start(false)
            }
        }
        fn launched(a: Result<Accepted, String>) -> Origin {
            match a {
                Ok(Accepted::Launch(_, l)) => l.origin,
                Ok(Accepted::Ask(_)) => panic!("asked the operator"),
                Err(e) => panic!("{e}"),
            }
        }
        fn routes(root: &Path) -> (Scope, Scope, Scope) {
            let (parent, worker) = scopes(root);
            let claude = Scope {
                route: Runtime::Claude.route(Some("fixture".into())),
                ..parent.clone()
            };
            (parent, worker, claude)
        }
        fn with_parent(engine: &mut Engine, parent: &Scope, access: &str) {
            engine.running.insert(
                parent.clone(),
                live(
                    parent,
                    vec![view(parent, "a", "running", access)],
                    &["a"],
                    vec![],
                )
                .0,
            );
        }
        fn thread(scope: &Scope, id: &str) -> Caller {
            Caller::Hosted {
                scope: scope.clone(),
                hint: Some(id.into()),
                launch: "helper".into(),
            }
        }

        #[test]
        fn workers_inherit_the_mapped_mode_lower_on_request_and_classifiers_need_no_ask() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, claude) = routes(root.path());
            for scope in [&worker, &claude] {
                engine
                    .running
                    .insert(scope.clone(), live(scope, vec![], &[], vec![]).0);
            }
            use ox_desk_core::modes::{Decision, Level, ModeRequest};
            for (access, runtime, request, expect, decided) in [
                (
                    "workspace",
                    Runtime::Codex,
                    None,
                    Access::Workspace,
                    Decision::Inherited,
                ),
                (
                    "workspace",
                    Runtime::Claude,
                    None,
                    Access::AcceptEdits,
                    Decision::Inherited,
                ),
                (
                    "workspace",
                    Runtime::Codex,
                    Some(Level::Read),
                    Access::ReadOnly,
                    Decision::Lowered,
                ),
                (
                    "auto_review",
                    Runtime::Claude,
                    None,
                    Access::Auto,
                    Decision::Inherited,
                ),
                (
                    "auto_review",
                    Runtime::Codex,
                    None,
                    Access::AutoReview,
                    Decision::Inherited,
                ),
            ] {
                with_parent(&mut engine, &parent, access);
                let mut s = inherit(runtime);
                s.permission = request.map(ModeRequest::Level);
                let o = launched(engine.accept_launch(&thread(&parent, "a"), s));
                assert_eq!(
                    (o.started, o.decided),
                    (Some(expect), Some(decided)),
                    "{access} {runtime:?}"
                );
                assert!(o.yolo.is_none());
            }
            // Old callers: workspace_write false is read, true is write, capped by the source.
            with_parent(&mut engine, &parent, "read_only");
            let o = launched(engine.accept_launch(&thread(&parent, "a"), start(false)));
            assert_eq!(o.started, Some(Access::ReadOnly));
            let err = engine
                .accept_launch(&thread(&parent, "a"), start(true))
                .err()
                .unwrap();
            assert!(err.contains("above the source"), "{err}");
            // A request above the source is refused before anything starts.
            let mut above = inherit(Runtime::Codex);
            above.permission = Some(ModeRequest::Level(Level::Yolo));
            assert!(engine.accept_launch(&thread(&parent, "a"), above).is_err());
            // An unverified parent mode grants nothing.
            with_parent(&mut engine, &parent, "configured");
            assert!(
                engine
                    .accept_launch(&thread(&parent, "a"), inherit(Runtime::Codex))
                    .is_err()
            );
        }

        fn explicit_yolo(runtime: Runtime) -> WorkerStart {
            WorkerStart {
                permission: Some(ox_desk_core::modes::ModeRequest::Level(Ceiling::Yolo)),
                ..inherit(runtime)
            }
        }

        #[test]
        fn inherited_yolo_runs_in_the_requested_checkout_and_keeps_its_visible_parent() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            let checkout = root.path().join("isolated");
            std::fs::create_dir(&checkout).unwrap();
            let child = Scope {
                project: checkout.canonicalize().unwrap(),
                ..worker
            };
            with_parent(&mut engine, &parent, "yolo");
            engine
                .running
                .insert(child.clone(), live(&child, vec![], &[], vec![]).0);
            let request = WorkerStart {
                checkout: Some(checkout),
                ..inherit(Runtime::Codex)
            };
            let result = engine
                .accept_launch(&thread(&parent, "a"), request)
                .unwrap();
            let Accepted::Launch(scope, launch) = result else {
                panic!("unexpected approval request")
            };
            assert_eq!(scope, child);
            assert_eq!(launch.origin.parent.scope.as_ref(), Some(&parent));
            assert_eq!(launch.origin.parent.session.as_deref(), Some("a"));
            assert_eq!(launch.origin.started, Some(Access::Yolo));
            assert!(
                matches!(launch.origin.yolo, Some(YoloAllowance::Inherited { session }) if session == "a")
            );
        }

        #[test]
        fn session_yolo_requires_one_live_token_for_each_activation() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _, _) = routes(root.path());
            let (route, log) = live(
                &scope,
                vec![view(&scope, "a", "idle", "workspace")],
                &[],
                vec![],
            );
            engine.running.insert(scope.clone(), route);
            ox_desk_core::state_file::write_json(
                &root.path().join("host-mode-policy.json"),
                &HostPolicy::YoloAsksEveryTime,
            )
            .unwrap();
            let activate = |engine: &mut Engine| {
                engine
                    .act(
                        1,
                        "mode",
                        &scope,
                        Action::SetAccess("a".into(), Access::Yolo),
                        None,
                    )
                    .unwrap();
                engine.process(&scope, vec![], true).unwrap();
                engine.running[&scope]
                    .state
                    .asks
                    .iter()
                    .find(|a| crate::yolo::is_request(a))
                    .unwrap()
                    .clone()
            };
            let first = activate(&mut engine);
            assert!(log.lock().unwrap().is_empty());
            assert!(
                engine
                    .act(
                        2,
                        "no-proof",
                        &scope,
                        Action::AnswerAsk(first.id.clone(), 0),
                        None
                    )
                    .is_err()
            );
            engine
                .act(
                    2,
                    "accept",
                    &scope,
                    Action::AnswerAsk(first.id.clone(), 0),
                    Some(first.token.clone()),
                )
                .unwrap();
            assert!(matches!(
                log.lock().unwrap().last(),
                Some((Action::SetAccess(_, Access::Yolo), _))
            ));
            engine
                .act(
                    1,
                    "lower",
                    &scope,
                    Action::SetAccess("a".into(), Access::Workspace),
                    None,
                )
                .unwrap();
            let next = activate(&mut engine);
            assert_ne!(first.id, next.id);
            assert!(
                engine
                    .act(
                        2,
                        "stale",
                        &scope,
                        Action::AnswerAsk(next.id.clone(), 0),
                        Some(first.token)
                    )
                    .is_err()
            );
            engine
                .act(
                    2,
                    "lower-instead",
                    &scope,
                    Action::AnswerAsk(next.id, 1),
                    Some(next.token),
                )
                .unwrap();
            assert!(matches!(
                log.lock().unwrap().last(),
                Some((Action::SetAccess(_, Access::AutoReview), _))
            ));
        }

        #[test]
        fn host_policy_controls_the_real_worker_admission_path() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine
                .running
                .insert(worker.clone(), live(&worker, vec![], &[], vec![]).0);
            let save = |policy| {
                ox_desk_core::state_file::write_json(
                    &root.path().join("host-mode-policy.json"),
                    &policy,
                )
                .unwrap()
            };
            let caller = thread(&parent, "a");
            save(HostPolicy::AlwaysYolo);
            let origin = launched(engine.accept_launch(&caller, explicit_yolo(Runtime::Codex)));
            assert_eq!(origin.yolo, Some(YoloAllowance::HostPolicy));
            assert_eq!(
                origin.decided,
                Some(ox_desk_core::modes::Decision::HostPolicy)
            );
            save(HostPolicy::AlwaysRead);
            let child = launched(engine.accept_launch(&caller, inherit(Runtime::Codex)));
            assert_eq!(child.started, Some(Access::ReadOnly));
            assert_eq!(child.ceiling, Ceiling::Yolo);
            assert!(crate::workers::operator_access(&child, Access::Workspace).is_ok());
            engine.set_grant(&parent, "a", true).unwrap();
            save(HostPolicy::YoloAsksEveryTime);
            assert!(matches!(
                engine
                    .accept_launch(&caller, inherit(Runtime::Codex))
                    .unwrap(),
                Accepted::Ask(_)
            ));
            let operator = Caller::Operator {
                scope: parent.clone(),
                parent: "a".into(),
                device: "phone".into(),
            };
            let mut requested = explicit_yolo(Runtime::Codex);
            requested.yolo_confirmed = true;
            assert!(matches!(
                engine.accept_launch(&operator, requested).unwrap(),
                Accepted::Ask(_)
            ));
        }

        #[test]
        fn a_yolo_child_waits_for_the_operator_who_accepts_one_and_declines_another() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine
                .running
                .insert(worker.clone(), live(&worker, vec![], &[], vec![]).0);
            let request = |engine: &mut Engine, confirmed: bool| {
                let (done, result) = std::sync::mpsc::sync_channel(1);
                let mut s = explicit_yolo(Runtime::Codex);
                // An agent cannot confirm YOLO for itself.
                s.yolo_confirmed = confirmed;
                engine
                    .launch(thread(&parent, "a"), s, Replier::Channel(done))
                    .unwrap();
                result.try_recv().unwrap().unwrap()
            };
            let first = request(&mut engine, true);
            assert_eq!(first["status"], "waiting_for_operator");
            assert_eq!(first["waiting_for_operator"]["reason"], "yolo_acceptance");
            assert!(
                engine.running[&worker].launching.is_none(),
                "nothing started"
            );
            let job = first["job_id"].as_u64().unwrap();
            let ask = engine.running[&parent]
                .state
                .asks
                .iter()
                .find(|a| crate::yolo::job_of(&a.id) == Some(job))
                .cloned()
                .expect("an operator ask on the parent session");
            assert_eq!(ask.thread.as_deref(), Some("a"));
            assert_eq!(
                ask.choices,
                ["Accept YOLO", "Start in Auto-review", "Decline"]
            );
            for part in [
                "Parent: a",
                "Runtime: Codex",
                "Route: OX API · fixture",
                "Model: fixture",
                "Task: check",
            ] {
                assert!(ask.detail.contains(part), "{part} in {}", ask.detail);
            }
            assert!(!ask.token.is_empty());
            // The parent sees waiting_for_operator and has no way to answer.
            let status = engine
                .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: job })
                .unwrap();
            assert_eq!(status["status"], "waiting_for_operator");
            assert!(
                engine
                    .parent_call(
                        &thread(&parent, "a"),
                        ParentCall::Steer {
                            job_id: job,
                            text: "yes".into()
                        }
                    )
                    .is_err()
            );
            assert!(
                crate::workers::parent_call(json!({"op":"answer","job_id":job,"choice":0}))
                    .is_err()
            );
            assert_eq!(
                engine
                    .parent_call(&thread(&parent, "a"), ParentCall::List)
                    .unwrap()["jobs"][0]["status"],
                "waiting_for_operator"
            );
            // A stale token is refused; the operator accepts with the live one.
            assert!(
                engine
                    .act(
                        7,
                        "x",
                        &parent,
                        Action::AnswerAsk(ask.id.clone(), 0),
                        Some("stale".into())
                    )
                    .is_err()
            );
            engine
                .act(
                    7,
                    "y1",
                    &parent,
                    Action::AnswerAsk(ask.id.clone(), 0),
                    Some(ask.token.clone()),
                )
                .unwrap();
            let l = engine.running[&worker]
                .launching
                .as_ref()
                .expect("launch after acceptance");
            assert_eq!(l.origin.job_id, job);
            assert_eq!(l.origin.started, Some(Access::Yolo));
            assert!(
                matches!(&l.origin.yolo, Some(YoloAllowance::Accepted { request, .. }) if request.ends_with(&job.to_string()))
            );
            assert_eq!(
                engine
                    .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: job })
                    .unwrap()["status"],
                "starting"
            );
            assert!(
                !engine.running[&parent]
                    .state
                    .asks
                    .iter()
                    .any(crate::yolo::is_request)
            );
            // A second request is declined.
            let second = request(&mut engine, false)["job_id"].as_u64().unwrap();
            engine.process(&parent, vec![], true).unwrap();
            let ask = engine.running[&parent]
                .state
                .asks
                .iter()
                .find(|a| crate::yolo::job_of(&a.id) == Some(second))
                .cloned()
                .unwrap();
            engine
                .act(
                    7,
                    "y2",
                    &parent,
                    Action::AnswerAsk(ask.id, 2),
                    Some(ask.token),
                )
                .unwrap();
            let status = engine
                .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: second })
                .unwrap();
            assert_eq!(status["status"], "declined");
            // The ledger says which acceptance allowed nothing yet, and never stores the task.
            let saved = std::fs::read_to_string(root.path().join("ownership-v1.json")).unwrap();
            assert!(saved.contains("declined") && !saved.contains("\"task\""));
            // "Start in Auto-review" starts the child below YOLO.
            let third = request(&mut engine, false)["job_id"].as_u64().unwrap();
            engine.running.get_mut(&worker).unwrap().launching = None;
            let ask = engine.running[&parent]
                .state
                .asks
                .iter()
                .find(|a| crate::yolo::job_of(&a.id) == Some(third))
                .cloned()
                .unwrap();
            engine
                .act(
                    7,
                    "y3",
                    &parent,
                    Action::AnswerAsk(ask.id, 1),
                    Some(ask.token),
                )
                .unwrap();
            let l = engine.running[&worker].launching.as_ref().unwrap();
            assert_eq!(l.origin.started, Some(Access::AutoReview));
            assert!(l.origin.yolo.is_none());
            assert_eq!(
                l.origin.decided,
                Some(ox_desk_core::modes::Decision::Capped)
            );
        }

        #[test]
        fn a_grant_expires_after_twelve_hours_and_ends_with_its_session() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, _worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine
                .act(1, "g1", &parent, Action::YoloGrant("a".into(), true), None)
                .unwrap();
            let grant = engine.ledger.data.grants[0].clone();
            assert!(grant.granted_at > 0);
            assert_eq!(grant.expires_at(), grant.granted_at + 12 * 3600);
            assert!(engine.yolo_grant(&parent, "a").is_some());
            // Past twelve hours the grant no longer covers anything and is pruned.
            engine
                .ledger
                .change(|d| d.grants[0].granted_at -= 12 * 3600 + 1)
                .unwrap();
            assert!(engine.yolo_grant(&parent, "a").is_none());
            assert!(engine.ledger.data.grants.is_empty());
            // A fresh grant is removed when its session is ended.
            engine
                .act(1, "g2", &parent, Action::YoloGrant("a".into(), true), None)
                .unwrap();
            engine.drop_grant(&parent, "a").unwrap();
            assert!(engine.yolo_grant(&parent, "a").is_none());
        }

        #[test]
        fn a_project_yolo_default_is_announced_once() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, _worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine.yolo_projects = vec![parent.project.clone()];
            assert!(engine.yolo_grant(&parent, "a").is_some());
            let notice = engine.running[&parent].store.notice().map(str::to_owned);
            assert!(
                notice
                    .unwrap()
                    .contains("YOLO is on for this project by default")
            );
            engine
                .running
                .get_mut(&parent)
                .unwrap()
                .store
                .dismiss_notice();
            assert!(engine.yolo_grant(&parent, "a").is_some());
            assert!(
                engine.running[&parent].store.notice().is_none(),
                "announced once"
            );
        }

        #[test]
        fn a_parent_runs_at_most_the_capped_number_of_workers_and_expired_ones_stop() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "workspace");
            let ids = ["w1", "w2", "w3", "w4"];
            engine.running.insert(
                worker.clone(),
                live(
                    &worker,
                    ids.iter()
                        .map(|id| view(&worker, id, "running", "read_only"))
                        .collect(),
                    &ids,
                    vec![],
                )
                .0,
            );
            let parent_ref = workers_parent(&parent, "a");
            for (n, id) in ids.iter().enumerate() {
                let mut o = origin(parent_ref.clone(), Ceiling::Write);
                o.job_id = 100 + n as u64;
                engine.workers.stamp(&worker, id, o);
            }
            assert!(
                engine.check_worker_cap_with(&parent_ref, None).is_ok(),
                "no limit by default"
            );
            let err = engine
                .check_worker_cap_with(&parent_ref, Some(4))
                .unwrap_err();
            assert!(err.contains("OX_WORKER_MAX_PER_PARENT"), "{err}");
            // A write worker beside running write workers is warned, a read one is not.
            assert!(
                engine
                    .write_overlap(&worker, Ceiling::Write)
                    .unwrap()
                    .contains("git worktree")
            );
            assert!(engine.write_overlap(&worker, Ceiling::Read).is_none());
            // Past its limit a running worker is stopped and reported as expired.
            let entry = engine.workers.get_mut(&worker, "w1").unwrap();
            entry.marks.max_minutes = Some(1);
            entry.marks.started_at -= 120;
            entry.marks.run_started_at -= 120;
            engine.expire_workers().unwrap();
            let status = engine.job_status(100).unwrap();
            assert_eq!(status["status"], "cancelled");
            assert!(status["error"].as_str().unwrap().contains("1 minutes"));
            assert!(status["elapsed_seconds"].as_i64().unwrap() >= 120);
            assert_eq!(engine.job_status(101).unwrap()["status"], "running");
        }

        #[test]
        fn a_grant_covers_descendants_and_revoking_it_makes_the_next_spawn_ask() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            with_parent(&mut engine, &parent, "yolo");
            engine.running.insert(
                worker.clone(),
                live(
                    &worker,
                    vec![view(&worker, "w", "running", "yolo")],
                    &["w"],
                    vec![],
                )
                .0,
            );
            engine
                .act(1, "g1", &parent, Action::YoloGrant("a".into(), true), None)
                .unwrap();
            let o = launched(engine.accept_launch(&thread(&parent, "a"), inherit(Runtime::Codex)));
            assert_eq!(o.started, Some(Access::Yolo));
            assert!(
                matches!(&o.yolo, Some(YoloAllowance::Grant { session: Some(s), .. }) if s == "a")
            );
            // A YOLO worker of the granted session is covered too.
            let mut child = origin(workers_parent(&parent, "a"), Ceiling::Yolo);
            child.yolo = o.yolo.clone();
            engine.workers.stamp(&worker, "w", child);
            engine.process(&parent, vec![], true).unwrap();
            let row = engine.running[&parent]
                .state
                .sessions
                .iter()
                .find(|r| r.id == "a")
                .unwrap();
            assert_eq!(row.detail["yolo_grant"], true);
            assert_eq!(row.detail["yolo_children"][0]["session"], "w");
            let nested =
                launched(engine.accept_launch(&thread(&worker, "w"), inherit(Runtime::Codex)));
            assert!(matches!(nested.yolo, Some(YoloAllowance::Grant { .. })));
            // Revoking affects new spawns only; the running child keeps its stamp.
            engine
                .act(1, "g2", &parent, Action::YoloGrant("a".into(), false), None)
                .unwrap();
            assert!(matches!(
                engine.accept_launch(&thread(&parent, "a"), explicit_yolo(Runtime::Codex)),
                Ok(Accepted::Ask(_))
            ));
            assert!(matches!(
                engine.accept_launch(&thread(&worker, "w"), explicit_yolo(Runtime::Codex)),
                Ok(Accepted::Ask(_))
            ));
            assert_eq!(
                engine.workers.get(&worker, "w").unwrap().origin.started,
                None
            );
            assert!(
                engine
                    .workers
                    .get(&worker, "w")
                    .unwrap()
                    .origin
                    .yolo
                    .is_some()
            );
            // The operator's own /worker needs the YOLO confirmation instead.
            let operator = Caller::Operator {
                scope: parent.clone(),
                parent: "a".into(),
                device: "client-1".into(),
            };
            engine.running.insert(
                parent.clone(),
                live_with(
                    &parent,
                    vec![view(&parent, "a", "idle", "yolo")],
                    vec![("a", timeline(&[], false))],
                    vec![],
                )
                .0,
            );
            assert!(
                engine
                    .accept_launch(&operator, inherit(Runtime::Codex))
                    .err()
                    .unwrap()
                    .contains("Confirm YOLO")
            );
            let mut confirmed = inherit(Runtime::Codex);
            confirmed.yolo_confirmed = true;
            let o = launched(engine.accept_launch(&operator, confirmed));
            assert!(matches!(o.yolo, Some(YoloAllowance::Operator { .. })));
        }

        #[test]
        fn a_host_restart_declines_a_pending_yolo_request() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                with_parent(&mut engine, &parent, "bypass_permissions");
                engine
                    .running
                    .insert(worker.clone(), live(&worker, vec![], &[], vec![]).0);
                let (done, result) = std::sync::mpsc::sync_channel(1);
                // A Claude parent in bypass: the parent scope here is Codex, so use the table directly.
                let s = explicit_yolo(Runtime::Codex);
                engine.running.insert(
                    parent.clone(),
                    live(
                        &parent,
                        vec![view(&parent, "a", "running", "yolo")],
                        &["a"],
                        vec![],
                    )
                    .0,
                );
                engine
                    .launch(thread(&parent, "a"), s, Replier::Channel(done))
                    .unwrap();
                result.try_recv().unwrap().unwrap()["job_id"]
                    .as_u64()
                    .unwrap()
            };
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.asking.is_empty() && engine.ledger.data.asking.is_empty());
            with_parent(&mut engine, &parent, "yolo");
            let status = engine
                .parent_call(&thread(&parent, "a"), ParentCall::Status { job_id: job })
                .unwrap();
            assert_eq!(status["status"], "declined");
            assert!(status["error"].as_str().unwrap().contains("restarted"));
            let queued: Vec<_> = engine
                .ledger
                .data
                .worker_notices
                .pending
                .iter()
                .filter(|n| n.job == job)
                .map(|n| n.status.as_str())
                .collect();
            assert_eq!(queued, ["waiting_for_operator", "declined"]);
            assert!(
                !engine.running.contains_key(&worker),
                "nothing is relaunched"
            );
        }

        /// A live parent asks for a YOLO worker; the operator has not answered yet.
        fn waiting_yolo_request(engine: &mut Engine, parent: &Scope, worker: &Scope) -> u64 {
            with_parent(engine, parent, "yolo");
            engine
                .running
                .insert(worker.clone(), live(worker, vec![], &[], vec![]).0);
            let (done, result) = std::sync::mpsc::sync_channel(1);
            let mut s = explicit_yolo(Runtime::Codex);
            s.yolo_confirmed = true;
            engine
                .launch(thread(parent, "a"), s, Replier::Channel(done))
                .unwrap();
            let waiting = result.try_recv().unwrap().unwrap();
            assert_eq!(waiting["status"], "waiting_for_operator");
            let job = waiting["job_id"].as_u64().unwrap();
            assert!(engine.asking.contains_key(&job));
            job
        }

        #[test]
        fn ending_the_parent_withdraws_its_worker_yolo_request_and_a_late_answer_is_refused() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = waiting_yolo_request(&mut engine, &parent, &worker);
            // The fixture store has no live process to end, so run the step that follows the end.
            engine.end_children(&parent, "a").unwrap();
            assert!(!engine.asking.contains_key(&job), "ask withdrawn");
            assert!(engine.ledger.data.asking.is_empty(), "ledger cleared");
            let outcome = engine
                .ledger
                .data
                .outcomes
                .iter()
                .find(|o| o.origin.job_id == job)
                .expect("a cancelled outcome");
            assert_eq!(outcome.status, "cancelled");
            assert_eq!(outcome.message, ENDED_PARENT);
            // A late answer cannot start the worker.
            assert!(engine.answer_yolo(7, &parent, job, 0).is_err());
            assert!(
                engine.running[&worker].launching.is_none(),
                "nothing started"
            );
        }

        #[test]
        fn a_yolo_answer_is_refused_once_its_parent_session_is_no_longer_live() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = waiting_yolo_request(&mut engine, &parent, &worker);
            // The parent's route goes away without the end path running.
            engine.running.remove(&parent);
            let refused = engine.answer_yolo(7, &parent, job, 0).unwrap_err();
            assert_eq!(refused.to_string(), ENDED_PARENT);
            assert!(
                !engine.asking.contains_key(&job),
                "the stale ask is withdrawn"
            );
            assert!(
                engine.running[&worker].launching.is_none(),
                "nothing started"
            );
        }

        /// Wait for the off-thread context window lookup, then advance the handoff.
        fn settle(engine: &mut Engine, target: &Scope) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while engine.running[target].launching.is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                engine.advance_flows().unwrap();
            }
        }
        fn claude_source(root: &Path) -> Scope {
            Scope {
                project: root.canonicalize().unwrap(),
                route: Runtime::Claude.route(None),
            }
        }

        #[test]
        fn a_handoff_is_refused_while_the_source_is_busy_and_names_what_is_pending() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let source = claude_source(root.path());
            let handoff = |request: &str| {
                Action::StartHandoff(HandoffStart {
                    host: None,
                    source: "s".into(),
                    runtime: Runtime::Codex,
                    model: Some("fixture".into()),
                    mode: Default::default(),
                    request: request.into(),
                    yolo_confirmed: false,
                })
            };
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "running", "accept_edits")],
                    vec![("s", timeline(&[("u1", "user", "hi")], true))],
                    vec![],
                )
                .0,
            );
            let err = engine
                .act(1, "h1", &source, handoff("next"), None)
                .unwrap_err();
            assert!(err.to_string().contains("running a turn"), "{err}");
            let ask = AskView {
                command: None,
                id: RequestId::Number(4),
                token: "t".into(),
                thread: Some("s".into()),
                turn: Some("t1".into()),
                item: None,
                choice_details: vec![None],
                title: "Run cargo test".into(),
                detail: String::new(),
                choices: vec!["Allow".into()],
                questions: vec![],
            };
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "needs_input", "accept_edits")],
                    vec![("s", timeline(&[("u1", "user", "hi")], false))],
                    vec![ask],
                )
                .0,
            );
            let err = engine
                .act(1, "h2", &source, handoff("next"), None)
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("1 pending request: Run cargo test"),
                "{err}"
            );
            assert!(engine.preparing.is_empty());
            assert!(engine.ledger.data.handoffs.is_empty(), "nothing started");
        }

        #[test]
        fn a_handoff_carries_real_messages_stamps_references_and_fails_on_restart() {
            let root = tempfile::tempdir().unwrap();
            let source = claude_source(root.path());
            let (_, target) = scopes(root.path());
            let items = [
                ("u1", "user", "first request"),
                ("c1", "tool", "cargo test"),
                ("a1", "assistant", "tests pass"),
                ("r1", "thinking", "private reasoning"),
                ("u2", "user", "second request"),
                ("a2", "assistant", "partial repl"),
            ];
            {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                engine.running.insert(
                    source.clone(),
                    live_with(
                        &source,
                        vec![view(&source, "s", "idle", "accept_edits")],
                        vec![("s", timeline(&items, false))],
                        vec![],
                    )
                    .0,
                );
                let (l, log) = live(&target, vec![], &[], vec![]);
                engine.running.insert(target.clone(), l);
                // A window a stock client reported earlier on the target route sets the budget.
                engine
                    .ledger
                    .change(|d| {
                        crate::durable::record_window(
                            &mut d.windows,
                            crate::durable::Window {
                                route: target.route.clone(),
                                model: Some("fixture".into()),
                                tokens: 300_000,
                            },
                        )
                    })
                    .unwrap();
                engine
                    .act(
                        1,
                        "h1",
                        &source,
                        Action::StartHandoff(HandoffStart {
                            host: None,
                            source: "s".into(),
                            runtime: Runtime::Codex,
                            model: Some("fixture".into()),
                            mode: Default::default(),
                            request: "now do this".into(),
                            yolo_confirmed: false,
                        }),
                        None,
                    )
                    .unwrap();
                engine.process(&source, vec![], true).unwrap();
                settle(&mut engine, &target);
                let l = engine.running[&target]
                    .launching
                    .as_ref()
                    .expect("target opening");
                let o = &l.origin;
                assert_eq!(o.relation, ox_desk_core::origin::Relation::Handoff);
                assert_eq!(
                    o.started,
                    Some(Access::Workspace),
                    "Accept edits maps to Workspace"
                );
                let h = o.handoff.as_ref().unwrap();
                assert_eq!((h.session.as_str(), h.runtime), ("s", Runtime::Claude));
                assert_eq!(h.selection.items, ["u1", "c1", "a1", "u2", "a2"]);
                assert_eq!(h.selection.omitted, 0);
                assert_eq!(
                    (h.selection.window, h.selection.window_known),
                    (300_000, true)
                );
                assert!(matches!(
                    log.lock().unwrap()[0].0,
                    Action::SetNextAccess(Access::Workspace)
                ));
                // The stand-in runtime confirms the preset, as a native store does.
                let confirm = LinkEvent::Host(Box::new(Server::Delta {
                    cursor: Cursor {
                        epoch: "e".into(),
                        seq: 3,
                    },
                    scope: target.clone(),
                    changes: vec![Change::State {
                        connection: "ready".into(),
                        capabilities: vec!["start".into()],
                        detail: json!({"next_access": "workspace"}),
                    }],
                }));
                engine.process(&target, vec![confirm], false).unwrap();
                // The target received the real messages, in order, then the request.
                let sent = log.lock().unwrap().clone();
                let (text, carried) = sent
                    .iter()
                    .find_map(|(a, _)| match a {
                        Action::InjectHandoff(text, items) => Some((text.clone(), items.clone())),
                        _ => None,
                    })
                    .expect("the first turn was sent");
                assert_eq!(text, "now do this");
                let texts: Vec<(String, String)> = carried
                    .iter()
                    .skip(1)
                    .map(|i| {
                        (
                            i["role"].as_str().unwrap().into(),
                            i["content"][0]["text"].as_str().unwrap().into(),
                        )
                    })
                    .collect();
                assert_eq!(
                    texts,
                    [
                        ("user", "first request"),
                        ("assistant", "[Command `cargo test` exited 0]"),
                        ("assistant", "tests pass"),
                        ("user", "second request"),
                        ("assistant", "partial repl"),
                    ]
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))
                );
                // The ledger keeps references, never message text.
                let saved = std::fs::read_to_string(root.path().join("ownership-v1.json")).unwrap();
                assert!(saved.contains("\"starting\"") && saved.contains("\"u2\""));
                for text in [
                    "first request",
                    "tests pass",
                    "now do this",
                    "private reasoning",
                ] {
                    assert!(!saved.contains(text), "{text} is not stored");
                }
            }
            // The host restarts before the target's first turn completes: failed, never retried.
            let (tx, rx) = std::sync::mpsc::sync_channel(64);
            let engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.running.is_empty());
            assert!(rx.try_recv().is_err());
            let h = &engine.ledger.data.handoffs[0];
            assert_eq!(h.status, "failed");
            assert!(
                h.message
                    .as_deref()
                    .unwrap()
                    .contains("Nothing was retried")
            );
        }

        #[test]
        fn a_same_runtime_route_change_is_a_handoff_with_a_claude_context_block() {
            let root = tempfile::tempdir().unwrap();
            let (target, previous, mut engine, events) = warm_claude(root.path());
            let source = Scope {
                route: Runtime::Claude.route(Some("fixture".into())),
                ..target.clone()
            };
            let items = [("u1", "user", "first request"), ("a1", "assistant", "done")];
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "idle", "auto")],
                    vec![("s", timeline(&items, false))],
                    vec![],
                )
                .0,
            );
            engine
                .act(
                    1,
                    "h1",
                    &source,
                    Action::StartHandoff(HandoffStart {
                        host: None,
                        source: "s".into(),
                        runtime: Runtime::Claude,
                        model: None,
                        mode: Default::default(),
                        request: "continue on my subscription".into(),
                        yolo_confirmed: false,
                    }),
                    None,
                )
                .unwrap();
            engine.process(&source, vec![], true).unwrap();
            settle(&mut engine, &target);
            let o = &engine.running[&target].launching.as_ref().unwrap().origin;
            // Claude Auto is a classifier mode and is inherited without asking.
            assert_eq!(o.started, Some(Access::Auto));
            assert!(o.yolo.is_none());
            let launch = engine.running[&target].launching.as_ref().unwrap();
            let Payload::Handoff {
                text,
                items: carried,
                ..
            } = &launch.payload
            else {
                panic!("expected Claude handoff");
            };
            assert!(carried.is_empty(), "Claude Code has no history injection");
            let (block, request) = ox_desk_core::handoff::split_claude_turn(text).unwrap();
            assert_eq!(request, "continue on my subscription");
            assert!(
                block.contains("first request")
                    && block.contains("<message role=\"assistant\" id=\"a1\">")
            );
            let first_turn = text.clone();
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                engine.process(&target, vec![], false).unwrap();
                let store = &engine.running[&target].store;
                if let Some(id) = store.selected().filter(|id| *id != previous)
                    && store
                        .transcript(id)
                        .is_some_and(|t| t.user_messages(&first_turn, &[]) == 1)
                {
                    assert_eq!(store.applied_access(Some(id)), Some(Access::Auto));
                    break;
                }
                let event = events
                    .recv_timeout(until.saturating_duration_since(Instant::now()))
                    .expect("Claude fixture did not receive the carried first turn");
                engine.running.get_mut(&target).unwrap().store.apply(event);
            }
        }

        #[test]
        fn a_codex_handoff_is_reported_only_after_its_history_and_first_turn_are_accepted() {
            let root = tempfile::tempdir().unwrap();
            let (_, target) = scopes(root.path());
            let later = Instant::now() + Duration::from_secs(60);
            // The runtime refused thread/inject_items: the transcript carries the error.
            let mut refused = timeline(&[], false);
            refused.detail["error"] = json!("thread/inject_items: invalid item");
            let (l, _) = live_with(
                &target,
                vec![view(&target, "t", "idle", "workspace")],
                vec![("t", refused)],
                vec![],
            );
            let err = first_turn(&l.store, "t", later).unwrap().unwrap_err();
            assert!(err.contains("invalid item"), "{err}");
            // Nothing in flight and no error: the first turn was accepted.
            let (l, _) = live_with(
                &target,
                vec![view(&target, "t", "running", "workspace")],
                vec![("t", timeline(&[], true))],
                vec![],
            );
            assert_eq!(first_turn(&l.store, "t", later), Some(Ok(())));
        }

        #[test]
        fn read_ox_thread_reads_only_the_callers_own_chain_in_its_project() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let source = claude_source(root.path());
            let (_, target) = scopes(root.path());
            let back = Scope {
                route: Runtime::Claude.route(Some("other".into())),
                ..source.clone()
            };
            engine.running.insert(
                source.clone(),
                live_with(
                    &source,
                    vec![view(&source, "s", "idle", "default")],
                    vec![(
                        "s",
                        timeline(
                            &[("u1", "user", "first"), ("a1", "assistant", "long answer")],
                            false,
                        ),
                    )],
                    vec![],
                )
                .0,
            );
            engine.running.insert(
                target.clone(),
                live_with(
                    &target,
                    vec![
                        view(&target, "t", "running", "workspace"),
                        view(&target, "x", "running", "workspace"),
                    ],
                    vec![
                        ("t", timeline(&[("v1", "user", "codex turn")], true)),
                        ("x", timeline(&[], true)),
                    ],
                    vec![],
                )
                .0,
            );
            engine.running.insert(
                back.clone(),
                live_with(
                    &back,
                    vec![view(&back, "b", "running", "default")],
                    vec![("b", timeline(&[], true))],
                    vec![],
                )
                .0,
            );
            let stamp = |from: &Scope, session: &str, project: &Path| {
                let mut o = origin(workers_parent(from, session), Ceiling::Write);
                o.relation = ox_desk_core::origin::Relation::Handoff;
                o.handoff = Some(Box::new(ox_desk_core::handoff::HandoffSource {
                    history_broker: None,
                    host: None,
                    scope: Scope {
                        project: project.to_owned(),
                        route: from.route.clone(),
                    },
                    session: session.into(),
                    native: None,
                    title: session.into(),
                    runtime: from.route.runtime,
                    route: from.route.clone(),
                    source_access: None,
                    source_level: None,
                    decision: ox_desk_core::modes::Decision::Inherited,
                    selection: Default::default(),
                }));
                o
            };
            let project = source.project.clone();
            engine
                .workers
                .stamp(&target, "t", stamp(&source, "s", &project));
            engine
                .workers
                .stamp(&back, "b", stamp(&target, "t", &project));
            let read = |engine: &mut Engine,
                        caller: Caller,
                        host: Option<&str>,
                        session: Option<&str>,
                        item: Option<&str>| {
                let (done, result) = std::sync::mpsc::sync_channel(1);
                engine
                    .read_thread(
                        &caller,
                        ParentCall::ReadThread {
                            host: host.map(Into::into),
                            session: session.map(Into::into),
                            item: item.map(Into::into),
                            after: None,
                            limit: None,
                        },
                        Replier::Channel(done),
                    )
                    .unwrap();
                result.try_recv().expect("answered at once")
            };
            // The Codex target reads an omitted item of its Claude source in full.
            let page = read(&mut engine, thread(&target, "t"), None, None, Some("a1")).unwrap();
            assert_eq!(page["items"][0]["text"], "long answer");
            // The second handoff reads the whole chain, back to the first source.
            let page = read(&mut engine, thread(&back, "b"), None, Some("s"), None).unwrap();
            assert_eq!(page["items"][0]["text"], "first");
            let page = read(&mut engine, thread(&back, "b"), None, None, None).unwrap();
            assert_eq!(page["items"][0]["text"], "codex turn");
            // A sibling session on the same app-server has no chain; the source itself has none.
            assert!(
                read(&mut engine, thread(&target, "x"), None, None, None)
                    .unwrap_err()
                    .contains("no source")
            );
            assert!(
                read(&mut engine, thread(&target, "t"), None, Some("x"), None)
                    .unwrap_err()
                    .contains("not in this session's handoff chain")
            );
            // A source outside the caller's project is refused.
            engine
                .workers
                .stamp(&target, "x", stamp(&source, "s", Path::new("/elsewhere")));
            assert!(
                read(&mut engine, thread(&target, "x"), None, None, None)
                    .unwrap_err()
                    .contains("outside")
            );
            let me = engine.host_id.to_string();
            let qualified =
                read(&mut engine, thread(&back, "b"), Some(&me), Some("s"), None).unwrap();
            assert_eq!(qualified["source"]["host"]["id"], me);
            assert!(
                read(
                    &mut engine,
                    thread(&back, "b"),
                    Some("another-host"),
                    Some("s"),
                    None
                )
                .is_err()
            );

            // Two machines can have identical project paths and native session IDs.
            // A foreign stamp must never turn into a lookup in this host's transcript.
            let mut foreign = stamp(&source, "s", &project);
            foreign.handoff.as_mut().unwrap().host = Some(ox_desk_core::handoff::SourceHost {
                id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                name: "studio".into(),
            });
            engine.workers.get_mut(&target, "x").unwrap().origin = foreign;
            let chain = engine.handoff_chain(&target, "x");
            assert_eq!(chain.len(), 1);
            assert_eq!(chain[0].host.as_ref().unwrap().name, "studio");
            assert!(
                read(&mut engine, thread(&target, "x"), None, None, None)
                    .unwrap_err()
                    .contains("another host")
            );
            // A terminal launch is not a hosted session.
            let terminal = Caller::External(ExternalCaller {
                project: project.clone(),
                runtime: Runtime::Codex,
                session: Some("t".into()),
                launch: "l".into(),
            });
            assert!(read(&mut engine, terminal, None, None, None).is_err());
        }

        /// A Live whose store is a real local Codex store on an inert stdio process.
        fn local_live(scope: &Scope) -> Live {
            let link =
                codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
            let store = Store::open(
                ox_desk_core::store::Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: "openai".into(),
                },
                link,
            )
            .unwrap();
            Live {
                state: store.host_snapshot(scope),
                store,
                launching: None,
                fork_origin: None,
                confirming: None,
                opening: None,
                awaiting_input: Default::default(),
                terminal: None,
                schema_started: true,
                recovered: vec![],
                inbox: Inbox::new(|| {}),
                generation: 1,
                tokens: HashMap::new(),
                tracker: Tracker::new(),
                held_handoff: None,
            }
        }

        /// A real Claude store over a fake stock helper, with its first blank consumed by
        /// a lead turn: the state a warm route is in when a worker starts.
        fn warm_claude(
            root: &Path,
        ) -> (
            Scope,
            String,
            Engine,
            std::sync::mpsc::Receiver<ox_desk_core::links::LinkEvent>,
        ) {
            use ox_desk_core::store::claude::{Launcher, SUBSCRIPTION_PROVIDER};
            // The fixture's journal records the real cwd; macOS temp dirs sit behind /var -> /private/var.
            let root = &root.canonicalize().unwrap();
            let scope = Scope {
                project: root.to_owned(),
                route: Runtime::Claude.route(None),
            };
            let (etx, rx) = std::sync::mpsc::channel();
            let mut store = Store::open_claude(
                ox_desk_core::store::Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: SUBSCRIPTION_PROVIDER.into(),
                },
                Launcher {
                    ox: Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/worker-claude.py"),
                    path: std::env::var_os("PATH").unwrap(),
                    profiles: ox_desk_core::claude::catalog::Profiles {
                        api: root.join("api"),
                        subscription: root.join("native"),
                    },
                    index: root.join("index.json"),
                    aliases: vec![],
                },
                move |event| {
                    let _ = etx.send(event);
                },
            )
            .unwrap();
            assert!(store.can_send(None));
            assert!(store.live_threads().is_empty());
            store
                .dispatch(Action::SendText(None, "lead first turn".into()))
                .unwrap();
            let lead = store.take_opened().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while store.applied_access(Some(&lead)).is_none() || store.thread_busy(&lead) {
                if let ox_desk_core::store::Connection::Closed(reason) = store.state() {
                    panic!("Claude fixture failed initialization: {reason}");
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let event = rx.recv_timeout(remaining).unwrap_or_else(|err| {
                    panic!("Claude fixture lead turn: {err}, state {:?}", store.state())
                });
                store.apply(event);
            }
            let mut live = local_live(&scope);
            live.state = store.host_snapshot(&scope);
            live.store = store;
            let (tx, _) = std::sync::mpsc::sync_channel(128);
            let mut engine = Engine::open(config(root), root, tx).unwrap();
            engine.running.insert(scope.clone(), live);
            (scope, lead, engine, rx)
        }
        /// Start one session worker from `lead` on the lead's own warm route and drive the
        /// host until its launch answers.
        fn launch_on_warm_claude(
            engine: &mut Engine,
            scope: &Scope,
            lead: &str,
            rx: &std::sync::mpsc::Receiver<ox_desk_core::links::LinkEvent>,
        ) -> Result<Value, String> {
            launch_pinned_on_warm_claude(engine, scope, lead, rx, None)
        }
        fn launch_pinned_on_warm_claude(
            engine: &mut Engine,
            scope: &Scope,
            lead: &str,
            rx: &std::sync::mpsc::Receiver<ox_desk_core::links::LinkEvent>,
            subscription_model: Option<&str>,
        ) -> Result<Value, String> {
            launch_task_on_warm_claude(engine, scope, lead, rx, subscription_model, "check")
        }
        fn launch_task_on_warm_claude(
            engine: &mut Engine,
            scope: &Scope,
            lead: &str,
            rx: &std::sync::mpsc::Receiver<ox_desk_core::links::LinkEvent>,
            subscription_model: Option<&str>,
            task: &str,
        ) -> Result<Value, String> {
            let caller = Caller::Operator {
                scope: scope.clone(),
                parent: lead.to_owned(),
                device: "probe".into(),
            };
            let mut request = start(false);
            request.task = task.into();
            request.runtime = Runtime::Claude;
            request.model = None;
            request.parent = lead.to_owned();
            request.mode = WorkerMode::Session;
            request.subscription_model = subscription_model.map(str::to_owned);
            let (reply, answered) = std::sync::mpsc::sync_channel(1);
            engine
                .launch(caller, request, Replier::Channel(reply))
                .unwrap();
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                while let Ok(event) = rx.try_recv() {
                    engine.running.get_mut(scope).unwrap().store.apply(event);
                }
                engine.process(scope, vec![], false).unwrap();
                if let Ok(result) = answered.try_recv() {
                    return result;
                }
                if Instant::now() >= until {
                    let live = &engine.running[scope];
                    panic!(
                        "the launch never answered: stage {:?}, can_send {}, next {:?}, applied {:?}, notice {:?}",
                        live.launching.as_ref().map(|l| match l.stage {
                            Stage::Opening => "opening",
                            Stage::AccessRequested => "access",
                            Stage::Sent => "sent",
                        }),
                        live.store.can_send(None),
                        live.store.next_access(),
                        live.store.applied_access(None),
                        live.store.notice(),
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        /// Drive the host until `done` holds for its state.
        fn pump_engine(
            engine: &mut Engine,
            scope: &Scope,
            rx: &std::sync::mpsc::Receiver<ox_desk_core::links::LinkEvent>,
            done: &dyn Fn(&Engine) -> bool,
        ) {
            let until = Instant::now() + Duration::from_secs(20);
            while !done(engine) {
                assert!(Instant::now() < until, "the host never reached the state");
                while let Ok(event) = rx.try_recv() {
                    engine.running.get_mut(scope).unwrap().store.apply(event);
                }
                engine.process(scope, vec![], false).unwrap();
                thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn a_hosted_worker_wakes_its_claude_parent_with_its_result_once() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            engine
                .running
                .get_mut(&scope)
                .unwrap()
                .store
                .dispatch(Action::SendText(
                    Some(lead.clone()),
                    "delegate a review".into(),
                ))
                .unwrap();
            let mut request = start(false);
            request.runtime = Runtime::Claude;
            request.model = None;
            request.subscription_model = Some("fable".into());
            request.mode = WorkerMode::Session;
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            engine
                .launch(thread(&scope, &lead), request, Replier::Channel(reply))
                .unwrap();
            pump_engine(&mut engine, &scope, &rx, &|e| {
                !e.ledger.data.worker_notices.latest.is_empty()
                    && e.ledger.data.worker_notices.pending.is_empty()
            });
            let job = answer.recv().unwrap().unwrap()["job_id"].as_u64().unwrap();
            let text = format!("OX worker #{job}: completed (update 1).\nreviewed");
            let live = &engine.running[&scope];
            assert_eq!(
                live.store
                    .transcript(&lead)
                    .unwrap()
                    .user_messages(&text, &[]),
                1
            );
            let row = live.state.sessions.iter().find(|s| s.id == lead).unwrap();
            assert_eq!(
                catalog_row(row).detail["worker_notice"],
                json!({"job_id": job, "status":"completed"})
            );
            engine.process(&scope, vec![], true).unwrap();
            let (tx, _) = std::sync::mpsc::sync_channel(1);
            let engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert!(engine.ledger.data.worker_notices.pending.is_empty());
            let row = engine.cold[&scope]
                .sessions
                .iter()
                .find(|s| s.id == lead)
                .unwrap();
            assert_eq!(row.detail["worker_notice"]["job_id"], job);
        }

        #[test]
        fn worker_notices_wait_for_a_busy_parent_and_confirm_native_acceptance() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let origin = origin(workers_parent(&scope, &lead), Ceiling::Read);
            engine
                .running
                .get_mut(&scope)
                .unwrap()
                .store
                .dispatch(Action::SendText(Some(lead.clone()), "work first".into()))
                .unwrap();
            engine
                .ledger
                .change(|d| {
                    d.worker_notices
                        .observe(&origin, "waiting_for_operator", "Please answer.");
                    d.worker_notices
                        .observe(&origin, "waiting_for_operator", "Same open request.");
                    d.record_outcome(crate::durable::Outcome {
                        origin,
                        status: "declined".into(),
                        message: "The operator declined.".into(),
                    });
                })
                .unwrap();
            engine.advance_flows().unwrap();
            let pending = &engine.ledger.data.worker_notices.pending;
            assert_eq!(pending.len(), 2);
            assert!(pending.iter().all(|n| !n.submitted));
            let messages: Vec<_> = pending.iter().map(|n| n.text.clone()).collect();
            // Only drive the prior native turn to its boundary. The automatic input
            // is now in Claude's stdin but its echo is still in the fixture's inbox.
            while !engine.ledger.data.worker_notices.pending[0].submitted {
                let event = rx.recv_timeout(Duration::from_secs(3)).unwrap();
                engine.process(&scope, vec![event], false).unwrap();
            }
            assert_eq!(
                engine.ledger.data.worker_notices.pending.len(),
                2,
                "stdin write awaits Claude's echo"
            );
            assert!(!engine.ledger.data.worker_notices.pending[1].submitted);
            pump_engine(&mut engine, &scope, &rx, &|e| {
                e.ledger.data.worker_notices.pending.is_empty()
            });
            let t = engine.running[&scope].store.transcript(&lead).unwrap();
            for text in &messages {
                assert_eq!(t.user_messages(text, &[]), 1);
            }
        }

        #[test]
        fn restarted_host_reconciles_an_unconfirmed_worker_notice_before_resending() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _) = std::sync::mpsc::sync_channel(8);
            let mut engine = Engine::open(config(root.path()), root.path(), tx.clone()).unwrap();
            let (scope, _) = scopes(root.path());
            let mut saved_parent = live(&scope, vec![], &[], vec![]).0.state;
            saved_parent.sessions = vec![view(&scope, "parent", "idle", "read_only")];
            engine
                .ledger
                .update(SavedScope::from_state(&saved_parent))
                .unwrap();
            let origin = origin(workers_parent(&scope, "parent"), Ceiling::Read);
            engine
                .ledger
                .change(|d| {
                    d.worker_notices
                        .observe(&origin, "failed", "The runtime refused the turn.");
                    // The host stopped after writing to the runtime but before its receipt.
                    // The best-effort live alert was already reserved at the time of the crash.
                    d.worker_notices.pending[0].announced = true;
                    d.worker_notices.pending[0].submitted = true;
                })
                .unwrap();
            let text = engine.ledger.data.worker_notices.pending[0].text.clone();
            drop(engine);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            assert_eq!(
                engine.cold[&scope].sessions[0].detail["worker_notice"],
                json!({"job_id":41, "status":"failed"})
            );
            engine.advance_flows().unwrap();
            assert!(
                engine.running.is_empty(),
                "explicitly closed parents stay closed"
            );
            assert_eq!(engine.ledger.data.worker_notices.pending.len(), 1);
            let (parent, log) = live_with(
                &scope,
                vec![view(&scope, "parent", "idle", "read_only")],
                vec![(
                    "parent",
                    timeline(&[("persisted-input", "user", &text)], false),
                )],
                vec![],
            );
            engine.running.insert(scope, parent);
            engine.advance_flows().unwrap();
            assert!(engine.ledger.data.worker_notices.pending.is_empty());
            assert!(
                log.lock()
                    .unwrap()
                    .iter()
                    .all(|(action, _)| !matches!(action, Action::SendText(..)))
            );
        }

        #[test]
        fn reopening_an_ended_claude_parent_delivers_its_saved_notice() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            engine.end_thread(&scope, &lead, false).unwrap();
            let origin = origin(workers_parent(&scope, &lead), Ceiling::Read);
            engine
                .ledger
                .change(|d| {
                    d.worker_notices
                        .observe(&origin, "interrupted", "Partial result")
                })
                .unwrap();
            engine.process(&scope, vec![], true).unwrap();
            let n = &engine.ledger.data.worker_notices.pending[0];
            assert!(!n.submitted);
            let text = n.text.clone();
            let live = engine.running.get_mut(&scope).unwrap();
            assert!(live.store.is_ended(&lead));
            live.store.select(&lead).unwrap();
            pump_engine(&mut engine, &scope, &rx, &|e| {
                e.ledger.data.worker_notices.pending.is_empty()
            });
            assert_eq!(
                engine.running[&scope]
                    .store
                    .transcript(&lead)
                    .unwrap()
                    .user_messages(&text, &[]),
                1
            );
        }

        #[test]
        fn a_refused_history_page_holds_the_uncertain_notice_without_repeated_reads() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _) = std::sync::mpsc::sync_channel(8);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            let origin = origin(workers_parent(&scope, "parent"), Ceiling::Read);
            engine
                .ledger
                .change(|d| {
                    d.worker_notices.observe(&origin, "completed", "Result");
                    d.worker_notices.pending[0].submitted = true;
                    d.worker_notices.pending[0].announced = true;
                })
                .unwrap();
            let mut page = timeline(&[], false);
            page.detail["older"] = true.into();
            page.detail["error"] = "Earlier turns unavailable".into();
            let (parent, log) = live_with(
                &scope,
                vec![view(&scope, "parent", "idle", "read_only")],
                vec![("parent", page)],
                vec![],
            );
            engine.running.insert(scope, parent);
            for _ in 0..10 {
                engine.advance_flows().unwrap();
            }
            let actions = log.lock().unwrap();
            assert_eq!(
                actions
                    .iter()
                    .filter(|(a, _)| matches!(a, Action::LoadOlder(_)))
                    .count(),
                1
            );
            assert!(
                actions
                    .iter()
                    .all(|(a, _)| !matches!(a, Action::SendText(..)))
            );
            assert!(engine.ledger.data.worker_notices.pending[0].submitted);
        }

        #[test]
        fn clearing_a_persons_draft_after_a_refusal_releases_the_queued_notice() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let origin = origin(workers_parent(&scope, &lead), Ceiling::Read);
            engine
                .ledger
                .change(|d| {
                    d.worker_notices.observe(&origin, "failed", "Worker failed");
                    d.worker_notices.pending[0].submitted = true;
                })
                .unwrap();
            let n = engine.ledger.data.worker_notices.pending[0].clone();
            engine
                .notice_attempts
                .insert(n.id, Instant::now() - Duration::from_secs(31));
            engine
                .running
                .get_mut(&scope)
                .unwrap()
                .store
                .edit(Some(&lead), "My own message".into());
            engine
                .record_notice_receipts(vec![ox_desk_core::store::WorkerNoticeReceipt::Rejected(
                    n.id,
                )])
                .unwrap();
            engine.advance_flows().unwrap();
            assert!(!engine.ledger.data.worker_notices.pending[0].submitted);
            let store = &mut engine.running.get_mut(&scope).unwrap().store;
            assert_eq!(store.draft(Some(&lead)), "My own message");
            store.edit(Some(&lead), String::new());
            engine.advance_flows().unwrap();
            pump_engine(&mut engine, &scope, &rx, &|e| {
                e.ledger.data.worker_notices.pending.is_empty()
            });
            assert_eq!(
                engine.running[&scope]
                    .store
                    .transcript(&lead)
                    .unwrap()
                    .user_messages(&n.text, &[]),
                1
            );
        }

        #[test]
        fn a_read_worker_finishing_its_plan_reports_without_an_operator() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let started = launch_task_on_warm_claude(
                &mut engine,
                &scope,
                &lead,
                &rx,
                None,
                "PLAN-EXIT review",
            )
            .expect("a read worker starts");
            let session = started["session"].as_str().unwrap().to_owned();
            let raw = session.rsplit(':').next().unwrap().to_owned();
            let job = engine.workers.get(&scope, &session).unwrap().origin.job_id;
            pump_engine(&mut engine, &scope, &rx, &|e| {
                e.job_status(job).unwrap()["status"] == "completed"
            });
            // The host turned the exit down itself with the report instruction; no
            // request ever waited for a person.
            let answer: Value = serde_json::from_str(
                &std::fs::read_to_string(root.path().join(format!("plan-exit-{raw}"))).unwrap(),
            )
            .unwrap();
            assert_eq!(answer["response"]["behavior"], "deny");
            assert_eq!(answer["response"]["message"], crate::workers::PLAN_REPORT);
            assert!(engine.running[&scope].store.asks().is_empty());
            let status = engine.job_status(job).unwrap();
            assert!(status["waiting_for_operator"].is_null());
            assert_eq!(status["access_ceiling"], "read");
            assert_eq!(status["started_mode"], "plan");
            assert!(
                status["result"]
                    .as_str()
                    .is_some_and(|r| r.contains("the plan is the report")),
                "{status}"
            );
        }

        #[test]
        fn a_pinned_subscription_worker_runs_its_exact_model_from_the_first_turn() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let started =
                launch_pinned_on_warm_claude(&mut engine, &scope, &lead, &rx, Some("fable"))
                    .expect("an offered model starts");
            let session = started["session"].as_str().unwrap().to_owned();
            // The launch answer already separates the request, the pin and who pays.
            assert_eq!(started["billing"], "subscription");
            assert_eq!(started["requested_model"], "fable");
            assert_eq!(started["pinned_model"], "claude-fable-5-2");
            // The process ran the pinned model from its first turn, in the granted mode.
            let raw = session.rsplit(':').next().unwrap();
            let argv: Vec<String> = serde_json::from_str(
                std::fs::read_to_string(root.path().join(format!("argv-{raw}")))
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap(),
            )
            .unwrap();
            let at = |flag: &str| argv.iter().position(|a| a == flag).map(|i| &argv[i + 1]);
            assert_eq!(at("--model").map(String::as_str), Some("fable"));
            assert_eq!(at("--permission-mode").map(String::as_str), Some("plan"));
            assert!(argv.contains(&"--subscription".to_owned()));
            // The runtime's answer confirms the model, in status and the saved record.
            pump_engine(&mut engine, &scope, &rx, &|e| {
                e.workers
                    .get(&scope, &session)
                    .is_some_and(|w| w.marks.actual_model.is_some() && w.marks.last.is_some())
            });
            let entry = engine.workers.get(&scope, &session).unwrap();
            assert_eq!(
                entry.origin.pinned_model.as_deref(),
                Some("claude-fable-5-2")
            );
            assert_eq!(entry.origin.requested_model.as_deref(), Some("fable"));
            let job = entry.origin.job_id;
            pump_engine(&mut engine, &scope, &rx, &|e| {
                e.job_status(job).unwrap()["model_state"] == "confirmed"
            });
            let status = engine.job_status(job).unwrap();
            assert_eq!(status["actual_model"], "claude-fable-5-2");
            let last = engine
                .workers
                .get(&scope, &session)
                .unwrap()
                .marks
                .last
                .clone();
            assert_eq!(
                last.and_then(|l| l.model).as_deref(),
                Some("claude-fable-5-2")
            );
            // A continuation after the process ended keeps the pin, never the default.
            let live = engine.running.get_mut(&scope).unwrap();
            live.store.end_session(&session).unwrap();
            live.store
                .dispatch(Action::DraftEdit(Some(session.clone()), "again".into()))
                .unwrap();
            live.store
                .dispatch(Action::SendText(Some(session.clone()), "again".into()))
                .unwrap();
            let launches = || {
                std::fs::read_to_string(root.path().join(format!("argv-{raw}")))
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
                    .collect::<Vec<_>>()
            };
            pump_engine(&mut engine, &scope, &rx, &|_| launches().len() >= 2);
            let resumed = &launches()[1];
            assert!(resumed.contains(&"--resume".to_owned()), "{resumed:?}");
            let at = resumed
                .iter()
                .position(|a| a == "--model")
                .expect("pinned on resume");
            assert_eq!(
                resumed[at + 1],
                "claude-fable-5-2",
                "the validated exact ID"
            );
        }

        #[test]
        fn an_unoffered_subscription_model_stops_the_launch_before_any_turn() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let error = launch_pinned_on_warm_claude(
                &mut engine,
                &scope,
                &lead,
                &rx,
                Some("claude-fable-9"),
            )
            .expect_err("an unoffered model never starts");
            assert!(error.contains("does not offer claude-fable-9"), "{error}");
            assert!(error.contains("claude-fable-5-2"), "{error}");
            // No session was stamped and the dedicated process was closed unused.
            assert!(engine.workers.jobs().next().is_none());
            let live = &engine.running[&scope];
            assert_eq!(
                live.store.claude_worker_state(),
                ox_desk_core::store::claude::WorkerBlank::None
            );
            // Its process never received a user turn: nothing ran or billed.
            for file in std::fs::read_dir(root.path()).unwrap().flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_prefix("argv-")
                    && std::fs::read_to_string(file.path())
                        .unwrap()
                        .contains("claude-fable-9")
                {
                    assert!(
                        !live
                            .store
                            .has_live_session(&format!("claude:subscription:{id}"))
                    );
                }
            }
        }

        #[test]
        fn consecutive_workers_start_on_the_leads_claude_route() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            assert!(engine.running[&scope].store.can_send(None));
            assert_eq!(
                engine.running[&scope].store.live_threads(),
                std::slice::from_ref(&lead)
            );
            let mut sessions = Vec::new();
            for _ in 0..2 {
                let done = launch_on_warm_claude(&mut engine, &scope, &lead, &rx)
                    .expect("a worker starts on a warm route");
                sessions.push(done["session"].as_str().unwrap().to_owned());
            }
            assert_ne!(sessions[0], sessions[1]);
            assert!(sessions.iter().all(|s| s != &lead));
        }

        #[test]
        fn continue_in_claude_starts_on_an_already_used_route_without_a_new_draft() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let mut o = origin(workers_parent(&scope, &lead), Ceiling::Read);
            o.runtime = Runtime::Claude;
            o.route = Runtime::Claude.route(None);
            o.relation = Relation::Handoff;
            o.started = Some(ox_desk_core::runtime::Access::Plan);
            let (reply, answered) = std::sync::mpsc::sync_channel(1);
            engine.running.get_mut(&scope).unwrap().launching = Some(Launching {
                origin: o,
                payload: Payload::Handoff {
                    text: "carry on".into(),
                    items: vec![],
                    record: "h1".into(),
                    from: None,
                },
                reply: Replier::Channel(reply),
                stage: Stage::Opening,
                deadline: Instant::now() + Duration::from_secs(60),
                detached: false,
                abandoned: false,
                memory_brief: None,
            });
            let until = Instant::now() + Duration::from_secs(20);
            let result = loop {
                while let Ok(event) = rx.try_recv() {
                    engine.running.get_mut(&scope).unwrap().store.apply(event);
                }
                engine.process(&scope, vec![], false).unwrap();
                if let Ok(result) = answered.try_recv() {
                    break result;
                }
                assert!(Instant::now() < until, "the handoff never started");
                thread::sleep(Duration::from_millis(20));
            };
            let done = result.expect("the handoff target started");
            assert_ne!(done["session"], json!(lead));
        }

        #[test]
        fn a_pending_yolo_request_keeps_its_idle_parent_busy_until_a_confirmed_end() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker, _) = routes(root.path());
            let job = waiting_yolo_request(&mut engine, &parent, &worker);
            engine.running.insert(
                parent.clone(),
                live(
                    &parent,
                    vec![view(&parent, "a", "idle", "yolo")],
                    &[],
                    vec![],
                )
                .0,
            );
            engine.process(&parent, vec![], true).unwrap();
            assert!(engine.thread_busy(&parent, "a"), "an owed decision is busy");
            // The process list and the reaper read the same busy state.
            assert!(
                engine
                    .scope_rows(&parent, &engine.running[&parent], vec!["a".into()])
                    .iter()
                    .all(|p| p.running)
            );
            // End idle and automatic reaping both leave the request for the operator.
            let refused = engine
                .act(1, "e1", &parent, Action::EndSession("a".into()), None)
                .unwrap_err();
            assert!(refused.to_string().contains("busy"), "{refused}");
            engine
                .reap_idle(Instant::now() + Duration::from_secs(86_400))
                .unwrap();
            assert!(engine.asking.contains_key(&job), "reaping kept the request");
            // An explicit, confirmed End withdraws it.
            engine.end_children(&parent, "a").unwrap();
            assert!(!engine.asking.contains_key(&job));
        }

        #[test]
        fn a_workers_final_answer_survives_reaping_and_a_host_restart() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let mut child = live(
                &worker,
                vec![view(&worker, "w", "idle", "read_only")],
                &[],
                vec![],
            )
            .0;
            let mut hist = timeline(&[("final", "assistant", "finished answer")], false);
            hist.detail["last_finished"] = json!("final-turn");
            child.state.transcripts.insert("w".into(), hist);
            engine.running.insert(worker.clone(), child);
            engine.workers.stamp(
                &worker,
                "w",
                origin(workers_parent(&parent, "p"), Ceiling::Read),
            );
            engine.process(&worker, vec![], true).unwrap();
            let before = engine.job_status(41).unwrap();
            assert_eq!(before["status"], "completed");
            assert_eq!(before["result"], "finished answer");
            engine.reap_empty(&worker).unwrap();
            assert!(engine.cold.contains_key(&worker));
            let after = engine.job_status(41).unwrap();
            assert_eq!(after["status"], "completed", "{after}");
            assert_eq!(after["result"], "finished answer");
            // The host restarts: only the ledger on disk remains.
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let restarted = engine.job_status(41).unwrap();
            assert_eq!(restarted["status"], "completed", "{restarted}");
            assert_eq!(restarted["result"], "finished answer");
            // The ledger holds the answer, not the transcript.
            let saved = std::fs::read_to_string(root.path().join(crate::durable::LEDGER)).unwrap();
            assert!(!saved.contains("turn-final"), "no transcript item saved");
        }

        #[test]
        fn continuing_an_old_worker_measures_the_limit_from_the_new_run() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            engine.running.insert(
                worker.clone(),
                live(
                    &worker,
                    vec![view(&worker, "w", "running", "read_only")],
                    &["w"],
                    vec![],
                )
                .0,
            );
            engine.workers.stamp(
                &worker,
                "w",
                origin(workers_parent(&parent, "p"), Ceiling::Read),
            );
            let entry = engine.workers.get_mut(&worker, "w").unwrap();
            entry.marks.max_minutes = Some(60);
            // Launched ten hours ago; the operator continued it a minute ago.
            entry.marks.started_at -= 10 * 3600;
            entry.marks.run_started_at -= 60;
            engine.expire_workers().unwrap();
            assert_eq!(engine.job_status(41).unwrap()["status"], "running");
            // The same worker really running past the limit in this run still expires.
            engine
                .workers
                .get_mut(&worker, "w")
                .unwrap()
                .marks
                .run_started_at -= 3600;
            engine.expire_workers().unwrap();
            assert_eq!(engine.job_status(41).unwrap()["status"], "cancelled");
        }

        #[test]
        fn opening_an_unsent_claude_draft_adds_no_process_to_the_running_list() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, _rx) = warm_claude(root.path());
            let live = engine.running.get_mut(&scope).unwrap();
            live.store.dispatch(Action::NewDraft).unwrap();
            assert!(live.store.can_send(None));
            assert_eq!(live.store.applied_access(None), None);
            engine.process(&scope, vec![], false).unwrap();
            let rows = engine.processes();
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].draft);
            assert_eq!(rows[0].threads, std::slice::from_ref(&lead));
            assert_eq!(engine.running[&scope].store.live_threads(), [lead]);
        }

        #[test]
        fn a_staged_worker_contract_binds_to_its_launch_not_to_the_next_new_session() {
            let root = tempfile::tempdir().unwrap();
            let (scope, _lead, mut engine, rx) = warm_claude(root.path());
            let contract =
                ox_desk_core::claude_worker_contract::Contract::with_support(true, false, true);
            let live = engine.running.get_mut(&scope).unwrap();
            live.store.dispatch(Action::NewDraft).unwrap();
            live.store
                .stage_claude_worker(
                    Some(contract),
                    None,
                    ox_desk_core::runtime::Access::AcceptEdits,
                )
                .unwrap();
            let pump = |engine: &mut Engine, until: &dyn Fn(&Store) -> bool| {
                let end = Instant::now() + Duration::from_secs(10);
                while !until(&engine.running[&scope].store) {
                    assert!(Instant::now() < end, "runtime never became ready");
                    if let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
                        engine.running.get_mut(&scope).unwrap().store.apply(e);
                    }
                }
            };
            use ox_desk_core::store::claude::WorkerBlank;
            pump(&mut engine, &|s| {
                s.can_send(None) && s.claude_worker_state() == WorkerBlank::Ready
            });
            // The lead starts another session in the same window: a normal draft.
            let live = engine.running.get_mut(&scope).unwrap();
            live.store
                .dispatch(Action::SendText(None, "lead again".into()))
                .unwrap();
            let other = live.store.take_opened().unwrap();
            assert_eq!(
                live.store.claude_worker_state(),
                WorkerBlank::Ready,
                "kept for its launch"
            );
            live.store
                .send_on_worker_blank(
                    Action::SendText(None, "worker task".into()),
                    Sender::WorkerTask,
                )
                .unwrap();
            let worker = live.store.take_opened().unwrap();
            assert_ne!(worker, other);
            assert_eq!(live.store.claude_worker_state(), WorkerBlank::None);
            let argv = |id: &str| {
                std::fs::read_to_string(
                    root.path()
                        .join(format!("argv-{}", id.rsplit(':').next().unwrap())),
                )
                .unwrap_or_default()
            };
            let flag = contract.args().into_iter().next().expect("contract args");
            pump(&mut engine, &|s| s.applied_access(Some(&other)).is_some());
            let (w, o) = (argv(&worker), argv(&other));
            assert!(!w.is_empty() && !o.is_empty(), "{w} / {o}");
            assert!(w.contains(&flag), "worker lost its contract: {w}");
            assert!(!o.contains(&flag), "the lead took the worker contract: {o}");
        }

        #[test]
        fn a_worker_keeps_its_launch_contract_on_every_relaunch() {
            use ox_desk_core::claude_worker_contract::Contract;
            use ox_desk_core::store::claude::WorkerBlank;
            let root = tempfile::tempdir().unwrap();
            let (scope, _lead, mut engine, rx) = warm_claude(root.path());
            let contract = Contract::with_support(true, false, true);
            let pump = |engine: &mut Engine, until: &dyn Fn(&Store) -> bool| {
                let end = Instant::now() + Duration::from_secs(10);
                while !until(&engine.running[&scope].store) {
                    assert!(Instant::now() < end, "runtime never became ready");
                    if let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
                        engine.running.get_mut(&scope).unwrap().store.apply(e);
                    }
                }
            };
            let live = engine.running.get_mut(&scope).unwrap();
            live.store.dispatch(Action::NewDraft).unwrap();
            live.store
                .stage_claude_worker(
                    Some(contract),
                    None,
                    ox_desk_core::runtime::Access::AcceptEdits,
                )
                .unwrap();
            pump(&mut engine, &|s| {
                s.can_send(None) && s.claude_worker_state() == WorkerBlank::Ready
            });
            let live = engine.running.get_mut(&scope).unwrap();
            live.store
                .send_on_worker_blank(
                    Action::SendText(None, "worker task".into()),
                    Sender::WorkerTask,
                )
                .unwrap();
            let worker = live.store.take_opened().unwrap();
            let raw = worker.rsplit(':').next().unwrap().to_owned();
            let argv = |id: &str| -> Vec<Vec<String>> {
                std::fs::read_to_string(root.path().join(format!("argv-{id}")))
                    .unwrap_or_default()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect()
            };
            let policy = contract.args();
            let has_policy =
                |argv: &[String]| argv.windows(policy.len()).any(|w| w == policy.as_slice());
            let at = |a: &[String], flag: &str| {
                a.iter().position(|x| x == flag).map(|i| a[i + 1].clone())
            };
            pump(&mut engine, &|s| s.has_live_session(&worker));
            let first = argv(&raw);
            assert_eq!(first.len(), 1, "{first:?}");
            assert!(has_policy(&first[0]), "initial launch: {:?}", first[0]);
            // Let the journal and catalog row exist, then end the worker.
            pump(&mut engine, &|s| s.catalog_row(&worker).is_some());
            let live = engine.running.get_mut(&scope).unwrap();
            live.store.end_session(&worker).unwrap();
            // Cold resume after reap: a follow-up through SendText.
            live.store
                .dispatch(Action::DraftEdit(Some(worker.clone()), "follow-up".into()))
                .unwrap();
            live.store
                .dispatch(Action::SendText(Some(worker.clone()), "follow-up".into()))
                .unwrap();
            let end = Instant::now() + Duration::from_secs(10);
            while argv(&raw).len() < 2 {
                assert!(Instant::now() < end, "no resumed launch");
                if let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
                    engine.running.get_mut(&scope).unwrap().store.apply(e);
                }
            }
            let resumed = argv(&raw);
            assert!(has_policy(&resumed[1]), "resumed launch: {:?}", resumed[1]);
            assert!(resumed[1].contains(&"--resume".to_owned()));
            assert_eq!(
                at(&resumed[1], "--permission-mode").as_deref(),
                Some("acceptEdits")
            );
            assert_eq!(at(&resumed[1], "--tools"), at(&first[0], "--tools"));
            assert_eq!(at(&resumed[1], "--settings"), at(&first[0], "--settings"));
            // Both launches allow the same ordinary workspace command through Bash.
            for a in [&first[0], &resumed[1]] {
                let tools = at(a, "--tools").unwrap();
                let allowed = at(a, "--allowedTools").unwrap();
                assert!(tools.split(',').any(|t| t == "Bash"));
                assert!(allowed.split(',').any(|t| t == "Bash"));
                assert!(
                    at(a, "--settings")
                        .unwrap()
                        .contains("autoAllowBashIfSandboxed")
                );
            }
            // Fork shares the branch: it launches under the source's contract.
            pump(&mut engine, &|s| s.can_manage_thread(&worker));
            let live = engine.running.get_mut(&scope).unwrap();
            live.store
                .dispatch(Action::ForkThread(worker.clone()))
                .unwrap();
            let end = Instant::now() + Duration::from_secs(10);
            while argv(&raw).len() < 3 {
                assert!(Instant::now() < end, "no fork launch");
                if let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
                    engine.running.get_mut(&scope).unwrap().store.apply(e);
                }
            }
            let forked = argv(&raw);
            assert!(
                forked[2].contains(&"--fork-session".to_owned()),
                "{:?}",
                forked[2]
            );
            assert!(has_policy(&forked[2]), "fork launch: {:?}", forked[2]);
            // A host restart: a fresh store knows the worker only from its persisted
            // origin in the ledger, which the host applies when it opens the route.
            let (parent, _) = scopes(root.path());
            let mut stamp = origin(workers_parent(&parent, "p"), Ceiling::Write);
            stamp.runtime = Runtime::Claude;
            stamp.requested = Ceiling::Write;
            engine.workers.stamp(&scope, &worker, stamp);
            let (etx, rx2) = std::sync::mpsc::channel();
            let mut fresh = Store::open_claude(
                ox_desk_core::store::Profile {
                    project: scope.project.clone(),
                    route: scope.route.clone(),
                    provider: ox_desk_core::store::claude::SUBSCRIPTION_PROVIDER.into(),
                },
                ox_desk_core::store::claude::Launcher {
                    ox: Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/worker-claude.py"),
                    path: std::env::var_os("PATH").unwrap(),
                    profiles: ox_desk_core::claude::catalog::Profiles {
                        api: root.path().join("api"),
                        subscription: root.path().join("native"),
                    },
                    index: root.path().join("index.json"),
                    aliases: vec![],
                },
                move |event| {
                    let _ = etx.send(event);
                },
            )
            .unwrap();
            annotate(
                &engine.host_id.to_string(),
                &engine.workers,
                &engine.ledger.data,
                &scope,
                &mut fresh,
            );
            fresh.restore_ended(&worker, "worker", 0, None);
            let end = Instant::now() + Duration::from_secs(10);
            while fresh.catalog_row(&worker).is_none() || !fresh.can_send(None) {
                assert!(Instant::now() < end, "fresh store never listed the worker");
                if let Ok(e) = rx2.recv_timeout(Duration::from_millis(50)) {
                    fresh.apply(e);
                }
            }
            fresh
                .dispatch(Action::DraftEdit(
                    Some(worker.clone()),
                    "after restart".into(),
                ))
                .unwrap();
            // The fork's own process may also log here, so pick the launch that
            // resumes the worker after the restart rather than a fixed position.
            let before = argv(&raw).len();
            fresh
                .dispatch(Action::SendText(
                    Some(worker.clone()),
                    "after restart".into(),
                ))
                .unwrap();
            let resumes = |a: &Vec<String>| {
                a.windows(2).any(|w| w[0] == "--resume" && w[1] == raw)
                    && !a.contains(&"--fork-session".to_owned())
            };
            let end = Instant::now() + Duration::from_secs(10);
            let restarted = loop {
                if let Some(a) = argv(&raw).into_iter().skip(before).find(|a| resumes(a)) {
                    break a;
                }
                assert!(
                    Instant::now() < end,
                    "no resume after restart: {:?}",
                    argv(&raw)
                );
                if let Ok(e) = rx2.recv_timeout(Duration::from_millis(50)) {
                    fresh.apply(e);
                }
            };
            // After a restart the contract is re-derived from the saved origin on
            // this machine: the same shell either way, sandboxed where Claude Code's
            // sandbox can run (CI runners may lack bwrap and socat).
            let here = Contract::detect(true, false).args();
            assert!(
                restarted.windows(here.len()).any(|w| w == here.as_slice()),
                "after restart: {restarted:?}"
            );
            assert_eq!(
                at(&restarted, "--permission-mode").as_deref(),
                Some("acceptEdits")
            );
        }

        #[test]
        fn ending_a_thread_on_a_closed_route_starts_no_process() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            engine.cold.insert(
                scope.clone(),
                crate::durable::SavedScope {
                    scope: scope.clone(),
                    sessions: vec![],
                    pending: vec![],
                }
                .recover(),
            );
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                1,
                ClientState {
                    update_lease: None,
                    peer,
                    version: Some(wire::VERSION),
                    prepared: None,
                    subscription: Some(Some(scope.clone())),
                    catalog: false,
                    watch_resources: false,
                    terminal: false,
                    surface: None,
                },
            );
            for (key, action) in [
                ("e1", Action::EndSession("a".into())),
                ("e2", Action::ForceEndSession("a".into())),
                ("e1", Action::EndSession("a".into())),
            ] {
                engine
                    .request(
                        1,
                        Client::Act {
                            id: key.into(),
                            scope: scope.clone(),
                            action,
                            approval: None,
                        },
                    )
                    .unwrap();
            }
            assert!(
                !engine.running.contains_key(&scope),
                "no runtime was started"
            );
            let sent: Vec<Server> = frames
                .try_iter()
                .map(|p| serde_json::from_slice(&p).unwrap())
                .collect();
            assert!(
                sent.iter().all(|f| matches!(f, Server::Ack { .. })),
                "{sent:?}"
            );
        }

        #[test]
        fn ending_a_root_ends_a_live_grandchild_under_an_exited_middle_parent() {
            let root = tempfile::tempdir().unwrap();
            let (scope, lead, mut engine, rx) = warm_claude(root.path());
            let middle = launch_on_warm_claude(&mut engine, &scope, &lead, &rx).unwrap()["session"]
                .as_str()
                .unwrap()
                .to_owned();
            let leaf = launch_on_warm_claude(&mut engine, &scope, &middle, &rx).unwrap()["session"]
                .as_str()
                .unwrap()
                .to_owned();
            let live =
                |engine: &Engine, id: &str| engine.running[&scope].store.has_live_session(id);
            assert!(live(&engine, &middle) && live(&engine, &leaf));
            // The middle parent exits first: its child is no longer a live child of anything.
            engine
                .running
                .get_mut(&scope)
                .unwrap()
                .store
                .end_session(&middle)
                .unwrap();
            assert!(!live(&engine, &middle) && live(&engine, &leaf));
            engine.end_children(&scope, &lead).unwrap();
            assert!(!live(&engine, &leaf), "the grandchild is still running");
        }

        #[test]
        fn a_launch_already_sent_when_its_parent_is_force_ended_retires_the_child_on_arrival() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            engine.running.insert(
                parent.clone(),
                live(
                    &parent,
                    vec![view(&parent, "a", "idle", "workspace")],
                    &[],
                    vec![],
                )
                .0,
            );
            let (reply, answered) = std::sync::mpsc::sync_channel(1);
            let mut starting = local_live(&worker);
            starting.launching = Some(Launching {
                origin: origin(workers_parent(&parent, "a"), Ceiling::Read),
                payload: Payload::Worker(start(false)),
                reply: Replier::Channel(reply),
                stage: Stage::Sent,
                deadline: Instant::now() + Duration::from_secs(60),
                detached: false,
                abandoned: false,
                memory_brief: None,
            });
            engine.running.insert(worker.clone(), starting);
            engine.end_children(&parent, "a").unwrap();
            let launch = engine
                .running
                .get_mut(&worker)
                .unwrap()
                .launching
                .take()
                .expect("a sent launch cannot be recalled");
            assert!(launch.abandoned);
            // The runtime reports the session after the parent ended.
            engine.workers.stamp(&worker, "w", launch.origin.clone());
            engine
                .finish_launch(
                    &worker,
                    Some(Finished::new(launch, Ok(()), Some("w".into()))),
                )
                .unwrap();
            let err = answered.try_recv().unwrap().unwrap_err();
            assert_eq!(err, ENDED_PARENT);
            assert_eq!(engine.job_status(41).unwrap()["status"], "cancelled");
        }

        #[test]
        fn ending_a_busy_session_needs_force_and_an_idle_one_does_not() {
            let root = tempfile::tempdir().unwrap();
            let (scope, _) = scopes(root.path());
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (running, log) = live(
                &scope,
                vec![view(&scope, "a", "running", "workspace")],
                &["a"],
                vec![],
            );
            engine.running.insert(scope.clone(), running);
            let refused = engine
                .act(1, "e1", &scope, Action::EndSession("a".into()), None)
                .unwrap_err();
            assert!(refused.to_string().contains("busy"), "{refused}");
            assert!(
                !log.lock()
                    .unwrap()
                    .iter()
                    .any(|(a, _)| matches!(a, Action::EndSession(_))),
                "a refused end reaches no runtime"
            );
            // Forced, it passes the guard and only then meets the missing live process.
            let forced = engine
                .act(1, "e2", &scope, Action::ForceEndSession("a".into()), None)
                .unwrap_err();
            assert!(forced.to_string().contains("no live process"), "{forced}");
        }

        #[test]
        fn a_parent_stays_running_while_a_worker_it_started_is_busy() {
            let root = tempfile::tempdir().unwrap();
            let (parent, worker) = scopes(root.path());
            let running_of_parent = |worker_turn: &[&str]| {
                let (tx, _rx) = std::sync::mpsc::sync_channel(64);
                let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
                engine.running.insert(
                    parent.clone(),
                    live(
                        &parent,
                        vec![view(&parent, "a", "idle", "workspace")],
                        &[],
                        vec![],
                    )
                    .0,
                );
                engine.running.insert(
                    worker.clone(),
                    live(
                        &worker,
                        vec![view(&worker, "w", "running", "read_only")],
                        worker_turn,
                        vec![],
                    )
                    .0,
                );
                engine.workers.stamp(
                    &worker,
                    "w",
                    origin(workers_parent(&parent, "a"), Ceiling::Read),
                );
                let live = &engine.running[&parent];
                let rows = engine.scope_rows(&parent, live, vec!["a".into()]);
                rows.first().expect("parent row").running
            };
            // The parent's own turn is done, but its worker is mid-turn on another scope.
            assert!(running_of_parent(&["w"]));
            // Both idle: the parent is not running.
            assert!(!running_of_parent(&[]));
        }

        #[test]
        fn the_reaper_decision_waits_for_the_whole_timeout_and_never_ends_busy_work() {
            let timeout = Duration::from_secs(30 * 60);
            assert!(!reapable(Duration::from_secs(29 * 60), timeout, false));
            assert!(reapable(timeout, timeout, false));
            assert!(reapable(timeout * 2, timeout, false));
            assert!(!reapable(timeout * 2, timeout, true));
            assert!(
                !reapable(timeout * 2, Duration::ZERO, false),
                "zero disables"
            );
            assert_eq!(idle_timeout(None), timeout);
            assert_eq!(idle_timeout(Some("5")), Duration::from_secs(300));
            assert_eq!(idle_timeout(Some(" 0 ")), Duration::ZERO);
            assert_eq!(idle_timeout(Some("soon")), timeout);
        }

        #[test]
        fn an_empty_codex_scope_is_dropped_and_stays_listed_for_a_later_subscriber() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            engine.running.insert(scope.clone(), local_live(&scope));
            assert!(engine.processes().is_empty(), "no thread, no process row");
            // A thread that is not live cannot be ended.
            assert!(engine.end_thread(&scope, "gone", false).is_err());
            // A held handoff keeps the scope open until its subscriber takes it.
            engine.running.get_mut(&scope).unwrap().held_handoff = Some((
                "k".into(),
                Server::Handoff {
                    argv: vec![],
                    id: "k".into(),
                    scope: scope.clone(),
                    thread: None,
                    command: String::new(),
                },
                Instant::now() + Duration::from_secs(600),
            ));
            engine.reap_empty(&scope).unwrap();
            assert!(engine.running.contains_key(&scope));
            engine.running.get_mut(&scope).unwrap().held_handoff = None;
            engine.reap_empty(&scope).unwrap();
            assert!(!engine.running.contains_key(&scope), "app-server dropped");
            assert_eq!(engine.cold[&scope].connection, "disconnected");
        }

        #[test]
        fn idle_reaping_ends_nothing_while_unused_time_is_short() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            engine.running.insert(scope.clone(), local_live(&scope));
            engine.idle = Duration::from_secs(60);
            engine.reap_idle(Instant::now()).unwrap();
            assert!(engine.running.contains_key(&scope));
            // An empty scope idle past the timeout is closed.
            engine
                .reap_idle(Instant::now() + Duration::from_secs(61))
                .unwrap();
            assert!(!engine.running.contains_key(&scope));
        }

        #[test]
        fn a_handoff_held_for_a_gone_client_reaches_the_next_desktop_subscriber_only() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            let mut live = local_live(&scope);
            live.held_handoff = Some((
                "k".into(),
                Server::Handoff {
                    argv: vec!["codex".into()],
                    id: "k".into(),
                    scope: scope.clone(),
                    thread: Some("t".into()),
                    command: String::new(),
                },
                Instant::now() + Duration::from_secs(600),
            ));
            engine.running.insert(scope.clone(), live);
            let client = |terminal| {
                let (peer, frames) = crate::transport::test_peer();
                (
                    ClientState {
                        update_lease: None,
                        peer,
                        version: Some(wire::VERSION),
                        prepared: None,
                        subscription: None,
                        catalog: false,
                        watch_resources: false,
                        terminal,
                        surface: None,
                    },
                    frames,
                )
            };
            let sent = |frames: &std::sync::mpsc::Receiver<crate::transport::Packet>| {
                frames
                    .try_iter()
                    .map(|p| String::from_utf8_lossy(&p).into_owned())
                    .collect::<Vec<_>>()
            };
            // A phone, push or operator subscriber of the scope never takes it.
            let (phone, phone_frames) = client(false);
            engine.clients.insert(8, phone);
            engine.subscribe(8, Some(scope.clone()), None).unwrap();
            assert!(
                !sent(&phone_frames)
                    .iter()
                    .any(|f| f.contains("\"handoff\"")),
                "a non-desktop subscriber received the terminal handoff"
            );
            assert!(engine.running[&scope].held_handoff.is_some());
            let (desktop, frames) = client(true);
            engine.clients.insert(9, desktop);
            engine.subscribe(9, Some(scope.clone()), None).unwrap();
            let sent = sent(&frames);
            assert!(sent.iter().any(|f| f.contains("\"handoff\"")), "{sent:?}");
            assert!(engine.running[&scope].held_handoff.is_none());
        }

        #[test]
        fn an_expired_held_handoff_is_dropped_and_its_scope_reaped() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (scope, _) = scopes(root.path());
            let mut live = local_live(&scope);
            live.held_handoff = Some((
                "k".into(),
                Server::Handoff {
                    argv: vec![],
                    id: "k".into(),
                    scope: scope.clone(),
                    thread: None,
                    command: String::new(),
                },
                Instant::now() + HELD_HANDOFF_TTL,
            ));
            engine.running.insert(scope.clone(), live);
            // A scope-less subscriber never receives it.
            let (peer, frames) = crate::transport::test_peer();
            engine.clients.insert(
                9,
                ClientState {
                    update_lease: None,
                    peer,
                    version: Some(wire::VERSION),
                    prepared: None,
                    subscription: None,
                    catalog: false,
                    watch_resources: false,
                    terminal: false,
                    surface: None,
                },
            );
            engine.subscribe(9, None, None).unwrap();
            assert!(
                !frames
                    .try_iter()
                    .any(|p| String::from_utf8_lossy(&p).contains("\"handoff\""))
            );
            assert!(engine.running[&scope].held_handoff.is_some());
            engine.idle = Duration::from_secs(3600);
            engine.reap_idle(Instant::now()).unwrap();
            assert!(engine.running.contains_key(&scope), "still held");
            engine
                .reap_idle(Instant::now() + HELD_HANDOFF_TTL + Duration::from_secs(1))
                .unwrap();
            assert!(!engine.running.contains_key(&scope), "expired and reaped");
        }

        #[test]
        fn cancelling_a_worker_that_is_still_launching_aborts_the_launch() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let rows = vec![view(&parent, "a", "running", "workspace")];
            engine
                .running
                .insert(parent.clone(), live(&parent, rows, &["a"], vec![]).0);
            let mut starting = local_live(&worker);
            let (reply, answered) = std::sync::mpsc::sync_channel(1);
            starting.launching = Some(Launching {
                origin: origin(workers_parent(&parent, "a"), Ceiling::Read),
                payload: Payload::Worker(start(false)),
                reply: Replier::Channel(reply),
                stage: Stage::Opening,
                deadline: Instant::now() + Duration::from_secs(60),
                detached: false,
                abandoned: false,
                memory_brief: None,
            });
            engine.running.insert(worker.clone(), starting);
            let status = engine
                .parent_call(&hosted(&parent), ParentCall::Status { job_id: 41 })
                .unwrap();
            assert_eq!(status["status"], "starting");
            let done = engine
                .parent_call(&hosted(&parent), ParentCall::Cancel { job_id: 41 })
                .unwrap();
            assert_eq!(done["status"], "cancelled");
            let told = answered.try_recv().expect("the launcher is answered");
            assert!(told.unwrap_err().contains("cancelled"));
            assert!(
                !engine.running.contains_key(&worker),
                "the unused worker runtime is closed"
            );
            assert!(
                engine
                    .unstarted_jobs(&crate::workers::Resolved::Hosted {
                        scope: parent.clone(),
                        session: "a".into(),
                    })
                    .is_empty()
            );
        }

        #[test]
        fn a_launch_whose_message_is_already_sent_cannot_be_cancelled() {
            let root = tempfile::tempdir().unwrap();
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let mut engine = Engine::open(config(root.path()), root.path(), tx).unwrap();
            let (parent, worker) = scopes(root.path());
            let mut sent = local_live(&worker);
            let (reply, _answered) = std::sync::mpsc::sync_channel(1);
            sent.launching = Some(Launching {
                origin: origin(workers_parent(&parent, "a"), Ceiling::Read),
                payload: Payload::Worker(start(false)),
                reply: Replier::Channel(reply),
                stage: Stage::Sent,
                deadline: Instant::now() + Duration::from_secs(60),
                detached: false,
                abandoned: false,
                memory_brief: None,
            });
            engine.running.insert(worker.clone(), sent);
            assert!(engine.abort_launch(41).unwrap().is_none());
            assert!(engine.running[&worker].launching.is_some());
        }
    }
}
