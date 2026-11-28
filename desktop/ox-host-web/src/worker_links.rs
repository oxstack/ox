//! Operator control of outbound worker links. These registrations use their
//! own keys and directory. Receiving-host settings govern each inbound link.
use super::{App, DENY, ErrCode};
pub use crate::admin::WorkerLinkAction as Action;
use crate::{
    client::{self, Invitation},
    paired_hosts::{self, SavedHost},
};
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use std::io;

pub(super) async fn apply(
    app: App,
    device: String,
    action: Action,
) -> Result<Json<Value>, ErrCode> {
    let slot = super::remote_settings::HELPERS
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let check = app.clone();
    let caller = device.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        execute(&check, Some(&caller), action)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !app.state.lock().unwrap().alive(&device) {
        return Err(DENY);
    }
    Ok(Json(
        result.unwrap_or_else(|e| json!({"error":e.to_string()})),
    ))
}

fn describe(h: &SavedHost) -> Value {
    json!({"id":h.id, "origin":h.origin, "name":h.name})
}

pub(super) fn execute(app: &App, device: Option<&str>, action: Action) -> io::Result<Value> {
    let (registry, destinations, source_id, name, origin, local_https_port) = {
        let s = app.state.lock().unwrap();
        if device.is_some_and(|id| !s.alive(id)) {
            return Err(io::Error::other("This device needs to pair again."));
        }
        (
            s.worker_links_path(),
            s.key_destinations_path(),
            s.host_id()?,
            s.host_profile(None)?.name,
            s.configuration().map(|(origin, _)| origin),
            s.reply(None).https_port,
        )
    };
    let rows = paired_hosts::worker_links(&registry)?;
    let acceptable = |expected: Option<&SavedHost>, current: &[SavedHost], new: &SavedHost| {
        let matching = |h: &&SavedHost| h.id == new.id || h.origin == new.origin;
        let old = rows.iter().find(matching);
        let now = current.iter().find(matching);
        let unchanged = match (old, now) {
            (None, None) => true,
            (Some(old), Some(now)) => {
                old.same_pairing(now) && now.id == new.id && now.origin == new.origin
            }
            _ => false,
        };
        device.is_none_or(|id| app.state.lock().unwrap().alive(id))
            && expected.is_none_or(|h| h.id == new.id && h.origin == new.origin)
            && new.id != source_id
            && unchanged
    };
    let on_tailnet = |target: &str| -> io::Result<()> {
        if origin.as_ref().is_some_and(|local| {
            local == target
                || super::key_transfer::tailnet(local) != super::key_transfer::tailnet(target)
        }) {
            return Err(io::Error::other(
                "Choose a different OX host on your tailnet.",
            ));
        }
        Ok(())
    };
    let label = if name.is_empty() {
        paired_hosts::device_label()
    } else {
        name
    };
    let label: String = label.chars().take(64).collect();
    let pair = |invitation: Invitation, expected: Option<&SavedHost>| -> io::Result<Value> {
        on_tailnet(&invitation.origin)?;
        let host = paired_hosts::enroll(
            invitation,
            &label,
            &registry,
            true,
            Some(&source_id),
            |current, new| acceptable(expected, current, new),
        )?;
        Ok(describe(&host))
    };
    match action {
        Action::WorkerLinks => {
            let known = paired_hosts::destinations(&destinations)?
                .into_iter()
                .filter(|h| {
                    h.id != source_id
                        && Some(&h.origin) != origin.as_ref()
                        && !rows.iter().any(|r| r.id == h.id || r.origin == h.origin)
                })
                .map(|h| describe(&h))
                .collect::<Vec<_>>();
            let state = app.state.lock().unwrap();
            let incoming = state
                .reply(None)
                .worker_hosts
                .into_iter()
                .filter(|d| device.is_none_or(|id| state.belongs_to_login(id, &d.login)))
                .collect::<Vec<_>>();
            Ok(
                json!({"hosts":rows.iter().map(describe).collect::<Vec<_>>(), "available":known, "incoming":incoming,
                    "supports_session_moves": app.moves.is_some()}),
            )
        }
        Action::LinkPairedHost { host } => {
            super::background_keychain().map_err(io::Error::other)?;
            let target = paired_hosts::destinations(&destinations)?
                .into_iter()
                .find(|h| h.id == host && Some(&h.origin) != origin.as_ref() && h.id != source_id)
                .ok_or_else(|| io::Error::other("This host is no longer paired."))?;
            let invite = target
                .settings(json!({"action":"invite_worker_host"}))
                .map_err(|e| {
                    if matches!(
                        e.kind(),
                        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
                    ) {
                        io::Error::other("Update the receiving host to create a worker invitation.")
                    } else {
                        e
                    }
                })?;
            let link = invite["url"].as_str().ok_or_else(|| {
                io::Error::other("Update the receiving host to create a worker invitation.")
            })?;
            let invitation = Invitation::parse(link, "")?;
            if invitation.origin != target.origin {
                return Err(io::Error::other("The receiver's pairing address changed."));
            }
            pair(invitation, Some(&target))
        }
        Action::AddWorkerLink { address, code } => {
            super::background_keychain().map_err(io::Error::other)?;
            if paired_hosts::window_address(&address, &code) {
                // An address with no code and no invitation fragment means the
                // host is waiting in a pairing window, so nothing is redeemed.
                let target = paired_hosts::window_origin(&address).map_err(|_| {
                    io::Error::other(
                        "Enter the host's HTTPS .ts.net address, or paste its invitation link.",
                    )
                })?;
                on_tailnet(&target)?;
                let host = paired_hosts::enroll_window(
                    &target,
                    &label,
                    &registry,
                    true,
                    Some(&source_id),
                    |current, new| acceptable(None, current, new),
                )?;
                Ok(describe(&host))
            } else {
                pair(Invitation::parse(&address, &code)?, None)
            }
        }
        Action::AddWorkerLinkWindow { origin: target } => {
            super::background_keychain().map_err(io::Error::other)?;
            let target = crate::serve::origin(target.trim().trim_end_matches('/'))
                .ok_or_else(|| io::Error::other("That is not an HTTPS tailnet address."))?;
            on_tailnet(&target)?;
            let host = paired_hosts::enroll_window(
                &target,
                &label,
                &registry,
                true,
                Some(&source_id),
                |current, new| acceptable(None, current, new),
            )
            .map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    io::Error::other(
                        "That host is not waiting for a device. Open a pairing window there and try again.",
                    )
                } else {
                    e
                }
            })?;
            Ok(describe(&host))
        }
        Action::NearbyWindowHosts => Ok(json!({
            "hosts": crate::discovery::open_hosts(local_https_port)
                .into_iter()
                .filter(|host| Some(&host.origin) != origin.as_ref())
                .map(|host| json!({
                    "origin": host.origin,
                    "name": host.name,
                    "tags": host.tags,
                    "expires_at": host.expires_at,
                }))
                .collect::<Vec<_>>()
        })),
        Action::RemoveWorkerLink { host } => {
            super::background_keychain().map_err(io::Error::other)?;
            let target = rows
                .iter()
                .find(|h| h.id == host)
                .ok_or_else(|| io::Error::other("Unknown worker host."))?;
            let revoked = target
                .client()
                .and_then(|c| {
                    client::runtime()?.block_on(c.worker_request(json!({"action":"revoke_self"})))
                })
                .is_ok();
            target.forget_from(&registry)?;
            Ok(json!({"removed":true, "revoked":revoked}))
        }
    }
}
