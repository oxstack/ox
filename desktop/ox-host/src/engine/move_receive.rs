//! Connect signed incoming moves with the destination's single host actor.
use super::{Engine, Message};
use crate::{
    move_ledger::{Activated, Cancelled, Commit, Ledger, Plan, Prepared, Proposal},
    move_target::{Files, VerifiedTarget},
};
use ox_host_web::move_transport::{
    Incoming, Receiver, Reservation, VerifiedActivation, VerifiedCancellation,
};
use serde_json::Value;
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Sender, SyncSender},
    },
    time::Duration,
};
use uuid::Uuid;

pub(crate) enum Request {
    Reserve(Reservation, Sender<io::Result<Plan>>),
    Prepare {
        source: Uuid,
        target: Box<VerifiedTarget>,
        reply: Sender<io::Result<Prepared>>,
    },
    Activate(VerifiedActivation<Value>, Sender<io::Result<Activated>>),
    Cancel(VerifiedCancellation, Sender<io::Result<()>>),
}

pub(crate) fn receiver(state: PathBuf, host: Uuid, actor: SyncSender<Message>) -> Receiver {
    Arc::new(move |request| match request {
        Incoming::Reserve(reservation) => {
            let plan = call(&actor, |reply| Request::Reserve(reservation, reply))?;
            Ok(Value::String(serde_json::to_string(&plan)?))
        }
        Incoming::Prepare(staged) => {
            let source = id(&staged.offer.source)?;
            let expected_plan = staged.expected_plan.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "The destination did not reserve a plan for this move.",
                )
            })?;
            let directory = staged
                .plan
                .parent()
                .ok_or_else(|| io::Error::other("Move staging directory is missing."))?;
            // The archive and Git checkout can be large. Verify and install
            // them off the actor, then recheck authority before opening a writer.
            let ledger = Ledger::open(&state, host)?;
            let target = VerifiedTarget::install(
                &state,
                &ledger,
                source,
                Files {
                    directory,
                    manifest: &staged.offer.manifest,
                    manifest_sha256: staged.receipt.manifest_sha256,
                    expected_plan: Some(expected_plan),
                },
            )?;
            if target.plan().id.to_string() != staged.offer.transfer {
                return Err(io::Error::other(
                    "The staged move belongs to another transfer.",
                ));
            }
            let prepared = call(&actor, |reply| Request::Prepare {
                source,
                target: Box::new(target),
                reply,
            })?;
            Ok(serde_json::to_value(prepared)?)
        }
        Incoming::Activate(activation) => {
            let activated = call(&actor, |reply| Request::Activate(activation, reply))?;
            Ok(serde_json::to_value(activated)?)
        }
        Incoming::Cancel(cancellation) => {
            call(&actor, |reply| Request::Cancel(cancellation, reply))?;
            Ok(serde_json::json!({"cancelled": true}))
        }
    })
}

fn call<T>(
    actor: &SyncSender<Message>,
    command: impl FnOnce(Sender<io::Result<T>>) -> Request,
) -> io::Result<T> {
    let (reply, receive) = mpsc::channel();
    actor
        .send(Message::MoveReceive(Box::new(command(reply))))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "The destination host stopped."))?;
    receive
        .recv_timeout(Duration::from_secs(60))
        .map_err(io::Error::other)?
}

