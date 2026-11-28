//! Paged native metadata and the merged navigation index. Opening a row is the only
//! operation here that creates a project workbench.
use super::*;
use crate::sidebar_state::{GroupingMode, ThreadMetaKey};
use ox_desk_core::host_protocol::{Scope as HostScope, SessionOutcome, SessionResult};
use ox_desk_core::origin::{Origin, Relation};
use ox_desk_core::runtime::Runtime;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Scope {
    pub runtime: Runtime,
    pub api: bool,
}
impl Scope {
    pub(super) fn saved_route(
        self,
        row: &ThreadRow,
        aliases: &[String],
        provider: Option<&str>,
    ) -> Option<Route> {
        match self.runtime {
            Runtime::Claude => row.saved_runtime_route(aliases).filter(|r| self.matches(r)),
            Runtime::Codex if !self.api && row.model_provider == "openai" => {
                Some(Route::CODEX_SUBSCRIPTION)
            }
            Runtime::Codex if self.api && provider == Some(&row.model_provider) => row
                .model
                .as_ref()
                .filter(|m| aliases.contains(m))
                .cloned()
                .map(Route::codex_api),
            Runtime::Antigravity if !self.api && row.model_provider == "antigravity" => {
                Some(Runtime::Antigravity.route(None))
            }
            _ => None,
        }
    }

    pub fn matches(self, route: &Route) -> bool {
        self.runtime == route.runtime && self.api == route.is_api()
    }
}

#[derive(Clone, Copy)]
pub(super) enum Location {
    Live(usize, usize),
    Saved(usize),
    Hosted(usize),
    PinnedCache(usize),
}

#[derive(PartialEq)]
pub(super) struct Saved {
    pub row: ThreadRow,
    pub scope: Scope,
}

#[derive(PartialEq)]
struct Hosted {
    saved: Saved,
    route: Route,
    status: String,
    /// The immutable launch stamp. Its parent reference is the authority for
    /// nesting, even when the parent runs in another checkout or billing route.
    origin: Option<Origin>,
    /// The model the runtime confirmed, once it has answered.
    actual_model: Option<String>,
    result: Option<SessionResult>,
}
impl Hosted {
    fn from_view(
        scope: &ox_desk_core::host_protocol::Scope,
        view: &ox_desk_core::host_protocol::SessionView,
    ) -> Option<Self> {
        if view.detail["owned"] != true {
            return None;
        }
        let row = ox_desk_core::store::replica::row(view)?;
        if row.cwd != scope.project {
            return None;
        }
        Some(Self {
            saved: Saved {
                row,
                scope: Scope {
                    runtime: scope.route.runtime,
                    api: scope.route.is_api(),
                },
            },
            route: scope.route.clone(),
            status: view.status.clone(),
            origin: serde_json::from_value(view.detail["origin"].clone()).ok(),
            actual_model: view.detail["worker_model"]["actual_model"]
                .as_str()
                .or_else(|| view.detail["worker_marks"]["actual_model"].as_str())
                .map(str::to_owned),
            result: view.last_result(),
        })
    }
}
#[derive(Default)]
pub(super) struct Discovery {
    pub rows: Vec<Saved>,
    hosted: Vec<Hosted>,
    pub pinned_cache: Vec<Saved>,
    // Cold host rows carry worker updates even before native discovery supplies their titles.
    worker_notices: HashMap<ox_desk_core::host_protocol::Scope, HashMap<String, String>>,
    archived: HashMap<ox_desk_core::host_protocol::Scope, HashSet<String>>,
    watch: Option<platform::CatalogWatch>,
    pub revision: usize,
    pub indexed: Option<(usize, String)>,
    pub pending: usize,
    /// The last discovery failure, so a later success can clear the banner it raised.
    pub error: Option<String>,
    removed: HashMap<Scope, HashMap<PathBuf, HashSet<String>>>,
    /// Parents whose child rows are folded away, by project and thread id.
    pub collapsed: HashSet<(PathBuf, String)>,
    /// Day groups the user folded in the sidebar.
    pub folded_days: HashSet<Option<chrono::NaiveDate>>,
    /// Each indexed child's parent, by project and thread id, from the last index.
    pub parents: HashMap<(PathBuf, String), (PathBuf, String)>,
}

/// What a parent session has under it, counted over all descendants.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Tree {
    /// Direct children.
    pub children: usize,
    pub running: usize,
    pub waiting: usize,
    /// The first running descendant, in catalog order.
    pub first_running: Option<String>,
}

/// Tree counts for every parent in the navigation index, keyed by project and thread id.
/// Set by `index_catalog`; read by the workbench activity line and the sidebar rows.
#[derive(Default)]
pub(crate) struct WorkerIndex {
    pub trees: HashMap<(PathBuf, String), Tree>,
    pub parents: HashMap<(PathBuf, String), ParentLink>,
    pub results: HashMap<(HostScope, String), Vec<WorkerRow>>,
}

pub(crate) struct WorkerRow {
    pub target: ParentLink,
    pub status: &'static str,
    pub billing: String,
    pub access: String,
    pub model: Option<String>,
}

fn worker_status(status: &str, result: Option<&SessionResult>) -> &'static str {
    match status {
        "needs_input" => "Needs you",
        "running" => "Running",
        "queued" => "Queued",
        "failed" => "Failed",
        "interrupted" => "Interrupted",
        _ => match result.map(|result| result.outcome) {
            Some(SessionOutcome::Completed) => "Completed",
            Some(SessionOutcome::Failed) => "Failed",
            Some(SessionOutcome::Stopped) => "Stopped",
            None => "Open",
        },
    }
}

/// Where a worker's recorded parent lives. The origin stamp carries the parent's
/// full host scope and session key, so navigation works across projects, routes
/// and filtered views without looking the parent up in the child's project.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParentLink {
    pub project: PathBuf,
    /// The exact parent route when the host recorded a hosted parent scope.
    pub route: Option<Route>,
    pub id: String,
    /// The parent's title when it is visible in the current index.
    pub title: Option<String>,
}
impl ParentLink {
    /// A launch stamp names its parent. Handoffs record a source rather than a
    /// worker parent and keep their existing presentation.
    fn from_origin(origin: &Origin) -> Option<Self> {
        if origin.relation == Relation::Handoff || origin.linked.is_some() {
            return None;
        }
        let id = origin.parent.session.clone()?;
        let (project, route) = match &origin.parent.scope {
            Some(scope) => (scope.project.clone(), Some(scope.route.clone())),
            // A terminal parent has no host scope. Its project and native ID still
            // let a same-project catalog row resolve it.
            None => (origin.parent.project.clone(), None),
        };
        Some(Self {
            project,
            route,
            id,
            title: None,
        })
    }
}
/// Resolve a recorded parent against the entries in the current index. Exact
/// scope first, then the same project, so a filtered or differently routed view
/// still groups a worker under its recorded parent when that parent is present.
fn resolve_parent_key<'a, K: ?Sized>(
    parent: &ParentLink,
    by_scope: &HashMap<(PathBuf, Scope, String), &'a K>,
    by_project: &HashMap<(PathBuf, String), &'a K>,
) -> Option<&'a K> {
    parent
        .route
        .as_ref()
        .and_then(|route| {
            by_scope
                .get(&(
                    parent.project.clone(),
                    Scope {
                        runtime: route.runtime,
                        api: route.is_api(),
                    },
                    parent.id.clone(),
                ))
                .copied()
        })
        .or_else(|| {
            by_project
                .get(&(parent.project.clone(), parent.id.clone()))
                .copied()
        })
}

