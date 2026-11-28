//! The review is always addressed by the selected conversation and turn.
use super::Workbench;
use crate::appearance::Step;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small},
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::input::RopeExt;
use ox_desk_core::actions::Action;
use std::collections::HashMap;
mod patch;
use patch::{Cursor, Excerpt, Selection};

type FileKey = (String, String, String);

struct FileSelection {
    key: FileKey,
    range: Option<Selection>,
}

#[derive(Default)]
pub(super) struct ReviewPane {
    pub(super) open: bool,
    pinned: HashMap<String, String>,
    expanded: HashMap<FileKey, Vec<Cursor>>,
    selection: Option<FileSelection>,
}

impl ReviewPane {
    fn clear_range(&mut self, key: &FileKey) {
        if let Some(selection) = self.selection.as_mut().filter(|s| &s.key == key) {
            selection.range = None;
        }
    }
}

impl Workbench {
    pub(super) fn review_layout_changed(&self, cx: &mut Context<Self>) {
        cx.emit(super::CatalogChanged);
    }
    pub(in crate::views) fn toggle_review(&mut self, cx: &mut Context<Self>) {
        if self
            .store
            .as_ref()
            .is_some_and(|s| s.selected().and_then(|id| s.transcript(id)).is_some())
        {
            self.review.open = !self.review.open;
            self.review.selection = None;
            self.review_layout_changed(cx);
            cx.notify();
        }
    }
    pub(super) fn review_toggle(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let owner = store.selected()?;
        store.transcript(owner)?;
        let p = Palette::get(cx);
        let open = self.review.open;
        Some(
            crate::kit::icon(
                p,
                "toggle-review",
                if open {
                    gpui_component::IconName::PanelRightClose
                } else {
                    gpui_component::IconName::PanelRightOpen
                },
                "Changes & plan",
                true,
                cx.listener(|s, _, _, cx| {
                    s.toggle_review(cx);
                }),
            )
            .when(open, |b| b.bg(rgb(p.hover)).text_color(rgb(p.fg)))
            .into_any_element(),
        )
    }

