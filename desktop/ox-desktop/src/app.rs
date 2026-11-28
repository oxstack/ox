//! Release the session tree if a desktop view fails while building a frame.
pub(crate) mod fleet;
pub(crate) mod notifications;
pub(crate) mod projects;
use crate::{appearance::Palette, kit, views::host::Host};
use gpui::*;
use std::{
    cell::Cell,
    panic::{self, AssertUnwindSafe},
};

struct FailedView;
impl Global for FailedView {}

thread_local! {
    // View payloads can include model output, local paths, or text from an editor.
    static IN_VIEW: Cell<bool> = const { Cell::new(false) };
}

pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if IN_VIEW.get() {
            if let Some(loc) = info.location() {
                eprintln!("OX Desktop view failed at {}:{}.", loc.file(), loc.line());
            } else {
                eprintln!("OX Desktop view failed.");
            }
        } else {
            previous(info);
        }
    }));
}

pub fn guard<T: 'static>(
    cx: &mut Context<T>,
    draw: impl FnOnce(&mut Context<T>) -> AnyElement,
) -> AnyElement {
    try_draw(cx, draw).unwrap_or_else(|| div().into_any_element())
}

pub fn try_draw<T: 'static, R>(
    cx: &mut Context<T>,
    draw: impl FnOnce(&mut Context<T>) -> R,
) -> Option<R> {
    if cx.has_global::<FailedView>() {
        return None;
    }
    let nested = IN_VIEW.replace(true);
    let result = panic::catch_unwind(AssertUnwindSafe(|| draw(cx)));
    IN_VIEW.set(nested);
    match result {
        Ok(output) => Some(output),
        Err(_) => {
            cx.set_global(FailedView);
            None
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClientOnly(pub bool);
impl Global for ClientOnly {}

pub struct Workspace {
    local: Option<Entity<Host>>,
    hosts: std::collections::HashMap<String, Entity<Host>>,
    active: Option<String>,
    settings: Option<Entity<crate::preferences::Hosts>>,
    error: Option<String>,
    inventory: std::collections::HashMap<Option<String>, Entity<fleet::Inventory>>,
    page: Option<fleet::Page>,
    chooser: Option<projects::Chooser>,
    project_request: u64,
    tree: fleet::tree::Expansion,
    pub(crate) tree_cursor: Option<String>,
    pub(crate) tree_focus: FocusHandle,
    tree_scroll: UniformListScrollHandle,
    navigation_revision: u64,
    scoped_search: bool,
    searching: bool,
    search_return: Option<FocusHandle>,
    filter: Entity<gpui_component::input::InputState>,
    _failure: Subscription,
    _updates: Subscription,
    _directory: Subscription,
}
impl Workspace {
    pub fn new(
        local: Option<Entity<Host>>,
        restore_host: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if !cx.has_global::<projects::Recent>() {
            cx.set_global(projects::Recent(
                crate::workspace_state::read_preference("project-hosts.json").unwrap_or_default(),
            ));
        }
        notifications::listen(w, cx);
        let failure = cx.observe_global::<FailedView>(|s, cx| {
            s.local.take();
            s.hosts.clear();
            s.inventory.clear();
            cx.notify();
        });
        let settings = (local.is_none()
            && cx
                .global::<crate::preferences::HostDirectory>()
                .0
                .is_empty())
        .then(|| cx.new(|cx| crate::preferences::Hosts::new(w, cx)));
        let filter = kit::field(
            "",
            "Filter by title, project, @host or is:waiting",
            false,
            w,
            cx,
        );
        cx.subscribe_in(
            &filter,
            w,
            |s, _, event: &gpui_component::input::InputEvent, w, cx| {
                if matches!(event, gpui_component::input::InputEvent::PressEnter { .. }) {
                    let page = s
                        .page
                        .filter(|_| !s.searching)
                        .unwrap_or(fleet::Page::Sessions);
                    if let Some(row) = s.page_rows(page, cx).into_iter().find(|r| r.online) {
                        s.open_destination(&row.target, w, cx);
                    }
                }
                cx.notify();
            },
        )
        .detach();
        let owner = cx.weak_entity();
        let window_id = w.window_handle().window_id();
        let bindings = crate::keymap::bindings(cx);
        cx.intercept_keystrokes(move |event, w, cx| {
            if w.window_handle().window_id() != window_id || w.default_prevented() {
                return;
            }
            let key = &event.keystroke;
            if owner
                .read_with(cx, |s, _| s.chooser.is_some())
                .unwrap_or(false)
            {
                if key.modifiers == Modifiers::default() || key.modifiers == Modifiers::shift() {
                    let _ = owner.update(cx, |s, cx| {
                        s.chooser_key(&key.key, key.modifiers.shift, w, cx)
                    });
                }
                return;
            }
            let toggle = bindings.iter().any(|(binding, cmd)| {
                *cmd == ox_desk_core::keys::Command::Palette
                    && binding.key == key.key
                    && binding.modifiers == key.modifiers
            });
            let escape = key.modifiers == Modifiers::default() && key.key == "escape";
            if !toggle && !escape {
                return;
            }
            let _ = owner.update(cx, |s, cx| {
                if s.visible_host().is_some() || (!toggle && !s.searching) {
                    return;
                }
                if s.searching {
                    if s.filter
                        .update(cx, |f, cx| f.marked_text_range(w, cx).is_some())
                    {
                        return;
                    }
                    s.searching = false;
                    if let Some(focus) = s.search_return.take() {
                        focus.focus(w, cx);
                    }
                } else {
                    s.search_return = w.focused(cx);
                    s.searching = true;
                    s.ensure_library(None, cx);
                    let field = s.filter.clone();
                    w.on_next_frame(move |w, cx| field.read(cx).focus_handle(cx).focus(w, cx));
                }
                w.prevent_default();
                cx.stop_propagation();
                cx.notify();
            });
        })
        .detach();
        let mut workspace = Self {
            inventory: Default::default(),
            chooser: None,
            project_request: 0,
            tree: Default::default(),
            tree_cursor: None,
            tree_focus: cx.focus_handle(),
            tree_scroll: Default::default(),
            navigation_revision: 0,
            scoped_search: false,
            searching: false,
            search_return: None,
            page: None,
            filter,
            local,
            hosts: Default::default(),
            active: None,
            settings,
            error: None,
            _failure: failure,
            _updates: cx.observe_global::<crate::updates::Updates>(|_, cx| cx.notify()),
            _directory: cx.observe_global::<crate::preferences::HostDirectory>(|s, cx| {
                let saved = &cx.global::<crate::preferences::HostDirectory>().0;
                s.hosts.retain(|id, view| {
                    saved
                        .iter()
                        .any(|h| &h.id == id && view.read(cx).paired_with(h))
                });
                if s.active
                    .as_ref()
                    .is_some_and(|id| !s.hosts.contains_key(id))
                {
                    s.active = None;
                }
                s.sync_inventory(cx);
                cx.notify();
            }),
        };
        if let Some(host) = &workspace.local {
            workspace.attach_host(host, w, cx);
        }
        workspace.sync_inventory(cx);
        if restore_host
            && let Ok(Some(id)) =
                crate::workspace_state::read_preference::<Option<String>>("selected-host.json")
            && let Some(host) = cx
                .global::<crate::preferences::HostDirectory>()
                .0
                .iter()
                .find(|host| host.id == id)
                .cloned()
        {
            workspace.select(host, w, cx);
        }
        workspace
    }
    fn attach_host(&self, view: &Entity<Host>, w: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity();
        view.update(cx, |host, cx| {
            host.workspace = Some(workspace.downgrade());
            cx.observe(&workspace, |host, workspace, cx| {
                if workspace.read(cx).visible_host().map(Entity::entity_id) != Some(cx.entity_id())
                {
                    host.hide_palette(cx);
                }
            })
            .detach();
        });
        cx.subscribe(view, |s, _, request: &fleet::EnsureInventory, cx| {
            s.ensure_library(Some(&fleet::Query::parse(&request.query)), cx);
        })
        .detach();
        cx.subscribe_in(
            view,
            w,
            |s, host, request: &projects::NewProjectSession, w, cx| {
                if s.visible_host() == Some(host) {
                    s.pick_for_workbench(request, w, cx);
                }
            },
        )
        .detach();
        cx.subscribe(view, |s, host, used: &projects::UsedProject, cx| {
            if s.visible_host() == Some(&host) {
                s.remember_project(&used.0, cx);
            }
        })
        .detach();
        cx.subscribe_in(view, w, |s, _, target: &fleet::Destination, w, cx| {
            s.open_destination(target, w, cx)
        })
        .detach();
    }
    pub(crate) fn visible_host(&self) -> Option<&Entity<Host>> {
        if self.chooser.is_some()
            || self.searching
            || self.settings.is_some()
            || self.page.is_some()
        {
            return None;
        }
        match &self.active {
            Some(id) => self.hosts.get(id),
            None => self.local.as_ref(),
        }
    }
    pub(crate) fn session_host_name(&self, id: &Option<String>, cx: &App) -> Option<String> {
        (self.inventory.contains_key(id) && self.inventory.keys().any(Option::is_some))
            .then(|| self.host_name(id, cx))
    }
    pub(crate) fn navigation_revision(&self) -> u64 {
        self.navigation_revision
    }
    fn fleet_changed(&mut self, cx: &mut Context<Self>) {
        self.navigation_revision = self.navigation_revision.wrapping_add(1);
        if let Some(view) = self.visible_host() {
            gpui::App::notify(cx, view.entity_id());
        }
        cx.notify();
    }
    fn set_active(&mut self, id: Option<String>) {
        self.chooser = None;
        self.searching = false;
        self.search_return = None;
        if self.active != id {
            self.tree_cursor = None;
            crate::workspace_state::remember_host(id.clone());
            if let Some(entry) = self.inventory.get(&id) {
                self.tree.activate(entry.entity_id());
            }
            self.navigation_revision = self.navigation_revision.wrapping_add(1);
            self.active = id;
        }
    }

    fn select(
        &mut self,
        host: crate::platform::hosts::SavedHost,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chooser = None;
        self.searching = false;
        self.search_return = None;
        self.settings = None;
        self.page = None;
        self.error = None;
        if self.hosts.contains_key(&host.id) {
            self.set_active(Some(host.id));
            cx.notify();
            return;
        }
        let link = crate::platform::HostLink::Remote(host.clone());
        let recent =
            crate::workspace_state::read_preference::<crate::workspace_state::LastWorkspace>(
                &link.state_file("workspace.json"),
            )
            .unwrap_or_default();
        if let Some(route) = recent.saved_route(&recent.project)
            && recent.project.is_absolute()
        {
            self.show_project(host, recent.project, route, w, cx);
            return;
        }
        let owner = cx.weak_entity();
        let handle = w.window_handle();
        self.error = crate::views::remote_project::open(
            host.clone(),
            None,
            move |path, route, cx| {
                let _ = handle.update(cx, |_, w, cx| {
                    owner.update(cx, |s, cx| s.show_project(host, path, route, w, cx))
                });
            },
            cx,
        )
        .err();
        cx.notify();
    }

    fn show_project(
        &mut self,
        host: crate::platform::hosts::SavedHost,
        path: std::path::PathBuf,
        route: ox_desk_core::store::Route,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !cx
            .global::<crate::preferences::HostDirectory>()
            .0
            .iter()
            .any(|h| h.same_pairing(&host))
        {
            return;
        }
        let id = host.id.clone();
        let link = crate::platform::HostLink::Remote(host);
        let layouts = crate::workspace_state::read_preference(&link.state_file("layout.json"))
            .unwrap_or_default();
        let view = cx.new(|cx| Host::new(link, Some(route), path, layouts, None, false, w, cx));
        self.attach_host(&view, w, cx);
        self.hosts.insert(id.clone(), view);
        self.set_active(Some(id));
        self.settings = None;
        cx.notify();
    }
}
impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::platform::restore_probe::first_frame(window);
        crate::platform::layout_probe::begin_frame(window, "OX Desktop", || ());
        if !cx.has_global::<FailedView>() {
            let p = Palette::get(cx);
            let saved = &cx.global::<crate::preferences::HostDirectory>().0;
            let active = self
                .active
                .as_ref()
                .and_then(|id| self.hosts.get(id))
                .or(self.local.as_ref());
            if saved.is_empty()
                && self.settings.is_none()
                && self.chooser.is_none()
                && !self.searching
                && self.page.is_none()
                && let Some(host) = active
                && let Some(banner) = crate::updates::banner(p, cx)
            {
                return kit::col()
                    .size_full()
                    .child(banner)
                    .child(div().relative().flex_1().min_h_0().child(host.clone()))
                    .into_any_element();
            }
            if saved.is_empty()
                && self.settings.is_none()
                && self.chooser.is_none()
                && !self.searching
                && self.page.is_none()
                && let Some(host) = active
            {
                return host.clone().into_any_element();
            }
            let mut bar = kit::row().flex_wrap().gap_1().p_1();
            if self.local.is_some() {
                bar = bar.child(kit::button(
                    p,
                    "choose-local",
                    "This computer",
                    self.active.is_none() && self.settings.is_none() && self.page.is_none(),
                    true,
                    cx.listener(|s, _, _, cx| {
                        s.set_active(None);
                        s.page = None;
                        s.settings = None;
                        cx.notify();
                    }),
                ));
            }
            for host in saved {
                let host = host.clone();
                bar = bar.child(kit::button(
                    p,
                    format!("select-host-{}", host.id),
                    self.inventory
                        .get(&Some(host.id.clone()))
                        .map(|v| v.read(cx).display_name().to_owned())
                        .unwrap_or_else(|| host.name.clone()),
                    self.active.as_ref() == Some(&host.id)
                        && self.settings.is_none()
                        && self.page.is_none(),
                    true,
                    cx.listener(move |s, _, w, cx| s.select(host.clone(), w, cx)),
                ));
            }
            if !saved.is_empty() {
                let waiting: usize = self.inventory.values().map(|e| e.read(cx).waiting()).sum();
                for (page, label) in [
                    (fleet::Page::Projects, "Projects".to_owned()),
                    (fleet::Page::Sessions, "All sessions".to_owned()),
                    (fleet::Page::Inbox, format!("Inbox ({waiting})")),
                ] {
                    bar = bar.child(kit::button(
                        p,
                        label.clone(),
                        label,
                        self.page == Some(page),
                        true,
                        cx.listener(move |s, _, _, cx| {
                            s.chooser = None;
                            s.searching = false;
                            s.search_return = None;
                            s.page = if s.page == Some(page) {
                                None
                            } else {
                                Some(page)
                            };
                            if page != fleet::Page::Projects {
                                for host in s.inventory.values() {
                                    host.update(cx, |h, cx| h.load_saved(cx));
                                }
                            }
                            s.settings = None;
                            cx.notify();
                        }),
                    ));
                }
            }
            bar = bar.child(kit::button(
                p,
                "manage-hosts",
                "Manage hosts",
                self.settings.is_some(),
                true,
                cx.listener(|s, _, w, cx| {
                    s.searching = false;
                    s.search_return = None;
                    s.chooser = None;
                    s.page = None;
                    s.settings = if s.settings.is_some() {
                        None
                    } else {
                        Some(cx.new(|cx| crate::preferences::Hosts::new(w, cx)))
                    };
                    cx.notify()
                }),
            ));
            let mut view = kit::col()
                .size_full()
                .bg(rgb(p.bg))
                .text_color(rgb(p.fg))
                .child(bar);
            if let Some(error) = &self.error {
                view = view.child(kit::small(p, error.clone()));
            }
            if self.chooser.is_some() {
                view = view.child(self.project_chooser(cx));
            } else if self.searching {
                view = view.child(self.fleet_view(fleet::Page::Sessions, cx));
            } else if let Some(settings) = &self.settings {
                view = view.child(
                    div()
                        .id("manage-hosts-pane")
                        .overflow_y_scroll()
                        .flex_1()
                        .p_4()
                        .child(settings.clone()),
                );
            } else if self.page == Some(fleet::Page::Projects) {
                view = view.child(self.projects_page(cx));
            } else if let Some(page) = self.page {
                view = view.child(self.fleet_view(page, cx));
            } else if let Some(host) = active {
                view = view.child(div().relative().flex_1().min_h_0().child(host.clone()));
            } else {
                let (rows, _) = self.tree_rows(&None, "", None, cx);
                let ws = cx.weak_entity();
                let keyboard = fleet::tree::Keyboard::new(
                    &rows,
                    self.tree_scroll.clone(),
                    self.tree_focus.clone(),
                    None,
                );
                keyboard.recover(self.tree_cursor.as_deref(), ws.clone(), None, window);
                let width = px(300.).min(window.viewport_size().width);
                view = view.child(
                    kit::col()
                        .w(width)
                        .flex_1()
                        .min_h_0()
                        .bg(rgb(p.nav))
                        .child(kit::small(p, "Choose a host to open a project."))
                        .child(
                            uniform_list(
                                "client-hosts",
                                rows.len(),
                                cx.processor(move |s, range: std::ops::Range<usize>, _, _| {
                                    range
                                        .map(|i| {
                                            rows[i].render(
                                                &ws,
                                                width,
                                                p,
                                                &keyboard,
                                                i,
                                                s.tree_cursor.as_deref(),
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .track_scroll(&self.tree_scroll)
                            .flex_1()
                            .min_h_0(),
                        ),
                );
            }
            return view.into_any_element();
        }
        let p = Palette::get(cx);
        kit::col()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .child("OX Desktop closed the workspace after a display error.")
            .child(kit::small(
                p,
                "Reopen the app to resume a saved conversation.",
            ))
            .child(kit::button(
                p,
                "close-failed-workspace",
                "Close window",
                true,
                true,
                |_, window, _| window.remove_window(),
            ))
            .into_any_element()
    }
}