/// Unfolds every collapsed ancestor of `start`. Returns whether any fold was removed.
/// Bounded by the parent map's size, so a malformed parent cycle cannot spin.
fn unfold_ancestors(
    parents: &HashMap<(PathBuf, String), (PathBuf, String)>,
    collapsed: &mut HashSet<(PathBuf, String)>,
    start: (PathBuf, String),
) -> bool {
    let mut unfolded = false;
    let mut at = start;
    for _ in 0..parents.len() {
        let Some(parent) = parents.get(&at).cloned() else {
            break;
        };
        unfolded |= collapsed.remove(&parent);
        at = parent;
    }
    unfolded
}

/// The identity of one catalog row: project, scope, thread id and provider.
type Key = (PathBuf, Scope, String, String);

/// One deduplicated sidebar row before parent links are resolved.
struct Indexed {
    order: (i64, u128),
    loc: Location,
    parent: Option<String>,
    quality: (bool, bool, bool, bool, bool, bool, i64),
    origin: Option<Origin>,
    result: Option<(HostScope, SessionResult)>,
}

impl Indexed {
    fn merge(&mut self, mut incoming: Self) {
        // Display quality and activity can differ across catalogs. Preserve
        // the host stamp even when a native row has a better title.
        let order = self.order.max(incoming.order);
        if incoming.quality > self.quality {
            std::mem::swap(self, &mut incoming);
        }
        self.order = order;
        self.parent = self.parent.take().or(incoming.parent);
        self.origin = self.origin.take().or(incoming.origin);
        if incoming.result.as_ref().is_some_and(|(_, incoming)| {
            self.result
                .as_ref()
                .is_none_or(|(_, current)| incoming.completed_at > current.completed_at)
        }) {
            self.result = incoming.result;
        }
    }
}

impl Discovery {
    pub(super) fn all(&self) -> impl Iterator<Item = &Saved> {
        self.hosted.iter().map(|h| &h.saved).chain(&self.rows)
    }
    /// Count of sessions waiting on operator input in `(project, route)`, from the live catalog.
    pub(super) fn waiting_count(&self, project: &Path, route: &Route) -> usize {
        self.hosted
            .iter()
            .filter(|h| {
                h.saved.row.cwd == project && &h.route == route && h.status == "needs_input"
            })
            .count()
    }
    /// Fold or unfold one day group of the sidebar.
    pub(super) fn toggle_day(&mut self, day: Option<chrono::NaiveDate>) {
        if !self.folded_days.remove(&day) {
            self.folded_days.insert(day);
        }
        self.revision += 1;
    }
    /// Fold or unfold a parent's children in the sidebar.
    pub(super) fn toggle(&mut self, project: PathBuf, id: String) {
        let key = (project, id);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.revision += 1;
    }
    pub(super) fn worker_notice(&self, project: &Path, route: &Route, id: &str) -> Option<&str> {
        let scope = ox_desk_core::host_protocol::Scope {
            project: project.to_owned(),
            route: route.clone(),
        };
        self.worker_notices.get(&scope)?.get(id).map(String::as_str)
    }
    fn live_event(&mut self, event: platform::CatalogEvent) -> bool {
        use platform::CatalogEvent;
        let summary = |view: &ox_desk_core::host_protocol::SessionView| {
            let event = &view.detail["worker_notice"];
            let job = event["job_id"].as_u64()?;
            let status = event["status"].as_str()?.replace('_', " ");
            Some(match event["host"].as_str() {
                Some(host) => format!("Worker #{job} {status} on {host}"),
                None => format!("Worker #{job} {status}"),
            })
        };
        match event {
            CatalogEvent::Projects(_)
            | CatalogEvent::NoHostStatus
            | CatalogEvent::Host(_)
            | CatalogEvent::DraftWaiting(..)
            | CatalogEvent::Waiting(..)
            | CatalogEvent::Disconnected(_)
            | CatalogEvent::Notice(_) => false,
            CatalogEvent::Reset => {
                let changed = !self.hosted.is_empty()
                    || !self.archived.is_empty()
                    || !self.worker_notices.is_empty();
                self.worker_notices.clear();
                self.archived.clear();
                self.hosted.clear();
                changed
            }
            CatalogEvent::Archived(scope, ids) => {
                if ids.is_empty() {
                    return self.archived.remove(&scope).is_some();
                }
                if self.archived.get(&scope) == Some(&ids) {
                    return false;
                }
                self.archived.insert(scope, ids);
                true
            }
            CatalogEvent::Replace(scope, views) => {
                let updates: HashMap<_, _> = views
                    .iter()
                    .filter_map(|v| Some((v.id.clone(), summary(v)?)))
                    .collect();
                let previous = self.worker_notices.remove(&scope).unwrap_or_default();
                let changed = previous != updates;
                if !updates.is_empty() {
                    self.worker_notices.insert(scope.clone(), updates);
                }
                let rows: Vec<_> = views
                    .iter()
                    .filter_map(|v| Hosted::from_view(&scope, v))
                    .collect();
                let matches =
                    |r: &Hosted| r.route == scope.route && r.saved.row.cwd == scope.project;
                if self.hosted.iter().filter(|r| matches(r)).eq(&rows) {
                    return changed;
                }
                self.hosted.retain(|r| !matches(r));
                self.hosted.extend(rows);
                true
            }
            CatalogEvent::Update(scope, view) => {
                let changed = if let Some(label) = summary(&view) {
                    let old = self
                        .worker_notices
                        .entry(scope.clone())
                        .or_default()
                        .insert(view.id.clone(), label.clone());
                    old.as_ref() != Some(&label)
                } else {
                    self.worker_notices
                        .get_mut(&scope)
                        .and_then(|rows| rows.remove(&view.id))
                        .is_some()
                };
                let Some(row) = Hosted::from_view(&scope, &view) else {
                    return changed;
                };
                if let Some(old) = self.hosted.iter_mut().find(|r| {
                    r.route == scope.route
                        && r.saved.row.cwd == scope.project
                        && r.saved.row.id == view.id
                }) {
                    if *old == row {
                        return changed;
                    }
                    *old = row;
                } else {
                    self.hosted.push(row)
                }
                true
            }
        }
    }
    pub(super) fn route_status(
        &self,
        project: &Path,
        scope: Scope,
        row: &ThreadRow,
    ) -> Option<(Route, Option<&'static str>)> {
        let h = self.hosted.iter().find(|h| {
            h.saved.scope == scope
                && h.saved.row.cwd == project
                && h.saved.row.id == row.id
                && h.saved.row.model_provider == row.model_provider
        })?;
        let status = match h.status.as_str() {
            "needs_input" => Some("Needs you"),
            "running" => Some("Running"),
            "failed" => Some("Error"),
            "interrupted" => Some("Interrupted"),
            "native_live" => Some(match h.route.runtime {
                Runtime::Codex => "Live in Codex",
                Runtime::Claude => "Live in Claude Code",
                Runtime::Antigravity => "Live in Antigravity",
                Runtime::OpenCode => "Live in OpenCode",
            }),
            _ => None,
        };
        Some((h.route.clone(), status))
    }

    fn worker_results(&self) -> HashMap<(HostScope, String), Vec<WorkerRow>> {
        let mut workers = HashMap::<_, Vec<_>>::new();
        let mut ordered: Vec<_> = self.hosted.iter().collect();
        ordered.sort_by(|a, b| {
            b.saved
                .row
                .updated_at
                .cmp(&a.saved.row.updated_at)
                .then_with(|| a.saved.row.id.cmp(&b.saved.row.id))
        });
        for worker in ordered {
            let Some(origin) = worker
                .origin
                .as_ref()
                .filter(|origin| origin.relation == Relation::Worker && origin.linked.is_none())
            else {
                continue;
            };
            let (Some(parent), Some(session)) = (&origin.parent.scope, &origin.parent.session)
            else {
                continue;
            };
            let row = &worker.saved.row;
            workers
                .entry((parent.clone(), session.clone()))
                .or_default()
                .push(WorkerRow {
                    target: ParentLink {
                        project: row.cwd.clone(),
                        route: Some(worker.route.clone()),
                        title: Some(row.title().to_owned()),
                        id: row.id.clone(),
                    },
                    status: worker_status(&worker.status, worker.result.as_ref()),
                    billing: worker.route.label(),
                    access: origin.ceiling.label(origin.runtime).to_owned(),
                    model: worker.actual_model.clone(),
                });
        }
        workers
    }

