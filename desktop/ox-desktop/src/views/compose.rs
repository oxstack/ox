use super::workbench::Workbench;
use crate::appearance::Step;
use crate::kit::{DropdownMenu, PopupMenuItem};
use crate::{appearance::Palette, col, kit::button, row};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    button::ButtonVariants,
    input::{InputEvent, TextareaState},
};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::runtime::{Access, Runtime};

fn native_command(runtime: Runtime, text: &str) -> bool {
    matches!(runtime, Runtime::Codex | Runtime::Claude) && text.trim_start().starts_with('/')
}

pub(super) fn typed_launch(
    owner: &str,
    text: &str,
    runtime: Runtime,
) -> Option<Result<DeskAction, String>> {
    use ox_desk_core::modes::{Level, ModeRequest};
    let yolo = ModeRequest::Level(Level::Yolo);
    if let Some(result) = ox_desk_core::handoff::HandoffStart::parse_command(owner, text) {
        return Some(result.map(|mut start| {
            start.yolo_confirmed = start.mode == yolo;
            DeskAction::StartHandoff(start)
        }));
    }
    if !runtime.supports_workers() {
        return None;
    }
    ox_desk_core::origin::WorkerStart::parse_command(owner, text).map(|result| {
        result.map(|mut start| {
            start.yolo_confirmed = start.permission == Some(yolo);
            DeskAction::StartWorker(start)
        })
    })
}

impl Workbench {
    fn send_input(&mut self, owner: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = &self.store else {
            return;
        };
        if self.image_work.contains_key(&owner.map(str::to_owned)) || store.selected() != owner {
            return;
        }
        if !store.draft_images(owner).is_empty()
            && !store.draft(owner).trim_start().starts_with('/')
            && (!store.has_command_schema()
                || store.commands_loading()
                || store.image_model_warning(owner).is_some())
        {
            return;
        }
        let id = owner.map(str::to_owned);
        if store.draft(owner).trim() == "/model" {
            self.action(cx, DeskAction::DraftEdit(id, String::new()));
            self.open_model_picker(super::route_chip::Kind::Model, window, cx);
            return;
        }
        if let Some(result) =
            owner.and_then(|id| typed_launch(id, store.draft(owner), self.route.runtime))
        {
            match result {
                Ok(action) => {
                    let remote =
                        matches!(&action, DeskAction::StartHandoff(start) if start.host.is_some());
                    self.action(cx, action);
                    if !remote {
                        self.action(cx, DeskAction::DraftEdit(id, String::new()));
                    }
                }
                Err(message) => {
                    self.launch_error = Some(message);
                    cx.notify();
                }
            }
            return;
        }
        let action = if native_command(self.route.runtime, store.draft(owner)) {
            if !store.has_command_schema() || store.commands_loading() {
                self.pending_command = Some((id, store.draft(owner).to_owned()));
                cx.notify();
                return;
            }
            DeskAction::ExecuteCommand(id)
        } else {
            DeskAction::Submit(id)
        };
        self.action(cx, action);
    }

    pub(super) fn finish_command_discovery(&mut self, cx: &mut Context<Self>) {
        let Some(store) = &self.store else { return };
        if !store.has_command_schema() || store.commands_loading() {
            return;
        }
        if let Some((owner, text)) = self.pending_command.take()
            && self.front
            && store.selected() == owner.as_deref()
            && store.draft(owner.as_deref()) == text
        {
            self.action(cx, DeskAction::ExecuteCommand(owner));
        }
    }

