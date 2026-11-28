//! One workbench per canonical project and billing route. Each owns its editors,
//! server requests and connection for the lifetime of the window.
use crate::appearance::{AccentColor, Step};
use crate::sidebar_state::{GroupingMode, SidebarState, ThreadMetaKey};
mod discovery;
use discovery::{Discovery, Location, Scope};
pub(super) use discovery::{ParentLink, WorkerIndex};
pub(crate) mod layout;
mod palette;
mod shortcuts;
use super::model_picker::{self, ModelPicker, ModelPickerEvent};
use super::running::Running;
use super::workbench::{CatalogChanged, Workbench};
use crate::kit::{DropdownMenu, SPACE_M, SPACE_S, SPACE_XS};
use crate::{
    activity,
    appearance::Palette,
    col,
    kit::button,
    platform, row, settings, small,
    workspace_state::{LastWorkspace, SeenResults},
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::FocusTrapElement;
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::host_protocol::SessionOutcome;
use ox_desk_core::runtime::{Billing, Runtime};
use ox_desk_core::store::{Connection, Route, ThreadRow};
use std::{ops::Range, path::PathBuf, time::Instant};

#[derive(Clone)]
enum CatalogEntry {
    /// Top `★ Pinned` section header.
    Pinned {
        count: usize,
        folded: bool,
        running: usize,
        waiting: usize,
    },
    /// User-defined named section header ("Inbox", "Review queue", …).
    Section {
        id: String,
        name: String,
        color: Option<AccentColor>,
        count: usize,
        folded: bool,
        running: usize,
        waiting: usize,
    },
    /// Auto-grouped project folder header (`ox`, `oxa`, …).
    Project {
        project: PathBuf,
        starred: bool,
        color: Option<AccentColor>,
        count: usize,
        folded: bool,
        running: usize,
        waiting: usize,
    },
    /// A day group header: its thread count and whether it is folded.
    Day {
        day: Option<chrono::NaiveDate>,
        count: usize,
        folded: bool,
    },
    JustFinished,
    ViewFinished,
    NoRunning,
    Thread {
        loc: Location,
        timestamp: i64,
        ordinal: usize,
        depth: usize,
        result: Option<SessionOutcome>,
        in_finished_shelf: bool,
    },
}

/// Inline editor state for renaming a thread row or creating/renaming a user section in the sidebar.
enum SidebarInlineEdit {
    RenameThread {
        key: ThreadMetaKey,
        scope: Scope,
        row: ThreadRow,
        input: Entity<gpui_component::input::InputState>,
    },
    NewSection {
        move_thread: Option<(ThreadMetaKey, ThreadRow, bool)>,
        input: Entity<gpui_component::input::InputState>,
    },
    RenameSection {
        id: String,
        input: Entity<gpui_component::input::InputState>,
    },
}

fn session_day(timestamp: i64) -> Option<chrono::NaiveDate> {
    if timestamp <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.with_timezone(&chrono::Local).date_naive())
}

fn day_label(day: Option<chrono::NaiveDate>, today: chrono::NaiveDate) -> String {
    let Some(day) = day else {
        return "Date unknown".into();
    };
    match (today - day).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => day.format("%b %-d, %Y").to_string(),
    }
}

