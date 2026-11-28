//! A workbench owns the visible state for one project and billing route.
use crate::appearance::Step;
use crate::kit::{SPACE_S, SPACE_XS};
use crate::platform::HostLink;
#[cfg(test)]
use crate::platform::open_store;
use crate::{activity, appearance::Palette, col, platform, row};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::{
    inbox::Inbox,
    store::{Connection, Route, Store},
};
use std::{collections::HashMap, path::PathBuf, time::Instant};

pub(super) mod empty;
mod review;
pub(super) mod thread_menu;
mod usage;
mod worker;

/// Plain wording for the connection errors users meet most, each with its next step.
/// Anything unrecognised is shown as is.
fn plain_status(message: &str) -> String {
    match message {
        "Session host disconnected" | "Connection closed" => {
            "The session host stopped. Reconnect to start it.".into()
        }
        "Incomplete or oversized host frame" => {
            "The session host sent an unreadable reply. Reconnect, or restart OX Desktop if it keeps happening.".into()
        }
        "Claude Code closed its output" => {
            "Claude Code stopped unexpectedly. Reconnect to start it again.".into()
        }
        _ => message.to_owned(),
    }
}

fn current_activity(store: &Store) -> Option<(activity::Scene, u64, Option<String>)> {
    use activity::Scene;
    use ox_desk_core::transcript::{Row, item::Item};
    let id = store.selected()?;
    if store
        .asks()
        .iter()
        .any(|ask| ask.thread.as_deref() == Some(id))
    {
        return Some((Scene::Waiting, 0, None));
    }
    let transcript = store.transcript(id)?;
    if transcript.is_loading() {
        return Some((Scene::Loading, 0, None));
    }
    let active = transcript.active_turn()?;
    let tail = transcript
        .len()
        .checked_sub(1)
        .and_then(|i| transcript.row(i));
    let key = match tail {
        Some(Row::Message(key)) => Some(key),
        Some(Row::Tools(group)) => group.back(),
        None => None,
    };
    let item = key
        .filter(|key| key.turn == active)
        .and_then(|key| transcript.entry(key));
    let detail = item
        .and_then(|entry| match &entry.item {
            Item::Reasoning { summary } => summary
                .last()
                .and_then(|s| s.lines().next())
                .map(|s| s.trim().to_owned()),
            Item::UserMessage { .. } | Item::AgentMessage { .. } | Item::Plan { .. } => None,
            other => Some(super::transcript::cards::summary(other)),
        })
        .filter(|s| !s.is_empty());
    Some((
        Scene::for_item(item.map(|entry| &entry.item)),
        activity::seed(id, active),
        detail,
    ))
}

/// A row-level action requested while a pane or thread was still opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::views) enum DeferredRowAction {
    Archive,
    End,
    GenerateTitle,
    FindInTranscript,
}

pub struct Workbench {
    pub(super) workspace: Option<WeakEntity<crate::app::Workspace>>,
    pub(super) handoff_pending: Option<String>,
    pub(super) link: HostLink,
    workers: Entity<super::host::WorkerIndex>,
    pub(super) pending_local_command: Option<super::terminal_review::Pending>,
    worker_results_open: Option<String>,
    pub(super) enter_sends: bool,
    review: review::ReviewPane,
    pub(super) review_width: u32,
    pub(super) pane_width: Pixels,
    pub(super) host_caption: Option<String>,
    pub(super) worker_update: Option<String>,
    resizing_review: bool,
    pub(super) route_models: Entity<Vec<String>>,
    pub(super) model_picker: Option<super::route_chip::Picker>,
    /// When an outside press last closed the model panel.
    pub(super) picker_dismissed: Option<std::time::Instant>,
    thread_edit: Option<thread_menu::ThreadEdit>,
    pub(super) launch_request: Option<super::worker_sheet::LaunchKind>,
    pub(super) launch: Option<super::worker_sheet::Launch>,
    pub(super) move_picker: Option<super::session_move::Picker>,
    pub(super) launch_picker: Option<(
        super::worker_sheet::LaunchKind,
        Entity<super::model_picker::ModelPicker>,
        Subscription,
    )>,
    pub(super) project: PathBuf,
    pub(super) route: Route,
    pub(super) front: bool,
    pub(super) query: String,
    pending_new: bool,
    pending_selection: Option<(ox_desk_core::store::ThreadRow, bool)>,
    pending_row_action: Option<(String, DeferredRowAction)>,
    pending_draft: Option<(String, Vec<ox_desk_core::mentions::ImageInput>)>,
    pub(super) store: Option<Store>,
    pub(super) launch_error: Option<String>,
    pub(super) mention_index: usize,
    pub(super) mention_task: Option<Task<()>>,
    pub(super) file_hover: Option<(Option<String>, Vec<PathBuf>)>,
    pub(super) image_work: HashMap<Option<String>, usize>,
    pub(super) pending_command: Option<(Option<String>, String)>,
    pub(super) connecting: bool,
    pub(super) awaiting_login: bool,
    retry_available: bool,
    first_frame: Option<Instant>,
    pub(super) stats: Option<Stats>,
    pub(super) wake: flume::Sender<()>,
    waiting_frame: Option<flume::Sender<()>>,
    pub(super) focus_editor_for: Option<Option<String>>,
    pub(super) editors: HashMap<Option<String>, Entity<gpui_component::input::TextareaState>>,
    pub(super) focused_ask: Option<ox_desk_core::runtime::RequestId>,
    pub(super) answer_fields:
        HashMap<ox_desk_core::runtime::RequestId, Vec<Entity<gpui_component::input::InputState>>>,
    pub(super) fire: Option<Entity<activity::Runner>>,
    pub(super) runner: Option<Entity<activity::Runner>>,
    pub(super) mascot_frame: u8,
    /// The selected thread's running turn at the last render, to notice it ending.
    last_turn: Option<(String, String)>,
    /// A just-ended turn's one-shot scene, kept until it has been seen once.
    finale: Option<(String, activity::Scene)>,
    finale_watch: Option<Subscription>,
    pub(super) transcript_views: HashMap<String, super::transcript::TranscriptView>,
    _pump: Task<()>,
}

