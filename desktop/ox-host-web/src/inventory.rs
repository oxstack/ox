//! Signed probes return host resources, launch permissions and registered project membership.
use crate::{admin::WorkerGrant, bridge, client, paired_hosts, server::App};
use futures_util::future::join_all;
use ox_desk_core::{
    host_protocol::{Client, HostInfo, Server, VERSION},
    runtime::{Route, Runtime},
};
use ox_host_service::checkout::repository_id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::{io::BufReader, time::timeout};

mod placement;
pub use placement::choose;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Inventory {
    pub host: HostInfo,
    #[serde(deserialize_with = "known_values")]
    runtimes: Vec<Runtime>,
    #[serde(deserialize_with = "known_values")]
    routes: Vec<Route>,
    routes_known: bool,
    #[serde(default)]
    codex_subscription_workers: bool,
    #[serde(default)]
    linked_read: bool,
    #[serde(default)]
    linked_write: bool,
    /// The target accepts the optional `memory_brief` launch fields. Older
    /// hosts reject unknown fields, so the parent must not send them blindly.
    #[serde(default)]
    memory_brief: bool,
    #[serde(default)]
    supports_session_moves: bool,
    has_project: Option<bool>,
}
// A newer host may also run an agent or billing route this client cannot launch.
fn known_values<'de, D, T>(input: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(Vec::<Value>::deserialize(input)?
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}
#[derive(Serialize, Deserialize)]
struct Peer {
    #[serde(flatten)]
    inventory: Inventory,
    grant: WorkerGrant,
}
#[derive(Default)]
pub(crate) struct Cache(tokio::sync::Mutex<Option<(Instant, Inventory, HashSet<String>)>>);

pub(crate) async fn local(
    app: &App,
    project: Option<String>,
    fresh: bool,
) -> io::Result<Inventory> {
    let mut cache = app.inventory.0.lock().await;
    if fresh
        || cache
            .as_ref()
            .is_none_or(|(when, ..)| when.elapsed() >= Duration::from_secs(10))
    {
        let (inventory, projects) = sample(app).await?;
        *cache = Some((Instant::now(), inventory, projects));
    }
    let (_, inventory, projects) = cache.as_ref().expect("initialized above");
    let mut inventory = inventory.clone();
    inventory.has_project = project.map(|id| projects.contains(&id));
    Ok(inventory)
}