    pub(super) fn review_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.review.open {
            return None;
        }
        let store = self.store.as_ref()?;
        let owner = store.selected()?;
        let transcript = store.transcript(owner)?;
        let p = Palette::get(cx);
        let mut pane =
            col()
                .id(SharedString::from(format!("turn-review-{owner}")))
                .overflow_y_scroll()
                .h_full()
                .min_w_0()
                .gap_3()
                .p_3()
                .bg(rgb(p.panel))
                .text_color(rgb(p.fg))
                .border_l_1()
                .border_color(rgb(p.line))
                .child(row().justify_between().child("Changes & plan").child(
                    crate::kit::icon_button(
                        p,
                        "close-review",
                        "×",
                        "Close changes and plan",
                        cx.listener(|s, _, _, cx| {
                            s.review.open = false;
                            s.review.selection = None;
                            s.review_layout_changed(cx);
                            cx.notify();
                        }),
                    ),
                ));
        let children = store
            .threads()
            .iter()
            .filter(|r| r.parent_thread_id.as_deref() == Some(owner));
        for (i, child) in children.enumerate() {
            if i == 0 {
                pane = pane.child(div().text_size(p.text(Step::Title)).child("Workers"));
            }
            let id = child.id.clone();
            let title = child.title().to_owned();
            pane = pane.child(button(
                p,
                ("review-worker", i),
                title,
                false,
                true,
                cx.listener(move |s, _, _, cx| s.action(cx, Action::SelectThread(id.clone()))),
            ));
        }
        if transcript.has_older() {
            let id = owner.to_owned();
            pane = pane.child(button(
                p,
                "older-review-turns",
                "Load earlier turns",
                false,
                !transcript.is_loading(),
                cx.listener(move |s, _, _, cx| s.action(cx, Action::LoadOlder(id.clone()))),
            ));
        }
        let turns = transcript.reviews.turns().collect::<Vec<_>>();
        let selected = self
            .review
            .pinned
            .get(owner)
            .and_then(|pin| turns.iter().position(|(id, _)| id == pin))
            .unwrap_or(0);
        let Some(&(turn, review)) = turns.get(selected) else {
            return Some(
                pane.child(small(p, "No changes or plan in the loaded turns."))
                    .into_any_element(),
            );
        };
        if turns.len() > 1 {
            let navigation = |id: &'static str, label: &'static str, target: Option<usize>| {
                let thread = owner.to_owned();
                let pin = target.filter(|&i| i != 0).map(|i| turns[i].0.to_owned());
                button(
                    p,
                    id,
                    label,
                    false,
                    target.is_some(),
                    cx.listener(move |s, _, _, cx| {
                        s.review.selection = None;
                        match &pin {
                            Some(turn) => {
                                s.review.pinned.insert(thread.clone(), turn.clone());
                            }
                            None => {
                                s.review.pinned.remove(&thread);
                            }
                        }
                        cx.notify();
                    }),
                )
            };
            pane = pane.child(
                row()
                    .gap_2()
                    .child(navigation(
                        "earlier-review",
                        "Earlier",
                        (selected + 1 < turns.len()).then_some(selected + 1),
                    ))
                    .child(small(
                        p,
                        format!("{} of {}", turns.len() - selected, turns.len()),
                    ))
                    .child(navigation("later-review", "Later", selected.checked_sub(1))),
            );
        }
        if !review.has_content() {
            return Some(
                pane.child(small(p, "No changes or plan in this turn."))
                    .into_any_element(),
            );
        }
        if !review.steps.is_empty() || review.explanation.is_some() || !review.plan_text.is_empty()
        {
            pane = pane.child(div().text_size(p.text(Step::Title)).child("Plan"));
            if let Some(text) = &review.explanation {
                pane = pane.child(text.clone());
            }
            for step in &review.steps {
                let (label, color) = match step.status.as_str() {
                    "completed" => ("Done", p.accent),
                    "inProgress" | "in_progress" => ("In progress", p.fg),
                    "pending" => ("Pending", p.muted),
                    other => (other, p.muted),
                };
                pane = pane.child(
                    col()
                        .child(small(p, label.to_owned()).text_color(rgb(color)))
                        .child(step.text.clone()),
                );
            }
            if !review.plan_text.is_empty() {
                pane = pane.child(review.plan_text.clone());
            }
        }
        let files = review.file_groups();
        if !files.is_empty() {
            let thread = owner.to_owned();
            let turn = turn.to_owned();
            pane = pane.child(
                row()
                    .justify_between()
                    .child(
                        div()
                            .text_size(p.text(Step::Title))
                            .child(format!("Files ({})", files.len())),
                    )
                    .child(button(
                        p,
                        "copy-turn-patch",
                        "Copy turn patch",
                        false,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            if let Some(review) = s
                                .store
                                .as_ref()
                                .and_then(|store| store.transcript(&thread))
                                .and_then(|t| t.reviews.get(&turn))
                            {
                                cx.write_to_clipboard(ClipboardItem::new_string(review.patch()));
                            }
                        }),
                    )),
            );
        }
        let mut selection_bar = None;
        for (index, (path, patches)) in files.into_iter().enumerate() {
            let key = (owner.to_owned(), turn.to_owned(), path.to_owned());
            let pages = self.review.expanded.get(&key);
            let expanded = pages.is_some();
            let toggle_key = key.clone();
            let path_label = std::path::Path::new(path)
                .strip_prefix(&self.project)
                .ok()
                .and_then(|p| p.to_str())
                .unwrap_or(path);
            let bodies = patches.iter().map(|p| p.diff.clone()).collect::<Vec<_>>();
            let mut file = col()
                .gap_1()
                .border_t_1()
                .border_color(rgb(p.line))
                .pt_2()
                .child(
                    row()
                        .gap_2()
                        .flex_wrap()
                        .justify_between()
                        .child(
                            button(
                                p,
                                ("review-file", index),
                                format!("{} {path_label}", if expanded { "▾" } else { "▸" }),
                                false,
                                true,
                                cx.listener(move |s, _, _, cx| {
                                    if s.review.expanded.remove(&toggle_key).is_none() {
                                        s.review.expanded.insert(toggle_key.clone(), Vec::new());
                                    } else if s
                                        .review
                                        .selection
                                        .as_ref()
                                        .is_some_and(|selection| selection.key == toggle_key)
                                    {
                                        s.review.selection = None;
                                    }
                                    cx.notify();
                                }),
                            )
                            .min_w_0()
                            .tooltip(path.to_owned()),
                        )
                        .child(button(
                            p,
                            ("copy-file-patch", index),
                            "Copy patch",
                            false,
                            bodies.iter().any(|s| !s.is_empty()),
                            move |_, _, cx| {
                                let mut text = String::new();
                                for body in &bodies {
                                    if !text.is_empty() && !text.ends_with('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(body);
                                }
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            },
                        )),
                );
            if let Some(pages) = pages {
                let page = patch::page(&patches, pages.last().cloned().unwrap_or_default());
                let selecting = self.review.selection.as_ref().filter(|s| s.key == key);
                if let Some(selection) = selecting {
                    let excerpt = selection
                        .range
                        .as_ref()
                        .and_then(|range| range.excerpt(&patches, &page.lines));
                    selection_bar = Some(Self::review_selection_bar(
                        p,
                        &key,
                        path_label,
                        selection.range.is_some(),
                        excerpt,
                        cx,
                    ));
                }
                if page.lines.iter().any(|line| line.location.is_some()) {
                    let selection_key = key.clone();
                    file = file.child(
                        row().child(
                            crate::kit::toolbar_button(
                                p,
                                ("select-review-lines", index),
                                if selecting.is_some() {
                                    "Selecting lines"
                                } else {
                                    "Select lines"
                                },
                                true,
                                cx.listener(move |s, _, _, cx| {
                                    if s.store.as_ref().and_then(|store| store.selected())
                                        != Some(selection_key.0.as_str())
                                    {
                                        return;
                                    }
                                    if s.review
                                        .selection
                                        .as_ref()
                                        .is_some_and(|selection| selection.key == selection_key)
                                    {
                                        s.review.selection = None;
                                    } else {
                                        s.review.selection = Some(FileSelection {
                                            key: selection_key.clone(),
                                            range: None,
                                        });
                                    }
                                    cx.notify();
                                }),
                            )
                            .when(selecting.is_some(), |b| {
                                b.bg(rgb(p.hover)).tooltip("Exit line selection")
                            }),
                        ),
                    );
                }
                for (line_index, line) in page.lines.into_iter().enumerate() {
                    let color = match line.text.as_bytes().first() {
                        Some(b'+') => p.accent,
                        Some(b'-') => p.error,
                        Some(b'@') => p.warning,
                        _ => p.fg,
                    };
                    let content = div()
                        .w_full()
                        .min_w_0()
                        .font_family("monospace")
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(color))
                        .child(line.text);
                    let Some(selecting) = selecting else {
                        file = file.child(content);
                        continue;
                    };
                    let selected = selecting
                        .range
                        .as_ref()
                        .zip(line.location.as_ref())
                        .is_some_and(|(range, line)| range.includes(&patches, line));
                    let gutter = if let Some(location) = line.location {
                        let description = location.description();
                        let source = location.source(&patches).clone();
                        let selection_key = key.clone();
                        crate::kit::toolbar_button(
                            p,
                            SharedString::from(format!("review-line-{index}-{line_index}")),
                            location.label(),
                            true,
                            cx.listener(move |s, _, _, cx| {
                                if let Some(selection) = s
                                    .review
                                    .selection
                                    .as_mut()
                                    .filter(|selection| selection.key == selection_key)
                                {
                                    selection.range = Some(Selection::select(
                                        selection.range.take(),
                                        source.clone(),
                                        location.clone(),
                                    ));
                                    cx.notify();
                                }
                            }),
                        )
                        .accessibility_label(description)
                        .font_family("monospace")
                        .text_size(p.text(Step::Meta))
                        .w(px(46.))
                        .flex_shrink_0()
                        .h(px(21.))
                        .px_1()
                        .text_color(rgb(color))
                        .into_any_element()
                    } else {
                        div().w(px(46.)).flex_shrink_0().into_any_element()
                    };
                    file = file.child(
                        row()
                            .items_start()
                            .when(selected, |line| line.bg(rgb(p.hover)).rounded_sm())
                            .child(gutter)
                            .child(content),
                    );
                }
                if page.next.is_some() || !pages.is_empty() {
                    let back = key.clone();
                    file = file.child(
                        row()
                            .gap_1()
                            .child(button(
                                p,
                                ("prev-patch", index),
                                "Previous lines",
                                false,
                                !pages.is_empty(),
                                cx.listener(move |s, _, _, cx| {
                                    if let Some(stack) = s.review.expanded.get_mut(&back) {
                                        stack.pop();
                                    }
                                    s.review.clear_range(&back);
                                    cx.notify()
                                }),
                            ))
                            .child(button(
                                p,
                                ("next-patch", index),
                                "Next lines",
                                false,
                                page.next.is_some(),
                                cx.listener(move |s, _, _, cx| {
                                    if let (Some(stack), Some(next)) =
                                        (s.review.expanded.get_mut(&key), page.next.clone())
                                    {
                                        stack.push(next);
                                    }
                                    s.review.clear_range(&key);
                                    cx.notify()
                                }),
                            )),
                    );
                }
            }
            pane = pane.child(file);
        }
        Some(if let Some(bar) = selection_bar {
            col()
                .size_full()
                .min_h_0()
                .child(pane.flex_1().min_h_0())
                .child(bar)
                .into_any_element()
        } else {
            pane.into_any_element()
        })
    }

    fn review_selection_bar(
        p: Palette,
        key: &FileKey,
        path_label: &str,
        had_range: bool,
        excerpt: Option<Excerpt>,
        cx: &Context<Self>,
    ) -> Div {
        let key = key.clone();
        let path_label = path_label.to_owned();
        let instruction = match &excerpt {
            Some(_) => "Choose an end line in this hunk, or add the selection.",
            None if had_range => "The diff changed. Select a line again.",
            None => "Choose the first line.",
        };
        col()
            .gap_1()
            .flex_shrink_0()
            .border_t_1()
            .border_color(rgb(p.line))
            .bg(rgb(p.panel))
            .px_3()
            .py_2()
            .when_some(excerpt.as_ref(), |bar, excerpt| {
                bar.child(small(p, excerpt.label.clone()))
            })
            .child(small(p, instruction))
            .child(
                row()
                    .gap_1()
                    .flex_wrap()
                    .child(button(
                        p,
                        "add-review-to-composer",
                        "Add to composer",
                        false,
                        excerpt.is_some(),
                        cx.listener(move |s, _, window, cx| {
                            if let Some(excerpt) = &excerpt {
                                s.add_review_excerpt(&key, &path_label, excerpt, window, cx);
                            }
                        }),
                    ))
                    .child(crate::kit::toolbar_button(
                        p,
                        "clear-review-selection",
                        "Start over",
                        had_range,
                        cx.listener(|s, _, _, cx| {
                            if let Some(selection) = s.review.selection.as_mut() {
                                selection.range = None;
                                cx.notify();
                            }
                        }),
                    ))
                    .child(crate::kit::toolbar_button(
                        p,
                        "cancel-review-selection",
                        "Cancel",
                        true,
                        cx.listener(|s, _, _, cx| {
                            s.review.selection = None;
                            cx.notify();
                        }),
                    )),
            )
    }

    fn add_review_excerpt(
        &mut self,
        key: &FileKey,
        path_label: &str,
        excerpt: &Excerpt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        if store.selected() != Some(key.0.as_str()) {
            return;
        }
        let range = self
            .review
            .selection
            .as_ref()
            .filter(|selection| &selection.key == key)
            .and_then(|selection| selection.range.as_ref());
        let current = store
            .transcript(&key.0)
            .and_then(|thread| thread.reviews.get(&key.1))
            .and_then(|turn| {
                turn.file_groups()
                    .into_iter()
                    .find(|(path, _)| *path == key.2)
            });
        if !current.is_some_and(|(_, patches)| excerpt.matches(&patches, range)) {
            cx.notify();
            return;
        }
        let owner = Some(key.0.clone());
        let current_draft = store.draft(owner.as_deref());
        let draft = excerpt.append_to(current_draft.to_owned(), path_label);
        // Preserve editor tokens and make the insertion undoable. The existing draft action
        // also covers an unmounted editor.
        if let Some(editor) = self.editors.get(&owner)
            && editor.read(cx).value().as_ref() == current_draft
        {
            let position = editor
                .read(cx)
                .text()
                .offset_to_position(current_draft.len());
            let insertion = draft[current_draft.len()..].to_owned();
            editor.update(cx, |input, cx| {
                if input.marked_text_range(window, cx).is_none() {
                    input.set_cursor_position(position, window, cx);
                    input.insert(insertion, window, cx);
                }
            });
        }
        self.review.selection = None;
        // Narrow layouts show the review instead of the composer.
        if self.pane_width - px(self.review_width as f32 + 5.) < px(500.) {
            self.review.open = false;
            self.review_layout_changed(cx);
        }
        self.focus_editor_for = Some(owner.clone());
        self.action(cx, Action::DraftEdit(owner, draft));
    }
}
