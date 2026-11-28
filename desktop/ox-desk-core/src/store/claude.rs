//! A Claude billing route owns a catalog and an independent stock process for each
//! open session. The session key and launch generation accompany every callback.
mod events;
mod history;
mod native;

use super::{Connection, Profile, Store, ThreadRow, catalog::now_secs};
use crate::{
    changed::Changed,
    claude::{
        self as wire,
        catalog::{Bindings, Origin, Profiles},
    },
    links::{
        self, LinkEvent,
        claude::{self as pipe, Session},
    },
    runtime::{Access, Billing, RequestId, Runtime},
    transcript::item::{Content, Item},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsString,
    fmt,
    fs::File,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    thread,
};

pub const SUBSCRIPTION_PROVIDER: &str = "claude-subscription";
pub const API_PROVIDER: &str = "claude-ox-api";

pub struct Launcher {
    pub ox: PathBuf,
    pub path: OsString,
    pub profiles: Profiles,
    pub index: PathBuf,
    pub aliases: Vec<String>,
}

pub struct Project {
    launcher: Arc<Launcher>,
    deliver: Arc<dyn Fn(LinkEvent) + Send + Sync>,
    sessions: HashMap<String, Process>,
    catalog: HashMap<String, ThreadRow>,
    asks: HashMap<RequestId, (String, wire::Permission)>,
    /// Routes the synchronous dispatch of a worker's first input to its already verified process.
    dispatching_worker: Option<String>,
    /// The worker contract the next new session starts with; one launch consumes it.
    worker: Option<crate::claude_worker_contract::Contract>,
    /// The process reserved for one worker or handoff until it can accept input.
    worker_blank: Option<String>,
    /// The exact subscription model the next worker process is pinned to with `--model`.
    worker_model: Option<String>,
    serial: u64,
    native: native::Native,
}
#[derive(Default)]
struct Process {
    generation: u64,
    events: events::EventState,
    handoff: Option<super::commands::Handoff>,
    move_release: Option<pipe::ReapStatus>,
    link: Option<pipe::Link>,
    initialized: bool,
    limit_reporter: Option<crate::usage::reported::Reporter>,
    /// The first `system/init` matched the project. Later ones report the Bash
    /// tool's current directory, which follows `cd`, so only the first is checked.
    project_checked: bool,
    capabilities: Option<crate::slash::Capabilities>,
    mode: Option<Access>,
    controls: HashMap<String, Control>,
    first: Option<(String, String)>,
    deferred_command: Option<DeferredCommand>,
    /// The sender statement of `first`, fixed when the host accepted that message. It
    /// waits with the message while the process starts, and goes out with it.
    first_context: Option<String>,
    memory_attachment: Option<(String, crate::memory::Attachment)>,
    /// Native message UUID and durable host notice id until the CLI echoes it.
    worker_notice: Option<(String, u64)>,
    /// The permission mode this process was launched with.
    launch_mode: Option<String>,
    /// The explicit subscription model to reuse if a turn needs to be retried.
    selected_model: Option<String>,
    started_new: bool,
    memory_brief_pending: bool,
    /// Reading a saved journal, optionally following a live external client.
    viewer: Option<native::Viewer>,
    /// Models that answered top-level turns in this process, in first-answer order.
    answering: Vec<String>,
}
struct DeferredCommand {
    new_thread: bool,
    terminal_allowed: bool,
}
enum Control {
    Mode(Access),
    Model(String),
    Interrupt,
}

pub struct Update {
    owner: String,
    generation: u64,
    data: Data,
}
impl fmt::Debug for Update {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeUpdate")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
enum Data {
    Connected(pipe::Link),
    Stock(pipe::Event),
    History(history::Batch),
    /// Appended journal rows of a read-only viewer, to apply in place.
    Tail(history::Tail),
    /// A viewer's read finished; the follower resumes from where it stopped.
    Viewed(Option<Box<history::Follower>>),
    Failed(String),
}
impl Update {
    pub(crate) fn unblocks(&self) -> bool {
        !matches!(
            self.data,
            Data::History(_)
                | Data::Tail(_)
                | Data::Stock(pipe::Event::Frame(
                    wire::Event::Delta { .. } | wire::Event::Item { .. }
                ))
        )
    }
}

fn new_id() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let n = u128::from_be_bytes(bytes);
    Ok(format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        n >> 96,
        (n >> 80) & 0xffff,
        (n >> 64) & 0xffff,
        (n >> 48) & 0xffff,
        n & 0xffffffffffff
    ))
}
fn session_key(billing: &Billing, id: &str) -> String {
    let scope = match billing {
        Billing::Subscription => "subscription",
        Billing::Api(_) => "api",
    };
    format!("claude:{scope}:{id}")
}
fn stock_id(key: &str) -> io::Result<&str> {
    key.split(':')
        .nth(2)
        .filter(|id| wire::catalog::valid_session_id(id))
        .ok_or_else(|| io::Error::other("Invalid Claude session ID"))
}
fn child_key(root: &str, tool: &str) -> String {
    format!("{root}/{tool}")
}
fn row(profile: &Profile, id: String, title: String, updated_at: i64) -> ThreadRow {
    ThreadRow {
        id,
        name: None,
        generated_title: None,
        preview: title,
        cwd: profile.project.clone(),
        model_provider: profile.provider.clone(),
        model: match &profile.route.billing {
            Billing::Api(alias) => Some(alias.clone()),
            Billing::Subscription => None,
        },
        agent_nickname: None,
        updated_at,
        parent_thread_id: None,
        worker: false,
        unread: false,
        status: None,
        activity_order: 0,
    }
}
/// Read session metadata in both native profiles without running either client.
pub fn saved_rows(profiles: &Profiles, index: &std::path::Path) -> io::Result<Vec<ThreadRow>> {
    let aliases = Bindings::new(index.into()).aliases(&profiles.api)?;
    Ok(profiles
        .all()?
        .into_iter()
        .map(|s| saved_row(s, &aliases))
        .collect())
}