async fn hello(app: &App) -> io::Result<Server> {
    let frame = timeout(Duration::from_secs(6), async {
        let mut conn = bridge::connect(&app.host).await?;
        bridge::write(&mut conn, &Client::Hello { version: VERSION }).await?;
        bridge::line(&mut BufReader::new(conn), &mut Vec::new()).await
    })
    .await??;
    Ok(serde_json::from_slice(&frame)?)
}
fn add_discovered(app: &App, projects: &mut Vec<std::path::PathBuf>) {
    projects.extend(
        app.discovered
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.project.clone()),
    );
    projects.sort();
    projects.dedup();
}
/// A transfer always resolves the receiver's current registered directories again.
pub(crate) async fn projects(app: &App, target: &str) -> io::Result<Vec<std::path::PathBuf>> {
    match hello(app).await? {
        Server::Hello {
            host: Some(host),
            mut projects,
            ..
        } if host.id.as_deref() == Some(target) => {
            add_discovered(app, &mut projects);
            Ok(projects)
        }
        _ => Err(io::Error::other("Worker host identity changed.")),
    }
}
async fn sample(app: &App) -> io::Result<(Inventory, HashSet<String>)> {
    let hello = hello(app).await?;
    let Server::Hello {
        host: Some(host),
        capabilities,
        mut projects,
        mut routes,
        ..
    } = hello
    else {
        return Err(io::Error::other(
            "Session host did not return its identity.",
        ));
    };
    let supports = |cap: &str| capabilities.iter().any(|s| s == cap);
    if !supports("worker_inventory") {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let runtimes: Vec<_> = [Runtime::Codex, Runtime::Claude]
        .into_iter()
        .filter(|r| supports(&format!("worker_runtime_{}", r.executable())))
        .collect();
    routes.retain(|r| {
        runtimes.contains(&r.runtime) && (!r.is_api() || supports("worker_api_provider"))
    });
    add_discovered(app, &mut projects);
    let private = app.state.lock().unwrap().worker_directory();
    let projects = tokio::task::spawn_blocking(move || {
        let private = private.canonicalize().unwrap_or(private);
        projects
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .filter(|p| !p.starts_with(&private))
            .filter_map(|p| repository_id(&p))
            .collect()
    })
    .await?;
    Ok((
        Inventory {
            host,
            runtimes,
            routes,
            has_project: None,
            routes_known: supports("worker_routes_known"),
            codex_subscription_workers: supports("worker_codex_subscription"),
            linked_read: supports("linked_read_workers"),
            linked_write: supports("linked_write_workers"),
            memory_brief: supports("worker_memory_brief"),
            supports_session_moves: app.moves.is_some(),
        },
        projects,
    ))
}
pub(crate) fn report(mut inventory: Inventory, grant: WorkerGrant) -> io::Result<Value> {
    if !grant.may_launch {
        inventory.host.subscription_limits.clear();
    }
    Ok(serde_json::to_value(Peer { inventory, grant })?)
}
async fn query_peer(
    host: &paired_hosts::SavedHost,
    source: &str,
    project: Option<&str>,
    fresh: bool,
) -> io::Result<Peer> {
    let mut request = json!({"action":"inventory","project":project});
    if fresh {
        request["fresh"] = true.into();
    }
    let reply = host.client()?.worker_request(request).await?;
    let mut peer: Peer = serde_json::from_value(reply).map_err(|_| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Update the receiving OX host to use linked workers.",
        )
    })?;
    if peer.inventory.host.id.as_ref() != Some(&host.id) || peer.grant.source != source {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Worker link identity changed.",
        ));
    }
    if !peer.inventory.codex_subscription_workers {
        peer.inventory
            .routes
            .retain(|r| *r != Route::CODEX_SUBSCRIPTION);
    }
    Ok(peer)
}
/// Target capabilities that decide how the parent builds a launch envelope.
pub struct Launch {
    /// The target accepts write results from linked workers.
    pub linked_write: bool,
    /// The target understands `memory_brief`. Older hosts deny unknown fields.
    pub memory_brief: bool,
}
/// Check this job against a current target sample before building its checkout.
pub fn check_launch(
    host: &paired_hosts::SavedHost,
    source: &str,
    project: &Path,
    start: &ox_desk_core::origin::WorkerStart,
    parent: ox_desk_core::modes::Level,
) -> io::Result<Launch> {
    let repo = repository_id(project)
        .ok_or_else(|| io::Error::other("Remote workers need a shared Git origin."))?;
    let peer = client::runtime()?.block_on(query_peer(host, source, Some(&repo), true))?;
    check_peer(&peer, &host.name, start, parent)?;
    Ok(Launch {
        linked_write: peer.inventory.linked_write,
        memory_brief: peer.inventory.memory_brief,
    })
}
fn check_peer(
    peer: &Peer,
    target: &str,
    start: &ox_desk_core::origin::WorkerStart,
    parent: ox_desk_core::modes::Level,
) -> io::Result<()> {
    if !peer.inventory.linked_read {
        return Err(io::Error::other(format!(
            "Update {target} to run linked workers."
        )));
    }
    if !peer.grant.may_launch {
        return Err(io::Error::other(format!(
            "New worker launches are disabled on {target}."
        )));
    }
    if peer.inventory.has_project != Some(true) {
        return Err(io::Error::other(format!(
            "Register a checkout of this repository on {target} with ox host project add DIR."
        )));
    }
    if !peer.inventory.runtimes.contains(&start.runtime) {
        return Err(io::Error::other(format!(
            "{} is not installed on {target}.",
            start.runtime.name()
        )));
    }
    if start.route() == Route::CODEX_SUBSCRIPTION && !peer.inventory.codex_subscription_workers {
        return Err(io::Error::other(format!(
            "Update {target} to run workers on its ChatGPT subscription."
        )));
    }
    if !peer.inventory.routes.contains(&start.route()) {
        return Err(io::Error::other(format!(
            "{} is unavailable on {target}.",
            start.route().label()
        )));
    }
    let ceiling = parent.min(peer.grant.ceiling);
    let mode = ox_desk_core::modes::resolve(Some(ceiling), start.mode_request(), start.runtime)
        .map_err(io::Error::other)?;
    let policy = peer.inventory.host.mode_policy.unwrap_or_default();
    if policy
        .child_start(mode, start.mode_request(), ceiling, start.runtime)
        .level
        != ox_desk_core::modes::Level::Read
        && !peer.inventory.linked_write
    {
        return Err(io::Error::other(format!(
            "{target} supports linked read workers. Choose read mode for this job."
        )));
    }
    Ok(())
}

