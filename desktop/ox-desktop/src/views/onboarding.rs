//! First-run actions are scoped to the project and route of the visible pane.
use super::workbench::Workbench;
use crate::appearance::Step;
use crate::{activity, appearance::Palette, col, kit::button, platform, row, small};
use gpui::{prelude::FluentBuilder, *};
use ox_desk_core::runtime::Runtime;
use ox_desk_core::store::Route;
use std::time::{Duration, Instant};

pub(super) struct ConfigureProvider;
impl EventEmitter<ConfigureProvider> for Workbench {}
/// Switch this project to another route; `None` means the first API model (or add one).
pub(super) struct SwitchRoute(pub Option<Route>);
impl EventEmitter<SwitchRoute> for Workbench {}

const POLL_EVERY: Duration = Duration::from_millis(1500);
const POLL_FOR: Duration = Duration::from_secs(600);

/// Route for a fresh install, from local CLI state and any already-saved API model.
pub(super) fn first_launch_route() -> Route {
    platform::onboarding::initial_route(
        platform::onboarding::probe(Runtime::Codex),
        platform::onboarding::probe(Runtime::Claude),
        || {
            crate::settings::load()
                .ok()
                .and_then(|s| s.models.first().cloned())
        },
    )
}

/// Secondary choices beside the main action of an onboarding card.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Extras {
    pub configure_api: bool,
    pub use_other: bool,
}
pub(super) fn extras(issue: &Issue, other_installed: bool) -> Extras {
    match issue {
        Issue::Missing | Issue::Old(_) => Extras {
            configure_api: true,
            use_other: other_installed,
        },
        Issue::SignIn => Extras {
            configure_api: true,
            use_other: false,
        },
        Issue::Provider => Extras {
            configure_api: false,
            use_other: false,
        },
    }
}
pub(super) enum Issue {
    Missing,
    Old(String),
    SignIn,
    Provider,
}
impl Workbench {
    /// Keep the runtime the user selected when switching billing routes.
    pub(super) fn api_route(&self, model: String) -> Route {
        let runtime = self.route.runtime;
        if runtime.supports_api() {
            runtime.route(Some(model))
        } else {
            Route::codex_api(model)
        }
    }
    pub(super) fn first_run_issue(&self) -> Option<Issue> {
        if self.connecting {
            return None;
        }
        let state = self.store.as_ref().map(|s| s.state());
        if matches!(
            state,
            Some(
                ox_desk_core::store::Connection::Ready
                    | ox_desk_core::store::Connection::Connecting
            )
        ) {
            return None;
        }
        if let Some(ox_desk_core::store::Connection::Closed(e)) = state
            && e.starts_with(self.route.runtime.name())
            && e.contains("older than the required")
        {
            return Some(Issue::Old(e.clone()));
        }
        if let Some(e) = &self.launch_error {
            if e.starts_with(self.route.runtime.name()) && e.contains("older than the required") {
                return Some(Issue::Old(e.clone()));
            }
            if e.starts_with("Configure an OX API provider") {
                return Some(Issue::Provider);
            }
        }
        if self.route.runtime == Runtime::Antigravity
            && self.link.remote().is_some()
            && self
                .visible_error()
                .is_some_and(|e| e.contains("Install the official Antigravity CLI"))
        {
            return Some(Issue::Missing);
        }
        if self.link.remote().is_none() && platform::onboarding::missing(self.route.runtime) {
            return Some(Issue::Missing);
        }
        matches!(state, Some(ox_desk_core::store::Connection::SignInRequired))
            .then_some(Issue::SignIn)
    }
    pub(super) fn onboarding(&self, issue: Issue, cx: &Context<Self>) -> Div {
        let p = Palette::get(cx);
        let runtime = self.route.runtime;
        let name = runtime.name();
        if let Some(host) = self.link.remote().cloned() {
            let login = matches!(issue, Issue::SignIn);
            let install = matches!(issue, Issue::Missing);
            let notice = match issue {
                Issue::Old(e) => e,
                Issue::Provider => format!("Configure this provider on {}.", host.name),
                Issue::Missing => format!("Install {name} on {}.", host.name),
                Issue::SignIn => format!("Sign in to {name} on {}.", host.name),
            };
            return col()
                .items_center()
                .justify_center()
                .size_full()
                .gap_2()
                .child(small(p, notice))
                .when(install, |d| {
                    d.child(button(
                        p,
                        "remote-runtime-guide",
                        format!("Open {name} installation guide ↗"),
                        true,
                        true,
                        cx.listener(move |_, _, _, cx| cx.open_url(runtime.install_guide())),
                    ))
                })
                .when(login, |d| {
                    d.child(button(
                        p,
                        "remote-login",
                        "Sign in on host",
                        true,
                        true,
                        cx.listener(move |s, _, _, cx| {
                            if let Some(preset) =
                                ox_host_web::terminal_protocol::OpenTerminal::login(runtime)
                            {
                                s.awaiting_login = true;
                                s.launch_error =
                                    crate::terminal::open_remote(host.clone(), preset, cx).err();
                            }
                            cx.notify()
                        }),
                    ))
                })
                .child(button(
                    p,
                    "remote-reconnect",
                    "Reconnect",
                    false,
                    true,
                    cx.listener(|s, _, w, cx| s.reconnect(w, cx)),
                ));
        }
        let (title, note) = match &issue {
            Issue::Missing => (
                format!("Install {name}"),
                format!("Install the official {name} CLI to open a session in OX."),
            ),
            Issue::Old(v) => (format!("Update {name}"), v.clone()),
            Issue::Provider => (
                "Configure an API provider".into(),
                "Add an API key (Anthropic, OpenAI, OpenRouter or a local server) to use this model route.".into(),
            ),
            Issue::SignIn => (
                format!("Connect {name}"),
                format!("Sign in to your {} account to use {name} in OX.", runtime.account()),
            ),
        };
        let other = Runtime::ALL
            .into_iter()
            .find(|&other| other != runtime && !platform::onboarding::missing(other));
        let extras = extras(&issue, other.is_some());
        let api_active = self.route.is_api();
        let segment = |id: &'static str, label: &'static str, active: bool, to: Option<Route>| {
            button(
                p,
                id,
                label,
                active,
                true,
                cx.listener(move |_, _, _, cx| {
                    if !active {
                        cx.emit(SwitchRoute(to.clone()))
                    }
                }),
            )
        };
        let mut panel = col()
            .w_full()
            .max_w(px(470.))
            .gap_3()
            .child(
                row()
                    .gap_2()
                    .flex_wrap()
                    .children(
                        [
                            ("route-codex", Runtime::Codex),
                            ("route-claude", Runtime::Claude),
                            ("route-antigravity", Runtime::Antigravity),
                        ]
                        .map(|(id, candidate)| {
                            segment(
                                id,
                                candidate.name(),
                                !api_active && runtime == candidate,
                                Some(candidate.route(None)),
                            )
                        }),
                    )
                    .child(segment("route-api", "API key", api_active, None)),
            )
            .child(activity::ox(p, 0, 3.))
            .child(div().text_size(p.text(Step::Display)).child(title))
            .child(small(p, note));
        match issue {
            Issue::Missing | Issue::Old(_) => {
                if let Some((label, args)) =
                    platform::onboarding::installer(runtime, matches!(issue, Issue::Old(_)))
                {
                    panel=panel.child(button(p,"install-runtime",label,true,true,cx.listener(move|s,_,w,cx|{
                        match platform::onboarding::repair(&s.project,&args){
                            Ok(())=>{s.awaiting_login=true;s.launch_error=Some(format!("Finish installing {name} in the terminal, then return here."));s.poll_ready(w,cx);}
                            Err(e)=>s.launch_error=Some(e),
                        }
                        cx.notify()
                    })));
                } else {
                    panel = panel.child(button(
                        p,
                        "runtime-guide",
                        format!("Open {name} installation guide ↗"),
                        true,
                        true,
                        cx.listener(move |s, _, w, cx| {
                            s.awaiting_login = true;
                            s.poll_ready(w, cx);
                            cx.open_url(runtime.install_guide())
                        }),
                    ));
                }
            }
            Issue::Provider => {
                panel = panel.child(button(
                    p,
                    "setup-provider",
                    "Configure an API provider",
                    true,
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(ConfigureProvider)),
                ))
            }
            Issue::SignIn => {
                panel = panel.child(button(
                    p,
                    "native-sign-in",
                    format!("Sign in with {} ↗", runtime.account()),
                    true,
                    true,
                    cx.listener(|s, _, w, cx| s.start_sign_in(w, cx)),
                ));
            }
        }
        if extras.configure_api {
            panel = panel.child(button(
                p,
                "setup-provider",
                "Configure an API provider",
                false,
                true,
                cx.listener(|_, _, _, cx| cx.emit(ConfigureProvider)),
            ));
        }
        if extras.use_other
            && let Some(target) = other
        {
            panel = panel.child(button(
                p,
                "use-other-runtime",
                format!("Use installed {} instead", target.name()),
                false,
                true,
                cx.listener(move |_, _, _, cx| cx.emit(SwitchRoute(Some(target.route(None))))),
            ));
        }
        if platform::onboarding::is_home(&self.project) {
            panel = panel.child(
                row()
                    .gap_2()
                    .items_center()
                    .child(small(
                        p,
                        "Your project folder is your home directory. Sessions can edit files there.",
                    ))
                    .child(button(
                        p,
                        "choose-project",
                        "Choose project folder…",
                        false,
                        true,
                        cx.listener(|_, _, _, cx| cx.emit(super::workbench::empty::OpenProject)),
                    )),
            );
        }
        if self.awaiting_login {
            panel = panel.child(button(
                p,
                "retry-connection",
                "Check connection",
                false,
                true,
                cx.listener(|s, _, w, cx| s.reconnect(w, cx)),
            ));
        }
        if let Some(message) = self.launch_error.as_ref().filter(|s| {
            !s.contains("older than the required") && !s.starts_with("Configure an OX API provider")
        }) {
            panel = panel.child(small(p, message.clone()));
        }
        col()
            .size_full()
            .items_center()
            .justify_center()
            .px_6()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .child(panel)
    }
    /// While an install or sign-in finishes elsewhere, reconnect by itself once the CLI turns
    /// ready. Checks are file lookups only. Stops on success, when waiting ends, or after 10 min.
    pub(super) fn poll_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let runtime = self.route.runtime;
        let api = self.route.is_api();
        let ready = move || {
            if runtime == Runtime::Antigravity {
                !platform::onboarding::missing(runtime)
            } else {
                platform::onboarding::ready(runtime, api)
            }
        };
        cx.spawn_in(window, async move |view, cx| {
            let start = Instant::now();
            let mut was_ready = ready();
            while start.elapsed() < POLL_FOR {
                cx.background_executor().timer(POLL_EVERY).await;
                let now = cx.background_spawn(async move { ready() }).await;
                let keep = view.update_in(cx, |s, w, cx| {
                    if !s.awaiting_login {
                        return false;
                    }
                    if now && !was_ready {
                        s.reconnect(w, cx);
                    }
                    true
                });
                was_ready = now;
                if !keep.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }
    fn start_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.awaiting_login = true;
        let runtime = self.route.runtime;
        if runtime != Runtime::Antigravity {
            self.poll_ready(window, cx);
        }
        let login = cx.background_spawn(async move { platform::launch_login(runtime, &project) });
        cx.spawn(async move |view, cx| {
            let result = login.await;
            let _ = view.update(cx, |s, cx| {
                if s.awaiting_login {
                    s.launch_error = Some(match result {
                        Ok(()) => {
                            format!(
                                "Finish signing in with {}, then return to this window.",
                                runtime.name()
                            )
                        }
                        Err(error) => {
                            s.awaiting_login = false;
                            error
                        }
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[core::prelude::v1::test]
    fn issues_offer_the_right_extra_actions() {
        let both = |c, o| Extras {
            configure_api: c,
            use_other: o,
        };
        assert_eq!(extras(&Issue::Missing, true), both(true, true));
        assert_eq!(extras(&Issue::Old("x".into()), false), both(true, false));
        assert_eq!(extras(&Issue::SignIn, true), both(true, false));
        assert_eq!(extras(&Issue::Provider, true), both(false, false));
    }
}
