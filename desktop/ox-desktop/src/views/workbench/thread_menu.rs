use super::Workbench;
use crate::views::worker_sheet::LaunchKind;
use crate::{
    appearance::Palette,
    kit::{DropdownMenu, button, field},
    row,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::{Input, InputState};
use ox_desk_core::actions::Action;

pub(super) enum ThreadEdit {
    Name(String, Entity<InputState>),
    Archive(String),
    /// End a busy thread (a running turn, approval or worker), after confirmation.
    End(String),
}
impl ThreadEdit {
    fn owner(&self) -> &str {
        match self {
            Self::Name(id, _) | Self::Archive(id) | Self::End(id) => id,
        }
    }
}

/// Notify `Host` that the user renamed a thread from the workbench header menu so
/// `sidebar.json` records the custom title.
pub(in crate::views) struct ThreadRenamed {
    pub id: String,
    pub name: String,
}
impl EventEmitter<ThreadRenamed> for Workbench {}

impl Workbench {
    fn apply_thread_edit(&mut self, cx: &mut Context<Self>) {
        let action = match &self.thread_edit {
            Some(ThreadEdit::Name(id, input)) => {
                let name = input.read(cx).value();
                if name.trim().is_empty() {
                    return;
                }
                cx.emit(ThreadRenamed {
                    id: id.clone(),
                    name: name.trim().to_owned(),
                });
                Action::RenameThread(id.clone(), name.to_string())
            }
            Some(ThreadEdit::Archive(id)) => Action::ArchiveThread(id.clone()),
            Some(ThreadEdit::End(id)) => Action::ForceEndSession(id.clone()),
            None => return,
        };
        self.thread_edit = None;
        self.action(cx, action);
    }
    pub(crate) fn request_archive(&mut self, id: String, cx: &mut Context<Self>) {
        if self.store.as_ref().is_some_and(|store| {
            store.can_manage_thread(&id)
                && store.can_edit_thread_metadata()
                && !store.is_native_live(&id)
        }) {
            self.thread_edit = Some(ThreadEdit::Archive(id));
            cx.notify();
        }
    }
    /// End a thread's live process. A busy thread (running turn, pending approval or
    /// running worker) asks first, inline, because ending it cancels that work.
    pub(crate) fn request_end(&mut self, id: String, cx: &mut Context<Self>) {
        let busy = self
            .store
            .as_ref()
            .is_some_and(|store| store.thread_busy(&id))
            || self.running_workers(&id, cx) > 0;
        if busy {
            self.thread_edit = Some(ThreadEdit::End(id));
            cx.notify();
        } else {
            self.action(cx, Action::EndSession(id));
        }
    }
    pub(super) fn thread_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.store.as_ref()?.selected()?.to_owned();
        let store = self.store.as_ref()?;
        let manage = store.can_manage_thread(&id);
        let can_end = store.can_end_session(&id);
        let can_launch =
            store.state() == &ox_desk_core::store::Connection::Ready && !store.in_terminal(&id);
        if !manage && !can_end && !can_launch {
            self.thread_edit = None;
            return None;
        }
        if self
            .thread_edit
            .as_ref()
            .is_some_and(|edit| edit.owner() != id)
        {
            self.thread_edit = None;
        }
        let p = Palette::get(cx);
        if let Some(edit) = &self.thread_edit {
            let (input, label) = match edit {
                ThreadEdit::Name(_, state) => (Some(Input::new(state)), "Save name"),
                ThreadEdit::Archive(_) => (None, "Archive this thread"),
                ThreadEdit::End(_) => (None, "End running session"),
            };
            return Some(
                row()
                    .flex_wrap()
                    .min_w_0()
                    .gap_2()
                    .when_some(input, |r, i| r.child(div().w(px(200.)).min_w_0().child(i)))
                    .child(button(
                        p,
                        "apply-thread-edit",
                        label,
                        true,
                        true,
                        cx.listener(|s, _, _, cx| s.apply_thread_edit(cx)),
                    ))
                    .child(crate::kit::toolbar_button(
                        p,
                        "cancel-thread-edit",
                        "Cancel",
                        true,
                        cx.listener(|s, _, _, cx| {
                            s.thread_edit = None;
                            cx.notify();
                        }),
                    ))
                    .into_any_element(),
            );
        }
        let view = cx.weak_entity();
        let edit_metadata = manage && store.can_edit_thread_metadata();
        // Codex refuses Archive from a client that does not hold the thread.
        let archivable = edit_metadata && !store.is_native_live(&id);
        let can_generate = manage && store.can_generate_title(&id);
        Some(
            crate::kit::icon(
                p,
                "thread-menu",
                gpui_component::IconName::Ellipsis,
                "Thread actions",
                true,
                |_, _, _| {},
            )
            .dropdown_menu(move |menu, _, _| {
                let rename = view.clone();
                let owner = id.clone();
                let target = id.clone();
                let fork = view.clone();
                let archive = view.clone();
                let find = view.clone();
                let archived = id.clone();
                let naming = view.clone();
                let unnamed = id.clone();
                let end = view.clone();
                let ended = id.clone();
                let handoff = view.clone();
                menu.when(edit_metadata, |menu| menu.item(
                    crate::kit::menu_item("Rename thread").on_click(move |_, window, cx| {
                        let _ = rename.update(cx, |s, cx| {
                            let Some(store) = &s.store else { return };
                            if !store.can_manage_thread(&owner) {
                                return;
                            }
                            let title = store.catalog_row(&owner).map(|r| r.title()).unwrap_or("");
                            let input = field(title, "Thread name", false, window, cx);
                            let focus = input.read(cx).focus_handle(cx);
                            window.on_next_frame(move |w,cx|focus.focus(w,cx));
                            cx.subscribe(&input, |s, input, event:&gpui_component::input::InputEvent,cx|{
                                if matches!(event, gpui_component::input::InputEvent::PressEnter{..})
                                    && matches!(&s.thread_edit,Some(ThreadEdit::Name(_,current)) if *current == input)
                                { s.apply_thread_edit(cx); }
                            }).detach();
                            s.thread_edit = Some(ThreadEdit::Name(owner.clone(), input));
                            cx.notify();
                        });
                    }),
                ))
                .when(can_generate, |menu| menu.item(
                    crate::kit::menu_item("Generate title").on_click(move |_, _, cx| {
                        let _ = naming.update(cx, |s, cx| s.action(cx, Action::GenerateTitle(unnamed.clone())));
                    }),
                ))
                .when(manage, |menu| menu.item(crate::kit::menu_item("Find in transcript").on_click(move |_,w,cx| {
                    let _ = find.update(cx,|s,cx|s.find_in_transcript(w,cx));
                })))
                .map(|mut menu| {
                    if !manage {
                        return menu;
                    }
                    for (title, expand) in [("Expand all details",Some(true)),("Fold all details",Some(false)),("Show / hide thinking",None)] {
                        let view = view.clone();
                        menu = menu.item(crate::kit::menu_item(title).on_click(move |_,_,cx| {
                            let _ = view.update(cx,|s,cx|s.transcript_folding(expand,cx));
                        }));
                    }
                    menu
                })
                .when(manage, |menu| menu.item(crate::kit::menu_item("Fork thread").on_click(move |_, _, cx| {
                    let _ =
                        fork.update(cx, |s, cx| s.action(cx, Action::ForkThread(target.clone())));
                })))
                .when(archivable, |menu|menu.item(
                    crate::kit::menu_item("Archive thread…").on_click(move |_, _, cx| {
                        let _ = archive.update(cx, |s, cx| {
                            if s.store
                                .as_ref()
                                .is_some_and(|store| store.can_manage_thread(&archived))
                            {
                                s.thread_edit = Some(ThreadEdit::Archive(archived.clone()));
                                cx.notify();
                            }
                        });
                    }),
                ))
                .when(can_launch, |menu| menu.item(
                    crate::kit::menu_item("Continue in…").on_click(move |_, _, cx| {
                        let _ = handoff.update(cx, |s, cx| s.request_launch(LaunchKind::Handoff, cx));
                    }),
                ))
                .when(can_end, |menu| menu.item(
                    crate::kit::menu_item("End session").on_click(move |_, _, cx| {
                        let _ = end.update(cx, |s, cx| s.request_end(ended.clone(), cx));
                    }),
                ))
            })
            .into_any_element(),
        )
    }
}