fn age(now: i64, timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    match now.saturating_sub(timestamp).max(0) {
        0..60 => "now".into(),
        s @ 60..3600 => format!("{}m", s / 60),
        s @ 3600..86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Whether a new session can start on `route` now, or the one next step to show instead.
/// An API route needs its model alias to be configured. The ChatGPT plan needs a signed-in
/// account. The Claude plan is checked by the session itself, which asks to sign in.
fn new_route_gate(
    route: &Route,
    models: &[String],
    chatgpt_account: bool,
) -> Result<(), &'static str> {
    use ox_desk_core::runtime::Runtime;
    // Any catalog model may open (stars and the configured list only order choices);
    // `ox --model` validates the alias at launch and names the fix if it is unknown.
    let _ = models;
    match &route.billing {
        Billing::Api(_) => Ok(()),
        Billing::Subscription if route.runtime == Runtime::Codex && !chatgpt_account => {
            Err("Sign in to ChatGPT in Settings → Accounts first.")
        }
        Billing::Subscription => Ok(()),
    }
}

impl EventEmitter<crate::app::fleet::EnsureInventory> for Host {}
impl EventEmitter<crate::app::projects::NewProjectSession> for Host {}
impl EventEmitter<crate::app::projects::UsedProject> for Host {}
impl EventEmitter<crate::app::fleet::Destination> for Host {}
pub struct Host {
    pub(crate) workspace: Option<WeakEntity<crate::app::Workspace>>,
    link: platform::HostLink,
    palette: Option<palette::State>,
    _palette_keys: Subscription,
    /// The searchable all-models picker, opened from the route menu.
    model_picker: Option<(Entity<ModelPicker>, Subscription)>,
    /// The open picker is the AGENT picker (the composer's choices), not the all-models list.
    agent_picker: bool,
    /// Runtime the all-models picker routes to. Claude Code is offered only when aliases exist,
    /// as the route menu's Claude alias routes were.
    model_runtime: Runtime,
    sidebar_visible: bool,
    drawer_open: bool,
    drawer_focus: FocusHandle,
    before_drawer: Option<FocusHandle>,
    sidebar_width: u32,
    resizing_sidebar: bool,
    saved_layouts: layout::SavedLayouts,
    sidebar: SidebarState,
    sidebar_edit: Option<SidebarInlineEdit>,
    filter: Entity<gpui_component::input::InputState>,
    filter_task: Option<Task<()>>,
    recent: LastWorkspace,
    preferences: Option<Entity<crate::preferences::Preferences>>,
    before_settings: Option<FocusHandle>,
    workers: Entity<WorkerIndex>,
    panes: Vec<Entity<Workbench>>,
    active: usize,
    models: Entity<Vec<String>>,
    // Replacing this task cancels delivery of older settings and provider results.
    route_refresh: Task<()>,
    api_provider: Option<String>,
    native_account: bool,
    favorite_models: Vec<String>,
    catalog_versions: Vec<(usize, u64)>,
    catalog: Vec<CatalogEntry>,
    seen_results: SeenResults,
    fleet_tree: crate::app::fleet::tree::Tree,
    discovery: Discovery,
    running: Entity<Running>,
    list_focus: FocusHandle,
    list_scroll: UniformListScrollHandle,
    choose_route: Option<(PathBuf, ThreadRow)>,
    stats: bool,
    list_rows: u64,
    max_range: usize,
    furthest_row: usize,
    measure_from: Option<gpui::FrameDurationSnapshot>,
    _probe: Option<platform::probe::Socket>,
    message: Option<String>,
    /// Set when first-run onboarding opened Providers: models known then; a newly saved one becomes the route.
    provider_onboarding: Option<(EntityId, Vec<String>)>,
}
const MIN_THREAD_WIDTH: f32 = 600.;
const DIVIDER_WIDTH: f32 = 5.;

fn needs_drawer(viewport: Pixels, sidebar_width: u32) -> bool {
    viewport < px(sidebar_width as f32 + DIVIDER_WIDTH + MIN_THREAD_WIDTH)
}

impl Host {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        link: platform::HostLink,
        initial_route: Option<Route>,
        project: PathBuf,
        saved_layouts: layout::SavedLayouts,
        first_frame: Option<Instant>,
        stats: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let probe = if stats { Self::probe(window, cx) } else { None };
        let mut recent = if link.remote().is_some() {
            crate::workspace_state::read_preference(&link.state_file("workspace.json"))
                .unwrap_or_default()
        } else {
            LastWorkspace::load()
        };
        let layout = saved_layouts.for_project(&project);
        let route = initial_route
            .or_else(|| layout.active_route(&project))
            .or_else(|| recent.saved_route(&project))
            .unwrap_or_else(super::onboarding::first_launch_route);
        recent.remember(&project, &route);
        let message = LastWorkspace::save(
            link.remote().map(|_| link.state_file("workspace.json")),
            &project,
            &route,
        )
        .err()
        .map(|e| e.to_string());
        let models = cx.new(|_| Vec::new());
        let workers = cx.new(|_| WorkerIndex::default());
        let pane = Self::pane(
            link.clone(),
            workers.clone(),
            project,
            route,
            first_frame,
            stats,
            models.clone(),
            window,
            cx,
        );
        let route_refresh = Self::refresh_routes(link.clone(), window, cx);
        cx.observe_window_visibility(window, |s, _, w, cx| {
            if w.is_visible() {
                crate::sky::wake(cx);
                cx.notify()
            } else {
                s.running.update(cx, |r, cx| r.set_shown(false, cx));
            }
        })
        .detach();
        cx.observe_window_activation(window, |_, w, cx| {
            if w.is_window_active() {
                crate::sky::wake(cx);
            }
        })
        .detach();
        let filter = crate::kit::field("", "Search threads…", false, window, cx);
        cx.subscribe(
            &filter,
            |s, field, event: &gpui_component::input::InputEvent, cx| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    let query = field.read(cx).value().trim().to_owned();
                    s.filter_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(120))
                            .await;
                        let _ = this.update(cx, |s, cx| s.search(query, cx));
                    }));
                }
            },
        )
        .detach();

        let mut host = Self {
            workspace: None,
            link: link.clone(),
            palette: None,
            sidebar_visible: !layout.sidebar_hidden,
            drawer_open: false,
            drawer_focus: cx.focus_handle(),
            before_drawer: None,
            sidebar_width: layout.side_width(),
            resizing_sidebar: false,
            saved_layouts,
            sidebar: SidebarState::load(&link.state_file("sidebar.json")),
            sidebar_edit: None,
            _palette_keys: Self::palette_keys(window, cx),
            model_picker: None,
            agent_picker: false,
            model_runtime: Runtime::Codex,
            filter,
            filter_task: None,
            list_rows: 0,
            max_range: 0,
            furthest_row: 0,
            measure_from: None,
            _probe: probe,
            recent,
            preferences: None,
            before_settings: None,
            workers,
            panes: vec![pane],
            active: 0,
            models,
            route_refresh,
            api_provider: None,
            native_account: false,
            favorite_models: vec![],
            catalog_versions: vec![],
            catalog: vec![],
            seen_results: crate::workspace_state::read_preference(
                &link.state_file(SeenResults::FILE_NAME),
            )
            .unwrap_or_default(),
            fleet_tree: Default::default(),
            discovery: Discovery::default(),
            running: cx.new(|_| Running::new(link)),
            list_focus: cx.focus_handle().tab_stop(true),
            list_scroll: Default::default(),
            choose_route: None,
            stats,
            message,
            provider_onboarding: None,
        };
        host.restore_layout(layout, window, cx);
        host.start_discovery(cx);
        host.watch_sessions(window, cx);
        cx.observe_window_bounds(window, |_, _, cx| cx.notify())
            .detach();
        host
    }
    fn probe(window: &mut Window, cx: &mut Context<Self>) -> Option<platform::probe::Socket> {
        let (socket, requests) = match platform::probe::Socket::bind() {
            Ok(Some(probe)) => probe,
            Ok(None) => return None,
            Err(e) => {
                eprintln!("Could not bind desk stats socket: {e}");
                return None;
            }
        };
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(sample) = requests.recv_async().await {
                // Input runs outside the Host update: a click handler updates Host itself.
                if let Some(input) = &sample.input {
                    let reply = cx
                        .update(|window, cx| Self::probe_input(input, window, cx))
                        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                    let _ = sample.reply.send(reply);
                    // A hidden window gets no frame callbacks, so lay out every window now:
                    // the control registry and hit testing then reflect what the input did.
                    let _ = cx.update(|window, cx| {
                        {
                            let pending = window.draw(cx);
                            pending.clear(cx);
                        }
                        let main = window.window_handle().window_id();
                        for other in cx.windows() {
                            if other.window_id() != main {
                                let _ = other.update(cx, |_, window, cx| {
                                    let pending = window.draw(cx);
                                    pending.clear(cx);
                                });
                            }
                        }
                    });
                    continue;
                }
                if this
                    .update_in(cx, |s, window, cx| {
                        let stats = s.snapshot(window, cx);
                        let _ = sample.reply.send(stats);
                        if sample.reset {
                            s.measure_from = Some(window.frame_duration_snapshot());
                        }
                        if sample.refresh {
                            window.refresh();
                            let main = window.window_handle().window_id();
                            for other in cx.windows() {
                                if other.window_id() != main {
                                    let _ = other.update(cx, |_, window, _| window.refresh());
                                }
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
        Some(socket)
    }
    /// Runs a probe `press`/`key` in-process and returns `{"ok":true}` or `{"error":...}`.
    fn probe_input(input: &platform::probe::Input, window: &mut Window, cx: &mut App) -> String {
        use gpui::{
            Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
        };
        let result = match input {
            platform::probe::Input::Press(query) => {
                platform::layout_probe::find(query).map(|(target, position)| {
                    let click = move |window: &mut Window, cx: &mut App| {
                        let modifiers = Modifiers::default();
                        window.dispatch_event(
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position,
                                pressed_button: None,
                                modifiers,
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers,
                                click_count: 1,
                                first_mouse: false,
                            }),
                            cx,
                        );
                        window.dispatch_event(
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers,
                                click_count: 1,
                            }),
                            cx,
                        );
                    };
                    if window.window_handle().window_id() == target {
                        click(window, cx);
                    } else if let Some(other) =
                        cx.windows().into_iter().find(|w| w.window_id() == target)
                    {
                        let _ = other.update(cx, |_, window, cx| click(window, cx));
                    }
                })
            }
            platform::probe::Input::Key(key) => {
                // Test-only path: the leak is one short string per probe command.
                match gpui::Keystroke::parse(Box::leak(key.clone().into_boxed_str())) {
                    Ok(keystroke) => {
                        let active = cx.active_window();
                        match active {
                            Some(other)
                                if other.window_id() != window.window_handle().window_id() =>
                            {
                                let _ = other.update(cx, |_, window, cx| {
                                    window.dispatch_keystroke(keystroke, cx)
                                });
                            }
                            _ => {
                                window.dispatch_keystroke(keystroke, cx);
                            }
                        }
                        Ok(())
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        };
        match result {
            Ok(()) => serde_json::json!({"ok": true}),
            Err(error) => serde_json::json!({"error": error}),
        }
        .to_string()
    }
    fn snapshot(&self, window: &Window, cx: &App) -> String {
        let mut frames = window.frame_duration_snapshot();
        if let Some(start) = &self.measure_from {
            let _ = frames
                .draw_duration_histogram
                .subtract(&start.draw_duration_histogram);
            let _ = frames
                .dirty_to_present_histogram
                .subtract(&start.dirty_to_present_histogram);
            let _ = frames
                .present_interval_histogram
                .subtract(&start.present_interval_histogram);
        }
        let mut drains = 0;
        let mut notifications = 0;
        let mut rows = self.list_rows;
        let mut spawns = 0;
        let mut unknown = 0;
        let mut stop_ns = 0;
        let mut tail_ns = 0;
        let mut measured_items = 0;
        let mut uniform_row_budget = None;
        for pane in &self.panes {
            let pane = pane.read(cx);
            if let Some(stats) = &pane.stats {
                drains += stats.drains;
                notifications += stats.notifications;
                rows += stats.rows;
                spawns += stats.spawns;
                stop_ns = stop_ns.max(stats.stop_ns);
                if pane.front {
                    tail_ns = stats.painted.get();
                    measured_items = stats.measured_items.len();
                    uniform_row_budget = stats.tail_owner.as_ref().and_then(|(id, _)| {
                        pane.transcript_views
                            .get(id)?
                            .uniform_row_budget(stats.tail_height.get())
                    });
                }
            }
            unknown += pane.store.as_ref().map_or(0, |s| s.unknown_frames());
        }
        let active = self.panes[self.active].read(cx).store.as_ref();
        let (windows, layout) = crate::platform::layout_probe::snapshot();
        let settings = self
            .preferences
            .as_ref()
            .map(|p| p.read(cx).probe_state(cx));
        serde_json::json!({
            "windows": windows, "layout": layout, "settings": settings,
            "saved_sessions":self.discovery.rows.len(),"discovery_pending":self.discovery.pending,
            "connected":active.is_some_and(|s|s.state()==&Connection::Ready),
            "pane": self.active, "selected_thread": active.and_then(|s|s.selected()), "catalog_len": active.map_or(0, |s|s.threads().len()),
            "selected_requests":active.map_or(0, |s|s.asks().iter().filter(|a|a.thread.as_deref()==s.selected()).count()),
            "selected_parent":active.and_then(|s|s.catalog_row(s.selected()?)).and_then(|r|r.parent_thread_id.as_deref()),
            "visible_turn":active.and_then(|s|s.selected().and_then(|id|s.transcript(id))).is_some_and(|t|t.active_turn().is_some()),
            "draft":active.map(|s|s.draft(s.selected())),
            "voluntary_switches":ox_desk_core::perf::voluntary_switches(),
            "frames_drawn":frames.draw_duration_histogram.len(),"draw_p95_ns":frames.draw_duration_histogram.value_at_quantile(0.95),
            "dirty_to_present_p95_ns":frames.dirty_to_present_histogram.value_at_quantile(0.95),"present_interval_p95_ns":frames.present_interval_histogram.value_at_quantile(0.95),
            "drains":drains,"notifications":notifications,"rows":rows,"list_rows":self.list_rows,"max_list_range":self.max_range,"furthest_row":self.furthest_row,
            "spawns":spawns,"unknown":unknown,"stop_ns":stop_ns,"tail_ns":tail_ns,"visible":window.is_visible(),"measured_items":measured_items,"uniform_row_budget":uniform_row_budget,
        }).to_string()
    }
    /// Open the searchable picker over every model. Settings lists only starred discovered
    /// models, so the full catalog comes from the same request the Favorites page refreshes with.
    fn open_all_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.panes[self.active].read(cx).route.runtime;
        self.model_runtime = if !active.supports_api()
            || (active == Runtime::Claude && self.models.read(cx).is_empty())
        {
            Runtime::Codex
        } else {
            active
        };
        let entries: Vec<_> = self
            .models
            .read(cx)
            .iter()
            .map(|id| model_picker::api_entry(id))
            .collect();
        let picker = cx.new(|cx| ModelPicker::new(self.link.clone(), entries, window, cx));
        let subscription = cx.subscribe_in(
            &picker,
            window,
            |s: &mut Host, _, event: &ModelPickerEvent, window, cx| match event {
                ModelPickerEvent::Choose(alias) => {
                    s.model_picker = None;
                    let project = s.panes[s.active].read(cx).project.clone();
                    let route = s
                        .model_runtime
                        .route(Some(model_picker::bare(alias).to_owned()));
                    s.open(project, route, window, cx);
                }
                ModelPickerEvent::Starred(favorites) => s.favorite_models = favorites.clone(),
                ModelPickerEvent::HandOff(_) => {}
                ModelPickerEvent::AddProvider => {
                    s.model_picker = None;
                    let pane = s.panes[s.active].clone();
                    s.open_providers(&pane, window, cx);
                }
                ModelPickerEvent::Close => s.model_picker = None,
            },
        );
        self.agent_picker = false;
        self.model_picker = Some((picker.clone(), subscription));
        picker.update(cx, |p, cx| p.set_loading(true, cx));
        let link = self.link.clone();
        let task = cx.background_spawn(async move {
            link.remote()
                .map_or_else(settings::discover_models, |h| h.models())
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = task.await;
            picker.update(cx, |p, cx| {
                match result {
                    Ok(ids) => p.set_entries(
                        ids.iter().map(|id| model_picker::api_entry(id)).collect(),
                        cx,
                    ),
                    Err(error) => p.set_notice(Some(error), cx),
                }
                p.set_loading(false, cx);
            });
        })
        .detach();
        cx.notify();
    }
    /// The AGENT picker: the composer's model choices, shown over the window.
    fn open_agent_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model_picker.take().is_some() && self.agent_picker {
            self.agent_picker = false;
            cx.notify();
            return;
        }
        let pane = self.panes[self.active].clone();
        let (entries, runtime, owner) = {
            let workbench = pane.read(cx);
            let Some(store) = &workbench.store else {
                return self.open_all_models(window, cx);
            };
            let owner = store.selected().map(str::to_owned);
            (
                super::route_chip::composer_entries(store, owner.as_deref()),
                store.profile().route.runtime,
                owner,
            )
        };
        let picker = cx.new(|cx| {
            ModelPicker::new(self.link.clone(), entries, window, cx)
                .with_runtime(runtime)
                .with_route_catalog(&self.models, cx)
        });
        let subscription = cx.subscribe_in(
            &picker,
            window,
            move |s: &mut Host, picker, event: &ModelPickerEvent, window, cx| match event {
                ModelPickerEvent::Choose(id) => {
                    let runtime = picker.read(cx).runtime();
                    s.model_picker = None;
                    s.agent_picker = false;
                    let id = id.clone();
                    let owner = owner.clone();
                    s.panes[s.active]
                        .update(cx, |pane, cx| pane.choose_model(owner, id, runtime, cx));
                    cx.notify();
                }
                ModelPickerEvent::Starred(favorites) => s.favorite_models = favorites.clone(),
                ModelPickerEvent::HandOff(_) => {}
                ModelPickerEvent::AddProvider => {
                    s.model_picker = None;
                    s.agent_picker = false;
                    let pane = s.panes[s.active].clone();
                    s.open_providers(&pane, window, cx);
                }
                ModelPickerEvent::Close => {
                    s.model_picker = None;
                    s.agent_picker = false;
                    cx.notify();
                }
            },
        );
        self.agent_picker = true;
        self.model_picker = Some((picker, subscription));
        cx.notify();
    }
    fn model_overlay(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (picker, _) = self.model_picker.as_ref()?;
        let p = Palette::get(cx);
        let runtime = self.model_runtime;
        let mut runtimes = div().flex().gap_1().child(button(
            p,
            "model-runtime-codex",
            Runtime::Codex.name(),
            runtime == Runtime::Codex,
            true,
            cx.listener(|s, _, _, cx| {
                s.model_runtime = Runtime::Codex;
                cx.notify();
            }),
        ));
        if !self.models.read(cx).is_empty() {
            runtimes = runtimes.child(button(
                p,
                "model-runtime-claude",
                Runtime::Claude.name(),
                runtime == Runtime::Claude,
                true,
                cx.listener(|s, _, _, cx| {
                    s.model_runtime = Runtime::Claude;
                    cx.notify();
                }),
            ));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                .pt(px(72.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|s, _, _, cx| {
                        s.model_picker = None;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(440.))
                        .p_2()
                        .bg(rgb(p.panel))
                        .border_1()
                        .border_color(rgb(p.line_strong))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .children((!self.agent_picker).then_some(runtimes))
                        .child(picker.clone()),
                ),
        )
    }
    fn refresh_routes(
        link: platform::HostLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let host = link.remote().cloned();
        let snapshot = cx.background_spawn(async move {
            host.map_or_else(settings::load, |h| h.snapshot()).map(|s| {
                (
                    s.models,
                    s.api_provider,
                    s.chatgpt_account.is_some(),
                    s.favorites,
                )
            })
        });
        // Start both reads immediately, but publish the local settings first: a slow
        // snapshot must never overwrite the discovered aliases from this refresh.
        let discovered = cx.background_spawn(async move {
            link.remote()
                .map_or_else(settings::discover_models, |h| h.models())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = snapshot.await;
            let _ = this.update_in(cx, |s, window, cx| {
                match result {
                    Ok((models, provider, native_account, favorites)) => {
                        s.favorite_models = favorites;
                        s.models.write(cx, models);
                        s.api_provider = provider;
                        if native_account && !s.native_account {
                            // A sign-in started in Accounts has no live app-server to notify
                            // when the signed-out OX helper exited before initialization.
                            s.panes[s.active].update(cx, |pane, cx| {
                                if pane.waiting_for_subscription() {
                                    pane.reconnect(window, cx);
                                }
                            });
                        }
                        s.native_account = native_account;
                        s.finish_provider_onboarding(false, window, cx);
                    }
                    Err(error) => s.message = Some(error),
                }
                if window.is_visible() {
                    cx.notify();
                }
            });
            let found = discovered.await;
            let _ = this.update_in(cx, |s, window, cx| {
                if let Ok(found) = found {
                    let merged = ox_desk_core::ports::ox::merge_catalog(s.models.read(cx), found);
                    if &merged != s.models.read(cx) {
                        s.models.write(cx, merged);
                        cx.notify();
                    }
                }
                s.finish_provider_onboarding(true, window, cx);
            });
        })
    }
    fn finish_provider_onboarding(
        &mut self,
        catalog_finished: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preferences.is_some() {
            return;
        }
        let Some((started_from, before)) = &self.provider_onboarding else {
            return;
        };
        let owner = &self.panes[self.active];
        if owner.entity_id() != *started_from {
            self.provider_onboarding = None;
            return;
        }
        let model = self
            .models
            .read(cx)
            .iter()
            .find(|model| !before.contains(model))
            .cloned();
        if catalog_finished || model.is_some() {
            self.provider_onboarding = None;
        }
        if let Some(model) = model {
            let pane = owner.read(cx);
            self.open(pane.project.clone(), pane.api_route(model), window, cx);
        }
    }
    fn compact(&self, window: &Window) -> bool {
        needs_drawer(window.viewport_size().width, self.sidebar_width)
    }

    fn close_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drawer_open {
            self.drawer_open = false;
            if let Some(previous) = self.before_drawer.take() {
                window.focus(&previous, cx);
            }
            cx.notify();
        }
    }

    fn reveal_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = if self.compact(window) {
            self.drawer_open
        } else {
            self.sidebar_visible
        };
        if !visible {
            self.toggle_sidebar(window, cx);
        }
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact(window) {
            if self.drawer_open {
                self.close_drawer(window, cx);
            } else {
                self.before_drawer = window.focused(cx);
                self.drawer_open = true;
                let filter = self.filter.read(cx).focus_handle(cx);
                window.on_next_frame(move |w, cx| w.focus(&filter, cx));
            }
        } else {
            self.sidebar_visible = !self.sidebar_visible;
        }
        cx.notify();
    }

    fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_drawer(window, cx);
        if self.preferences.take().is_some() {
            if let Some(previous) = self.before_settings.take() {
                window.focus(&previous, cx);
            }
            self.route_refresh = Self::refresh_routes(self.link.clone(), window, cx);
        } else {
            let project = self.panes[self.active].read(cx).project.clone();
            self.before_settings = window.focused(cx);
            let view = cx.new(|cx| {
                crate::preferences::Preferences::new(self.link.clone(), project, window, cx)
            });
            Self::listen_for_settings_continue(&view, window, cx);
            self.preferences = Some(view);
        }
        let front = self.preferences.is_none();
        self.panes[self.active].update(cx, |p, cx| p.set_front_in(front, window, cx));
        cx.notify();
    }
    /// Open Settings on its Remote access page, or move an open Settings there.
    fn show_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preferences.is_none() {
            self.show_settings(window, cx);
        }
        if let Some(view) = &self.preferences {
            view.update(cx, |view, cx| view.open_remote(cx));
        }
    }

    /// The sidebar footer: an icon rail on the keyline, and the one running indicator.
    fn sidebar_footer(
        &self,
        p: Palette,
        running_count: usize,
        running_open: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let settings_label = if self.preferences.is_some() {
            "Back to thread"
        } else {
            "Settings"
        };
        let pane = self.panes[self.active].downgrade();
        let usage = gpui_component::popover::Popover::new("sidebar-usage")
            .anchor(Anchor::BottomLeft)
            .trigger(crate::kit::icon(
                p,
                "sidebar-usage-button",
                gpui_component::IconName::ChartPie,
                "Usage",
                true,
                |_, _, _| {},
            ))
            .content(move |_, window, cx| {
                pane.read_with(cx, |view, _| view.usage_details(p, window, &pane))
                    .unwrap_or_else(|_| div().into_any_element())
            });
        let running = self.running.clone();
        row()
            .flex_shrink_0()
            .h(px(44.))
            .px(px(crate::kit::GUTTER))
            .gap(px(SPACE_XS))
            .child(
                crate::kit::icon(
                    p,
                    "settings",
                    gpui_component::IconName::Settings,
                    settings_label,
                    true,
                    cx.listener(|s, _, w, cx| s.show_settings(w, cx)),
                )
                .when(self.preferences.is_some(), |b| b.text_color(rgb(p.accent))),
            )
            .child(crate::kit::icon(
                p,
                "remote-access",
                gpui_component::IconName::Globe,
                "Remote access",
                true,
                cx.listener(|s, _, w, cx| s.show_remote(w, cx)),
            ))
            .child(usage)
            .child(div().flex_1())
            .when(running_count > 0, |footer| {
                footer.child(
                    crate::kit::nav_row(
                        "running-toggle",
                        format!(
                            "{running_count} open sessions, {}",
                            if running_open { "hide" } else { "show" }
                        ),
                        crate::kit::ICON_SIZE,
                        true,
                        move |_, _, cx| {
                            running.update(cx, |r, cx| {
                                r.open = !r.open;
                                cx.notify();
                            })
                        },
                    )
                    .w_auto()
                    .child(
                        row()
                            .gap(px(SPACE_S))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(p.muted)))
                            .child(
                                div()
                                    .text_size(p.text(Step::Meta))
                                    .text_color(rgb(p.muted))
                                    .child(format!("{running_count} open")),
                            )
                            .child(
                                div()
                                    .text_size(p.text(Step::Label))
                                    .text_color(rgb(p.quiet()))
                                    .child(if running_open { "▾" } else { "▴" }),
                            ),
                    ),
                )
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn pane(
        link: platform::HostLink,
        workers: Entity<WorkerIndex>,
        project: PathBuf,
        route: Route,
        first_frame: Option<Instant>,
        stats: bool,
        models: Entity<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Workbench> {
        let pane = cx.new(|cx| {
            Workbench::new(
                link,
                workers,
                project,
                route,
                first_frame,
                stats,
                models,
                window,
                cx,
            )
        });
        Self::subscribe_pane(&pane, window, cx);
        pane
    }

    #[allow(clippy::too_many_arguments)]
    fn lazy_pane(
        link: platform::HostLink,
        workers: Entity<WorkerIndex>,
        project: PathBuf,
        route: Route,
        stats: bool,
        models: Entity<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Workbench> {
        let pane = cx.new(|cx| {
            Workbench::new_lazy(link, workers, project, route, stats, models, window, cx)
        });
        Self::subscribe_pane(&pane, window, cx);
        pane
    }

    fn subscribe_pane(pane: &Entity<Workbench>, window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(
            pane,
            window,
            |s, pane, ready: &super::handoff_sheet::HandoffReady, window, cx| {
                if *pane != s.panes[s.active]
                    || pane
                        .read(cx)
                        .store
                        .as_ref()
                        .and_then(|store| store.selected())
                        != Some(ready.source_session.as_str())
                {
                    return;
                }
                let source = s.link.remote().map(|h| h.id.clone());
                if let Some(workspace) = s.workspace.as_ref().and_then(|w| w.upgrade()) {
                    workspace.update(cx, |app, cx| {
                        if !app.handoff_still_visible(&source, &ready.host, ready.registration) {
                            return;
                        }
                        app.open_confirmed_result(
                            ready.host.clone(),
                            ready.scope.clone(),
                            ready.session.clone(),
                            true,
                            window,
                            cx,
                        )
                    });
                }
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, _, _: &super::workbench::FocusSearch, window, cx| {
                s.reveal_sidebar(window, cx);
                s.filter.read(cx).focus_handle(cx).focus(window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, _, _: &super::workbench::empty::OpenProject, window, cx| {
                s.pick_project(window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, _, event: &super::workbench::OpenOnPhone, window, cx| {
                s.show_remote(window, cx);
                if let Some(view) = &s.preferences {
                    view.update(cx, |view, cx| {
                        view.open_session_on_phone(
                            &event.scope,
                            &event.thread,
                            event.title.clone(),
                            cx,
                        )
                    });
                }
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, owner, event: &super::onboarding::SwitchRoute, window, cx| {
                let project = owner.read(cx).project.clone();
                let route = match &event.0 {
                    Some(route) => route.clone(),
                    None => match s.models.read(cx).first() {
                        Some(model) => owner.read(cx).api_route(model.clone()),
                        None => {
                            s.provider_onboarding =
                                Some((owner.entity_id(), s.models.read(cx).clone()));
                            return s.open_providers(owner, window, cx);
                        }
                    },
                };
                s.open(project, route, window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, owner, _: &super::onboarding::ConfigureProvider, window, cx| {
                s.provider_onboarding = Some((owner.entity_id(), s.models.read(cx).clone()));
                s.open_providers(owner, window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, owner, event: &super::route_chip::NewModelRoute, window, cx| {
                if owner.read(cx).image_work.contains_key(&None) {
                    s.message = Some(
                        "Wait for image attachment preparation to finish before switching models."
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                if event.0.runtime == Runtime::Antigravity
                    && owner.read(cx).store.as_ref().is_some_and(|store| {
                        store.selected().is_none() && !store.draft_images(None).is_empty()
                    })
                {
                    s.message = Some("Remove draft attachments before switching to Antigravity, or start an Antigravity session from the sidebar.".into());
                    cx.notify();
                    return;
                }
                let (project, same_route, carried) = owner.update(cx, |view, _| {
                    let same_route = view.route == event.0;
                    let carried = view.store.as_mut().and_then(|store| {
                        if store.selected().is_some() {
                            return None;
                        }
                        let text = store.draft(None).to_owned();
                        let images = store.draft_images(None).to_vec();
                        if !same_route && (!text.is_empty() || !images.is_empty()) {
                            store.edit(None, String::new());
                            while !store.draft_images(None).is_empty() {
                                store.remove_image(None, 0);
                            }
                        }
                        (!text.is_empty() || !images.is_empty()).then_some((text, images))
                    });
                    (view.project.clone(), same_route, carried)
                });
                match new_route_gate(&event.0, s.models.read(cx), s.native_account) {
                    Ok(()) => {
                        s.open(project, event.0.clone(), window, cx);
                        s.panes[s.active].update(cx, |pane, cx| {
                            pane.action(cx, DeskAction::NewDraft);
                            if !same_route && let Some((text, images)) = carried {
                                pane.adopt_new_draft(text, images, cx);
                            }
                        });
                    }
                    Err(message) => {
                        if !same_route && let Some((text, images)) = carried {
                            owner.update(cx, |pane, cx| pane.adopt_new_draft(text, images, cx));
                        }
                        s.message = Some(message.into());
                        cx.notify();
                    }
                }
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, owner, _: &super::route_chip::OpenProviders, window, cx| {
                s.open_providers(owner, window, cx);
            },
        )
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, owner, _: &super::worker_sheet::OpenWorkerHosts, window, cx| {
                let project = owner.read(cx).project.clone();
                s.before_settings = window.focused(cx);
                owner.update(cx, |p, cx| p.set_front(false, cx));
                s.preferences = Some(cx.new(|cx| {
                    crate::preferences::Preferences::worker_hosts(
                        s.link.clone(),
                        project,
                        window,
                        cx,
                    )
                }));
                cx.notify();
            },
        )
        .detach();
        cx.subscribe(pane, |s, _, _: &CatalogChanged, cx| {
            s.refresh_palette_search(cx);
            cx.notify();
        })
        .detach();
        cx.subscribe(pane, |s, _, event: &super::transcript::ResultViewed, cx| {
            if s.seen_results
                .record(&event.scope, &event.thread, &event.result)
            {
                crate::workspace_state::remember_seen_result(
                    s.link.state_file(SeenResults::FILE_NAME),
                    event.scope.clone(),
                    event.thread.clone(),
                    event.result.clone(),
                );
                s.discovery.revision += 1;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe_in(
            pane,
            window,
            |s, owner, event: &super::workbench::OpenThread, window, cx| {
                s.open_thread(owner, event.0.clone(), window, cx);
            },
        )
        .detach();
        cx.subscribe(
            pane,
            |s, owner, event: &super::workbench::thread_menu::ThreadRenamed, cx| {
                let view = owner.read(cx);
                let key =
                    ThreadMetaKey::new(view.project.clone(), view.route.runtime, event.id.clone());
                s.sidebar.set_custom_title(&key, Some(event.name.clone()));
                s.save_sidebar(cx);
            },
        )
        .detach();
    }
    fn listen_for_settings_continue(
        view: &Entity<crate::preferences::Preferences>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(
            view,
            window,
            |host, owner, event: &crate::preferences::ContinueSetup, window, cx| {
                if host.preferences.as_ref() == Some(owner) {
                    host.provider_onboarding = None;
                    host.show_settings(window, cx);
                    if let Some(model) = &event.0 {
                        let pane = host.panes[host.active].read(cx);
                        host.open(
                            pane.project.clone(),
                            pane.api_route(model.clone()),
                            window,
                            cx,
                        );
                    }
                }
            },
        )
        .detach();
    }

    fn open_providers(
        &mut self,
        owner: &Entity<Workbench>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = owner.read(cx).project.clone();
        owner.update(cx, |p, cx| p.set_front(false, cx));
        let view = cx.new(|cx| {
            crate::preferences::Preferences::providers(self.link.clone(), project, window, cx)
        });
        Self::listen_for_settings_continue(&view, window, cx);
        self.preferences = Some(view);
        cx.notify();
    }
    fn switch(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.close_drawer(window, cx);
        if self.preferences.take().is_some() {
            self.before_settings = None;
            self.panes[self.active].update(cx, |p, cx| p.set_front_in(true, window, cx));
            cx.notify();
        }
        if self.active == index {
            return;
        }
        self.panes[self.active].update(cx, |p, cx| p.set_front(false, cx));
        self.active = index;
        self.choose_route = None;
        self.search(self.filter.read(cx).value().trim().to_owned(), cx);
        self.panes[index].update(cx, |p, cx| p.set_front_in(true, window, cx));
        let p = self.panes[index].read(cx);
        self.recent.remember(&p.project, &p.route);
        if let Err(err) = LastWorkspace::save(
            self.link
                .remote()
                .map(|_| self.link.state_file("workspace.json")),
            &p.project,
            &p.route,
        ) {
            self.message = Some(err.to_string());
        }
        cx.notify();
    }
    fn open(
        &mut self,
        project: PathBuf,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.emit(crate::app::projects::UsedProject(project.clone()));
        self.restore_project(&project, window, cx);
        let i = self
            .panes
            .iter()
            .position(|p| {
                let p = p.read(cx);
                p.project == project && p.route == route
            })
            .unwrap_or_else(|| {
                self.panes.push(Self::pane(
                    self.link.clone(),
                    self.workers.clone(),
                    project,
                    route,
                    None,
                    self.stats,
                    self.models.clone(),
                    window,
                    cx,
                ));
                self.panes.len() - 1
            });
        self.switch(i, window, cx);
    }
    fn pick_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(host) = self.link.remote().cloned() {
            let target = cx.weak_entity();
            let handle = window.window_handle();
            let route = self.panes[self.active].read(cx).route.clone();
            self.message = super::remote_project::open(
                host,
                Some(route),
                move |path, route, cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        target.update(cx, |s, cx| s.open(path, route, window, cx))
                    });
                },
                cx,
            )
            .err();
            return;
        }
        let choice = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |s, cx| {
            let result = choice.await;
            let _ = s.update_in(cx, |s, window, cx| match result {
                Ok(Ok(Some(paths))) if !paths.is_empty() => {
                    let path = paths[0].clone();
                    s.with_project_dir(path, true, window, cx, |s, project, window, cx| {
                        let route = s.recent.route(&project);
                        s.open(project, route, window, cx);
                    });
                }
                Ok(Err(err)) => {
                    s.message = Some(err.to_string());
                    cx.notify();
                }
                _ => (),
            });
        })
        .detach();
    }
    /// Check, off the UI thread, that `path` (canonicalised when asked) is a directory,
    /// then continue with it. A stalled network mount cannot freeze the window this way.
    /// An unavailable project sets the sidebar message instead.
    fn with_project_dir(
        &mut self,
        path: PathBuf,
        canonical: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.link.remote().is_some() {
            if path.is_absolute() {
                then(self, path, window, cx);
            }
            return;
        }
        let probe = path.clone();
        let task = cx.background_spawn(async move {
            let resolved = if canonical {
                probe.canonicalize().ok()
            } else {
                Some(probe)
            };
            resolved.filter(|p| p.is_dir())
        });
        cx.spawn_in(window, async move |s, cx| {
            let resolved = task.await;
            let _ = s.update_in(cx, |s, window, cx| match resolved {
                Some(project) => then(s, project, window, cx),
                None => {
                    s.message = Some(format!("Project unavailable: {}", path.display()));
                    cx.notify();
                }
            });
        })
        .detach();
    }
    /// The status filter as one quiet icon menu in the search row. Each choice sets the
    /// same `is:` token the search box accepts; "All" clears it.
    fn filter_menu(&self, p: Palette, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const CHOICES: [(&str, &str); 5] = [
            ("", "All"),
            ("is:running", "Running"),
            ("is:waiting", "Needs you"),
            ("is:finished", "Finished"),
            ("is:pinned", "Pinned"),
        ];
        let current = self.filter.read(cx).value().to_string();
        let active = CHOICES
            .iter()
            .skip(1)
            .find(|(token, _)| current.split_whitespace().any(|word| word == *token))
            .copied();
        let label = match active {
            Some((_, name)) => format!("Filter threads: {name}"),
            None => "Filter threads".to_owned(),
        };
        let filter = self.filter.clone();
        let host = cx.weak_entity();
        crate::kit::icon(
            p,
            "thread-filter",
            gpui_component::IconName::Settings2,
            label,
            true,
            |_, _, _| {},
        )
        .size(px(24.))
        .when(active.is_some(), |b| b.text_color(rgb(p.accent)))
        .dropdown_menu(move |mut menu, _, _| {
            for (token, name) in CHOICES {
                let filter = filter.clone();
                let checked = match active {
                    Some((current, _)) => current == token,
                    None => token.is_empty(),
                };
                menu = menu.item(crate::kit::menu_item(name).checked(checked).on_click(
                    move |_, window, cx| {
                        let value = filter.read(cx).value().to_string();
                        let next = select_status_filter(&value, token);
                        filter.update(cx, |field, cx| field.set_value(next, window, cx));
                    },
                ));
            }
            let host = host.clone();
            menu.separator()
                .item(
                    crate::kit::menu_item("New section…").on_click(move |_, window, cx| {
                        let _ = host.update(cx, |s, cx| s.begin_new_section(None, window, cx));
                    }),
                )
        })
    }
    fn grouping_control(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        row()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(crate::kit::CONTROL_RADIUS))
            .bg(rgb(p.surface()))
            .children(
                [
                    ("threads-recent", "Recent", GroupingMode::Timeline),
                    ("threads-folders", "Folders", GroupingMode::Folders),
                ]
                .into_iter()
                .map(|(id, label, grouping)| {
                    let selected = self.sidebar.grouping == grouping;
                    crate::kit::toolbar_button(
                        p,
                        id,
                        label,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            if s.sidebar.grouping != grouping {
                                s.sidebar.grouping = grouping;
                                s.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                                s.save_sidebar(cx);
                            }
                        }),
                    )
                    .h(px(crate::kit::CHIP_HEIGHT))
                    .flex_1()
                    .toggled(selected)
                    .when(selected, |b| {
                        b.bg(rgb(p.hover))
                            .text_color(rgb(p.fg))
                            .font_weight(FontWeight::MEDIUM)
                    })
                }),
            )
    }
    pub(super) fn save_sidebar(&mut self, cx: &mut Context<Self>) {
        self.discovery.revision += 1;
        self.fleet_tree.revision = u64::MAX;
        crate::workspace_state::remember_sidebar(
            self.link.state_file("sidebar.json"),
            self.sidebar.clone(),
        );
        cx.notify();
    }

    fn begin_row_rename(
        &mut self,
        key: ThreadMetaKey,
        scope: Scope,
        row: ThreadRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = self
            .sidebar
            .thread(&key)
            .and_then(|m| m.custom_title.as_deref())
            .unwrap_or_else(|| row.title());
        let input = crate::kit::field(initial, "Thread name", false, window, cx);
        let focus = input.read(cx).focus_handle(cx);
        window.on_next_frame(move |w, cx| focus.focus(w, cx));
        cx.subscribe_in(
            &input,
            window,
            |s, field, event: &gpui_component::input::InputEvent, window, cx| {
                if matches!(event, gpui_component::input::InputEvent::PressEnter { .. })
                    && matches!(
                        &s.sidebar_edit,
                        Some(SidebarInlineEdit::RenameThread { input, .. }) if input == field
                    )
                {
                    s.commit_sidebar_edit(window, cx);
                }
            },
        )
        .detach();
        self.sidebar_edit = Some(SidebarInlineEdit::RenameThread {
            key,
            scope,
            row,
            input,
        });
        cx.notify();
    }

    fn begin_new_section(
        &mut self,
        move_thread: Option<(ThreadMetaKey, ThreadRow, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = crate::kit::field("", "New section name…", false, window, cx);
        let focus = input.read(cx).focus_handle(cx);
        window.on_next_frame(move |w, cx| focus.focus(w, cx));
        cx.subscribe_in(
            &input,
            window,
            |s, field, event: &gpui_component::input::InputEvent, window, cx| {
                if matches!(event, gpui_component::input::InputEvent::PressEnter { .. })
                    && matches!(
                        &s.sidebar_edit,
                        Some(SidebarInlineEdit::NewSection { input, .. }) if input == field
                    )
                {
                    s.commit_sidebar_edit(window, cx);
                }
            },
        )
        .detach();
        self.sidebar_edit = Some(SidebarInlineEdit::NewSection { move_thread, input });
        cx.notify();
    }

    fn begin_rename_section(
        &mut self,
        id: String,
        current_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = crate::kit::field(current_name, "Section name…", false, window, cx);
        let focus = input.read(cx).focus_handle(cx);
        window.on_next_frame(move |w, cx| focus.focus(w, cx));
        cx.subscribe_in(
            &input,
            window,
            |s, field, event: &gpui_component::input::InputEvent, window, cx| {
                if matches!(event, gpui_component::input::InputEvent::PressEnter { .. })
                    && matches!(
                        &s.sidebar_edit,
                        Some(SidebarInlineEdit::RenameSection { input, .. }) if input == field
                    )
                {
                    s.commit_sidebar_edit(window, cx);
                }
            },
        )
        .detach();
        self.sidebar_edit = Some(SidebarInlineEdit::RenameSection { id, input });
        cx.notify();
    }

    fn commit_sidebar_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.sidebar_edit.take() else {
            return;
        };
        match edit {
            SidebarInlineEdit::RenameThread {
                key,
                scope,
                row,
                input,
            } => {
                let name = input.read(cx).value().trim().to_owned();
                if name.is_empty() {
                    cx.notify();
                    return;
                }
                self.sidebar.set_custom_title(&key, Some(name.clone()));
                self.sidebar.thread_mut(&key).cache_from(&row, scope.api);
                self.sidebar.set_custom_title(&key, Some(name.clone()));
                // Also dispatch RenameThread to any connected pane owning this thread, or
                // open & rename if this is the active project.
                for pane in &self.panes {
                    let matches = {
                        let p = pane.read(cx);
                        p.project == key.project
                            && scope.matches(&p.route)
                            && p.store
                                .as_ref()
                                .is_some_and(|s| s.can_manage_thread(&key.id))
                    };
                    if matches {
                        pane.update(cx, |p, cx| {
                            p.action(cx, DeskAction::RenameThread(key.id.clone(), name.clone()));
                        });
                    }
                }
                self.save_sidebar(cx);
            }
            SidebarInlineEdit::NewSection { move_thread, input } => {
                let name = input.read(cx).value().trim().to_owned();
                if !name.is_empty() {
                    self.sidebar.grouping = GroupingMode::Folders;
                    let id = self.sidebar.create_section(name, Some(AccentColor::Cobalt));
                    if let Some((key, row, api)) = move_thread {
                        self.sidebar.move_to_section(&key, Some(id), &row, api);
                    }
                    self.save_sidebar(cx);
                } else {
                    cx.notify();
                }
            }
            SidebarInlineEdit::RenameSection { id, input } => {
                let name = input.read(cx).value().trim().to_owned();
                if self.sidebar.rename_section(&id, name) {
                    self.save_sidebar(cx);
                } else {
                    cx.notify();
                }
            }
        }
        let _ = window;
    }

    fn search(&mut self, query: String, cx: &mut Context<Self>) {
        for pane in &self.panes {
            if pane.read(cx).query != query {
                pane.update(cx, |p, cx| {
                    p.query = query.clone();
                    p.action(cx, DeskAction::SearchThreads(query.clone()));
                });
            }
        }
        cx.notify();
    }
    pub(crate) fn project_route(&self, path: &std::path::Path) -> Option<Route> {
        self.recent.saved_route(path)
    }
    pub(crate) fn sidebar_state(&self) -> &SidebarState {
        &self.sidebar
    }
    pub(crate) fn paired_with(&self, host: &crate::platform::hosts::SavedHost) -> bool {
        self.link
            .remote()
            .is_some_and(|other| host.same_pairing(other))
    }
    pub(crate) fn focus_selection(&mut self, cx: &mut Context<Self>) {
        self.panes[self.active].update(cx, |pane, cx| {
            pane.focus_editor_for = Some(
                pane.store
                    .as_ref()
                    .and_then(|s| s.selected())
                    .map(str::to_owned),
            );
            cx.notify();
        });
    }
    pub(crate) fn new_project_draft(
        &mut self,
        project: PathBuf,
        route: Route,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(project, route, w, cx);
        self.panes[self.active].update(cx, |p, cx| p.start_project_draft(cx));
    }
    fn request_project_draft(
        &mut self,
        project: PathBuf,
        route: Route,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.is_some()
            && !cx
                .global::<crate::preferences::HostDirectory>()
                .0
                .is_empty()
        {
            cx.emit(crate::app::projects::NewProjectSession { project, route });
        } else {
            self.new_project_draft(project, route, w, cx);
        }
    }
    fn request_active_draft(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let pane = self.panes[self.active].read(cx);
        if !pane
            .store
            .as_ref()
            .is_some_and(|store| store.state() == &Connection::Ready)
        {
            return;
        }
        self.request_project_draft(pane.project.clone(), pane.route.clone(), w, cx);
    }
    pub(crate) fn show_thread(
        &mut self,
        scope: ox_desk_core::host_protocol::Scope,
        id: Option<String>,
        row: Option<ThreadRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = id else {
            self.open(scope.project, scope.route, window, cx);
            self.panes[self.active].update(cx, |pane, cx| {
                pane.focus_editor_for = Some(None);
                pane.action(cx, DeskAction::NewDraft)
            });
            return;
        };
        self.with_project_dir(
            scope.project,
            false,
            window,
            cx,
            move |s, project, w, cx| {
                let row = row.unwrap_or_else(|| ThreadRow {
                    id: id.clone(),
                    cwd: project.clone(),
                    model_provider: String::new(),
                    worker: false,
                    parent_thread_id: None,
                    agent_nickname: None,
                    model: None,
                    name: None,
                    preview: String::new(),
                    generated_title: None,
                    updated_at: 0,
                    status: None,
                    unread: false,
                    activity_order: 0,
                });
                s.clear_row_unread(
                    &ThreadMetaKey::new(project.clone(), scope.route.runtime, id),
                    cx,
                );
                let selected = row.id.clone();
                s.open_catalog(project, row, scope.route, w, cx);
                s.panes[s.active].update(cx, |pane, cx| {
                    pane.focus_editor_for = Some(Some(selected));
                    cx.notify();
                });
            },
        );
    }
    fn open_catalog(
        &mut self,
        project: PathBuf,
        row: ThreadRow,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(project, route, window, cx);
        self.choose_route = None;
        self.panes[self.active].update(cx, |pane, cx| pane.select_catalog(row, false, cx));
        cx.notify();
    }
    fn pane_waiting_count(&self, pane: &Workbench) -> usize {
        pane.store.as_ref().map_or_else(
            || self.discovery.waiting_count(&pane.project, &pane.route),
            |s| s.asks().len(),
        )
    }
    fn project_row_state(
        &self,
        project: &std::path::Path,
        t: &ThreadRow,
        scope: Scope,
        cx: &App,
    ) -> (Option<Route>, Option<&'static str>) {
        let hosted = self.discovery.route_status(project, scope, t);
        let mut status = hosted
            .as_ref()
            .and_then(|h| h.1)
            .or_else(|| t.status.as_ref().and_then(|s| s.label()));
        let mut owner = hosted.map(|h| h.0);
        for pane in &self.panes {
            let view = pane.read(cx);
            if view.project != project || !scope.matches(&view.route) {
                continue;
            }
            let Some(store) = &view.store else { continue };
            if store.profile().provider != t.model_provider {
                continue;
            }
            if store
                .asks()
                .iter()
                .any(|a| a.thread.as_ref() == Some(&t.id))
            {
                status = Some("Needs you");
            }
            if store.is_native_live(&t.id) {
                // Another client is driving it: not an error, not ended, not ours.
                status = Some(if store.native_app() == "Codex" {
                    "Live in Codex"
                } else {
                    "Live in Claude Code"
                });
            } else if store.is_ended(&t.id) {
                status = status.or(Some("Ended"));
            }
            if let Some(thread) = store.transcript(&t.id) {
                if thread.is_ready() {
                    owner = Some(view.route.clone());
                }
                status = status.or_else(|| {
                    if store.is_native_live(&t.id) {
                        None
                    } else if thread.active_turn().is_some() {
                        Some("Running")
                    } else if thread.error().is_some() {
                        Some("Error")
                    } else if thread.unread() {
                        Some("Unread")
                    } else {
                        None
                    }
                });
            }
        }
        (
            owner.or_else(|| {
                scope.saved_route(t, self.models.read(cx), self.api_provider.as_deref())
            }),
            status,
        )
    }

    fn activate_row(
        &mut self,
        project: PathBuf,
        scope: Scope,
        row: ThreadRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_sidebar_row(project, scope, row, false, window, cx);
    }

    fn activate_sidebar_row(
        &mut self,
        project: PathBuf,
        scope: Scope,
        row: ThreadRow,
        reveal_result: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_project_dir(project, false, window, cx, move |s, project, window, cx| {
            s.activate_row_then(project, scope, row, None, reveal_result, window, cx);
        });
    }

    fn activate_row_with_action(
        &mut self,
        project: PathBuf,
        scope: Scope,
        row: ThreadRow,
        action: super::workbench::DeferredRowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_project_dir(project, false, window, cx, move |s, project, window, cx| {
            s.activate_row_then(project, scope, row, Some(action), false, window, cx);
        });
    }

    fn clear_row_unread(&mut self, key: &ThreadMetaKey, cx: &mut Context<Self>) {
        if self
            .sidebar
            .thread(key)
            .and_then(|m| m.unread_override)
            .is_some()
        {
            self.sidebar.set_unread_override(key, None);
            self.save_sidebar(cx);
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn activate_row_then(
        &mut self,
        project: PathBuf,
        scope: Scope,
        row: ThreadRow,
        action: Option<super::workbench::DeferredRowAction>,
        reveal_result: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = row.id.clone();
        let mkey = ThreadMetaKey::new(project.clone(), scope.runtime, id.clone());
        self.clear_row_unread(&mkey, cx);
        let (route, _) = self.project_row_state(&project, &row, scope, cx);
        if let Some(route) = route {
            self.open_catalog(project, row, route, window, cx);
            if reveal_result {
                self.panes[self.active].read(cx).reveal_latest_result(&id);
            }
            if let Some(action) = action {
                self.panes[self.active]
                    .update(cx, |p, cx| p.queue_row_action(id, action, window, cx));
            }
        } else if (row.needs_claude_alias()
            || (scope.api && self.api_provider.as_deref() == Some(&row.model_provider)))
            && !self.models.read(cx).is_empty()
        {
            self.choose_route = Some((project, row));
            self.reveal_sidebar(window, cx);
            cx.notify();
        } else {
            self.reveal_sidebar(window, cx);
            self.message = Some(format!(
                "Set up {} in Settings to open this thread.",
                row.model_provider
            ));
            cx.notify();
        }
    }

    fn navigate(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers != Modifiers::default() {
            return;
        }
        if !self.fleet_tree.native {
            return;
        }
        let selected = self.panes[self.active]
            .read(cx)
            .store
            .as_ref()
            .and_then(|s| s.selected());
        let current = selected.and_then(|id| {
            self.catalog.iter().position(|entry| {
                self.located_entry(entry, cx)
                    .is_some_and(|(r, project, scope)| {
                        r.id == id
                            && project == self.panes[self.active].read(cx).project
                            && scope.matches(&self.panes[self.active].read(cx).route)
                    })
            })
        });
        if let Some(pos) = current
            && let Some((row, project, scope)) = self
                .catalog
                .get(pos)
                .and_then(|e| self.located_entry(e, cx))
        {
            let row = row.clone();
            let project = project.to_path_buf();
            let key = ThreadMetaKey::new(project.clone(), scope.runtime, row.id.clone());
            match event.keystroke.key.as_str() {
                "p" => {
                    let now = chrono::Local::now().timestamp();
                    self.sidebar.toggle_pin(&key, &row, scope.api, now);
                    self.save_sidebar(cx);
                    cx.stop_propagation();
                    return;
                }
                "u" => {
                    let (_, status) = self.project_row_state(&project, &row, scope, cx);
                    let currently_unread = self
                        .sidebar
                        .effective_unread_override(&key, row.updated_at)
                        .unwrap_or(row.unread || status == Some("Unread"));
                    self.sidebar.set_unread_override_at(
                        &key,
                        Some(!currently_unread),
                        row.updated_at,
                    );
                    self.save_sidebar(cx);
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    let current_color = self.sidebar.thread(&key).and_then(|m| m.color);
                    self.sidebar
                        .set_thread_color(&key, AccentColor::next(current_color));
                    self.save_sidebar(cx);
                    cx.stop_propagation();
                    return;
                }
                "r" | "f2" => {
                    self.begin_row_rename(key, scope, row, window, cx);
                    cx.stop_propagation();
                    return;
                }
                "/" => {
                    let focus = self.filter.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        let rows = self
            .catalog
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((i, self.located_entry(e, cx)?)));
        let target = match event.keystroke.key.as_str() {
            "home" => rows.into_iter().next(),
            "end" => rows.into_iter().next_back(),
            "up" | "k" => rows.rev().find(|(i, _)| current.is_none_or(|pos| *i < pos)),
            "down" | "j" => rows
                .into_iter()
                .find(|(i, _)| current.is_none_or(|pos| *i > pos)),
            _ => return,
        };
        if let Some((index, (row, project, scope))) = target {
            let reveal_result = matches!(
                self.catalog[index],
                CatalogEntry::Thread {
                    result: Some(_),
                    ..
                }
            );
            self.activate_sidebar_row(
                project.to_owned(),
                scope,
                row.clone(),
                reveal_result,
                window,
                cx,
            );
            self.list_scroll
                .scroll_to_item(self.fleet_tree.rows.len() + index, ScrollStrategy::Nearest);
        } else if matches!(event.keystroke.key.as_str(), "up" | "k")
            && let Some(last) = self.fleet_tree.rows.len().checked_sub(1)
            && let (Some(keys), Some(ws)) = (&self.fleet_tree.keyboard, &self.workspace)
        {
            keys.select(last, ws, window, cx);
        }
        cx.stop_propagation();
    }

    #[allow(clippy::too_many_arguments)]
    fn render_section_header(
        p: Palette,
        width: Pixels,
        id: SharedString,
        title: SharedString,
        count: usize,
        folded: bool,
        running: usize,
        waiting: usize,
        accent: Option<AccentColor>,
        is_pinned: bool,
        menu_builder: Option<crate::kit::RowMenuBuilder>,
        toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        use gpui_component::menu::ContextMenuExt;
        let label = format!(
            "{title}, {count} {}",
            if count == 1 { "thread" } else { "threads" }
        );
        let dot_color = accent.map(|c| p.thread_accent(c));
        let btn = crate::kit::nav_row(id.clone(), label, 24., true, toggle).child(
            row()
                .w_full()
                .gap(px(SPACE_S))
                .when(is_pinned, |r| {
                    r.child(
                        div()
                            .font_family(crate::appearance::LABEL_FONT)
                            .text_size(p.text(Step::Label))
                            .text_color(rgb(p.accent))
                            .child("★"),
                    )
                })
                .when_some(dot_color, |r, c| {
                    r.child(div().size(px(6.)).rounded_full().bg(rgb(c)))
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(p.text(Step::Meta))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(if is_pinned { p.accent } else { p.quiet() }))
                        .child(title.clone()),
                )
                .child(
                    div()
                        .font_family(crate::appearance::LABEL_FONT)
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.quiet()))
                        .child(count.to_string()),
                )
                .when(waiting > 0, |r| {
                    r.child(
                        row()
                            .gap(px(3.))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(p.warning)))
                            .child(
                                div()
                                    .font_family(crate::appearance::LABEL_FONT)
                                    .text_size(p.text(Step::Label) * 0.9)
                                    .text_color(rgb(p.warning))
                                    .child(waiting.to_string()),
                            ),
                    )
                })
                .when(running > 0, |r| {
                    r.child(
                        row()
                            .gap(px(3.))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(p.success)))
                            .child(
                                div()
                                    .font_family(crate::appearance::LABEL_FONT)
                                    .text_size(p.text(Step::Label) * 0.9)
                                    .text_color(rgb(p.success))
                                    .child(running.to_string()),
                            ),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.quiet()))
                        .child(if folded { "▸" } else { "▾" }),
                ),
        );
        let inner: AnyElement = match menu_builder {
            Some(build) => btn
                .context_menu(move |menu, w, cx| build(menu, w, cx))
                .into_any_element(),
            None => btn.into_any_element(),
        };
        div()
            .w(width)
            .h(px(crate::kit::ROW_HEIGHT))
            .px(px(crate::kit::GUTTER))
            .flex()
            .flex_col()
            .justify_end()
            .child(inner)
            .into_any_element()
    }

    /// Keys of sibling root threads in the same sidebar group (Pinned, user section, or project folder)
    /// as `target`, in their current rendered order.
    fn sibling_group_keys(&self, target: &ThreadMetaKey, cx: &App) -> Vec<ThreadMetaKey> {
        let target_meta = self.sidebar.thread(target);
        let target_pinned = target_meta.and_then(|m| m.pinned_at).is_some();
        let target_section = target_meta.and_then(|m| m.section.clone());
        let mut group = Vec::new();
        for entry in &self.catalog {
            if let CatalogEntry::Thread { depth: 0, .. } = entry
                && let Some((row, project, scope)) = self.located_entry(entry, cx)
            {
                let key = ThreadMetaKey::new(project.to_path_buf(), scope.runtime, row.id.clone());
                let meta = self.sidebar.thread(&key);
                let pinned = meta.and_then(|m| m.pinned_at).is_some();
                let section = meta.and_then(|m| m.section.clone());
                let same_group = if target_pinned {
                    pinned
                } else if target_section.is_some() {
                    !pinned && section == target_section
                } else {
                    !pinned && section.is_none() && project == target.project
                };
                if same_group && !group.contains(&key) {
                    group.push(key);
                }
            }
        }
        group
    }

    fn ordered_project_keys(&self) -> Vec<PathBuf> {
        let mut list = Vec::new();
        for entry in &self.catalog {
            if let CatalogEntry::Project { project, .. } = entry
                && !list.contains(project)
            {
                list.push(project.clone());
            }
        }
        list
    }

    #[allow(clippy::too_many_arguments)]
    fn build_thread_row_menu(
        mut menu: gpui_component::menu::PopupMenu,
        host: WeakEntity<Self>,
        key: ThreadMetaKey,
        scope: Scope,
        row: ThreadRow,
        pinned: bool,
        thread_color: Option<AccentColor>,
        thread_section: Option<String>,
        sections: Vec<(String, String)>,
        unread: bool,
        native_live: Option<&'static str>,
        can_end: bool,
        custom_title: bool,
        window: &mut Window,
        cx: &mut Context<gpui_component::menu::PopupMenu>,
    ) -> gpui_component::menu::PopupMenu {
        let host_pin = host.clone();
        let key_pin = key.clone();
        let row_pin = row.clone();
        let pin_label = if pinned { "Unpin thread" } else { "Pin thread" };
        menu = menu.item(crate::kit::menu_item(pin_label).on_click(move |_, _, cx| {
            let _ = host_pin.update(cx, |s, cx| {
                let now = chrono::Local::now().timestamp();
                s.sidebar.toggle_pin(&key_pin, &row_pin, scope.api, now);
                s.save_sidebar(cx);
            });
        }));
        let host_ren = host.clone();
        let key_ren = key.clone();
        let row_ren = row.clone();
        menu = menu.item(
            crate::kit::menu_item("Rename thread…").on_click(move |_, window, cx| {
                let _ = host_ren.update(cx, |s, cx| {
                    s.begin_row_rename(key_ren.clone(), scope, row_ren.clone(), window, cx);
                });
            }),
        );
        let host_gen = host.clone();
        let key_gen = key.clone();
        let row_gen = row.clone();
        let has_native_name = row.name.as_deref().is_some_and(|n| !n.trim().is_empty());
        let is_worker =
            row.worker || row.parent_thread_id.is_some() || row.agent_nickname.is_some();
        let has_generated = row.generated_title.is_some();
        let gen_disabled = custom_title || has_native_name || is_worker || has_generated;
        let gen_label = if custom_title || has_native_name {
            "Generate title — custom name set"
        } else if is_worker {
            "Generate title — worker thread"
        } else if has_generated {
            "Generate title — already titled"
        } else {
            "Generate title"
        };
        menu = menu.item(
            crate::kit::menu_item(gen_label)
                .disabled(gen_disabled)
                .on_click(move |_, window, cx| {
                    let _ = host_gen.update(cx, |s, cx| {
                        s.activate_row_with_action(
                            key_gen.project.clone(),
                            scope,
                            row_gen.clone(),
                            super::workbench::DeferredRowAction::GenerateTitle,
                            window,
                            cx,
                        );
                    });
                }),
        );
        let host_col = host.clone();
        let key_col = key.clone();
        menu = menu.submenu("Colour", window, cx, move |mut sub, _, _| {
            for choice in AccentColor::ALL {
                let host_c = host_col.clone();
                let key_c = key_col.clone();
                sub = sub.item(
                    crate::kit::menu_item(choice.label())
                        .checked(thread_color == Some(choice))
                        .on_click(move |_, _, cx| {
                            let _ = host_c.update(cx, |s, cx| {
                                s.sidebar.set_thread_color(&key_c, Some(choice));
                                s.save_sidebar(cx);
                            });
                        }),
                );
            }
            let host_none = host_col.clone();
            let key_none = key_col.clone();
            sub.separator().item(
                crate::kit::menu_item("No colour")
                    .checked(thread_color.is_none())
                    .on_click(move |_, _, cx| {
                        let _ = host_none.update(cx, |s, cx| {
                            s.sidebar.set_thread_color(&key_none, None);
                            s.save_sidebar(cx);
                        });
                    }),
            )
        });
        let host_move = host.clone();
        let key_move = key.clone();
        let row_move = row.clone();
        let proj_label = key
            .project
            .file_name()
            .map(|n| format!("Project folder ({})", n.to_string_lossy()))
            .unwrap_or_else(|| "Project folder".into());
        menu = menu.submenu("Move to", window, cx, move |mut sub, _, _| {
            for (sec_id, sec_name) in &sections {
                let host_s = host_move.clone();
                let key_s = key_move.clone();
                let row_s = row_move.clone();
                let target_id = sec_id.clone();
                sub = sub.item(
                    crate::kit::menu_item(sec_name.clone())
                        .checked(thread_section.as_deref() == Some(sec_id.as_str()))
                        .on_click(move |_, _, cx| {
                            let _ = host_s.update(cx, |s, cx| {
                                s.sidebar.grouping = GroupingMode::Folders;
                                s.sidebar.move_to_section(
                                    &key_s,
                                    Some(target_id.clone()),
                                    &row_s,
                                    scope.api,
                                );
                                s.save_sidebar(cx);
                            });
                        }),
                );
            }
            let host_proj = host_move.clone();
            let key_proj = key_move.clone();
            let row_proj = row_move.clone();
            let host_new = host_move.clone();
            let key_new = key_move.clone();
            let row_new = row_move.clone();
            sub.separator()
                .item(
                    crate::kit::menu_item(proj_label.clone())
                        .checked(thread_section.is_none())
                        .on_click(move |_, _, cx| {
                            let _ = host_proj.update(cx, |s, cx| {
                                s.sidebar
                                    .move_to_section(&key_proj, None, &row_proj, scope.api);
                                s.save_sidebar(cx);
                            });
                        }),
                )
                .item(
                    crate::kit::menu_item("New section…").on_click(move |_, window, cx| {
                        let _ = host_new.update(cx, |s, cx| {
                            s.begin_new_section(
                                Some((key_new.clone(), row_new.clone(), scope.api)),
                                window,
                                cx,
                            );
                        });
                    }),
                )
        });
        let host_up = host.clone();
        let key_up = key.clone();
        let host_down = host.clone();
        let key_down = key.clone();
        menu = menu
            .item(
                crate::kit::menu_item("Move up in section").on_click(move |_, _, cx| {
                    let _ = host_up.update(cx, |s, cx| {
                        let group = s.sibling_group_keys(&key_up, cx);
                        s.sidebar.step_thread_order(&group, &key_up, -1);
                        s.save_sidebar(cx);
                    });
                }),
            )
            .item(
                crate::kit::menu_item("Move down in section").on_click(move |_, _, cx| {
                    let _ = host_down.update(cx, |s, cx| {
                        let group = s.sibling_group_keys(&key_down, cx);
                        s.sidebar.step_thread_order(&group, &key_down, 1);
                        s.save_sidebar(cx);
                    });
                }),
            );
        let host_unread = host.clone();
        let key_unread = key.clone();
        let row_updated_at = row.updated_at;
        let unread_label = if unread { "Mark read" } else { "Mark unread" };
        menu = menu.item(
            crate::kit::menu_item(unread_label).on_click(move |_, _, cx| {
                let _ = host_unread.update(cx, |s, cx| {
                    s.sidebar
                        .set_unread_override_at(&key_unread, Some(!unread), row_updated_at);
                    s.save_sidebar(cx);
                });
            }),
        );
        let host_find = host.clone();
        let key_find = key.clone();
        let row_find = row.clone();
        menu = menu.item(crate::kit::menu_item("Find in transcript").on_click(
            move |_, window, cx| {
                let _ = host_find.update(cx, |s, cx| {
                    s.activate_row_with_action(
                        key_find.project.clone(),
                        scope,
                        row_find.clone(),
                        super::workbench::DeferredRowAction::FindInTranscript,
                        window,
                        cx,
                    );
                });
            },
        ));
        let copy_id = key.id.clone();
        let copy_path = key.project.display().to_string();
        let copy_title = row.title().to_owned();
        menu = menu.submenu("Copy", window, cx, move |sub, _, _| {
            let id_str = copy_id.clone();
            let path_str = copy_path.clone();
            let title_str = copy_title.clone();
            sub.item(
                crate::kit::menu_item("Thread ID").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(id_str.clone()));
                }),
            )
            .item(
                crate::kit::menu_item("Project path").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path_str.clone()));
                }),
            )
            .item(
                crate::kit::menu_item("Thread title").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(title_str.clone()));
                }),
            )
        });
        let archive_disabled =
            native_live.is_some() || scope.runtime != Runtime::Codex || row.worker;
        let archive_label = if let Some(app) = native_live {
            format!("Archive — {app}")
        } else if scope.runtime != Runtime::Codex {
            format!("Archive — unavailable in {}", scope.runtime.name())
        } else if row.worker {
            "Archive — worker thread".to_owned()
        } else {
            "Archive thread…".to_owned()
        };
        let host_arch = host.clone();
        let key_arch = key.clone();
        let row_arch = row.clone();
        menu = menu.separator().item(
            crate::kit::menu_item(archive_label)
                .disabled(archive_disabled)
                .on_click(move |_, window, cx| {
                    let _ = host_arch.update(cx, |s, cx| {
                        s.activate_row_with_action(
                            key_arch.project.clone(),
                            scope,
                            row_arch.clone(),
                            super::workbench::DeferredRowAction::Archive,
                            window,
                            cx,
                        );
                    });
                }),
        );
        if can_end {
            let host_end = host.clone();
            let key_end = key.clone();
            let row_end = row.clone();
            menu = menu.item(crate::kit::menu_item("End running session").on_click(
                move |_, window, cx| {
                    let _ = host_end.update(cx, |s, cx| {
                        s.activate_row_with_action(
                            key_end.project.clone(),
                            scope,
                            row_end.clone(),
                            super::workbench::DeferredRowAction::End,
                            window,
                            cx,
                        );
                    });
                },
            ));
        }
        let host_hide = host;
        let key_hide = key;
        menu.item(
            crate::kit::menu_item("Remove from list").on_click(move |_, _, cx| {
                let _ = host_hide.update(cx, |s, cx| {
                    s.sidebar.set_hidden(&key_hide, true);
                    s.save_sidebar(cx);
                });
            }),
        )
    }

    fn navigation_rows(
        &mut self,
        range: Range<usize>,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let offset = self.fleet_tree.rows.len();
        let mut rows = Vec::with_capacity(range.len());
        if let Some(ws) = self.workspace.as_ref().and_then(WeakEntity::upgrade)
            && let Some(keys) = &self.fleet_tree.keyboard
        {
            let p = Palette::get(cx);
            let cursor = ws.read(cx).tree_cursor.as_deref();
            for (i, row) in self
                .fleet_tree
                .rows
                .iter()
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                rows.push(row.render(&ws.downgrade(), width, p, keys, i, cursor));
            }
        }
        if self.fleet_tree.native && range.end > offset {
            rows.extend(self.rows(
                range.start.saturating_sub(offset)..range.end - offset,
                width,
                cx,
            ));
        }
        rows
    }

    fn rows(
        &mut self,
        range: Range<usize>,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let p = Palette::get(cx);
        let active = self.panes[self.active].read(cx);
        let active_project = active.project.clone();
        let active_route = active.route.clone();
        let execution_host = active
            .host_caption
            .as_deref()
            .or_else(|| self.link.remote().map(|host| host.name.as_str()))
            .unwrap_or("This computer")
            .to_owned();
        let active_selected = active
            .store
            .as_ref()
            .and_then(|s| s.selected())
            .map(str::to_owned);
        let mut measured = 0;
        let mut rows = Vec::with_capacity(range.len());
        let now = chrono::Local::now();
        let entries: Vec<CatalogEntry> =
            self.catalog.get(range.clone()).unwrap_or_default().to_vec();
        for entry in entries {
            let (depth, timestamp, result, in_finished_shelf) = match &entry {
                CatalogEntry::Thread {
                    ordinal,
                    depth,
                    timestamp,
                    result,
                    in_finished_shelf,
                    ..
                } => {
                    if self.stats {
                        measured += 1;
                        self.furthest_row = self.furthest_row.max(*ordinal + 1);
                    }
                    (*depth, *timestamp, *result, *in_finished_shelf)
                }
                CatalogEntry::JustFinished | CatalogEntry::NoRunning => {
                    let (id, label) = if matches!(entry, CatalogEntry::JustFinished) {
                        ("just-finished", "Just finished")
                    } else {
                        ("no-running-sessions", "No running sessions")
                    };
                    rows.push(
                        crate::platform::layout_probe::mark_element(
                            row()
                                .w(width)
                                .h(px(crate::kit::ROW_HEIGHT))
                                .px(px(crate::kit::GUTTER))
                                .text_size(p.text(Step::Meta))
                                .text_color(rgb(p.quiet()))
                                .child(label),
                            &id.into(),
                            &label.into(),
                            true,
                        )
                        .into_any_element(),
                    );
                    continue;
                }
                CatalogEntry::ViewFinished => {
                    rows.push(
                        row()
                            .w(width)
                            .h(px(crate::kit::ROW_HEIGHT))
                            .px(px(crate::kit::GUTTER))
                            .child(crate::kit::toolbar_button(
                                p,
                                "view-finished",
                                "View finished",
                                true,
                                cx.listener(|s, _, window, cx| {
                                    let value = s.filter.read(cx).value().to_string();
                                    let next = select_status_filter(&value, "is:finished");
                                    s.filter
                                        .update(cx, |field, cx| field.set_value(next, window, cx));
                                    s.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                                }),
                            ))
                            .into_any_element(),
                    );
                    continue;
                }
                CatalogEntry::Pinned {
                    count,
                    folded,
                    running,
                    waiting,
                } => {
                    rows.push(Self::render_section_header(
                        p,
                        width,
                        "section-pinned".into(),
                        "Pinned".into(),
                        *count,
                        *folded,
                        *running,
                        *waiting,
                        None,
                        true,
                        None,
                        cx.listener(|s, _, _, cx| {
                            s.sidebar.pinned_collapsed = !s.sidebar.pinned_collapsed;
                            s.save_sidebar(cx);
                        }),
                    ));
                    continue;
                }
                CatalogEntry::Section {
                    id,
                    name,
                    color,
                    count,
                    folded,
                    running,
                    waiting,
                } => {
                    let sec_id = id.clone();
                    let sec_name = name.clone();
                    let sec_color = *color;
                    let host = cx.weak_entity();
                    let menu_sec_id = sec_id.clone();
                    let menu_sec_name = sec_name.clone();
                    let menu: crate::kit::RowMenuBuilder =
                        std::rc::Rc::new(move |mut menu, window, cx| {
                            let host_rename = host.clone();
                            let id_rename = menu_sec_id.clone();
                            let name_rename = menu_sec_name.clone();
                            menu = menu.item(crate::kit::menu_item("Rename section…").on_click(
                                move |_, window, cx| {
                                    let _ = host_rename.update(cx, |s, cx| {
                                        s.begin_rename_section(
                                            id_rename.clone(),
                                            &name_rename,
                                            window,
                                            cx,
                                        );
                                    });
                                },
                            ));
                            let host_color = host.clone();
                            let id_color = menu_sec_id.clone();
                            menu =
                                menu.submenu("Section colour", window, cx, move |mut sub, _, _| {
                                    for choice in AccentColor::ALL {
                                        let host_c = host_color.clone();
                                        let id_c = id_color.clone();
                                        sub = sub.item(
                                            crate::kit::menu_item(choice.label())
                                                .checked(sec_color == Some(choice))
                                                .on_click(move |_, _, cx| {
                                                    let _ = host_c.update(cx, |s, cx| {
                                                        s.sidebar
                                                            .set_section_color(&id_c, Some(choice));
                                                        s.save_sidebar(cx);
                                                    });
                                                }),
                                        );
                                    }
                                    let host_none = host_color.clone();
                                    let id_none = id_color.clone();
                                    sub.separator().item(
                                        crate::kit::menu_item("No colour")
                                            .checked(sec_color.is_none())
                                            .on_click(move |_, _, cx| {
                                                let _ = host_none.update(cx, |s, cx| {
                                                    s.sidebar.set_section_color(&id_none, None);
                                                    s.save_sidebar(cx);
                                                });
                                            }),
                                    )
                                });
                            let host_up = host.clone();
                            let id_up = menu_sec_id.clone();
                            let host_down = host.clone();
                            let id_down = menu_sec_id.clone();
                            let host_del = host.clone();
                            let id_del = menu_sec_id.clone();
                            menu.separator()
                                .item(crate::kit::menu_item("Move section up").on_click(
                                    move |_, _, cx| {
                                        let _ = host_up.update(cx, |s, cx| {
                                            s.sidebar.move_section(&id_up, -1);
                                            s.save_sidebar(cx);
                                        });
                                    },
                                ))
                                .item(crate::kit::menu_item("Move section down").on_click(
                                    move |_, _, cx| {
                                        let _ = host_down.update(cx, |s, cx| {
                                            s.sidebar.move_section(&id_down, 1);
                                            s.save_sidebar(cx);
                                        });
                                    },
                                ))
                                .separator()
                                .item(crate::kit::menu_item("Delete section").on_click(
                                    move |_, _, cx| {
                                        let _ = host_del.update(cx, |s, cx| {
                                            s.sidebar.delete_section(&id_del);
                                            s.save_sidebar(cx);
                                        });
                                    },
                                ))
                        });
                    rows.push(Self::render_section_header(
                        p,
                        width,
                        SharedString::from(format!("section-{sec_id}")),
                        SharedString::from(sec_name),
                        *count,
                        *folded,
                        *running,
                        *waiting,
                        sec_color,
                        false,
                        Some(menu),
                        cx.listener(move |s, _, _, cx| {
                            s.sidebar.toggle_section_collapsed(&sec_id);
                            s.save_sidebar(cx);
                        }),
                    ));
                    continue;
                }
                CatalogEntry::Project {
                    project,
                    starred,
                    color,
                    count,
                    folded,
                    running,
                    waiting,
                } => {
                    let proj_path = project.clone();
                    let proj_starred = *starred;
                    let proj_color = *color;
                    let proj_name = proj_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| proj_path.display().to_string());
                    let proj_name = self
                        .workspace
                        .as_ref()
                        .and_then(WeakEntity::upgrade)
                        .and_then(|ws| {
                            ws.read(cx).shared_caption(
                                self.link.remote().map(|h| h.id.as_str()),
                                &proj_path,
                                cx,
                            )
                        })
                        .map_or_else(
                            || proj_name.clone(),
                            |hosts| format!("{proj_name} · {hosts}"),
                        );
                    let host = cx.weak_entity();
                    let menu_proj = proj_path.clone();
                    let menu: crate::kit::RowMenuBuilder =
                        std::rc::Rc::new(move |mut menu, window, cx| {
                            let host_new = host.clone();
                            let p_new = menu_proj.clone();
                            menu = menu.item(
                                crate::kit::menu_item("New session in project").on_click(
                                    move |_, window, cx| {
                                        let _ = host_new.update(cx, |s, cx| {
                                            let route = s.recent.route(&p_new);
                                            s.request_project_draft(
                                                p_new.clone(),
                                                route,
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            );
                            let host_star = host.clone();
                            let p_star = menu_proj.clone();
                            let star_label = if proj_starred {
                                "Unstar folder"
                            } else {
                                "Star folder"
                            };
                            menu = menu.item(crate::kit::menu_item(star_label).on_click(
                                move |_, _, cx| {
                                    let _ = host_star.update(cx, |s, cx| {
                                        let now = chrono::Local::now().timestamp();
                                        s.sidebar.toggle_project_star(&p_star, now);
                                        s.save_sidebar(cx);
                                    });
                                },
                            ));
                            let host_color = host.clone();
                            let p_color = menu_proj.clone();
                            menu =
                                menu.submenu("Folder colour", window, cx, move |mut sub, _, _| {
                                    for choice in AccentColor::ALL {
                                        let host_c = host_color.clone();
                                        let p_c = p_color.clone();
                                        sub = sub.item(
                                            crate::kit::menu_item(choice.label())
                                                .checked(proj_color == Some(choice))
                                                .on_click(move |_, _, cx| {
                                                    let _ = host_c.update(cx, |s, cx| {
                                                        s.sidebar
                                                            .set_project_color(&p_c, Some(choice));
                                                        s.save_sidebar(cx);
                                                    });
                                                }),
                                        );
                                    }
                                    let host_none = host_color.clone();
                                    let p_none = p_color.clone();
                                    sub.separator().item(
                                        crate::kit::menu_item("No colour")
                                            .checked(proj_color.is_none())
                                            .on_click(move |_, _, cx| {
                                                let _ = host_none.update(cx, |s, cx| {
                                                    s.sidebar.set_project_color(&p_none, None);
                                                    s.save_sidebar(cx);
                                                });
                                            }),
                                    )
                                });
                            let host_up = host.clone();
                            let p_up = menu_proj.clone();
                            let host_down = host.clone();
                            let p_down = menu_proj.clone();
                            menu = menu
                                .separator()
                                .item(crate::kit::menu_item("Move folder up").on_click(
                                    move |_, _, cx| {
                                        let _ = host_up.update(cx, |s, cx| {
                                            let ordered = s.ordered_project_keys();
                                            s.sidebar.step_project_order(&ordered, &p_up, -1);
                                            s.save_sidebar(cx);
                                        });
                                    },
                                ))
                                .item(crate::kit::menu_item("Move folder down").on_click(
                                    move |_, _, cx| {
                                        let _ = host_down.update(cx, |s, cx| {
                                            let ordered = s.ordered_project_keys();
                                            s.sidebar.step_project_order(&ordered, &p_down, 1);
                                            s.save_sidebar(cx);
                                        });
                                    },
                                ));
                            let p_copy = menu_proj.display().to_string();
                            menu.separator().item(
                                crate::kit::menu_item("Copy project path").on_click(
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            p_copy.clone(),
                                        ));
                                    },
                                ),
                            )
                        });
                    rows.push(Self::render_section_header(
                        p,
                        width,
                        SharedString::from(format!("project-folder-{}", proj_path.display())),
                        SharedString::from(proj_name),
                        *count,
                        *folded,
                        *running,
                        *waiting,
                        proj_color,
                        proj_starred,
                        Some(menu),
                        cx.listener(move |s, _, _, cx| {
                            s.sidebar.toggle_project_collapsed(&proj_path);
                            s.save_sidebar(cx);
                        }),
                    ));
                    continue;
                }
                CatalogEntry::Day { day, count, folded } => {
                    let day_val = *day;
                    let title = day_label(day_val, now.date_naive());
                    rows.push(Self::render_section_header(
                        p,
                        width,
                        SharedString::from(format!("day-{title}")),
                        SharedString::from(title),
                        *count,
                        *folded,
                        0,
                        0,
                        None,
                        false,
                        None,
                        cx.listener(move |s, _, _, cx| {
                            s.discovery.toggle_day(day_val);
                            cx.notify();
                        }),
                    ));
                    continue;
                }
            };
            let Some((t, project, scope)) = self.located_entry(&entry, cx) else {
                continue;
            };
            let (route, status) = self.project_row_state(project, t, scope, cx);
            let confirmed_model = match entry {
                CatalogEntry::Thread { loc, .. } => self.actual_model_at(loc, cx),
                _ => None,
            };
            let api = scope.api;
            let badge = {
                route.as_ref().map_or_else(
                    || match (&t.model, api) {
                        (None, true) => "OX API · saved model unknown".into(),
                        (Some(model), _) => format!("{} · {model} · unavailable", t.model_provider),
                        _ => format!("{} · unavailable", t.model_provider),
                    },
                    |route| {
                        self.panes
                            .iter()
                            .map(|pane| pane.read(cx))
                            .filter(|view| &view.route == route && view.project == project)
                            .filter_map(|view| view.store.as_ref())
                            .find(|store| store.transcript(&t.id).is_some_and(|t| t.is_ready()))
                            .map_or_else(
                                || route.label(),
                                |store| store.thread_route(Some(&t.id)).label(),
                            )
                    },
                )
            };
            let worker_notice = route
                .as_ref()
                .and_then(|r| self.discovery.worker_notice(project, r, &t.id));
            let badge = match confirmed_model {
                Some(model) => format!("{badge} · {model}"),
                None => badge,
            };
            let tree = self
                .workers
                .read(cx)
                .trees
                .get(&(project.to_path_buf(), t.id.clone()))
                .cloned();
            // A worker shown on its own keeps a readable parent reference.
            let parent_hint = self
                .workers
                .read(cx)
                .parents
                .get(&(project.to_path_buf(), t.id.clone()))
                .map(|link| {
                    link.title.as_ref().map_or_else(
                        || {
                            let parent_project = link
                                .project
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy();
                            format!("Worker · Lead project {parent_project}")
                        },
                        |title| format!("Worker for {title}"),
                    )
                });
            let row = t.clone();
            let project = project.to_owned();
            let meta_key = ThreadMetaKey::new(project.clone(), scope.runtime, t.id.clone());
            if let Some(SidebarInlineEdit::RenameThread {
                key: edit_key,
                input,
                ..
            }) = &self.sidebar_edit
                && *edit_key == meta_key
            {
                let input = input.clone();
                rows.push(
                    gpui::div()
                        .w(width)
                        .h(px(crate::kit::ROW_HEIGHT))
                        .px(px(crate::kit::GUTTER))
                        .flex()
                        .items_center()
                        .gap(px(SPACE_XS))
                        .child(div().flex_1().min_w_0().child(crate::kit::input(&input)))
                        .child(crate::kit::icon_button(
                            p,
                            "save-row-rename",
                            "✓",
                            "Save thread name",
                            cx.listener(|s, _, w, cx| s.commit_sidebar_edit(w, cx)),
                        ))
                        .child(crate::kit::icon_button(
                            p,
                            "cancel-row-rename",
                            "×",
                            "Cancel rename",
                            cx.listener(|s, _, _, cx| {
                                s.sidebar_edit = None;
                                cx.notify();
                            }),
                        ))
                        .into_any_element(),
                );
                continue;
            }
            let meta = self.sidebar.thread(&meta_key);
            let pinned = meta.and_then(|m| m.pinned_at).is_some();
            let thread_color = meta.and_then(|m| m.color);
            let effective_accent = self.sidebar.effective_color(&meta_key);
            let thread_section = meta.and_then(|m| m.section.clone());
            let custom_title = meta.and_then(|m| m.custom_title.clone());
            let has_custom_title = custom_title.is_some();
            let display_title = custom_title.unwrap_or_else(|| t.title().to_owned());
            let unread_override = self
                .sidebar
                .effective_unread_override(&meta_key, t.updated_at);
            let unread = unread_override.unwrap_or(t.unread || status == Some("Unread"));
            let status = if status == Some("Unread") && unread_override == Some(false) {
                None
            } else {
                status
            };
            let fold_key = (project.clone(), t.id.clone());
            let folders = self.sidebar.grouping == GroupingMode::Folders && !in_finished_shelf;
            let folded = folders
                && tree.as_ref().is_some_and(|tree| tree.children > 0)
                && self.discovery.collapsed.contains(&fold_key);
            // A folded parent speaks for the children that need attention.
            let status = match tree.as_ref() {
                Some(tree) if folded && tree.waiting > 0 => Some("Needs you"),
                Some(tree) if folded && tree.running > 0 => status.or(Some("Running")),
                _ => status,
            };
            let cartridge = if depth > 0 || t.worker || t.parent_thread_id.is_some() {
                activity::Cartridge::Worker
            } else if api {
                activity::Cartridge::Api
            } else {
                match scope.runtime {
                    Runtime::Codex => activity::Cartridge::Codex,
                    Runtime::Claude => activity::Cartridge::Claude,
                    Runtime::Antigravity => activity::Cartridge::Antigravity,
                }
            };
            let project_name = project
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let show_project_tag = pinned || thread_section.is_some() || !folders;
            let model = crate::kit::ListRow {
                cartridge,
                title: display_title,
                route: {
                    let mut route = if depth == 0
                        && let Some(hint) = parent_hint
                    {
                        format!("{hint} · {project_name} · {badge}")
                    } else if !t.worker && t.parent_thread_id.is_some() {
                        format!("Subagent · {badge}")
                    } else {
                        format!("{project_name} · {badge}")
                    };
                    if let Some(tree) = tree.as_ref().filter(|tree| tree.children > 0) {
                        let noun = if tree.children == 1 {
                            "worker"
                        } else {
                            "workers"
                        };
                        route.push_str(&format!(" · {} {noun}", tree.children));
                        if tree.running > 0 {
                            route.push_str(&format!(", {} running", tree.running));
                        }
                    }
                    if let Some(update) = worker_notice {
                        route = format!("{update} · {route}");
                    }
                    if t.worker {
                        route = format!("Worker on {execution_host} · {route}");
                    }
                    route
                },
                age: age(now.timestamp(), timestamp),
                status: status.or(unread.then_some("Unread")),
                result: result.map(|outcome| {
                    if in_finished_shelf {
                        match outcome {
                            SessionOutcome::Completed => "Completed",
                            SessionOutcome::Failed => "Failed",
                            SessionOutcome::Stopped => "Stopped",
                        }
                    } else {
                        "New result"
                    }
                }),
                selected: active_project == project
                    && scope.matches(&active_route)
                    && active_selected.as_deref() == Some(t.id.as_str()),
                depth,
                pinned,
                accent: effective_accent,
                project_tag: if t.worker {
                    Some(execution_host.clone())
                } else {
                    (show_project_tag && depth == 0 && !project_name.is_empty())
                        .then_some(project_name)
                },
            };
            // The disclosure is a sibling of the row in the gutter, so it folds without
            // selecting and the row keeps the sidebar keyline.
            let disclosure = tree
                .as_ref()
                .filter(|tree| folders && tree.children > 0)
                .map(|tree| {
                    let glyph = if folded { "▸" } else { "▾" };
                    let label = format!(
                        "{} {} worker{}",
                        if folded { "Show" } else { "Hide" },
                        tree.children,
                        if tree.children == 1 { "" } else { "s" }
                    );
                    gpui::div()
                        .id(SharedString::from(format!(
                            "fold-{}-{}-{}",
                            fold_key.0.display(),
                            scope.api,
                            fold_key.1
                        )))
                        .absolute()
                        .left_0()
                        .top_0()
                        .w(px(crate::kit::GUTTER))
                        .h(px(crate::kit::ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_size(p.text(Step::Label))
                        .text_color(rgb(p.quiet()))
                        .hover(move |d| d.text_color(rgb(p.fg)))
                        .child(glyph)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
                        })
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.discovery.toggle(fold_key.0.clone(), fold_key.1.clone());
                            cx.notify();
                        }))
                });
            let native_live = match status {
                Some("Live in Codex") => Some("Live in Codex"),
                Some("Live in Claude Code") => Some("Live in Claude Code"),
                _ => None,
            };
            let can_end = matches!(status, Some("Running" | "Needs you"));
            let sections: Vec<(String, String)> = self
                .sidebar
                .sections
                .iter()
                .map(|s| (s.id.clone(), s.name.clone()))
                .collect();
            let host = cx.weak_entity();
            let row_for_menu = row.clone();
            let menu_builder: crate::kit::RowMenuBuilder =
                std::rc::Rc::new(move |menu, window, cx| {
                    Self::build_thread_row_menu(
                        menu,
                        host.clone(),
                        meta_key.clone(),
                        scope,
                        row_for_menu.clone(),
                        pinned,
                        thread_color,
                        thread_section.clone(),
                        sections.clone(),
                        unread,
                        native_live,
                        can_end,
                        has_custom_title,
                        window,
                        cx,
                    )
                });
            rows.push(
                gpui::div()
                    .w(width)
                    .relative()
                    .px(px(crate::kit::GUTTER))
                    .child(model.render_with_menu(
                        p,
                        SharedString::from(format!(
                            "thread-{}-{}-{}",
                            project.display(),
                            scope.api,
                            t.id
                        )),
                        Some(menu_builder),
                        cx.listener(move |s, _, window, cx| {
                            window.focus(&s.list_focus, cx);
                            s.activate_sidebar_row(
                                project.clone(),
                                scope,
                                row.clone(),
                                result.is_some(),
                                window,
                                cx,
                            );
                        }),
                    ))
                    .children(disclosure)
                    .into_any_element(),
            );
        }
        if self.stats {
            self.list_rows += measured as u64;
            self.max_range = self.max_range.max(measured);
        }
        if range.end > 0 && range.end + 3 >= self.catalog.len() {
            for &(i, _) in &self.catalog_versions {
                let pane = self.panes[i].clone();
                if pane.read(cx).store.as_ref().is_some_and(|s| s.has_more()) {
                    cx.defer(move |cx| {
                        pane.update(cx, |p, cx| {
                            if p.store.as_ref().is_some_and(|s| s.has_more()) {
                                p.action(cx, DeskAction::MoreThreads);
                            }
                        })
                    });
                }
            }
        }
        rows
    }
}
impl Render for Host {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| self.draw(window, cx).into_any_element())
    }
}