    fn replace(&mut self, runtime: Runtime, api: Option<bool>, rows: Vec<ThreadRow>) {
        self.rows.retain(|saved| {
            let belongs =
                saved.scope.runtime == runtime && api.is_none_or(|api| saved.scope.api == api);
            if belongs {
                self.removed
                    .entry(saved.scope)
                    .or_default()
                    .entry(saved.row.cwd.clone())
                    .or_default()
                    .insert(saved.row.id.clone());
            }
            !belongs
        });
        for row in rows.into_iter().filter(|r| r.cwd.is_absolute()) {
            let scope = Scope {
                runtime,
                api: api.unwrap_or_else(|| row.saved_claude_api().unwrap_or(false)),
            };
            if let Some(ids) = self
                .removed
                .get_mut(&scope)
                .and_then(|m| m.get_mut(&row.cwd))
            {
                ids.remove(&row.id);
            }
            self.rows.push(Saved { row, scope });
        }
        self.removed.retain(|_, projects| {
            projects.retain(|_, ids| !ids.is_empty());
            !projects.is_empty()
        });
        self.revision += 1;
    }

    fn removed(&self, project: &Path, scope: Scope, id: &str) -> bool {
        self.removed
            .get(&scope)
            .and_then(|projects| projects.get(project))
            .is_some_and(|ids| ids.contains(id))
    }
}

