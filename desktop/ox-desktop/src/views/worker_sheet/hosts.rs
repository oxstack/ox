use super::*;
use crate::kit;
use crate::platform::HostLink;
use gpui_component::input::InputState;
use ox_desk_core::{host_protocol::HostResources, origin::WorkerNeeds};
use serde::Deserialize;
use std::{path::PathBuf, time::Duration};

#[derive(Deserialize)]
struct Host {
    id: String,
    name: String,
    resources: Option<HostResources>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(
        default,
        deserialize_with = "ox_desk_core::usage::reported::deserialize_windows"
    )]
    subscription_limits:
        std::collections::BTreeMap<String, Vec<ox_desk_core::usage::reported::ReportedWindow>>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum OfferedRoute {
    Known(Route),
    Other(serde::de::IgnoredAny),
}
#[derive(Deserialize)]
struct Entry {
    host: Host,
    parent_host: bool,
    #[serde(default)]
    codex_subscription_workers: bool,
    paired_name: Option<String>,
    status: String,
    #[serde(default)]
    routes: Vec<OfferedRoute>,
}
#[derive(Deserialize)]
struct Inventory {
    hosts: Vec<Entry>,
    warning: Option<String>,
    project_placement: Option<ProjectRule>,
    placement_error: Option<String>,
}

#[derive(Deserialize)]
struct ProjectRule {
    host: Option<String>,
    os: Option<String>,
}

fn limit_lines(inventory: &Inventory, target: &str, route: &Route, now: i64) -> Vec<String> {
    if route.is_api() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let rule = (target == "project")
        .then_some(inventory.project_placement.as_ref())
        .flatten();
    let selector = rule
        .and_then(|r| r.host.as_deref())
        .unwrap_or(if target == "project" { "auto" } else { target });
    for entry in &inventory.hosts {
        let name = entry.paired_name.as_ref().unwrap_or(&entry.host.name);
        let selected = selector == "auto"
            || if let Some(label) = selector.strip_prefix("label:") {
                entry
                    .host
                    .labels
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(label))
            } else if selector == "parent" {
                entry.parent_host
            } else {
                selector == entry.host.id
                    || selector.eq_ignore_ascii_case(name)
                    || selector.eq_ignore_ascii_case(&entry.host.name)
            };
        if !selected
            || entry.status != "online"
            || rule.and_then(|r| r.os.as_ref()).is_some_and(|os| {
                entry
                    .host
                    .resources
                    .as_ref()
                    .is_none_or(|r| !os.eq_ignore_ascii_case(&r.os))
            })
            || !entry
                .routes
                .iter()
                .any(|r| matches!(r, OfferedRoute::Known(offered) if offered == route))
        {
            continue;
        }
        let Some(windows) = entry
            .host
            .subscription_limits
            .get(route.runtime.executable())
        else {
            continue;
        };
        for w in windows
            .iter()
            .filter(|w| w.resets_at.is_none_or(|t| t > now))
        {
            let mut parts = vec![format!(
                "{name} · {} · {}",
                route.runtime.account(),
                w.label
            )];
            if let Some(pct) = w.used_percent {
                parts.push(format!("{pct}% used"));
            }
            if let Some(status) = &w.status
                && (status != "allowed" || w.used_percent.is_none())
            {
                parts.push(status.clone());
            }
            for (label, value) in [
                (
                    if now.saturating_sub(w.observed_at) > 3600 {
                        "last reported"
                    } else {
                        "reported"
                    },
                    Some(w.observed_at),
                ),
                ("resets", w.resets_at),
            ] {
                if let Some(t) = value.and_then(chrono::DateTime::from_timestamp_secs) {
                    parts.push(format!(
                        "{label} {}",
                        t.with_timezone(&chrono::Local).format("%b %e %H:%M")
                    ));
                }
            }
            lines.push(parts.join(" · "));
        }
    }
    lines
}

pub(super) struct Picker {
    target: String,
    inventory: Option<Result<Option<Inventory>, String>>,
    memory: Entity<InputState>,
    cpus: Entity<InputState>,
    os: Entity<InputState>,
    gpu: bool,
    refresh: flume::Sender<()>,
    _refresh: Task<()>,
}

pub(in crate::views) struct OpenWorkerHosts;
impl EventEmitter<OpenWorkerHosts> for Workbench {}

