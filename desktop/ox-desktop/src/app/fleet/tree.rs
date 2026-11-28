//! Navigation rows that surround the current host's native thread list.
use super::{Destination, Inventory};
use crate::{
    app::Workspace,
    appearance::{Palette, Step},
    kit,
    sidebar_state::{SidebarState, ThreadMetaKey},
};
use gpui::{prelude::FluentBuilder, *};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(crate) struct Expansion {
    hosts: HashMap<EntityId, bool>,
    projects: HashMap<(EntityId, PathBuf), bool>,
}
impl Expansion {
    pub(super) fn retain(&mut self, live: &HashSet<EntityId>) {
        self.hosts.retain(|host, _| live.contains(host));
        self.projects.retain(|(host, _), _| live.contains(host));
    }
    pub(crate) fn activate(&mut self, id: EntityId) {
        self.hosts.remove(&id);
    }
}

#[derive(Clone)]
enum Action {
    Host(bool),
    Project(PathBuf, bool),
    Session(Destination),
    Message,
}
pub(crate) struct TreeRow {
    host: Option<String>,
    registration: EntityId,
    title: String,
    detail: String,
    tooltip: String,
    action: Action,
    status: Option<u32>,
    policy: Option<&'static str>,
    muted: bool,
    enabled: bool,
    new_project: Option<crate::app::projects::TreeStart>,
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

impl Inventory {
    fn summary(&self, p: Palette) -> (String, u32) {
        if !self.online {
            return (
                self.status(),
                match self.error {
                    Some(io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied) => p.warning,
                    None => p.accent,
                    _ => p.muted,
                },
            );
        }
        let summary = self
            .info
            .as_ref()
            .and_then(|i| i.resources.as_ref())
            .map_or_else(
                || "Connected".to_owned(),
                |r| {
                    format!(
                        "{} · load {:.1} · {:.1} GiB free",
                        r.os,
                        r.load_one_minute,
                        r.available_memory as f64 / 1_073_741_824.
                    )
                },
            );
        (summary, p.success)
    }
}

pub(crate) struct Current<'a> {
    pub sidebar: &'a SidebarState,
    pub native_projects: HashSet<&'a Path>,
    pub project: &'a Path,
    pub route: &'a ox_desk_core::store::Route,
    pub thread: Option<&'a str>,
}
struct ProjectRows<'a> {
    expansion: &'a Expansion,
    native: bool,
    current: Option<&'a Current<'a>>,
    sidebar: &'a SidebarState,
    remembered: Option<&'a crate::views::host::Host>,
    filter: &'a str,
    repositories: &'a HashMap<&'a str, BTreeSet<String>>,
}
impl Workspace {
    pub(crate) fn tree_rows(
        &self,
        active: &Option<String>,
        filter: &str,
        active_sidebar: Option<Current<'_>>,
        cx: &App,
    ) -> (Vec<TreeRow>, bool) {
        if !self.inventory.keys().any(Option::is_some) {
            return (Vec::new(), true);
        }
        let p = Palette::get(cx);
        let mut entries: Vec<_> = self.inventory.iter().collect();
        // Peer headers stay reachable above a long active thread history.
        entries.sort_by_cached_key(|(id, e)| {
            (
                *id == active,
                id.is_some(),
                e.read(cx).display_name().to_lowercase(),
                (*id).clone(),
            )
        });
        let mut repositories: HashMap<&str, BTreeSet<String>> = HashMap::new();
        for (host, entry) in &entries {
            let name = self.host_name(host, cx);
            if let Some(dir) = &entry.read(cx).directory {
                for checkout in &dir.checkouts {
                    if let Some(repo) = checkout.repository.as_deref() {
                        repositories.entry(repo).or_default().insert(name.clone());
                    }
                }
            }
        }
        let mut rows = Vec::new();
        let mut native = false;
        for (id, entry) in entries {
            let reg = entry.entity_id();
            let inv = entry.read(cx);
            let current = active == id;
            let open = self.tree.hosts.get(&reg).copied().unwrap_or(current);
            let (detail, color) = inv.summary(p);
            let name = self.host_name(id, cx);
            let policy = inv
                .info
                .as_ref()
                .and_then(|i| i.mode_policy)
                .filter(|p| *p != ox_desk_core::host_policy::HostPolicy::FollowInterface);
            let title = name.clone();
            rows.push(TreeRow {
                host: id.clone(),
                registration: reg,
                title,
                detail,
                tooltip: format!("{name} · {}", inv.status()),
                action: Action::Host(open),
                status: Some(color),
                policy: policy.map(|p| p.label()),
                muted: !inv.online,
                enabled: true,
                new_project: None,
            });
            if !open {
                continue;
            }
            let active_native = current && (inv.online || inv.error.is_none());
            native |= active_native;
            if active_native
                && active_sidebar.as_ref().is_none_or(|c| {
                    c.sidebar.grouping != crate::sidebar_state::GroupingMode::Folders
                })
            {
                continue;
            }
            // The rendering Host supplies its sidebar directly to avoid borrowing it twice.
            let other = (!current)
                .then(|| match id {
                    Some(id) => self.hosts.get(id),
                    None => self.local.as_ref(),
                })
                .flatten();
            let sidebar = other.map_or(&inv.saved_sidebar, |h| h.read(cx).sidebar_state());
            let focused = active_sidebar.as_ref().filter(|_| current);
            rows.extend(peer_projects(
                entry,
                ProjectRows {
                    expansion: &self.tree,
                    native: active_native,
                    current: focused,
                    sidebar: focused.map_or(sidebar, |c| c.sidebar),
                    remembered: other.map(|h| h.read(cx)),
                    filter,
                    repositories: &repositories,
                },
                cx,
            ));
        }
        (rows, native)
    }
}

