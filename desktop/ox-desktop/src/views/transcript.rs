use crate::appearance::Step;
pub(super) mod cards;
mod tools;
use super::workbench::Workbench;
use crate::{
    appearance::Palette,
    col,
    kit::{button, row},
    small,
};
use gpui::{prelude::FluentBuilder, *};
use gpui_component::button::Button;
use gpui_component::text::{TextView, TextViewState};
use gpui_component::{ActiveTheme, menu::ContextMenuExt};
use ox_desk_core::actions::Action as DeskAction;
use ox_desk_core::host_protocol::{Scope, SessionResult};
use ox_desk_core::store::Connection;
use ox_desk_core::{
    transcript::item::Item,
    transcript::{ItemKey, Row},
};
use std::collections::HashSet;

const OVERDRAW: f32 = 240.;
/// The jump control shows once the view is this many viewport heights above the bottom.
const FAR: f32 = 1.5;

/// How far the view sits above the bottom, in pixels (0 when pinned to it).
fn above_bottom(list: &ListState) -> Pixels {
    (list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y).max(px(0.))
}
fn is_far(list: &ListState) -> bool {
    let viewport = list.viewport_bounds().size.height;
    viewport > px(0.) && above_bottom(list) > viewport * FAR
}

struct CachedMessage {
    key: ItemKey,
    revision: u64,
    state: Entity<TextViewState>,
    scrollable: bool,
}

pub(super) struct TranscriptView {
    list: ListState,
    expanded: HashSet<ItemKey>,
    tool_limits: std::collections::HashMap<ItemKey, usize>,
    /// Tool items whose long output is shown in full instead of head and tail.
    show_all: HashSet<ItemKey>,
    all_expanded: bool,
    hide_thinking: bool,
    find: Option<tools::Find>,
    prepended: usize,
    following: bool,
    /// Scrolled up more than `FAR` viewport heights from the bottom.
    far: bool,
    /// Items that existed the last time the view was at (or near) the bottom.
    seen: usize,
    reported_result: Option<String>,
    document_height: Pixels,
    markdown: std::collections::VecDeque<CachedMessage>,
}

pub(super) struct ResultViewed {
    pub scope: Scope,
    pub thread: String,
    pub result: SessionResult,
}

impl EventEmitter<ResultViewed> for Workbench {}

impl TranscriptView {
    pub(super) fn invalidate_messages(&mut self) {
        self.markdown.clear();
    }
    fn message(
        &mut self,
        key: &ItemKey,
        revision: u64,
        text: &str,
        project: Option<std::path::PathBuf>,
        cx: &mut App,
    ) -> TextView {
        // Visible rows can be measured more than once per frame. Reuse their parser,
        // selections and layout. Evicted rows reconstruct directly from the store.
        let mut cached = self
            .markdown
            .iter()
            .position(|entry| &entry.key == key)
            .and_then(|index| self.markdown.remove(index))
            .unwrap_or_else(|| CachedMessage {
                key: key.clone(),
                revision,
                scrollable: false,
                state: cx.new(|cx| TextViewState::markdown(text, cx)),
            });
        if cached.revision != revision {
            cached.state.update(cx, |s, cx| s.set_text(text, cx));
            cached.revision = revision;
        }
        // A document with thousands of paragraphs would otherwise measure every
        // paragraph on every append. The native text view owns a virtual block list.
        let scrollable = text.len() >= 16 * 1024;
        if scrollable && !cached.scrollable && self.following {
            cached
                .state
                .read(cx)
                .list_state()
                .set_follow_mode(FollowMode::Tail);
            cached.state.read(cx).list_state().scroll_to_end();
        }
        cached.scrollable = scrollable;
        let height = self.document_height;
        let element = cards::markdown(TextView::new(&cached.state), project)
            .selectable(true)
            .when(scrollable, |view| view.scrollable(true).h(height));
        self.markdown.push_front(cached);
        self.markdown.truncate(48);
        element
    }
    pub(super) fn uniform_row_budget(&self, row: f32) -> Option<usize> {
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        (row > 0.).then(|| ((viewport + OVERDRAW) / row).ceil() as usize + 1)
    }
}

