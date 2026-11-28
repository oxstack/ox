//! The live sessions the session host runs, behind the sidebar footer's one
//! "N open" toggle: per-row End (on hover or focus) and an "End all idle" action.
//! It polls the host while the sidebar shows it, and again right after every End.
use crate::appearance::Step;
use crate::{
    appearance::Palette,
    kit::{button, col, row, small, toolbar_button},
    platform,
    platform::HOST_NOT_RUNNING,
};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::host_protocol::{ProcessView, Scope};
use std::{
    io,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A process to end: its scope and every thread id it runs.
type Target = (Scope, Vec<String>);

const POLL: Duration = Duration::from_secs(5);
/// Poll less often while nothing is running.
const POLL_EMPTY: Duration = Duration::from_secs(20);

/// A compact duration such as `40s`, `12m`, `3h` or `2d`. Negative spans read as zero.
pub(crate) fn span(seconds: i64) -> String {
    match seconds.max(0) {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m", s / 60),
        s @ 3600..86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// The idle line for a row: `busy` while a turn or approval is in progress, otherwise
/// how long since its last activity.
pub(crate) fn idle_label(now: i64, process: &ProcessView) -> String {
    if process.running {
        "busy".into()
    } else {
        format!("idle {}", span(now.saturating_sub(process.last_activity)))
    }
}

/// The rows "End all idle" ends: every process that is not running, in host order.
pub(crate) fn idle_rows(processes: &[ProcessView]) -> Vec<&ProcessView> {
    processes
        .iter()
        .filter(|p| !p.running && !p.draft)
        .collect()
}

/// The idle rows of a fresh snapshot that were also idle in the snapshot the user saw.
/// A process that started a turn since then, or changed its threads, is left alone.
pub(crate) fn still_idle(seen: &[Target], fresh: &[ProcessView]) -> Vec<Target> {
    fresh
        .iter()
        .filter(|p| !p.running && !p.draft)
        .filter(|p| {
            seen.iter()
                .any(|(scope, threads)| *scope == p.scope && *threads == p.threads)
        })
        .map(|p| (p.scope.clone(), p.threads.clone()))
        .collect()
}

/// The last path component of a project, or the whole path when it has none.
pub(crate) fn project_name(project: &Path) -> String {
    project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.display().to_string())
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "claude" => "Claude",
        "codex" => "Codex",
        "worker" => "Worker",
        _ => "Process",
    }
}

fn error_line(p: Palette, message: String) -> Div {
    div()
        .text_size(p.text(Step::Meta))
        .text_color(rgb(p.error))
        .child(message)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[derive(Default)]
pub struct Running {
    processes: Vec<ProcessView>,
    loaded: bool,
    loading: bool,
    /// A refresh was requested while one was in flight; run it when that one lands.
    again: bool,
    ending: bool,
    /// A failed refresh or poll. Cleared by the next successful refresh.
    error: Option<String>,
    /// The last End that failed. Kept until dismissed or until the next End.
    end_error: Option<String>,
    /// A busy row whose End is waiting for the user to confirm.
    confirm: Option<Target>,
    /// Set by the sidebar each time it draws. Polling stops while the panel is hidden.
    shown: bool,
    link: platform::HostLink,
    monitor: Arc<platform::ProcessMonitor>,
    /// The list is expanded above the sidebar footer.
    pub(super) open: bool,
    poll: Option<Task<()>>,
}

impl Running {
    pub(super) fn new(link: platform::HostLink) -> Self {
        Self {
            link,
            ..Self::default()
        }
    }
    /// The one count for the footer's "n open": every live host process, busy or idle.
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }

    pub(super) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.shown != shown {
            self.shown = shown;
            if shown {
                self.refresh(cx);
            } else {
                self.poll = None;
            }
        }
    }

    /// Update the monitor snapshot and report whether the visible panel needs painting.
    fn apply_process_list(&mut self, result: io::Result<Vec<ProcessView>>) -> bool {
        let old_count = self.process_count();
        let was_open = self.open;
        match result {
            Ok(processes) => {
                self.open &= !processes.is_empty();
                self.processes = processes;
                self.loaded = true;
                self.error = None;
            }
            Err(e) if e.kind() == io::ErrorKind::NotConnected => {
                // The host was stopped: nothing is running under it any more.
                self.processes.clear();
                self.error = Some(HOST_NOT_RUNNING.to_owned());
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        // The collapsed footer displays only the count. An open panel also
        // shows elapsed time, so its labels change while the list stays open.
        was_open || old_count != self.process_count()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.again = true;
            return;
        }
        self.poll = None;
        self.loading = true;
        let monitor = self.monitor.clone();
        let link = self.link.clone();
        let task = cx.background_spawn(async move { monitor.list(&link) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.loading = false;
                let repaint = s.apply_process_list(result);
                if s.shown {
                    if repaint {
                        cx.notify();
                    }
                    if std::mem::take(&mut s.again) {
                        s.refresh(cx);
                    } else {
                        let wait = if s.processes.is_empty() {
                            POLL_EMPTY
                        } else {
                            POLL
                        };
                        s.poll = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(wait).await;
                            let _ = this.update(cx, |s, cx| s.refresh(cx));
                        }));
                    }
                } else {
                    s.again = false;
                }
            });
        })
        .detach();
    }

    /// End the processes `plan` picks, in the background, then refresh the list. `plan`
    /// runs off the UI thread, so it may read the host first.
    fn end(
        &mut self,
        plan: impl FnOnce() -> Result<Vec<Target>, String> + Send + 'static,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        if self.ending {
            return;
        }
        let link = self.link.clone();
        self.ending = true;
        self.end_error = None;
        let task = cx.background_spawn(async move {
            let groups = match plan() {
                Ok(groups) => groups,
                Err(message) => return Some(message),
            };
            let mut failure = None;
            for (scope, threads) in groups {
                if let Err(e) = link.end_threads(scope, threads, force) {
                    failure.get_or_insert(e.to_string());
                }
            }
            failure
        });
        cx.spawn(async move |this, cx| {
            let failure = task.await;
            let _ = this.update(cx, |s, cx| {
                s.ending = false;
                s.end_error = failure;
                s.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// End one row. A busy row is only ended after its inline confirmation.
    fn end_row(&mut self, target: Target, busy: bool, cx: &mut Context<Self>) {
        if busy {
            self.confirm = Some(target);
            cx.notify();
        } else {
            self.end(move || Ok(vec![target]), false, cx);
        }
    }

    /// End every idle row, judged against a fresh host snapshot taken just before ending.
    fn end_all_idle(&mut self, cx: &mut Context<Self>) {
        let seen: Vec<Target> = idle_rows(&self.processes)
            .into_iter()
            .map(|p| (p.scope.clone(), p.threads.clone()))
            .collect();
        if seen.is_empty() {
            return;
        }
        let monitor = self.monitor.clone();
        self.end(
            move || {
                let fresh = monitor
                    .list(&platform::HostLink::Local)
                    .map_err(|e| e.to_string())?;
                Ok(still_idle(&seen, &fresh))
            },
            false,
            cx,
        );
    }
}

impl Render for Running {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let view = cx.weak_entity();
        let now = now_secs();
        let idle = idle_rows(&self.processes).len();
        let count = self.process_count();

        let rows = self.processes.iter().enumerate().map(|(i, process)| {
            let view = view.clone();
            let draft = process.draft;
            let model = if draft {
                "ready (idle)".to_owned()
            } else {
                process
                    .model
                    .clone()
                    .unwrap_or_else(|| process.route.clone())
            };
            let end_enabled = !self.ending;
            let target: Target = (process.scope.clone(), process.threads.clone());
            let busy = process.running;
            let confirming = self.confirm.as_ref() == Some(&target);
            let confirm_target = target.clone();
            let end_target = target;
            let row_view = view.clone();
            let group = SharedString::from(format!("running-row-{i}"));
            let line = row()
                .group(group.clone())
                .gap_2()
                .items_start()
                .min_w_0()
                .child(
                    div()
                        .mt(px(6.))
                        .size(px(6.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(if process.running {
                            p.success
                        } else {
                            p.line_strong
                        })),
                )
                .child(
                    col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            row()
                                .id(ElementId::from(format!("running-kind-{i}")))
                                .gap_1()
                                .min_w_0()
                                .when(draft, |r| {
                                    r.tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(
                                            "Keeps Claude Code warm for new sessions",
                                        )
                                        .build(window, cx)
                                    })
                                })
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(p.text(Step::Meta))
                                        .text_color(rgb(p.fg))
                                        .child(kind_label(&process.kind)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(p.text(Step::Meta))
                                        .text_color(rgb(p.muted))
                                        .child(model),
                                ),
                        )
                        .child(small(
                            p,
                            if draft {
                                format!(
                                    "{} · up {}",
                                    project_name(&process.project),
                                    span(now.saturating_sub(process.started_at))
                                )
                            } else {
                                format!(
                                    "{} · up {} · {}",
                                    project_name(&process.project),
                                    span(now.saturating_sub(process.started_at)),
                                    idle_label(now, process)
                                )
                            },
                        )),
                )
                .child(
                    // End shows on row hover or keyboard focus; it stays a tab stop.
                    div()
                        .flex_shrink_0()
                        .when(!confirming, |d| {
                            d.opacity(0.)
                                .group_hover(group.clone(), |s| s.opacity(1.))
                                .in_focus(|s| s.opacity(1.))
                        })
                        .child(
                            toolbar_button(
                                p,
                                ElementId::from(format!("running-end-{i}")),
                                "End",
                                end_enabled && !confirming,
                                move |_, _, cx| {
                                    let target = end_target.clone();
                                    let _ =
                                        row_view.update(cx, |s, cx| s.end_row(target, busy, cx));
                                },
                            )
                            .h(px(24.))
                            .tooltip("End this session"),
                        ),
                );
            let confirm_view = view.clone();
            let keep_view = view.clone();
            col().gap_1().child(line).when(confirming, |c| {
                c.child(
                    row()
                        .gap_2()
                        .flex_wrap()
                        .child(small(
                            p,
                            "This session is busy. Ending it stops its turn and workers.",
                        ))
                        .child(button(
                            p,
                            ElementId::from(format!("running-confirm-{i}")),
                            "End session",
                            true,
                            end_enabled,
                            move |_, _, cx| {
                                let target = confirm_target.clone();
                                let _ = confirm_view.update(cx, |s, cx| {
                                    s.confirm = None;
                                    s.end(move || Ok(vec![target]), true, cx);
                                });
                            },
                        ))
                        .child(toolbar_button(
                            p,
                            ElementId::from(format!("running-keep-{i}")),
                            "Keep running",
                            true,
                            move |_, _, cx| {
                                let _ = keep_view.update(cx, |s, cx| {
                                    s.confirm = None;
                                    cx.notify();
                                });
                            },
                        )),
                )
            })
        });

        let header_view = view.clone();
        let dismiss_view = view.clone();
        if !self.open || (count == 0 && self.end_error.is_none()) {
            return div().id("running-panel");
        }
        div()
            .id("running-panel")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .px(px(crate::kit::GUTTER))
            .pt(px(crate::kit::SPACE_S))
            .gap_1()
            .child(
                row()
                    .justify_between()
                    .min_w_0()
                    .child(crate::kit::micro_label(p, "LIVE SESSIONS"))
                    .child(
                        toolbar_button(
                            p,
                            "running-end-idle",
                            "End all idle",
                            idle > 0 && !self.ending,
                            move |_, _, cx| {
                                let _ = header_view.update(cx, |s, cx| s.end_all_idle(cx));
                            },
                        )
                        .h(px(24.)),
                    ),
            )
            // Without a running host (local-only desktop) there is nothing to list:
            // say so quietly instead of repeating a red error every poll.
            .when(self.error.as_deref() == Some(HOST_NOT_RUNNING), |c| {
                c.child(small(p, HOST_NOT_RUNNING))
            })
            .when_some(
                self.error
                    .clone()
                    .filter(|e| e != HOST_NOT_RUNNING)
                    .filter(|_| self.loaded),
                |c, message| c.child(error_line(p, message)),
            )
            .when_some(self.end_error.clone(), |c, message| {
                c.child(
                    row()
                        .gap_2()
                        .min_w_0()
                        .child(div().flex_1().min_w_0().child(error_line(p, message)))
                        .child(button(
                            p,
                            "running-end-dismiss",
                            "Dismiss",
                            false,
                            true,
                            move |_, _, cx| {
                                let _ = dismiss_view.update(cx, |s, cx| {
                                    s.end_error = None;
                                    cx.notify();
                                });
                            },
                        )),
                )
            })
            .when(count == 0 && self.error.is_none(), |c| {
                c.child(small(
                    p,
                    if self.loaded {
                        "No open sessions"
                    } else {
                        "Checking the session host…"
                    },
                ))
            })
            .when(count > 0, |c| {
                c.child(
                    col()
                        .id("running-rows")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .gap_2()
                        .children(rows),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{Running, idle_label, idle_rows, project_name, span, still_idle};
    use ox_desk_core::host_protocol::{ProcessView, Scope};
    use std::path::Path;

    fn process(threads: &[&str], running: bool, last: i64) -> ProcessView {
        ProcessView {
            kind: "claude".into(),
            scope: Scope {
                project: "/work/app".into(),
                route: ox_desk_core::runtime::Runtime::Claude.route(None),
            },
            project: "/work/app".into(),
            threads: threads.iter().map(|t| (*t).to_owned()).collect(),
            route: "Claude".into(),
            model: None,
            started_at: 0,
            last_activity: last,
            running,
            draft: false,
        }
    }

    #[test]
    fn span_formats_each_unit_and_clamps_negative() {
        assert_eq!(span(-5), "0s");
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(60), "1m");
        assert_eq!(span(3599), "59m");
        assert_eq!(span(3600), "1h");
        assert_eq!(span(86_399), "23h");
        assert_eq!(span(86_400), "1d");
        assert_eq!(span(3 * 86_400 + 5), "3d");
    }

    #[test]
    fn collapsed_process_poll_updates_the_snapshot_without_repainting_a_stable_count() {
        let mut state = Running::default();
        assert!(state.apply_process_list(Ok(vec![process(&["a"], false, 5)])));
        assert!(!state.apply_process_list(Ok(vec![process(&["b"], true, 10)])));
        assert_eq!(state.processes[0].threads, ["b"]);
        assert!(state.processes[0].running);

        state.open = true;
        assert!(state.apply_process_list(Ok(state.processes.clone())));
        assert!(state.apply_process_list(Ok(vec![])));
        assert!(!state.open);
        assert!(!state.apply_process_list(Ok(vec![])));
    }

    #[test]
    fn idle_label_reports_busy_or_time_since_activity() {
        let busy = process(&["a"], true, 900);
        assert_eq!(idle_label(1000, &busy), "busy");
        let idle = process(&["a"], false, 700);
        assert_eq!(idle_label(1000, &idle), "idle 5m");
        // Activity stamped in the future never shows a negative idle time.
        let future = process(&["a"], false, 2000);
        assert_eq!(idle_label(1000, &future), "idle 0s");
    }

    #[test]
    fn end_all_idle_selects_only_rows_that_are_not_running() {
        let rows = vec![
            process(&["a"], false, 0),
            process(&["b", "c"], true, 0),
            process(&["d", "e"], false, 0),
        ];
        let picked: Vec<Vec<String>> = idle_rows(&rows)
            .into_iter()
            .map(|p| p.threads.clone())
            .collect();
        assert_eq!(
            picked,
            vec![vec!["a".to_owned()], vec!["d".to_owned(), "e".to_owned()]]
        );
        assert!(idle_rows(&rows[1..2]).is_empty());
    }

    #[test]
    fn end_all_idle_skips_rows_that_started_a_turn_since_the_snapshot() {
        let seen = vec![process(&["a"], false, 0), process(&["d", "e"], false, 0)];
        let seen: Vec<_> = idle_rows(&seen)
            .into_iter()
            .map(|p| (p.scope.clone(), p.threads.clone()))
            .collect();
        // "a" went busy after the snapshot, "d e" is still idle, and "f" is new and idle.
        let fresh = vec![
            process(&["a"], true, 0),
            process(&["d", "e"], false, 0),
            process(&["f"], false, 0),
        ];
        assert_eq!(
            still_idle(&seen, &fresh),
            vec![(fresh[1].scope.clone(), vec!["d".to_owned(), "e".to_owned()])]
        );
        // A process whose thread set changed is not the one the user saw.
        let regrouped = vec![process(&["d"], false, 0)];
        assert!(still_idle(&seen, &regrouped).is_empty());
    }

    #[test]
    fn end_all_idle_skips_the_ready_draft_process() {
        let mut ready = process(&["draft"], false, 0);
        ready.draft = true;
        let rows = [ready, process(&["a"], false, 0)];
        let picked: Vec<_> = idle_rows(&rows).iter().map(|p| p.threads.clone()).collect();
        assert_eq!(picked, vec![vec!["a".to_owned()]]);
        let seen: Vec<_> = idle_rows(&rows)
            .into_iter()
            .map(|p| (p.scope.clone(), p.threads.clone()))
            .collect();
        assert_eq!(still_idle(&seen, &rows).len(), 1);
    }

    #[test]
    fn project_name_is_the_basename_or_the_whole_root() {
        assert_eq!(project_name(Path::new("/home/oscar/ox")), "ox");
        assert_eq!(project_name(Path::new("/")), "/");
    }
}
