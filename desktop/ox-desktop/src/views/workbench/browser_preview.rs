//! A local browser viewport with explicit launch and draft evidence.
use super::Workbench;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small},
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::{Input, InputEvent, InputState};
use ox_desk_core::{
    runtime::Runtime,
    store::{Connection, Store},
};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

mod stream;

pub(super) const WIDTH: f32 = 435.;

pub(super) struct BrowserPreview {
    owner: String,
    viewport: Entity<Viewport>,
    focus: FocusHandle,
    _evidence: Subscription,
    pub(super) expanded: bool,
}

struct Viewport {
    project: PathBuf,
    label: String,
    address: Entity<InputState>,
    frame: Option<stream::Frame>,
    screenshot_supported: bool,
    capturing: bool,
    error: Option<String>,
    needs_install: bool,
    installing: Option<Task<()>>,
    stopping: Option<Task<()>>,
    reader: Option<Task<()>>,
}

struct AddScreenshot(Arc<Vec<u8>>);
impl EventEmitter<AddScreenshot> for Viewport {}

fn session_label(route: &ox_desk_core::store::Route, thread: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&(route, thread)).expect("route and thread serialize"));
    let key = format!("{:x}", digest.finalize());
    format!("thread-{}", &key[..32])
}

impl Viewport {
    fn start(&mut self, url: Option<String>, window: &Window, cx: &mut Context<Self>) {
        if self.reader.is_some() || self.installing.is_some() || self.stopping.is_some() {
            return;
        }
        self.error = None;
        let (wake, incoming) = flume::bounded(1);
        let latest = Arc::new(Mutex::new(None));
        let worker_slot = latest.clone();
        let project = self.project.clone();
        let label = self.label.clone();
        let worker = std::thread::Builder::new()
            .name("ox-browser-viewport".into())
            .spawn(move || stream::run(&project, &label, url.as_deref(), &wake, &worker_slot));
        if let Err(error) = worker {
            self.error = Some(format!("Could not start the viewport reader: {error}"));
            cx.notify();
            return;
        }
        self.reader = Some(cx.spawn_in(window, async move |view, cx| {
            while incoming.recv_async().await.is_ok() {
                let latest = latest
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(latest) = latest
                    && view
                        .update_in(cx, |viewport, window, cx| {
                            match latest {
                                stream::Update::Frame(next) => {
                                    if let Some(old) = viewport.frame.replace(next) {
                                        let _ = window.drop_image(old.image);
                                    }
                                }
                                stream::Update::NeedsInstall => {
                                    viewport.needs_install = true;
                                    viewport.reader = None;
                                }
                                stream::Update::Ended(error) => {
                                    if let Some(old) = viewport.frame.take() {
                                        let _ = window.drop_image(old.image);
                                    }
                                    viewport.error = Some(error);
                                    viewport.reader = None;
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn stop(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.frame.is_none() || self.stopping.is_some() {
            return;
        }
        self.error = None;
        let project = self.project.clone();
        let label = self.label.clone();
        let stop =
            cx.background_spawn(async move { crate::platform::browser::close(&project, &label) });
        self.stopping = Some(cx.spawn_in(window, async move |view, cx| {
            let result = stop.await;
            let _ = view.update_in(cx, |viewport, window, cx| {
                viewport.stopping = None;
                viewport.error = Some(match result {
                    Ok(()) => {
                        viewport.reader = None;
                        if let Some(old) = viewport.frame.take() {
                            let _ = window.drop_image(old.image);
                        }
                        "Browser stopped.".into()
                    }
                    Err(error) => error,
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn install(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.needs_install || self.installing.is_some() {
            return;
        }
        self.error = None;
        let install = cx.background_spawn(async { crate::platform::browser::install() });
        self.installing = Some(cx.spawn_in(window, async move |view, cx| {
            let result = install.await;
            let _ = view.update_in(cx, |viewport, window, cx| {
                viewport.installing = None;
                match result {
                    Ok(()) => {
                        viewport.needs_install = false;
                        viewport.start(None, window, cx);
                    }
                    Err(error) => {
                        viewport.error = Some(error);
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }
}

impl Render for Viewport {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        if let Some(frame) = &self.frame {
            let panel = col()
                .id("browser-preview-live")
                .min_h_0()
                .flex_1()
                .gap_2()
                .child(
                    row()
                        .flex_wrap()
                        .justify_between()
                        .gap_x_2()
                        .child(small(p, "Live · View only"))
                        .child(
                            row()
                                .flex_wrap()
                                .gap_x_2()
                                .child(button(
                                    p,
                                    "browser-preview-copy-agent",
                                    "Copy agent command",
                                    false,
                                    self.stopping.is_none(),
                                    cx.listener(|s, _, _, cx| {
                                        let command =
                                            format!("ox browser --session {} snapshot", s.label);
                                        cx.write_to_clipboard(ClipboardItem::new_string(command));
                                    }),
                                ))
                                .child(button(
                                    p,
                                    "browser-preview-stop",
                                    if self.stopping.is_some() {
                                        "Stopping browser…"
                                    } else {
                                        "Stop browser"
                                    },
                                    false,
                                    self.stopping.is_none(),
                                    cx.listener(|s, _, window, cx| s.stop(window, cx)),
                                )),
                        ),
                )
                .child(
                    col()
                        .bg(rgb(p.nav))
                        .border_1()
                        .border_color(rgb(p.line))
                        .overflow_hidden()
                        .rounded_md()
                        .min_h_0()
                        .flex_1()
                        .child(
                            img(frame.image.clone())
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        ),
                )
                .when(self.screenshot_supported, |panel| {
                    panel.child(row().justify_end().child(button(
                        p,
                        "browser-preview-evidence",
                        if self.capturing {
                            "Adding screenshot…"
                        } else {
                            "Add screenshot"
                        },
                        true,
                        !self.capturing && self.stopping.is_none(),
                        cx.listener(|s, _, _, cx| {
                            if let Some(frame) = &s.frame
                                && !s.capturing
                            {
                                cx.emit(AddScreenshot(frame.jpeg.clone()));
                            }
                        }),
                    )))
                })
                .children(self.error.as_ref().map(|error| small(p, error.clone())));
            return crate::platform::layout_probe::mark_element(
                panel,
                &"browser-preview-live".into(),
                &"Live browser viewport".into(),
                false,
            )
            .into_any_element();
        }
        if self.needs_install {
            let installing = self.installing.is_some();
            return col()
                .min_h_0()
                .flex_1()
                .justify_center()
                .gap_2()
                .child("Set up Preview")
                .child(small(
                    p,
                    "OX downloads agent-browser and uses Chrome or Chromium if available.",
                ))
                .children(self.error.as_ref().map(|error| small(p, error.clone())))
                .child(row().child(button(
                    p,
                    "browser-preview-install",
                    if installing {
                        "Installing browser component…"
                    } else {
                        "Install browser component"
                    },
                    true,
                    !installing,
                    cx.listener(|s, _, window, cx| s.install(window, cx)),
                )))
                .into_any_element();
        }
        let tip = if self.stopping.is_some() {
            small(p, "Stopping browser…")
        } else if let Some(error) = &self.error {
            small(p, error.clone())
        } else {
            small(p, "Connecting to this session’s browser…")
        };
        let address = self.address.read(cx).value();
        let busy = self.reader.is_some() || self.stopping.is_some();
        let agent_url = if address.trim().is_empty() {
            "http://127.0.0.1:3000"
        } else {
            address.trim()
        };
        let quoted = shlex::try_quote(agent_url).unwrap_or_else(|_| "http://127.0.0.1:3000".into());
        let open_command = format!("ox browser --session {} open {quoted}", self.label);
        col()
            .min_h_0()
            .flex_1()
            .justify_center()
            .gap_2()
            .child("Watch a local app")
            .child(tip)
            .child(small(p, "Dev server URL"))
            .child(crate::platform::layout_probe::mark_element(
                div().child(Input::new(&self.address).disabled(busy)),
                &"browser-preview-url".into(),
                &"Dev server URL".into(),
                !busy,
            ))
            .child(
                row()
                    .flex_wrap()
                    .gap_2()
                    .child(button(
                        p,
                        "browser-preview-start",
                        "Start Preview",
                        true,
                        !busy && !address.trim().is_empty(),
                        cx.listener(|s, _, window, cx| {
                            let address = s.address.read(cx).value();
                            s.start(Some(address.trim().to_owned()), window, cx);
                        }),
                    ))
                    .child(button(
                        p,
                        "browser-preview-copy",
                        "Copy agent command",
                        false,
                        true,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(open_command.clone()))
                        },
                    ))
                    .child(button(
                        p,
                        "browser-preview-retry",
                        "Retry connection",
                        false,
                        !busy,
                        cx.listener(|s, _, window, cx| s.start(None, window, cx)),
                    )),
            )
            .into_any_element()
    }
}

impl Workbench {
    pub(super) fn clear_browser_preview(&mut self, window: Option<&mut Window>, cx: &mut App) {
        if let Some(preview) = self.browser_preview.take()
            && let Some(image) = preview
                .viewport
                .read(cx)
                .frame
                .as_ref()
                .map(|frame| frame.image.clone())
        {
            cx.drop_image(image, window);
        }
    }

    fn stage_browser_screenshot(
        &mut self,
        generation: EntityId,
        bytes: Arc<Vec<u8>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = self.browser_owner().map(str::to_owned) else {
            return;
        };
        let Some(preview) = self
            .browser_preview
            .as_ref()
            .filter(|preview| preview.viewport.entity_id() == generation && preview.owner == owner)
        else {
            return;
        };
        if !self
            .store
            .as_ref()
            .is_some_and(|store| store.can_compose(Some(&owner)))
        {
            preview.viewport.update(cx, |viewport, cx| {
                viewport.error =
                    Some("Wait for the composer to be ready, then add the screenshot.".into());
                cx.notify();
            });
            return;
        }
        preview.viewport.update(cx, |viewport, cx| {
            viewport.capturing = true;
            viewport.error = None;
            cx.notify();
        });
        let draft = Some(owner.clone());
        *self.image_work.entry(draft.clone()).or_default() += 1;
        cx.notify();
        let convert = cx.background_spawn(async move {
            crate::platform::attachments::encode("image/jpeg", &bytes)
        });
        cx.spawn_in(window, async move |view, cx| {
            let image = convert.await;
            let _ = view.update_in(cx, |s, window, cx| {
                if let Some(count) = s.image_work.get_mut(&draft) {
                    *count -= 1;
                    if *count == 0 {
                        s.image_work.remove(&draft);
                    }
                }
                cx.notify();
                let Some(preview) = s.browser_preview.as_ref().filter(|preview| {
                    preview.viewport.entity_id() == generation
                        && preview.owner == owner
                        && s.browser_owner() == Some(owner.as_str())
                }) else {
                    return;
                };
                preview.viewport.update(cx, |viewport, cx| {
                    viewport.capturing = false;
                    cx.notify();
                });
                let Some(store) = s
                    .store
                    .as_mut()
                    .filter(|store| store.can_compose(Some(&owner)))
                else {
                    return;
                };
                // The bounded viewport JPEG is already smaller than the local paste limit.
                store.attach_image(Some(&owner), image);
                if preview.expanded || s.pane_width < px(500. + WIDTH) {
                    s.clear_browser_preview(Some(window), cx);
                }
                s.focus_editor_for = Some(Some(owner));
                cx.notify();
            });
        })
        .detach();
    }

    fn browser_owner(&self) -> Option<&str> {
        if !self.front || self.link.remote().is_some() {
            return None;
        }
        let store = self.store.as_ref()?;
        (store.state() == &Connection::Ready)
            .then(|| store.selected())
            .flatten()
    }

    pub(super) fn browser_preview_is_current(&self) -> bool {
        self.browser_preview
            .as_ref()
            .is_some_and(|preview| self.browser_owner() == Some(preview.owner.as_str()))
    }

    fn toggle_browser_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser_preview.is_some() {
            self.clear_browser_preview(Some(window), cx);
            cx.notify();
            return;
        }
        let Some(owner) = self.browser_owner().map(str::to_owned) else {
            return;
        };
        self.review.open = false;
        let project = self.project.clone();
        let label = session_label(&self.route, &owner);
        let screenshot_supported = self.route.runtime != Runtime::Antigravity;
        let viewport = cx.new(|cx| {
            let address =
                cx.new(|cx| InputState::new(window, cx).placeholder("http://127.0.0.1:3000"));
            cx.subscribe_in(
                &address,
                window,
                |s: &mut Viewport, address, event, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        let address = address.read(cx).value().trim().to_owned();
                        if !address.is_empty() {
                            s.start(Some(address), window, cx);
                        }
                    } else if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            )
            .detach();
            let mut view = Viewport {
                project,
                label,
                address,
                frame: None,
                screenshot_supported,
                capturing: false,
                error: None,
                needs_install: false,
                installing: None,
                stopping: None,
                reader: None,
            };
            view.start(None, window, cx);
            view
        });
        let evidence = cx.subscribe_in(
            &viewport,
            window,
            |s, view, event: &AddScreenshot, window, cx| {
                s.stage_browser_screenshot(view.entity_id(), event.0.clone(), window, cx);
            },
        );
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.browser_preview = Some(BrowserPreview {
            owner,
            viewport,
            focus,
            _evidence: evidence,
            expanded: false,
        });
        cx.notify();
    }

    pub(super) fn browser_preview_toggle(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.browser_owner()?;
        let p = Palette::get(cx);
        Some(
            button(
                p,
                "browser-preview-toggle",
                "Preview",
                false,
                true,
                cx.listener(|s, _, window, cx| s.toggle_browser_preview(window, cx)),
            )
            .into_any_element(),
        )
    }

    pub(super) fn browser_preview_panel(
        &self,
        narrow: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let preview = self.browser_preview.as_ref()?;
        let p = Palette::get(cx);
        let full_width = narrow || preview.expanded;
        let close = button(
            p,
            "browser-preview-close",
            if narrow {
                "Back to thread"
            } else {
                "Close Preview"
            },
            false,
            true,
            cx.listener(|s, _, window, cx| {
                s.clear_browser_preview(Some(window), cx);
                s.focus_editor_for = Some(
                    s.store
                        .as_ref()
                        .and_then(Store::selected)
                        .map(str::to_owned),
                );
                cx.notify();
            }),
        );
        let expand = (!narrow).then(|| {
            button(
                p,
                "browser-preview-expand",
                if preview.expanded {
                    "Dock Preview"
                } else {
                    "Expand Preview"
                },
                false,
                true,
                cx.listener(|s, _, window, cx| {
                    if let Some(preview) = s.browser_preview.as_mut() {
                        preview.expanded = !preview.expanded;
                        window.focus(&preview.focus, cx);
                    }
                    cx.notify();
                }),
            )
        });
        Some(
            col()
                .id("browser-preview")
                .track_focus(&preview.focus)
                .on_key_down(cx.listener(move |s, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        if let Some(preview) = s.browser_preview.as_mut()
                            && preview.expanded
                            && !narrow
                        {
                            preview.expanded = false;
                            window.focus(&preview.focus, cx);
                        } else {
                            s.clear_browser_preview(Some(window), cx);
                            s.focus_editor_for = Some(
                                s.store
                                    .as_ref()
                                    .and_then(Store::selected)
                                    .map(str::to_owned),
                            );
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .flex_shrink_0()
                .min_w_0()
                .h_full()
                .when(full_width, |panel| panel.w_full())
                .when(!full_width, |panel| panel.w(px(WIDTH)))
                .bg(rgb(p.panel))
                .border_l_1()
                .border_color(rgb(p.line))
                .p_3()
                .gap_3()
                .child(
                    row()
                        .flex_wrap()
                        .gap_2()
                        .justify_between()
                        .child("Preview")
                        .child(row().gap_2().children(expand).child(close)),
                )
                .child(preview.viewport.clone())
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::session_label;
    use ox_desk_core::store::Route;

    #[test]
    fn session_labels_are_stable_and_scoped_to_conversation_and_route() {
        let route = &Route::CODEX_SUBSCRIPTION;
        assert_eq!(
            session_label(route, "desk-0000"),
            session_label(route, "desk-0000")
        );
        assert_ne!(
            session_label(route, "desk-0000"),
            session_label(route, "desk-0001")
        );
        assert_ne!(
            session_label(route, "desk-0000"),
            session_label(&Route::codex_api("example".into()), "desk-0000")
        );
    }
}