/// The round "Jump to latest" control floating over the bottom-right of the transcript,
/// with a "N new" count while content arrived below the viewport.
fn jump_to_latest(
    p: Palette,
    list: ListState,
    unseen: usize,
    motion: bool,
    cx: &mut Context<Workbench>,
) -> Div {
    div().absolute().bottom_3().right_4().child(
        crate::row()
            .items_center()
            .gap_1()
            .pl_2()
            .when(unseen == 0, |d| d.pl_0())
            .rounded_full()
            .bg(rgb(p.raised))
            .border_1()
            .border_color(rgb(p.line_strong))
            .when(unseen > 0, |d| {
                d.child(
                    div()
                        .text_size(p.text(Step::Meta))
                        .text_color(rgb(p.accent))
                        .child(format!("{unseen} new")),
                )
            })
            .child(
                crate::kit::icon_button(
                    p,
                    "jump-latest",
                    "↓",
                    "Jump to latest",
                    cx.listener(move |_, _, _, cx| {
                        glide_to_end(list.clone(), motion, cx);
                    }),
                )
                .rounded(px(14.)),
            ),
    )
}

/// Scroll to the newest content and resume following it. With motion this eases over a
/// few frames; reduced motion jumps.
fn glide_to_end(list: ListState, motion: bool, cx: &mut Context<Workbench>) {
    if !motion {
        list.scroll_to_end();
        cx.notify();
        return;
    }
    cx.spawn(async move |this, cx| {
        for _ in 0..12 {
            let left = above_bottom(&list);
            if left < px(2.) {
                break;
            }
            list.scroll_by(left * 0.35 + px(1.));
            let _ = this.update(cx, |_, cx| cx.notify());
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
        }
        // Anchor to the end so following resumes and later content stays in view.
        list.scroll_to_end();
        let _ = this.update(cx, |_, cx| cx.notify());
    })
    .detach();
}

impl Workbench {
    pub(super) fn reveal_latest_result(&self, id: &str) {
        if let Some(view) = self.transcript_views.get(id) {
            view.list.scroll_to_end();
        }
    }