fn saved_row(
    saved: wire::catalog::Session,
    aliases: &HashMap<(PathBuf, String), String>,
) -> ThreadRow {
    let api = saved.origin == Origin::Api;
    let billing = if api {
        Billing::Api(String::new())
    } else {
        Billing::Subscription
    };
    let id = session_key(&billing, &saved.id);
    let model = if api {
        aliases
            .get(&(saved.project.clone(), saved.id))
            .cloned()
            .or(saved.model)
    } else {
        None
    };
    let profile = Profile {
        project: saved.project,
        route: Runtime::Claude.route(None),
        provider: if api {
            API_PROVIDER
        } else {
            SUBSCRIPTION_PROVIDER
        }
        .into(),
    };
    let mut result = row(&profile, id, saved.preview, saved.updated_at);
    result.name = saved.native_title;
    result.model = model;
    result
}

impl Launcher {
    fn saved_session(
        &self,
        profile: &Profile,
        source: &str,
        confirm_missing_alias: bool,
    ) -> io::Result<wire::catalog::Session> {
        let origin = if profile.route.is_api() {
            Origin::Api
        } else {
            Origin::Subscription
        };
        let saved = self.profiles.session(&profile.project, source, origin)?;
        if let Billing::Api(alias) = &profile.route.billing {
            let index = Bindings::new(self.index.clone());
            let bound = index
                .alias(&profile.project, &self.profiles.api, source)?
                .or(saved.model.clone());
            match bound.as_deref() {
                Some(bound) if bound == alias => (),
                None if confirm_missing_alias => {
                    index.bind(&profile.project, &self.profiles.api, source, alias)?
                }
                _ => {
                    return Err(io::Error::other(
                        "This session was saved with a different model alias. Choose the saved alias in the sidebar to resume it.",
                    ));
                }
            }
        }
        Ok(saved)
    }
    fn catalog(&self, profile: &Profile) -> io::Result<HashMap<String, ThreadRow>> {
        let aliases = Bindings::new(self.index.clone()).aliases(&self.profiles.api)?;
        let mut rows = HashMap::new();
        for saved in self.profiles.list(&profile.project)? {
            // A UUID in both profiles belongs to the explicitly selected billing route.
            let saved = if saved.origin == Origin::Ambiguous {
                self.profiles.session(
                    &profile.project,
                    &saved.id,
                    if profile.route.is_api() {
                        Origin::Api
                    } else {
                        Origin::Subscription
                    },
                )?
            } else {
                saved
            };
            let row = saved_row(saved, &aliases);
            rows.insert(row.id.clone(), row);
        }
        Ok(rows)
    }
}

impl ThreadRow {
    pub fn saved_claude_api(&self) -> Option<bool> {
        match self.model_provider.as_str() {
            SUBSCRIPTION_PROVIDER => Some(false),
            API_PROVIDER => Some(true),
            _ => None,
        }
    }

    pub fn saved_runtime_route(&self, aliases: &[String]) -> Option<crate::runtime::Route> {
        match self.model_provider.as_str() {
            SUBSCRIPTION_PROVIDER => Some(Runtime::Claude.route(None)),
            API_PROVIDER => self
                .model
                .as_ref()
                .filter(|model| aliases.contains(model))
                .map(|alias| Runtime::Claude.route(Some(alias.clone()))),
            _ => None,
        }
    }
    pub fn needs_claude_alias(&self) -> bool {
        self.model_provider == API_PROVIDER && self.model.is_none()
    }
}
impl Access {
    fn mode(self) -> Option<&'static str> {
        match self {
            Self::Default => Some("default"),
            Self::Auto => Some("auto"),
            Self::Plan => Some("plan"),
            Self::AcceptEdits => Some("acceptEdits"),
            Self::BypassPermissions => Some("bypassPermissions"),
            _ => None,
        }
    }
    fn from_mode(name: &str) -> Option<Self> {
        Self::choices(Runtime::Claude)
            .iter()
            .copied()
            .find(|a| a.mode() == Some(name))
    }
}
/// A launch's dedicated worker process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerBlank {
    None,
    Starting,
    Ready,
    /// The process failed to start or exited before the first send.
    Lost,
}
impl Store {
    pub fn is_session_scoped(&self) -> bool {
        if self.remote().is_some() {
            return self.profile.route.runtime == Runtime::Claude;
        }
        matches!(self.link, links::Link::Claude(_))
    }
    /// A worker's first input must go to the process started with its contract.
    pub fn stage_claude_worker(
        &mut self,
        contract: Option<crate::claude_worker_contract::Contract>,
        model: Option<String>,
        mode: Access,
    ) -> io::Result<()> {
        if !matches!(self.link, links::Link::Claude(_)) {
            return Ok(());
        }
        if model.is_some() && self.profile.route.is_api() {
            return Err(io::Error::other(
                "A subscription model cannot be pinned on an API billing route.",
            ));
        }
        self.drop_claude_worker();
        let id = new_id()?;
        let preset = self.next_preset.replace(mode);
        let options = self.next_options.take();
        self.claude_mut().worker = contract;
        self.claude_mut().worker_model = model;
        let spawned = self.spawn_claude(Session::New(id), None, false);
        self.next_preset = preset;
        self.next_options = options;
        let p = self.claude_mut();
        p.worker = None;
        p.worker_model = None;
        let owner = spawned?;
        self.claude_mut().worker_blank = Some(owner);
        Ok(())
    }
    /// The mode reported by stock on this launch's own process.
    pub fn claude_worker_access(&self) -> Option<Access> {
        let project = self.claude();
        project.sessions.get(project.worker_blank.as_ref()?)?.mode
    }

    pub fn set_claude_worker_access(&mut self, preset: Access) -> io::Result<()> {
        let owner = self
            .claude()
            .worker_blank
            .clone()
            .ok_or_else(|| io::Error::other("The worker's runtime is not prepared"))?;
        self.claude_mode(&owner, preset)
    }