fn peer_projects(entry: &Entity<Inventory>, options: ProjectRows<'_>, cx: &App) -> Vec<TreeRow> {
    let ProjectRows {
        expansion,
        native,
        current,
        sidebar,
        remembered,
        filter,
        repositories,
    } = options;
    let mut output = Vec::new();
    let inv = entry.read(cx);
    let id = inv.link.remote().map(|h| h.id.clone());
    let reg = entry.entity_id();
    let mut projects = BTreeMap::<PathBuf, Vec<_>>::new();
    let mut registered = HashMap::new();
    if let Some(dir) = &inv.directory {
        for checkout in &dir.checkouts {
            if native
                && current.is_some_and(|c| {
                    c.project == checkout.path
                        || c.native_projects.contains(checkout.path.as_path())
                })
            {
                continue;
            }
            registered.insert(&checkout.path, checkout.repository.as_deref());
            projects.entry(checkout.path.clone()).or_default();
        }
    }
    let sessions = if native {
        // The active workbench already renders its own sessions, pins and sections.
        // Only add registered folders with no native list entry.
        let occupied: HashSet<_> = inv
            .projects
            .iter()
            .filter(|(_, p)| !p.titles.is_empty())
            .map(|(scope, _)| &scope.project)
            .chain(inv.saved.keys().map(|scope| &scope.project))
            .collect();
        projects.retain(|path, _| !occupied.contains(path));
        Vec::new()
    } else {
        inv.rows(false)
    };
    for (scope, thread, mut title) in sessions {
        let Some(thread) = thread else {
            continue;
        };
        let meta = sidebar.thread(&ThreadMetaKey::new(
            scope.project.clone(),
            scope.route.runtime,
            thread.clone(),
        ));
        if meta.is_some_and(|m| m.hidden) {
            continue;
        }
        if let Some(custom) = meta.and_then(|m| m.custom_title.as_ref()) {
            title.clone_from(custom);
        }
        let updated = inv.native_row(&scope, &thread).map_or(0, |r| r.updated_at);
        let route = scope.route.label();
        let target = Destination {
            host: id.clone(),
            registration: reg,
            scope: Some(scope.clone()),
            thread: Some(thread),
        };
        projects.entry(scope.project).or_default().push((
            std::cmp::Reverse(updated),
            title,
            route,
            target,
        ));
    }
    let query = filter.trim().to_lowercase();
    let message = if projects.is_empty() && !native {
        Some(if inv.scanning {
            "Loading sessions…"
        } else if inv.online {
            "No projects reported"
        } else {
            "No cached projects"
        })
    } else {
        None
    };
    let mut names = HashMap::new();
    for path in projects.keys() {
        *names.entry(project_name(path)).or_insert(0usize) += 1;
    }
    let single = projects.len() == 1;
    for (path, mut sessions) in projects {
        let name = project_name(&path);
        let display = path.display().to_string();
        let project_matches = query.is_empty() || display.to_lowercase().contains(&query);
        sessions.retain(|(_, title, route, _)| {
            project_matches || format!("{title} {route}").to_lowercase().contains(&query)
        });
        if !project_matches && sessions.is_empty() {
            continue;
        }
        let open = expansion
            .projects
            .get(&(reg, path.clone()))
            .copied()
            .unwrap_or_else(|| {
                single || !query.is_empty() || current.is_some_and(|c| c.project == path)
            });
        let shared = registered
            .get(&path)
            .copied()
            .flatten()
            .and_then(|repo| repositories.get(repo))
            .filter(|hosts| hosts.len() > 1);
        let title = shared.map_or_else(
            || name.clone(),
            |hosts| {
                format!(
                    "{name} · {}",
                    hosts
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        );
        let new_project = registered
            .get(&path)
            .filter(|_| inv.online)
            .and_then(|repo| {
                let last = current
                    .and_then(|c| (c.project == path).then(|| c.route.clone()))
                    .or_else(|| remembered.and_then(|h| h.project_route(&path)))
                    .or_else(|| inv.recent.saved_route(&path));
                let route = crate::app::projects::available_route(
                    &inv.directory.as_ref()?.routes,
                    None,
                    last.as_ref(),
                )?;
                Some(crate::app::projects::TreeStart {
                    checkout: crate::workspace_state::Checkout {
                        host: id.clone(),
                        path: path.clone(),
                    },
                    registration: reg,
                    host_name: inv.display_name().into(),
                    repository: repo.map(str::to_owned),
                    route,
                })
            });
        output.push(TreeRow {
            host: id.clone(),
            registration: reg,
            title,
            detail: if names[&name] > 1 {
                display.clone()
            } else {
                format!(
                    "{} {}",
                    sessions.len(),
                    if sessions.len() == 1 {
                        "session"
                    } else {
                        "sessions"
                    }
                )
            },
            tooltip: display,
            action: Action::Project(path, open),
            status: None,
            policy: None,
            muted: !inv.online,
            enabled: true,
            new_project,
        });
        if open {
            sessions.sort_by(|a, b| {
                (&a.0, &a.1, &a.2, &a.3.thread).cmp(&(&b.0, &b.1, &b.2, &b.3.thread))
            });
            for (_, title, route, target) in sessions {
                let selected = current.is_some_and(|c| {
                    target.thread.as_deref() == c.thread
                        && target.scope.as_ref().is_some_and(|scope| {
                            c.project == scope.project && c.route == &scope.route
                        })
                });
                let title = if selected {
                    format!("● {title}")
                } else {
                    title
                };
                let route = if target
                    .scope
                    .as_ref()
                    .zip(target.thread.as_deref())
                    .and_then(|(scope, thread)| inv.native_row(scope, thread))
                    .is_some_and(|row| row.worker)
                {
                    format!("Worker on {} · {route}", inv.display_name())
                } else {
                    route
                };

                output.push(TreeRow {
                    host: id.clone(),
                    registration: reg,
                    tooltip: format!("{title} · {route}"),
                    title,
                    detail: route,
                    action: Action::Session(target),
                    status: None,
                    policy: None,
                    muted: !inv.online,
                    enabled: inv.online,
                    new_project: None,
                });
            }
        }
    }
    if let Some(message) = message {
        output.push(TreeRow {
            host: id.clone(),
            registration: reg,
            title: message.into(),
            detail: String::new(),
            tooltip: String::new(),
            action: Action::Message,
            status: None,
            policy: None,
            muted: !inv.online,
            enabled: false,
            new_project: None,
        });
    }
    output
}

impl TreeRow {
    pub(crate) fn id(&self) -> String {
        let reg = self.registration;
        match &self.action {
            Action::Host(_) => format!("tree-host-{reg:?}"),
            Action::Project(path, _) => format!("tree-project-{reg:?}-{}", path.display()),
            Action::Session(t) => format!("tree-thread-{reg:?}-{:?}-{:?}", t.scope, t.thread),
            Action::Message => format!("tree-empty-{reg:?}"),
        }
    }
    pub(crate) fn render(
        &self,
        owner: &WeakEntity<Workspace>,
        width: Pixels,
        p: Palette,
        nav: &Keyboard,
        index: usize,
        current: Option<&str>,
    ) -> AnyElement {
        let reg = self.registration;
        let host = self.host.clone();
        let ws = owner.clone();
        let (indent, arrow) = match &self.action {
            Action::Host(open) => (0., Some(*open)),
            Action::Project(_, open) => (8., Some(*open)),
            _ => (19., None),
        };
        let id = self.id();
        let action = self.action.clone();
        let mut line = kit::row().w_full().min_w_0().gap_1().pl(px(indent));
        if let Some(open) = arrow {
            line = line.child(div().text_size(p.text(Step::Label)).child(if open {
                "▾"
            } else {
                "▸"
            }));
        }
        if let Some(color) = self.status {
            line = line.child(
                div()
                    .size(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(color)),
            );
        }
        line = line.child(
            kit::col()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .truncate()
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(if self.muted { p.muted } else { p.fg }))
                        .child(self.title.clone()),
                )
                .when(!self.detail.is_empty(), |col| {
                    col.child(
                        div()
                            .truncate()
                            .text_size(p.text(Step::Label))
                            .text_color(rgb(p.muted))
                            .child(self.detail.clone()),
                    )
                }),
        );
        if let Some(policy) = self.policy {
            line = line.child(
                div()
                    .flex_shrink_0()
                    .px_1()
                    .rounded_sm()
                    .bg(rgb(p.raised))
                    .text_color(rgb(p.fg))
                    .text_size(p.text(Step::Label))
                    .child(policy),
            );
        }
        let name = match &self.action {
            Action::Host(open) | Action::Project(_, open) => format!(
                "{} · {} · {}",
                self.title,
                if *open { "expanded" } else { "collapsed" },
                self.tooltip
            ),
            Action::Session(_) if !self.enabled => format!("{} · Offline", self.tooltip),
            _ => self.tooltip.clone(),
        };
        let focused = current == Some(id.as_str());

        let keys = nav.clone();
        let clicked = owner.clone();
        let disclosure = arrow.is_some();
        let button = kit::nav_row(
            id.clone(),
            name.clone(),
            kit::ROW_HEIGHT,
            self.enabled,
            move |_, w, cx| {
                let _ = ws.update(cx, |s, cx| {
                    let Some(entry) = s
                        .inventory
                        .get(&host)
                        .filter(|e| e.entity_id() == reg)
                        .cloned()
                    else {
                        return;
                    };
                    match &action {
                        Action::Host(open) => {
                            s.tree.hosts.insert(reg, !open);
                            if !open {
                                entry.update(cx, |inv, cx| inv.load_saved(cx));
                            }
                        }
                        Action::Project(path, open) => {
                            s.tree.projects.insert((reg, path.clone()), !*open);
                        }
                        Action::Session(target) => {
                            s.open_destination(target, w, cx);
                            return;
                        }
                        Action::Message => return,
                    }
                    s.fleet_changed(cx);
                });
                if disclosure {
                    keys.select(index, &clicked, w, cx);
                }
            },
        )
        .tooltip(name)
        .when(focused && self.enabled, |button| {
            button.track_focus(&nav.focus)
        })
        .child(line);
        let mut row = kit::row()
            .w(width)
            .h(px(kit::ROW_HEIGHT))
            .px(px(kit::GUTTER))
            .child(div().flex_1().min_w_0().child(button));
        if let Some(start) = self.new_project.clone() {
            let ws = owner.clone();
            row = row.child(kit::icon_button(
                p,
                format!("{id}-new"),
                "+",
                format!(
                    "New session on {} · {} · {}",
                    start.host_name,
                    start.checkout.path.display(),
                    start.route.label()
                ),
                move |_, w, cx| {
                    let _ = ws.update(cx, |s, cx| s.start_tree_project(&start, w, cx));
                },
            ));
        }
        let keys = nav.clone();
        let owner = owner.clone();
        row.when(focused && !self.enabled, |row| {
            row.track_focus(&nav.focus).bg(rgb(p.hover))
        })
        .on_key_down(move |event: &KeyDownEvent, w, cx| {
            if event.keystroke.modifiers != Modifiers::default() {
                return;
            }
            let Some(next) = keys.next(index, &event.keystroke.key) else {
                if matches!(event.keystroke.key.as_str(), "down" | "j")
                    && index + 1 == keys.ids.len()
                    && let Some(focus) = &keys.native_focus
                {
                    keys.scroll
                        .scroll_to_item(keys.ids.len(), ScrollStrategy::Nearest);
                    focus.focus(w, cx);
                }
                if matches!(
                    event.keystroke.key.as_str(),
                    "up" | "down" | "j" | "k" | "end" | "p" | "u" | "c" | "r" | "f2" | "/"
                ) {
                    cx.stop_propagation();
                }
                return;
            };
            keys.select(next, &owner, w, cx);
            w.prevent_default();
            cx.stop_propagation();
        })
        .into_any_element()
    }
}