impl Picker {
    pub(super) fn new(
        link: HostLink,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Workbench>,
    ) -> Self {
        let (refresh, requested) = flume::bounded(1);
        let task = cx.spawn(async move |this, cx| {
            loop {
                let link = link.clone();
                let project = project.clone();
                let result = cx
                    .background_spawn(async move {
                        let data = link.worker_hosts(project).map_err(|e| e.to_string())?;
                        data.map(serde_json::from_value)
                            .transpose()
                            .map_err(|e| e.to_string())
                    })
                    .await;
                let legacy = matches!(result, Ok(None));
                let _ = this.update(cx, |s, cx| {
                    if let Some(Launch::Worker(d)) = &mut s.launch {
                        d.hosts.inventory = Some(result);
                        cx.notify();
                    }
                });
                if legacy {
                    return;
                }
                futures_lite::future::or(
                    cx.background_executor().timer(Duration::from_secs(30)),
                    async {
                        let _ = requested.recv_async().await;
                    },
                )
                .await;
            }
        });
        Self {
            target: "parent".into(),
            inventory: None,
            memory: kit::field("", "Any", false, window, cx),
            cpus: kit::field("", "Any", false, window, cx),
            os: kit::field("", "Any", false, window, cx),
            gpu: false,
            refresh,
            _refresh: task,
        }
    }

    pub(super) fn same_as_parent(&self) -> bool {
        self.target == "parent"
    }
    pub(super) fn offers_subscription(&self, runtime: Runtime) -> bool {
        let Some(Ok(inventory)) = self.inventory.as_ref() else {
            return true;
        };
        let Some(inventory) = inventory else {
            return runtime == Runtime::Claude;
        };
        let route = runtime.route(None);
        inventory.hosts.iter().any(|entry| {
            let selected = matches!(self.target.as_str(), "auto" | "project")
                || (self.target == "parent" && entry.parent_host)
                || self.target == entry.host.id;
            selected
                && entry.status == "online"
                && (runtime != Runtime::Codex || entry.codex_subscription_workers)
                && entry
                    .routes
                    .iter()
                    .any(|r| matches!(r, OfferedRoute::Known(r) if r == &route))
        })
    }

