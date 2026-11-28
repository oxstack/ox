//! Source-side validation at the boundary of the connected destination host.
use super::{Change, Engine, Message, Scope, Server, target_moves::TargetMoves};
use crate::{
    move_bundle::{Bundle, Presentation},
    move_ledger::{
        Activated, ArchivePin, Cancelled, Commit, Ledger, Plan, Prepared, Proposal, Status,
    },
};
use ox_desk_core::{
    actions::{Action, MoveSession},
    host_protocol::{SessionView, Timeline},
    store::Connection,
};
use ox_host_service::checkout::project_location;
use ox_host_web::{
    move_manifest::Manifest,
    move_transport::{Cancellation, Offer, Reservation, Sender},
    paired_hosts::SavedHost,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Outgoing {
    reserving: HashMap<Uuid, Request>,
    transferring: HashSet<Uuid>,
    reopening: HashMap<Uuid, u64>,
    cancellations: HashMap<Uuid, CancellationDelivery>,
    pub(super) errors: HashMap<Uuid, String>,
}

enum CancellationDelivery {
    Sending,
    RetryAfter(Instant),
}

struct Request {
    client: u64,
    key: String,
    scope: Scope,
    thread: String,
    turn: Option<String>,
}

pub(crate) enum Outcome {
    Reserved(Uuid, io::Result<Plan>),
    Prepared(Plan, io::Result<(ArchivePin, Prepared)>),
    Activated(Plan, io::Result<Activated>),
    Cancelled(Cancelled, io::Result<()>),
}

impl Outgoing {
    pub(super) fn project_row(
        &self,
        ledger: &Ledger,
        state: &Path,
        targets: &TargetMoves,
        scope: &Scope,
        row: &mut SessionView,
    ) -> io::Result<()> {
        if !scope.route.runtime.supports_session_moves() {
            return Ok(());
        }
        targets.stamp_row(scope, row);
        if let Some(progress) = self.progress(ledger, state, scope, &row.id)? {
            attach_move_progress(row, progress);
        } else if is_owned_lead(row)
            && row.detail["native_live"].is_null()
            && row.detail["terminal"] != true
            && (scope.route.runtime != ox_desk_core::runtime::Runtime::Claude
                || row
                    .capabilities
                    .iter()
                    .any(|capability| capability == "end"))
            && row
                .capabilities
                .iter()
                .any(|capability| capability == "open")
        {
            row.capabilities.push("move_session".into());
        }
        Ok(())
    }

    fn progress(
        &self,
        ledger: &Ledger,
        state: &Path,
        scope: &Scope,
        native: &str,
    ) -> io::Result<Option<Value>> {
        let Some((plan, status, stage)) = ledger.source_move_at(scope, native)? else {
            return Ok(None);
        };
        let error = self.errors.get(&plan.id);
        let phase = match status {
            Status::Queued if error.is_some() => "failed",
            Status::Queued if ledger.queued_result_seen(&plan)? => "waiting_for_workers",
            Status::Queued => "waiting",
            Status::Preparing if error.is_some() => "failed",
            Status::Preparing => "preparing",
            Status::Fenced if error.is_none() => "closing",
            Status::Fenced | Status::Released | Status::Committed => "checking",
            Status::Confirmed => "ready",
            Status::Cancelled => "cancelled",
            Status::Prepared | Status::Active => return Ok(None),
        };
        let destination = linked_host(state, plan.target_host).ok();
        let name = destination
            .as_ref()
            .map_or("Connected host", |host| host.name.as_str());
        let origin = destination.as_ref().map(|host| host.origin.as_str());
        let receipt = stage.filter(|_| status == Status::Confirmed).map(
            |stage| json!({"operation":plan.id, "host":plan.target_host, "thread":stage.native, "scope":plan.target_scope}),
        );
        Ok(Some(
            json!({"id":plan.id, "destination":{"id":plan.target_host, "name":name, "origin":origin},
            "phase":phase, "message":error, "receipt":receipt}),
        ))
    }
}

impl Engine {
    fn publish_move_progress(&mut self, scope: &Scope, native: &str) -> io::Result<()> {
        let Some(progress) =
            self.move_send
                .progress(&self.source_moves.ledger, &self.state_dir, scope, native)?
        else {
            return Ok(());
        };
        let state = self
            .running
            .get_mut(scope)
            .map(|live| &mut live.state)
            .or_else(|| self.cold.get_mut(scope));
        let Some(row) =
            state.and_then(|state| state.sessions.iter_mut().find(|row| row.id == native))
        else {
            return Ok(());
        };
        if row.detail["move"] == progress {
            return Ok(());
        }
        attach_move_progress(row, progress);
        let change = Change::Session {
            session: row.clone(),
        };
        self.broadcast(scope, vec![change])
    }

    pub(super) fn view_source_move_status(
        &mut self,
        scope: &Scope,
        action: &Action,
    ) -> io::Result<bool> {
        let native = match action {
            Action::SelectThread(thread) => thread,
            Action::OpenCatalogRow(row) => &row.id,
            _ => return Ok(false),
        };
        let showing_only_status = self
            .source_moves
            .ledger
            .source_move_at(scope, native)?
            .is_some_and(|(_, phase, _)| {
                matches!(
                    phase,
                    Status::Preparing
                        | Status::Fenced
                        | Status::Released
                        | Status::Committed
                        | Status::Confirmed
                )
            });
        if showing_only_status {
            self.publish_move_progress(scope, native)?;
        }
        Ok(showing_only_status)
    }

    pub(super) fn start_move(
        &mut self,
        client: u64,
        key: String,
        scope: Scope,
        action: MoveSession,
    ) -> io::Result<()> {
        if !scope.route.runtime.supports_session_moves() {
            return Err(io::Error::other(
                "This runtime does not support session moves.",
            ));
        }
        let operation = parse_id(&action.operation)?;
        let destination = parse_id(&action.destination)?;
        if self.host_id == destination || self.move_send.reserving.contains_key(&operation) {
            return Err(io::Error::other(
                "Choose another connected host for this move.",
            ));
        }
        if let Some(plan) = self.source_moves.ledger.source_plan(operation)? {
            if plan.source.scope == scope
                && plan.source.native == action.thread
                && plan.target_host == destination
            {
                return self.reply(client, Server::Ack { id: key });
            }
            return Err(io::Error::other("The request ID belongs to another move."));
        }
        if self
            .move_send
            .reserving
            .values()
            .any(|request| request.scope == scope && request.thread == action.thread)
        {
            return Err(io::Error::other(
                "The destination is already checking this session.",
            ));
        }
        let live = self
            .running
            .get(&scope)
            .ok_or_else(|| io::Error::other("Open the session before moving it."))?;
        if live.store.state() != &Connection::Ready {
            return Err(io::Error::other(
                "Open the source session before moving it.",
            ));
        }
        if !live
            .state
            .sessions
            .iter()
            .any(|view| view.id == action.thread && is_owned_lead(view))
        {
            return Err(io::Error::other(
                "Open the lead session to move it after its workers finish.",
            ));
        }
        if live.store.is_native_live(&action.thread) || live.store.in_terminal(&action.thread) {
            return Err(io::Error::other(
                "Close the session’s terminal before moving it.",
            ));
        }
        if scope.route.runtime == ox_desk_core::runtime::Runtime::Claude
            && !live.store.has_live_session(&action.thread)
        {
            return Err(io::Error::other("Run this session in OX before moving it."));
        }
        let turn = if action.after_turn {
            Some(
                live.store
                    .transcript(&action.thread)
                    .and_then(|view| view.active_turn())
                    .ok_or_else(|| {
                        io::Error::other("The active turn changed. Open Move session again.")
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        let (session, source) = self
            .source_moves
            .ledger
            .source_identity(&scope, &action.thread)?;
        let earlier_cancellations: Vec<_> = self
            .source_moves
            .ledger
            .pending_cancellations()?
            .into_iter()
            .filter(|entry| entry.plan.session == session && entry.plan.target_host == destination)
            .collect();
        let state = self.state_dir.clone();
        let actor = self.tx.clone();
        self.move_send.reserving.insert(
            operation,
            Request {
                client,
                key,
                scope,
                thread: action.thread,
                turn,
            },
        );
        thread::spawn(move || {
            let result = (|| {
                let destination = linked_host(&state, destination)?;
                // Clear an earlier reservation before asking the same host to
                // accept this session again. Delivery is idempotent.
                for entry in earlier_cancellations {
                    deliver_cancellation(&destination, &entry)?;
                    let _ = actor.send(Message::MoveSend(Box::new(Outcome::Cancelled(
                        entry,
                        Ok(()),
                    ))));
                }
                let (_, relative) = project_location(&source.scope.project)?;
                reserve(
                    &destination,
                    operation,
                    Proposal {
                        session,
                        source,
                        project_in_checkout: relative.as_os_str().as_bytes().to_vec(),
                    },
                )
            })();
            let _ = actor.send(Message::MoveSend(Box::new(Outcome::Reserved(
                operation, result,
            ))));
        });
        Ok(())
    }

    pub(super) fn move_command(&mut self, scope: &Scope, command: &Action) -> io::Result<()> {
        let id = match command {
            Action::CancelMove(id) | Action::CheckMove(id) | Action::RetryMove(id) => parse_id(id)?,
            _ => return Err(io::Error::other("Unknown move action.")),
        };
        let plan = self
            .source_moves
            .ledger
            .source_plan(id)?
            .filter(|plan| &plan.source.scope == scope)
            .ok_or_else(|| io::Error::other("This move belongs to another session."))?;
        if matches!(command, Action::CancelMove(_)) {
            self.source_moves.ledger.cancel_source(&plan)?;
        }
        self.move_send.errors.remove(&id);
        self.move_send.reopening.remove(&id);
        if matches!(command, Action::RetryMove(_)) {
            let ledger = &self.source_moves.ledger;
            let status = ledger.source_status(&plan)?;
            if status == Status::Preparing
                || (status == Status::Queued && ledger.queued_result_seen(&plan)?)
            {
                self.ensure(scope, None)?;
                self.advance_source_moves()?;
            }
        }
        if matches!(command, Action::CheckMove(_)) {
            self.source_moves.retry_refused_release(id);
            self.advance_source_moves()?;
        }
        self.publish_move_progress(scope, &plan.source.native)?;
        self.drive_move_transfers()
    }

    pub(super) fn move_outcome(&mut self, outcome: Outcome) -> io::Result<()> {
        let (plan, result) = match outcome {
            Outcome::Cancelled(cancelled, result) => {
                let id = cancelled.plan.id;
                match result {
                    Ok(()) => {
                        self.source_moves.ledger.confirm_cancellation(&cancelled)?;
                        self.move_send.cancellations.remove(&id);
                    }
                    Err(error) => {
                        if let Some(delivery) = self.move_send.cancellations.get_mut(&id) {
                            *delivery = CancellationDelivery::RetryAfter(
                                Instant::now() + Duration::from_secs(30),
                            );
                            ox_host_service::errlog::record(&format!(
                                "move cancellation could not reach the destination: {error}"
                            ));
                        }
                    }
                }
                return Ok(());
            }
            Outcome::Reserved(id, result) => {
                let Some(request) = self.move_send.reserving.remove(&id) else {
                    return Ok(());
                };
                let result = result.and_then(|plan| {
                    match request.turn {
                        Some(turn) => self.queue_move_for_turn(&plan, turn)?,
                        None => self.prepare_source_move(&plan, false)?,
                    };
                    self.publish_move_progress(&plan.source.scope, &plan.source.native)
                });
                let frame = match result {
                    Ok(_) => Server::Ack { id: request.key },
                    Err(error) => Server::Error {
                        id: Some(request.key),
                        message: error.to_string(),
                    },
                };
                self.reply(request.client, frame)?;
                return self.drive_move_transfers();
            }
            Outcome::Prepared(plan, result) => {
                if self.source_moves.ledger.source_status(&plan)? != Status::Preparing {
                    self.move_send.transferring.remove(&plan.id);
                    return Ok(());
                }
                let result = result.and_then(|(archive, prepared)| {
                    self.bind_move_manifest(&plan, archive)?;
                    self.stage_source_move(plan.target_host, &prepared)
                        .map(|_| ())
                });
                (plan, result)
            }
            Outcome::Activated(plan, result) => {
                let result =
                    result.and_then(|receipt| self.confirm_move(plan.target_host, &receipt));
                (plan, result)
            }
        };
        self.move_send.transferring.remove(&plan.id);
        if let Err(error) = result {
            self.move_send.errors.insert(plan.id, error.to_string());
        }
        self.publish_move_progress(&plan.source.scope, &plan.source.native)?;
        self.drive_move_transfers()
    }

    pub(super) fn drive_move_transfers(&mut self) -> io::Result<()> {
        self.drive_move_cancellations()?;
        for (plan, phase) in self.source_moves.ledger.pending_sources()? {
            self.publish_move_progress(&plan.source.scope, &plan.source.native)?;
            if self.move_send.transferring.contains(&plan.id)
                || self.move_send.errors.contains_key(&plan.id)
            {
                continue;
            }
            let work = match phase {
                Status::Preparing => {
                    if !self.restore_source_move_history(&plan) {
                        continue;
                    }
                    // A client can send to a moved session without opening its
                    // timeline first. Carry the existing archive on every hop.
                    match self.load_moved_history(&plan.source.scope, &plan.source.native, false) {
                        Ok(true) => self.process_scope(&plan.source.scope, vec![], true)?,
                        Ok(false) => {}
                        Err(error) => {
                            self.move_send.errors.insert(plan.id, error.to_string());
                            continue;
                        }
                    }
                    let Ok(timeline) = self.move_source_timeline(&plan) else {
                        continue;
                    };
                    if timeline.detail["older"] == true {
                        if self.load_moved_history(&plan.source.scope, &plan.source.native, true)? {
                            self.process_scope(&plan.source.scope, vec![], true)?;
                            continue;
                        }
                        let live = self
                            .running
                            .get_mut(&plan.source.scope)
                            .expect("move timeline has a live scope");
                        if let Err(error) = live.store.load_older(&plan.source.native) {
                            self.move_send.errors.insert(plan.id, error.to_string());
                        }
                        continue;
                    }
                    let Some(row) = self.running[&plan.source.scope]
                        .state
                        .sessions
                        .iter()
                        .find(|row| row.id == plan.source.native)
                    else {
                        continue;
                    };
                    Work::Prepare(Presentation::from(row), timeline.clone())
                }
                Status::Committed => Work::Activate(self.committed_move(&plan)?),
                _ => continue,
            };
            self.move_send.transferring.insert(plan.id);
            let state = self.state_dir.clone();
            let actor = self.tx.clone();
            thread::spawn(move || {
                let outcome = match work {
                    Work::Prepare(presentation, timeline) => {
                        let result = (|| {
                            let destination = linked_host(&state, plan.target_host)?;
                            let bundle = Bundle::capture(&state, &plan, presentation, &timeline)?;
                            let archive = ArchivePin {
                                manifest: bundle.manifest.sha256()?,
                                transcript: bundle.manifest.transcript.sha256,
                            };
                            Ok((archive, prepare(&destination, &plan, &bundle)?))
                        })();
                        Outcome::Prepared(plan, result)
                    }
                    Work::Activate(commit) => {
                        let result = linked_host(&state, plan.target_host)
                            .and_then(|destination| activate(&destination, &state, &commit));
                        Outcome::Activated(plan, result)
                    }
                };
                let _ = actor.send(Message::MoveSend(Box::new(outcome)));
            });
        }
        Ok(())
    }

    pub(super) fn restore_source_move_history(&mut self, plan: &Plan) -> bool {
        if self.move_send.errors.contains_key(&plan.id) {
            return false;
        }
        match self.reopen_source_move_history(plan) {
            Ok(ready) => ready,
            Err(error) => {
                self.move_send.errors.insert(plan.id, error.to_string());
                false
            }
        }
    }

    fn reopen_source_move_history(&mut self, plan: &Plan) -> io::Result<bool> {
        let scope = &plan.source.scope;
        let thread = &plan.source.native;
        if !self.running.contains_key(scope) {
            self.ensure(scope, None)?;
        }
        let live = self.running.get_mut(scope).expect("source route opened");
        match live.store.state() {
            Connection::Connecting => return Ok(false),
            Connection::Ready => {}
            _ => {
                return Err(io::Error::other(
                    "Reconnect the source runtime and retry the move.",
                ));
            }
        }
        if live.store.is_native_live(thread) {
            return Err(io::Error::other(
                "Close the session in the other runtime before moving it.",
            ));
        }
        let transcript = live.store.transcript(thread);
        if transcript.is_some_and(|view| view.is_loading()) {
            return Ok(false);
        }
        if transcript.is_some_and(|view| view.is_ready())
            && live.store.move_writer_group(thread).is_some()
        {
            self.move_send.reopening.remove(&plan.id);
            return Ok(true);
        }
        if self.move_send.reopening.get(&plan.id) == Some(&live.generation) {
            return Err(io::Error::other(
                transcript
                    .and_then(|view| view.error())
                    .unwrap_or("The source history could not be reopened."),
            ));
        }
        if live.store.catalog_row(thread).is_none()
            && let Some(row) = live.recovered.iter().find(|row| row.id == *thread)
        {
            live.store
                .adopt_catalog_row(serde_json::from_value(row.detail["row"].clone())?)?;
        }
        live.store.reopen_move_source_writer(thread)?;
        self.move_send.reopening.insert(plan.id, live.generation);
        Ok(false)
    }

    fn drive_move_cancellations(&mut self) -> io::Result<()> {
        for entry in self.source_moves.ledger.pending_cancellations()? {
            let id = entry.plan.id;
            if match self.move_send.cancellations.get(&id) {
                Some(CancellationDelivery::Sending) => true,
                Some(CancellationDelivery::RetryAfter(at)) => *at > Instant::now(),
                None => false,
            } {
                continue;
            }
            self.move_send
                .cancellations
                .insert(id, CancellationDelivery::Sending);
            let state = self.state_dir.clone();
            let actor = self.tx.clone();
            thread::spawn(move || {
                let result = linked_host(&state, entry.plan.target_host)
                    .and_then(|destination| deliver_cancellation(&destination, &entry));
                let _ = actor.send(Message::MoveSend(Box::new(Outcome::Cancelled(
                    entry, result,
                ))));
            });
        }
        Ok(())
    }
}

fn attach_move_progress(row: &mut ox_desk_core::host_protocol::SessionView, progress: Value) {
    row.detail["move"] = progress;
    if !row.capabilities.iter().any(|value| value == "move_session") {
        row.capabilities.push("move_session".into());
    }
}

fn is_owned_lead(row: &ox_desk_core::host_protocol::SessionView) -> bool {
    row.detail["owned"] == true && row.parent.is_none() && row.detail["worker"] != true
}

enum Work {
    Prepare(Presentation, Timeline),
    Activate(Commit),
}

fn deliver_cancellation(destination: &SavedHost, cancelled: &Cancelled) -> io::Result<()> {
    let plan = &cancelled.plan;
    let receipt = Sender { destination }.cancel(&Cancellation {
        source: plan.source.host.to_string(),
        target: plan.target_host.to_string(),
        transfer: plan.id.to_string(),
        cancelled_plan: serde_json::to_string(plan)?,
    })?;
    if receipt["cancelled"] != true {
        return Err(io::Error::other(
            "The destination did not acknowledge the cancellation.",
        ));
    }
    Ok(())
}

fn linked_host(state: &Path, id: Uuid) -> io::Result<SavedHost> {
    let id = id.to_string();
    ox_host_web::paired_hosts::worker_links(&state.join("worker-links.json"))?
        .into_iter()
        .find(|candidate| candidate.id == id)
        .ok_or_else(|| io::Error::other("The destination host link is unavailable."))
}

fn parse_id(id: &str) -> io::Result<Uuid> {
    Uuid::parse_str(id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid host or move ID."))
}

pub(super) fn reserve(
    destination: &SavedHost,
    operation: Uuid,
    proposal: Proposal,
) -> io::Result<Plan> {
    let sender = Sender { destination };
    let reserve = Reservation {
        source: proposal.source.host.to_string(),
        target: destination.id.clone(),
        transfer: operation.to_string(),
        proposal: serde_json::to_string(&proposal)?,
    };
    let receipt = sender.reserve(&reserve)?;
    let raw = receipt
        .as_str()
        .ok_or_else(|| io::Error::other("The destination did not return a session move plan."))?;
    let plan: Plan = serde_json::from_str(raw)?;
    if plan.id != operation
        || plan.session != proposal.session
        || plan.source != proposal.source
        || plan.target_host.to_string() != destination.id
        || plan.target_scope.route != proposal.source.scope.route
    {
        return Err(io::Error::other(
            "The destination approved a different session move.",
        ));
    }
    Ok(plan)
}

pub(super) fn prepare(
    destination: &SavedHost,
    plan: &Plan,
    bundle: &Bundle,
) -> io::Result<Prepared> {
    let offer = offer(destination, plan, &bundle.manifest)?;
    let prepared: Prepared = Sender { destination }.prepare(&offer, bundle.directory())?;
    if prepared.plan != *plan || prepared.stage.digest != offer.manifest.sha256()? {
        return Err(io::Error::other(
            "The destination prepared different move files.",
        ));
    }
    Ok(prepared)
}

pub(super) fn activate(
    destination: &SavedHost,
    state: &Path,
    commit: &Commit,
) -> io::Result<Activated> {
    let plan = &commit.prepared.plan;
    let bundle = Bundle::recover(state, plan)?;
    let offer = offer(destination, plan, &bundle.manifest)?;
    if offer.manifest.sha256()? != commit.prepared.stage.digest {
        return Err(io::Error::other(
            "The staged files differ from the committed move.",
        ));
    }
    let activated: Activated =
        serde_json::from_value(Sender { destination }.activate(&offer, commit)?)?;
    if activated.commit != *commit {
        return Err(io::Error::other(
            "The destination confirmed a different move.",
        ));
    }
    Ok(activated)
}

fn offer(destination: &SavedHost, plan: &Plan, manifest: &Manifest) -> io::Result<Offer> {
    if destination.id != plan.target_host.to_string() {
        return Err(io::Error::other("The saved link belongs to another host."));
    }
    Ok(Offer {
        source: plan.source.host.to_string(),
        target: destination.id.clone(),
        transfer: plan.id.to_string(),
        manifest: manifest.clone(),
    })
}