    pub(super) fn transcript_panel(&mut self, cx: &mut Context<Self>) -> Div {
        let p = Palette::get(cx);
        let Some(store) = &mut self.store else {
            return col().flex_1().min_h_0();
        };
        let Some(id) = store.selected().map(str::to_owned) else {
            return col().flex_1().min_h_0();
        };
        let changed_rows = store.take_remeasure(&id);
        let Some(transcript) = store.transcript(&id) else {
            return col().flex_1().min_h_0();
        };
        let count = transcript.len();
        let view = self.transcript_views.entry(id.clone()).or_insert_with(|| {
            let list = ListState::new(count, ListAlignment::Bottom, px(OVERDRAW));
            list.set_follow_mode(FollowMode::Tail);
            let owner = id.clone();
            list.set_scroll_handler(cx.listener(move |s, event: &ListScrollEvent, _, cx| {
                if let Some(view) = s.transcript_views.get_mut(&owner)
                    && view.following != event.is_following_tail
                {
                    view.following = event.is_following_tail;
                    cx.notify();
                }
                // The list is still borrowed here: measure the distance once it is free.
                let workbench = cx.weak_entity();
                let watched = owner.clone();
                cx.defer(move |cx| {
                    let _ = workbench.update(cx, |s, cx| {
                        if let Some(view) = s.transcript_views.get_mut(&watched) {
                            let far = is_far(&view.list);
                            if far != view.far {
                                view.far = far;
                                cx.notify();
                            }
                        }
                    });
                });
                if event.visible_range.start <= 2
                    && !event.is_following_tail
                    && s.store
                        .as_ref()
                        .and_then(|store| store.transcript(&owner))
                        .is_some_and(|t| t.has_older() && !t.is_loading())
                {
                    s.action(cx, DeskAction::LoadOlder(owner.clone()));
                }
            }));
            TranscriptView {
                list,
                expanded: HashSet::new(),
                tool_limits: Default::default(),
                show_all: HashSet::new(),
                all_expanded: false,
                hide_thinking: false,
                find: None,
                prepended: transcript.prepended(),
                following: true,
                far: false,
                seen: count,
                reported_result: None,
                document_height: px(360.),
                markdown: Default::default(),
            }
        });
        let inserted = transcript.prepended().saturating_sub(view.prepended);
        if inserted > 0 {
            view.list.splice(0..0, inserted);
        }
        view.prepended = transcript.prepended();
        let previous = view.list.item_count();
        if count > previous {
            view.list.splice(previous..previous, count - previous);
        } else if count < previous {
            view.list.splice(count..previous, 0);
        }
        for row in changed_rows {
            view.list.remeasure_items(row..row + 1);
        }
        let height = view.list.viewport_bounds().size.height;
        if height > px(0.) {
            view.document_height = (height * 0.8).max(px(140.));
        }
        if let Some(find) = &mut view.find
            && find.paging
            && !transcript.is_loading()
        {
            find.paging = false;
            let owner = id.clone();
            let workbench = cx.weak_entity();
            cx.defer(move |cx| {
                let _ = workbench.update(cx, |s, cx| {
                    if s.store.as_ref().and_then(|store| store.selected()) == Some(&owner) {
                        s.search_transcript(if inserted == 0 { 0 } else { 1 }, cx);
                    }
                });
            });
        }
        // Pinned to the bottom (or close): new content is already in view.
        view.far = is_far(&view.list);
        if !view.far {
            view.seen = count;
        }
        let (far, unseen) = (view.far, count.saturating_sub(view.seen));
        let motion = p.motion;
        let list_state = view.list.clone();
        let jump_state = view.list.clone();
        let viewed = store
            .last_result(&id)
            .filter(|result| {
                self.front
                    && store.state() == &Connection::Ready
                    && transcript.is_ready()
                    && transcript.active_turn().is_none()
                    && view.reported_result.as_deref() != Some(&result.turn)
                    && (transcript.last_finished() == Some(&result.turn)
                        || count == 0
                        || transcript.row(count - 1).is_some_and(|row| match row {
                            Row::Message(key) => key.turn == result.turn,
                            Row::Tools(keys) => {
                                keys.back().is_some_and(|key| key.turn == result.turn)
                            }
                        }))
            })
            .map(|result| {
                (
                    ResultViewed {
                        scope: Scope {
                            project: self.project.clone(),
                            route: self.route.clone(),
                        },
                        thread: id.clone(),
                        result: result.clone(),
                    },
                    list_state.clone(),
                    cx.weak_entity(),
                )
            });
        let find_bar = tools::find_bar(view, &id, p, cx);
        let banner = handoff_banner(store, &id, p, cx);
        col()
            .flex_1()
            .min_h_0()
            .children(banner)
            .children(find_bar)
            .when(transcript.is_loading(), |d| {
                d.child(
                    crate::row()
                        .gap_2()
                        .when_some(self.runner.clone(), |r, ox| r.child(ox))
                        .child(small(p, "Loading history…")),
                )
            })
            .when_some(transcript.error(), |d, error| {
                d.child(
                    crate::row()
                        .flex_wrap()
                        .gap_2()
                        .child(small(p, error.to_owned()))
                        .child(crate::kit::copy_message(p, "copy-transcript-error", error)),
                )
            })
            .when(transcript.has_older() && !transcript.is_loading(), |d| {
                let id = id.clone();
                d.child(button(
                    p,
                    "older-turns",
                    "Earlier messages",
                    false,
                    true,
                    cx.listener(move |s, _, _, cx| s.action(cx, DeskAction::LoadOlder(id.clone()))),
                ))
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            list_state,
                            cx.processor(move |s, index, _, cx| {
                                crate::app::guard(cx, |cx| s.transcript_row(&id, index, cx))
                            }),
                        )
                        .size_full(),
                    )
                    .when(far, |d| {
                        d.child(jump_to_latest(p, jump_state, unseen, motion, cx))
                    })
                    .when_some(viewed, |d, (viewed, list, owner)| {
                        d.child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, cx| {
                                    let visible = bounds.size.height > px(0.)
                                        && (count == 0
                                            || list.is_following_tail()
                                            || list.bounds_for_item(count - 1).is_some_and(
                                                |item| {
                                                    item.top() < bounds.bottom()
                                                        && item.bottom() > bounds.top()
                                                },
                                            ));
                                    if window.is_window_active() && window.is_visible() && visible {
                                        cx.defer(move |cx| {
                                            let _ = owner.update(cx, |s, cx| {
                                                if let Some(view) =
                                                    s.transcript_views.get_mut(&viewed.thread)
                                                    && view.reported_result.as_deref()
                                                        != Some(&viewed.result.turn)
                                                {
                                                    view.reported_result =
                                                        Some(viewed.result.turn.clone());
                                                    cx.emit(viewed);
                                                }
                                            });
                                        });
                                    }
                                },
                            )
                            .absolute()
                            .size_full(),
                        )
                    }),
            )
    }

    fn transcript_row(&mut self, id: &str, index: usize, cx: &mut Context<Self>) -> AnyElement {
        if let Some(stats) = &mut self.stats {
            stats.rows += 1;
        }
        let p = Palette::get(cx);
        let Some(store) = &self.store else {
            return div().into_any_element();
        };
        let runtime = store.runtime_name();
        let Some(t) = store.transcript(id) else {
            return div().into_any_element();
        };
        let Some(row) = t.row(index) else {
            return div().into_any_element();
        };
        let view = self.transcript_views.get_mut(id).unwrap();
        let owner = id.to_owned();
        let mut frame = col().w_full().min_w_0().px_3().py_1().gap_1();
        use crate::platform::restore_probe::{self, Milestone};
        if self.front && restore_probe::wants(id, Milestone::TranscriptVisible) {
            frame = frame.relative().child(
                canvas(|_, _, _| (), restore_probe::transcript_painted)
                    .absolute()
                    .size_full(),
            );
        }
        if let Some(stats) = &mut self.stats {
            if stats
                .tail_owner
                .as_ref()
                .is_none_or(|(thread, size)| thread != id || *size != t.len())
            {
                stats.tail_owner = Some((id.into(), t.len()));
                stats.painted = Default::default();
                stats.tail_height = Default::default();
                stats.measured_items.clear();
            }
            stats.measured_items.insert(index);
        }
        if let Some(stats) = &self.stats
            && index + 1 == t.len()
        {
            let painted = stats.painted.clone();
            let height = stats.tail_height.clone();
            frame = frame.relative().child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, _, _| {
                        height.set(f32::from(bounds.size.height));
                        if painted.get() == 0 {
                            painted.set(ox_desk_core::perf::monotonic_ns());
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        match row {
            Row::Message(key) => {
                let entry = t.entry(key).expect("transcript row has an entry");
                if view.find.as_ref().and_then(|f| f.selected.as_ref()) == Some(key) {
                    frame = frame.border_l_2().border_color(rgb(p.accent));
                }
                let element = match &entry.item {
                    Item::Reasoning { summary } if summary.is_empty() || view.hide_thinking => {
                        div()
                    }
                    Item::Reasoning { summary } => {
                        let expanded = view.all_expanded ^ view.expanded.contains(key);
                        let key = key.clone();
                        let body = if expanded {
                            summary.join("\n\n")
                        } else {
                            summary
                                .first()
                                .and_then(|s| s.lines().next())
                                .unwrap_or("Thinking")
                                .to_owned()
                        };
                        frame.child(
                            div()
                                .id(SharedString::from(format!("reasoning-{index}")))
                                .cursor_pointer()
                                .italic()
                                .text_size(p.text(Step::Meta))
                                .text_color(rgb(p.muted))
                                .when(!expanded, |d| d.truncate())
                                .child(body)
                                .on_click(cx.listener(move |s, _, _, cx| {
                                    s.toggle_row(&owner, index, &key, cx)
                                })),
                        )
                    }
                    Item::UserMessage { content } => {
                        let mut text = content
                            .iter()
                            .filter_map(|c| c.text.as_deref().or(c.path.as_deref()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        // A Claude handoff target's first turn carries the earlier
                        // conversation. Fold it under the handoff marker.
                        let mut carried = None;
                        let folded = handoff_fold(store, id, key, &text);
                        if let Some((block, request)) = folded {
                            let expanded = view.all_expanded ^ view.expanded.contains(key);
                            let shown = if expanded {
                                format!("{block}{request}")
                            } else {
                                request.to_owned()
                            };
                            let toggle_key = key.clone();
                            let toggle_owner = owner.clone();
                            carried = Some(
                                div()
                                    .id(SharedString::from(format!("handoff-context-{index}")))
                                    .cursor_pointer()
                                    .text_size(p.text(Step::Meta))
                                    .text_color(rgb(p.muted))
                                    .child(if expanded {
                                        "Hide carried handoff context"
                                    } else {
                                        "Carried handoff context is folded · show"
                                    })
                                    .on_click(cx.listener(move |s, _, _, cx| {
                                        s.toggle_row(&toggle_owner, index, &toggle_key, cx)
                                    })),
                            );
                            text = shown;
                        }
                        frame.items_end().py_2().child(
                            col()
                                .min_w_0()
                                .max_w(relative(0.88))
                                .gap_1()
                                .rounded_lg()
                                .border_r_2()
                                .border_color(rgb(p.accent))
                                .bg(rgb(crate::appearance::mix(p.panel, p.accent, 16)))
                                .px_3()
                                .py_2()
                                .child(
                                    small(p, "You")
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(p.fg)),
                                )
                                .children(carried)
                                .child(markdown(id, key, text)),
                        )
                    }
                    Item::AgentMessage { text, is_update } => frame
                        .child(small(p, if *is_update { "Update" } else { runtime }))
                        .child(view.message(
                            key,
                            entry.revision,
                            text,
                            (self.link.remote().is_none() && t.active_turn().is_none()).then(
                                || {
                                    store
                                        .catalog_row(id)
                                        .map(|r| &r.cwd)
                                        .unwrap_or(&self.project)
                                        .clone()
                                },
                            ),
                            cx,
                        )),
                    Item::Plan { text } => {
                        frame
                            .child(small(p, "Plan"))
                            .child(markdown(id, key, text.clone()))
                    }
                    _ => frame,
                };
                let target = key.clone();
                let owner = id.to_owned();
                let view = cx.weak_entity();
                element
                    .context_menu(move |menu, _, cx| {
                        let Some(view) = view.upgrade() else {
                            return menu;
                        };
                        match view
                            .read(cx)
                            .store
                            .as_ref()
                            .and_then(|s| s.transcript(&owner))
                        {
                            Some(t) => tools::copy_menu(menu, t, &target),
                            None => menu,
                        }
                    })
                    .into_any_element()
            }
            Row::Tools(keys) => {
                let mono = cx.theme().mono_font_family.clone();
                let key = keys.front().unwrap().clone();
                let expanded = view.all_expanded ^ view.expanded.contains(&key);
                let binding = key.clone();
                let group_size = keys.len();
                let shown_all = view.show_all.clone();
                let visible = view.tool_limits.get(&key).copied().unwrap_or(cards::PAGE);
                let mut group = col()
                    .id(("tool-card", index))
                    .min_w_0()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(p.line));
                group = group.child(cards::header(
                    index,
                    t.entry(keys.back().expect("tool group has entries"))
                        .unwrap(),
                    group_size,
                    expanded,
                    p,
                    &mono,
                    cx.listener({
                        let owner = owner.clone();
                        move |s, _, _, cx| s.toggle_row(&owner, index, &binding, cx)
                    }),
                ));
                if expanded {
                    for (offset, item_key) in keys.iter().take(visible).enumerate() {
                        if let Some(entry) = t.entry(item_key) {
                            let local_review = (self.link.remote().is_none()
                                && t.active_turn().is_none())
                            .then(|| super::terminal_review::Review::failed(entry, &self.project))
                            .flatten();
                            let show_all = shown_all.contains(item_key);
                            let view = cx.weak_entity();
                            let (id, item) = (owner.clone(), item_key.clone());
                            let toggle_all = {
                                let (id, item) = (owner.clone(), item_key.clone());
                                cx.listener(move |s, _, _, cx| {
                                    if let Some(v) = s.transcript_views.get_mut(&id)
                                        && !v.show_all.remove(&item)
                                    {
                                        v.show_all.insert(item.clone());
                                    }
                                    cx.notify();
                                })
                            };
                            group = group.child(cards::detail(
                                entry,
                                offset,
                                group_size > 1,
                                p,
                                &mono,
                                show_all,
                                toggle_all,
                                move |_, _, cx| {
                                    if let Some(view) = view.upgrade()
                                        && let Some(entry) = view
                                            .read(cx)
                                            .store
                                            .as_ref()
                                            .and_then(|s| s.transcript(&id))
                                            .and_then(|t| t.entry(&item))
                                    {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            cards::full_text(entry),
                                        ));
                                    }
                                },
                            ));
                            if let Some(review) = local_review {
                                let source = super::terminal_review::Source::Failed {
                                    thread: owner.clone(),
                                    item: item_key.clone(),
                                };
                                group =
                                    group
                                        .child(small(p, review.directory_label()))
                                        .child(button(
                                            p,
                                            ("terminal-tool", offset),
                                            "Run in OX terminal",
                                            false,
                                            true,
                                            cx.listener(move |s, _, _, cx| {
                                                s.review_tool_command(
                                                    source.clone(),
                                                    review.clone(),
                                                    cx,
                                                );
                                            }),
                                        ));
                            }
                        }
                    }
                    if group_size > visible {
                        group = group.child(cards::more(
                            group_size - visible,
                            cx.listener(move |s, _, _, cx| {
                                if let Some(view) = s.transcript_views.get_mut(&owner) {
                                    view.tool_limits.insert(key.clone(), visible + cards::PAGE);
                                    view.list.remeasure_items(index..index + 1);
                                    cx.notify();
                                }
                            }),
                        ));
                    }
                }
                frame.child(group).into_any_element()
            }
        }
    }

    fn toggle_row(&mut self, owner: &str, index: usize, key: &ItemKey, cx: &mut Context<Self>) {
        if let Some(state) = self.transcript_views.get_mut(owner) {
            if !state.expanded.remove(key) {
                state.expanded.insert(key.clone());
            }
            state.list.remeasure_items(index..index + 1);
            cx.notify();
        }
    }
}

