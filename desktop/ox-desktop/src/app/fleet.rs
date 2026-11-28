//! Live navigation metadata for a single host, including unopened sessions.
mod cache;
pub(crate) mod tree;
use crate::platform::{CatalogEvent, CatalogWatch, HostLink};
use gpui::*;
use ox_desk_core::{
    host_protocol::{Scope, SessionView},
    store::{ThreadRow, replica},
};
use std::time::{Duration, Instant};
use std::{
    collections::{HashMap, HashSet},
    io,
};

#[derive(Default)]
struct Project {
    titles: HashMap<String, CatalogRow>,
    archived: HashSet<String>,
    waiting: HashSet<String>,
    draft_waiting: bool,
}

#[derive(Default)]
struct CatalogRow {
    title: String,
    native: Option<ThreadRow>,
}
impl CatalogRow {
    fn live(view: SessionView) -> (String, Self) {
        let native = replica::row(&view);
        (
            view.id,
            Self {
                title: view.title,
                native,
            },
        )
    }
}
type Library = HashMap<Scope, HashMap<String, CatalogRow>>;

#[derive(Default)]
pub(super) struct Inventory {
    pub link: HostLink,
    pub(super) legacy_projects: bool,
    pub(super) recent: crate::workspace_state::LastWorkspace,
    pub(super) directory: Option<ox_desk_core::host_protocol::ProjectDirectory>,
    projects: HashMap<Scope, Project>,
    saved: Library,
    previous: Vec<(Scope, Option<String>, String)>,
    received_catalog: bool,
    complete_library: bool,
    cache_scheduled: bool,
    last_write: Option<Instant>,
    scan_requested: bool,
    scanning: bool,
    scan_started: Option<Instant>,
    scan_error: Option<String>,
    online: bool,
    info: Option<ox_desk_core::host_protocol::HostInfo>,
    error: Option<io::ErrorKind>,
    saved_sidebar: crate::sidebar_state::SidebarState,
    last_seen: Option<chrono::DateTime<chrono::Local>>,
    _watch: Option<CatalogWatch>,
}