#[derive(Clone)]
pub(crate) struct Keyboard {
    ids: std::rc::Rc<Vec<String>>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    native_focus: Option<FocusHandle>,
}
impl Keyboard {
    pub(crate) fn new(
        rows: &[TreeRow],
        scroll: UniformListScrollHandle,
        focus: FocusHandle,
        native_focus: Option<FocusHandle>,
    ) -> Self {
        Self {
            ids: std::rc::Rc::new(rows.iter().map(TreeRow::id).collect()),
            scroll,
            focus,
            native_focus,
        }
    }
    fn next(&self, index: usize, key: &str) -> Option<usize> {
        match key {
            "home" => Some(0),
            "end" if self.native_focus.is_none() => self.ids.len().checked_sub(1),
            "up" | "k" => index.checked_sub(1),
            "down" | "j" if index + 1 < self.ids.len() => Some(index + 1),
            _ => None,
        }
    }
    pub(crate) fn recover(
        &self,
        cursor: Option<&str>,
        owner: WeakEntity<Workspace>,
        fallback: Option<FocusHandle>,
        w: &mut Window,
    ) {
        let Some(cursor) = cursor
            .filter(|id| self.focus.is_focused(w) && !self.ids.iter().any(|key| key == *id))
            .map(str::to_owned)
        else {
            return;
        };
        let keys = self.clone();
        w.on_next_frame(move |w, cx| {
            if !owner
                .upgrade()
                .is_some_and(|ws| ws.read(cx).tree_cursor.as_ref() == Some(&cursor))
            {
                return;
            }
            if keys.ids.is_empty() {
                let _ = owner.update(cx, |s, _| s.tree_cursor = None);
                if let Some(focus) = fallback {
                    focus.focus(w, cx);
                }
            } else {
                keys.select(0, &owner, w, cx);
            }
        });
    }

    pub(crate) fn select(
        &self,
        index: usize,
        ws: &WeakEntity<Workspace>,
        w: &mut Window,
        cx: &mut App,
    ) {
        let Some(id) = self.ids.get(index) else {
            return;
        };
        let _ = ws.update(cx, |s, cx| {
            s.tree_cursor = Some(id.clone());
            if let Some(view) = s.visible_host() {
                gpui::App::notify(cx, view.entity_id());
            }
            cx.notify();
        });
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        let focus = self.focus.clone();
        w.on_next_frame(move |w, cx| focus.focus(w, cx));
    }
}

pub(crate) struct Tree {
    pub keyboard: Option<Keyboard>,
    pub palette: Option<Palette>,
    pub revision: u64,
    pub filter: String,
    pub rows: Vec<TreeRow>,
    pub native: bool,
}
impl Default for Tree {
    fn default() -> Self {
        Self {
            keyboard: None,
            palette: None,
            revision: u64::MAX,
            filter: String::new(),
            rows: Vec::new(),
            native: true,
        }
    }
}
