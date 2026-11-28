//! A directional, signed move over an existing worker host link.
use crate::{
    client,
    paired_hosts::SavedHost,
    server::{App, Proof, paired_identity},
    state::{move_admission::Binding, move_staging},
    worker_checkout::{CHUNK, connection::Connection},
};
use axum::{
    Json,
    extract::{State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::Response,
};
pub use move_staging::{Offer, StagedMove, VerifiedActivation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub source: String,
    pub target: String,
    pub transfer: String,
    pub proposal: String,
}

impl Reservation {
    fn binding(&self, plan: &str) -> Binding {
        Binding::for_plan(&self.source, &self.target, &self.transfer, plan.as_bytes())
    }
}

pub enum Incoming {
    /// Validate the proposal and return the canonical JSON plan as a string.
    /// Only the destination chooses the local checkout path in that plan.
    Reserve(Reservation),
    Prepare(StagedMove),
    Activate(VerifiedActivation<Value>),
    Cancel(VerifiedCancellation),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Cancellation {
    pub source: String,
    pub target: String,
    pub transfer: String,
    pub cancelled_plan: String,
}

impl Cancellation {
    fn binding(&self) -> Binding {
        Binding::for_plan(
            &self.source,
            &self.target,
            &self.transfer,
            self.cancelled_plan.as_bytes(),
        )
    }
}

pub struct VerifiedCancellation {
    state: Arc<Mutex<crate::state::State>>,
    device: String,
    request: Cancellation,
}

impl VerifiedCancellation {
    pub fn cancel<R>(self, apply: impl FnOnce(&Cancellation) -> io::Result<R>) -> io::Result<R> {
        let mut state = self.state.lock().unwrap();
        state.check_move_reservation(&self.device, &self.request.binding())?;
        let result = apply(&self.request)?;
        state.revoke_move_admission(&self.request.transfer)?;
        Ok(result)
    }
}

pub type Receiver = Arc<dyn Fn(Incoming) -> io::Result<Value> + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    request: String,
    proof: Proof,
}

pub(crate) async fn reserve(
    State(app): State<App>,
    headers: HeaderMap,
    Json(open): Json<Envelope>,
) -> Result<Json<Value>, StatusCode> {
    let device = paired_identity(&app, &headers)?;
    let receiver = app.moves.ok_or(StatusCode::NOT_FOUND)?;
    let reservation: Reservation =
        serde_json::from_str(&open.request).map_err(|_| StatusCode::BAD_REQUEST)?;
    let peer = reservation.binding("");
    {
        let mut state = app.state.lock().unwrap();
        if !state.prove_worker(
            &device,
            &open.proof.nonce,
            &open.proof.signature,
            &open.request,
        ) || state.check_move_peer(&device, &peer).is_err()
        {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let slot = app
        .slots
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let result = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let returned = receiver(Incoming::Reserve(reservation.clone()))?;
        let plan = returned
            .as_str()
            .ok_or_else(|| io::Error::other("The destination did not produce a move plan."))?;
        app.state
            .lock()
            .unwrap()
            .reserve_move_plan(&device, &reservation.binding(plan))?;
        Ok(returned)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(reply(result))
}

pub(crate) async fn prepare(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let device = paired_identity(&app, &headers)?;
    let receiver = app.moves.ok_or(StatusCode::NOT_FOUND)?;
    if !app
        .state
        .lock()
        .unwrap()
        .worker_grant(&device)
        .is_some_and(|grant| grant.may_accept_moves)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let slot = app
        .slots
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(ws
        .max_message_size(CHUNK)
        .max_frame_size(CHUNK)
        .on_upgrade(move |socket| async move {
            let mut connection = Connection::open(socket);
            let result =
                move_staging::stage_socket(app.state, &device, &mut connection, move |files| {
                    let _slot = slot;
                    receiver(Incoming::Prepare(files))
                })
                .await;
            if let Err(error) = result {
                move_staging::send_error(&connection, &error).await;
            }
            connection.finish().await;
        }))
}

pub(crate) async fn activate(
    State(app): State<App>,
    headers: HeaderMap,
    Json(open): Json<Envelope>,
) -> Result<Json<Value>, StatusCode> {
    let device = paired_identity(&app, &headers)?;
    let receiver = app.moves.ok_or(StatusCode::NOT_FOUND)?;
    let slot = app
        .slots
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let activation = VerifiedActivation::open(app.state, &device, &open.request, &open.proof)
        .map_err(|_| StatusCode::FORBIDDEN)?;
    let result = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        receiver(Incoming::Activate(activation))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(reply(result))
}

pub(crate) async fn cancel(
    State(app): State<App>,
    headers: HeaderMap,
    Json(open): Json<Envelope>,
) -> Result<Json<Value>, StatusCode> {
    let device = paired_identity(&app, &headers)?;
    let receiver = app.moves.ok_or(StatusCode::NOT_FOUND)?;
    let request: Cancellation =
        serde_json::from_str(&open.request).map_err(|_| StatusCode::BAD_REQUEST)?;
    {
        let mut state = app.state.lock().unwrap();
        if !state.prove_worker(
            &device,
            &open.proof.nonce,
            &open.proof.signature,
            &open.request,
        ) || state
            .check_move_reservation(&device, &request.binding())
            .is_err()
        {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let slot = app
        .slots
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let verified = VerifiedCancellation {
        state: app.state,
        device,
        request,
    };
    let result = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        receiver(Incoming::Cancel(verified))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(reply(result))
}

fn reply(result: io::Result<Value>) -> Json<Value> {
    Json(match result {
        Ok(receipt) => json!({"receipt": receipt}),
        Err(error) => json!({"error": error.to_string()}),
    })
}

/// Repeated calls authenticate the same move, even when a previous reply was lost.
pub struct Sender<'a> {
    pub destination: &'a SavedHost,
}

impl Sender<'_> {
    pub fn reserve(&self, request: &Reservation) -> io::Result<Value> {
        self.control("move-reserve", serde_json::to_string(request)?)
    }

    pub fn prepare<T: serde::de::DeserializeOwned>(
        &self,
        offer: &Offer,
        files: &Path,
    ) -> io::Result<T> {
        let peer = self.destination.client()?;
        let request = serde_json::to_string(offer)?;
        client::runtime()?.block_on(async {
            let proof = peer.proof("worker", Some(&request)).await?;
            let socket = peer
                .websocket("move-stage", json!({"request": request, "proof": proof}))
                .await?;
            move_staging::send_files_for_prepare(&mut Connection::open(socket), offer, files).await
        })
    }

    pub fn activate<T: Serialize>(&self, offer: &Offer, commit: &T) -> io::Result<Value> {
        self.control(
            "move-activate",
            serde_json::to_string(&json!({"offer": offer, "commit": commit}))?,
        )
    }

    pub fn cancel(&self, request: &Cancellation) -> io::Result<Value> {
        self.control("move-cancel", serde_json::to_string(request)?)
    }

    fn control(&self, path: &str, request: String) -> io::Result<Value> {
        let peer = self.destination.client()?;
        client::runtime()?.block_on(async {
            let proof = peer.proof("worker", Some(&request)).await?;
            let mut reply = peer
                .post(
                    path,
                    json!({"request": request, "proof": proof}),
                    "The destination host couldn’t accept this session move.",
                    Duration::from_secs(30),
                )
                .await?;
            if let Some(message) = reply["error"].as_str() {
                return Err(io::Error::other(message.to_owned()));
            }
            let receipt = reply["receipt"].take();
            if receipt.is_null() {
                return Err(io::Error::other(
                    "The destination returned no move receipt.",
                ));
            }
            Ok(receipt)
        })
    }
}