/// The carried block and request to show folded, for the first user message
/// of a handoff target only.
fn handoff_fold(
    store: &ox_desk_core::store::Store,
    id: &str,
    key: &ItemKey,
    text: &str,
) -> Option<(String, String)> {
    let t = store.transcript(id)?;
    ox_desk_core::handoff::fold(store.origin(id), t, key, text)
        .map(|(block, request)| (block.to_owned(), request.to_owned()))
}

/// A handoff link as a card: the other session's title, the route it is on, and a state tone.
/// Success and failure read differently at a glance; the open control is optional.
fn handoff_card(
    p: Palette,
    id: impl Into<ElementId>,
    eyebrow: &str,
    title: String,
    detail: String,
    tone: u32,
    open: Option<Button>,
) -> AnyElement {
    row()
        .id(id)
        .w_full()
        .gap_3()
        .p_2()
        .border_1()
        .border_color(rgb(p.line))
        .bg(rgb(p.panel))
        .child(div().w(px(3.)).min_h(px(36.)).flex_shrink_0().bg(rgb(tone)))
        .child(
            col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_size(p.text(Step::Label))
                        .font_family("monospace")
                        .text_color(rgb(tone))
                        .child(eyebrow.to_owned()),
                )
                .child(
                    div()
                        .text_size(p.text(Step::Ui))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(p.fg))
                        .truncate()
                        .child(title),
                )
                .child(small(p, detail)),
        )
        .children(open)
        .into_any_element()
}