pub(super) struct FocusSearch;
impl EventEmitter<FocusSearch> for Workbench {}
pub(super) struct CatalogChanged;
impl EventEmitter<CatalogChanged> for Workbench {}
/// Ask the host to open a recorded parent session: the full scope travels with
/// the id so the target is reachable across checkouts and billing routes.
pub(super) struct OpenThread(pub super::host::ParentLink);
impl EventEmitter<OpenThread> for Workbench {}

pub(super) struct OpenOnPhone {
    pub scope: ox_desk_core::host_protocol::Scope,
    pub thread: String,
    pub title: String,
}
impl EventEmitter<OpenOnPhone> for Workbench {}

#[derive(Default)]
pub(super) struct Stats {
    pub(super) drains: u64,
    pub(super) notifications: u64,
    pub(super) rows: u64,
    pub(super) spawns: u64,
    pub(super) stop_ns: u64,
    pub(super) painted: std::rc::Rc<std::cell::Cell<u64>>,
    pub(super) tail_height: std::rc::Rc<std::cell::Cell<f32>>,
    pub(super) tail_owner: Option<(String, usize)>,
    pub(super) measured_items: std::collections::HashSet<usize>,
}

impl Workbench {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        link: HostLink,
        workers: Entity<super::host::WorkerIndex>,
        project: PathBuf,
        route: Route,
        first_frame: Option<Instant>,
        stats: bool,
        route_models: Entity<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            link,
            workers,
            project,
            route,
            Some(first_frame),
            stats,
            route_models,
            window,
            cx,
        )
    }

    /// Create a background pane without connecting to `ox-host` or spawning a runtime yet.
    /// It connects on its first `set_front(true, window, cx)`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_lazy(
        link: HostLink,
        workers: Entity<super::host::WorkerIndex>,
        project: PathBuf,
        route: Route,
        stats: bool,
        route_models: Entity<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            link,
            workers,
            project,
            route,
            None,
            stats,
            route_models,
            window,
            cx,
        )
    }

    /// `active`: `Some(first_frame)` starts connected as the front pane; `None` starts lazy.
    #[allow(clippy::too_many_arguments)]
    fn build(
        link: HostLink,
        workers: Entity<super::host::WorkerIndex>,
        project: PathBuf,
        route: Route,
        active: Option<Option<Instant>>,
        stats: bool,
        route_models: Entity<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let front = active.is_some();
        let first_frame = active.flatten();
        cx.observe_window_activation(window, |s, window, cx| {
            if s.front {
                cx.notify();
            }
            if window.is_window_active() && s.awaiting_login && s.front {
                s.reconnect(window, cx);
            }
        })
        .detach();
        cx.observe_window_visibility(window, |s, _, window, cx| {
            if s.store.is_some() {
                let _ = s.wake.try_send(());
            }
            if window.is_visible() && s.front {
                cx.notify();
            } else {
                if let Some(waiter) = s.waiting_frame.take() {
                    let _ = waiter.try_send(());
                }
                s.runner = None;
                s.fire = None;
            }
        })
        .detach();
        let (wake, pump) = if front {
            Self::connection(link.clone(), project.clone(), route.clone(), window, cx)
        } else {
            let (wake, _) = flume::bounded(1);
            (wake, Task::ready(()))
        };
        Self {
            workspace: None,
            handoff_pending: None,
            link,
            workers,
            worker_results_open: None,
            enter_sends: cx
                .global::<crate::preferences::behavior::Active>()
                .enter_sends,
            thread_edit: None,
            review: Default::default(),
            review_width: 360,
            pane_width: window.viewport_size().width,
            host_caption: None,
            worker_update: None,
            resizing_review: false,
            model_picker: None,
            route_models,
            picker_dismissed: None,
            launch_request: None,
            launch: None,
            move_picker: None,
            launch_picker: None,
            front,
            query: String::new(),
            pending_new: false,
            pending_selection: None,
            pending_row_action: None,
            pending_draft: None,
            project,
            route,
            store: None,
            launch_error: None,
            pending_local_command: None,
            mention_index: 0,
            mention_task: None,
            file_hover: None,
            image_work: Default::default(),
            pending_command: None,
            connecting: front,
            awaiting_login: false,
            retry_available: true,
            first_frame,
            stats: stats.then(Stats::default),
            wake,
            waiting_frame: None,
            runner: None,
            last_turn: None,
            finale: None,
            finale_watch: None,
            fire: None,
            focused_ask: None,
            mascot_frame: 0,
            focus_editor_for: None,
            editors: HashMap::new(),
            answer_fields: HashMap::new(),
            transcript_views: HashMap::new(),
            _pump: pump,
        }
    }

    fn connection(
        link: HostLink,
        directory: PathBuf,
        billing: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (flume::Sender<()>, Task<()>) {
        let (wake, wakes) = flume::bounded(1);
        let signal = wake.clone();
        let inbox = Inbox::new(move || {
            let _ = signal.try_send(());
        });
        let input = inbox.clone();
        let start = cx.background_spawn(async move {
            link.open_store(directory, billing, move |event| input.push(event))
        });
        let pump = cx.spawn_in(window, async move |view, cx| {
            let result = start.await;
            let mut deadline = None;
            if view
                .update_in(cx, |s, window, cx| {
                    s.connecting = false;
                    match result {
                        Ok(mut store) => {
                            if let Some(previous) = &mut s.store {
                                if !s.pending_new
                                    && s.pending_selection.is_none()
                                    && store.profile().project == previous.profile().project
                                    && store.profile().route == previous.profile().route
                                    && let Some(row) =
                                        previous.selected().and_then(|id| previous.catalog_row(id))
                                {
                                    s.pending_selection = Some((row.clone(), false));
                                }
                                store.recover_from(previous);
                                s.answer_fields.clear();
                                s.focused_ask = None;
                                s.transcript_views.clear();
                            }
                            s.pending_command = None;
                            s.launch_error = None;
                            if let Err(e) = store.search(s.query.clone()) {
                                s.launch_error = Some(e.to_string());
                            }
                            if let Some((text, images)) = s.pending_draft.take() {
                                while !store.draft_images(None).is_empty() {
                                    store.remove_image(None, 0);
                                }
                                store.edit(None, text);
                                for img in images {
                                    store.attach_image(None, img);
                                }
                            }
                            deadline = store.next_deadline();
                            store.set_front(s.front);
                            s.store = Some(store);
                            if let Some(stats) = &mut s.stats {
                                stats.spawns += 1;
                            }
                        }
                        Err(error) => {
                            s.pending_new = false;
                            let message = error.to_string();
                            // Retrying cannot repair a folder's permissions.
                            let fixable = ox_host_service::unsafe_folder(&message).is_some();
                            s.launch_error = Some(message);
                            if !fixable {
                                s.retry_once(window, cx);
                            }
                        }
                    }
                    if window.is_visible() {
                        cx.emit(CatalogChanged);
                        if s.front {
                            cx.notify();
                        }
                    }
                })
                .is_err()
            {
                return;
            }
            let (tick, frames) = flume::bounded(1);
            let frame_pending = std::rc::Rc::new(std::cell::Cell::new(false));
            loop {
                let awakened = if let Some(when) = deadline {
                    let timer = cx
                        .background_executor()
                        .timer(when.saturating_duration_since(Instant::now()));
                    futures_lite::future::or(async { wakes.recv_async().await.is_ok() }, async {
                        timer.await;
                        true
                    })
                    .await
                } else {
                    wakes.recv_async().await.is_ok()
                };
                if !awakened {
                    break;
                }
                // Keep the inbox full until the visible frame is due. The reader
                // can continue appending without waking the main thread for each delta.
                let next = view.update_in(cx, |s, window, _| {
                    if !window.is_visible()
                        || !s.front
                        || inbox.starts_with(|event| event.unblocks_interaction())
                    {
                        return None;
                    }
                    s.waiting_frame = Some(tick.clone());
                    if !frame_pending.replace(true) {
                        let send = tick.clone();
                        let pending = frame_pending.clone();
                        window.on_next_frame(move |_, _| {
                            pending.set(false);
                            let _ = send.try_send(());
                        });
                    }
                    Some(())
                });
                match next {
                    Ok(Some(())) => {
                        // Some Wayland compositors pause frame callbacks for a covered
                        // surface without a visibility event. Keep protocol work moving.
                        let timeout = cx
                            .background_executor()
                            .timer(std::time::Duration::from_millis(80));
                        futures_lite::future::or(
                            async {
                                let _ = frames.recv_async().await;
                            },
                            timeout,
                        )
                        .await;
                    }
                    Ok(None) => (),
                    Err(_) => break,
                }
                if view
                    .update_in(cx, |s, window, cx| {
                        s.waiting_frame = None;
                        let batch = inbox.take();
                        let local_result = s.local_command_outcome(&batch);
                        if let Some(store) = s.store.as_mut() {
                            if !batch.is_empty()
                                && let Some(stats) = &mut s.stats
                            {
                                stats.drains += 1;
                            }
                            let changed = store.drain(batch, Instant::now());
                            for id in changed.replaced_threads() {
                                if let Some(view) = s.transcript_views.get_mut(id) {
                                    view.invalidate_messages();
                                }
                            }
                            if (store.profile().route == Route::CODEX_SUBSCRIPTION
                                || store.profile().route.runtime
                                    == ox_desk_core::runtime::Runtime::Antigravity)
                                && store.state() == &Connection::Ready
                            {
                                s.awaiting_login = false;
                            }
                            if s.front
                                && store.state() == &Connection::Ready
                                && store.profile().route == Route::CODEX_SUBSCRIPTION
                                && !store.usage.requested
                            {
                                let _ = store.read_limits();
                            }
                            if matches!(
                                store.state(),
                                Connection::Closed(_) | Connection::SignInRequired
                            ) {
                                s.pending_new = false;
                            }
                            if store.state() == &Connection::Ready
                                && std::mem::take(&mut s.pending_new)
                                && let Err(e) = store.dispatch(DeskAction::NewDraft)
                            {
                                s.launch_error = Some(e.to_string());
                            }
                            if store.state() == &Connection::Ready
                                && let Some((row, confirmed)) = s.pending_selection.take()
                                && (confirmed
                                    || store.selected() != Some(row.id.as_str())
                                    || !store
                                        .transcript(&row.id)
                                        .is_some_and(|t| t.is_ready() || t.active_turn().is_some()))
                            {
                                s.launch_error = store
                                    .dispatch(if confirmed {
                                        DeskAction::ConfirmAliasAndResume(Box::new(row))
                                    } else {
                                        DeskAction::OpenCatalogRow(Box::new(row))
                                    })
                                    .err()
                                    .map(|e| e.to_string());
                            }
                            deadline = store.next_deadline();
                            let disconnected = matches!(store.state(), Connection::Closed(_));
                            if window.is_visible() && changed.affects_catalog() {
                                cx.emit(CatalogChanged);
                            }
                            if window.is_visible()
                                && s.front
                                && changed.affects_view(store.selected())
                            {
                                if let Some(stats) = &mut s.stats {
                                    stats.notifications += 1;
                                }
                                cx.notify();
                            }
                            s.finish_local_command(local_result, cx);
                            s.finish_command_discovery(cx);
                            s.finish_pending_row_action(window, cx);
                            s.launch_handoff(cx);
                            if disconnected {
                                s.retry_once(window, cx);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        (wake, pump)
    }

    pub(super) fn waiting_for_subscription(&self) -> bool {
        !self.route.is_api()
            && self
                .store
                .as_ref()
                .is_some_and(|s| s.state() == &Connection::SignInRequired)
    }
    pub(super) fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting
            || self
                .store
                .as_ref()
                .is_some_and(|s| s.state() == &Connection::Connecting)
        {
            return;
        }
        self.pending_local_command = None;
        // Returning before terminal login finishes must leave the next activation armed.
        if !self.waiting_for_subscription() {
            self.awaiting_login = false;
        }
        self.connecting = true;
        self.launch_error = None;
        (self.wake, self._pump) = Self::connection(
            self.link.clone(),
            self.project.clone(),
            self.route.clone(),
            window,
            cx,
        );
        cx.notify();
    }
    fn retry_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.retry_available) {
            self.reconnect(window, cx);
        }
    }
    /// The error this pane shows in its own header, if any.
    pub(super) fn visible_error(&self) -> Option<&str> {
        self.launch_error
            .as_deref()
            .or_else(|| match self.store.as_ref()?.state() {
                Connection::Closed(message) => Some(message),
                _ => None,
            })
    }
    /// Make an unsafe OX state folder private again off the UI thread, then reconnect.
    /// If that fails, name the exact command instead.
    fn fix_permissions(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let target = folder.clone();
        let task =
            cx.background_spawn(async move { ox_host_service::fix_folder_permissions(&target) });
        cx.spawn_in(window, async move |s, cx| {
            let result = task.await;
            let _ = s.update_in(cx, |s, window, cx| match result {
                Ok(()) => s.reconnect(window, cx),
                Err(e) => {
                    s.launch_error = Some(format!(
                        "Could not fix permissions ({e}). In a terminal, run: {}",
                        ox_host_service::fix_command(&folder)
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn queue_row_action(
        &mut self,
        id: String,
        action: DeferredRowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_row_action = Some((id, action));
        self.finish_pending_row_action(window, cx);
    }

    fn finish_pending_row_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, action)) = self.pending_row_action.clone() else {
            return;
        };
        let Some(store) = &self.store else {
            return;
        };
        if store.state() != &Connection::Ready || store.selected() != Some(id.as_str()) {
            return;
        }
        match action {
            DeferredRowAction::Archive => {
                if !store.can_edit_thread_metadata() {
                    self.pending_row_action = None;
                    return;
                }
                if !store.can_manage_thread(&id) {
                    return;
                }
                self.pending_row_action = None;
                self.request_archive(id, cx);
            }
            DeferredRowAction::End => {
                self.pending_row_action = None;
                self.request_end(id, cx);
            }
            DeferredRowAction::GenerateTitle => {
                if !store
                    .transcript(&id)
                    .is_some_and(|t| t.is_ready() || t.active_turn().is_some())
                {
                    return;
                }
                self.pending_row_action = None;
                if !store.can_generate_title(&id) {
                    return;
                }
                self.action(cx, DeskAction::GenerateTitle(id));
            }
            DeferredRowAction::FindInTranscript => {
                if !store
                    .transcript(&id)
                    .is_some_and(|t| t.is_ready() || t.active_turn().is_some())
                {
                    return;
                }
                self.pending_row_action = None;
                if !self.transcript_views.contains_key(&id) {
                    // Render once so `transcript_views` creates the entry for `id`, then open Find.
                    self.pending_row_action = Some((id, action));
                    cx.notify();
                    return;
                }
                self.find_in_transcript(window, cx);
            }
        }
    }

    pub(super) fn start_project_draft(&mut self, cx: &mut Context<Self>) {
        self.pending_selection = None;
        self.focus_editor_for = Some(None);
        if self
            .store
            .as_ref()
            .is_some_and(|s| s.state() == &Connection::Ready)
        {
            self.action(cx, DeskAction::NewDraft);
        } else {
            self.pending_new = true;
            cx.notify();
        }
    }
    pub(super) fn select_catalog(
        &mut self,
        row: ox_desk_core::store::ThreadRow,
        confirmed: bool,
        cx: &mut Context<Self>,
    ) {
        self.pending_new = false;
        if self
            .store
            .as_ref()
            .is_some_and(|s| s.state() == &Connection::Ready)
        {
            self.action(
                cx,
                if confirmed {
                    DeskAction::ConfirmAliasAndResume(Box::new(row))
                } else {
                    DeskAction::OpenCatalogRow(Box::new(row))
                },
            );
        } else {
            self.pending_selection = Some((row, confirmed));
            cx.notify();
        }
    }
    pub(super) fn restore_review(&mut self, open: bool, width: u32) {
        self.review.open = open;
        if width > 0 {
            self.review_width = width.clamp(230, 600);
        }
    }
    pub(super) fn review_visible(&self) -> bool {
        self.review.open
    }
    pub(super) fn restore_selection(&mut self, row: ox_desk_core::store::ThreadRow) {
        self.pending_selection = Some((row, false))
    }
    /// Carry over an unsent first-prompt draft (text and attached images) when switching
    /// routes/models before sending.
    pub(super) fn adopt_new_draft(
        &mut self,
        text: String,
        images: Vec<ox_desk_core::mentions::ImageInput>,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() && images.is_empty() {
            return;
        }
        if let Some(store) = &mut self.store {
            let _ = store.dispatch(DeskAction::NewDraft);
            while !store.draft_images(None).is_empty() {
                store.remove_image(None, 0);
            }
            store.edit(None, text);
            for img in images {
                store.attach_image(None, img);
            }
            cx.notify();
        } else {
            self.pending_selection = None;
            self.pending_draft = Some((text, images));
            cx.notify();
        }
    }
    pub(super) fn layout_selection(&self) -> Option<&ox_desk_core::store::ThreadRow> {
        self.pending_selection
            .as_ref()
            .map(|(row, _)| row)
            .or_else(|| {
                let store = self.store.as_ref()?;
                store.catalog_row(store.selected()?)
            })
    }
    pub(super) fn action(&mut self, cx: &mut Context<Self>, action: DeskAction) {
        if let DeskAction::StartHandoff(start) = &action
            && let Some(selector) = &start.host
        {
            let mut start = start.clone();
            start.host = None;
            match self.select_handoff_destination(selector, cx) {
                Ok(Some(to)) => self.send_handoff(start, to, cx),
                Ok(None) => self.action(cx, DeskAction::StartHandoff(start)),
                Err(e) => {
                    self.launch_error = Some(e);
                    cx.notify();
                }
            }
            return;
        }
        if let Some(host) = self.link.remote()
            && let Some(store) = &mut self.store
        {
            let command = match &action {
                DeskAction::ExecuteCommand(id) => Some((id, store.draft(id.as_deref()))),
                DeskAction::PaletteCommand(id, text) => Some((id, text.as_str())),
                DeskAction::Terminal(id) => Some((id, "/terminal")),
                _ => None,
            };
            if let Some((owner, text)) = command
                && (text.trim() == "/terminal"
                    || matches!(
                        store.parse_command(owner.as_deref(), text),
                        Some(ox_desk_core::slash::Action::Terminal)
                    ))
            {
                use ox_host_web::terminal_protocol::OpenTerminal;
                let req = match owner {
                    Some(thread) => OpenTerminal::NativeSession {
                        scope: ox_desk_core::host_protocol::Scope {
                            project: store.profile().project.clone(),
                            route: self.route.clone(),
                        },
                        thread: thread.clone(),
                    },
                    None => OpenTerminal::Shell {
                        project: store.profile().project.clone(),
                    },
                };
                self.launch_error = crate::terminal::open_remote(host.clone(), req, cx).err();
                cx.notify();
                return;
            }
        }
        if matches!(
            action,
            DeskAction::NewDraft | DeskAction::OpenCatalogRow(_) | DeskAction::SelectThread(_)
        ) {
            cx.emit(CatalogChanged);
            self.pending_selection = None;
            self.pending_row_action = None;
            self.model_picker = None;
            self.thread_edit = None;
            self.pending_command = None;
            self.launch = None;
            self.launch_picker = None;
        }
        if matches!(action, DeskAction::Stop(_))
            && let Some(stats) = &mut self.stats
        {
            stats.stop_ns = ox_desk_core::perf::monotonic_ns();
        }
        if let Some(store) = &mut self.store {
            self.launch_error = match store.dispatch(action) {
                Ok(ox_desk_core::actions::Effect::Applied) => None,
                Ok(ox_desk_core::actions::Effect::FocusSearch) => {
                    cx.emit(FocusSearch);
                    None
                }
                Err(err) => Some(err.to_string()),
            };
            let _ = self.wake.try_send(());
            cx.notify();
        }
    }
    fn launch_handoff(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &mut self.store else { return };
        let Some(handoff) = store.take_handoff() else {
            return;
        };
        if self.link.remote().is_some() {
            self.launch_error = Some("Open the terminal on the session host.".into());
            return;
        }
        let project = store.profile().project.clone();
        let task = cx.background_spawn(async move {
            platform::launch_runtime_terminal(&project, handoff.argv)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                match result {
                    Ok(()) => {
                        let copy = handoff.command != "/terminal";
                        if copy {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                handoff.command.clone(),
                            ));
                        }
                        if let Some(store) = &mut s.store
                            && store.draft(handoff.thread.as_deref()).trim() == handoff.command
                        {
                            store.edit(handoff.thread.as_deref(), String::new());
                        }
                        s.launch_error = Some(if copy {
                            format!("Paste the command in stock {}.", s.route.runtime.name())
                        } else {
                            format!(
                                "Type / to browse the stock {} commands.",
                                s.route.runtime.name()
                            )
                        });
                    }
                    Err(e) => {
                        if let Some(id) = handoff.thread
                            && s.store.as_ref().and_then(|s| s.selected()) == Some(id.as_str())
                        {
                            s.action(cx, DeskAction::ResumeHere(id));
                        }
                        s.launch_error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    /// What the activity slot shows: the live scene, or the finale of a turn that just ended.
    fn activity_scene(&mut self) -> Option<(activity::Scene, u64, Option<String>)> {
        use activity::Scene;
        let store = self.store.as_ref()?;
        let id = store.selected();
        let transcript = id.and_then(|id| store.transcript(id));
        let turn = id
            .zip(transcript.and_then(|t| t.active_turn()))
            .map(|(id, turn)| (id.to_owned(), turn.to_owned()));
        if turn != self.last_turn {
            self.finale = match (self.last_turn.take(), &turn) {
                (Some((thread, _)), None) if id == Some(thread.as_str()) => {
                    let failed = transcript.is_some_and(|t| t.error().is_some());
                    let stopped = transcript.is_some_and(|t| t.interrupted());
                    Some((
                        thread,
                        if stopped {
                            Scene::Stopped
                        } else if failed {
                            Scene::GameOver
                        } else {
                            Scene::Done
                        },
                    ))
                }
                _ => None,
            };
            self.last_turn = turn;
        }
        if self
            .finale
            .as_ref()
            .is_some_and(|(thread, _)| id != Some(thread.as_str()))
        {
            self.finale = None;
        }
        current_activity(store).or(self.finale.as_ref().map(|(_, scene)| (*scene, 0, None)))
    }
    pub(super) fn set_front(&mut self, front: bool, cx: &mut Context<Self>) {
        if !front {
            self.focus_editor_for = None;
        }
        if front && !self.front {
            self.refresh_worker_hosts();
        }
        self.front = front;
        if let Some(store) = &mut self.store {
            store.set_front(front);
        }
        if front {
            cx.notify();
        } else {
            self.runner = None;
            self.fire = None;
        }
    }

    pub(super) fn set_front_in(
        &mut self,
        front: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !front {
            self.focus_editor_for = None;
        }
        if front && !self.front {
            self.refresh_worker_hosts();
        }
        self.front = front;
        if let Some(store) = &mut self.store {
            store.set_front(front);
        }
        if front && self.store.is_none() && !self.connecting {
            self.reconnect(window, cx);
        }
        if front {
            cx.notify();
        } else {
            self.runner = None;
            self.fire = None;
        }
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| self.draw(window, cx).into_any_element())
    }
}

impl Workbench {
    fn draw(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        if let Some(started) = self.first_frame.take() {
            window.on_next_frame(move |_, cx| {
                println!("desk-first-frame ms={}", started.elapsed().as_millis());
                cx.quit();
            });
        }
        if let Some(issue) = self.first_run_issue() {
            return self.onboarding(issue, cx);
        }
        if let Some(kind) = self.launch_request.take() {
            self.open_launch(kind, window, cx);
        }
        let unavailable = !self.connecting
            && self.store.as_ref().is_none_or(|s| {
                matches!(
                    s.state(),
                    Connection::Closed(_) | Connection::SignInRequired
                )
            });
        let connecting = format!("Connecting to {}…", self.route.runtime.name());
        let sign_in = format!("Sign in to {}", self.route.runtime.account());
        let status = self
            .launch_error
            .as_deref()
            .or(self.handoff_pending.as_deref())
            .unwrap_or_else(|| {
                if self.connecting {
                    return &connecting;
                }
                match self.store.as_ref().map(Store::state) {
                    None | Some(Connection::Connecting) => &connecting,
                    Some(Connection::Ready) => "Connected",
                    Some(Connection::SignInRequired) => &sign_in,
                    Some(Connection::Closed(message)) => message,
                }
            });
        let title = self
            .store
            .as_ref()
            .filter(|store| {
                self.launch_error.is_none()
                    && !self.connecting
                    && store.state() == &Connection::Ready
            })
            .and_then(|store| store.catalog_row(store.selected()?))
            .map(|row| row.title().to_owned());
        let status = plain_status(status);
        let unsafe_folder = ox_host_service::unsafe_folder(&status);
        let live = self.activity_scene();
        let seen = self.front && window.is_visible();
        let animate = p.motion && seen && window.is_window_active();
        // A finale plays once even with motion off: it is a still that leaves on one timer.
        if let Some((scene, seed, _)) = live
            .as_ref()
            .filter(|(scene, _, _)| animate || (seen && scene.one_shot().is_some()))
        {
            if self.runner.as_ref().is_none_or(|r| {
                let r = r.read(cx);
                (r.scene, r.seed) != (*scene, *seed)
            }) {
                let (scene, seed) = (*scene, *seed);
                let runner = cx.new(|_| activity::Runner::new(scene, seed));
                self.finale_watch = scene.one_shot().map(|_| {
                    cx.subscribe(&runner, |s, _, _: &activity::Finished, cx| {
                        s.finale = None;
                        s.runner = None;
                        cx.notify();
                    })
                });
                self.runner = Some(runner);
            }
        } else {
            self.runner = None;
        }
        let activity_status = live
            .filter(|(scene, _, _)| *scene != activity::Scene::Loading)
            .map(|(scene, seed, detail)| {
                let icon = self
                    .runner
                    .as_ref()
                    .map(|r| r.clone().into_any_element())
                    .unwrap_or_else(|| scene.still(p, seed));
                let text = match detail {
                    Some(d) => format!("{} · {d}", scene.label()),
                    None => scene.label().to_owned(),
                };
                row()
                    .gap_2()
                    .py_1()
                    .min_w_0()
                    .child(icon)
                    .child(crate::kit::small(p, text).min_w_0().truncate())
            });
        let ready = self.launch_error.is_none()
            && !self.connecting
            && self
                .store
                .as_ref()
                .is_some_and(|s| s.state() == &Connection::Ready && s.selected().is_none());
        let transcript = (!ready).then(|| self.transcript_panel(cx));
        if self.pending_row_action.is_some() {
            self.finish_pending_row_action(window, cx);
        }
        let move_button = self.live_move_button(p, cx);
        let move_panel = self.live_move_panel(p, cx);
        let source_reserved = self.live_move_locked();
        let move_pending = self.live_move_pending();
        let controls = (!move_pending).then(|| self.thread_menu(cx)).flatten();
        let start_worker = (!move_pending)
            .then(|| self.start_worker_button(p, cx))
            .flatten();
        let review_toggle = self.review_toggle(cx);
        let inline_request = self.pane_width < px(825.);
        let asks = self.ask_sheet(inline_request, window, cx);
        let launch = (!source_reserved)
            .then(|| self.launch_sheet(window.viewport_size().height * 0.6, cx))
            .flatten();
        let usage = self.usage_bar(cx);
        let review = asks.is_none().then(|| self.review_panel(cx)).flatten();
        let review_width = px(self.review_width as f32 + 5.);
        let narrow_review = review.is_some() && self.pane_width - review_width < px(500.);
        let detail_width = if review.is_some() && !narrow_review {
            self.pane_width - review_width
        } else if asks.is_some() && !inline_request {
            self.pane_width - px(325.)
        } else {
            self.pane_width
        };
        let composer = self.composer(detail_width, window, cx);
        use crate::platform::restore_probe::{self, Milestone};
        if self.front
            && let Some(store) = &self.store
            && let Some(id) = store.selected()
        {
            if !narrow_review
                && !source_reserved
                && restore_probe::wants(id, Milestone::ComposerReady)
                && store.can_compose(Some(id))
            {
                restore_probe::mark(Milestone::ComposerReady);
            }
            if restore_probe::wants(id, Milestone::ThreadLive)
                && store.state() == &Connection::Ready
                && store
                    .transcript(id)
                    .is_some_and(|t| t.is_ready() && !t.is_loading() && t.error().is_none())
            {
                restore_probe::mark(Milestone::ThreadLive);
            }
        }
        let terminal = (status == "Connected" && !source_reserved).then(|| {
            let project = self.project.clone();
            crate::kit::icon(
                p,
                "open-terminal",
                gpui_component::IconName::SquareTerminal,
                "Open a terminal in this project",
                true,
                cx.listener(move |s, _, _, cx| {
                    if let Err(error) = s.link.open_shell(project.clone(), cx) {
                        s.launch_error = Some(error);
                        cx.notify();
                    }
                }),
            )
        });
        let (inline_sheet, side_sheet) = if inline_request {
            (asks, None)
        } else {
            (None, asks)
        };
        row()
            .size_full()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .when(!narrow_review, |layout| {
                layout.child(
                    col()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .when(detail_width < px(480.), |view| view.p_2())
                        .when(detail_width >= px(480.), |view| view.p_4())
                        .child(
                            row()
                                .gap_2()
                                .justify_between()
                                .items_start()
                                .flex_shrink_0()
                                // Text and icons share the transcript's content edges.
                                .px(px(crate::kit::SPACE_M))
                                .pb(px(SPACE_S))
                                .when(detail_width < px(500.), |header| {
                                    header.flex_col().items_start()
                                })
                                .child(
                                    col()
                                        .min_w_0()
                                        .max_w_full()
                                        .when(detail_width >= px(500.), |col| col.flex_1())
                                        .child({
                                            let project = self
                                                .project
                                                .file_name()
                                                .unwrap_or_default()
                                                .to_string_lossy()
                                                .into_owned();
                                            let leaf = title
                                                .clone()
                                                .unwrap_or_else(|| "New session".into());
                                            row()
                                                .h(px(crate::kit::ICON_SIZE))
                                                .min_w_0()
                                                .gap(px(SPACE_S))
                                                .text_size(p.text(Step::Ui))
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .text_color(rgb(p.muted))
                                                        .child(project),
                                                )
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .text_color(rgb(p.quiet()))
                                                        .child("/"),
                                                )
                                                .child(
                                                    div()
                                                        .id("current-thread-title")
                                                        .min_w_0()
                                                        .truncate()
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .text_color(rgb(p.fg))
                                                        .child(leaf.clone())
                                                        .tooltip(move |window, cx| {
                                                            gpui_component::tooltip::Tooltip::new(
                                                                leaf.clone(),
                                                            )
                                                            .build(window, cx)
                                                        }),
                                                )
                                        })
                                        .when_some(self.host_caption.as_ref(), |view, host| {
                                            let route = match &self.route.billing {
                                                ox_desk_core::runtime::Billing::Api(alias) => {
                                                    format!("API · {alias}")
                                                }
                                                ox_desk_core::runtime::Billing::Subscription => {
                                                    "subscription".into()
                                                }
                                            };
                                            let label = SharedString::from(format!(
                                                "Runs on {host} · {} · {route}",
                                                self.route.runtime.name()
                                            ));
                                            view.child(crate::platform::layout_probe::mark_element(
                                                crate::small(p, label.clone()).truncate(),
                                                &"session-host-route".into(),
                                                &label,
                                                false,
                                            ))
                                        })
                                        .when(status != "Connected", |view| {
                                            view.child(crate::small(p, status.clone()))
                                        })
                                        .when_some(
                                            self.store.as_ref().and_then(|store| {
                                                serde_json::from_value::<
                                                    ox_desk_core::memory::Attachment,
                                                >(
                                                    store
                                                        .session_mark(
                                                            store.selected()?,
                                                            "memory_attachment",
                                                        )?
                                                        .clone(),
                                                )
                                                .ok()
                                            }),
                                            |view, attachment| {
                                                view.child(crate::small(p, attachment.label()))
                                            },
                                        )
                                        .when_some(self.worker_crumb(p, cx), |view, crumb| {
                                            view.child(crumb)
                                        }),
                                )
                                .child(
                                    row()
                                        .flex_wrap()
                                        .min_w_0()
                                        .gap(px(SPACE_XS))
                                        .when_some(
                                            self.store
                                                .as_ref()
                                                .and_then(|store| store.selected())
                                                .filter(|_| self.link.remote().is_none())
                                                .map(str::to_owned),
                                            |tools, thread| {
                                                let title = title
                                                    .clone()
                                                    .unwrap_or_else(|| "this session".to_owned());
                                                tools.child(crate::kit::button(
                                                    p,
                                                    "session-open-on-phone",
                                                    "Open on phone",
                                                    false,
                                                    true,
                                                    cx.listener(move |s, _, _, cx| {
                                                        cx.emit(OpenOnPhone {
                                                            scope:
                                                                ox_desk_core::host_protocol::Scope {
                                                                    project: s.project.clone(),
                                                                    route: s.route.clone(),
                                                                },
                                                            thread: thread.clone(),
                                                            title: title.clone(),
                                                        })
                                                    }),
                                                ))
                                            },
                                        )
                                        .children(review_toggle)
                                        .children(terminal)
                                        .children(start_worker)
                                        .children(move_button)
                                        .children(controls),
                                ),
                        )
                        .when_some(
                            self.store
                                .as_ref()
                                .and_then(|s| s.selected().filter(|id| s.in_terminal(id)))
                                .map(str::to_owned),
                            |view, id| {
                                view.child(crate::kit::button(
                                    p,
                                    "resume-here",
                                    format!(
                                        "Resume here after closing stock {}",
                                        self.route.runtime.name()
                                    ),
                                    false,
                                    true,
                                    cx.listener(move |s, _, _, cx| {
                                        s.action(cx, DeskAction::ResumeHere(id.clone()))
                                    }),
                                ))
                            },
                        )
                        .when(unavailable, |view| {
                            view.child(match unsafe_folder.clone() {
                                Some(folder) => row().child(crate::kit::button(
                                    p,
                                    "fix-permissions",
                                    "Fix permissions",
                                    true,
                                    true,
                                    cx.listener(move |s, _, window, cx| {
                                        s.fix_permissions(folder.clone(), window, cx)
                                    }),
                                )),
                                None => row().child(crate::kit::button(
                                    p,
                                    "retry-connection",
                                    "Reconnect",
                                    false,
                                    true,
                                    cx.listener(|s, _, window, cx| s.reconnect(window, cx)),
                                )),
                            })
                        })
                        .when_some(
                            self.store.as_ref().and_then(Store::command_notice),
                            |d, message| {
                                d.child(
                                    row()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(message.to_owned())
                                        .child(crate::kit::copy_message(
                                            p,
                                            "copy-command-notice",
                                            message,
                                        ))
                                        .child(crate::kit::button(
                                            p,
                                            "dismiss-notice",
                                            "Dismiss",
                                            false,
                                            true,
                                            cx.listener(|s, _, _, cx| {
                                                s.action(cx, DeskAction::DismissNotice)
                                            }),
                                        )),
                                )
                            },
                        )
                        .children(transcript)
                        .map(|view| {
                            // One centred column holds the composer and what belongs to it:
                            // 680 px under the empty-state greeting, 760 px docked.
                            let dock = col()
                                .w_full()
                                .max_w(px(if ready { 680. } else { 760. }))
                                .gap(px(SPACE_S))
                                .children(activity_status.filter(|_| !source_reserved))
                                .children(self.workers_line(p, cx))
                                .when_some(self.worker_update.as_deref(), |line, update| {
                                    line.child(crate::platform::layout_probe::mark_element(
                                        crate::small(p, update.to_owned()).truncate(),
                                        &"worker-update".into(),
                                        &update.into(),
                                        false,
                                    ))
                                })
                                .children(launch)
                                .children(move_panel)
                                .children(inline_sheet)
                                .when(!source_reserved, |dock| dock.child(composer))
                                .children(usage);
                            let dock = row().w_full().justify_center().child(dock);
                            if ready {
                                view.child(self.empty_state(p, dock, cx))
                            } else {
                                view.child(dock.flex_shrink_0().pt(px(SPACE_S)))
                            }
                        }),
                )
            })
            .on_mouse_move(cx.listener(|s, event: &MouseMoveEvent, window, cx| {
                if !s.resizing_review {
                    return;
                }
                if event.pressed_button == Some(MouseButton::Left) {
                    let available = window.viewport_size().width - event.position.x;
                    let limit = f32::from(s.pane_width - px(505.)).clamp(230., 600.);
                    s.review_width = f32::from(available).clamp(230., limit) as u32;
                    s.review_layout_changed(cx);
                } else {
                    s.resizing_review = false;
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|s, _, _, _| s.resizing_review = false),
            )
            .when_some(review, |d, pane| {
                d.when(!narrow_review, |layout| {
                    layout.child(
                        div()
                            .id("review-divider")
                            .w(px(5.))
                            .h_full()
                            .cursor_col_resize()
                            .bg(rgb(p.line))
                            .hover(move |style| style.bg(rgb(p.accent)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|s, _, _, cx| {
                                    s.resizing_review = true;
                                    cx.notify();
                                }),
                            ),
                    )
                })
                .child(
                    col()
                        .h_full()
                        .w(px(self.review_width as f32))
                        .flex_shrink_0()
                        .when(narrow_review, |pane| pane.w_full().min_w_0())
                        .child(pane),
                )
            })
            .children(side_sheet)
    }
}

#[cfg(test)]
mod tests;
