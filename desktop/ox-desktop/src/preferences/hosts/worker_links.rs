//! Manage the selected host's worker links. The local and remote controls
//! operate on the host's private registry, independent of desktop pairing.
use super::*;
use ox_desk_core::modes::Level;
use ox_host_web::admin::{self, WorkerGrant, WorkerLinkAction as Action};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Target {
    id: String,
    name: String,
    origin: String,
}
#[derive(Deserialize)]
pub(super) struct Directory {
    hosts: Vec<Target>,
    available: Vec<Target>,
    incoming: Vec<admin::Device>,
}

fn local(request: admin::Request) -> io::Result<admin::Reply> {
    let paths = ox_host_service::Paths::discover()?;
    ox_host_web::access::status(&paths).map_err(io::Error::other)?;
    admin::request(&paths.state_dir, &request)
}
#[derive(Deserialize)]
struct LinkChange {
    revoked: Option<bool>,
}
fn call<T: serde::de::DeserializeOwned>(
    host: Option<&SavedHost>,
    command: Action,
) -> io::Result<T> {
    let reply = match host {
        Some(h) => h.settings(json!(command)),
        None => local(admin::Request::WorkerLinks { command })?
            .worker_links
            .ok_or_else(|| io::Error::other("Update the host to manage its worker links.")),
    }?;
    Ok(serde_json::from_value(reply)?)
}
fn directory(host: Option<&SavedHost>) -> io::Result<Directory> {
    call(host, Action::WorkerLinks)
}

impl Hosts {
    pub(in crate::preferences) fn open_worker_links(
        &mut self,
        source: Option<SavedHost>,
        cx: &mut Context<Self>,
    ) {
        self.selected = source;
        self.link_action(Action::WorkerLinks, cx);
    }

