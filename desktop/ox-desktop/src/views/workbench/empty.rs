//! The friendly empty state: what the main area shows before any thread is selected.
//! Composer first: the ox, the greeting and the project, then the composer itself.
use super::Workbench;
use crate::appearance::{Palette, Step};
use crate::kit::{SPACE_L, button, col, row};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::runtime::Billing;

/// Ask the host to open its folder picker, so a user with no thread can open a project.
pub(in crate::views) struct OpenProject;
impl EventEmitter<OpenProject> for Workbench {}

impl Workbench {
    /// The empty main area: the ox above "What shall we work on?", the project name and
    /// `dock`, the centred composer column. Typing in it starts the new session.
    pub(super) fn empty_state(&mut self, p: Palette, dock: Div, cx: &mut Context<Self>) -> Div {
        let project = self
            .project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.project.display().to_string());
        let billing = match &self.route.billing {
            Billing::Subscription => self.route.label(),
            Billing::Api(alias) => format!("API · {alias}"),
        };
        let host = self
            .link
            .remote()
            .map_or("This computer", |host| host.name.as_str());
        let connection = format!(
            "{project} · {} · {billing} · {host}",
            self.route.runtime.name()
        );
        let frame = self.mascot_frame;
        let offer_setup = self.store.as_ref().is_some_and(|store| {
            store.state() == &ox_desk_core::store::Connection::Ready
                && store.selected().is_none()
                && store.draft(None).trim().is_empty()
        });
        col()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .px(px(SPACE_L))
            .child(
                div()
                    .id("empty-mascot")
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("READY PLAYER ONE · Click to trot")
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.mascot_frame = (s.mascot_frame + 1) % 8;
                        cx.notify();
                    }))
                    .child(crate::activity::ox(p, frame, 4.)),
            )
            .child(
                div()
                    .mt(px(SPACE_L))
                    .text_size(p.text(Step::Title) * 1.1)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(p.fg))
                    .child("What shall we work on?"),
            )
            .child(
                div()
                    .mt(px(6.))
                    .max_w_full()
                    .text_align(TextAlign::Center)
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child(connection),
            )
            .child(div().mt(px(24.)).w_full().child(dock))
            .when(offer_setup, |view| {
                view.child(row().mt_2().child(button(
                    p, "project-setup-starter", "Get project setup help", false, true,
                    cx.listener(|s, _, _, cx| {
                        if let Some(store) = s.store.as_mut()
                            && store.selected().is_none()
                            && store.draft(None).trim().is_empty()
                        {
                            store.edit(None, "Help me set up this project. Read its setup instructions, check the prerequisites, and explain the next steps. Ask before changing my machine or installing anything.".into());
                            s.focus_editor_for = Some(None);
                            cx.notify();
                        }
                    }),
                )))
            })
            .child(
                div()
                    .mt(px(12.))
                    .font_family(crate::appearance::LABEL_FONT)
                    .text_size(p.text(Step::Label) * 0.9)
                    .text_color(rgb(p.quiet()))
                    .child("PASTE OR DROP SCREENSHOTS & FILES · @ TO MENTION · / FOR COMMANDS"),
            )
    }
}