/// The session's title when this client has it, else a readable stand-in. Never a session id.
fn session_title(store: &ox_desk_core::store::Store, session: &str, fallback: String) -> String {
    store
        .catalog_row(session)
        .map(|row| row.title().to_owned())
        .unwrap_or(fallback)
}

/// The handoff link cards at the top of a session: where it came from, where it went, and
/// each attempt still running or failed.
fn handoff_banner(
    store: &ox_desk_core::store::Store,
    id: &str,
    p: Palette,
    cx: &mut Context<Workbench>,
) -> Option<Div> {
    let mut lines: Vec<AnyElement> = Vec::new();
    let route = &store.profile().route;
    if let Some(origin) = store.origin(id)
        && let Some(text) = ox_desk_core::handoff::marker(origin)
        && let Some(h) = &origin.handoff
    {
        let source = h.session.clone();
        let title = session_title(
            store,
            &source,
            format!("Earlier session on {}", h.route.label()),
        );
        let open = (&h.route == route).then(|| {
            button(
                p,
                "handoff-source",
                "Open",
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.action(cx, DeskAction::SelectThread(source.clone()))
                }),
            )
        });
        lines.push(handoff_card(
            p,
            "handoff-source-card",
            "CONTINUED FROM",
            title,
            format!("{} · {text}", h.route.label()),
            p.line_strong,
            open,
        ));
    }
    let list = |key: &str| {
        store
            .session_mark(id, key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    };
    for (i, to) in list("handed_off_to").into_iter().enumerate() {
        let label = to["label"].as_str().unwrap_or("another route").to_owned();
        let session = to["session"].as_str().unwrap_or_default().to_owned();
        let same = serde_json::from_value::<ox_desk_core::runtime::Route>(to["route"].clone())
            .is_ok_and(|r| &r == route);
        let title = session_title(store, &session, format!("Continued in {label}"));
        let open = same.then(|| {
            button(
                p,
                ("handoff-target", i),
                "Open",
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.action(cx, DeskAction::SelectThread(session.clone()))
                }),
            )
        });
        lines.push(handoff_card(
            p,
            ("handoff-target-card", i),
            "HANDED OFF TO",
            title,
            label,
            p.success,
            open,
        ));
    }
    for (i, attempt) in list("handoff_attempts").into_iter().enumerate() {
        let label = attempt["label"].as_str().unwrap_or("another route");
        let title = format!("Continue in {label}");
        let card = if attempt["status"].as_str() == Some("failed") {
            handoff_card(
                p,
                ("handoff-attempt", i),
                "HANDOFF FAILED",
                title,
                attempt["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_owned(),
                p.error,
                None,
            )
        } else {
            handoff_card(
                p,
                ("handoff-attempt", i),
                "HANDING OFF",
                title,
                "Starting the new session…".into(),
                p.accent,
                None,
            )
        };
        lines.push(card);
    }
    (!lines.is_empty()).then(|| col().gap_2().px_3().py_2().children(lines))
}

