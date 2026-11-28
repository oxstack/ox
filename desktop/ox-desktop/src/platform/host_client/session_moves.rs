//! Typed views of the host's move status and linked destination choices.
use super::HostLink;
use ox_desk_core::{host_protocol::Scope, runtime::Route, store::Store};
use serde::Deserialize;
use serde_json::Value;
use std::{io, path::PathBuf};

pub(crate) struct MoveDestination {
    pub id: String,
    pub name: String,
    pub unavailable: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MovePhase {
    Waiting,
    WaitingForWorkers,
    Preparing,
    Closing,
    Checking,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Deserialize)]
pub(crate) struct MoveStatus {
    pub id: String,
    pub destination: MoveHost,
    pub phase: MovePhase,
    pub message: Option<String>,
    receipt: Option<MoveReceipt>,
}

#[derive(Deserialize)]
pub(crate) struct MoveHost {
    id: String,
    pub name: String,
    pub origin: Option<String>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct MoveReceipt {
    operation: String,
    pub host: String,
    pub thread: String,
    pub scope: Scope,
}

impl MoveStatus {
    pub(crate) fn read(store: &Store, thread: &str) -> Option<Self> {
        serde_json::from_value(store.session_mark(thread, "move")?.clone()).ok()
    }

    pub(crate) fn verified_receipt(&self) -> Option<&MoveReceipt> {
        self.receipt.as_ref().filter(|receipt| {
            self.phase == MovePhase::Ready
                && self.id == receipt.operation
                && self.destination.id == receipt.host
                && !receipt.thread.is_empty()
        })
    }
}

impl HostLink {
    pub(crate) fn move_destinations(
        &self,
        project: PathBuf,
        route: &Route,
    ) -> io::Result<Vec<MoveDestination>> {
        Ok(destinations(self.worker_hosts(project)?.as_ref(), route))
    }
}

fn destinations(inventory: Option<&Value>, route: &Route) -> Vec<MoveDestination> {
    inventory
        .and_then(|report| report["hosts"].as_array())
        .into_iter()
        .flatten()
        .filter(|row| row["parent_host"] != true)
        .filter_map(|row| {
            let unavailable = match row["status"].as_str() {
                Some("online") if row["supports_session_moves"] != true => Some("OX update needed"),
                Some("online") if row["grant"]["may_accept_moves"] != true => {
                    Some("Accept session moves is off")
                }
                Some("online")
                    if !row["routes"].as_array().is_some_and(|routes| {
                        routes.iter().any(|r| {
                            serde_json::from_value::<Route>(r.clone()).is_ok_and(|r| &r == route)
                        })
                    }) =>
                {
                    Some("Runtime or account unavailable")
                }
                Some("online") => None,
                Some("access_denied") => Some("Host link denied"),
                Some("needs_update") => Some("OX update needed"),
                _ => Some("Offline or unreachable"),
            };
            Some(MoveDestination {
                id: row["host"]["id"].as_str()?.into(),
                name: row["host"]["name"].as_str()?.into(),
                unavailable,
            })
        })
        .collect()
}
