mod worker_links;
use crate::{
    appearance::{Palette, Step},
    kit::{button, field},
    platform::hosts::{self, KeyReview, OpenHost, SavedHost},
};
use gpui::{prelude::*, *};
use gpui_component::input::{Input, InputState};
use ox_desk_core::host_policy::HostPolicy;
use ox_host_web::host_profile::{Edit, HostProfile};
use ox_host_web::host_updates::{Channel, Command, Mode, Status};
use std::io;

pub(crate) struct Hosts {
    links: Option<worker_links::Directory>,
    nearby: Vec<OpenHost>,
    scanning: bool,
    worker_address: Entity<InputState>,
    worker_code: Entity<InputState>,
    address: Entity<InputState>,
    profile_fields: Option<(HostProfile, Entity<InputState>, Entity<InputState>)>,
    code: Entity<InputState>,
    hosts: Vec<SavedHost>,
    selected: Option<SavedHost>,
    policy: Option<HostPolicy>,
    pair_policy: Option<HostPolicy>,
    choosing_pair_policy: bool,
    status: Option<Status>,
    review: Option<KeyReview>,
    busy: bool,
    notice: String,
}
impl Hosts {
    pub fn new(w: &mut Window, cx: &mut Context<Self>) -> Self {
        let loaded = hosts::list();
        Self {
            links: None,
            nearby: Vec::new(),
            scanning: false,
            worker_address: field("", "Worker invitation or host URL", false, w, cx),
            worker_code: field("", "Code (for a host URL)", true, w, cx),
            profile_fields: None,
            address: field(
                "",
                "Invitation link or https://host.tailnet.ts.net:8443",
                false,
                w,
                cx,
            ),
            code: field("", "Pairing code (when entering a URL)", false, w, cx),
            notice: loaded
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default(),
            hosts: loaded.unwrap_or_default(),
            selected: None,
            policy: None,
            pair_policy: None,
            choosing_pair_policy: false,
            status: None,
            review: None,
            busy: false,
        }
    }
    fn work<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        job: impl FnOnce() -> io::Result<T> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.notice.clear();
        let job = cx.background_spawn(async move { job() });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok(value) => apply(s, value, cx),
                    Err(e) => s.notice = e.to_string(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn select(&mut self, host: Option<SavedHost>, cx: &mut Context<Self>) {
        self.links = None;
        self.selected = host;
        self.policy = None;
        self.profile_fields = None;
        self.status = None;
        self.review = None;
        self.update(Command::Status, cx);
    }
    fn update(&mut self, command: Command, cx: &mut Context<Self>) {
        let host = self.selected.clone();
        self.work(
            cx,
            move || match host {
                Some(host) => host.updates(command),
                None => {
                    let paths = ox_host_service::Paths::discover()?;
                    ox_host_web::access::status(&paths).map_err(io::Error::other)?;
                    ox_host_web::admin::request(
                        &paths.state_dir,
                        &ox_host_web::admin::Request::Update { command },
                    )?
                    .update
                    .ok_or_else(|| {
                        io::Error::other("The host needs an update to manage its settings here")
                    })
                }
            },
            |s, status, _| s.status = Some(status),
        )
    }
    fn host_policy(host: Option<&SavedHost>, set: Option<HostPolicy>) -> io::Result<HostPolicy> {
        if let Some(h) = host {
            let response =
                h.settings(serde_json::json!({"action":"host_mode_policy","set":set}))?;
            serde_json::from_value(response["policy"].clone())
                .map_err(|_| io::Error::other("Update the host to manage its mode policy."))
        } else {
            let paths = ox_host_service::Paths::discover()?;
            if let Some(policy) = set {
                policy.save(&paths.state_dir)?;
                return Ok(policy);
            }
            HostPolicy::read(&paths.state_dir)
        }
    }
    fn edit_policy(&mut self, set: Option<HostPolicy>, cx: &mut Context<Self>) {
        let host = self.selected.clone();
        self.work(
            cx,
            move || Self::host_policy(host.as_ref(), set),
            |s, policy, _| s.policy = Some(policy),
        );
    }
    fn profile(host: Option<SavedHost>, edit: Option<Edit>) -> io::Result<HostProfile> {
        match host {
            Some(h) => Ok(serde_json::from_value(h.settings(
                serde_json::json!({"action":"host_profile","edit":edit}),
            )?)?),
            None => {
                let paths = ox_host_service::Paths::discover()?;
                ox_host_web::access::status(&paths).map_err(io::Error::other)?;
                ox_host_web::admin::request(
                    &paths.state_dir,
                    &ox_host_web::admin::Request::HostProfile { edit },
                )?
                .profile
                .ok_or_else(|| io::Error::other("Update this host to edit its details."))
            }
        }
    }
    fn edit_profile(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let host = self.selected.clone();
        let task = cx.background_spawn(async move { Self::profile(host, None) });
        self.busy = true;
        self.notice.clear();
        cx.notify();
        cx.spawn_in(w, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Ok(p) => {
                        let name =
                            field(&p.name, "Host name (defaults to system name)", false, w, cx);
                        let labels = field(
                            &p.labels.join(", "),
                            "Labels, separated by commas",
                            false,
                            w,
                            cx,
                        );
                        s.profile_fields = Some((p, name, labels));
                    }
                    Err(e) => s.notice = e.to_string(),
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn save_profile(&mut self, cx: &mut Context<Self>) {
        let Some((before, name, labels)) = &self.profile_fields else {
            return;
        };
        let after = HostProfile {
            name: name.read(cx).value().to_string(),
            labels: labels
                .read(cx)
                .value()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        }
        .normalized();
        let after = match after {
            Ok(after) => after,
            Err(e) => {
                self.notice = e.to_string();
                cx.notify();
                return;
            }
        };
        let edit = Edit {
            before: before.clone(),
            after,
        };
        let host = self.selected.clone();
        self.work(
            cx,
            move || Self::profile(host, Some(edit)),
            |s, _, _| {
                s.profile_fields = None;
                s.notice = "Host details saved.".into();
            },
        );
    }
    fn remove(&mut self, host: SavedHost, revoke: bool, cx: &mut Context<Self>) {
        self.work(
            cx,
            move || {
                if revoke {
                    host.revoke()?;
                } else {
                    host.forget()?;
                }
                hosts::list()
            },
            move |s, rows, cx| {
                s.hosts = rows;
                s.selected = None;
                s.links = None;
                s.policy = None;
                s.profile_fields = None;
                s.review = None;
                s.status = None;
                cx.set_global(HostDirectory(s.hosts.clone()));
                s.notice = if revoke {
                    "This desktop has been revoked."
                } else {
                    "Host forgotten on this desktop."
                }
                .into();
            },
        );
    }
    fn add(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let address = self.address.read(cx).value().to_string();
        let code = self.code.read(cx).value().to_string();
        // An address with no code and no invitation fragment means the other
        // machine is waiting in a pairing window, so nothing is redeemed.
        let paired = move || {
            if hosts::window_address(&address, &code) {
                hosts::pair_window(&address, &hosts::device_label())
            } else {
                hosts::pair(&address, &code, &hosts::device_label())
            }
        };
        self.pair(w, cx, paired, true);
    }
    /// Find tailnet peers whose operator is waiting for a device right now.
    fn scan(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.notice.clear();
        cx.notify();
        let task = cx.background_spawn(async move { hosts::waiting_hosts() });
        cx.spawn_in(w, async move |this, cx| {
            let found = task.await;
            let _ = this.update_in(cx, |s, _, cx| {
                s.scanning = false;
                s.notice = if found.is_empty() {
                    "No host on this tailnet is waiting to pair. Open a pairing window there, or pair with its invitation."
                        .into()
                } else {
                    format!("{} host(s) waiting to pair.", found.len())
                };
                s.nearby = found;
                cx.notify();
            });
        })
        .detach();
    }
    fn pair_nearby(&mut self, index: usize, w: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.nearby.get(index).cloned() else {
            return;
        };
        let paired = move || hosts::pair_window(&host.origin, &hosts::device_label());
        self.pair(w, cx, paired, false);
    }
    fn pair(
        &mut self,
        w: &mut Window,
        cx: &mut Context<Self>,
        paired: impl FnOnce() -> io::Result<SavedHost> + Send + 'static,
        clear_fields: bool,
    ) {
        let choice = self.pair_policy;
        let task = cx.background_spawn(async move {
            let host = paired()?;
            let policy = choice
                .map(|selected| Self::host_policy(Some(&host), Some(selected)))
                .transpose();
            Ok::<_, io::Error>((host, policy))
        });
        self.busy = true;
        self.notice.clear();
        cx.notify();
        cx.spawn_in(w, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Ok((host, policy)) => {
                        s.pair_policy = None;
                        s.choosing_pair_policy = false;
                        if clear_fields {
                            s.address.update(cx, |v, cx| v.set_value("", w, cx));
                            s.code.update(cx, |v, cx| v.set_value("", w, cx));
                        }
                        s.nearby.clear();
                        s.hosts
                            .retain(|h| h.id != host.id && h.origin != host.origin);
                        s.hosts.push(host.clone());
                        cx.set_global(HostDirectory(s.hosts.clone()));
                        s.select(Some(host), cx);
                        match policy {
                            Ok(current) => s.policy = current,
                            Err(error) => {
                                s.notice = format!(
                                    "Host paired. Its mode policy could not be changed: {error}"
                                )
                            }
                        }
                    }
                    Err(e) => s.notice = e.to_string(),
                }
                cx.notify()
            });
        })
        .detach()
    }
}
#[derive(Default)]
pub(crate) struct HostDirectory(pub Vec<SavedHost>);
impl Global for HostDirectory {}
impl Render for Hosts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut list = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(p.text(Step::Title)).child("Hosts"));
        if !cx.global::<crate::app::ClientOnly>().0 {
            list = list.child(button(
                p,
                "local-host",
                "This computer",
                self.selected.is_none(),
                !self.busy,
                cx.listener(|s, _, _, cx| s.select(None, cx)),
            ));
        }
        for host in &self.hosts {
            let host = host.clone();
            list = list.child(button(
                p,
                format!("host-{}", host.id),
                host.name.clone(),
                self.selected.as_ref().is_some_and(|h| h.id == host.id),
                !self.busy,
                cx.listener(move |s, _, _, cx| s.select(Some(host.clone()), cx)),
            ));
        }
        if self.selected.is_some() || !cx.global::<crate::app::ClientOnly>().0 {
            list = list.child(self.worker_controls(p, cx));
        }
        if let Some(host) = self.selected.clone() {
            let revoke = host.clone();
            let forget = host.clone();
            list = list.child(host.origin.clone()).child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(button(
                        p,
                        "review-keys",
                        "Send API keys…",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| {
                            let host = host.clone();
                            s.work(
                                cx,
                                move || {
                                    KeyReview::prepare(
                                        host,
                                        crate::settings::load().map_err(io::Error::other)?,
                                    )
                                },
                                |s, r, _| s.review = Some(r),
                            );
                        }),
                    ))
                    .child(button(
                        p,
                        "revoke-desktop",
                        "Revoke this desktop",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| {
                            s.remove(revoke.clone(), true, cx);
                        }),
                    ))
                    .child(button(
                        p,
                        "forget-host",
                        "Forget on this desktop",
                        false,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| s.remove(forget.clone(), false, cx)),
                    )),
            );
        }
        if self.selected.is_some() || !cx.global::<crate::app::ClientOnly>().0 {
            if let Some((_, name, labels)) = &self.profile_fields {
                list = list
                    .child("Host name")
                    .child(Input::new(name))
                    .child("Labels")
                    .child(Input::new(labels))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button(
                                p,
                                "save-host-details",
                                "Save",
                                true,
                                !self.busy,
                                cx.listener(|s, _, _, cx| s.save_profile(cx)),
                            ))
                            .child(button(
                                p,
                                "cancel-host-details",
                                "Cancel",
                                false,
                                !self.busy,
                                cx.listener(|s, _, _, cx| {
                                    s.profile_fields = None;
                                    cx.notify();
                                }),
                            )),
                    );
            } else {
                list = list.child(button(
                    p,
                    "edit-host-details",
                    "Edit host name and labels…",
                    false,
                    !self.busy,
                    cx.listener(|s, _, w, cx| s.edit_profile(w, cx)),
                ));
            }
        }
        if let Some(review) = &self.review {
            list=list.child(div().text_color(rgb(p.warning)).child(format!("Send selected API keys to {} ({}). Anyone with access to that host’s OX key store can use the keys and incur charges. Revoking this desktop later leaves the copies on the host.",review.host.name,review.host.origin)));
            for (i, row) in review.rows.iter().enumerate() {
                let key = &row.key;
                let status = &row.status;
                let label = if key.scope == "https://api.search.brave.com/" {
                    format!("Brave Search ({})", key.name)
                } else {
                    key.name.clone()
                };
                list = list.child(button(
                    p,
                    ("transfer-key", i),
                    format!(
                        "{} {label} · {} · {status}",
                        if row.selected { "☑" } else { "☐" },
                        key.scope
                    ),
                    false,
                    !self.busy && row.available,
                    cx.listener(move |s, _, _, cx| {
                        if let Some(r) = &mut s.review {
                            r.rows[i].selected = !r.rows[i].selected;
                        }
                        cx.notify();
                    }),
                ));
            }
            let count = review.rows.iter().filter(|row| row.selected).count();
            if review.rows.is_empty() {
                list = list.child("This computer has no configured API keys to send.");
            }
            list = list.child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        p,
                        "send-keys",
                        format!("Send {count} selected"),
                        true,
                        count > 0 && !self.busy,
                        cx.listener(|s, _, _, cx| {
                            if let Some(r) = s.review.take() {
                                s.work(
                                    cx,
                                    move || {
                                        let host = r.host.name.clone();
                                        r.send(crate::settings::request)
                                            .map(|receipt| receipt.message(&host))
                                    },
                                    |s, message, _| s.notice = message,
                                );
                            }
                        }),
                    ))
                    .child(button(
                        p,
                        "cancel-keys",
                        "Cancel",
                        false,
                        !self.busy,
                        cx.listener(|s, _, _, cx| {
                            s.review = None;
                            cx.notify()
                        }),
                    )),
            );
        }
        if self.selected.is_some() || !cx.global::<crate::app::ClientOnly>().0 {
            if let Some(current) = self.policy {
                let mut choices = div().flex().gap_2().flex_wrap();
                for (index, mode) in HostPolicy::ALL.into_iter().enumerate() {
                    choices = choices.child(button(
                        p,
                        ("host-mode", index),
                        mode.label(),
                        current == mode,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| s.edit_policy(Some(mode), cx)),
                    ));
                }
                list = list.child("Host mode policy").child(choices);
            } else {
                list = list.child(button(
                    p,
                    "edit-host-mode",
                    "Host mode policy…",
                    false,
                    !self.busy,
                    cx.listener(|s, _, _, cx| s.edit_policy(None, cx)),
                ));
            }
        }
        if let Some(status) = &self.status {
            list = list.child(format!("OX {} · {}", status.installed, status.message));
            if status.managed {
                let mut controls = div().flex().gap_2().flex_wrap();
                for (label, command, selected) in [
                    (
                        "Automatic when idle",
                        Command::SetMode {
                            mode: Mode::Automatic,
                        },
                        status.mode == Mode::Automatic,
                    ),
                    (
                        "Download only",
                        Command::SetMode {
                            mode: Mode::Download,
                        },
                        status.mode == Mode::Download,
                    ),
                    (
                        "Manual",
                        Command::SetMode { mode: Mode::Manual },
                        status.mode == Mode::Manual,
                    ),
                    (
                        "Stable",
                        Command::SetChannel {
                            channel: Channel::Stable,
                        },
                        status.channel == Channel::Stable,
                    ),
                    (
                        "Dev",
                        Command::SetChannel {
                            channel: Channel::Dev,
                        },
                        status.channel == Channel::Dev,
                    ),
                    ("Update when idle", Command::UpdateWhenIdle, false),
                    ("Refresh", Command::Status, false),
                ] {
                    controls = controls.child(button(
                        p,
                        label,
                        label,
                        selected,
                        !self.busy,
                        cx.listener(move |s, _, _, cx| s.update(command, cx)),
                    ));
                }
                if status.scheduled {
                    controls = controls.child(button(
                        p,
                        "cancel-host-update",
                        "Cancel scheduled update",
                        false,
                        !self.busy,
                        cx.listener(|s, _, _, cx| s.update(Command::Cancel, cx)),
                    ));
                }
                list = list.child(controls);
            }
        }
        list = list
            .child(div().text_size(p.text(Step::Title)).child("Add host"))
            .child("Run ox host pair on the other machine. Add --terminal to allow shells and subscription sign-in from this desktop. Leave the code empty when that host is waiting in a pairing window.")
            .child(Input::new(&self.address))
            .child(Input::new(&self.code))
            .child(button(p,"pair-mode", format!("Mode policy · {}", self.pair_policy.map_or("Use host setting", HostPolicy::label)), false, !self.busy,
                cx.listener(|s,_,_,cx| {s.choosing_pair_policy = !s.choosing_pair_policy; cx.notify();})));
        if self.choosing_pair_policy {
            let mut modes = div().flex().gap_2().flex_wrap();
            for (index, choice) in std::iter::once(None)
                .chain(HostPolicy::ALL.into_iter().map(Some))
                .enumerate()
            {
                modes = modes.child(button(
                    p,
                    ("new-host-mode", index),
                    choice.map_or("Use host setting", HostPolicy::label),
                    choice == self.pair_policy,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| {
                        s.pair_policy = choice;
                        s.choosing_pair_policy = false;
                        cx.notify();
                    }),
                ));
            }
            list = list.child(modes);
        }
        list = list.child(button(
            p,
            "add-host",
            "Pair host",
            true,
            !self.busy,
            cx.listener(|s, _, w, cx| s.add(w, cx)),
        ));
        list = list
            .child(
                button(
                    p,
                    "scan-hosts",
                    if self.scanning {
                        "Looking for hosts…"
                    } else {
                        "Look for hosts waiting to pair"
                    },
                    false,
                    !self.busy && !self.scanning,
                    cx.listener(|s, _, w, cx| s.scan(w, cx)),
                ),
            )
            .child("A host whose operator opened a pairing window appears here. Pairing needs no code or link.");
        for (index, host) in self.nearby.iter().enumerate() {
            let tags = if host.tags.is_empty() {
                String::new()
            } else {
                format!(" · {}", host.tags.join(", "))
            };
            list = list.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(format!("{}{tags}", host.name))
                    .child(
                        div()
                            .text_size(p.text(Step::Meta))
                            .child(host.origin.clone()),
                    )
                    .child(button(
                        p,
                        ("pair-nearby", index),
                        "Pair",
                        true,
                        !self.busy,
                        cx.listener(move |s, _, w, cx| s.pair_nearby(index, w, cx)),
                    )),
            );
        }
        if self.busy {
            list = list.child("Contacting host…");
        }
        if !self.notice.is_empty() {
            list = list.child(self.notice.clone());
        }
        list
    }
}