fn markdown(owner: &str, key: &ItemKey, text: String) -> impl IntoElement {
    cards::markdown(
        TextView::markdown(
            SharedString::from(format!("{owner}/{}/{}", key.turn, key.item)),
            text,
        ),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::handoff_fold;
    use ox_desk_core::transcript::ItemKey;
    use ox_desk_core::{
        host_protocol::{Cursor, ScopeState, Server, SessionView, Timeline, TimelineItem},
        links::LinkEvent,
        runtime::Runtime,
        store::{Profile, Store},
    };
    use serde_json::json;

    #[test]
    fn only_a_handoff_targets_first_user_message_is_folded() {
        let route = Runtime::Claude.route(None);
        let mut store = Store::remote_client(
            Profile {
                project: "/p".into(),
                route: route.clone(),
                provider: "anthropic".into(),
            },
            |_, _| Ok("sent".into()),
        );
        let block = "<ox-handoff>\ncarried\n</ox-handoff>\n\nrequest";
        let user = |turn: &str, id: &str| TimelineItem {
            turn: turn.into(),
            id: id.into(),
            role: "user".into(),
            text: block.into(),
            revision: 1,
            detail: json!({"UserMessage": {"content": [{"text": block}]}}),
        };
        let timeline = Timeline {
            items: vec![user("t1", "u1"), user("t2", "u2")],
            detail: json!({"ready": true}),
        };
        let origin: serde_json::Value = serde_json::from_str(
            &r#"{"relation":"handoff","parent":{"project":"/p","scope":null,"session":"s","runtime":"claude"},
            "launch":"operator:x","by_operator":true,"runtime":"claude","route":ROUTE,
            "mode":"session","job_id":0,"requested":"read","ceiling":"read","source":null,
            "handoff":{"scope":{"project":"/p","route":ROUTE},"session":"s","native":null,"title":"t",
                "runtime":"claude","route":ROUTE,"source_access":null,"source_level":null,
                "decision":"inherited","selection":{"total":0,"carried":0,"omitted":0,"items":[],"cut":[],
                "ranges":[],"window":64000,"window_known":false}}}"#
                .replace("ROUTE", &serde_json::to_string(&route).unwrap()),
        )
        .unwrap();
        let session = |id: &str, detail| SessionView {
            id: id.into(),
            title: id.into(),
            parent: None,
            status: "idle".into(),
            updated_at: 1,
            capabilities: vec!["open".into()],
            detail,
        };
        store.apply(LinkEvent::Host(Box::new(Server::Snapshot {
            cursor: Cursor {
                epoch: "e".into(),
                seq: 1,
            },
            scopes: vec![ScopeState {
                scope: ox_desk_core::host_protocol::Scope {
                    project: "/p".into(),
                    route,
                },
                connection: "ready".into(),
                capabilities: vec![],
                sessions: vec![
                    session("target", json!({"origin": origin})),
                    session("plain", json!({})),
                ],
                asks: vec![],
                transcripts: [
                    ("target".to_owned(), timeline.clone()),
                    ("plain".to_owned(), timeline),
                ]
                .into(),
                detail: json!({}),
            }],
        })));
        let key = |turn: &str, id: &str| ItemKey {
            turn: turn.into(),
            item: id.into(),
        };
        assert!(store.origin("target").is_some());
        assert_eq!(
            handoff_fold(&store, "target", &key("t1", "u1"), block).map(|(_, r)| r),
            Some("request".to_owned())
        );
        assert_eq!(
            handoff_fold(&store, "target", &key("t2", "u2"), block),
            None
        );
        assert_eq!(handoff_fold(&store, "plain", &key("t1", "u1"), block), None);
    }
}
