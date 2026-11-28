//! A worker session says what it is: its job number, the access ceiling it was
//! launched under, who pays and which model answered, and the session that started it. A parent says how many of its
//! workers are still running.
use super::{OpenThread, Workbench};
use crate::appearance::Palette;
use crate::appearance::Step;
use crate::views::host::ParentLink;
use crate::{activity, col, kit::button, row};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::host_protocol::{Scope, SessionOutcome};

impl Workbench {
    /// The recorded parent of an open worker. The launch stamp carries the
    /// parent's exact scope, so a worker in a review checkout can return to the
    /// conversation that started it even when that lives in another project.
    fn parent_link(&self, id: &str, cx: &App) -> Option<ParentLink> {
        let store = self.store.as_ref()?;
        let origin = store.origin(id)?;
        let session = origin.parent.session.clone()?;
        let from_store = store
            .threads()
            .iter()
            .find(|row| row.id == session)
            .map(|row| row.title().to_owned());
        let fallback = ParentLink {
            project: origin.parent.scope.as_ref().map_or_else(
                || origin.parent.project.clone(),
                |scope| scope.project.clone(),
            ),
            route: origin
                .parent
                .scope
                .as_ref()
                .map(|scope| scope.route.clone()),
            id: session,
            title: from_store,
        };
        Some(
            self.workers
                .read(cx)
                .parents
                .get(&(self.project.clone(), id.to_owned()))
                .cloned()
                .unwrap_or(fallback),
        )
    }