    /// The account's picker as the worker's own process reported it at initialization,
    /// in the runtime's order. None until that process has initialized.
    pub fn claude_worker_picker(&self) -> Option<Vec<crate::claude_models::PickerModel>> {
        if !matches!(self.link, links::Link::Claude(_)) {
            return None;
        }
        let p = self.claude();
        let s = p.sessions.get(p.worker_blank.as_deref()?)?;
        let caps = s.capabilities.as_ref().filter(|_| s.initialized)?;
        let mut rows: Vec<_> = caps.models.iter().collect();
        rows.sort_by_key(|(value, info)| (info.rank.unwrap_or(u32::MAX), value.as_str()));
        Some(
            rows.into_iter()
                .map(|(value, info)| crate::claude_models::PickerModel {
                    value: value.clone(),
                    resolved: info.resolved.clone(),
                    display: info.label.clone(),
                    description: String::new(),
                })
                .collect(),
        )
    }
    /// The models that answered top-level turns of a live Claude session, first one first.
    pub fn claude_answering_models(&self, id: &str) -> &[String] {
        if !matches!(self.link, links::Link::Claude(_)) {
            return &[];
        }
        self.claude()
            .sessions
            .get(id)
            .map_or(&[], |s| s.answering.as_slice())
    }
    /// Where the worker's dedicated process stands.
    pub fn claude_worker_state(&self) -> WorkerBlank {
        if !matches!(self.link, links::Link::Claude(_)) {
            return WorkerBlank::None;
        }
        let p = self.claude();
        match p.worker_blank.as_deref() {
            None => WorkerBlank::None,
            Some(k) => match p.sessions.get(k) {
                None => WorkerBlank::Lost,
                Some(s) if s.initialized && s.link.is_some() && s.controls.is_empty() => {
                    WorkerBlank::Ready
                }
                Some(_) => WorkerBlank::Starting,
            },
        }
    }
    /// Send a launch's first turn on its dedicated process and nowhere else.
    pub fn send_on_worker_blank(
        &mut self,
        action: crate::actions::Action,
        sender: crate::surface::Sender,
    ) -> io::Result<()> {
        let Some(owner) = self.claude_mut().worker_blank.take() else {
            return Err(io::Error::other("The worker's runtime is not prepared"));
        };
        self.claude_mut().dispatching_worker = Some(owner.clone());
        let result = self.dispatch_as(action, sender);
        let p = self.claude_mut();
        p.dispatching_worker = None;
        if result.is_err() {
            p.sessions.remove(&owner);
        }
        result.map(|_| ())
    }
    /// Close a dedicated process its launch did not use.
    pub fn drop_claude_worker(&mut self) {
        if let links::Link::Claude(p) = &mut self.link {
            p.worker = None;
            p.worker_model = None;
            if let Some(owner) = p.worker_blank.take() {
                p.sessions.remove(&owner);
            }
        }
    }
    pub(super) fn claude_applied_access(&self, id: Option<&str>) -> Option<Access> {
        let project = self.claude();
        let owner = id.or(project.dispatching_worker.as_deref())?;
        let session = project.sessions.get(owner)?;
        session.link.as_ref()?;
        session.mode
    }
    pub(super) fn claude_can_submit(&self, id: Option<&str>) -> bool {
        let project = self.claude();
        let owner = id.or(project.dispatching_worker.as_deref());
        if owner.is_none() {
            return true;
        }
        owner
            .and_then(|k| project.sessions.get(k))
            .is_some_and(|p| p.initialized && p.link.is_some() && p.handoff.is_none())
            && !self.claude_settings_pending(id)
    }
    pub(super) fn can_configure_saved_claude(&self, id: &str) -> bool {
        self.is_session_scoped()
            && self.can_resume_send(id)
            && self.transcript(id).is_some_and(|t| t.ready && t.verified)
    }
    pub(super) fn claude_settings_pending(&self, id: Option<&str>) -> bool {
        let p = self.claude();
        id.or(p.dispatching_worker.as_deref())
            .and_then(|k| p.sessions.get(k))
            .is_some_and(|s| {
                s.controls
                    .values()
                    .any(|c| matches!(c, Control::Mode(_) | Control::Model(_)))
            })
    }
    pub(super) fn handing_off_claude(&self) -> bool {
        matches!(&self.link, links::Link::Claude(project) if project.sessions.values().any(|p|
            p.handoff.is_some() || p.deferred_command.as_ref().is_some_and(|c| c.terminal_allowed)))
    }
    pub(super) fn opening_claude_command(&self) -> bool {
        matches!(&self.link, links::Link::Claude(project) if project.sessions.values().any(|p|
            p.deferred_command.as_ref().is_some_and(|c| c.new_thread)))
    }
    pub(super) fn release_claude(
        &mut self,
        id: &str,
        handoff: super::commands::Handoff,
    ) -> io::Result<()> {
        let p = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .ok_or_else(|| io::Error::other("Open the Claude session before handing it off"))?;
        p.handoff = Some(handoff);
        p.link.take();
        Ok(())
    }
    pub(super) fn claude_move_writer_group(&self, id: &str) -> Option<u32> {
        self.claude()
            .sessions
            .get(id)?
            .link
            .as_ref()
            .map(|link| link.process_group())
    }