impl Inventory {
    pub fn start(link: HostLink, cx: &mut Context<Self>) -> Self {
        let mut catalog = Self {
            recent: crate::workspace_state::read_preference(&link.state_file("workspace.json"))
                .unwrap_or_default(),
            saved_sidebar: crate::sidebar_state::SidebarState::load(
                &link.state_file("sidebar.json"),
            ),
            link,
            ..Default::default()
        };
        match catalog.link.watch_catalog(true) {
            Ok((watch, events)) => {
                catalog._watch = Some(watch);
                let link = catalog.link.clone();
                let restored = cx.background_spawn(async move { cache::Snapshot::read(&link) });
                cx.spawn(async move |this, cx| {
                    if let Some(snapshot) = restored.await {
                        let _ = this.update(cx, |s, cx| {
                            snapshot.restore(s);
                            cx.notify();
                        });
                    }
                    while let Ok(first) = events.recv_async().await {
                        if this
                            .update(cx, |s, cx| {
                                let was_online = s.online;
                                for event in std::iter::once(first).chain(events.try_iter()) {
                                    if let CatalogEvent::Notice(notice) = &event {
                                        notice.show(&s.link, cx);
                                    } else {
                                        s.receive(event);
                                    }
                                }
                                if s.online {
                                    s.schedule_cache(cx);
                                } else if was_online {
                                    s.save_cache(cx);
                                }
                                if s.scan_requested && s.scan_started.is_none() {
                                    s.load_saved(cx);
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => catalog.error = Some(e.kind()),
        }
        catalog
    }
    fn receive(&mut self, event: CatalogEvent) {
        if let CatalogEvent::Disconnected(reason) = event {
            if self.online {
                self.last_seen = Some(chrono::Local::now());
            }
            self.error = Some(reason);
            self.online = false;
            return;
        }
        self.last_seen = Some(chrono::Local::now());
        if !self.online {
            self.scan_started = None;
        }
        self.online = true;
        self.error = None;
        match event {
            CatalogEvent::Notice(_) => (),
            CatalogEvent::Projects(dir) => {
                self.legacy_projects = dir.is_none();
                self.directory = dir;
            }
            CatalogEvent::NoHostStatus => self.info = None,
            CatalogEvent::Host(h) => self.info = Some(h),
            CatalogEvent::Reset => {
                self.received_catalog = true;
                self.projects.clear();
            }
            CatalogEvent::Archived(scope, ids) => {
                self.projects.entry(scope).or_default().archived = ids
            }
            CatalogEvent::DraftWaiting(scope, waiting) => {
                self.projects.entry(scope).or_default().draft_waiting = waiting
            }
            CatalogEvent::Waiting(scope, ids) => {
                self.projects.entry(scope).or_default().waiting = ids
            }
            CatalogEvent::Replace(scope, rows) => {
                self.projects.entry(scope).or_default().titles =
                    rows.into_iter().map(CatalogRow::live).collect();
            }
            CatalogEvent::Update(scope, row) => {
                let (id, row) = CatalogRow::live(row);
                self.projects
                    .entry(scope)
                    .or_default()
                    .titles
                    .insert(id, row);
            }
            CatalogEvent::Disconnected(_) => unreachable!(),
        }
    }

    pub(super) fn load_saved(&mut self, cx: &mut Context<Self>) {
        self.scan_requested = true;
        if !self.online
            || self.scanning
            || self
                .scan_started
                .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        self.scanning = true;
        self.scan_started = Some(Instant::now());
        let link = self.link.clone();
        let task = cx.background_spawn(async move { link.discover_rows(true) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.scanning = false;
                match result {
                    Ok((rows, errors)) => {
                        let mut saved = Library::new();
                        for (scope, row) in rows {
                            saved.entry(scope).or_default().insert(
                                row.id.clone(),
                                CatalogRow {
                                    title: row.title().to_owned(),
                                    native: Some(row),
                                },
                            );
                        }
                        s.apply_saved(saved, errors.is_empty());
                        s.scan_error = errors.first().cloned();
                        s.schedule_cache(cx);
                    }
                    Err(e) => s.scan_error = Some(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn apply_saved(&mut self, rows: Library, complete: bool) {
        if complete {
            self.complete_library = true;
            self.saved = rows;
        } else {
            for (scope, items) in rows {
                self.saved.entry(scope).or_default().extend(items);
            }
        }
    }
    fn native_row(&self, scope: &Scope, id: &str) -> Option<&ThreadRow> {
        self.projects
            .get(scope)
            .and_then(|p| p.titles.get(id))
            .and_then(|r| r.native.as_ref())
            .or_else(|| self.saved.get(scope)?.get(id)?.native.as_ref())
    }
    pub fn display_name(&self) -> &str {
        self.info
            .as_ref()
            .and_then(|h| h.name.as_deref())
            .unwrap_or_else(|| {
                self.link
                    .remote()
                    .map_or("This computer", |h| h.name.as_str())
            })
    }
    pub fn status(&self) -> String {
        if self.online {
            let status: String = self
                .info
                .as_ref()
                .and_then(|h| h.resources.as_ref())
                .map_or_else(
                    || "Connected".into(),
                    |r| {
                        format!(
                            "{} {} · {:.1} GiB available · load {:.1} · {} active workers",
                            r.os,
                            r.arch,
                            r.available_memory as f64 / 1_073_741_824.,
                            r.load_one_minute,
                            r.running_workers
                        )
                    },
                );
            let status = if self.scanning {
                format!("{status} · Loading saved sessions…")
            } else if self.scan_error.is_some() {
                format!("{status} · Some saved sessions unavailable")
            } else {
                status
            };
            return match self.info.as_ref().and_then(|host| host.mode_policy) {
                Some(policy)
                    if policy != ox_desk_core::host_policy::HostPolicy::FollowInterface =>
                {
                    format!("{status} · {}", policy.label())
                }
                _ => status,
            };
        }
        match self.error {
            Some(io::ErrorKind::Unsupported) => "Needs update".into(),
            Some(io::ErrorKind::PermissionDenied) => "Pairing needs attention".into(),
            Some(_) => self.last_seen.as_ref().map_or_else(
                || "Offline".into(),
                |t| {
                    let state = if self.received_catalog
                        && chrono::Local::now().signed_duration_since(*t).num_seconds() < 5
                    {
                        "Reconnecting"
                    } else {
                        "Offline"
                    };
                    format!("{state} · last seen {}", t.format("%a %H:%M"))
                },
            ),
            None => "Connecting…".into(),
        }
    }
    pub fn rows(&self, waiting_only: bool) -> Vec<(Scope, Option<String>, String)> {
        let mut rows = Vec::new();
        for (scope, project) in &self.projects {
            let add = |id: &String| {
                (
                    scope.clone(),
                    Some(id.clone()),
                    project
                        .titles
                        .get(id)
                        .map(|r| &r.title)
                        .filter(|t| !t.is_empty())
                        .unwrap_or(id)
                        .clone(),
                )
            };
            if waiting_only {
                rows.extend(project.waiting.iter().map(add));
                if project.draft_waiting {
                    rows.push((scope.clone(), None, "New session approval".into()));
                }
            } else {
                rows.extend(
                    project
                        .titles
                        .keys()
                        .filter(|id| !project.archived.contains(*id))
                        .map(add),
                );
            }
        }
        if !waiting_only {
            for (scope, saved) in &self.saved {
                let live = self.projects.get(scope);
                for (id, entry) in saved {
                    let title = &entry.title;
                    if live.is_some_and(|p| p.titles.contains_key(id) || p.archived.contains(id)) {
                        continue;
                    }
                    rows.push((
                        scope.clone(),
                        Some(id.clone()),
                        if title.is_empty() { id } else { title }.clone(),
                    ));
                }
            }
        }
        if !waiting_only && !self.online && !self.complete_library {
            self.add_last_seen(&mut rows);
        }
        rows.sort_by(|a, b| (&a.0.project, &a.2, &a.1).cmp(&(&b.0.project, &b.2, &b.1)));
        rows
    }
    fn can_open(&self, scope: Option<&Scope>, thread: Option<&str>) -> bool {
        if !self.online {
            return false;
        }
        let Some(scope) = scope else {
            return thread.is_none();
        };
        let live = self.projects.get(scope);
        match thread {
            Some(id) => {
                live.is_some_and(|p| p.waiting.contains(id))
                    || (!live.is_some_and(|p| p.archived.contains(id))
                        && (live.is_some_and(|p| p.titles.contains_key(id))
                            || self
                                .saved
                                .get(scope)
                                .is_some_and(|rows| rows.contains_key(id))))
            }
            None => live.is_some_and(|p| p.draft_waiting),
        }
    }
    pub fn waiting(&self) -> usize {
        self.projects
            .values()
            .map(|p| p.waiting.len() + usize::from(p.draft_waiting))
            .sum()
    }
    pub fn online(&self) -> bool {
        self.online
    }
}

#[derive(Clone)]
pub(crate) struct HandoffDestination {
    pub registration: EntityId,
    pub repository: String,
    pub link: HostLink,
    pub id: String,
    pub name: String,
    pub project: std::path::PathBuf,
    pub routes: Vec<ox_desk_core::store::Route>,
}

pub(crate) struct EnsureInventory {
    pub query: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Destination {
    pub host: Option<String>,
    registration: EntityId,
    pub scope: Option<Scope>,
    pub thread: Option<String>,
}
pub(crate) struct PaletteRow {
    pub title: String,
    pub context: String,
    pub target: Destination,
    pub online: bool,
}
/// A slash command carries its own arguments, including worker and handoff destinations.
pub(crate) struct Query<'a> {
    hosts: Vec<&'a str>,
    labels: Vec<&'a str>,
    waiting: bool,
    pub text: String,
}
impl<'a> Query<'a> {
    pub fn parse(raw: &'a str) -> Self {
        let mut query = Self {
            hosts: vec![],
            labels: vec![],
            waiting: false,
            text: String::new(),
        };
        let mut words = Vec::new();
        for word in raw.split_whitespace() {
            if let Some(name) = word.strip_prefix('@') {
                query.hosts.push(name);
            } else if let Some(label) = word.strip_prefix("label:") {
                query.labels.push(label);
            } else if word == "is:waiting" {
                query.waiting = true;
            } else {
                words.push(word);
            }
        }
        query.text = words.join(" ");
        query
    }
    pub fn scoped(&self) -> bool {
        self.waiting || !self.hosts.is_empty() || !self.labels.is_empty()
    }
    fn accepts(&self, inventory: &Inventory, id: &Option<String>) -> bool {
        let name = inventory.display_name().to_lowercase();
        let saved = inventory.link.remote().map(|h| h.name.to_lowercase());
        self.hosts.iter().all(|q| {
            name.contains(q)
                || saved.as_deref().is_some_and(|s| s.contains(q))
                || id
                    .as_deref()
                    .or_else(|| inventory.info.as_ref()?.id.as_deref())
                    .is_some_and(|id| id.starts_with(q))
                || (id.is_none() && ("local".starts_with(q) || "this computer".contains(q)))
        }) && self.labels.iter().all(|q| {
            inventory
                .info
                .as_ref()
                .is_some_and(|h| h.labels.iter().any(|l| l.eq_ignore_ascii_case(q)))
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Projects,
    Sessions,
    Inbox,
}

impl super::Workspace {
    pub(super) fn ensure_library(&mut self, query: Option<&Query>, cx: &mut Context<Self>) {
        if query.is_none_or(Query::scoped)
            && self.local.is_some()
            && !self.inventory.contains_key(&None)
        {
            self.scoped_search = true;
            self.sync_inventory(cx);
        }
        for (id, host) in &self.inventory {
            if let Some(q) = query
                && (q.waiting
                    || (!q.scoped() && id == &self.active)
                    || (host.read(cx).online && !q.accepts(host.read(cx), id)))
            {
                continue;
            }
            host.update(cx, |h, cx| h.load_saved(cx));
        }
    }
    pub(crate) fn loading_results(&self, query: &Query, cx: &App) -> bool {
        self.inventory.iter().any(|(id, entry)| {
            let host = entry.read(cx);
            query.accepts(host, id)
                && ((!host.online && host.error.is_none()) || (host.scanning && !query.waiting))
        })
    }
    pub(super) fn sync_inventory(&mut self, cx: &mut Context<Self>) {
        super::notifications::remove_unpaired(cx);
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        cx.background_spawn(async { cache::remove_unpaired() })
            .detach();
        let saved = &cx.global::<crate::preferences::HostDirectory>().0;
        let mut links: Vec<_> = saved.iter().cloned().map(HostLink::Remote).collect();
        if (!links.is_empty() || self.scoped_search) && self.local.is_some() {
            links.push(HostLink::Local);
        }
        self.inventory.retain(|_, entry| {
            let current = &entry.read(cx).link;
            links.iter().any(|l| match (current.remote(), l.remote()) {
                (Some(a), Some(b)) => a.same_pairing(b) && a.origin == b.origin,
                (None, None) => true,
                _ => false,
            })
        });
        for link in links {
            let id = link.remote().map(|h| h.id.clone());
            self.inventory.entry(id).or_insert_with(|| {
                let entry = cx.new(|cx| Inventory::start(link, cx));
                cx.observe(&entry, |s, _, cx| s.fleet_changed(cx)).detach();
                entry
            });
        }
        self.tree
            .retain(&self.inventory.values().map(Entity::entity_id).collect());
    }
    pub(crate) fn palette_rows(
        &self,
        query: &Query,
        current: Option<(Option<&str>, &crate::sidebar_state::SidebarState)>,
        cx: &App,
    ) -> Vec<PaletteRow> {
        let mut rows = Vec::new();
        let mut counts = HashMap::new();
        for entry in self.inventory.values() {
            *counts
                .entry(entry.read(cx).display_name().to_lowercase())
                .or_insert(0usize) += 1;
        }
        for (id, entry) in &self.inventory {
            if !query.scoped() && current.is_some_and(|(active, _)| id.as_deref() == active) {
                continue;
            }
            let catalog = entry.read(cx);
            if !query.accepts(catalog, id) {
                continue;
            }
            let name = catalog.display_name();
            let identity = catalog
                .link
                .remote()
                .map_or("This computer", |h| h.origin.as_str());
            let online = catalog.online();
            let name = if counts.get(&name.to_lowercase()).copied().unwrap_or(0) > 1 {
                format!("{name} ({identity})")
            } else {
                name.to_owned()
            };
            if !query.waiting {
                rows.push(PaletteRow {
                    title: name.to_owned(),
                    context: format!("Host · {identity} · {}", catalog.status()),
                    target: Destination {
                        host: id.clone(),
                        registration: entry.entity_id(),
                        scope: None,
                        thread: None,
                    },
                    online,
                });
            }
            let view = match id {
                None => self.local.as_ref(),
                Some(id) => self.hosts.get(id),
            };
            let sidebar = if let Some((_, sidebar)) =
                current.filter(|(active, _)| id.as_deref() == *active)
            {
                sidebar
            } else {
                view.map_or(&catalog.saved_sidebar, |v| v.read(cx).sidebar_state())
            };
            for (scope, thread, mut title) in catalog.rows(query.waiting) {
                if let Some(meta) = thread.as_ref().and_then(|sid| {
                    sidebar.thread(&crate::sidebar_state::ThreadMetaKey::new(
                        scope.project.clone(),
                        scope.route.runtime,
                        sid.clone(),
                    ))
                }) {
                    if meta.hidden && !query.waiting {
                        continue;
                    }
                    if let Some(custom) = &meta.custom_title {
                        title.clone_from(custom);
                    }
                }
                let context = format!(
                    "{name} · {} · {}{}",
                    scope.project.display(),
                    scope.route.label(),
                    if online { "" } else { " · Offline" }
                );
                rows.push(PaletteRow {
                    title,
                    context,
                    target: Destination {
                        host: id.clone(),
                        registration: entry.entity_id(),
                        scope: Some(scope),
                        thread,
                    },
                    online,
                });
            }
        }
        rows.sort_by(|a, b| {
            a.target
                .scope
                .is_some()
                .cmp(&b.target.scope.is_some())
                .then_with(|| {
                    (&a.title, &a.context, &a.target.host, &a.target.thread).cmp(&(
                        &b.title,
                        &b.context,
                        &b.target.host,
                        &b.target.thread,
                    ))
                })
        });
        rows
    }
    pub(crate) fn handoff_destinations(
        &self,
        link: &HostLink,
        path: &std::path::Path,
        cx: &App,
    ) -> Vec<HandoffDestination> {
        let current = link.remote().map(|h| h.id.clone());
        let Some(repo) = self
            .inventory
            .get(&current)
            .and_then(|e| e.read(cx).directory.as_ref())
            .and_then(|dir| dir.checkouts.iter().find(|p| p.path == path))
            .and_then(|p| p.repository.as_ref())
        else {
            return Vec::new();
        };
        let mut matches = Vec::new();
        for (id, e) in &self.inventory {
            let host = e.read(cx);
            if id == &current || !host.online() {
                continue;
            }
            let Some(dir) = &host.directory else { continue };
            let Some(uuid) = host.info.as_ref().and_then(|i| i.id.as_ref()) else {
                continue;
            };
            for p in dir
                .checkouts
                .iter()
                .filter(|p| p.repository.as_ref() == Some(repo))
            {
                matches.push(HandoffDestination {
                    registration: e.entity_id(),
                    repository: repo.clone(),
                    link: host.link.clone(),
                    id: uuid.clone(),
                    name: host.display_name().to_owned(),
                    project: p.path.clone(),
                    routes: dir.routes.clone(),
                });
            }
        }
        matches.sort_by(|a, b| (&a.name, &a.id, &a.project).cmp(&(&b.name, &b.id, &b.project)));
        matches
    }
    pub(crate) fn handoff_still_visible(
        &self,
        source: &Option<String>,
        target: &Option<String>,
        registration: EntityId,
    ) -> bool {
        self.settings.is_none()
            && self.page.is_none()
            && (&self.active == source || &self.active == target)
            && self
                .inventory
                .get(target)
                .is_some_and(|e| e.entity_id() == registration)
    }
    pub(crate) fn reachable(&self, target: &Destination, cx: &App) -> bool {
        self.inventory.get(&target.host).is_some_and(|i| {
            i.entity_id() == target.registration
                && i.read(cx)
                    .can_open(target.scope.as_ref(), target.thread.as_deref())
        })
    }
    pub(crate) fn linked_worker_lead(
        &self,
        lead: &ox_desk_core::origin::LinkedParent,
        cx: &App,
    ) -> Option<(String, Option<Destination>)> {
        let (host, entry) = self.inventory.iter().find(|(host, entry)| {
            host.as_deref() == Some(lead.host.as_str())
                || entry
                    .read(cx)
                    .info
                    .as_ref()
                    .and_then(|info| info.id.as_deref())
                    == Some(lead.host.as_str())
        })?;
        let label = format!("Lead on {}", self.host_name(host, cx));
        let destination = lead
            .parent
            .scope
            .as_ref()
            .zip(lead.parent.session.as_ref())
            .map(|(scope, thread)| Destination {
                host: host.clone(),
                registration: entry.entity_id(),
                scope: Some(scope.clone()),
                thread: Some(thread.clone()),
            })
            .filter(|destination| self.reachable(destination, cx));
        Some((label, destination))
    }
    pub(crate) fn open_destination(
        &mut self,
        target: &Destination,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.reachable(target, cx) {
            return;
        }
        if let Some(scope) = &target.scope {
            self.open_result(
                target.host.clone(),
                scope.clone(),
                target.thread.clone(),
                w,
                cx,
            );
        } else if let Some(entry) = self.inventory.get(&target.host)
            && entry.read(cx).online()
        {
            if let Some(host) = entry.read(cx).link.remote().cloned() {
                self.select(host, w, cx);
            } else {
                self.set_active(None);
                self.settings = None;
                self.page = None;
                cx.notify();
            }
            if let Some(view) = self.visible_host() {
                view.update(cx, |h, cx| h.focus_selection(cx));
            }
        }
    }
    fn open_result(
        &mut self,
        id: Option<String>,
        scope: Scope,
        thread: Option<String>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_confirmed_result(id, scope, thread, false, w, cx)
    }
    pub(crate) fn open_confirmed_result(
        &mut self,
        id: Option<String>,
        scope: Scope,
        thread: Option<String>,
        confirmed: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.inventory.get(&id) else {
            return;
        };
        if !entry.read(cx).online()
            || (!confirmed && !entry.read(cx).can_open(Some(&scope), thread.as_deref()))
        {
            return;
        }
        let row = thread
            .as_deref()
            .and_then(|id| entry.read(cx).native_row(&scope, id))
            .cloned();
        if let Some(host) = entry.read(cx).link.remote().cloned()
            && !self.hosts.contains_key(&host.id)
        {
            self.show_project(host, scope.project.clone(), scope.route.clone(), w, cx);
        }
        let host = match &id {
            Some(id) => self.hosts.get(id),
            None => self.local.as_ref(),
        };
        if let Some(view) = host {
            view.update(cx, |host, cx| host.show_thread(scope, thread, row, w, cx));
            self.set_active(id);
            self.settings = None;
            self.page = None;
            cx.notify();
        }
    }
    pub(super) fn page_rows(&self, page: Page, cx: &App) -> Vec<PaletteRow> {
        let value = self.filter.read(cx).value().trim().to_lowercase();
        let mut query = Query::parse(&value);
        query.waiting |= page == Page::Inbox;
        let mut rows = self.palette_rows(&query, None, cx);
        rows.retain(|row| {
            let text = format!("{} {}", row.title, row.context).to_lowercase();
            query
                .text
                .split_whitespace()
                .all(|term| text.contains(term))
        });
        rows.sort_by(|a, b| {
            (
                &a.target.host,
                a.target.scope.as_ref().map(|s| &s.project),
                &a.title,
            )
                .cmp(&(
                    &b.target.host,
                    b.target.scope.as_ref().map(|s| &s.project),
                    &b.title,
                ))
        });
        rows
    }
    pub(super) fn fleet_view(&self, page: Page, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::{appearance::Palette, kit};
        let p = Palette::get(cx);
        let mut panel = kit::col()
            .id("fleet-sessions")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_2()
            .p_3()
            .child(gpui_component::input::Input::new(&self.filter));
        let rows = self.page_rows(page, cx);
        if rows.is_empty() {
            let raw = self.filter.read(cx).value().to_lowercase();
            let msg = if self.loading_results(&Query::parse(&raw), cx) {
                "Loading sessions…"
            } else if page == Page::Inbox {
                "No matching sessions need a response."
            } else {
                "No matching sessions."
            };
            panel = panel.child(kit::small(p, msg));
        }
        for (index, row) in rows.into_iter().enumerate() {
            panel = panel.child(
                kit::col()
                    .child(kit::button(
                        p,
                        ("fleet-result", index),
                        row.title,
                        false,
                        row.online,
                        cx.listener(move |s, _, w, cx| s.open_destination(&row.target, w, cx)),
                    ))
                    .child(kit::small(p, row.context)),
            );
        }
        panel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::store::Route;
    #[::core::prelude::v1::test]
    fn short_reconnect_keeps_last_known_metadata_without_admitting_cached_sessions() {
        use ox_desk_core::host_protocol::{ProjectCheckout, ProjectDirectory};
        let mut host = Inventory::default();
        let scope = Scope {
            project: "/same".into(),
            route: ox_desk_core::store::Route::CODEX_SUBSCRIPTION,
        };
        let row = (
            scope.clone(),
            Some("cached".to_owned()),
            "Saved title".to_owned(),
        );
        host.previous = vec![row.clone()];
        host.receive(CatalogEvent::Host(
            serde_json::from_value(
                serde_json::json!({"id":"studio","name":"Studio","version":"dev","epoch":"one"}),
            )
            .unwrap(),
        ));
        host.receive(CatalogEvent::Projects(Some(ProjectDirectory {
            checkouts: vec![ProjectCheckout {
                path: scope.project.clone(),
                repository: None,
            }],
            routes: vec![scope.route.clone()],
        })));
        host.receive(CatalogEvent::Disconnected(io::ErrorKind::UnexpectedEof));
        host.receive(CatalogEvent::Reset);
        assert!(!host.can_open(Some(&scope), Some("cached")));
        host.receive(CatalogEvent::Disconnected(io::ErrorKind::UnexpectedEof));
        assert_eq!(host.display_name(), "Studio");
        assert_eq!(
            host.directory.as_ref().unwrap().checkouts[0].path,
            scope.project
        );
        assert_eq!(host.rows(false), [row]);
        host.receive(CatalogEvent::NoHostStatus);
        host.receive(CatalogEvent::Projects(None));
        assert!(host.info.is_none() && host.directory.is_none());
        host.apply_saved(HashMap::new(), true);
        host.receive(CatalogEvent::Disconnected(io::ErrorKind::UnexpectedEof));
        assert!(host.rows(false).is_empty());
    }

    #[::core::prelude::v1::test]
    fn filters_accept_host_name_id_local_alias_and_labels_together() {
        let mut host = Inventory::default();
        host.receive(CatalogEvent::Host(serde_json::from_value(serde_json::json!({
            "name":"Build Station","id":"beef0000", "labels":["Heavy"], "epoch":"running","version":"dev"
        })).unwrap()));
        for query in ["@build", "@beef", "@local", "@this", "@build label:heavy"] {
            assert!(Query::parse(query).accepts(&host, &None), "{query}");
        }
        let filter = Query::parse("launch @build is:waiting label:heavy repair");
        assert!(filter.waiting && filter.accepts(&host, &None));
        assert_eq!(filter.text, "launch repair");
        assert!(!Query::parse("@laptop").accepts(&host, &None));
        assert!(!Query::parse("@build label:gpu").accepts(&host, &None));
        assert!(!Query::parse("@local").accepts(&host, &Some("beef0000".into())));
    }
    #[::core::prelude::v1::test]
    fn saved_catalog_survives_partial_scans_and_routes_remain_distinct() {
        let mut host = Inventory::default();
        let scope = Scope {
            project: "/work/ox".into(),
            route: Route::codex_api("a/model".into()),
        };
        let other = Scope {
            route: Route::codex_api("b/model".into()),
            ..scope.clone()
        };
        let saved = |title: &str| {
            HashMap::from([(
                "same".into(),
                CatalogRow {
                    title: title.into(),
                    native: None,
                },
            )])
        };
        host.apply_saved(HashMap::from([(scope.clone(), saved("first"))]), true);
        host.apply_saved(HashMap::from([(other.clone(), saved("second"))]), false);
        host.receive(CatalogEvent::Reset);
        assert_eq!(host.rows(false).len(), 2);
        host.receive(CatalogEvent::Archived(
            other.clone(),
            HashSet::from(["same".into()]),
        ));
        assert_eq!(
            host.rows(false),
            vec![(scope.clone(), Some("same".into()), "first".into())]
        );
        assert!(!host.can_open(Some(&other), Some("same")));
        assert!(host.can_open(Some(&scope), Some("same")));
        host.receive(CatalogEvent::Disconnected(io::ErrorKind::ConnectionReset));
        assert!(!host.can_open(Some(&scope), Some("same")));
        host.apply_saved(HashMap::new(), true);
        assert!(host.rows(false).is_empty());
    }
    #[::core::prelude::v1::test]
    fn identical_session_ids_and_paths_stay_on_their_host_across_reconnect() {
        let scope = Scope {
            project: "/work/ox".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let mut hosts = [
            Inventory::default(),
            Inventory::default(),
            Inventory::default(),
        ];
        for (index, host) in hosts.iter_mut().enumerate() {
            host.receive(CatalogEvent::Reset);
            host.receive(CatalogEvent::Replace(
                scope.clone(),
                vec![SessionView {
                    id: "same".into(),
                    title: format!("Thread on {index}"),
                    status: "waiting".into(),
                    parent: None,
                    updated_at: 0,
                    capabilities: vec![],
                    detail: Default::default(),
                }],
            ));
            host.receive(CatalogEvent::Waiting(
                scope.clone(),
                HashSet::from(["same".into()]),
            ));
        }
        hosts[1].receive(CatalogEvent::Disconnected(io::ErrorKind::ConnectionReset));
        hosts[0].receive(CatalogEvent::Waiting(scope.clone(), Default::default()));
        assert!(hosts[0].rows(true).is_empty());
        assert!(!hosts[1].online());
        assert!(hosts[1].last_seen.is_some());
        assert_eq!(hosts[1].rows(true)[0].2, "Thread on 1");
        assert!(hosts[2].online());
        assert_eq!(hosts[2].rows(true)[0].2, "Thread on 2");
        hosts[1].receive(CatalogEvent::Reset);
        assert_eq!(hosts[1].waiting(), 0);
        assert_eq!(hosts[2].waiting(), 1);
        hosts[0].receive(CatalogEvent::DraftWaiting(scope.clone(), true));
        assert_eq!(
            hosts[0].rows(true),
            vec![(scope.clone(), None, "New session approval".into())]
        );
        hosts[0].receive(CatalogEvent::DraftWaiting(scope, false));
        assert!(hosts[0].rows(true).is_empty());
    }
}