    pub(super) fn aliases(&self, runtime: Runtime) -> Option<Vec<String>> {
        if self.target == "parent" {
            return None;
        }
        let inventory = self.inventory.as_ref()?.as_ref().ok()?.as_ref()?;
        Some(
            inventory
                .hosts
                .iter()
                .filter(|e| {
                    matches!(self.target.as_str(), "auto" | "project") || self.target == e.host.id
                })
                .flat_map(|e| e.routes.iter())
                .filter_map(|route| match route {
                    OfferedRoute::Known(route) => Some(route),
                    OfferedRoute::Other(_) => None,
                })
                .filter(|r| r.runtime == runtime)
                .filter_map(|r| match &r.billing {
                    Billing::Api(alias) => Some(alias.clone()),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    }

    pub(super) fn apply(&self, start: &mut WorkerStart, cx: &App) -> Result<(), String> {
        if start.model.is_none() && !self.offers_subscription(start.runtime) {
            return Err("The selected host does not offer this subscription for workers. Check its sign-in and OX version, or choose an API model.".into());
        }
        if self.target == "project" {
            return Ok(());
        }
        start.host = Some(self.target.clone());
        if self.target == "auto" {
            let number = |field: &Entity<InputState>, name: &str| {
                let text = field.read(cx).value();
                let text = text.trim();
                if text.is_empty() {
                    Ok(None)
                } else {
                    text.parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .map(Some)
                        .ok_or_else(|| format!("Enter a positive whole number for {name}."))
                }
            };
            let os = self.os.read(cx).value().trim().to_owned();
            start.needs = Some(WorkerNeeds {
                memory_gb: number(&self.memory, "memory")?,
                cpus: number(&self.cpus, "CPUs")?,
                os: (!os.is_empty()).then_some(os),
                gpu: self.gpu,
            });
        }
        start.validate()
    }
}

impl Workbench {
    pub(in crate::views) fn refresh_worker_hosts(&self) {
        if let Some(Launch::Worker(draft)) = &self.launch {
            let _ = draft.hosts.refresh.try_send(());
        }
    }

    fn set_worker_host(&mut self, host: String, cx: &mut Context<Self>) {
        self.launch_picker = None;
        if let Some(Launch::Worker(d)) = &mut self.launch {
            d.hosts.target = host;
            d.choice.error = None;
        }
        cx.notify();
    }
    pub(super) fn host_row(&self, p: Palette, draft: &WorkerDraft, cx: &mut Context<Self>) -> Div {
        let picker = &draft.hosts;
        let inventory = picker
            .inventory
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .and_then(Option::as_ref);
        let mut choices = row().flex_wrap().gap_2();
        let source = self
            .host_caption
            .as_deref()
            .or_else(|| self.link.remote().map(|h| h.name.as_str()))
            .unwrap_or("This computer");
        let mut entries = vec![(
            "parent".to_owned(),
            format!("{source} · this session’s host"),
            true,
        )];
        if let Some(inventory) = inventory {
            for entry in &inventory.hosts {
                let id = if entry.parent_host {
                    "parent"
                } else {
                    &entry.host.id
                };
                let name = entry.paired_name.as_ref().unwrap_or(&entry.host.name);
                let label = if entry.parent_host {
                    format!("{name} · this session’s host")
                } else if entry.status != "online" {
                    format!("{name} · {}", entry.status.replace('_', " "))
                } else if let Some(r) = &entry.host.resources {
                    format!(
                        "{name} · load {:.1} · {:.1} GB free",
                        r.load_one_minute,
                        r.available_memory as f64 / 1e9
                    )
                } else {
                    name.clone()
                };
                if entry.parent_host {
                    entries[0].1 = label;
                } else {
                    entries.push((id.to_owned(), label, entry.status == "online"));
                }
            }
            if let Some(rule) = &inventory.project_placement {
                let label = [
                    "Project rule",
                    rule.host.as_deref().unwrap_or("Prefer parent"),
                ]
                .into_iter()
                .chain(rule.os.as_deref())
                .collect::<Vec<_>>()
                .join(" · ");
                entries.push(("project".into(), label, true));
            }
            entries.push(("auto".into(), "Auto (match requirements)".into(), true));
        }
        for (i, (id, label, enabled)) in entries.into_iter().enumerate() {
            choices = choices.child(button(
                p,
                ("worker-host", i),
                label,
                picker.target == id,
                enabled,
                cx.listener(move |s, _, _, cx| s.set_worker_host(id.clone(), cx)),
            ));
        }
        let mut content = col().gap_1().child(choices);
        let status = match picker.inventory.as_ref() {
            None => Some("Loading worker hosts…"),
            Some(Err(e)) => Some(e.as_str()),
            Some(Ok(Some(v))) => v.warning.as_deref().or(v.placement_error.as_deref()),
            Some(Ok(None)) => Some("Update this host to run workers on linked hosts."),
        };
        if let Some(status) = status {
            content = content.child(small(p, status.to_owned()));
        }
        if let Some(inventory) = inventory {
            let linked = inventory.hosts.iter().any(|entry| !entry.parent_host);
            if !linked {
                content = content.child(small(
                    p,
                    "Allow workers from this session’s host to run on another computer.",
                ));
            }
            content = content.child(
                kit::toolbar_button(
                    p,
                    "worker-host-settings",
                    if linked {
                        "Manage worker hosts…"
                    } else {
                        "Set up worker hosts…"
                    },
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(OpenWorkerHosts)),
                )
                .self_start(),
            );
            let route = draft.choice.runtime.route(draft.choice.model.clone());
            for line in limit_lines(
                inventory,
                &picker.target,
                &route,
                chrono::Utc::now().timestamp(),
            ) {
                content = content.child(small(p, line));
            }
        }
        if picker.target == "auto" {
            let mut inputs = row().gap_2().flex_wrap();
            for (name, input) in [
                ("Memory (GB)", &picker.memory),
                ("CPUs", &picker.cpus),
                ("OS", &picker.os),
            ] {
                inputs = inputs.child(div().w(px(108.)).child(field(p, name, kit::input(input))));
            }
            content = content.child(inputs).child(button(
                p,
                "worker-needs-gpu",
                "Require GPU",
                picker.gpu,
                true,
                cx.listener(|s, _, _, cx| {
                    if let Some(Launch::Worker(d)) = &mut s.launch {
                        d.hosts.gpu = !d.hosts.gpu;
                        cx.notify();
                    }
                }),
            ));
        }
        field(p, "Run on", content)
    }
}

#[cfg(test)]
mod tests {
    use super::{Inventory, Runtime, limit_lines};
    use serde_json::json;
    #[test]
    fn limits_follow_the_selected_host_billing_and_project_rule() {
        let row = |name: &str, pct| {
            json!({
                "host":{"id":name, "name":name, "resources":null, "labels":[name], "subscription_limits":{
                    "claude":[{"label":"five hour","used_percent":pct,"status":"allowed","resets_at":300,"observed_at":100},
                              {"new_format":true}]}},
                "routes":[{"runtime":"claude","billing":"subscription"}],"status":"online","parent_host":name=="laptop"
            })
        };
        let inventory: Inventory = serde_json::from_value(json!({
            "hosts":[row("laptop",10),row("studio",90)], "project_placement":{"host":"label:studio"}
        }))
        .unwrap();
        let route = Runtime::Claude.route(None);
        for (target, pct) in [("parent", 10), ("studio", 90), ("project", 90)] {
            let lines = limit_lines(&inventory, target, &route, 200);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].contains(&format!("{pct}% used")));
        }
        assert_eq!(limit_lines(&inventory, "auto", &route, 200).len(), 2);
        assert!(limit_lines(&inventory, "auto", &route, 300).is_empty());
        assert!(
            limit_lines(
                &inventory,
                "studio",
                &Runtime::Claude.route(Some("api".into())),
                200
            )
            .is_empty()
        );
    }
}