    pub(super) fn release_claude_writer_for_move(&mut self, id: &str) -> io::Result<()> {
        if self.transcripts.keys().any(|child| {
            child
                .strip_prefix(id)
                .is_some_and(|suffix| suffix.starts_with('/'))
                && self.thread_busy(child)
        }) {
            return Err(io::Error::other(
                "Wait for this session's Claude subagents to finish.",
            ));
        }
        let p = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .filter(|p| p.initialized && p.viewer.is_none() && p.handoff.is_none())
            .ok_or_else(|| {
                io::Error::other("This Claude session does not have an owned writer.")
            })?;
        if p.move_release.is_some() {
            return Ok(());
        }
        if !p.controls.is_empty() || p.first.is_some() || p.deferred_command.is_some() {
            return Err(io::Error::other(
                "Wait for Claude Code to finish its pending request.",
            ));
        }
        let link = p.link.take().ok_or_else(|| {
            io::Error::other("This Claude session does not have a running process.")
        })?;
        p.move_release = Some(link.reap_status());
        drop(link);
        Ok(())
    }
    fn finish_claude_handoff(&mut self, id: &str) -> bool {
        let Some(handoff) = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .and_then(|p| p.handoff.take())
        else {
            return false;
        };
        self.claude_mut().sessions.remove(id);
        self.handoff_ready = Some(handoff);
        self.changed.mark(Changed::NOTICE);
        true
    }
    pub(super) fn claude_palette(&self, query: &str) -> Vec<ThreadRow> {
        let q = query.to_lowercase();
        self.claude()
            .catalog
            .values()
            .filter_map(|row| {
                let mut row = row.clone();
                self.catalog.titles.decorate(&mut row);
                (row.title().to_lowercase().contains(&q) || row.preview.to_lowercase().contains(&q))
                    .then_some(row)
            })
            .collect()
    }
    pub(super) fn claude_caps(&self, owner: Option<&str>) -> crate::slash::Capabilities {
        let p = self.claude();
        owner
            .or(p.dispatching_worker.as_deref())
            .and_then(|k| p.sessions.get(k.split('/').next()?))
            .and_then(|s| s.capabilities.clone())
            .or_else(|| self.commands.schema.clone())
            .unwrap_or_else(|| {
                // Before the first turn there is no stock process. Its stable subscription
                // aliases still work with --model, and API aliases come from this launcher.
                let (current, names): (&str, Vec<&str>) = match &self.profile.route.billing {
                    Billing::Subscription => {
                        ("default", vec!["default", "opus", "sonnet", "haiku"])
                    }
                    Billing::Api(alias) => (
                        alias,
                        p.launcher.aliases.iter().map(String::as_str).collect(),
                    ),
                };
                let models = names
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| {
                        (
                            name.to_owned(),
                            super::routes_thread::ModelInfo {
                                label: crate::slash::readable_model(name),
                                rank: Some(index as u32),
                                ..Default::default()
                            },
                        )
                    })
                    .collect();
                crate::slash::Capabilities {
                    provider: self.profile.provider.clone(),
                    model: current.into(),
                    models: Arc::new(models),
                    ..Default::default()
                }
            })
    }
    pub fn open_claude(
        profile: Profile,
        launcher: Launcher,
        deliver: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let catalog = launcher.catalog(&profile)?;
        let project = Project {
            launcher: Arc::new(launcher),
            deliver: Arc::new(deliver),
            catalog,
            sessions: Default::default(),
            asks: Default::default(),
            dispatching_worker: None,
            worker: None,
            worker_blank: None,
            worker_model: None,
            serial: 0,
            native: Default::default(),
        };
        let mut store = Self::with_link(profile, links::Link::Claude(Box::new(project)));
        store.claude_list();
        store.state = Connection::Ready;
        store.changed.mark(Changed::CONNECTION);
        Ok(store)
    }

    /// Reserve the destination's Claude ID without starting a process or a turn.
    pub fn prepare_claude_move_writer(&self, reserved: &str) -> io::Result<String> {
        if self.is_host_client()
            || !self.is_session_scoped()
            || self.state != Connection::Ready
            || !wire::catalog::valid_session_id(reserved)
        {
            return Err(io::Error::other(
                "Open the target Claude route before preparing a move.",
            ));
        }
        let origin = if self.profile.route.is_api() {
            Origin::Api
        } else {
            Origin::Subscription
        };
        let key = session_key(&self.profile.route.billing, reserved);
        if self.claude().sessions.contains_key(&key) || self.claude().catalog.contains_key(&key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Claude already has this session. Open it to continue.",
            ));
        }
        match self
            .claude()
            .launcher
            .profiles
            .modified(&self.profile.project, reserved, origin)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(key),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Claude already has this session. Open it to continue.",
            )),
            Err(error) => Err(error),
        }
    }

    /// Display an activated move's archive before Claude has written its first journal.
    pub fn show_reserved_claude_move(&mut self, operation: &str, row: ThreadRow) -> io::Result<()> {
        if self.prepare_claude_move_writer(operation)? != row.id {
            return Err(io::Error::other("The reserved Claude writer changed."));
        }
        let thread = row.id.clone();
        let access = self.move_start_access(Some(&thread));
        self.restore_ended(&thread, row.title(), row.updated_at, None);
        self.adopt_catalog_row(row)?;
        self.selected = Some(thread.clone());
        let transcript = self.transcripts.entry(thread.clone()).or_default();
        transcript.ready = true;
        transcript.verified = true;
        transcript.loading = false;
        transcript.error = None;
        transcript.access = access;
        self.changed.replace_transcript(&thread);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    /// Send the operator's first explicit input to the already activated move.
    /// If the first turn created a journal, use ordinary resume on later sends.
    pub fn start_claude_move_turn(
        &mut self,
        reserved: &str,
        owner: &str,
        carried_request: String,
        access: Option<Access>,
    ) -> io::Result<()> {
        if self.prepare_claude_move_writer(reserved)? != owner
            || self.claude().dispatching_worker.is_some()
        {
            return Err(io::Error::other(
                "This Claude session belongs to another move.",
            ));
        }
        let previous = self.drafts.remove(&None);
        let preset = std::mem::replace(&mut self.next_preset, access);
        let options = self.next_options.take();
        self.claude_move_session = Some(reserved.into());
        let result = self.start_handoff(carried_request, vec![]);
        self.claude_move_session = None;
        self.next_preset = preset;
        self.next_options = options;
        self.drafts.remove(&None);
        if let Some(draft) = previous {
            self.drafts.insert(None, draft);
        }
        result
    }
    fn claude(&self) -> &Project {
        match &self.link {
            links::Link::Claude(p) => p,
            _ => unreachable!("Claude integration"),
        }
    }
    fn claude_mut(&mut self) -> &mut Project {
        match &mut self.link {
            links::Link::Claude(p) => p,
            _ => unreachable!("Claude integration"),
        }
    }
    pub(super) fn claude_list(&mut self) {
        self.catalog.clear();
        let mut rows = self.claude_palette(&self.query);
        rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        for row in rows {
            self.catalog.insert(row);
        }
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
    }
    fn remember_claude(&mut self, row: ThreadRow) {
        self.claude_mut()
            .catalog
            .insert(row.id.clone(), row.clone());
        self.record_row(row);
    }
    fn spawn_claude(
        &mut self,
        session: Session,
        first: Option<(String, String)>,
        confirm_missing_alias: bool,
    ) -> io::Result<String> {
        self.spawn_claude_in(session, first, confirm_missing_alias, None)
    }
    /// `resume_mode` starts a resumed session in a chosen access mode; a new session
    /// takes its preset and any other resume starts in `default`.
    fn spawn_claude_in(
        &mut self,
        session: Session,
        first: Option<(String, String)>,
        confirm_missing_alias: bool,
        resume_mode: Option<Access>,
    ) -> io::Result<String> {
        let id = match &session {
            Session::New(id) | Session::Resume(id) | Session::Fork { id, .. } => id,
        }
        .clone();
        let owner = session_key(&self.profile.route.billing, &id);
        if self.claude().sessions.contains_key(&owner) {
            return Ok(owner);
        }
        // The journal as OX found it: only a write after this, by a client that is not
        // OX's headless one, names a foreign owner.
        let baseline = stock_id(&owner).ok().and_then(|raw| {
            let origin = if self.profile.route.is_api() {
                Origin::Api
            } else {
                Origin::Subscription
            };
            self.claude()
                .launcher
                .profiles
                .modified(&self.profile.project, raw, origin)
                .ok()
        });
        self.claude_mut()
            .native
            .ox_seen
            .insert(owner.clone(), baseline);
        let profile = self.profile.clone();
        // A draft preset belongs only to the new session it was chosen for.
        let preset = match &session {
            Session::New(_) => {
                let chosen = self.next_access();
                self.next_preset = None;
                chosen
            }
            Session::Resume(_) => self.starting_access(self.host_start_access(&owner), resume_mode),
            Session::Fork { source, .. } => {
                self.host_start_access(&session_key(&self.profile.route.billing, source))
            }
        };
        // A worker keeps its launch contract on every relaunch: a resume reuses its own,
        // a fork the contract of its source.
        let contract = match &session {
            Session::New(_) => self.claude_mut().worker.take(),
            Session::Resume(_) => self.worker_contracts.get(&owner).copied(),
            Session::Fork { source, .. } => self
                .worker_contracts
                .get(&session_key(&self.profile.route.billing, source))
                .copied(),
        };
        if let Some(c) = contract {
            self.worker_contracts.insert(owner.clone(), c);
        }
        // A pinned worker keeps its exact model the same way: never the account default.
        let model = match &session {
            Session::New(_) => self.claude_mut().worker_model.take(),
            Session::Resume(_) => self.worker_models.get(&owner).cloned(),
            Session::Fork { source, .. } => self
                .worker_models
                .get(&session_key(&self.profile.route.billing, source))
                .cloned(),
        }
        .filter(|_| !self.profile.route.is_api());
        if let Some(m) = &model {
            self.worker_models.insert(owner.clone(), m.clone());
        }
        // A new worker starts in the contract's mode. A resumed one keeps the access the
        // user chose since; without a recorded choice it starts in the contract's mode.
        let mode = match (&session, contract) {
            (Session::New(_), Some(c)) => Some(c.permission_mode().to_owned()),
            _ => preset.and_then(Access::mode).map(str::to_owned),
        }
        .or_else(|| contract.map(|c| c.permission_mode().to_owned()));
        // With no chosen mode, Claude Code starts in its own configured default,
        // as `claude` does in a terminal, and reports that mode on connect.
        let model = model.or_else(|| match &session {
            Session::New(_) if contract.is_none() => {
                self.next_options.take().map(|options| options.model)
            }
            Session::Resume(_) => self.resume_models.get(&owner).cloned(),
            _ => None,
        });
        let model = if let Billing::Api(alias) = &profile.route.billing {
            let source = match &session {
                Session::Resume(id) | Session::Fork { source: id, .. } => Some(id),
                _ => None,
            };
            let launcher = &self.claude().launcher;
            match (model, source) {
                (None, Some(source)) => Bindings::new(launcher.index.clone()).selected_model(
                    &profile.project,
                    &launcher.profiles.api,
                    source,
                    alias,
                )?,
                (chosen, _) => chosen,
            }
        } else {
            model
        };
        // A failed first send may have a reserved ID but no stock journal. Retry
        // only that creation, preserving the session's options and the unrelated new draft.
        let session = if matches!(session, Session::Resume(_))
            && baseline.is_none()
            && self.unsaved_claude_drafts.contains(&owner)
        {
            Session::New(id.clone())
        } else {
            session
        };
        let is_new = matches!(session, Session::New(_));
        let first_is_command = first
            .as_ref()
            .is_some_and(|(_, text)| text.trim_start().starts_with('/'));
        let deliver_brief_now = is_new && first.is_some() && !first_is_command;
        let memory_brief_pending = is_new && !deliver_brief_now;
        let (first_context, attachment) = if deliver_brief_now {
            self.first_turn_context()
        } else {
            (first.as_ref().and_then(|_| self.sender_context()), None)
        };
        let memory_attachment = first
            .as_ref()
            .and_then(|(uuid, _)| Some((uuid.clone(), attachment?)));
        let worker_notice = first
            .as_ref()
            .and_then(|(uuid, _)| Some((uuid.clone(), self.worker_notice_id()?)));
        let p = self.claude_mut();
        p.serial += 1;
        let generation = p.serial;
        let launcher = p.launcher.clone();
        let receive = p.deliver.clone();
        let key = owner.clone();
        p.sessions.insert(
            owner.clone(),
            Process {
                generation,
                first,
                first_context,
                memory_attachment,
                worker_notice,
                launch_mode: mode.clone(),
                selected_model: model.clone(),
                started_new: is_new,
                memory_brief_pending,
                ..Default::default()
            },
        );
        let alive = p.sessions[&owner].events.lifetime();
        let history_owner = owner.clone();
        thread::spawn(move || {
            let emit = move |data| {
                receive(LinkEvent::Claude(Update {
                    owner: key.clone(),
                    generation,
                    data,
                }))
            };
            let emit = Arc::new(emit);
            let start = || -> io::Result<()> {
                let index = Bindings::new(launcher.index.clone());
                let source = match &session {
                    Session::Resume(id) => Some(id),
                    Session::Fork { source, .. } => Some(source),
                    _ => None,
                };
                if let Some(source) = source {
                    let saved = launcher.saved_session(&profile, source, confirm_missing_alias)?;
                    if source != &id
                        && let Billing::Api(alias) = &profile.route.billing
                    {
                        index.bind(&profile.project, &launcher.profiles.api, source, alias)?;
                    }
                    history::load(&profile, &history_owner, &saved, &alive, |batch| {
                        emit(Data::History(batch))
                    })?;
                }
                if let Billing::Api(alias) = &profile.route.billing {
                    index.record(
                        &profile.project,
                        &launcher.profiles.api,
                        &id,
                        alias,
                        model.as_deref(),
                    )?
                }
                let output = emit.clone();
                let link = pipe::Link::spawn(
                    pipe::Launch {
                        ox: launcher.ox.clone(),
                        path: launcher.path.clone(),
                        project: profile.project,
                        billing: profile.route.billing,
                        session,
                        permission_mode: mode,
                        contract,
                        model,
                    },
                    move |event| output(Data::Stock(event)),
                )?;
                emit(Data::Connected(link));
                Ok(())
            };
            if let Err(e) = start() {
                emit(Data::Failed(e.to_string()));
            }
        });
        Ok(owner)
    }
    pub(super) fn claude_open(&mut self, id: &str) -> io::Result<()> {
        self.claude_open_alias(id, false)
    }
    pub(super) fn claude_open_alias(&mut self, id: &str, confirm: bool) -> io::Result<()> {
        if id.contains('/') {
            return Ok(());
        }
        stock_id(id)?;
        if confirm {
            return self.claude_resume(id, true);
        }
        self.native_live_now(id);
        self.view_native(id)
    }

    /// Explicitly resume an existing conversation, without sending any input.
    pub(super) fn claude_resume(&mut self, id: &str, confirm: bool) -> io::Result<()> {
        if self.native_live_now(id) {
            if confirm && self.is_viewer(id) {
                self.claude_mut().sessions.remove(id);
            }
            return self.view_journal(id, confirm);
        }
        if self.is_viewer(id) {
            self.claude_mut().sessions.remove(id);
        }
        self.spawn_claude(Session::Resume(stock_id(id)?.to_owned()), None, confirm)?;
        self.ended_threads.remove(id);
        self.transcripts.entry(id.into()).or_default().loading = true;
        Ok(())
    }
    /// Resume an ended Claude session with the draft as its first input, so the message
    /// goes out as soon as the process is initialised.
    pub(super) fn claude_resume_send(&mut self, id: &str, command: Option<&str>) -> io::Result<()> {
        let row = self
            .catalog
            .get(id)
            .ok_or_else(|| io::Error::other("Thread is outside this catalog"))?
            .clone();
        self.check_route(&row)?;
        if self.native_live_now(id) {
            return Err(io::Error::other(native::HINT));
        }
        if self.is_viewer(id) {
            self.claude_mut().sessions.remove(id);
        }
        let text = command.unwrap_or_else(|| self.draft(Some(id))).to_owned();
        self.restored_drafts.remove(id);
        let raw = stock_id(id)?.to_owned();
        // A send continues the conversation the user was having, so its first turn runs
        // in the access mode they had chosen, also after the host reaped the idle route
        // behind their back. History selection leaves that choice untouched.
        let mode = self.resume_access.get(id).copied();
        self.spawn_claude_in(Session::Resume(raw), Some((new_id()?, text)), false, mode)?;
        self.ended_threads.remove(id);
        let t = self.transcripts.entry(id.into()).or_default();
        t.loading = true;
        t.error = None;
        self.selected = Some(id.into());
        self.selection += 1;
        if command.is_none() {
            self.edit(Some(id), String::new());
        }
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS | Changed::CONNECTION);
        Ok(())
    }
    pub(super) fn claude_submit(
        &mut self,
        id: Option<&str>,
        command: Option<&str>,
    ) -> io::Result<()> {
        if let (Some(id), Some(command)) = (id, command)
            && self.can_configure_saved_claude(id)
        {
            return self.claude_resume_send(id, Some(command));
        }
        if !self.can_send(id) || id.is_some_and(|s| s.contains('/')) {
            return Err(io::Error::other(
                "Open a ready parent Claude session to send",
            ));
        }
        let text = command.unwrap_or_else(|| self.draft(id)).to_owned();
        if text.trim().is_empty() {
            return Err(io::Error::other("Enter a message"));
        }
        if !self.draft_images(id).is_empty() {
            return Err(io::Error::other(
                "Remove the attachment before submitting to Claude Code",
            ));
        }
        if let Some(id) = id {
            self.restored_drafts.remove(id);
        }
        let new_conversation = id.is_none() && command.is_none();
        let message = new_id()?;
        let owner = match id {
            Some(id) => id.to_owned(),
            None => match self.claude_mut().dispatching_worker.take() {
                Some(id) => id,
                None => {
                    let native = self.claude_move_session.take().map_or_else(new_id, Ok)?;
                    self.spawn_claude(Session::New(native), None, false)?
                }
            },
        };
        // A handoff's first turn titles the session from its request, not its block.
        let visible = if new_conversation && self.claude_handoff {
            crate::handoff::split_claude_turn(&text).map_or(text.as_str(), |(_, r)| r)
        } else {
            text.as_str()
        }
        .to_owned();
        if new_conversation {
            self.first_title_prompt(&owner, &visible);
        }
        if !self.claude().catalog.contains_key(&owner) {
            let new = row(
                &self.profile,
                owner.clone(),
                visible
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect(),
                now_secs(),
            );
            self.remember_claude(new);
        }
        self.selected = Some(owner.clone());
        self.selection += 1;
        let is_command = command.is_some() || text.trim_start().starts_with('/');
        let brief_pending = id.is_none()
            || self
                .claude()
                .sessions
                .get(&owner)
                .is_some_and(|p| p.memory_brief_pending);
        let deliver_brief = brief_pending && !is_command;
        let (context, attachment) = if deliver_brief {
            self.first_turn_context()
        } else {
            (self.sender_context(), None)
        };
        let worker_notice = self.worker_notice_id();
        let process = self
            .claude_mut()
            .sessions
            .get_mut(&owner)
            .ok_or_else(|| io::Error::other("Reconnect the session before sending"))?;
        if deliver_brief {
            process.memory_brief_pending = false;
            process.memory_attachment = attachment.map(|attachment| (message.clone(), attachment));
        }
        if let Some(id) = worker_notice {
            process.worker_notice = Some((message.clone(), id));
        }
        process.first = Some((message, text));
        process.first_context = context;
        self.flush_claude(&owner)?;
        if id.is_none() {
            self.host_opened = Some(owner.clone());
        }
        if command.is_none() {
            self.edit(id, String::new());
        }
        Ok(())
    }
    pub(crate) fn defer_claude_command(
        &mut self,
        id: Option<&str>,
        command: &str,
        terminal_allowed: bool,
    ) -> io::Result<()> {
        let owner = if let Some(id) = id {
            if !self.can_resume_send(id) {
                return Err(io::Error::other("Wait for this Claude session to be ready"));
            }
            self.claude_resume_send(id, Some(command))?;
            id.to_owned()
        } else {
            if !self.can_send(None) || self.opening_claude_command() {
                return Err(io::Error::other("Wait for the opening Claude session"));
            }
            self.spawn_claude(
                Session::New(new_id()?),
                Some((new_id()?, command.into())),
                false,
            )?
        };
        self.claude_mut()
            .sessions
            .get_mut(&owner)
            .unwrap()
            .deferred_command = Some(DeferredCommand {
            new_thread: id.is_none(),
            terminal_allowed,
        });
        Ok(())
    }

    fn publish_command_thread(&mut self, owner: &str, command: &str) {
        self.remember_claude(row(
            &self.profile,
            owner.into(),
            command.chars().take(120).collect(),
            now_secs(),
        ));
        self.selected = Some(owner.into());
        self.selection += 1;
        self.host_opened = Some(owner.into());
    }

    /// Return whether the verified process should receive the buffered input.
    fn resolve_claude_command(
        &mut self,
        owner: &str,
        deferred: DeferredCommand,
    ) -> io::Result<bool> {
        let (_, text) = self.claude().sessions[owner]
            .first
            .as_ref()
            .expect("deferred command input");
        let text = text.clone();
        if matches!(
            self.parse_command(Some(owner), &text),
            Some(crate::slash::Action::SessionCommand)
        ) {
            if deferred.new_thread {
                self.publish_command_thread(owner, &text);
            }
            return Ok(true);
        }
        if deferred.terminal_allowed {
            let thread = (!deferred.new_thread).then(|| owner.to_owned());
            let handoff = super::commands::Handoff {
                argv: self.handoff_argv(thread.as_deref())?,
                thread: thread.clone(),
                command: text,
            };
            if let Some(thread) = thread {
                self.terminal_threads.insert(thread);
            }
            self.claude_mut()
                .sessions
                .get_mut(owner)
                .unwrap()
                .first
                .take();
            self.release_claude(owner, handoff)?;
        } else {
            if deferred.new_thread {
                self.publish_command_thread(owner, &text);
            }
            self.claude_end(owner)?;
            self.notice = Some(format!(
                "{} is not available on this model route.",
                text.split_whitespace().next().unwrap_or("Command")
            ));
            self.changed.mark(Changed::NOTICE);
        }
        Ok(false)
    }

    fn flush_claude(&mut self, owner: &str) -> io::Result<()> {
        let Some(process) = self.claude_mut().sessions.get_mut(owner) else {
            return Ok(());
        };
        let (Some(_), true) = (&process.link, process.initialized) else {
            return Ok(());
        };
        if let Some(command) = process.deferred_command.take()
            && !self.resolve_claude_command(owner, command)?
        {
            return Ok(());
        }
        let transcript = self.transcripts.entry(owner.into()).or_default();
        transcript.loading = false;
        transcript.ready = true;
        transcript.verified = true;
        let process = self.claude_mut().sessions.get_mut(owner).unwrap();
        let Some((id, text)) = process.first.as_ref() else {
            return Ok(());
        };
        let link = process.link.as_ref().unwrap();
        let content = vec![json!({"type":"text","text":text})];
        match process.first_context.as_deref() {
            Some(context) => link.send_user_with_context(id, content, context),
            None => link.send_user(id, content),
        }?;
        let (id, text) = process.first.take().unwrap();
        process.first_context = None;
        process.events.sent(&id, &text);
        let transcript = self.transcripts.get_mut(owner).unwrap();
        let turn = transcript
            .active_turn
            .get_or_insert_with(|| id.clone())
            .clone();
        transcript.observe_item(
            turn,
            id,
            Item::UserMessage {
                content: vec![Content {
                    text: Some(text),
                    path: None,
                }],
            },
            true,
            None,
        );
        self.changed.transcript(owner);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
    pub(super) fn claude_stop(&mut self, id: &str) -> io::Result<()> {
        let process = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .ok_or_else(|| io::Error::other("No active Claude session"))?;
        let link = process
            .link
            .as_ref()
            .ok_or_else(|| io::Error::other("Waiting for Claude Code"))?;
        let request = link.interrupt()?;
        process.controls.insert(request, Control::Interrupt);
        if let Some(t) = self.transcripts.get_mut(id)
            && let Some(turn) = t.active_turn.clone()
        {
            t.stop_requested = Some(turn);
        }
        Ok(())
    }
    /// Drop a session's process, which closes its link and kills the process
    /// group. A later send can resume the saved thread.
    pub(super) fn claude_end(&mut self, id: &str) -> io::Result<()> {
        let project = self.claude();
        if project.dispatching_worker.as_deref() == Some(id) || !project.sessions.contains_key(id) {
            return Err(io::Error::other("No active Claude session"));
        }
        if project.sessions[id].viewer.is_some() {
            return Err(io::Error::other(
                "Another client is running this session; OX only follows it",
            ));
        }
        if project.sessions[id].handoff.is_some() {
            return Err(io::Error::other("This session is opening in a terminal"));
        }
        let mut p = self
            .claude_mut()
            .sessions
            .remove(id)
            .expect("checked above");
        if let Some(mode) = p.mode.filter(|_| p.link.is_some()) {
            self.resume_access.insert(id.into(), mode);
        }
        self.restore_claude_input(id, &mut p);
        drop(p);
        let removed: std::collections::HashSet<_> = self
            .claude()
            .asks
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| k.clone())
            .collect();
        self.asks.retain(|a| !removed.contains(&a.id));
        self.claude_mut().asks.retain(|k, _| !removed.contains(k));
        for (key, t) in &mut self.transcripts {
            if key == id || key.strip_prefix(id).is_some_and(|s| s.starts_with('/')) {
                t.verified = false;
                t.ready = false;
                t.loading = false;
                t.active_turn = None;
                t.error = None;
                self.changed.transcript(key);
            }
        }
        self.changed
            .mark(Changed::CONNECTION | Changed::ASK | Changed::THREADS);
        Ok(())
    }
    pub(super) fn claude_has_live(&self, id: &str) -> bool {
        let project = self.claude();
        project.dispatching_worker.as_deref() != Some(id)
            && project.worker_blank.as_deref() != Some(id)
            && project.sessions.get(id).is_some_and(|p| p.viewer.is_none())
    }
    pub(super) fn claude_is_blank(&self, id: &str) -> bool {
        self.claude().worker_blank.as_deref() == Some(id)
    }
    /// Include a dedicated worker process while its first input is being prepared.
    pub fn process_threads(&self) -> Vec<String> {
        let mut ids = self.live_threads();
        if let links::Link::Claude(p) = &self.link
            && let Some(id) = &p.worker_blank
            && p.sessions.contains_key(id)
        {
            ids.push(id.clone());
        }
        ids
    }
    pub(super) fn claude_live(&self) -> Vec<String> {
        let project = self.claude();
        let mut ids: Vec<String> = project
            .sessions
            .iter()
            .filter(|(k, p)| {
                project.dispatching_worker.as_deref() != Some(k.as_str())
                    && project.worker_blank.as_deref() != Some(k.as_str())
                    && p.viewer.is_none()
            })
            .map(|(k, _)| k.clone())
            .collect();
        ids.sort();
        ids
    }
    pub(super) fn claude_mode(&mut self, id: &str, mode: Access) -> io::Result<()> {
        let process = self
            .claude_mut()
            .sessions
            .get_mut(id)
            .ok_or_else(|| io::Error::other("Open a Claude session first"))?;
        let link = process
            .link
            .as_ref()
            .ok_or_else(|| io::Error::other("Waiting for Claude Code"))?;
        let request = link.set_mode(
            mode.mode()
                .ok_or_else(|| io::Error::other("Unsupported permission mode"))?,
        )?;
        process.controls.insert(request, Control::Mode(mode));
        Ok(())
    }
    fn record_claude_model(&self, owner: &str, model: &str) -> io::Result<()> {
        if let Billing::Api(alias) = &self.profile.route.billing {
            let launcher = &self.claude().launcher;
            Bindings::new(launcher.index.clone()).record(
                &self.profile.project,
                &launcher.profiles.api,
                stock_id(owner)?,
                alias,
                Some(model),
            )?;
        }
        Ok(())
    }
    pub(super) fn claude_choose(
        &mut self,
        id: Option<&str>,
        choice: super::routes_thread::Choice,
    ) -> io::Result<()> {
        let super::routes_thread::Choice::Model(model) = choice else {
            return Err(io::Error::other("Unsupported Claude session setting"));
        };
        match &self.profile.route.billing {
            Billing::Api(_)
                if !self.same_model_route(&model)
                    || !self.claude().launcher.aliases.contains(&model) =>
            {
                return Err(io::Error::other(
                    "Choose a configured model on this API provider.",
                ));
            }
            Billing::Subscription if !crate::claude_models::valid_model(&model) => {
                return Err(io::Error::other(
                    "Enter a Claude model name, such as sonnet.",
                ));
            }
            _ => {}
        }
        let Some(owner) = id else {
            self.next_options = Some(super::routes_thread::Options {
                model,
                ..Default::default()
            });
            return Ok(());
        };
        if self.can_configure_saved_claude(owner) {
            self.record_claude_model(owner, &model)?;
            self.resume_models.insert(owner.to_owned(), model);
            self.changed.transcript(owner);
            return Ok(());
        }
        let p = self
            .claude_mut()
            .sessions
            .get_mut(owner)
            .ok_or_else(|| io::Error::other("Claude session has closed"))?;
        let link = p
            .link
            .as_ref()
            .ok_or_else(|| io::Error::other("Waiting for Claude Code"))?;
        let request = link.set_model(&model)?;
        p.controls.insert(request, Control::Model(model));
        Ok(())
    }
    pub(super) fn claude_fork(&mut self, id: &str) -> io::Result<()> {
        let source = stock_id(id)?.to_owned();
        let owner = self.spawn_claude(
            Session::Fork {
                source,
                id: new_id()?,
            },
            None,
            false,
        )?;
        let title = self
            .catalog
            .get(id)
            .map_or("Claude session", ThreadRow::title);
        self.remember_claude(row(
            &self.profile,
            owner.clone(),
            format!("Fork of {title}"),
            now_secs(),
        ));
        self.selected = Some(owner.clone());
        self.selection += 1;
        self.host_opened = Some(owner.clone());
        self.transcripts.entry(owner).or_default().loading = true;
        Ok(())
    }
    pub(super) fn claude_response(&mut self, id: &RequestId, answer: Value) -> io::Result<()> {
        let (owner, ask) = self
            .claude()
            .asks
            .get(id)
            .ok_or_else(|| io::Error::other("Claude request has expired"))?;
        self.claude()
            .sessions
            .get(owner)
            .and_then(|p| p.link.as_ref())
            .ok_or_else(|| io::Error::other("Claude session has closed"))?
            .respond(&ask.request_id, answer)?;
        self.claude_mut().asks.remove(id);
        Ok(())
    }
    /// The session that owns an open Claude tool request and the tool it asks
    /// for. `None` for another runtime or an unknown request.
    pub fn claude_ask_tool(&self, id: &RequestId) -> Option<(&str, &str)> {
        let links::Link::Claude(project) = &self.link else {
            return None;
        };
        project
            .asks
            .get(id)
            .map(|(owner, ask)| (owner.as_str(), ask.tool.as_str()))
    }
    /// Turn down an open Claude tool request. Claude Code receives `message` as
    /// the tool's error result and the turn continues.
    pub fn deny_claude_ask(&mut self, id: &RequestId, message: &str) -> io::Result<()> {
        let answer = match &self.link {
            links::Link::Claude(project) => project.asks.get(id).map(|(_, ask)| ask.deny(message)),
            _ => None,
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Request already resolved"))?;
        self.claude_response(id, answer)?;
        self.asks.retain(|a| &a.id != id);
        self.changed.mark(Changed::ASK);
        Ok(())
    }
    pub(super) fn claude_questions(&mut self, id: &RequestId, values: &[String]) -> io::Result<()> {
        let (_, ask) = self
            .claude()
            .asks
            .get(id)
            .ok_or_else(|| io::Error::other("Claude request has expired"))?;
        if values.len() != ask.questions.len() {
            return Err(io::Error::other("Answer every question"));
        }
        let values = ask
            .questions
            .iter()
            .zip(values)
            .map(|(q, v)| (q.id.clone(), v.clone()))
            .collect();
        let answer = ask.answer(&values)?;
        self.claude_response(id, answer)
    }
    pub fn terminal_session_id<'a>(&self, id: &'a str) -> io::Result<&'a str> {
        if self.profile.route.runtime == Runtime::Claude {
            stock_id(id)
        } else {
            Ok(id)
        }
    }
}