impl Host {
    pub(super) fn watch_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (watch, events) = match self.link.watch_catalog(false) {
            Ok(pair) => pair,
            Err(error) => {
                self.message = Some(error.to_string());
                return;
            }
        };
        self.discovery.watch = Some(watch);
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(first) = events.recv_async().await {
                if this
                    .update_in(cx, |s, window, cx| {
                        let mut changed = false;
                        for event in std::iter::once(first).chain(events.try_iter()) {
                            if let platform::CatalogEvent::Notice(notice) = &event {
                                notice.show(&s.link, cx);
                            } else {
                                changed |= s.discovery.live_event(event);
                            }
                        }
                        if changed {
                            s.discovery.revision += 1;
                            if window.is_visible() {
                                cx.notify();
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn start_discovery(&mut self, cx: &mut Context<Self>) {
        if self.discovery.pending != 0 {
            return;
        }
        self.discovery.pending = 1;
        let link = self.link.clone();
        let task = cx.background_spawn(async move { link.discover() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.discovery.pending = 0;
                match result {
                    Ok((rows, errors)) => {
                        // A failure the user already saw is over once discovery works again.
                        if s.discovery
                            .error
                            .take()
                            .is_some_and(|e| s.message.as_ref() == Some(&e))
                        {
                            s.message = None;
                        }
                        let mut groups = HashMap::<(Runtime, Option<bool>), Vec<ThreadRow>>::new();
                        for (scope, row) in rows {
                            let runtime = scope.route.runtime;
                            let api = (runtime != Runtime::Claude).then_some(scope.route.is_api());
                            groups.entry((runtime, api)).or_default().push(row);
                        }
                        let sources = [
                            (Runtime::Claude, None),
                            (Runtime::Codex, Some(false)),
                            (Runtime::Codex, Some(true)),
                            (Runtime::Antigravity, Some(false)),
                        ];
                        for (runtime, api) in sources {
                            let mut page = groups.remove(&(runtime, api)).unwrap_or_default();
                            // A failed source must not erase previously visible histories.
                            if !errors.is_empty() {
                                for saved in &s.discovery.rows {
                                    let belongs = saved.scope.runtime == runtime
                                        && api.is_none_or(|api| saved.scope.api == api);
                                    if belongs
                                        && !page
                                            .iter()
                                            .any(|r| r.cwd == saved.row.cwd && r.id == saved.row.id)
                                    {
                                        page.push(saved.row.clone());
                                    }
                                }
                            }
                            s.discovery.replace(runtime, api, page);
                        }
                        for message in errors {
                            eprintln!("Could not discover saved sessions: {message}");
                        }
                    }
                    Err(e) => {
                        let message = e.to_string();
                        s.discovery.error = Some(message.clone());
                        s.message = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn archived(&self, project: &Path, scope: Scope, id: &str, cx: &App) -> bool {
        self.discovery.archived.iter().any(|(origin, ids)| {
            origin.project == project && scope.matches(&origin.route) && ids.contains(id)
        }) || (self.discovery.removed(project, scope, id)
            && !self.discovery.hosted.iter().any(|h| {
                h.saved.row.id == id && h.saved.scope == scope && h.saved.row.cwd == project
            }))
            || self.panes.iter().any(|p| {
                let p = p.read(cx);
                p.project == project
                    && scope.matches(&p.route)
                    && p.store
                        .as_ref()
                        .is_some_and(|s| s.archived_threads().contains(id))
            })
    }

    fn located<'a>(
        &'a self,
        loc: Location,
        cx: &'a App,
    ) -> Option<(&'a ThreadRow, &'a Path, Scope)> {
        match loc {
            Location::Live(pane, index) => {
                let pane = self.panes.get(pane)?.read(cx);
                let row = pane.store.as_ref()?.threads().get(index)?;
                let scope = Scope {
                    runtime: pane.route.runtime,
                    api: row.saved_claude_api().unwrap_or(pane.route.is_api()),
                };
                Some((row, &pane.project, scope))
            }
            Location::Hosted(index) => {
                let h = self.discovery.hosted.get(index)?;
                Some((&h.saved.row, &h.saved.row.cwd, h.saved.scope))
            }
            Location::Saved(index) => {
                let saved = self.discovery.rows.get(index)?;
                Some((&saved.row, &saved.row.cwd, saved.scope))
            }
            Location::PinnedCache(index) => {
                let saved = self.discovery.pinned_cache.get(index)?;
                Some((&saved.row, &saved.row.cwd, saved.scope))
            }
        }
    }

    pub(super) fn located_entry<'a>(
        &'a self,
        entry: &CatalogEntry,
        cx: &'a App,
    ) -> Option<(&'a ThreadRow, &'a Path, Scope)> {
        let CatalogEntry::Thread { loc, .. } = entry else {
            return None;
        };
        self.located(*loc, cx)
    }

    /// The launch stamp for an indexed row. Only host-owned sessions carry one.
    fn origin_at(&self, loc: Location, cx: &App) -> Option<Origin> {
        match loc {
            Location::Live(pane, index) => {
                let pane = self.panes.get(pane)?.read(cx);
                let store = pane.store.as_ref()?;
                let row = store.threads().get(index)?;
                store.origin(&row.id).cloned()
            }
            Location::Hosted(index) => self.discovery.hosted.get(index)?.origin.clone(),
            Location::Saved(_) | Location::PinnedCache(_) => None,
        }
    }

    /// The model the runtime confirmed for an indexed row, when the host saw it.
    pub(super) fn actual_model_at(&self, loc: Location, cx: &App) -> Option<String> {
        match loc {
            Location::Live(pane, index) => {
                let pane = self.panes.get(pane)?.read(cx);
                let store = pane.store.as_ref()?;
                let row = store.threads().get(index)?;
                store.actual_model(&row.id).map(str::to_owned)
            }
            Location::Hosted(index) => self.discovery.hosted.get(index)?.actual_model.clone(),
            Location::Saved(_) | Location::PinnedCache(_) => None,
        }
    }

    fn result_at(
        &self,
        loc: Location,
        row: &ThreadRow,
        cx: &App,
    ) -> Option<(HostScope, SessionResult)> {
        let (route, result) = match loc {
            Location::Live(pane, _) => {
                let pane = self.panes.get(pane)?.read(cx);
                (&pane.route, pane.store.as_ref()?.last_result(&row.id)?)
            }
            Location::Hosted(index) => {
                let hosted = self.discovery.hosted.get(index)?;
                (&hosted.route, hosted.result.as_ref()?)
            }
            Location::Saved(_) | Location::PinnedCache(_) => return None,
        };
        Some((
            HostScope {
                project: row.cwd.clone(),
                route: route.clone(),
            },
            result.clone(),
        ))
    }

    /// Find a session by its recorded identity, even when a search or status
    /// filter keeps it out of the rendered catalog. Discovery rows cover every
    /// project; open workbenches cover rows a scope has already loaded.
    fn find_row(
        &self,
        project: &Path,
        id: &str,
        route: Option<&Route>,
        cx: &App,
    ) -> Option<(ThreadRow, Scope)> {
        for hosted in &self.discovery.hosted {
            if hosted.saved.row.cwd == project
                && hosted.saved.row.id == id
                && route.is_none_or(|route| hosted.route == *route)
            {
                return Some((hosted.saved.row.clone(), hosted.saved.scope));
            }
        }
        for saved in &self.discovery.rows {
            if saved.row.cwd == project
                && saved.row.id == id
                && route.is_none_or(|route| saved.scope.matches(route))
            {
                return Some((saved.row.clone(), saved.scope));
            }
        }
        for pane in &self.panes {
            let pane = pane.read(cx);
            if pane.project != project {
                continue;
            }
            let Some(store) = &pane.store else {
                continue;
            };
            let Some(row) = store.catalog_row(id) else {
                continue;
            };
            let scope = Scope {
                runtime: pane.route.runtime,
                api: row
                    .saved_claude_api()
                    .unwrap_or_else(|| pane.route.is_api()),
            };
            if route.is_none_or(|route| scope.matches(route)) {
                return Some((row.clone(), scope));
            }
        }
        None
    }

    fn status_at(&self, loc: Location, cx: &App) -> Option<&'static str> {
        let (row, project, scope) = self.located(loc, cx)?;
        self.project_row_state(project, row, scope, cx).1
    }

    /// Aggregate of everything under `key`, recording every parent that has children.
    fn walk_tree(
        &self,
        key: &Key,
        children: &HashMap<&Key, Vec<(&Key, i64, Location)>>,
        cx: &App,
        out: &mut HashMap<(PathBuf, String), Tree>,
    ) -> Tree {
        let mut total = Tree::default();
        for (child, _, loc) in children.get(key).into_iter().flatten() {
            let own = self.status_at(*loc, cx);
            let below = self.walk_tree(child, children, cx, out);
            let running = own == Some("Running");
            total.children += 1;
            total.running += below.running + usize::from(running);
            total.waiting += below.waiting + usize::from(own == Some("Needs you"));
            if running {
                total.first_running.get_or_insert_with(|| child.2.clone());
            }
            if let Some(first) = below.first_running {
                total.first_running.get_or_insert(first);
            }
        }
        if total.children > 0 {
            out.insert((key.0.clone(), key.2.clone()), total.clone());
        }
        total
    }

    /// Open a session from the sidebar index, unfolding its parent if needed.
    pub(super) fn open_thread(
        &mut self,
        owner: &Entity<Workbench>,
        link: ParentLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = link.project.clone();
        let id = link.id.clone();
        let selected = owner
            .read(cx)
            .store
            .as_ref()
            .and_then(|s| s.selected())
            .map(str::to_owned);
        // The target may be a worker folded under the session being viewed, or under
        // any other folded ancestor. Unfold the whole chain so its row shows.
        let mut unfolded = selected.is_some_and(|selected| {
            self.discovery
                .collapsed
                .remove(&(project.clone(), selected))
        });
        unfolded |= unfold_ancestors(
            &self.discovery.parents,
            &mut self.discovery.collapsed,
            (project.clone(), id.clone()),
        );
        if unfolded {
            self.discovery.revision += 1;
            self.index_catalog(cx);
        }
        let mut found = self.find_row(&project, &id, link.route.as_ref(), cx);
        // The target may sit in a folded day group: open the groups and look again.
        if found.is_none() && !self.discovery.folded_days.is_empty() {
            self.discovery.folded_days.clear();
            self.discovery.revision += 1;
            self.index_catalog(cx);
            found = self.find_row(&project, &id, link.route.as_ref(), cx);
        }
        if let Some((row, scope)) = found {
            self.activate_row(project, scope, row, window, cx);
        } else if let Some(route) = link.route {
            // Discovery has not listed the parent yet. Its recorded scope is
            // enough to open the right workbench and select the session there.
            let row = ThreadRow {
                worker: true,
                parent_thread_id: None,
                agent_nickname: None,
                id,
                cwd: project.clone(),
                model_provider: String::new(),
                model: None,
                name: link.title,
                preview: String::new(),
                generated_title: None,
                updated_at: 0,
                status: None,
                unread: false,
                activity_order: 0,
            };
            self.open_catalog(project, row, route, window, cx);
        }
    }

    pub(super) fn index_catalog(&mut self, cx: &mut App) {
        let versions: Vec<_> = self
            .panes
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((i, p.read(cx).store.as_ref()?.catalog_revision())))
            .collect();
        let query = self.filter.read(cx).value().trim().to_lowercase();
        let state = (self.discovery.revision, query.clone());
        // `is:running`, `is:waiting` and `is:finished` filter by live status.
        let (status_filter, query) = split_status_filter(&query);
        let running_only = status_filter == Some(StatusFilter::Running);
        if versions == self.catalog_versions && self.discovery.indexed.as_ref() == Some(&state) {
            return;
        }
        self.discovery.indexed = Some(state);
        self.catalog_versions = versions;
        // Synthesize cached rows for pinned threads so they survive restart and render
        // even before discovery completes or if the thread dropped out of the live catalog.
        let mut pinned_cache = Vec::new();
        for (encoded, meta) in &self.sidebar.threads {
            if meta.pinned_at.is_some()
                && !meta.hidden
                && let Some(key) = ThreadMetaKey::decode(encoded)
            {
                pinned_cache.push(Saved {
                    row: meta.cached_row(&key),
                    scope: Scope {
                        runtime: key.runtime,
                        api: meta.cached_api.unwrap_or(false),
                    },
                });
            }
        }
        self.discovery.pinned_cache = pinned_cache;
        let locations = self
            .catalog_versions
            .iter()
            .flat_map(|&(i, _)| {
                (0..self.panes[i]
                    .read(cx)
                    .store
                    .as_ref()
                    .unwrap()
                    .threads()
                    .len())
                    .map(move |j| Location::Live(i, j))
            })
            .chain((0..self.discovery.hosted.len()).map(Location::Hosted))
            .chain((0..self.discovery.rows.len()).map(Location::Saved))
            .chain((0..self.discovery.pinned_cache.len()).map(Location::PinnedCache));
        let mut entries: HashMap<Key, Indexed> = HashMap::new();
        for loc in locations {
            let Some((r, project, scope)) = self.located(loc, cx) else {
                continue;
            };
            if self.archived(project, scope, &r.id, cx) {
                continue;
            }
            let meta_key = ThreadMetaKey::new(project.to_path_buf(), scope.runtime, r.id.clone());
            let meta = self.sidebar.thread(&meta_key);
            if meta.is_some_and(|m| m.hidden) {
                continue;
            }
            if matches!(loc, Location::PinnedCache(_))
                && entries
                    .keys()
                    .any(|k| k.0 == project && k.1.runtime == scope.runtime && k.2 == r.id)
            {
                continue;
            }
            let custom_title = meta.and_then(|m| m.custom_title.as_deref());
            if !query.is_empty()
                && !r.title().to_lowercase().contains(&query)
                && !custom_title.is_some_and(|t| t.to_lowercase().contains(&query))
                && !r.preview.to_lowercase().contains(&query)
                && !project.to_string_lossy().to_lowercase().contains(&query)
                && !matches!(loc, Location::Live(i, _) if self.panes[i].read(cx).query.to_lowercase() == query)
            {
                continue;
            }
            let result = self.result_at(loc, r, cx);
            if let Some(wanted) = status_filter {
                let (_, status) = self.project_row_state(project, r, scope, cx);
                let matches = match (wanted, status) {
                    (StatusFilter::Running, Some("Running")) => true,
                    (StatusFilter::Running, _) => result.is_some(),
                    (StatusFilter::Waiting, Some("Needs you")) => true,
                    (StatusFilter::Finished, Some("Running" | "Needs you")) => false,
                    (StatusFilter::Finished, _) => true,
                    (StatusFilter::Pinned, _) => meta.and_then(|m| m.pinned_at).is_some(),
                    _ => false,
                };
                if !matches {
                    continue;
                }
            }
            let key = (
                project.to_owned(),
                scope,
                r.id.clone(),
                r.model_provider.clone(),
            );
            let activity_at = result.as_ref().map_or(r.updated_at, |(_, result)| {
                r.updated_at.max(result.completed_at)
            });
            let order = (activity_at, r.activity_order);
            // Prefer authoritative, richer identity for rendering. Keep the newest
            // positive activity date independently when two catalogs describe the same row.
            let quality = (
                !matches!(loc, Location::PinnedCache(_)),
                r.worker || r.activity_order != 0,
                r.name.is_some() || r.agent_nickname.is_some(),
                r.generated_title.is_some(),
                !r.preview.trim().is_empty(),
                matches!(loc, Location::Live(..) | Location::Hosted(..)),
                r.updated_at,
            );
            let candidate = Indexed {
                order,
                loc,
                parent: r.parent_thread_id.clone(),
                quality,
                origin: self.origin_at(loc, cx),
                result,
            };
            match entries.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().merge(candidate)
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(candidate);
                }
            }
        }
        let mut ordered: Vec<_> = entries.keys().collect();
        ordered.sort_unstable_by(|a, b| {
            entries[b]
                .order
                .cmp(&entries[a].order)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.runtime.name().cmp(b.1.runtime.name()))
                .then_with(|| a.1.api.cmp(&b.1.api))
                .then_with(|| a.3.cmp(&b.3))
        });
        let unseen_results: HashMap<_, _> = entries
            .iter()
            .filter_map(|(key, entry)| {
                let (scope, result) = entry.result.as_ref()?;
                (!self.seen_results.contains(scope, &key.2, result)
                    && !matches!(self.status_at(entry.loc, cx), Some("Running" | "Needs you")))
                .then_some((key, result))
            })
            .collect();
        let mut just_finished = Vec::new();
        if running_only {
            ordered.retain(|key| {
                let status = self.status_at(entries[*key].loc, cx);
                if status == Some("Running") {
                    return true;
                }
                if let Some(result) = unseen_results.get(key) {
                    just_finished.push((*key, result.completed_at));
                }
                false
            });
            just_finished.sort_by_key(|(_, completed_at)| std::cmp::Reverse(*completed_at));
        }
        let mut roots = Vec::new();
        let mut children = HashMap::new();
        let mut parents: HashMap<(PathBuf, String), (PathBuf, String)> = HashMap::new();
        let mut links: HashMap<(PathBuf, String), ParentLink> = HashMap::new();
        // A host-owned worker runs on its own route and may run in another
        // checkout. Its origin names the exact parent scope and session.
        let by_scope: HashMap<_, _> = entries
            .keys()
            .map(|k| ((k.0.clone(), k.1, k.2.clone()), k))
            .collect();
        let by_project: HashMap<_, _> = entries
            .keys()
            .map(|k| ((k.0.clone(), k.2.clone()), k))
            .collect();
        for key in ordered
            .iter()
            .copied()
            .chain(just_finished.iter().map(|(key, _)| *key))
        {
            let entry = &entries[key];
            let node = (key, entry.order.0, entry.loc);
            let recorded = entry.origin.as_ref().and_then(ParentLink::from_origin);
            let mut resolved = recorded
                .as_ref()
                .and_then(|parent| resolve_parent_key(parent, &by_scope, &by_project));
            // Native subagent rows carry only a parent thread id in the same project.
            if recorded.is_none()
                && entry
                    .origin
                    .as_ref()
                    .is_none_or(|origin| origin.linked.is_none())
                && let Some(parent) = &entry.parent
            {
                resolved = by_scope
                    .get(&(key.0.clone(), key.1, parent.clone()))
                    .or_else(|| by_project.get(&(key.0.clone(), parent.clone())))
                    .copied();
            }
            if let Some(mut link) = recorded {
                if let Some((row, project, scope)) =
                    resolved.and_then(|k| self.located(entries[k].loc, cx))
                {
                    link.title = Some(row.title().to_owned());
                    if link.route.is_none() {
                        link.route = self.project_row_state(project, row, scope, cx).0;
                    }
                }
                links.insert((key.0.clone(), key.2.clone()), link);
            }
            let mkey = ThreadMetaKey::new(key.0.clone(), key.1.runtime, key.2.clone());
            let promoted = self.sidebar.thread(&mkey).is_some_and(|m| {
                m.pinned_at.is_some()
                    || m.section
                        .as_deref()
                        .is_some_and(|id| self.sidebar.sections.iter().any(|s| s.id == id))
            });
            if running_only && self.status_at(entry.loc, cx) != Some("Running") {
                continue;
            }
            if running_only {
                resolved = resolved
                    .filter(|parent| self.status_at(entries[*parent].loc, cx) == Some("Running"));
            }
            if !promoted && let Some(parent_key) = resolved.filter(|parent| *parent != key) {
                parents.insert(
                    (node.0.0.clone(), node.0.2.clone()),
                    (parent_key.0.clone(), parent_key.2.clone()),
                );
                children
                    .entry(parent_key)
                    .or_insert_with(Vec::new)
                    .push(node);
            } else {
                roots.push(node);
            }
        }
        // Counts cover the whole tree, folded or not, so a collapsed parent can still
        // say what is running under it.
        let mut tree = HashMap::new();
        for (key, _, _) in &roots {
            self.walk_tree(key, &children, cx, &mut tree);
        }
        let worker_results = self.discovery.worker_results();
        self.workers.update(cx, |index, _| {
            index.results = worker_results;
            index.trees = tree;
            index.parents = links;
        });
        self.discovery.parents = parents;
        let filtering = !query.is_empty() || status_filter.is_some();
        let mut pinned_roots = Vec::new();
        let mut unpinned_roots = Vec::new();
        for root in roots {
            let mkey =
                ThreadMetaKey::new((root.0).0.clone(), (root.0).1.runtime, (root.0).2.clone());
            if let Some(pinned_at) = self.sidebar.thread(&mkey).and_then(|m| m.pinned_at) {
                let order = self
                    .sidebar
                    .thread(&mkey)
                    .and_then(|m| m.order)
                    .unwrap_or(i64::MAX);
                pinned_roots.push((order, pinned_at, root));
            } else {
                unpinned_roots.push(root);
            }
        }
        // Pinned threads sort by explicit order first, then most recently pinned first.
        pinned_roots.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| (b.2).1.cmp(&(a.2).1))
        });
        let mut catalog = Vec::new();
        let mut ordinal = 0;
        let push_thread = |catalog: &mut Vec<CatalogEntry>,
                           ordinal: &mut usize,
                           key: &Key,
                           timestamp,
                           depth,
                           in_finished_shelf| {
            catalog.push(CatalogEntry::Thread {
                loc: entries[key].loc,
                timestamp,
                ordinal: *ordinal,
                depth,
                result: unseen_results.get(key).map(|result| result.outcome),
                in_finished_shelf,
            });
            *ordinal += 1;
        };
        let push_tree = |catalog: &mut Vec<CatalogEntry>,
                         children: &mut HashMap<&Key, Vec<(&Key, i64, Location)>>,
                         ordinal: &mut usize,
                         root: (&Key, i64, Location)| {
            let mut stack = vec![(root.0, root.1, 0)];
            while let Some((key, timestamp, depth)) = stack.pop() {
                push_thread(catalog, ordinal, key, timestamp, depth, false);
                if !filtering
                    && self
                        .discovery
                        .collapsed
                        .contains(&(key.0.clone(), key.2.clone()))
                {
                    continue;
                }
                if let Some(next) = children.remove(key) {
                    stack.extend(
                        next.into_iter()
                            .rev()
                            .map(|(k, time, _)| (k, time, depth + 1)),
                    );
                }
            }
        };
        let node_rollup = |nodes: &[(&Key, i64, Location)],
                           tree: &HashMap<(PathBuf, String), Tree>| {
            let mut running = 0;
            let mut waiting = 0;
            for (key, _, loc) in nodes {
                let own = self.status_at(*loc, cx);
                if own == Some("Running") {
                    running += 1;
                } else if own == Some("Needs you") {
                    waiting += 1;
                }
                if let Some(sub) = tree.get(&(key.0.clone(), key.2.clone())) {
                    running += sub.running;
                    waiting += sub.waiting;
                }
            }
            (running, waiting)
        };
        let tree_snapshot = self.workers.read(cx).trees.clone();
        if !pinned_roots.is_empty() {
            let nodes: Vec<_> = pinned_roots.iter().map(|(_, _, r)| *r).collect();
            let empty_tree = HashMap::new();
            let rollup = if self.sidebar.grouping == GroupingMode::Folders {
                &tree_snapshot
            } else {
                &empty_tree
            };
            let (running, waiting) = node_rollup(&nodes, rollup);
            let folded = !filtering && self.sidebar.pinned_collapsed;
            catalog.push(CatalogEntry::Pinned {
                count: nodes.len(),
                folded,
                running,
                waiting,
            });
            if !folded {
                for node in nodes {
                    if self.sidebar.grouping == GroupingMode::Folders {
                        push_tree(&mut catalog, &mut children, &mut ordinal, node);
                    } else {
                        push_thread(&mut catalog, &mut ordinal, node.0, node.1, 0, false);
                    }
                }
            }
        }
        match self.sidebar.grouping {
            GroupingMode::Timeline => {
                // A worker belongs on the day it did the work, even when its
                // parent is older, folded or pinned.
                let pinned: HashSet<_> = pinned_roots.iter().map(|(_, _, node)| node.0).collect();
                let recent: Vec<_> = ordered
                    .into_iter()
                    .filter(|key| !pinned.contains(key))
                    .map(|key| {
                        let entry = &entries[key];
                        let timestamp = entry.order.0;
                        (session_day(timestamp), timestamp, key)
                    })
                    .collect();
                for rows in recent.chunk_by(|a, b| a.0 == b.0) {
                    let day = rows[0].0;
                    let folded = !filtering && self.discovery.folded_days.contains(&day);
                    catalog.push(CatalogEntry::Day {
                        day,
                        count: rows.len(),
                        folded,
                    });
                    if !folded {
                        for &(_, timestamp, key) in rows {
                            push_thread(&mut catalog, &mut ordinal, key, timestamp, 0, false);
                        }
                    }
                }
            }
            GroupingMode::Folders => {
                let mut by_section: HashMap<String, Vec<(&Key, i64, Location)>> = HashMap::new();
                let mut by_project: HashMap<PathBuf, Vec<(&Key, i64, Location)>> = HashMap::new();
                for root in unpinned_roots {
                    let mkey = ThreadMetaKey::new(
                        (root.0).0.clone(),
                        (root.0).1.runtime,
                        (root.0).2.clone(),
                    );
                    if let Some(sec_id) = self
                        .sidebar
                        .thread(&mkey)
                        .and_then(|m| m.section.clone())
                        .filter(|id| self.sidebar.sections.iter().any(|s| &s.id == id))
                    {
                        by_section.entry(sec_id).or_default().push(root);
                    } else {
                        by_project.entry((root.0).0.clone()).or_default().push(root);
                    }
                }
                // Sort threads inside user sections by explicit order when set, else newest first.
                for section in &self.sidebar.sections {
                    let mut list = by_section.remove(&section.id).unwrap_or_default();
                    if filtering && list.is_empty() {
                        continue;
                    }
                    list.sort_by(|a, b| {
                        let ka =
                            ThreadMetaKey::new((a.0).0.clone(), (a.0).1.runtime, (a.0).2.clone());
                        let kb =
                            ThreadMetaKey::new((b.0).0.clone(), (b.0).1.runtime, (b.0).2.clone());
                        let oa = self
                            .sidebar
                            .thread(&ka)
                            .and_then(|m| m.order)
                            .unwrap_or(i64::MAX);
                        let ob = self
                            .sidebar
                            .thread(&kb)
                            .and_then(|m| m.order)
                            .unwrap_or(i64::MAX);
                        oa.cmp(&ob).then_with(|| b.1.cmp(&a.1))
                    });
                    let (running, waiting) = node_rollup(&list, &tree_snapshot);
                    let folded = !filtering && section.collapsed;
                    catalog.push(CatalogEntry::Section {
                        id: section.id.clone(),
                        name: section.name.clone(),
                        color: section.color,
                        count: list.len(),
                        folded,
                        running,
                        waiting,
                    });
                    if !folded {
                        for node in list {
                            push_tree(&mut catalog, &mut children, &mut ordinal, node);
                        }
                    }
                }
                // Ensure the active project always has a folder header when not filtering,
                // so a fresh project shows its folder even with zero threads.
                let active_project = self.panes[self.active].read(cx).project.clone();
                if !filtering {
                    by_project.entry(active_project.clone()).or_default();
                }
                let mut project_groups: Vec<_> = by_project.into_iter().collect();
                project_groups.sort_by(|(pa, la), (pb, lb)| {
                    let star_a = self.sidebar.is_project_starred(pa);
                    let star_b = self.sidebar.is_project_starred(pb);
                    let order_a = self.sidebar.project_order(pa).unwrap_or(i64::MAX);
                    let order_b = self.sidebar.project_order(pb).unwrap_or(i64::MAX);
                    let star_ts_a = self.sidebar.project_starred_at(pa).unwrap_or(0);
                    let star_ts_b = self.sidebar.project_starred_at(pb).unwrap_or(0);
                    let active_a = *pa == active_project;
                    let active_b = *pb == active_project;
                    let newest_a = la.first().map_or(0, |r| r.1);
                    let newest_b = lb.first().map_or(0, |r| r.1);
                    star_b
                        .cmp(&star_a)
                        .then_with(|| order_a.cmp(&order_b))
                        .then_with(|| star_ts_b.cmp(&star_ts_a))
                        .then_with(|| active_b.cmp(&active_a))
                        .then_with(|| newest_b.cmp(&newest_a))
                        .then_with(|| pa.cmp(pb))
                });
                for (project, mut list) in project_groups {
                    list.sort_by(|a, b| {
                        let ka =
                            ThreadMetaKey::new((a.0).0.clone(), (a.0).1.runtime, (a.0).2.clone());
                        let kb =
                            ThreadMetaKey::new((b.0).0.clone(), (b.0).1.runtime, (b.0).2.clone());
                        let oa = self
                            .sidebar
                            .thread(&ka)
                            .and_then(|m| m.order)
                            .unwrap_or(i64::MAX);
                        let ob = self
                            .sidebar
                            .thread(&kb)
                            .and_then(|m| m.order)
                            .unwrap_or(i64::MAX);
                        oa.cmp(&ob).then_with(|| b.1.cmp(&a.1))
                    });
                    let (running, waiting) = node_rollup(&list, &tree_snapshot);
                    let folded = !filtering && self.sidebar.is_project_collapsed(&project);
                    let color = self.sidebar.project_color(&project);
                    let starred = self.sidebar.is_project_starred(&project);
                    catalog.push(CatalogEntry::Project {
                        project,
                        starred,
                        color,
                        count: list.len(),
                        folded,
                        running,
                        waiting,
                    });
                    if !folded {
                        for node in list {
                            push_tree(&mut catalog, &mut children, &mut ordinal, node);
                        }
                    }
                }
            }
        }
        if !just_finished.is_empty() {
            const VISIBLE_RESULTS: usize = 4;
            catalog.push(CatalogEntry::JustFinished);
            for (key, completed_at) in just_finished.iter().take(VISIBLE_RESULTS) {
                push_thread(&mut catalog, &mut ordinal, key, *completed_at, 0, true);
            }
            if just_finished.len() > VISIBLE_RESULTS {
                catalog.push(CatalogEntry::ViewFinished);
            }
        } else if running_only && catalog.is_empty() {
            catalog.push(CatalogEntry::NoRunning);
        }
        self.catalog = catalog;
    }
}