    pub(super) fn composer(
        &mut self,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        if !cx.has_active_drag() {
            self.file_hover = None;
        }
        let p = Palette::get(cx);
        let Some(store) = &self.store else {
            return col();
        };
        let model_controls = self.model_controls(width, cx);
        let selected = store.selected().map(str::to_owned);
        let key = selected.as_deref();
        let antigravity = self.route.runtime == Runtime::Antigravity;
        let draft = store.draft(key);
        let busy = store.sending(key);
        let running = key
            .and_then(|id| store.transcript(id))
            .is_some_and(|t| t.active_turn().is_some());
        let enabled = store.can_compose(key)
            && !self.image_work.contains_key(&selected)
            && (store.draft_images(key).is_empty()
                || draft.trim_start().starts_with('/')
                || (store.has_command_schema()
                    && !store.commands_loading()
                    && store.image_model_warning(key).is_none()))
            && (!draft.trim().is_empty() || !store.draft_images(key).is_empty());
        let enter_sends = cx
            .global::<crate::preferences::behavior::Active>()
            .enter_sends;
        if self.enter_sends != enter_sends {
            for editor in self.editors.values() {
                editor.update(cx, |e, cx| e.set_submit_on_enter(enter_sends, cx));
            }
            self.enter_sends = enter_sends;
        }
        let editor = self.editors.entry(selected.clone()).or_insert_with(|| {
            let owner = selected.clone();
            let placeholder = if owner
                .as_deref()
                .is_some_and(|id| store.origin(id).is_some())
            {
                "Send an instruction to this worker…".to_owned()
            } else {
                // The model and route show in the chips below; the placeholder teaches.
                "Ask anything · @ files · / commands".to_owned()
            };
            let editor = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 6)
                    .submit_on_enter(enter_sends)
                    .placeholder(placeholder)
            });
            cx.subscribe_in(
                &editor,
                window,
                move |s, field, event, window, cx| match event {
                    InputEvent::Change => {
                        if let Some(store) = &mut s.store {
                            let text = field.read(cx).value();
                            if store.draft(owner.as_deref()) != text.as_ref() {
                                let _ = store.dispatch(DeskAction::DraftEdit(
                                    owner.clone(),
                                    text.to_string(),
                                ));
                                cx.notify();
                            }
                        }
                        s.search_mention(owner.clone(), field.clone(), cx);
                    }
                    InputEvent::PressEnter { secondary, shift }
                        if field.update(cx, |input, cx| {
                            input.marked_text_range(window, cx).is_none()
                        }) && (*secondary
                            || (!shift
                                && cx
                                    .global::<crate::preferences::behavior::Active>()
                                    .enter_sends)) =>
                    {
                        s.send_input(owner.as_deref(), window, cx)
                    }
                    _ => (),
                },
            )
            .detach();
            editor
        });
        if editor.read(cx).value().as_ref() != draft {
            editor.update(cx, |e, cx| {
                // The IME owns its temporary preedit until the platform commits it.
                if e.marked_text_range(window, cx).is_none() {
                    e.set_value(draft.to_owned(), window, cx);
                }
            });
        }
        let native_command = native_command(self.route.runtime, draft);
        let is_command = native_command || draft.trim() == "/model";
        if self.focus_editor_for.as_ref() == Some(&selected) {
            self.focus_editor_for = None;
            editor.read(cx).focus_handle(cx).focus(window, cx);
        }
        let caps = store.command_caps(key);
        let discovering =
            native_command && (!store.has_command_schema() || store.commands_loading());
        let term = is_command
            && draft.trim() != "/model"
            && matches!(
                store.parse_command(key, draft),
                Some(ox_desk_core::slash::Action::Terminal)
            );
        let enabled = enabled && !discovering;
        let send = if discovering {
            "Finding commands…"
        } else if term {
            "Open in terminal ↗"
        } else if is_command {
            "Run command"
        } else if busy {
            "Sending…"
        } else if running && !antigravity {
            "Steer"
        } else {
            "Send"
        };
        let effective = store.applied_access(key);
        let mode = if let Some(id) = key {
            effective.or_else(|| {
                store
                    .session_mark(id, "resume_access")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
            })
        } else {
            effective.or_else(|| store.next_access())
        };
        let granted = key
            .and_then(|id| store.session_mark(id, "yolo_grant"))
            .is_some_and(|v| v == true);
        // A live grant shows on the chip whatever the session's current mode is.
        let value = match (key, mode) {
            (Some(_), Some(m)) if granted && m.bypasses_permissions() => {
                "YOLO + workers".to_owned()
            }
            (Some(_), Some(m)) if granted => {
                format!("{} · YOLO allowed for workers", permission_value(m))
            }
            (Some(_), None) if granted => "YOLO allowed for workers".to_owned(),
            (_, Some(m)) => permission_value(m),
            _ => default_permission(
                self.route.runtime,
                cx.try_global::<crate::preferences::behavior::Active>()
                    .map(|a| a.0)
                    .unwrap_or_default(),
            ),
        };
        let access_hint = if key.is_none() {
            format!("Permissions for the next thread: {value}")
        } else {
            format!("Permissions: {value}")
        };
        if effective.is_some_and(Access::bypasses_permissions)
            && p.motion
            && self.front
            && window.is_visible()
            && window.is_window_active()
        {
            self.fire
                .get_or_insert_with(|| cx.new(crate::activity::Runner::burning));
        } else {
            self.fire = None;
        }
        let owner = selected.clone();
        let view = cx.weak_entity();
        let presets = key
            .and_then(|id| store.session_mark(id, "access_choices"))
            .and_then(|choices| serde_json::from_value::<Vec<Access>>(choices.clone()).ok())
            .unwrap_or_else(|| Access::choices(self.route.runtime).to_vec());
        // The grant menu (and its Revoke) follows the grant, not the current mode, so a
        // grant stays visible and revocable after the session drops back to Default.
        let grant_offer =
            key.is_some() && (granted || effective.is_some_and(Access::bypasses_permissions));
        let access = crate::kit::chip(
            p,
            "access-preset",
            "",
            value,
            false,
            key.map_or_else(|| store.can_send(None), |id| store.can_change_access(id)),
            |_, _, _| {},
        )
        .tooltip(access_hint)
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
            for &preset in &presets {
                let id = owner.clone();
                let view = view.clone();
                menu = menu.item(
                    crate::kit::menu_item(preset.label())
                        .checked(mode == Some(preset))
                        .on_click(move |_, _, cx| {
                            let _ = view.update(cx, |s, cx| {
                                if s.store.as_ref().and_then(|s| s.selected()) == id.as_deref() {
                                    let action = match &id {
                                        Some(id) => DeskAction::SetAccess(id.clone(), preset),
                                        None => DeskAction::SetNextAccess(preset),
                                    };
                                    s.action(cx, action);
                                }
                            });
                        }),
                );
            }
            if grant_offer {
                // The grant and its revoke share one group: the consequence is said in the menu.
                let id = owner.clone();
                let view = view.clone();
                menu = menu
                    .separator()
                    .item(PopupMenuItem::label("Workers and handoffs"));
                if granted {
                    menu = menu
                        .item(PopupMenuItem::label(
                            "YOLO is allowed for workers and handoffs started here.",
                        ))
                        .item(
                            crate::kit::menu_item("Revoke YOLO for workers and handoffs").on_click(
                                move |_, _, cx| {
                                    let _ = view.update(cx, |s, cx| {
                                        if let Some(id) = &id {
                                            s.action(cx, DeskAction::YoloGrant(id.clone(), false));
                                        }
                                    });
                                },
                            ),
                        );
                } else {
                    menu = menu
                        .item(PopupMenuItem::label(
                            "Lets workers and handoffs start in YOLO without asking.",
                        ))
                        .item(
                            crate::kit::menu_item("Allow YOLO for workers and handoffs").on_click(
                                move |_, _, cx| {
                                    let _ = view.update(cx, |s, cx| {
                                        if let Some(id) = &id {
                                            s.action(cx, DeskAction::YoloGrant(id.clone(), true));
                                        }
                                    });
                                },
                            ),
                        );
                }
            }
            menu
        });
        let handoff_notice = key
            .and_then(|id| ox_desk_core::handoff::HandoffStart::parse_command(id, draft))
            .and_then(Result::ok)
            .and_then(|start| ox_desk_core::handoff::provider_notice(&self.route, &start.route()));
        let suggestions = if is_command && !discovering {
            ox_desk_core::slash::completions(draft, &caps)
        } else {
            vec![]
        };
        // One fixed 28 px submit slot: ↑ sends (or steers, or runs a command); while a
        // turn runs and nothing is typed it becomes ■ Stop, so nothing ever moves.
        let stops = running
            && selected.is_some()
            && (antigravity || (draft.trim().is_empty() && store.draft_images(key).is_empty()));
        let submit = if stops {
            let stop_owner = selected.clone();
            crate::kit::icon_button(
                p,
                "stop-turn",
                "",
                format!("Stop · {}", stop_key()),
                cx.listener(move |s, _, _, cx| {
                    if let Some(id) = stop_owner.clone() {
                        s.action(cx, DeskAction::Stop(id))
                    }
                }),
            )
            .bg(rgb(p.raised))
            .child(gpui::div().size(px(10.)).bg(rgb(p.fg)))
        } else {
            let send_owner = selected.clone();
            crate::kit::icon(
                p,
                "send-message",
                gpui_component::IconName::ArrowUp,
                send,
                enabled,
                cx.listener(move |s, _, window, cx| {
                    s.send_input(send_owner.as_deref(), window, cx)
                }),
            )
            .when(enabled, |b| b.primary())
            .when(!enabled, |b| b.bg(rgb(p.raised)).text_color(rgb(p.quiet())))
        };
        let native = key.filter(|id| store.is_native_live(id)).map(str::to_owned);
        let editor_for_menu = editor.clone();
        let attach_owner = selected.clone();
        let can_attach = store.can_compose(key) && !self.image_work.contains_key(&selected);
        let attach_button = crate::kit::icon(
            p,
            "attach-file",
            gpui_component::IconName::Plus,
            "Attach image or file",
            can_attach,
            cx.listener(move |s, _, window, cx| {
                s.prompt_attach_files(attach_owner.clone(), window, cx)
            }),
        )
        .size(px(crate::kit::CHIP_HEIGHT));
        let hovering_files = self
            .file_hover
            .as_ref()
            .is_some_and(|(id, paths)| *id == selected && !paths.is_empty());
        col()
            .flex_shrink_0()
            .gap(px(crate::kit::SPACE_M))
            .pt(px(crate::kit::SPACE_M))
            .pb(px(crate::kit::SPACE_M))
            .px(px(crate::kit::SPACE_M))
            .rounded(px(crate::kit::OVERLAY_RADIUS))
            .border_1()
            .border_color(rgb(if hovering_files { p.accent } else { p.line }))
            .bg(rgb(if hovering_files { p.raised } else { p.panel }))
            .when(hovering_files, |panel| {
                panel.child(
                    row()
                        .gap(px(crate::kit::SPACE_S))
                        .child(
                            div()
                                .font_family(crate::appearance::LABEL_FONT)
                                .text_size(p.text(Step::Label))
                                .text_color(rgb(p.accent))
                                .child("DROP FILES · INSERT CARTRIDGE"),
                        )
                        .child(
                            div()
                                .text_size(p.text(Step::Meta))
                                .text_color(rgb(p.muted))
                                .child("Images attach directly; other files attach as @path"),
                        ),
                )
            })
            .when(!suggestions.is_empty(), |panel| {
                let mut choices = row().gap_1().flex_wrap();
                for (i, (text, hint)) in suggestions.into_iter().enumerate() {
                    let input = editor_for_menu.clone();
                    choices = choices.child(
                        button(
                            p,
                            ("command-choice", i),
                            text.clone(),
                            false,
                            true,
                            cx.listener(move |_, _, window, cx| {
                                input.update(cx, |edit, cx| {
                                    edit.set_value(text.clone(), window, cx)
                                });
                                input.read(cx).focus_handle(cx).focus(window, cx);
                            }),
                        )
                        .tooltip(hint),
                    );
                }
                panel.child(choices)
            })
            .when_some(native, |panel, id| {
                panel.child(
                    col()
                        .id("native-live-notice")
                        .gap_2()
                        .child(
                            div()
                                .text_size(p.text(Step::Meta))
                                .text_color(rgb(p.muted))
                                .child(store.native_live_hint()),
                        )
                        .child(row().child(button(
                            p,
                            "continue-native",
                            "Continue in a new OX session".to_owned(),
                            false,
                            true,
                            cx.listener(move |s, _, _, cx| {
                                s.action(cx, DeskAction::ForkThread(id.clone()))
                            }),
                        ))),
                )
            })
            .when_some(handoff_notice, |panel, notice| {
                panel.child(
                    div()
                        .id("handoff-provider-notice")
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.muted))
                        .child(notice),
                )
            })
            // Marked for the test probe so window tests focus the composer by id.
            .child(crate::platform::layout_probe::mark_element(
                div().w_full().child(super::mention_menu::composer_input(
                    editor.clone(),
                    selected.clone(),
                    store,
                    self.image_work.contains_key(&selected),
                    self.mention_index,
                    cx,
                )),
                &gpui::ElementId::from("composer"),
                &gpui::SharedString::from("Composer"),
                true,
            ))
            .child(
                row()
                    .items_center()
                    .gap(px(crate::kit::SPACE_XS))
                    .child(
                        row()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .items_center()
                            .gap(px(crate::kit::SPACE_XS))
                            // Chip text lines up with the textarea's text inset.
                            .pl(px(2.))
                            .when(!antigravity, |r| r.child(attach_button))
                            .children(model_controls)
                            .child(access)
                            .when(effective.is_some_and(Access::bypasses_permissions), |r| {
                                r.child(match &self.fire {
                                    Some(fire) => fire.clone().into_any_element(),
                                    None => crate::activity::flame(p, 0).into_any_element(),
                                })
                            }),
                    )
                    .child(submit),
            )
    }
}