async fn probe(host: paired_hosts::SavedHost, source: &str, project: Option<&str>) -> Value {
    let saved = json!({"id":host.id,"name":host.name});
    // Avoid a new network wait on every tool call while a laptop is asleep.
    // A repaired link gets a fresh key ID and is probed immediately.
    static UNAVAILABLE: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
    let key = host.key_id.clone();
    let paired_name = host.name.clone();
    {
        let mut offline = UNAVAILABLE.lock().unwrap();
        offline.retain(|_, since| since.elapsed() < Duration::from_secs(15));
        if offline.contains_key(&key) {
            return json!({"host":saved,"parent_host":false,"status":"unavailable"});
        }
    }
    let fetch = async {
        let host = tokio::task::spawn_blocking(move || -> io::Result<_> {
            host.client()?;
            Ok(host)
        })
        .await??;
        let peer = query_peer(&host, source, project, false).await?;
        let mut row = serde_json::to_value(peer)?;
        row["paired_name"] = paired_name.into();
        row["parent_host"] = false.into();
        row["status"] = "online".into();
        Ok::<_, io::Error>(row)
    };
    let status = match timeout(Duration::from_secs(5), fetch).await {
        Ok(Ok(row)) => return row,
        Ok(Err(e)) => match e.kind() {
            io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported => "needs_update",
            io::ErrorKind::PermissionDenied => "access_denied",
            io::ErrorKind::WouldBlock => "busy",
            _ => "unavailable",
        },
        Err(_) => "unavailable",
    };
    if status == "unavailable" {
        UNAVAILABLE.lock().unwrap().insert(key, Instant::now());
    }
    json!({"host":saved,"parent_host":false,"status":status})
}

/// List the parent's host and its explicitly paired worker destinations in saved order.
pub fn list(registry: &Path, project: &Path, source: &str, local: Value) -> io::Result<Value> {
    let mut hosts = vec![local];
    let links = match paired_hosts::worker_links(registry) {
        Ok(links) => links,
        Err(_) => {
            return Ok(
                json!({"hosts":hosts, "warning":"The saved worker host list could not be read."}),
            );
        }
    };
    if !links.is_empty() {
        let rt = client::runtime();
        if crate::server::background_keychain().is_err() || rt.is_err() {
            hosts.extend(links.iter().map(|h|json!({"host":{"id":h.id,"name":h.name},"parent_host":false,"status":"unavailable"})));
            return Ok(
                json!({"hosts":hosts, "warning":"Could not open the background worker connections."}),
            );
        }
        let project = repository_id(project);
        let remote = rt?.block_on(join_all(
            links
                .into_iter()
                .map(|h| probe(h, source, project.as_deref())),
        ));
        hosts.extend(remote);
    }
    Ok(json!({"hosts":hosts}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn broken_registry_preserves_parent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("worker-links.json");
        std::fs::write(&path, b"bad json").unwrap();
        let local = json!({"parent_host":true});
        let result = list(&path, temp.path(), "local", local.clone()).unwrap();
        assert_eq!(result["hosts"], json!([local]));
        assert!(result["warning"].is_string());
    }

    #[test]
    #[ignore = "launched by the two-host TLS fixture"]
    fn paired_inventory() {
        let state = std::path::PathBuf::from(std::env::var_os("OX_TEST_REMOTE_DIR").unwrap());
        let source = std::fs::read_to_string(state.join("host-id")).unwrap();
        let list_for = |project: &str| {
            let result = list(
                &state.join("worker-links.json"),
                &state.join(project),
                source.trim(),
                json!({"parent_host":true}),
            )
            .unwrap();
            let rows = result["hosts"].as_array().unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["parent_host"], true);
            assert!(!result.to_string().contains("fixture-secret"));
            rows[1].clone()
        };
        let row = list_for("inventory-source/ox");
        assert_eq!(row["status"], "online", "{row}");
        assert_eq!(row["has_project"], true);
        assert_eq!(row["grant"]["may_launch"], false);
        assert_eq!(row["grant"]["ceiling"], "read");
        assert!(row["host"]["resources"]["total_memory"].as_u64().unwrap() > 0);
        assert_eq!(list_for("another/ox")["has_project"], false);
        assert!(list_for("fixture")["has_project"].is_null());
    }
}