    fn link_action(&mut self, command: Action, cx: &mut Context<Self>) {
        let selected = self.selected.clone();
        self.work(cx, move || {
            let mut message = String::new();
            if !matches!(command, Action::WorkerLinks) {
                let result:LinkChange = call(selected.as_ref(), command)?;
                if result.revoked == Some(false) { message = "Removed here. Revoke the incoming link on the other host when it is reachable.".into(); }
            }
            Ok((directory(selected.as_ref())?, message))
        }, |s, (links, message), _| { s.links = Some(links); s.notice = message; })
    }
    fn invite_worker(&mut self, cx: &mut Context<Self>) {
        let selected = self.selected.clone();
        self.work(cx, move || {
            let invite: admin::Pairing = match selected {
                Some(host) => serde_json::from_value(host.settings(json!({"action":"invite_worker_host"}))?)?,
                None => {
                    let paths = ox_host_service::Paths::discover()?;
                    ox_host_web::access::run(&paths, ox_host_web::access::Operation::PairWorker { login: None }).map_err(io::Error::other)?.host.pairing
                        .ok_or_else(||io::Error::other("The host did not return a worker invitation."))?
                }
            };
            Ok(invite.url)
        }, |s, url, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
            s.notice = "Invitation copied. Paste it in Worker host links on the sending host within five minutes.".into();
        });
    }
    fn worker_access(
        &mut self,
        id: String,
        before: WorkerGrant,
        may_launch: bool,
        ceiling: Level,
        may_sync_memory: bool,
        cx: &mut Context<Self>,
    ) {
        let selected = self.selected.clone();
        self.work(cx, move || {
            match selected.as_ref() {
                Some(host) => { host.settings(json!({"action":"edit_worker_link", "id":id, "before":before, "may_launch":may_launch, "ceiling":ceiling, "may_sync_memory":may_sync_memory}))?; },
                None => { local(admin::Request::WorkerAccess { id, before, may_launch, ceiling, may_sync_memory: Some(may_sync_memory) })?; },
            }
            directory(selected.as_ref())
        }, |s, links, _| { s.links = Some(links); });
    }
    fn revoke_incoming(&mut self, id: String, cx: &mut Context<Self>) {
        let selected = self.selected.clone();
        self.work(
            cx,
            move || {
                match selected.as_ref() {
                    Some(host) => {
                        host.settings(json!({"action":"revoke_device", "id":id}))?;
                    }
                    None => {
                        local(admin::Request::Revoke { id })?;
                    }
                }
                directory(selected.as_ref())
            },
            |s, links, _| s.links = Some(links),
        );
    }
    pub(super) fn worker_controls(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let mut section = div().flex().flex_col().gap_2();
        let Some(links) = &self.links else {
            return section.child(button(
                p,
                "worker-links",
                "Manage worker hosts…",
                false,
                !self.busy,
                cx.listener(|s, _, _, cx| s.link_action(Action::WorkerLinks, cx)),
            ));
        };
        let source = self
            .selected
            .as_ref()
            .map_or("this computer", |host| host.name.as_str());
        section = section.child(format!("Send workers from {source}")).child(
            "New links allow launches up to YOLO, subject to the receiving host’s mode policy.",
        );
        for (i, host) in links.available.iter().enumerate() {
            let id = host.id.clone();
            section = section.child(
                button(
                    p,
                    ("worker-pair-known", i),
                    format!("Allow workers on {}", host.name),
                    false,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| {
                        s.link_action(Action::LinkPairedHost { host: id.clone() }, cx)
                    }),
                )
                .tooltip(host.origin.clone()),
            );
        }
        for (i, host) in links.hosts.iter().enumerate() {
            let id = host.id.clone();
            section = section
                .child(format!(
                    "{} · Linked for workers · {}",
                    host.name, host.origin
                ))
                .child(button(
                    p,
                    ("worker-remove-outgoing", i),
                    "Remove outgoing link",
                    false,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| {
                        s.link_action(Action::RemoveWorkerLink { host: id.clone() }, cx)
                    }),
                ));
        }
        section = section
            .child(Input::new(&self.worker_address))
            .child(Input::new(&self.worker_code))
            .child(button(
                p,
                "worker-pair",
                "Link by invitation or address",
                false,
                !self.busy,
                cx.listener(|s, _, w, cx| {
                    let address = s.worker_address.read(cx).value().trim().to_owned();
                    let code = s.worker_code.read(cx).value().trim().to_owned();
                    let selected = s.selected.clone();
                    let handle = w.window_handle();
                    s.work(
                        cx,
                        move || {
                            call::<serde::de::IgnoredAny>(
                                selected.as_ref(),
                                Action::AddWorkerLink { address, code },
                            )?;
                            directory(selected.as_ref())
                        },
                        move |s, links, cx| {
                            s.links = Some(links);
                            let fields = [s.worker_address.clone(), s.worker_code.clone()];
                            let _ = handle.update(cx, |_, w, cx| {
                                for f in fields {
                                    f.update(cx, |v, cx| v.set_value("", w, cx));
                                }
                            });
                        },
                    );
                }),
            ))
            .child(button(
                p,
                "worker-invitation",
                "Copy invitation for an incoming worker host",
                false,
                !self.busy,
                cx.listener(|s, _, _, cx| s.invite_worker(cx)),
            ));
        for (i, incoming) in links.incoming.iter().enumerate() {
            let Some(before) = incoming.worker.clone() else {
                continue;
            };
            let id = incoming.id.clone();
            let grant = before.clone();
            section = section.child(incoming.label.clone()).child(button(
                p,
                ("worker-enabled", i),
                if grant.may_launch {
                    "Allow worker launches · On"
                } else {
                    "Allow worker launches · Off"
                },
                grant.may_launch,
                !self.busy,
                cx.listener(move |s, _, _, cx| {
                    s.worker_access(
                        id.clone(),
                        grant.clone(),
                        !grant.may_launch,
                        grant.ceiling,
                        grant.may_sync_memory,
                        cx,
                    )
                }),
            ));
            let id = incoming.id.clone();
            let grant = before.clone();
            section = section.child(button(
                p,
                ("worker-memory-sync", i),
                if grant.may_sync_memory {
                    "Allow memory sync · On"
                } else {
                    "Allow memory sync · Off"
                },
                grant.may_sync_memory,
                !self.busy,
                cx.listener(move |s, _, _, cx| {
                    s.worker_access(
                        id.clone(),
                        grant.clone(),
                        grant.may_launch,
                        grant.ceiling,
                        !grant.may_sync_memory,
                        cx,
                    )
                }),
            ));
            let mut limits = div().flex().gap_2().flex_wrap();
            for level in Level::ALL {
                let id = incoming.id.clone();
                let before = before.clone();
                limits = limits.child(button(
                    p,
                    format!("worker-ceiling-{i}-{}", level.name()),
                    match level {
                        Level::Read => "Read",
                        Level::Write => "Write",
                        Level::Classifier => "Auto-review",
                        Level::Yolo => "YOLO",
                    },
                    before.ceiling == level,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| {
                        s.worker_access(
                            id.clone(),
                            before.clone(),
                            before.may_launch,
                            level,
                            before.may_sync_memory,
                            cx,
                        )
                    }),
                ));
            }
            let id = incoming.id.clone();
            section = section
                .child("Maximum worker mode")
                .child(limits)
                .child(button(
                    p,
                    ("revoke-worker", i),
                    "Revoke incoming link",
                    false,
                    !self.busy,
                    cx.listener(move |s, _, _, cx| s.revoke_incoming(id.clone(), cx)),
                ));
        }
        section.child(button(
            p,
            "worker-links-refresh",
            "Refresh links",
            false,
            !self.busy,
            cx.listener(|s, _, _, cx| s.link_action(Action::WorkerLinks, cx)),
        ))
    }
}