/// What a new session starts in when no permission mode has been chosen for it:
/// the "New sessions start in" setting, or the runtime's own default.
fn default_permission(
    runtime: ox_desk_core::runtime::Runtime,
    behavior: ox_desk_core::desktop_behavior::Behavior,
) -> String {
    use ox_desk_core::runtime::Runtime;
    let start = match runtime {
        Runtime::Claude => behavior.claude_start,
        Runtime::Codex => behavior.codex_start,
        Runtime::Antigravity => behavior.antigravity_start,
        Runtime::OpenCode => None,
    };
    match (runtime, start) {
        (_, Some(mode)) => permission_value(mode),
        (Runtime::Claude, None) => "Claude Code default".to_owned(),
        (Runtime::Codex, None) => "Configured in Codex".to_owned(),
        (Runtime::Antigravity, None) => "Antigravity default".to_owned(),
        (Runtime::OpenCode, None) => "OpenCode permission prompts".to_owned(),
    }
}

/// The permission chip's value: the mode alone, with Claude's "Default" spelled out.
fn permission_value(mode: Access) -> String {
    match mode {
        Access::Default => "Ask before edits".to_owned(),
        other => other.label().to_owned(),
    }
}

fn stop_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘⇧X"
    } else {
        "Ctrl Shift X"
    }
}