    /// The crumb under the title of an open worker: its parent, job, ceiling,
    /// billing route, confirmed model and checkout. Clicking the parent asks the
    /// host to open it in its recorded scope.
    pub(super) fn worker_crumb(&self, p: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let id = store.selected()?;
        let origin = store.origin(id)?;
        let parent = origin
            .linked
            .is_none()
            .then(|| self.parent_link(id, cx))
            .flatten();
        let linked_lead = origin.linked.as_ref().map(|lead| {
            self.workspace
                .as_ref()
                .and_then(WeakEntity::upgrade)
                .and_then(|workspace| workspace.read(cx).linked_worker_lead(lead, cx))
                .unwrap_or_else(|| ("Lead on another host".into(), None))
        });
        let parent_label = parent.as_ref().map(|parent| {
            let name = parent
                .title
                .clone()
                .unwrap_or_else(|| "Parent session".into());
            let elsewhere = parent.project != self.project;
            let leaf = parent
                .project
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            if elsewhere {
                format!("{name} · {leaf}")
            } else {
                name
            }
        });
        let parent_path = parent
            .as_ref()
            .map(|parent| parent.project.to_string_lossy().into_owned());
        let job = if self.host_caption.is_some() {
            format!("Worker {}", origin.job_id)
        } else {
            let host = self
                .link
                .remote()
                .map_or("this computer", |host| host.name.as_str());
            format!("Worker {} on {host}", origin.job_id)
        };
        let ceiling = format!("Ceiling {}", origin.ceiling.label(origin.runtime));
        let checkout = self.project.display().to_string();
        // Who pays and the model that answered; "(pending)" until the runtime confirms it.
        let model = store
            .session_mark(id, "worker_model")
            .and_then(ox_desk_core::origin::model_line)
            .unwrap_or_else(|| origin.route.label());
        Some(
            row()
                .flex_wrap()
                .gap_2()
                .items_center()
                .text_size(p.text(Step::Ui))
                .text_color(rgb(p.fg))
                .when_some(linked_lead, |line, (label, destination)| {
                    if let Some(destination) = destination {
                        let workspace = self.workspace.clone();
                        line.child(crate::kit::toolbar_button(
                            p,
                            "linked-worker-lead",
                            label,
                            true,
                            move |_, window, cx| {
                                if let Some(workspace) =
                                    workspace.as_ref().and_then(WeakEntity::upgrade)
                                {
                                    workspace.update(cx, |s, cx| {
                                        s.open_destination(&destination, window, cx)
                                    });
                                }
                            },
                        ))
                    } else {
                        line.child(crate::small(p, label))
                    }
                })
                .when_some(
                    parent.zip(parent_label).zip(parent_path),
                    |line, ((parent, label), path)| {
                        line.child(
                            div()
                                .id("worker-parent")
                                .cursor_pointer()
                                .text_color(rgb(p.accent))
                                .child(label)
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(path.clone())
                                        .build(window, cx)
                                })
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(OpenThread(parent.clone()))
                                })),
                        )
                        .child(crate::small(p, "▸"))
                    },
                )
                .child(div().px_1().border_1().border_color(rgb(p.line)).child(job))
                .child(crate::small(p, ceiling))
                .child(crate::small(p, model))
                .child(crate::small(p, format!("Checkout {checkout}")))
                .into_any_element(),
        )
    }

    /// "2 workers running" under the parent's activity line, linking to the first one.
    /// Workers running under a thread, at any depth.
    pub(super) fn running_workers(&self, id: &str, cx: &App) -> usize {
        self.workers
            .read(cx)
            .trees
            .get(&(self.project.clone(), id.to_owned()))
            .map_or(0, |tree| tree.running)
    }
    pub(super) fn workers_line(&self, p: Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.as_ref()?;
        let id = store.selected()?;
        let scope = Scope {
            project: self.project.clone(),
            route: self.route.clone(),
        };
        let owner = id.to_owned();
        let index = self.workers.read(cx);
        if let Some(workers) = index.results.get(&(scope, owner.clone())) {
            let is_open = self.worker_results_open.as_deref() == Some(id);
            let label = format!(
                "Workers ({}) {}",
                workers.len(),
                if is_open { "▾" } else { "▸" }
            );
            let mut panel = col().gap_1().child(row().child(button(
                p,
                "worker-results-toggle",
                label,
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.worker_results_open = (!is_open).then(|| owner.clone());
                    cx.notify();
                }),
            )));
            if is_open {
                let mut list = col()
                    .id("worker-results")
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .p_2()
                    .gap_1()
                    .bg(rgb(p.panel))
                    .border_1()
                    .border_color(rgb(p.line));
                for (position, worker) in workers.iter().enumerate() {
                    let target = worker.target.clone();
                    let title = target
                        .title
                        .clone()
                        .unwrap_or_else(|| "Worker session".into());
                    let mut meta = format!(
                        "{} · {} · {} ceiling",
                        worker.status, worker.billing, worker.access
                    );
                    if let Some(model) = &worker.model {
                        meta.push_str(&format!(" · {model}"));
                    }
                    list = list.child(
                        col()
                            .gap_1()
                            .child(row().child(button(
                                p,
                                ("worker-result", position),
                                title,
                                false,
                                true,
                                cx.listener(move |_, _, _, cx| cx.emit(OpenThread(target.clone()))),
                            )))
                            .child(crate::small(p, meta)),
                    );
                }
                panel = panel.child(list);
            }
            return Some(panel.into_any_element());
        }
        if store.is_host_client()
            && self.route.runtime.supports_workers()
            && store.origin(id).is_none()
            && store.draft(Some(id)).is_empty()
            && store
                .transcript(id)
                .is_some_and(|transcript| transcript.active_turn().is_none())
            && store
                .last_result(id)
                .is_some_and(|res| res.outcome == SessionOutcome::Completed)
        {
            return Some(row().child(button(
                p,
                "worker-review-starter",
                "Ask for a second opinion",
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    if let Some(store) = s.store.as_mut()
                        && store.selected() == Some(owner.as_str())
                        && store.draft(Some(&owner)).is_empty()
                    {
                        store.edit(Some(&owner), "Ask one independent read-only worker to review the result so far. Summarize its findings here.".into());
                        s.focus_editor_for = Some(Some(owner.clone()));
                        cx.notify();
                    }
                }),
            )).into_any_element());
        }
        let tree = self
            .workers
            .read(cx)
            .trees
            .get(&(self.project.clone(), id.to_owned()))?;
        if tree.running == 0 {
            return None;
        }
        let label = match tree.running {
            1 => "1 worker running".to_owned(),
            n => format!("{n} workers running"),
        };
        let first = tree.first_running.clone();
        Some(
            row()
                .id("workers-running")
                .gap_2()
                .py_1()
                .cursor_pointer()
                .child(activity::Scene::Worker.sprite(p, 0))
                .child(crate::small(p, label))
                .when_some(first, |line, first| {
                    line.on_click(cx.listener(move |s, _, _, cx| {
                        cx.emit(OpenThread(ParentLink {
                            project: s.project.clone(),
                            route: None,
                            id: first.clone(),
                            title: None,
                        }))
                    }))
                })
                .into_any_element(),
        )
    }
}
