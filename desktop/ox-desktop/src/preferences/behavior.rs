//! Desktop-owned display and input preferences.
use super::{Page, Preferences};
use crate::appearance::Palette;
use crate::appearance::Step;
use gpui::{prelude::*, *};
use ox_desk_core::desktop_behavior::{self, Outcome, Snapshot};
pub use ox_desk_core::desktop_behavior::{Behavior, ThemeSource, WorkerApprovals};
use ox_desk_core::runtime::{Access, Runtime};
pub struct Active(pub Behavior);
impl Global for Active {}
impl std::ops::Deref for Active {
    type Target = Behavior;
    fn deref(&self) -> &Behavior {
        &self.0
    }
}
/// "Same as terminal" plus every native preset of `runtime`, as start-mode choices.
fn start_choices(runtime: Runtime, b: Behavior) -> Vec<(&'static str, Behavior)> {
    if runtime == Runtime::OpenCode {
        return Vec::new();
    }
    let with = |start: Option<Access>| match runtime {
        Runtime::OpenCode => b,
        Runtime::Claude => Behavior {
            claude_start: start,
            ..b
        },
        Runtime::Codex => Behavior {
            codex_start: start,
            ..b
        },
        Runtime::Antigravity => Behavior {
            antigravity_start: start,
            ..b
        },
    };
    std::iter::once(("Same as terminal", with(None)))
        .chain(Access::choices(runtime).iter().map(|&mode| {
            let label = match mode {
                Access::Default => "Ask before edits",
                other => other.label(),
            };
            (label, with(Some(mode)))
        }))
        .collect()
}
impl Preferences {
    pub(super) fn refresh_remote_behavior(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.link.remote().cloned() else {
            return;
        };
        self.remote_behavior = None;
        let read = cx.background_spawn(async move {
            host.settings(serde_json::json!({"action":"read_behavior"}))
                .and_then(|v| Ok(serde_json::from_value::<Snapshot>(v)?.behavior))
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |view, cx| {
            let result = read.await;
            let _ = view.update(cx, |s, cx| {
                s.remote_behavior = Some(result);
                cx.notify();
            });
        })
        .detach();
    }
    fn preference_choices(
        &self,
        label: &'static str,
        options: &[(&'static str, Behavior)],
        current: Behavior,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut controls = div().flex().gap_1().flex_wrap();
        for &(caption, next) in options {
            controls = controls.child(crate::kit::button(
                p,
                SharedString::from(format!("{label}:{caption}")),
                caption,
                next == current,
                true,
                cx.listener(move |s, _, _, cx| s.save_behavior(current, next, cx)),
            ));
        }
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(162.))
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.muted))
                    .child(label),
            )
            .child(controls)
    }
    fn save_behavior(&mut self, current: Behavior, next: Behavior, cx: &mut Context<Self>) {
        if current == next || self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        let host = if self.page == Page::Accounts {
            self.link.remote().cloned()
        } else {
            None
        };
        let remote = host.is_some();
        let write = cx.background_spawn(async move {
            let revision = desktop_behavior::revision(current);
            let outcome = match host {
                Some(host) => serde_json::from_value(host.settings(serde_json::json!({
                    "action":"save_behavior", "revision":revision, "behavior":next
                }))?)?,
                None => desktop_behavior::write(&revision, next)?,
            };
            match outcome {
                Outcome::Applied => Ok(()),
                Outcome::ChangedOnDisk { .. } => Err(std::io::Error::other(
                    "Preferences changed on another device. Review the current setting.",
                )),
            }
        });
        cx.spawn(async move |view, cx| {
            let result = write.await;
            let _ = view.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok(()) if remote => {
                        s.remote_behavior = Some(Ok(next));
                        s.report(Ok(()), "Host preference saved.", cx);
                    }
                    Ok(()) => {
                        cx.set_global(Active(next));
                        crate::appearance::refresh(cx);
                        crate::sky::reload(cx);
                        s.report(Ok(()), "Preference saved.", cx);
                    }
                    Err(e) => {
                        if remote {
                            s.refresh_remote_behavior(cx);
                        }
                        s.report(Err(format!("Could not save preference: {e}")), "", cx);
                    }
                }
            });
        })
        .detach();
    }
    /// How agents start and who answers their workers, on the Accounts page.
    pub(super) fn agents_card(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let b = if self.link.remote().is_some() {
            match self.remote_behavior.as_ref() {
                Some(Ok(b)) => *b,
                Some(Err(e)) => {
                    return div().child(format!("Could not read host agent settings: {e}"));
                }
                None => return div().child("Loading host agent settings…"),
            }
        } else {
            cx.global::<Active>().0
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .border_1()
            .border_color(rgb(p.line))
            .rounded_lg()
            .bg(rgb(p.panel))
            .child(div().text_size(p.text(Step::Title)).child("Agents"))
            .child(self.preference_choices(
                "New Claude sessions",
                &start_choices(Runtime::Claude, b),
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "New Codex sessions",
                &start_choices(Runtime::Codex, b),
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "New Antigravity sessions",
                &start_choices(Runtime::Antigravity, b),
                b,
                p,
                cx,
            ))
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("Same as terminal uses the agent’s own permissions. A mode you pick in the composer applies to that one session."),
            )
            .child(self.preference_choices(
                "Worker requests",
                &[
                    (
                        "Parent agent answers",
                        Behavior {
                            worker_approvals: WorkerApprovals::Parent,
                            ..b
                        },
                    ),
                    (
                        "I answer",
                        Behavior {
                            worker_approvals: WorkerApprovals::Operator,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("A parent agent answers its workers' questions, and approves their actions when its own mode runs without asking you (Auto, Auto-review or YOLO). Read-only workers and mode changes always ask you. You can still answer any request yourself."),
            )
    }
    pub(super) fn notification_page(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        use crate::app::notifications::Settings;
        use gpui_component::switch::Switch;
        use ox_desk_core::host_protocol::NotificationKind;

        let current = *cx.default_global::<Settings>();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_size(p.text(Step::Title))
                    .child("Desktop notifications"),
            )
            .child(
                div()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.muted))
                    .child("Alerts from connected hosts appear on this computer when OX is in the background."),
            )
            .child(
                Switch::new("desktop-alerts-enabled")
                    .label("Receive desktop notifications")
                    .checked(current.enabled)
                    .color(rgb(p.accent))
                    .on_change(cx.listener(|s, checked: &bool, _, cx| {
                        let mut settings = *cx.default_global::<Settings>();
                        settings.enabled = *checked;
                        s.save_notifications(settings, cx);
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .border_t_1()
                    .border_color(rgb(p.line))
                    .pt_4()
                    .children([
                        ("desktop-alert-approval", "Approval needed", NotificationKind::Approval),
                        ("desktop-alert-question", "Question asked", NotificationKind::Question),
                        ("desktop-alert-finished", "Turn done", NotificationKind::Finished),
                        ("desktop-alert-failed", "Failed", NotificationKind::Failed),
                    ].into_iter().map(|(id, label, kind)| {
                        Switch::new(id)
                            .label(label)
                            .checked(current.selected(kind))
                            .color(rgb(p.accent))
                            .on_change(cx.listener(move |s, checked: &bool, _, cx| {
                                let mut settings = *cx.default_global::<Settings>();
                                settings.set(kind, *checked);
                                s.save_notifications(settings, cx);
                            }))
                    }))
                    .child(
                        Switch::new("desktop-alert-goal")
                            .label("Goal reached")
                            .checked(current.goal)
                            .color(rgb(p.accent))
                            .on_change(cx.listener(|s, checked: &bool, _, cx| {
                                let mut settings = *cx.default_global::<Settings>();
                                settings.goal = *checked;
                                s.save_notifications(settings, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(p.muted))
                            .child("When Codex completes a goal and a turn together, OX sends one alert."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(p.line))
                    .pt_4()
                    .child(
                        Switch::new("desktop-alert-titles")
                            .label("Include thread titles")
                            .checked(current.titles)
                            .color(rgb(p.accent))
                            .on_change(cx.listener(|s, checked: &bool, _, cx| {
                                let mut settings = *cx.default_global::<Settings>();
                                settings.titles = *checked;
                                s.save_notifications(settings, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(p.text(Step::Meta))
                            .text_color(rgb(p.muted))
                            .child("Titles may appear on your lock screen."),
                    ),
            )
    }
    fn save_notifications(
        &mut self,
        next: crate::app::notifications::Settings,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = next.save(cx) {
            self.message = e.to_string();
            self.error = true;
        }
        cx.notify();
    }
    pub(super) fn behavior_page(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let b = cx.global::<Active>().0;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.preference_choices(
                "Send messages",
                &[
                    (
                        "Ctrl/Cmd+Enter sends · Enter for newline",
                        Behavior {
                            enter_sends: false,
                            ..b
                        },
                    ),
                    (
                        "Enter sends · Shift+Enter for newline (default)",
                        Behavior {
                            enter_sends: true,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "Theme",
                &[
                    (
                        "Follow system",
                        Behavior {
                            theme: ThemeSource::System,
                            ..b
                        },
                    ),
                    (
                        "OX Arcade light",
                        Behavior {
                            theme: ThemeSource::Light,
                            ..b
                        },
                    ),
                    (
                        "OX Arcade dark",
                        Behavior {
                            theme: ThemeSource::Dark,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "Text size",
                &[
                    ("Small", Behavior { font_size: 14, ..b }),
                    ("Default", Behavior { font_size: 16, ..b }),
                    ("Large", Behavior { font_size: 18, ..b }),
                    ("Extra large", Behavior { font_size: 20, ..b }),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "Animation",
                &[
                    (
                        "Follow system",
                        Behavior {
                            reduce_motion: false,
                            ..b
                        },
                    ),
                    (
                        "Reduce motion",
                        Behavior {
                            reduce_motion: true,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(self.preference_choices(
                "OX updates",
                &[
                    ("Download automatically", Behavior { background_updates: true, ..b }),
                    ("Check manually", Behavior { background_updates: false, ..b }),
                ], b, p, cx,
            ))
            .child(self.preference_choices(
                "Live weather (MET Norway)",
                &[
                    (
                        "Weather on",
                        Behavior {
                            live_weather: true,
                            ..b
                        },
                    ),
                    (
                        "Weather off",
                        Behavior {
                            live_weather: false,
                            ..b
                        },
                    ),
                ],
                b,
                p,
                cx,
            ))
            .child(
                div()
                    .text_size(p.text(Step::Meta))
                    .text_color(rgb(p.muted))
                    .child("Only a city-level location derived from your timezone is sent to api.met.no. Time of day works without it."),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::Behavior;

    #[test]
    fn enter_sends_by_default_and_saved_newline_preference_wins() {
        assert!(Behavior::default().enter_sends);
        assert!(serde_json::from_str::<Behavior>("{}").unwrap().enter_sends);
        assert!(
            !serde_json::from_str::<Behavior>(r#"{"enter_sends":false}"#)
                .unwrap()
                .enter_sends
        );
    }
}
