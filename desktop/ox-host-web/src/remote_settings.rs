//! Same-user, paired-device settings. Separate from session actions and their durable receipts:
//! configuration documents, credentials and pairing codes must never enter the event journal.
use super::{App, DENY, ErrCode, Proof, device, login};
use axum::{
    Json,
    extract::State as Extract,
    http::{HeaderMap, StatusCode},
};
use ox_desk_core::{
    ports::ox,
    settings_protocol::{self as protocol, Request},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::Semaphore;
pub(super) static HELPERS: Semaphore = Semaphore::const_new(3);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    proof: Proof,
    request: protocol::model::Secret,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum DeviceAction {
    HostModePolicy {
        set: Option<ox_desk_core::host_policy::HostPolicy>,
    },
    HostProfile {
        edit: Option<crate::host_profile::Edit>,
    },
    PushPreferences {
        change: Option<crate::push::PreferenceEdit>,
    },
    Devices,
    PairDevice,
    InviteWorkerHost,
    OpenPairingWindow,
    ClosePairingWindow,
    ApprovePending {
        id: String,
    },
    DenyPending {
        id: String,
    },
    EditWorkerLink {
        id: String,
        before: crate::admin::WorkerGrant,
        may_launch: bool,
        ceiling: ox_desk_core::modes::Level,
        /// `None` leaves the link's memory-sync grant unchanged.
        #[serde(default)]
        may_sync_memory: Option<bool>,
    },
    HostUpdate {
        command: crate::host_updates::Command,
    },
    RevokeDevice {
        id: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum BehaviorAction {
    ReadBehavior,
    SaveBehavior {
        revision: String,
        behavior: ox_desk_core::desktop_behavior::Behavior,
    },
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryAction {
    ReadProjectMemory { project: PathBuf },
    AddProjectMemory { project: PathBuf, text: String },
}
impl MemoryAction {
    fn apply(self, path: PathBuf, host: &str) -> Result<Value, ox_memory::MemoryError> {
        use ox_memory::{Kind, MemoryStore, NewMemory, Scope, Sensitivity};

        let (project, text) = match self {
            Self::ReadProjectMemory { project } => (project, None),
            Self::AddProjectMemory { project, text } => (project, Some(text)),
        };
        if !project.is_absolute() {
            return Err(ox_memory::MemoryError::InvalidInput(
                "Open a project before using OX Memory.".into(),
            ));
        }
        let store = MemoryStore::open(path);
        if let Some(text) = text {
            store.upsert(NewMemory {
                id: None,
                scope: Scope::Project(ox_memory::canonical_project_id(&project)),
                kind: Kind::Preference,
                sensitivity: Sensitivity::Private,
                text,
                tags: Vec::new(),
                pin: false,
                source_host: host.to_owned(),
            })?;
        }
        let entries = store.list_for_project(&project, host, Sensitivity::Private)?;
        let entries: Vec<_> = entries
            .into_iter()
            .map(|entry| {
                json!({
                    "text": entry.text, "scope": entry.scope.short_label(),
                    "kind": entry.kind.as_str(), "sensitivity": entry.sensitivity.as_str(),
                    "pinned": entry.pin, "saved_here": entry.source_host == host,
                })
            })
            .collect();
        Ok(json!({"entries": entries}))
    }
}

pub(super) async fn settings(
    Extract(app): Extract<App>,
    headers: HeaderMap,
    body: Result<Json<Envelope>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ErrCode> {
    let id = device(&app, &headers)?;
    let Json(body) = body.map_err(|_| StatusCode::BAD_REQUEST)?;
    let text = body.request.expose();
    let identity = login(&headers).ok_or(DENY)?;
    let device_action = serde_json::from_str::<DeviceAction>(text).ok();
    {
        let mut state = app.state.lock().unwrap();
        if !state.prove_settings(&id, &body.proof.nonce, &body.proof.signature, text) {
            return Err(DENY);
        }
        if let Some(op) = &device_action {
            let data = match op {
                DeviceAction::HostUpdate { .. } => None,
                DeviceAction::PushPreferences { change } => Some(json!(
                    state
                        .push_preferences(&id, *change)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                )),
                DeviceAction::HostModePolicy { set } => Some(
                    json!({"policy":state.mode_policy(*set).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?}),
                ),
                DeviceAction::HostProfile { edit } => {
                    Some(json!(state.host_profile(edit.clone()).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::AlreadyExists {
                            StatusCode::CONFLICT
                        } else {
                            StatusCode::BAD_REQUEST
                        }
                    })?))
                }
                DeviceAction::Devices => {
                    let reply = state.reply(None);
                    let devices = reply
                        .devices
                        .into_iter()
                        .filter(|d| d.login == identity)
                        .collect::<Vec<_>>();
                    let worker_hosts: Vec<_> = reply
                        .worker_hosts
                        .into_iter()
                        .filter(|d| d.login == identity)
                        .collect();
                    Some(
                        json!({"devices":devices,"worker_hosts":worker_hosts,"self":id,"origin":reply.origin,"window":reply.window,"pending":reply.pending.into_iter().filter(|p| p.login == identity).collect::<Vec<_>>()}),
                    )
                }
                // A paired device speaks for the login it is paired under, so
                // a window it opens admits only that login.
                DeviceAction::OpenPairingWindow => Some(
                    serde_json::to_value(
                        state
                            .open_window(Some(identity.to_owned()))
                            .map_err(|_| StatusCode::CONFLICT)?,
                    )
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                ),
                DeviceAction::ClosePairingWindow => {
                    state.close_window();
                    Some(json!({"status":"closed"}))
                }
                DeviceAction::ApprovePending { id } => {
                    if state.pending_login(id).as_deref() != Some(identity) {
                        return Err(DENY);
                    }
                    state.approve_pending(id).ok_or(StatusCode::CONFLICT)?;
                    Some(json!({"status":"approved"}))
                }
                DeviceAction::DenyPending { id } => {
                    if state.pending_login(id).as_deref() != Some(identity) {
                        return Err(DENY);
                    }
                    state.deny_pending(id);
                    Some(json!({"status":"denied"}))
                }
                DeviceAction::PairDevice => Some(
                    serde_json::to_value(
                        state.device_invite(&id).map_err(|_| StatusCode::CONFLICT)?,
                    )
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                ),
                DeviceAction::InviteWorkerHost => Some(json!(
                    state.worker_invite(&id).map_err(|_| StatusCode::CONFLICT)?
                )),
                DeviceAction::EditWorkerLink {
                    id: target,
                    before,
                    may_launch,
                    ceiling,
                    may_sync_memory,
                } => {
                    if !state.belongs_to_login(target, identity) {
                        return Err(DENY);
                    }
                    Some(
                        match state.edit_worker(
                            target,
                            before,
                            *may_launch,
                            *ceiling,
                            *may_sync_memory,
                        ) {
                            Ok(()) => json!({"status":"applied"}),
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                                json!({"error": e.to_string()})
                            }
                            Err(_) => return Err(StatusCode::CONFLICT),
                        },
                    )
                }
                DeviceAction::RevokeDevice { id: target } => {
                    if !state.belongs_to_login(target, identity) {
                        return Err(DENY);
                    }
                    state
                        .revoke(target)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    Some(json!({"status":"applied"}))
                }
            };
            if let Some(data) = data {
                return Ok(Json(data));
            }
        }
    }
    if let Ok(action) = serde_json::from_str::<super::worker_links::Action>(text) {
        return super::worker_links::apply(app, id, action).await;
    }
    if let Ok(action) = serde_json::from_str::<super::key_transfer::KeyAction>(text) {
        return super::key_transfer::apply(app, id, identity.to_owned(), action).await;
    }
    if let Some(DeviceAction::HostUpdate { command }) = device_action {
        let active = app.state.clone();
        return tokio::task::spawn_blocking(move || {
            if !active.lock().unwrap().alive(&id) {
                return Err(DENY);
            }
            let status = crate::host_updates::control(command).map_err(|_| StatusCode::CONFLICT)?;
            Ok(Json(
                serde_json::to_value(status).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            ))
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    // Type-check and allowlist before a helper can open files. In particular raw v1 Read/Save/Key
    // and profile-directory requests are unavailable through the phone.
    if let Ok(action) = serde_json::from_str::<super::project_picker::ProjectAction>(text) {
        let _slot = HELPERS
            .try_acquire()
            .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
        return super::project_picker::apply(&app, &id, action).await;
    }
    if let Ok(memory) = serde_json::from_str::<MemoryAction>(text) {
        let slot = HELPERS
            .try_acquire()
            .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
        let active = app.state.clone();
        let requester = id.clone();
        let data = tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let (path, host) = {
                let state = active.lock().unwrap();
                if !state.alive(&requester) {
                    return Err(DENY);
                }
                let host = state
                    .host_id()
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                (state.memory_path(), host)
            };
            Ok(memory
                .apply(path, &host)
                .unwrap_or_else(|err| json!({"error":safe_error(&err.to_string())})))
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
        if !app.state.lock().unwrap().alive(&id) {
            return Err(DENY);
        }
        return Ok(Json(data));
    }
    let behavior = serde_json::from_str::<BehaviorAction>(text).ok();
    let request = if behavior.is_some() {
        None
    } else {
        let request: Request = serde_json::from_str(text).map_err(|_| StatusCode::BAD_REQUEST)?;
        if !matches!(
            &request,
            Request::ReadRows
                | Request::ReadAdvanced
                | Request::SaveAdvanced { .. }
                | Request::Edit { .. }
                | Request::EditKey { .. }
                | Request::ImportKeys { .. }
                | Request::Models
                | Request::ProviderModels { .. }
                | Request::TestProvider { .. }
        ) {
            return Err(DENY);
        }
        Some(request)
    };
    let slot = HELPERS
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let active = app.state.clone();
    let requester = id.clone();
    let data = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        if !active.lock().unwrap().alive(&requester) {
            return Err(DENY);
        }
        let data = match behavior {
            Some(BehaviorAction::ReadBehavior) => ox_desk_core::desktop_behavior::read()
                .and_then(|v| serde_json::to_value(v).map_err(std::io::Error::from))
                .map_err(|e| e.to_string()),
            Some(BehaviorAction::SaveBehavior { revision, behavior }) => {
                ox_desk_core::desktop_behavior::write(&revision, behavior)
                    .and_then(|v| serde_json::to_value(v).map_err(std::io::Error::from))
                    .map_err(|e| e.to_string())
            }
            None => helper(request.expect("typed helper request")),
        };
        Ok(data.unwrap_or_else(|e| json!({"error":safe_error(&e)})))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    if !app.state.lock().unwrap().alive(&id) {
        return Err(DENY);
    }
    Ok(Json(data))
}

pub(super) fn binary() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("OX_TEST_SETTINGS_OX") {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    std::env::current_exe()
        .map(|path| path.with_file_name("ox"))
        .map_err(|e| e.to_string())
}

fn helper(request: Request) -> Result<Value, String> {
    let companion = binary()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let encode = |result: serde_json::Result<Value>| {
        result.map_err(|_| "Incompatible settings response".to_owned())
    };
    match request {
        Request::ReadRows => encode(serde_json::to_value(ox::snapshot(&companion, &path)?)),
        Request::ReadAdvanced => {
            let mut docs: Vec<protocol::Document> = ox::request(&companion, &path, request)?;
            // Malformed favorites can contain arbitrary text; the native Favorites form repairs
            // them using the same file revision without disclosing that content to the phone.
            docs.retain(|d| {
                !d.id.contains("favorites") || serde_json::from_str::<Vec<String>>(&d.text).is_ok()
            });
            encode(serde_json::to_value(docs))
        }
        Request::Edit { .. }
        | Request::EditKey { .. }
        | Request::ImportKeys { .. }
        | Request::SaveAdvanced { .. } => {
            let result: protocol::model::Outcome = ox::request(&companion, &path, request)?;
            encode(serde_json::to_value(result))
        }
        Request::Models | Request::ProviderModels { .. } => {
            let models: Vec<String> = ox::request(&companion, &path, request)?;
            Ok(json!(models))
        }
        Request::TestProvider { .. } => {
            let count: usize = ox::request(&companion, &path, request)?;
            Ok(json!(count))
        }
        _ => Err("Unsupported settings operation".into()),
    }
}

fn safe_error(message: &str) -> &'static str {
    // TOML parser diagnostics, remote provider errors and OS error paths can quote private
    // input. Only fixed messages cross the device boundary.
    let s = message.to_lowercase();
    if s.contains("changed") || s.contains("reload") {
        "Settings changed on the laptop. Reload and review your edits."
    } else if s.contains("credential") || s.contains("api key") {
        "Check the provider credential name and value."
    } else if s.contains("provider") || s.contains("model") {
        "Check the provider and model settings on the laptop."
    } else {
        "The laptop could not apply this request. Review the settings and try again."
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use ox_memory::{Kind, MemoryStore, NewMemory, Scope, Sensitivity};

    #[test]
    fn paired_memory_excludes_local_only_and_other_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("memory.json");
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let store = MemoryStore::open(&path);
        for (scope, sensitivity, text) in [
            (Scope::Global, Sensitivity::LocalOnly, "Local-only fixture"),
            (
                Scope::Project("different-project".into()),
                Sensitivity::Public,
                "Other project fixture",
            ),
            (Scope::Global, Sensitivity::Private, "Shared fixture"),
        ] {
            store
                .upsert(NewMemory {
                    id: None,
                    scope,
                    kind: Kind::Fact,
                    sensitivity,
                    text: text.into(),
                    tags: Vec::new(),
                    pin: false,
                    source_host: "fixture".into(),
                })
                .unwrap();
        }
        let result = MemoryAction::AddProjectMemory {
            project: project.clone(),
            text: "Use the staging API".into(),
        }
        .apply(path.clone(), "fixture")
        .unwrap();
        let entries = result["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry["text"] == "Shared fixture")
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry["text"] == "Use the staging API" && entry["scope"] == "project")
        );
        assert_eq!(
            MemoryAction::ReadProjectMemory { project }
                .apply(path, "fixture")
                .unwrap(),
            result
        );
    }
}