impl Host {
    fn draw(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.remember_layout(window, cx);
        let p = Palette::get(cx);
        if self.stats && self._probe.is_none() {
            eprintln!("desk-stats {}", self.snapshot(window, cx));
        }
        self.index_catalog(cx);
        if let Some(ws) = self.workspace.as_ref().and_then(WeakEntity::upgrade) {
            let ws = ws.read(cx);
            let filter = self.filter.read(cx).value();
            if self.fleet_tree.palette != Some(p)
                || self.fleet_tree.revision != ws.navigation_revision()
                || self.fleet_tree.filter != filter.as_str()
            {
                let active = self.link.remote().map(|h| h.id.clone());
                let pane = self.panes[self.active].read(cx);
                let native_projects = self
                    .catalog
                    .iter()
                    .filter_map(|entry| match entry {
                        CatalogEntry::Project { project, .. } => Some(project.as_path()),
                        _ => self.located_entry(entry, cx).map(|(_, project, _)| project),
                    })
                    .collect();
                let current = crate::app::fleet::tree::Current {
                    native_projects,
                    sidebar: &self.sidebar,
                    project: &pane.project,
                    route: &pane.route,
                    thread: pane.store.as_ref().and_then(|s| s.selected()),
                };
                let (rows, native) = ws.tree_rows(&active, &filter, Some(current), cx);
                let keyboard = crate::app::fleet::tree::Keyboard::new(
                    &rows,
                    self.list_scroll.clone(),
                    ws.tree_focus.clone(),
                    (native && !self.catalog.is_empty()).then(|| self.list_focus.clone()),
                );
                if let Some(owner) = self.workspace.clone() {
                    keyboard.recover(
                        ws.tree_cursor.as_deref(),
                        owner,
                        Some(self.list_focus.clone()),
                        window,
                    );
                }
                self.fleet_tree = crate::app::fleet::tree::Tree {
                    keyboard: Some(keyboard),
                    palette: Some(p),
                    revision: ws.navigation_revision(),
                    filter: filter.to_string(),
                    rows,
                    native,
                };
            }
        }
        let compact = self.compact(window);
        if !compact {
            self.close_drawer(window, cx);
        }
        let inline = !compact && self.sidebar_visible;
        let shown = inline || self.drawer_open;
        self.running
            .update(cx, |r, cx| r.set_shown(shown && window.is_visible(), cx));
        let pane_width = window.viewport_size().width
            - if inline {
                px(self.sidebar_width as f32 + DIVIDER_WIDTH)
            } else {
                px(0.)
            };
        let host_caption = self
            .workspace
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|ws| {
                ws.read(cx)
                    .session_host_name(&self.link.remote().map(|h| h.id.clone()), cx)
            });
        if let Some(workspace) = &self.workspace {
            for pane in &self.panes {
                if pane.read(cx).workspace.is_none() {
                    pane.update(cx, |p, _| p.workspace = Some(workspace.clone()));
                }
            }
        }
        let pane = self.panes[self.active].read(cx);
        let worker_update = pane
            .store
            .as_ref()
            .and_then(|store| {
                self.discovery
                    .worker_notice(&pane.project, &pane.route, store.selected()?)
            })
            .map(str::to_owned);
        if pane.pane_width != pane_width
            || pane.host_caption != host_caption
            || pane.worker_update != worker_update
        {
            self.panes[self.active].update(cx, |pane, cx| {
                pane.pane_width = pane_width;
                pane.host_caption = host_caption;
                pane.worker_update = worker_update;
                cx.notify();
            });
        }
        let pane = self.panes[self.active].read(cx);
        let project = pane.project.clone();
        let saved_route = pane.route.clone();
        let current = saved_route.label();
        let hot = pane
            .store
            .as_ref()
            .and_then(|s| s.transcript(s.selected()?))
            .and_then(|t| t.access())
            .is_some_and(ox_desk_core::runtime::Access::bypasses_permissions);
        let pane_ready = pane
            .store
            .as_ref()
            .is_some_and(|s| s.state() == &Connection::Ready);
        let count = self.fleet_tree.rows.len()
            + if self.fleet_tree.native {
                self.catalog.len()
            } else {
                0
            };
        let (running_count, running_open) = {
            let running = self.running.read(cx);
            (running.process_count(), running.open)
        };
        // The AGENT picker shows and offers what the composer's model pill does.
        let value = pane.store.as_ref().map_or_else(
            || current.clone(),
            |s| super::route_chip::route_value(s, s.selected()),
        );
        let host = cx.weak_entity();
        let route = picker_row(p, "billing-route", "AGENT", value.clone())
            .tooltip(format!("Agent and model · {value}"))
            .on_click(move |_, window, cx| {
                let _ = host.update(cx, |s, cx| s.open_agent_picker(window, cx));
            });
        let label = project
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let host = cx.weak_entity();
        let projects: Vec<_> = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let pane = pane.read(cx);
                (
                    i,
                    pane.project.display().to_string(),
                    pane.route.label(),
                    self.pane_waiting_count(pane),
                )
            })
            .collect();
        let recent: Vec<_> = self
            .recent
            .recent
            .iter()
            .filter(|r| {
                !self
                    .panes
                    .iter()
                    .any(|pane| pane.read(cx).project == r.project)
            })
            .map(|r| (r.project.clone(), self.recent.route(&r.project)))
            .collect();
        let picker = picker_row(p, "project-picker", "PROJECT", label.clone())
            .tooltip(project.display().to_string())
            .dropdown_menu(move |mut menu, _, _| {
                for (index, path, route, asks) in &projects {
                    let index = *index;
                    let host = host.clone();
                    let label = if *asks > 0 {
                        format!("{path} · {route} · {asks} waiting")
                    } else {
                        format!("{path} · {route}")
                    };
                    menu =
                        menu.item(crate::kit::menu_item(label).on_click(move |_, window, cx| {
                            let _ = host.update(cx, |s, cx| s.switch(index, window, cx));
                        }));
                }
                for (directory, route) in &recent {
                    let host = host.clone();
                    let directory = directory.clone();
                    let route = route.clone();
                    menu = menu.item(
                        crate::kit::menu_item(directory.display().to_string()).on_click(
                            move |_, window, cx| {
                                let _ = host.update(cx, |s, cx| {
                                    let route = route.clone();
                                    s.with_project_dir(
                                        directory.clone(),
                                        true,
                                        window,
                                        cx,
                                        move |s, project, window, cx| {
                                            s.open(project, route, window, cx)
                                        },
                                    );
                                });
                            },
                        ),
                    );
                }
                let host = host.clone();
                menu.item(
                    crate::kit::menu_item("Open project…").on_click(move |_, window, cx| {
                        let _ = host.update(cx, |s, cx| s.pick_project(window, cx));
                    }),
                )
            });
        let mut background = Vec::new();
        let mut background_waiting = 0;
        for (index, other) in self
            .panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.active)
        {
            let other = other.read(cx);
            let waiting = self.pane_waiting_count(other);
            if waiting > 0 {
                background_waiting += waiting;
                background.push(
                    crate::kit::toolbar_button(
                        p,
                        ("waiting", index),
                        format!(
                            "{waiting} waiting · {}",
                            other
                                .project
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        ),
                        true,
                        cx.listener(move |s, _, window, cx| s.switch(index, window, cx)),
                    )
                    .w_full()
                    .justify_start()
                    .text_color(rgb(p.warning)),
                );
            }
        }
        let threads_label = if background_waiting > 0 {
            format!("Threads · {background_waiting} waiting")
        } else {
            "Threads".into()
        };
        let sidebar_width =
            px(self.sidebar_width as f32).min(window.viewport_size().width - px(36.));
        let gutter = px(crate::kit::GUTTER);
        let new_session_key = if cfg!(target_os = "macos") {
            "⌘N"
        } else {
            "Ctrl N"
        };
        let section_editor = match &self.sidebar_edit {
            Some(SidebarInlineEdit::NewSection { input, .. })
            | Some(SidebarInlineEdit::RenameSection { input, .. }) => Some(input.clone()),
            _ => None,
        };
        let sidebar = col()
            .w(sidebar_width)
            .flex_shrink_0()
            .h_full()
            .bg(rgb(p.nav))
            // Signature header: the sky runs edge to edge, the OX mark and the borderless
            // command button sit on the keyline.
            .child(
                row()
                    .relative()
                    .overflow_hidden()
                    .h(px(56.))
                    .flex_shrink_0()
                    .px(gutter)
                    .gap(px(SPACE_S))
                    .justify_between()
                    .child(activity::horizon(p, &crate::sky::now(cx)))
                    .child(activity::marquee(p, hot, false))
                    .child(row().gap(px(SPACE_XS)).child(self.palette_button(cx)).when(
                        compact,
                        |bar| {
                            bar.child(crate::kit::icon_button(
                                p,
                                "close-threads",
                                "×",
                                "Close threads",
                                cx.listener(|s, _, w, cx| s.close_drawer(w, cx)),
                            ))
                        },
                    )),
            )
            .child(col().pt(px(SPACE_M)).px(gutter).child(picker).child(route))
            .child(
                div().pt(px(SPACE_M)).px(gutter).child(
                    crate::kit::nav_row(
                        "new-session",
                        "New session",
                        crate::kit::CONTROL_HEIGHT,
                        pane_ready,
                        cx.listener(|s, _, window, cx| {
                            s.close_drawer(window, cx);
                            s.request_active_draft(window, cx);
                            cx.notify();
                        }),
                    )
                    .bg(rgb(p.surface()))
                    .tooltip(format!("New session · {new_session_key}"))
                    .child(
                        row()
                            .w_full()
                            .gap(px(SPACE_S))
                            .child(
                                gpui_component::Icon::new(gpui_component::IconName::Plus)
                                    .size(px(14.))
                                    .text_color(rgb(p.muted)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(p.text(Step::Ui))
                                    .text_color(rgb(p.fg))
                                    .child("New session"),
                            )
                            .child(
                                div()
                                    .font_family(crate::appearance::LABEL_FONT)
                                    .text_size(p.text(Step::Label))
                                    .text_color(rgb(p.quiet()))
                                    .child(new_session_key),
                            ),
                    ),
                ),
            )
            .child(
                div().pt(px(SPACE_S)).px(gutter).child(
                    crate::platform::layout_probe::mark_element(
                        div().child(
                            gpui_component::input::Input::new(&self.filter)
                                .prefix(
                                    gpui_component::Icon::new(gpui_component::IconName::Search)
                                        .size(px(14.))
                                        .text_color(rgb(p.quiet())),
                                )
                                .suffix(self.filter_menu(p, cx)),
                        ),
                        &"thread-search".into(),
                        &"Search threads".into(),
                        true,
                    ),
                ),
            )
            .child(
                div()
                    .pt(px(SPACE_XS))
                    .px(gutter)
                    .child(self.grouping_control(p, cx)),
            )
            .when_some(section_editor, |sidebar, input| {
                sidebar.child(
                    row()
                        .pt(px(SPACE_S))
                        .px(gutter)
                        .gap(px(SPACE_XS))
                        .child(div().flex_1().min_w_0().child(crate::kit::input(&input)))
                        .child(crate::kit::icon_button(
                            p,
                            "save-section-name",
                            "✓",
                            "Save section",
                            cx.listener(|s, _, w, cx| s.commit_sidebar_edit(w, cx)),
                        ))
                        .child(crate::kit::icon_button(
                            p,
                            "cancel-section-name",
                            "×",
                            "Cancel section edit",
                            cx.listener(|s, _, _, cx| {
                                s.sidebar_edit = None;
                                cx.notify();
                            }),
                        )),
                )
            })
            .when_some(self.choose_route.clone(), |sidebar, (project, row)| {
                let host = cx.weak_entity();
                let aliases = self.models.read(cx).clone();
                sidebar.child(
                    div().pt(px(SPACE_S)).px(gutter).child(
                        button(
                            p,
                            "select-saved-model",
                            if row.needs_claude_alias() {
                                "Choose the billing alias to resume"
                            } else {
                                "Start new API session"
                            },
                            true,
                            true,
                            |_, _, _| {},
                        )
                        .w_full()
                        .dropdown_menu(move |mut menu, _, _| {
                            for alias in &aliases {
                                let project = project.clone();
                                let resume = row.needs_claude_alias();
                                let route = if resume {
                                    ox_desk_core::runtime::Runtime::Claude
                                        .route(Some(alias.clone()))
                                } else {
                                    Route::codex_api(alias.clone())
                                };
                                let row = row.clone();
                                let host = host.clone();
                                let label = if resume {
                                    format!("Resume using {alias}")
                                } else {
                                    format!("New session · {alias}")
                                };
                                menu = menu.item(crate::kit::menu_item(label).on_click(
                                    move |_, window, cx| {
                                        let _ = host.update(cx, |s, cx| {
                                            let project = project.clone();
                                            if resume {
                                                s.open(project, route.clone(), window, cx);
                                                s.choose_route = None;
                                                s.panes[s.active].update(cx, |pane, cx| {
                                                    pane.select_catalog(row.clone(), true, cx)
                                                });
                                                return;
                                            }
                                            s.open(project, route.clone(), window, cx);
                                            s.panes[s.active].update(cx, |pane, cx| {
                                                pane.action(cx, DeskAction::NewDraft)
                                            });
                                        });
                                    },
                                ));
                            }
                            menu
                        }),
                    ),
                )
            })
            .when(!background.is_empty(), |sidebar| {
                sidebar.child(div().pt(px(SPACE_S)).px(gutter).children(background))
            })
            .child(
                div()
                    .id("thread-navigation")
                    .track_focus(&self.list_focus)
                    .on_key_down(cx.listener(Self::navigate))
                    .mt(px(SPACE_XS))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "project-threads",
                            count,
                            cx.processor(move |s, range, _, cx| {
                                crate::app::try_draw(cx, |cx| {
                                    s.navigation_rows(range, sidebar_width, cx)
                                })
                                .unwrap_or_default()
                            }),
                        )
                        .track_scroll(&self.list_scroll)
                        .flex_1()
                        .min_h_0(),
                    ),
            )
            .child(self.running.clone())
            .child(self.sidebar_footer(p, running_count, running_open, window, cx))
            .when_some(
                // The pane header already shows its own connection error: say it once.
                self.message.clone().filter(|m| {
                    self.panes[self.active].read(cx).visible_error() != Some(m.as_str())
                }),
                |d, m| {
                    d.child(div().px(gutter).pb(px(SPACE_S)).child(crate::kit::banner(
                        p,
                        "host-error",
                        "host-error-dismiss",
                        m,
                        cx.listener(|s, _, _, cx| {
                            s.message = None;
                            cx.notify();
                        }),
                    )))
                },
            );
        let (side, drawer) = if inline {
            (Some(sidebar), None)
        } else if self.drawer_open {
            let drawer = div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .bg(black().opacity(0.48))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|s, _, w, cx| s.close_drawer(w, cx)),
                        ),
                )
                .child(
                    sidebar
                        .occlude()
                        .focus_trap("thread-drawer", &self.drawer_focus),
                );
            (None, Some(drawer))
        } else {
            (None, None)
        };
        row()
            .relative()
            .size_full()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .on_mouse_move(cx.listener(|s, e: &MouseMoveEvent, w, cx| {
                if s.resizing_sidebar {
                    if e.pressed_button == Some(MouseButton::Left) {
                        let limit =
                            (f32::from(w.viewport_size().width) - MIN_THREAD_WIDTH - DIVIDER_WIDTH)
                                .clamp(190., 540.);
                        s.sidebar_width = f32::from(e.position.x).clamp(190., limit) as u32;
                    } else {
                        s.resizing_sidebar = false
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|s, _, _, cx| {
                    if s.resizing_sidebar {
                        s.resizing_sidebar = false;
                        cx.notify()
                    }
                }),
            )
            .when_some(side, |layout, side| {
                layout.child(side).child(self.divider(p, cx))
            })
            .child(
                col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .when(!inline, |pane| {
                        pane.child(
                            row()
                                .relative()
                                .overflow_hidden()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .bg(rgb(p.nav))
                                .child(activity::horizon(p, &crate::sky::now(cx)))
                                .child(activity::marquee(p, hot, true))
                                .child(crate::kit::toolbar_button(
                                    p,
                                    "show-threads",
                                    threads_label,
                                    true,
                                    cx.listener(|s, _, w, cx| s.toggle_sidebar(w, cx)),
                                ))
                                .child(div().child(self.palette_button(cx))),
                        )
                    })
                    .child(
                        col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(match &self.preferences {
                                Some(view) => view.clone().into_any_element(),
                                None => self.panes[self.active].clone().into_any_element(),
                            }),
                    ),
            )
            .children(drawer)
            .children(self.palette_overlay(cx))
            .children(self.model_overlay(cx))
    }
}

#[cfg(test)]
mod responsive_tests {
    use super::{needs_drawer, px};

    #[test]
    fn drawer_reserves_conversation_space_at_saved_sidebar_width() {
        assert!(needs_drawer(px(717.), 254));
        assert!(!needs_drawer(px(859.), 254));
        assert!(needs_drawer(px(1000.), 540));
        assert!(!needs_drawer(px(1280.), 540));
    }
}

/// A sidebar picker on the keyline: a quiet micro label, the value and a caret.
fn picker_row(
    p: Palette,
    id: &'static str,
    label: &'static str,
    value: String,
) -> gpui_component::button::Button {
    crate::kit::nav_row(
        id,
        format!("{} {value}", label.to_lowercase()),
        crate::kit::ICON_SIZE,
        true,
        |_, _, _| {},
    )
    .child(
        row()
            .w_full()
            .min_w_0()
            .gap(px(SPACE_S))
            .child(crate::kit::micro_label(p, label).w(px(56.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.fg))
                    .child(value),
            )
            .child(crate::kit::caret(p)),
    )
}

/// Sets the one `is:` status filter to `token`, keeping the search text. An empty token
/// clears the status filter ("All").
fn select_status_filter(value: &str, token: &str) -> String {
    let mut next: Vec<&str> = value
        .split_whitespace()
        .filter(|word| !word.starts_with("is:"))
        .collect();
    if !token.is_empty() {
        next.push(token);
    }
    next.join(" ")
}

#[cfg(test)]
mod tests {
    use super::select_status_filter;

    #[test]
    fn status_filter_menu_sets_one_filter_and_keeps_text() {
        assert_eq!(select_status_filter("", "is:running"), "is:running");
        assert_eq!(
            select_status_filter("fix bug", "is:waiting"),
            "fix bug is:waiting"
        );
        assert_eq!(
            select_status_filter("is:running fix", "is:waiting"),
            "fix is:waiting"
        );
        assert_eq!(select_status_filter("is:running fix", ""), "fix");
    }
}

#[cfg(test)]
mod route_gate_tests {
    use super::new_route_gate;
    use ox_desk_core::runtime::Runtime;

    #[test]
    fn subscription_routes_need_only_their_account() {
        let claude = Runtime::Claude.route(None);
        let codex = Runtime::Codex.route(None);
        assert!(new_route_gate(&claude, &[], false).is_ok());
        assert!(new_route_gate(&codex, &[], true).is_ok());
        assert!(
            new_route_gate(&codex, &[], false)
                .unwrap_err()
                .contains("ChatGPT")
        );
    }

    #[test]
    fn any_catalog_alias_opens_without_being_configured_or_starred() {
        let api = Runtime::Codex.route(Some("fixture/demo".into()));
        assert!(new_route_gate(&api, &["fixture/demo".into()], false).is_ok());
        // A discovery-only model is not in the configured list yet; it still opens.
        assert!(new_route_gate(&api, &[], false).is_ok());
        let claude = Runtime::Claude.route(Some("fixture/demo".into()));
        assert!(new_route_gate(&claude, &[], false).is_ok());
    }
}