#[test]
fn reservation_uses_the_receiving_hosts_route_and_project_directory() {
    use crate::{executable, move_ledger::Owner, runtime::RuntimeConfig};
    use ox_desk_core::{
        host_protocol::Scope,
        runtime::Route,
        settings_protocol::{VERSION, model::Snapshot},
    };

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().canonicalize().unwrap();
    let host = crate::identity::load(&state).unwrap();
    let snapshot = serde_json::to_string(&Snapshot {
        version: VERSION,
        chatgpt_account: Some("fixture".into()),
        ..Default::default()
    })
    .unwrap();
    let ox = state.join("ox");
    executable::write(
        &ox,
        &format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{snapshot}'\n"),
    );
    let (mailbox, actor) = mpsc::sync_channel(16);
    let config = RuntimeConfig {
        ox,
        path: "/usr/bin:/bin".into(),
        desktop: state.clone(),
    };
    let actor_mailbox = mailbox.clone();
    let actor_state = state.clone();
    let worker = std::thread::spawn(move || {
        Engine::open(config, &actor_state, actor_mailbox)
            .unwrap()
            .serve(actor)
            .unwrap()
    });
    let dispatch = receiver(state.clone(), host, mailbox.clone());
    let proposal = Proposal {
        session: Uuid::new_v4(),
        source: Owner {
            host: Uuid::new_v4(),
            generation: 0,
            native: "source-session".into(),
            scope: Scope {
                project: "/source/checkout/project".into(),
                route: Route::CODEX_SUBSCRIPTION,
            },
        },
        project_in_checkout: b"project".to_vec(),
    };
    let transfer = Uuid::new_v4();
    let reservation = Reservation {
        source: proposal.source.host.to_string(),
        target: host.to_string(),
        transfer: transfer.to_string(),
        proposal: serde_json::to_string(&proposal).unwrap(),
    };
    let reply = dispatch(Incoming::Reserve(reservation.clone())).unwrap();
    let plan: Plan = serde_json::from_str(reply.as_str().unwrap()).unwrap();
    assert_eq!(plan.source, proposal.source);
    assert_eq!(plan.target_scope.route, Route::CODEX_SUBSCRIPTION);
    assert_eq!(
        plan.target_scope.project,
        state
            .join(transfer.to_string())
            .join("checkout/checkout/project")
    );
    let root_transfer = Uuid::new_v4();
    let root = Reservation {
        transfer: root_transfer.to_string(),
        proposal: serde_json::to_string(&Proposal {
            project_in_checkout: Vec::new(),
            ..proposal
        })
        .unwrap(),
        ..reservation.clone()
    };
    let reply = dispatch(Incoming::Reserve(root)).unwrap();
    let root_plan: Plan = serde_json::from_str(reply.as_str().unwrap()).unwrap();
    let clean_path = state
        .join(root_transfer.to_string())
        .join("checkout/project");
    assert_eq!(
        root_plan.target_scope.project.as_os_str(),
        clean_path.as_os_str()
    );
    let other = Reservation {
        source: Uuid::new_v4().to_string(),
        ..reservation
    };
    assert!(dispatch(Incoming::Reserve(other)).is_err());
    mailbox.send(Message::Stop).unwrap();
    worker.join().unwrap();
}

fn id(id: &str) -> io::Result<Uuid> {
    Uuid::parse_str(id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid host or move ID."))
}

impl Engine {
    pub(super) fn receive_move(&mut self, request: Request) {
        match request {
            Request::Reserve(request, reply) => {
                let _ = reply.send(self.reserve_move_target(request));
            }
            Request::Prepare {
                source,
                target,
                reply,
            } => {
                if let Err(error) = self.prepare_move_target(source, *target, reply.clone()) {
                    let _ = reply.send(Err(error));
                }
            }
            Request::Activate(activation, reply) => {
                let result = activation.commit(|source, transfer, digest, raw| {
                    let commit: Commit = serde_json::from_value(raw)?;
                    if commit.prepared.plan.id != id(transfer)?
                        || commit.prepared.stage.digest != digest
                    {
                        return Err(io::Error::other("The commit differs from the signed move."));
                    }
                    self.activate_move_target(id(source)?, &commit)
                });
                let _ = reply.send(result);
            }
            Request::Cancel(cancellation, reply) => {
                let result = cancellation.cancel(|request| {
                    let cancelled = Cancelled {
                        plan: serde_json::from_str(&request.cancelled_plan)?,
                    };
                    if cancelled.plan.id != id(&request.transfer)?
                        || cancelled.plan.target_host != id(&request.target)?
                    {
                        return Err(io::Error::other(
                            "The cancellation differs from the signed move.",
                        ));
                    }
                    self.source_moves
                        .ledger
                        .cancel_receiver(id(&request.source)?, &cancelled)
                });
                let _ = reply.send(result);
            }
        }
    }

    fn reserve_move_target(&self, request: Reservation) -> io::Result<Plan> {
        if self.draining.is_some() {
            return Err(io::Error::other(crate::handover::REFUSAL));
        }
        let proposal: Proposal = serde_json::from_str(&request.proposal)?;
        if !proposal.source.scope.route.runtime.supports_session_moves() {
            return Err(io::Error::other(
                "This runtime does not support session moves.",
            ));
        }
        let plan = VerifiedTarget::reserve_plan(
            &self.state_dir,
            &self.source_moves.ledger,
            id(&request.source)?,
            self.host_id,
            id(&request.transfer)?,
            proposal,
        )?;
        if plan.target_host != id(&request.target)? {
            return Err(io::Error::other(
                "The proposal names another destination host.",
            ));
        }
        let route = &plan.target_scope.route;
        let available = self.config.prepare()?;
        if !available.routes.contains(route) || route.is_api() && !available.has_api_provider() {
            return Err(io::Error::other(format!(
                "{} is unavailable on this host.",
                route.label()
            )));
        }
        Ok(plan)
    }
}