#[cfg(test)]
mod unfold_tests {
    use super::unfold_ancestors;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    fn key(id: &str) -> (PathBuf, String) {
        (PathBuf::from("/work/app"), id.to_owned())
    }

    #[test]
    fn unfolds_every_collapsed_ancestor_of_a_folded_worker() {
        // grandchild G sits under child C, which sits under parent P.
        let parents = HashMap::from([(key("G"), key("C")), (key("C"), key("P"))]);
        let mut collapsed = HashSet::from([key("C"), key("P")]);
        assert!(unfold_ancestors(&parents, &mut collapsed, key("G")));
        assert!(collapsed.is_empty());
    }

    #[test]
    fn reports_nothing_to_do_without_folded_ancestors() {
        let parents = HashMap::from([(key("G"), key("C"))]);
        let mut collapsed = HashSet::from([key("X")]);
        assert!(!unfold_ancestors(&parents, &mut collapsed, key("G")));
        assert_eq!(collapsed, HashSet::from([key("X")]));
    }

    #[test]
    fn a_parent_cycle_terminates() {
        let parents = HashMap::from([(key("A"), key("B")), (key("B"), key("A"))]);
        let mut collapsed = HashSet::from([key("B")]);
        assert!(unfold_ancestors(&parents, &mut collapsed, key("A")));
        assert!(collapsed.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::{Discovery, Route, Runtime, Scope, ThreadRow};
    use std::path::Path;
    #[test]
    fn native_and_relay_homes_only_open_their_original_provider() {
        let row = |provider, model| {
            serde_json::from_value::<ThreadRow>(serde_json::json!({
                "id":"same","cwd":"/other","modelProvider":provider,"model":model
            }))
            .unwrap()
        };
        let saved = ["vendor/demo".to_owned()];
        let native = Scope {
            runtime: Runtime::Codex,
            api: false,
        };
        let relay = Scope {
            runtime: Runtime::Codex,
            api: true,
        };
        let resolve = |scope: Scope, t: &ThreadRow| scope.saved_route(t, &saved, Some("codeox"));
        assert_eq!(
            resolve(native, &row("openai", None)),
            Some(Route::CODEX_SUBSCRIPTION)
        );
        assert_eq!(
            resolve(relay, &row("codeox", Some("vendor/demo"))),
            Some(Route::codex_api(saved[0].clone()))
        );
        assert_eq!(
            resolve(native, &row("external-vendor", Some("vendor/demo"))),
            None
        );
        assert_eq!(resolve(native, &row("codeox", Some("vendor/demo"))), None);
        assert_eq!(resolve(relay, &row("openai", None)), None);
        assert_eq!(resolve(relay, &row("codeox", None)), None);
    }

    #[test]
    fn a_cold_parents_last_worker_update_survives_a_catalog_snapshot() {
        use crate::platform::CatalogEvent;
        use ox_desk_core::host_protocol::{Scope as Origin, SessionView};
        let project = Path::new("/project");
        let scope = Origin {
            project: project.into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let view = SessionView {
            id: "parent".into(),
            title: "Build".into(),
            parent: None,
            updated_at: 1,
            status: "idle".into(),
            capabilities: vec![],
            detail: serde_json::json!({"owned":true, "worker_notice":{"job_id": 42, "status": "interrupted"}}),
        };
        let mut library = Discovery::default();
        assert!(library.live_event(CatalogEvent::Replace(scope.clone(), vec![view.clone()])));
        assert_eq!(
            library.worker_notice(project, &scope.route, "parent"),
            Some("Worker #42 interrupted")
        );
        assert!(!library.live_event(CatalogEvent::Update(scope.clone(), view)));
        assert!(library.live_event(CatalogEvent::Replace(scope.clone(), vec![])));
        assert_eq!(library.worker_notice(project, &scope.route, "parent"), None);
    }

    #[test]
    fn global_sessions_arrive_without_discovery_and_keep_their_exact_route() {
        use crate::platform::CatalogEvent;
        use ox_desk_core::host_protocol::{Scope as Origin, SessionView};
        let project = Path::new("/other");
        let route = Route::codex_api("vendor/demo".into());
        let origin = Origin {
            project: project.into(),
            route: route.clone(),
        };
        let detail = serde_json::json!({"owned":true,"row":{"cwd":project,"id":"phone","modelProvider":"codeox","preview":"From my phone"}});
        let mut session = SessionView {
            id: "phone".into(),
            title: "From my phone".into(),
            parent: None,
            status: "running".into(),
            updated_at: 1,
            capabilities: vec![],
            detail,
        };
        let mut library = Discovery::default();
        assert!(library.live_event(CatalogEvent::Update(origin.clone(), session.clone())));
        let saved = &library.hosted[0].saved;
        assert_eq!(
            library.route_status(project, saved.scope, &saved.row),
            Some((route.clone(), Some("Running")))
        );
        assert_eq!(library.waiting_count(project, &route), 0);
        assert!(!library.live_event(CatalogEvent::Update(origin.clone(), session.clone())));
        session.status = "needs_input".into();
        assert!(library.live_event(CatalogEvent::Update(origin.clone(), session.clone())));
        let saved = &library.hosted[0].saved;
        assert_eq!(
            library.route_status(project, saved.scope, &saved.row),
            Some((route.clone(), Some("Needs you")))
        );
        assert_eq!(library.waiting_count(project, &route), 1);
        library.replace(Runtime::Codex, Some(true), vec![]);
        assert_eq!(library.all().count(), 1);
        assert!(library.live_event(CatalogEvent::Replace(origin, vec![])));
        assert_eq!(library.all().count(), 0);
        assert_eq!(library.waiting_count(project, &route), 0);
    }

    #[test]
    fn refreshing_one_home_removes_archived_rows_without_touching_the_other_home() {
        let mut library = Discovery::default();
        let project = Path::new("/work");
        let native = Scope {
            runtime: Runtime::Codex,
            api: false,
        };
        let relay = Scope {
            runtime: Runtime::Codex,
            api: true,
        };
        let row = || {
            serde_json::from_value::<ThreadRow>(
                serde_json::json!({"id":"duplicate","cwd":project,"modelProvider":"openai"}),
            )
            .unwrap()
        };
        library.replace(Runtime::Codex, Some(false), vec![row()]);
        library.replace(Runtime::Codex, Some(true), vec![row()]);
        library.replace(Runtime::Codex, Some(false), vec![]);
        assert_eq!(library.rows.len(), 1);
        assert!(library.removed(project, native, "duplicate"));
        assert!(!library.removed(project, relay, "duplicate"));
        library.replace(Runtime::Codex, Some(false), vec![row()]);
        assert!(!library.removed(project, native, "duplicate"));
        assert_eq!(library.rows.len(), 2);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum StatusFilter {
    Running,
    Waiting,
    Finished,
    Pinned,
}

/// Split a leading or trailing `is:` token from the catalog search text.
pub(super) fn split_status_filter(query: &str) -> (Option<StatusFilter>, String) {
    let mut filter = None;
    let rest: Vec<&str> = query
        .split_whitespace()
        .filter(|word| {
            let found = match *word {
                "is:running" => Some(StatusFilter::Running),
                "is:waiting" | "is:approval" => Some(StatusFilter::Waiting),
                "is:finished" | "is:done" => Some(StatusFilter::Finished),
                "is:pinned" => Some(StatusFilter::Pinned),
                _ => None,
            };
            filter = filter.or(found);
            found.is_none()
        })
        .collect();
    (filter, rest.join(" "))
}

#[cfg(test)]
mod status_filter_tests {
    use super::{StatusFilter, split_status_filter};
    #[test]
    fn status_tokens_leave_the_text_query() {
        assert_eq!(
            split_status_filter("is:running fix"),
            (Some(StatusFilter::Running), "fix".into())
        );
        assert_eq!(
            split_status_filter("tests is:waiting"),
            (Some(StatusFilter::Waiting), "tests".into())
        );
        assert_eq!(
            split_status_filter("is:finished"),
            (Some(StatusFilter::Finished), String::new())
        );
        assert_eq!(split_status_filter("plain"), (None, "plain".into()));
    }
}

#[cfg(test)]
mod parent_link_tests {
    use super::{ParentLink, Scope, resolve_parent_key};
    use ox_desk_core::origin::{Origin, Relation};
    use ox_desk_core::runtime::{Route, Runtime};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn origin(
        relation: Relation,
        project: &str,
        scope: Option<ox_desk_core::host_protocol::Scope>,
        session: Option<&str>,
    ) -> Origin {
        serde_json::from_value(serde_json::json!({
            "relation": relation,
            "parent": {"project": project, "scope": scope, "session": session, "runtime": "codex"},
            "launch": "helper:fixture",
            "by_operator": false,
            "runtime": "codex",
            "route": {"runtime": "codex", "billing": {"api": "fixture/demo"}},
            "mode": "session",
            "job_id": 4,
            "requested": "read",
            "ceiling": "read",
            "source": null
        }))
        .unwrap()
    }

    fn hosted_parent() -> ox_desk_core::host_protocol::Scope {
        ox_desk_core::host_protocol::Scope {
            project: PathBuf::from("/work/parent"),
            route: Route::CODEX_SUBSCRIPTION,
        }
    }

    #[test]
    fn worker_lineage_survives_richer_native_rows_in_either_catalog_order() {
        use super::{Indexed, Location};
        for host_first in [true, false] {
            let candidate = |host: bool| Indexed {
                order: (if host { 2 } else { 1 }, 0),
                loc: if host {
                    Location::Hosted(0)
                } else {
                    Location::Saved(0)
                },
                parent: host.then(|| "p".to_owned()),
                quality: (true, !host, false, false, false, false, 0),
                origin: host.then(|| {
                    origin(
                        Relation::Worker,
                        "/work/parent",
                        Some(hosted_parent()),
                        Some("p"),
                    )
                }),
                result: None,
            };
            let mut row = candidate(host_first);
            row.merge(candidate(!host_first));
            assert!(matches!(row.loc, Location::Saved(0)));
            assert_eq!(row.order.0, 2);
            assert_eq!(row.parent.as_deref(), Some("p"));
            let parent = ParentLink::from_origin(row.origin.as_ref().unwrap()).unwrap();
            assert_eq!(
                (parent.id.as_str(), parent.route),
                ("p", Some(Route::CODEX_SUBSCRIPTION))
            );
        }
    }

    #[test]
    fn a_worker_takes_its_project_and_route_from_the_recorded_parent_scope() {
        let link = ParentLink::from_origin(&origin(
            Relation::Worker,
            "/elsewhere",
            Some(hosted_parent()),
            Some("p"),
        ))
        .unwrap();
        assert_eq!(link.project, PathBuf::from("/work/parent"));
        assert_eq!(link.route, Some(Route::CODEX_SUBSCRIPTION));
        assert_eq!(link.id, "p");
    }

    #[test]
    fn a_recorded_parent_resolves_across_projects_before_the_same_project_fallback() {
        let parent = ParentLink::from_origin(&origin(
            Relation::Worker,
            "/work/parent",
            Some(hosted_parent()),
            Some("p"),
        ))
        .unwrap();
        let by_scope = HashMap::from([(
            (
                PathBuf::from("/work/parent"),
                Scope {
                    runtime: Runtime::Codex,
                    api: false,
                },
                "p".to_owned(),
            ),
            "parent-in-other-checkout",
        )]);
        let by_project = HashMap::new();
        assert_eq!(
            resolve_parent_key(&parent, &by_scope, &by_project),
            Some("parent-in-other-checkout")
        );
    }

    #[test]
    fn a_parent_whose_route_is_filtered_away_still_resolves_in_its_project() {
        let parent = ParentLink::from_origin(&origin(
            Relation::Worker,
            "/work/parent",
            Some(hosted_parent()),
            Some("p"),
        ))
        .unwrap();
        let by_scope = HashMap::new();
        let by_project = HashMap::from([(
            (PathBuf::from("/work/parent"), "p".to_owned()),
            "parent-in-this-project",
        )]);
        assert_eq!(
            resolve_parent_key(&parent, &by_scope, &by_project),
            Some("parent-in-this-project")
        );
    }

    #[test]
    fn a_terminal_parent_keeps_its_project_and_handoffs_are_not_worker_parents() {
        let terminal = ParentLink::from_origin(&origin(
            Relation::Worker,
            "/work/parent",
            None,
            Some("native-7"),
        ))
        .unwrap();
        assert_eq!(terminal.project, PathBuf::from("/work/parent"));
        assert_eq!(terminal.route, None);
        assert_eq!(terminal.id, "native-7");
        assert!(
            ParentLink::from_origin(&origin(
                Relation::Handoff,
                "/source",
                Some(hosted_parent()),
                Some("p")
            ))
            .is_none()
        );
    }

    #[test]
    fn a_linked_workers_lead_is_never_resolved_in_the_receiving_hosts_catalog() {
        let mut worker = origin(
            Relation::Worker,
            "/work/parent",
            Some(hosted_parent()),
            Some("p"),
        );
        worker.linked = Some(Box::new(ox_desk_core::origin::LinkedParent {
            host: "source-host".into(),
            device: "source-registration".into(),
            request_id: "worker-request".into(),
            parent: worker.parent.clone(),
            verified_level: ox_desk_core::modes::Level::Read,
            link_ceiling: ox_desk_core::modes::Level::Read,
        }));
        assert!(ParentLink::from_origin(&worker).is_none());
    }
}
