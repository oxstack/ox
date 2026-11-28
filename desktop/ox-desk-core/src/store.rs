//! Connection state and outstanding requests for the new workbench.
//! Each instance belongs to one project and billing route.
use crate::{
    asks::Ask,
    changed::Changed,
    codex::{
        frame::{Frame, RequestId},
        notice::Notification,
        reply::Reply,
        request::Call,
    },
    links::{Link, LinkEvent},
    transcript::Transcript,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

pub mod antigravity;
pub mod claude;
pub mod opencode;
mod projection;
pub mod replica;
mod titles;
pub use crate::runtime::Route;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Profile {
    pub project: PathBuf,
    pub route: Route,
    pub provider: String,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub enum Connection {
    #[default]
    Connecting,
    SignInRequired,
    Ready,
    Closed(String),
}

pub use crate::codex::history::ThreadRow;

#[derive(Debug)]
enum Pending {
    Options(String, routes_thread::Options),
    Limits(u64, u64, Option<crate::usage::reported::Reporter>),
    Discover(commands::Discovery),
    LeaveForTerminal(commands::Handoff),
    /// Best-effort release of a thread this client ended.
    Unsubscribe,
    Descendants(workers::Discovery),
    ChildIdentity(String),
    ThreadOp(thread_ops::PendingOp),
    PaletteSearch(u64),
    Mention(u64),
    Access(String),
    Compose(compose::Operation),
    Initialize,
    Account,
    RefreshAccount,
    List {
        generation: u64,
        cursor: Option<String>,
    },
    Read(String),
    Resume(String),
    History(String),
    ReadFullHistory(String),
    ResumeFullHistory(String),
}

impl Pending {
    fn loads_history_for(&self, id: &str) -> bool {
        match self {
            Self::Read(thread)
            | Self::Resume(thread)
            | Self::History(thread)
            | Self::ReadFullHistory(thread)
            | Self::ResumeFullHistory(thread) => thread == id,
            _ => false,
        }
    }
}

pub struct Store {
    host_opened: Option<String>,
    /// Codex items to inject before the next new thread's first turn.
    handoff_items: Option<Vec<serde_json::Value>>,
    /// Context windows the stock client reported, by model, not yet taken by the host.
    context_windows: Vec<(Option<String>, u64)>,
    /// The next new Claude session's first turn is a handoff block plus request.
    claude_handoff: bool,
    host_annotations: HashMap<String, (Option<String>, serde_json::Value)>,
    naming: Option<titles::Service>,
    authorized_models: HashMap<String, String>,
    next_options: Option<routes_thread::Options>,
    pub usage: crate::usage::Usage,
    archived_threads: HashSet<String>,
    commands: commands::Catalog,
    handoff_ready: Option<commands::Handoff>,
    terminal_threads: HashSet<String>,
    /// Threads whose live process this client ended. Cleared lazily: a thread
    /// that is live again is not reported as ended.
    ended_threads: HashSet<String>,
    /// Ended threads a message is waiting on: reopened, and the draft is sent once ready.
    resume_sends: HashSet<String>,
    /// The host's stamp on the action being dispatched now. Each submit copies it, so a
    /// queued or deferred message keeps the sender that was current when it was accepted.
    sender: Option<crate::surface::Sender>,
    memory: Option<crate::memory::MemoryConfig>,
    memory_attachments: HashMap<String, crate::memory::Attachment>,
    worker_notice_receipts: Vec<WorkerNoticeReceipt>,
    /// The stamp of each message waiting in `resume_sends`.
    resume_commands: HashMap<String, (crate::codex::request::ThreadCommand, bool)>,
    resume_senders: HashMap<String, crate::surface::Sender>,
    /// The Claude access mode an ended session had when its process went away, whether
    /// this client ended it or the host reaped the idle route. A send that reopens the
    /// session starts it in that mode; opening it from the list does not (see
    /// `claude_resume_send`).
    resume_access: HashMap<String, crate::runtime::Access>,
    /// Subscription model chosen while browsing a saved Claude session.
    resume_models: HashMap<String, String>,
    /// OX reserved these new IDs and restored their unacknowledged input on failure.
    /// A deliberate retry can create the same ID if stock never wrote its journal.
    unsaved_claude_drafts: HashSet<String>,
    /// Text the host put back in a session's composer after a send it had acknowledged
    /// failed (identified by the last unsent message). Cleared by the next send.
    restored_drafts: HashMap<String, (String, String)>,
    /// The launch contract of each hosted Claude worker session, kept with the session so
    /// every relaunch (cold resume, resume after a host restart, fork) starts under it.
    worker_contracts: HashMap<String, crate::claude_worker_contract::Contract>,
    /// The exact subscription model each pinned Claude worker session runs, kept like
    /// its contract so a continuation never falls back to the account default.
    worker_models: HashMap<String, String>,
    child_threads: HashMap<String, (String, PathBuf)>,
    /// The mode a new session starts in when nobody chose one: the saved
    /// "New sessions start in" setting, read when the session starts.
    start_mode: fn(crate::runtime::Runtime) -> Option<crate::runtime::Access>,
    next_preset: Option<crate::codex::access::Access>,
    host_start: Option<crate::modes::Level>,
    review_yolo_resumes: bool,
    // False until the host publishes this resume's operator ask.
    held_yolo_resumes: HashMap<String, bool>,
    worker_ceilings: HashMap<String, crate::modes::Level>,
    selection: u64,
    drafts: HashMap<Option<String>, compose::Draft>,
    changed: Changed,
    profile: Profile,
    link: Link,
    pending: HashMap<i64, (Instant, Pending)>,
    seq: i64,
    state: Connection,
    catalog: catalog::Catalog,
    palette_search: palette_search::Search,
    mention: mention_menu::Search,
    front: bool,
    catalog_revision: u64,
    cursor: Option<String>,
    query: String,
    list_generation: u64,
    listing: bool,
    created_while_listing: HashSet<String>,
    notice: Option<String>,
    unknown: u64,
    asks: Vec<Ask>,
    transcripts: HashMap<String, Transcript>,
    selected: Option<String>,
    reopen: Vec<String>,
    /// A thread selected before the connection was ready; it opens as soon as it is.
    queued_open: Option<String>,
}

impl Store {
    /// Consume the verified identity returned by the last new/fork operation.
    /// Independent of UI selection and thread list notifications.
    pub fn take_opened(&mut self) -> Option<String> {
        self.host_opened.take()
    }
    /// Host-owned metadata published with a session row, such as a worker's
    /// origin stamp. `parent` nests the row under a session in another scope.
    pub fn annotate_session(
        &mut self,
        id: &str,
        parent: Option<String>,
        detail: serde_json::Value,
    ) {
        let next = (parent, detail);
        if self.host_annotations.get(id) != Some(&next) {
            self.host_annotations.insert(id.to_owned(), next);
            self.changed.mark(Changed::THREADS);
        }
    }
    /// The context windows the stock client reported since the last call, as
    /// (model, tokens): Claude Code's `result.modelUsage[].contextWindow` and
    /// Codex's `thread/tokenUsage/updated` `modelContextWindow`.
    pub fn take_context_windows(&mut self) -> Vec<(Option<String>, u64)> {
        std::mem::take(&mut self.context_windows)
    }
    pub fn opening_thread(&self) -> bool {
        // A replica waits for the host to report the session it asked for.
        if let Some(remote) = self.remote() {
            return remote.await_new.is_some();
        }
        self.antigravity_opening()
            || self.opening_claude_command()
            || self.pending.values().any(|(_, p)| {
                matches!(p, Pending::Compose(compose::Operation::New(_)))
                    || matches!(p,Pending::ThreadOp(op) if op.is_fork())
            })
    }

    pub fn open(profile: Profile, link: impl Into<Link>) -> io::Result<Self> {
        let mut store = Self::with_link(profile, link.into());
        store.request(Call::Initialize, Pending::Initialize)?;
        Ok(store)
    }
    fn with_link(profile: Profile, link: Link) -> Self {
        Self {
            host_opened: None,
            handoff_items: None,
            context_windows: Vec::new(),
            claude_handoff: false,
            host_annotations: Default::default(),
            naming: None,
            authorized_models: Default::default(),
            next_options: None,
            usage: Default::default(),
            archived_threads: Default::default(),
            commands: Default::default(),
            handoff_ready: None,
            terminal_threads: Default::default(),
            ended_threads: Default::default(),
            resume_sends: Default::default(),
            sender: None,
            memory: None,
            memory_attachments: HashMap::new(),
            worker_notice_receipts: Vec::new(),
            resume_commands: Default::default(),
            resume_senders: Default::default(),
            resume_access: Default::default(),
            resume_models: Default::default(),
            unsaved_claude_drafts: Default::default(),
            restored_drafts: Default::default(),
            worker_contracts: Default::default(),
            worker_models: Default::default(),
            child_threads: Default::default(),
            next_preset: None,
            host_start: None,
            review_yolo_resumes: false,
            held_yolo_resumes: HashMap::new(),
            worker_ceilings: HashMap::new(),
            start_mode: crate::desktop_behavior::start_mode,
            selection: 0,
            drafts: HashMap::new(),
            changed: Changed::default(),
            profile,
            link,
            pending: HashMap::new(),
            seq: 0,
            state: Connection::Connecting,
            catalog: Default::default(),
            palette_search: Default::default(),
            mention: Default::default(),
            front: true,
            catalog_revision: 0,
            cursor: None,
            query: String::new(),
            list_generation: 0,
            listing: false,
            created_while_listing: HashSet::new(),
            notice: None,
            unknown: 0,
            asks: Vec::new(),
            transcripts: HashMap::new(),
            selected: None,
            reopen: vec![],
            queued_open: None,
        }
    }

    pub fn read_limits(&mut self) -> io::Result<()> {
        if self.remote().is_some() {
            if self.state != Connection::Ready
                || self.profile.route != Route::CODEX_SUBSCRIPTION
                || self.usage.loading
            {
                return Ok(());
            }
            self.forward(crate::actions::Action::ReadAccountLimits)?;
            self.usage.requested = true;
            self.usage.loading = true;
            return Ok(());
        }
        if self.state != Connection::Ready
            || self.profile.route != Route::CODEX_SUBSCRIPTION
            || self.usage.loading
        {
            return Ok(());
        }
        self.request(
            Call::Limits,
            Pending::Limits(
                self.usage.version(),
                crate::usage::reported::next_sequence(),
                self.usage.reporter,
            ),
        )?;
        self.usage.requested = true;
        self.usage.loading = true;
        Ok(())
    }
    /// Revalidate thread identity and billing before restoring a subscription.
    pub fn recover_from(&mut self, previous: &mut Self) {
        if self.is_host_client() && previous.is_host_client() {
            if self.profile.project == previous.profile.project
                && self.profile.route == previous.profile.route
            {
                self.drafts = std::mem::take(&mut previous.drafts);
                self.selected.clone_from(&previous.selected);
                self.front = previous.front;
                self.query.clone_from(&previous.query);
                self.recover_replica(previous);
            }
            return;
        }
        if self.profile != previous.profile {
            return;
        }
        if self.is_opencode() {
            self.drafts = std::mem::take(&mut previous.drafts);
            self.selected.clone_from(&previous.selected);
            return;
        }
        self.child_threads = std::mem::take(&mut previous.child_threads);
        self.terminal_threads = std::mem::take(&mut previous.terminal_threads);
        self.ended_threads = previous.ended_threads.clone();
        self.resume_access = std::mem::take(&mut previous.resume_access);
        self.resume_models = std::mem::take(&mut previous.resume_models);
        self.unsaved_claude_drafts = std::mem::take(&mut previous.unsaved_claude_drafts);
        self.restored_drafts = std::mem::take(&mut previous.restored_drafts);
        self.worker_contracts = std::mem::take(&mut previous.worker_contracts);
        self.worker_models = std::mem::take(&mut previous.worker_models);
        self.worker_ceilings = std::mem::take(&mut previous.worker_ceilings);
        self.authorized_models = std::mem::take(&mut previous.authorized_models);
        self.next_options = previous.next_options.take();
        self.selected = previous.selected.clone();
        self.selection = previous.selection;
        self.reopen = previous
            .transcripts
            .iter()
            .filter(|(id, t)| {
                // The selected thread always loads its history, ended or not; an
                // ended thread that is merely open elsewhere stays closed.
                self.selected.as_ref() == Some(*id)
                    || (!previous.ended_threads.contains(*id)
                        && (t.active_turn().is_some() || t.is_loading()))
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.reopen.extend(std::mem::take(&mut previous.reopen));
        self.reopen.sort_unstable();
        self.reopen.dedup();
        self.drafts = std::mem::take(&mut previous.drafts);
        self.next_preset = previous.next_preset;
        self.catalog = std::mem::take(&mut previous.catalog);
        self.front = previous.front;
        self.catalog_revision = previous.catalog_revision + 1;
        self.query = previous.query.clone();
        if self.is_antigravity() {
            self.antigravity_reopen();
        }
        if self.is_session_scoped() && self.state == Connection::Ready {
            self.reopen_claude_history();
        }
    }

    pub fn runtime_name(&self) -> &'static str {
        self.profile.route.runtime.name()
    }
    pub fn profile(&self) -> &Profile {
        &self.profile
    }
    pub fn state(&self) -> &Connection {
        &self.state
    }
    pub fn archived_threads(&self) -> &HashSet<String> {
        &self.archived_threads
    }
    pub fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }
    pub fn threads(&self) -> &[ThreadRow] {
        &self.catalog.rows
    }
    /// Show `text` as the scope's notice, for host flows that warn the operator.
    pub fn post_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
    }
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }
    pub fn asks(&self) -> &[Ask] {
        &self.asks
    }
    pub fn answer(&mut self, id: &RequestId, index: usize) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::AnswerAsk(id.clone(), index));
        }
        let pos =
            self.asks.iter().position(|a| &a.id == id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Request already resolved")
            })?;
        let answer = self.asks[pos]
            .decisions()
            .get(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Decision unavailable"))?;
        let answer = answer.answer.clone();
        if self.is_antigravity() {
            self.antigravity_answer(id, answer.0)?
        } else if matches!(self.link, Link::Claude(_)) {
            self.claude_response(id, answer.0)?
        } else {
            self.link.codex()?.answer(id.clone(), answer)?
        }
        self.asks.remove(pos);
        self.changed.mark(Changed::ASK);
        Ok(())
    }
    pub fn answer_questions(&mut self, id: &RequestId, values: &[String]) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::AnswerQuestions(
                id.clone(),
                values.to_vec(),
            ));
        }
        let pos = self
            .asks
            .iter()
            .position(|ask| &ask.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Request already resolved"))?;
        if matches!(self.link, Link::Claude(_)) {
            self.claude_questions(id, values)?
        } else {
            let answer = self.asks[pos].form_answer(values)?;
            self.link.codex()?.answer(id.clone(), answer)?
        }
        self.asks.remove(pos);
        self.changed.mark(Changed::ASK);
        Ok(())
    }
    pub fn unknown_frames(&self) -> u64 {
        self.unknown
    }
    pub fn has_more(&self) -> bool {
        self.cursor.is_some() && !self.listing
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.remote().is_some() {
            return None;
        }
        self.pending
            .values()
            .map(|(when, _)| *when)
            .chain(self.native_deadline())
            .chain(self.antigravity_deadline())
            .min()
    }

    fn request(&mut self, call: Call, pending: Pending) -> io::Result<()> {
        self.seq += 1;
        if let Err(err) = self.link.codex()?.request(self.seq, call) {
            self.failed(pending, err.to_string());
            return Err(err);
        }
        self.pending.insert(
            self.seq,
            (Instant::now() + Duration::from_secs(90), pending),
        );
        Ok(())
    }

    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn search(&mut self, query: String) -> io::Result<()> {
        if self.remote().is_some() {
            if self.query == query {
                return Ok(());
            }
            self.query = query.clone();
            self.remote_mut().restore = None;
            return self.forward(crate::actions::Action::SearchThreads(query));
        }
        if self.query == query {
            return Ok(());
        }
        self.query = query;
        self.list_generation += 1;
        self.cursor = None;
        self.listing = false;
        self.catalog.clear();
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
        self.list_more()
    }
    fn stale_page(&self, pending: &Pending) -> bool {
        matches!(pending,Pending::List{generation,..} if *generation!=self.list_generation)
            || matches!(pending, Pending::PaletteSearch(generation) if *generation != self.palette_search.generation)
    }
    pub fn list_more(&mut self) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::MoreThreads);
        }
        if self.is_antigravity() {
            return self.antigravity_list();
        }
        if matches!(self.link, Link::Claude(_)) {
            self.claude_list();
            return Ok(());
        }
        if self.listing || !matches!(self.state, Connection::Ready | Connection::SignInRequired) {
            return Ok(());
        }
        self.request(
            Call::List {
                project: self.profile.project.clone(),
                query: self.query.clone(),
                cursor: self.cursor.clone(),
            },
            Pending::List {
                generation: self.list_generation,
                cursor: self.cursor.clone(),
            },
        )?;
        self.listing = true;
        Ok(())
    }

    pub fn tick(&mut self, now: Instant) {
        if self.remote().is_some() {
            return;
        }
        if self.is_antigravity() {
            self.antigravity_tick(now);
            return;
        }
        self.refresh_native(now, false);
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (when, _))| *when <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some((_, pending)) = self.pending.remove(&id) {
                self.link.forget(id);
                self.failed(pending, "Codex request timed out".into());
            }
        }
    }

    fn failed(&mut self, pending: Pending, message: String) {
        if self.stale_page(&pending) {
            return;
        }
        self.signal_reply(&pending);
        match pending {
            Pending::Options(id, _) => {
                let expected = self.expected_alias(&id).map(str::to_owned);
                let transcript = self.transcripts.entry(id).or_default();
                if expected.is_some() && expected != transcript.model {
                    transcript.ready = false;
                    transcript.verified = false;
                }
                transcript.error = Some(message);
            }
            Pending::Limits(..) => {
                self.usage.loading = false;
                self.usage.error = Some(message);
            }
            Pending::Mention(version) => {
                if version == self.mention.version {
                    self.mention.error = Some(message);
                }
            }
            Pending::PaletteSearch(_) => self.palette_search.failed(message),
            Pending::ThreadOp(_) => self.notice = Some(message),
            Pending::ChildIdentity(_) => (), // A child can exit before its metadata arrives.
            Pending::Descendants(_) => {
                self.notice = Some(message);
            }
            Pending::LeaveForTerminal(handoff) => {
                if let Some(id) = handoff.thread {
                    self.terminal_threads.remove(&id);
                }
                self.notice = Some(message);
            }
            Pending::Unsubscribe => (),
            Pending::Discover(stage) => self.discovery_failed(stage, message),
            Pending::Compose(op) => self.compose_failed(op, message),
            Pending::RefreshAccount => (),
            Pending::Initialize | Pending::Account => self.state = Connection::Closed(message),
            Pending::Access(id) => {
                if let Some(announced) = self.held_yolo_resumes.get_mut(&id) {
                    *announced = false;
                }
                self.transcripts.entry(id).or_default().error = Some(message);
            }
            Pending::Resume(id) | Pending::ResumeFullHistory(id)
                if message.contains(Self::WRITER_LOCK) =>
            {
                self.native_locked(id)
            }
            Pending::Read(id)
            | Pending::Resume(id)
            | Pending::History(id)
            | Pending::ReadFullHistory(id)
            | Pending::ResumeFullHistory(id) => {
                let transcript = self.transcripts.entry(id).or_default();
                transcript.loading = false;
                transcript.before_resume.clear();
                transcript.error = Some(message);
            }
            Pending::List { .. } => {
                self.listing = false;
                self.notice = Some(message);
            }
        }
    }

    fn signal_reply(&mut self, pending: &Pending) {
        match pending {
            Pending::Mention(_)
            | Pending::LeaveForTerminal(_)
            | Pending::Discover(_)
            | Pending::Limits(..) => self.changed.mark(Changed::NOTICE),
            Pending::Compose(op) => {
                self.changed.mark(Changed::THREADS);
                if let Some(id) = op.thread() {
                    self.changed.transcript(id);
                }
            }
            Pending::ThreadOp(_) => self.changed.mark(Changed::THREADS),
            Pending::Unsubscribe => (),
            Pending::Initialize | Pending::Account | Pending::RefreshAccount => {
                self.changed.mark(Changed::CONNECTION)
            }
            Pending::PaletteSearch(_)
            | Pending::ChildIdentity(_)
            | Pending::List { .. }
            | Pending::Descendants(_) => self.changed.mark(Changed::THREADS),
            Pending::Options(id, _)
            | Pending::Access(id)
            | Pending::Read(id)
            | Pending::Resume(id)
            | Pending::History(id)
            | Pending::ReadFullHistory(id)
            | Pending::ResumeFullHistory(id) => self.changed.transcript(id),
        }
    }

    pub fn drain(&mut self, batch: Vec<LinkEvent>, now: Instant) -> Changed {
        for event in batch {
            self.apply(event);
        }
        self.tick(now);
        self.flush_resume_sends();
        std::mem::take(&mut self.changed)
    }

    pub fn apply(&mut self, event: LinkEvent) {
        if let Err(err) = self.handle(event) {
            self.changed.mark(Changed::NOTICE);
            self.notice = Some(err.to_string());
        }
    }

    fn handle(&mut self, event: LinkEvent) -> io::Result<()> {
        let frame = match event {
            LinkEvent::Host(frame) => return self.apply_host(*frame),
            LinkEvent::Frame(frame) => frame,
            LinkEvent::Title { key, name } => {
                self.generated_title(key, name);
                return Ok(());
            }
            LinkEvent::Antigravity(event) => return self.antigravity_event(event),
            LinkEvent::Claude(update) => return self.claude_event(update),
            LinkEvent::OpenCode(event) => return self.opencode_event(event),
            LinkEvent::SignInRequired => {
                self.state = Connection::SignInRequired;
                self.changed.mark(Changed::CONNECTION);
                return Ok(());
            }
            LinkEvent::Closed {
                reason,
                stderr_tail,
            } => {
                let reason = crate::links::close_reason(&reason, &stderr_tail);
                let sign_in = self.state == Connection::SignInRequired;
                self.changed.mark(Changed::CONNECTION | Changed::ASK);
                for (_, (_, pending)) in std::mem::take(&mut self.pending) {
                    self.failed(pending, reason.clone());
                }
                self.state = if sign_in {
                    Connection::SignInRequired
                } else {
                    Connection::Closed(reason.clone())
                };
                self.asks.clear();
                self.listing = false;
                for row in &mut self.catalog.rows {
                    row.status = None;
                }
                for (id, thread) in &mut self.transcripts {
                    let was_busy = thread.active_turn.take().is_some() || thread.is_loading();
                    if self.selected.as_ref() == Some(id)
                        || (was_busy && !self.ended_threads.contains(id))
                    {
                        self.reopen.push(id.clone());
                    }
                    thread.verified = false;
                    thread.ready = false;
                    thread.loading = false;
                    thread.access = None;
                    thread.ceiling = None;
                }
                return Ok(());
            }
        };
        match frame {
            Frame::Reply {
                id: RequestId::Number(id),
                result,
            } => {
                let Some((_, pending)) = self.pending.remove(&id) else {
                    return Ok(());
                };
                if self.stale_page(&pending) {
                    return Ok(());
                }
                self.signal_reply(&pending);
                let value = match result {
                    Ok(v) => v,
                    Err(err) => {
                        self.rpc_failed(pending, err);
                        return Ok(());
                    }
                };
                if self.profile.route == Route::CODEX_SUBSCRIPTION
                    && let Reply::Account(account) = &value
                {
                    self.usage.codex_account(account);
                }
                match (pending, value) {
                    (Pending::LeaveForTerminal(handoff), Reply::Unsubscribed(_)) => {
                        self.handoff_ready = Some(handoff);
                    }
                    (Pending::Unsubscribe, _) => (),
                    (Pending::ThreadOp(op), reply) => self.finish_thread_op(op, reply)?,
                    (Pending::ChildIdentity(id), Reply::Read(read, _)) => {
                        self.observe_child_identity(&id, *read.thread);
                    }
                    (Pending::Descendants(discovery), Reply::Threads(page)) => {
                        self.discovered(discovery, page)?
                    }
                    (Pending::Discover(stage), reply) => self.discovery_reply(stage, reply)?,
                    (Pending::Access(_), Reply::Acknowledged) => (),
                    (Pending::Options(id, options), Reply::Acknowledged) => {
                        self.apply_options(&id, options)
                    }
                    (Pending::Compose(op), reply) => self.compose_reply(op, reply)?,
                    (Pending::Initialize, Reply::UnsupportedVersion(message)) => {
                        self.state = Connection::Closed(message)
                    }
                    (Pending::Initialize, Reply::Acknowledged) => {
                        self.link.codex()?.initialized()?;
                        if self.profile.route == Route::CODEX_SUBSCRIPTION {
                            self.request(Call::Account, Pending::Account)?;
                        } else {
                            self.connected()?;
                        }
                    }
                    (Pending::Limits(version, started, account), Reply::Limits(limits)) => {
                        self.usage.loading = false;
                        if account == self.usage.reporter {
                            self.usage.read(*limits, version, started)
                        }
                    }
                    (Pending::Account | Pending::RefreshAccount, Reply::Account(account))
                        if !account.chatgpt() =>
                    {
                        self.state = Connection::SignInRequired;
                        self.list_more()?;
                    }
                    (Pending::Account, Reply::Account(_)) => self.connected()?,
                    (Pending::RefreshAccount, Reply::Account(_)) => (),
                    (Pending::Read(id), Reply::Read(read, _)) => self.checked_read(&id, read),
                    (Pending::Resume(id), Reply::Resume(reply, _)) => {
                        self.checked_resume(&id, *reply)
                    }
                    (Pending::ResumeFullHistory(id), Reply::Resume(mut reply, Some(page))) => {
                        reply.initial_turns_page = Some(page);
                        self.checked_resume(&id, *reply);
                    }
                    (Pending::ReadFullHistory(id), Reply::Read(read, Some(page))) => {
                        self.full_history_read(id, read, page);
                    }
                    (pending @ Pending::ReadFullHistory(_), Reply::Read(_, None))
                    | (pending @ Pending::ResumeFullHistory(_), Reply::Resume(_, None)) => {
                        self.failed(
                            pending,
                            "Codex did not return the requested thread history".into(),
                        );
                    }
                    (Pending::History(id), Reply::Turns(page)) => self.history_page(&id, page),
                    (Pending::Mention(version), Reply::Files(files)) => {
                        if version == self.mention.version {
                            self.mention.paths = files
                                .files
                                .into_iter()
                                .filter_map(|f| {
                                    f.in_project(&self.profile.project)
                                        .map(std::path::Path::to_owned)
                                })
                                .collect();
                        }
                    }
                    (Pending::PaletteSearch(_), Reply::Threads(page)) => {
                        self.palette_search.add(&self.profile.project, page)
                    }
                    (Pending::List { cursor, .. }, Reply::Threads(page)) => {
                        self.listing = false;
                        self.catalog_revision += 1;
                        if cursor.is_none() {
                            // Rows the listing returned stay, and so do the workers of those
                            // parents (they live in subdirectories the page loop skips).
                            let mut retained = self.created_while_listing.clone();
                            // A thread the host knows but discovery has not listed yet
                            // (a fresh one, or one reaped before it was indexed) stays
                            // resumable by a send.
                            retained.extend(self.ended_threads.iter().cloned());
                            retained.extend(self.resume_sends.iter().cloned());
                            retained.extend(
                                page.data
                                    .iter()
                                    .filter(|row| row.cwd == self.profile.project)
                                    .map(|row| row.id.clone()),
                            );
                            self.catalog.keep(&retained);
                        }
                        self.created_while_listing.clear();
                        for row in page.data {
                            if row.cwd == self.profile.project {
                                if self.catalog.get(&row.id).is_some() {
                                    self.sync_title(&row);
                                } else {
                                    self.record_lineage(&row);
                                    self.catalog.insert(row);
                                }
                            }
                        }
                        if cursor.is_some() && cursor == page.next_cursor {
                            self.cursor = None;
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Codex repeated a thread list cursor",
                            ));
                        }
                        self.cursor = page.next_cursor;
                    }
                    (pending, _) => self.failed(pending, "Unexpected Codex response type".into()),
                }
            }
            Frame::Activity(activity) => {
                if matches!(
                    activity.change,
                    crate::codex::activity::Change::TurnCompleted { error: None, .. }
                ) {
                    self.title_after_turn(&activity.thread);
                }

                if let crate::codex::activity::Change::Item { row, .. } = &activity.change
                    && let crate::transcript::item::Item::AgentOperation {
                        tool,
                        sender_thread_id,
                        receiver_thread_ids,
                        ..
                    } = &row.item
                    && tool == "spawnAgent"
                    && sender_thread_id == &activity.thread
                    && receiver_thread_ids
                        .iter()
                        .any(|id| !self.child_threads.contains_key(id))
                {
                    for id in receiver_thread_ids {
                        self.read_missing_child(id)?;
                    }
                    self.discover_children(&activity.thread)?;
                }
                self.catalog_activity(&activity);
                if let Some(t) = self.transcripts.get_mut(&activity.thread) {
                    if t.verified && t.ready {
                        let active = t.active_turn.is_some();
                        let completed = matches!(
                            activity.change,
                            crate::codex::activity::Change::TurnCompleted { .. }
                        );
                        let unread = (!self.front
                            || self.selected.as_deref() != Some(activity.thread.as_str()))
                            && !t.unread;
                        t.unread |= unread;
                        self.changed.transcript(&activity.thread);
                        t.apply_activity(activity);
                        if active != t.active_turn.is_some() || unread || completed {
                            self.changed.mark(Changed::THREADS);
                        }
                    } else if t.loading {
                        t.before_resume.push(activity);
                    }
                }
            }
            Frame::InvalidNotice { method, reason } => {
                self.changed.mark(Changed::NOTICE);
                self.unknown += 1;
                self.notice = Some(format!("Could not read {}: {reason}", method.as_str()));
            }
            Frame::Unknown { id, .. } => {
                self.unknown += 1;
                if let Some(id) = id {
                    self.link
                        .codex()?
                        .reject(id, -32601, "Unsupported request")?;
                }
            }
            Frame::Ask(ask) => {
                if ask.decisions().is_empty() && ask.questions.is_empty() {
                    self.link.codex()?.reject(
                        ask.id,
                        -32601,
                        "This desktop cannot render the requested form",
                    )?;
                } else if !self.asks.iter().any(|a| a.id == ask.id) {
                    self.changed.mark(Changed::ASK);
                    self.asks.push(ask);
                }
            }
            Frame::Notice(notice) => self.notification(notice)?,
            Frame::Reply { .. } => (),
        }
        Ok(())
    }
    fn connected(&mut self) -> io::Result<()> {
        self.state = Connection::Ready;
        self.list_more()?;
        let reopened = std::mem::take(&mut self.reopen);
        for id in &reopened {
            if self.in_terminal(id) {
                continue;
            }
            self.request(Call::Read(id.clone()), Pending::Read(id.clone()))?;
            self.mark_loading(id);
        }
        if let Some(id) = self.queued_open.take()
            && self.selected.as_deref() == Some(id.as_str())
            && !reopened.contains(&id)
            && !self.in_terminal(&id)
        {
            if let Some(t) = self.transcripts.get_mut(&id) {
                t.loading = false;
            }
            self.open_catalog_id(&id)?;
        }
        Ok(())
    }
}

mod history;

mod compose;
pub use compose::WorkerNoticeReceipt;

mod notifications;

mod catalog;

mod commands;
mod mention_menu;
mod palette_search;

mod workers;

mod thread_ops;
pub(crate) use thread_ops::ThreadOperation;

pub mod routes_thread;
