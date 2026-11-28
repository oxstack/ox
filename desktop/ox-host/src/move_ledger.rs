//! Durable ownership transitions for an operator-requested session move.
//! The host actor validates peer proofs, stages files, and releases the native
//! writer. This ledger binds those effects to the same move on every retry.
use crate::move_writer::WriterGroup;
use ox_desk_core::host_protocol::Scope;
use ox_desk_core::store::MoveWriterRelease;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owner {
    pub host: Uuid,
    pub generation: u64,
    pub scope: Scope,
    pub native: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub id: Uuid,
    pub session: Uuid,
    pub source: Owner,
    pub target_host: Uuid,
    pub target_scope: Scope,
}

/// A source offers its current writer and project within the captured checkout.
/// The destination derives its own absolute project path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Proposal {
    pub session: Uuid,
    pub source: Owner,
    pub project_in_checkout: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stage {
    pub digest: [u8; 32],
    pub native: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prepared {
    pub plan: Plan,
    pub stage: Stage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArchivePin {
    pub(crate) manifest: [u8; 32],
    pub(crate) transcript: [u8; 32],
}

/// Sent over the authenticated source-host connection only after the fence
/// and native writer release have been recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Commit {
    pub prepared: Prepared,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Activated {
    pub commit: Commit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cancelled {
    pub plan: Plan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Queued,
    Preparing,
    Fenced,
    Released,
    Committed,
    Confirmed,
    Prepared,
    Active,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    SourceQueued {
        turn: String,
        #[serde(default)]
        finished: bool,
    },
    SourcePreparing {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        archive: Option<ArchivePin>,
    },
    SourceFenced {
        stage: Stage,
        released: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writer: Option<WriterGroup>,
    },
    SourceCommitted {
        stage: Stage,
        confirmed: bool,
    },
    SourceCancelled {
        #[serde(default)]
        delivered: bool,
    },
    TargetPrepared {
        stage: Stage,
    },
    TargetActive {
        stage: Stage,
    },
    TargetCancelled {
        stage: Option<Stage>,
    },
}

impl Phase {
    fn status(&self) -> Status {
        match self {
            Self::SourceQueued { .. } => Status::Queued,
            Self::SourcePreparing { .. } => Status::Preparing,
            Self::SourceFenced {
                released: false, ..
            } => Status::Fenced,
            Self::SourceFenced { released: true, .. } => Status::Released,
            Self::SourceCommitted {
                confirmed: false, ..
            } => Status::Committed,
            Self::SourceCommitted {
                confirmed: true, ..
            } => Status::Confirmed,
            Self::SourceCancelled { .. } | Self::TargetCancelled { .. } => Status::Cancelled,
            Self::TargetPrepared { .. } => Status::Prepared,
            Self::TargetActive { .. } => Status::Active,
        }
    }

    fn pending(&self) -> bool {
        matches!(
            self,
            Self::SourceQueued { .. }
                | Self::SourcePreparing { .. }
                | Self::SourceFenced { .. }
                | Self::TargetPrepared { .. }
        )
    }

    fn stage(&self) -> Option<&Stage> {
        match self {
            Self::SourceFenced { stage, .. }
            | Self::SourceCommitted { stage, .. }
            | Self::TargetPrepared { stage }
            | Self::TargetActive { stage } => Some(stage),
            Self::TargetCancelled { stage } => stage.as_ref(),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    plan: Plan,
    phase: Phase,
}

impl Record {
    fn owner(&self) -> Owner {
        match &self.phase {
            Phase::SourceCommitted { stage, .. } | Phase::TargetActive { stage } => Owner {
                host: self.plan.target_host,
                generation: self.plan.source.generation + 1,
                scope: self.plan.target_scope.clone(),
                native: stage.native.clone(),
            },
            _ => self.plan.source.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Data {
    host: Uuid,
    moves: BTreeMap<Uuid, Record>,
}

pub(crate) struct Ledger {
    path: PathBuf,
    data: Data,
    failed: bool,
}

impl Ledger {
    /// The session host's startup lock gives its actor sole writer access.
    pub(crate) fn open(directory: &Path, host: Uuid) -> io::Result<Self> {
        let path = directory.join("session-moves-v1.json");
        let data: Data = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Data {
                host,
                moves: BTreeMap::new(),
            },
            Err(err) => return Err(err),
        };
        if data.host != host {
            return Err(io::Error::other(
                "The session move ledger belongs to another host.",
            ));
        }
        for (id, record) in &data.moves {
            validate(&record.plan)?;
            let expected = match record.phase {
                Phase::SourceQueued { .. }
                | Phase::SourcePreparing { .. }
                | Phase::SourceFenced { .. }
                | Phase::SourceCommitted { .. }
                | Phase::SourceCancelled { .. } => record.plan.source.host,
                _ => record.plan.target_host,
            };
            if *id != record.plan.id
                || expected != host
                || record
                    .phase
                    .stage()
                    .is_some_and(|stage| !valid_native(&stage.native))
            {
                return Err(io::Error::other(
                    "Invalid binding in the session move ledger.",
                ));
            }
        }
        Ok(Self {
            path,
            data,
            failed: false,
        })
    }

    pub(crate) fn status(&self, id: Uuid) -> io::Result<Option<Status>> {
        self.healthy()?;
        Ok(self.data.moves.get(&id).map(|r| r.phase.status()))
    }

    /// Resume or reconcile these source operations after a host restart.
    pub(crate) fn pending_sources(&self) -> io::Result<Vec<(Plan, Status)>> {
        self.healthy()?;
        Ok(self
            .data
            .moves
            .values()
            .filter_map(|record| {
                let status = record.phase.status();
                matches!(
                    status,
                    Status::Queued
                        | Status::Preparing
                        | Status::Fenced
                        | Status::Released
                        | Status::Committed
                )
                .then(|| (record.plan.clone(), status))
            })
            .collect())
    }

    pub(crate) fn owner(&self, session: Uuid) -> io::Result<Option<Owner>> {
        self.healthy()?;
        Ok(self
            .data
            .moves
            .values()
            .filter(|r| {
                r.plan.session == session && !matches!(r.phase, Phase::TargetCancelled { .. })
            })
            .map(Record::owner)
            .max_by_key(|o| o.generation))
    }

    /// Recover a moved session's logical ID from its current native writer.
    pub(crate) fn source_identity(&self, scope: &Scope, native: &str) -> io::Result<(Uuid, Owner)> {
        self.healthy()?;
        if !self.can_start_turn(scope, native) {
            return Err(io::Error::other("This session already has an active move."));
        }
        for record in self
            .data
            .moves
            .values()
            .filter(|record| self.local_native(record, scope, native))
        {
            if let Some(owner) = self.owner(record.plan.session)?
                && owner.host == self.data.host
                && &owner.scope == scope
                && owner.native == native
            {
                return Ok((record.plan.session, owner));
            }
        }
        Ok((
            Uuid::new_v4(),
            Owner {
                host: self.data.host,
                generation: 0,
                scope: scope.clone(),
                native: native.to_owned(),
            },
        ))
    }

    /// Restore the destination's catalog from its durable ownership records.
    pub(crate) fn active_receivers(&self) -> io::Result<Vec<Prepared>> {
        self.healthy()?;
        let mut active = Vec::new();
        for record in self.data.moves.values() {
            if let Phase::TargetActive { stage } = &record.phase
                && self.owner(record.plan.session)?.as_ref() == Some(&record.owner())
            {
                active.push(Prepared {
                    plan: record.plan.clone(),
                    stage: stage.clone(),
                });
            }
        }
        Ok(active)
    }

    pub(crate) fn unpublished_writers<'a>(
        &'a self,
        scope: &'a Scope,
    ) -> impl Iterator<Item = (Uuid, &'a str)> + 'a {
        self.data.moves.values().filter_map(move |record| {
            if &record.plan.target_scope != scope {
                return None;
            }
            match &record.phase {
                Phase::TargetPrepared { stage } | Phase::TargetCancelled { stage: Some(stage) } => {
                    Some((record.plan.id, stage.native.as_str()))
                }
                _ => None,
            }
        })
    }

    /// Check before any route can open a native writer or execute session work.
    /// A temporary loss of disk durability must never open a second writer.
    pub(crate) fn can_write(&self, scope: &Scope, native: &str) -> bool {
        !self.failed
            && !self.data.moves.values().any(|record| {
                let plan = &record.plan;
                if plan.source.host == self.data.host {
                    return &plan.source.scope == scope
                        && plan.source.native == native
                        && !matches!(
                            record.phase,
                            Phase::SourceCancelled { .. } | Phase::SourceQueued { .. }
                        );
                }
                &plan.target_scope == scope
                    && record
                        .phase
                        .stage()
                        .is_some_and(|stage| stage.native == native)
                    && !matches!(record.phase, Phase::TargetActive { .. })
            })
    }

    /// The actor checks ownership of the native thread before the first move.
    /// Preparing blocks new writes while the actor snapshots the idle source.
    pub(crate) fn prepare_source(&mut self, plan: &Plan) -> io::Result<Status> {
        self.record_source(plan, Phase::SourcePreparing { archive: None })
    }

    /// Queuing keeps the current turn writable and reserves its next turn.
    pub(crate) fn queue_source(&mut self, plan: &Plan, turn: &str) -> io::Result<Status> {
        if !valid_native(turn) {
            return Err(io::Error::other("The current turn is not known."));
        }
        if let Some(record) = self.record(plan)?
            && let Phase::SourceQueued { turn: existing, .. } = &record.phase
            && existing != turn
        {
            return Err(io::Error::other(
                "The move is already waiting for a different turn.",
            ));
        }
        self.record_source(
            plan,
            Phase::SourceQueued {
                turn: turn.into(),
                finished: false,
            },
        )
    }

    pub(crate) fn queued_turn(&self, plan: &Plan) -> io::Result<Option<&str>> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        Ok(match self.record(plan)?.map(|record| &record.phase) {
            Some(Phase::SourceQueued { turn, .. }) => Some(turn.as_str()),
            _ => None,
        })
    }

    pub(crate) fn can_start_turn(&self, scope: &Scope, native: &str) -> bool {
        self.can_write(scope, native)
            && !self.data.moves.values().any(|record| {
                matches!(record.phase, Phase::SourceQueued { .. })
                    && self.local_native(record, scope, native)
            })
    }

    /// Remember the terminal event even if a worker notice or native goal
    /// continuation later updates the live row's most recent result.
    pub(crate) fn observe_queued_result(
        &mut self,
        plan: &Plan,
        completed_turn: &str,
    ) -> io::Result<()> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        if let Phase::SourceQueued {
            turn,
            finished: false,
        } = self.require(plan)?
            && turn == completed_turn
        {
            self.set(
                plan,
                Phase::SourceQueued {
                    turn: turn.clone(),
                    finished: true,
                },
            )?;
        }
        Ok(())
    }

    pub(crate) fn queued_result_seen(&self, plan: &Plan) -> io::Result<bool> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        Ok(matches!(
            self.require(plan)?,
            Phase::SourceQueued { finished: true, .. }
        ))
    }

    /// The actor also ensures that the runtime and its worker graph are idle.
    pub(crate) fn start_queued(&mut self, plan: &Plan) -> io::Result<()> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        match self.require(plan)? {
            Phase::SourceQueued { finished: true, .. } => {
                self.set(plan, Phase::SourcePreparing { archive: None })
            }
            Phase::SourcePreparing { .. } => Ok(()),
            _ => Err(io::Error::other(
                "The move is still waiting for its requested turn.",
            )),
        }
    }

    /// The signed destination preparation must match the files frozen here.
    pub(crate) fn bind_source_manifest(
        &mut self,
        plan: &Plan,
        archive: ArchivePin,
    ) -> io::Result<()> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        match self.require(plan)? {
            Phase::SourcePreparing { archive: None } => self.set(
                plan,
                Phase::SourcePreparing {
                    archive: Some(archive),
                },
            ),
            Phase::SourcePreparing {
                archive: Some(saved),
            } if *saved == archive => Ok(()),
            _ => Err(io::Error::other(
                "This move is already bound to a different transfer.",
            )),
        }
    }

    pub(crate) fn source_archive(&self, plan: &Plan) -> io::Result<Option<ArchivePin>> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        Ok(match self.require(plan)? {
            Phase::SourcePreparing { archive } => *archive,
            _ => None,
        })
    }

    pub(crate) fn source_status(&self, plan: &Plan) -> io::Result<Status> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        Ok(self.require(plan)?.status())
    }

    pub(crate) fn source_plan(&self, id: Uuid) -> io::Result<Option<Plan>> {
        self.healthy()?;
        Ok(self
            .data
            .moves
            .get(&id)
            .filter(|record| record.plan.source.host == self.data.host)
            .map(|record| record.plan.clone()))
    }

    pub(crate) fn source_move_at(
        &self,
        scope: &Scope,
        native: &str,
    ) -> io::Result<Option<(Plan, Status, Option<Stage>)>> {
        self.healthy()?;
        let found = self
            .data
            .moves
            .values()
            .filter(|record| {
                let source = &record.plan.source;
                source.host == self.data.host && &source.scope == scope && source.native == native
            })
            .max_by_key(|record| record.phase.status() != Status::Cancelled);
        Ok(found.map(|record| {
            (
                record.plan.clone(),
                record.phase.status(),
                record.phase.stage().cloned(),
            )
        }))
    }

    fn record_source(&mut self, plan: &Plan, phase: Phase) -> io::Result<Status> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        if let Some(record) = self.record(plan)? {
            return Ok(record.phase.status());
        }
        self.check_no_pending_move(plan)?;
        match self.owner(plan.session)? {
            Some(owner) if owner != plan.source => {
                return Err(io::Error::other(
                    "The move does not start at the last known owner.",
                ));
            }
            None if plan.source.generation != 0 => {
                return Err(io::Error::other(
                    "This host has no record of owning that session generation.",
                ));
            }
            _ => {}
        }
        if !self.can_write(&plan.source.scope, &plan.source.native)
            || self.data.moves.values().any(|record| {
                record.plan.session != plan.session
                    && self.local_native(record, &plan.source.scope, &plan.source.native)
            })
        {
            return Err(io::Error::other(
                "The source native session already belongs to a move.",
            ));
        }
        let status = phase.status();
        self.set(plan, phase)?;
        Ok(status)
    }

    /// The receiver verifies and durably stages the archive and idle destination
    /// before making this record. The ledger never runs or imports the runtime.
    pub(crate) fn prepare_receiver(
        &mut self,
        authenticated_source: Uuid,
        prepared: &Prepared,
    ) -> io::Result<Prepared> {
        let plan = &prepared.plan;
        self.check_hosts(plan, authenticated_source, self.data.host)?;
        if let Some(record) = self.record(plan)? {
            if matches!(
                record.phase,
                Phase::TargetPrepared { .. } | Phase::TargetActive { .. }
            ) && record.phase.stage() == Some(&prepared.stage)
            {
                return Ok(prepared.clone());
            }
            return Err(io::Error::other(
                "The destination move was cancelled or its staging changed.",
            ));
        }
        self.check_new_receiver(plan)?;
        if !valid_native(&prepared.stage.native)
            || self
                .data
                .moves
                .values()
                .any(|record| self.local_native(record, &plan.target_scope, &prepared.stage.native))
        {
            return Err(io::Error::other(
                "Choose a fresh destination native session.",
            ));
        }
        self.set(
            plan,
            Phase::TargetPrepared {
                stage: prepared.stage.clone(),
            },
        )?;
        Ok(prepared.clone())
    }

    /// Check this before allocating a destination runtime or another archive.
    /// A lost prepare reply must recover the same native session mapping.
    pub(crate) fn prepared_receiver(
        &self,
        authenticated_source: Uuid,
        plan: &Plan,
    ) -> io::Result<Option<Prepared>> {
        self.check_hosts(plan, authenticated_source, self.data.host)?;
        match self.record(plan)?.map(|record| &record.phase) {
            Some(Phase::TargetPrepared { stage } | Phase::TargetActive { stage }) => {
                Ok(Some(Prepared {
                    plan: plan.clone(),
                    stage: stage.clone(),
                }))
            }
            Some(_) => Err(io::Error::other("The destination move was cancelled.")),
            None => {
                self.check_new_receiver(plan)?;
                Ok(None)
            }
        }
    }

    /// After checking the destination receipt, fence the source before asking
    /// the native runtime to release its writer. This is the last cancel point.
    pub(crate) fn fence_source(
        &mut self,
        authenticated_target: Uuid,
        prepared: &Prepared,
        writer: Option<WriterGroup>,
    ) -> io::Result<Status> {
        let plan = &prepared.plan;
        self.check_hosts(plan, self.data.host, authenticated_target)?;
        if !valid_native(&prepared.stage.native) {
            return Err(io::Error::other("Invalid destination native session."));
        }
        match self.require(plan)? {
            Phase::SourcePreparing {
                archive: Some(saved),
            } if saved.manifest == prepared.stage.digest => {
                self.set(
                    plan,
                    Phase::SourceFenced {
                        stage: prepared.stage.clone(),
                        released: false,
                        writer,
                    },
                )?;
                Ok(Status::Fenced)
            }
            phase @ (Phase::SourceFenced { stage, .. } | Phase::SourceCommitted { stage, .. })
                if stage == &prepared.stage =>
            {
                Ok(phase.status())
            }
            _ => Err(io::Error::other(
                "The source cannot accept a different or cancelled staging receipt.",
            )),
        }
    }

    pub(crate) fn fenced_writer(&self, plan: &Plan) -> io::Result<Option<&WriterGroup>> {
        Ok(match self.require(plan)? {
            Phase::SourceFenced {
                writer,
                released: false,
                ..
            } => writer.as_ref(),
            _ => None,
        })
    }

    /// Record a confirmed runtime release or the exit of the fenced process group.
    pub(crate) fn writer_released(&mut self, plan: &Plan) -> io::Result<()> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        match self.require(plan)? {
            Phase::SourceFenced {
                stage,
                released: false,
                ..
            } => self.set(
                plan,
                Phase::SourceFenced {
                    stage: stage.clone(),
                    released: true,
                    writer: None,
                },
            ),
            Phase::SourceFenced { released: true, .. } | Phase::SourceCommitted { .. } => Ok(()),
            _ => Err(io::Error::other(
                "Fence the source before releasing its writer.",
            )),
        }
    }

    /// Commit only a native release from the exact fenced thread and route.
    /// After a restart, a durable Released record can retry commit directly.
    pub(crate) fn accept_writer_release(
        &mut self,
        scope: &Scope,
        release: MoveWriterRelease,
    ) -> io::Result<Option<Commit>> {
        let Some(plan) = self
            .pending_sources()?
            .into_iter()
            .find_map(|(plan, phase)| {
                (phase == Status::Fenced
                    && plan.source.scope == *scope
                    && plan.source.native == release.thread)
                    .then_some(plan)
            })
        else {
            return Ok(None);
        };
        release.result.map_err(io::Error::other)?;
        self.writer_released(&plan)?;
        self.commit_source(&plan).map(Some)
    }

    pub(crate) fn commit_source(&mut self, plan: &Plan) -> io::Result<Commit> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        let stage = match self.require(plan)? {
            Phase::SourceFenced {
                stage,
                released: true,
                ..
            } => {
                let stage = stage.clone();
                self.set(
                    plan,
                    Phase::SourceCommitted {
                        stage: stage.clone(),
                        confirmed: false,
                    },
                )?;
                stage
            }
            Phase::SourceCommitted { stage, .. } => stage.clone(),
            _ => {
                return Err(io::Error::other(
                    "Wait for the source's native writer to close.",
                ));
            }
        };
        Ok(Commit {
            prepared: Prepared {
                plan: plan.clone(),
                stage,
            },
        })
    }

    pub(crate) fn activate_receiver(
        &mut self,
        authenticated_source: Uuid,
        commit: &Commit,
    ) -> io::Result<Activated> {
        let Prepared { plan, stage } = &commit.prepared;
        self.check_hosts(plan, authenticated_source, self.data.host)?;
        match self.require(plan)? {
            Phase::TargetPrepared { stage: saved } if stage == saved => {
                self.set(
                    plan,
                    Phase::TargetActive {
                        stage: stage.clone(),
                    },
                )?;
            }
            Phase::TargetActive { stage: saved } if stage == saved => {}
            _ => {
                return Err(io::Error::other(
                    "The commit does not match the prepared destination.",
                ));
            }
        }
        Ok(Activated {
            commit: commit.clone(),
        })
    }

    /// The UI can report that the destination is ready after this acknowledgment.
    pub(crate) fn confirm_source(
        &mut self,
        authenticated_target: Uuid,
        activated: &Activated,
    ) -> io::Result<()> {
        let Prepared { plan, stage } = &activated.commit.prepared;
        self.check_hosts(plan, self.data.host, authenticated_target)?;
        match self.require(plan)? {
            Phase::SourceCommitted {
                stage: saved,
                confirmed,
            } if saved == stage => {
                if !confirmed {
                    self.set(
                        plan,
                        Phase::SourceCommitted {
                            stage: stage.clone(),
                            confirmed: true,
                        },
                    )?;
                }
                Ok(())
            }
            _ => Err(io::Error::other(
                "The activation receipt does not match the committed move.",
            )),
        }
    }

    pub(crate) fn cancel_source(&mut self, plan: &Plan) -> io::Result<Cancelled> {
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        match self.require(plan)? {
            Phase::SourceQueued { .. } | Phase::SourcePreparing { .. } => {
                self.set(plan, Phase::SourceCancelled { delivered: false })?
            }
            Phase::SourceCancelled { .. } => {}
            _ => return Err(io::Error::other("The move has already fenced the source.")),
        }
        Ok(Cancelled { plan: plan.clone() })
    }

    pub(crate) fn pending_cancellations(&self) -> io::Result<Vec<Cancelled>> {
        self.healthy()?;
        Ok(self
            .data
            .moves
            .values()
            .filter(|record| matches!(record.phase, Phase::SourceCancelled { delivered: false }))
            .map(|record| Cancelled {
                plan: record.plan.clone(),
            })
            .collect())
    }

    pub(crate) fn confirm_cancellation(&mut self, cancelled: &Cancelled) -> io::Result<()> {
        let plan = &cancelled.plan;
        self.check_hosts(plan, self.data.host, plan.target_host)?;
        match self.require(plan)? {
            Phase::SourceCancelled { delivered: false } => {
                self.set(plan, Phase::SourceCancelled { delivered: true })
            }
            Phase::SourceCancelled { delivered: true } => Ok(()),
            _ => Err(io::Error::other("This move was not cancelled.")),
        }
    }

    pub(crate) fn cancel_receiver(
        &mut self,
        authenticated_source: Uuid,
        cancelled: &Cancelled,
    ) -> io::Result<()> {
        let plan = &cancelled.plan;
        self.check_hosts(plan, authenticated_source, self.data.host)?;
        let stage = match self.record(plan)?.map(|r| &r.phase) {
            Some(Phase::TargetPrepared { stage }) => Some(stage.clone()),
            Some(Phase::TargetCancelled { .. }) => return Ok(()),
            None => None,
            _ => {
                return Err(io::Error::other(
                    "The destination has already accepted the move.",
                ));
            }
        };
        self.set(plan, Phase::TargetCancelled { stage })
    }

    fn check_new_receiver(&self, plan: &Plan) -> io::Result<()> {
        self.check_no_pending_move(plan)?;
        let Some(known) = self.owner(plan.session)? else {
            return Ok(());
        };
        if known == plan.source {
            return Ok(());
        }
        // The authenticated sender can be ahead of our last known owner
        // after an onward move. Reconcile only when our own earlier transfer
        // already closed the local writer. Receiving still needs a new
        // native mapping and an authenticated commit for this exact plan.
        if plan.source.generation > known.generation
            && self.data.moves.values().any(|record| {
                record.plan.session == plan.session
                    && record.plan.source.host == self.data.host
                    && matches!(record.phase, Phase::SourceCommitted { .. })
                    && record.owner() == known
            })
        {
            return Ok(());
        }
        Err(io::Error::other(
            "The move does not start at the last known owner.",
        ))
    }

    fn check_no_pending_move(&self, plan: &Plan) -> io::Result<()> {
        if self
            .data
            .moves
            .values()
            .any(|r| r.plan.session == plan.session && r.phase.pending())
        {
            return Err(io::Error::other("This session already has a pending move."));
        }
        Ok(())
    }

    fn local_native(&self, record: &Record, scope: &Scope, native: &str) -> bool {
        let plan = &record.plan;
        if plan.source.host == self.data.host {
            &plan.source.scope == scope && plan.source.native == native
        } else {
            &plan.target_scope == scope && record.phase.stage().is_some_and(|s| s.native == native)
        }
    }

    fn check_hosts(&self, plan: &Plan, source: Uuid, target: Uuid) -> io::Result<()> {
        self.healthy()?;
        validate(plan)?;
        if plan.source.host != source || plan.target_host != target {
            return Err(io::Error::other(
                "The move does not bind these authenticated hosts.",
            ));
        }
        Ok(())
    }

    fn record(&self, plan: &Plan) -> io::Result<Option<&Record>> {
        match self.data.moves.get(&plan.id) {
            Some(r) if &r.plan != plan => {
                Err(io::Error::other("This move ID belongs to another binding."))
            }
            other => Ok(other),
        }
    }

    fn require(&self, plan: &Plan) -> io::Result<&Phase> {
        self.record(plan)?
            .map(|r| &r.phase)
            .ok_or_else(|| io::Error::other("The move has not been staged on this host."))
    }

    fn healthy(&self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "Restart the host to recover the session move ledger.",
            ));
        }
        Ok(())
    }

    fn set(&mut self, plan: &Plan, phase: Phase) -> io::Result<()> {
        let mut next = self.data.clone();
        next.moves.insert(
            plan.id,
            Record {
                plan: plan.clone(),
                phase,
            },
        );
        let save = || -> io::Result<()> {
            let dir = self.path.parent().expect("ledger directory");
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer(&mut file, &next)?;
            file.as_file().sync_all()?;
            file.persist(&self.path)?;
            File::open(dir)?.sync_all()
        };
        if let Err(error) = save() {
            // The rename may have happened before the directory sync failed.
            // No transition or write authority may rely on the in-memory copy.
            self.failed = true;
            return Err(error);
        }
        self.data = next;
        Ok(())
    }
}

fn validate(plan: &Plan) -> io::Result<()> {
    if plan.id.is_nil()
        || plan.session.is_nil()
        || plan.source.host.is_nil()
        || plan.target_host.is_nil()
        || plan.source.host == plan.target_host
        || plan.source.generation == u64::MAX
        || !valid_native(&plan.source.native)
        || !plan.source.scope.project.is_absolute()
        || !plan.target_scope.project.is_absolute()
        || plan.source.scope.route != plan.target_scope.route
    {
        return Err(io::Error::other("Invalid session move binding."));
    }
    Ok(())
}

fn valid_native(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::runtime::Route;

    fn scope(project: &str) -> Scope {
        Scope {
            project: project.into(),
            route: Route::CODEX_SUBSCRIPTION,
        }
    }

    fn plan(source: Uuid, target: Uuid) -> Plan {
        Plan {
            id: Uuid::new_v4(),
            session: Uuid::new_v4(),
            source: Owner {
                host: source,
                generation: 0,
                scope: scope("/laptop/ox"),
                native: "local".into(),
            },
            target_host: target,
            target_scope: scope("/studio/ox"),
        }
    }

    fn prepare(plan: &Plan, native: &str) -> Prepared {
        Prepared {
            plan: plan.clone(),
            stage: Stage {
                digest: [9; 32],
                native: native.into(),
            },
        }
    }

    fn archive(prepared: &Prepared) -> ArchivePin {
        ArchivePin {
            manifest: prepared.stage.digest,
            transcript: [8; 32],
        }
    }

    #[test]
    fn a_native_release_cannot_commit_an_unrelated_or_failed_move() {
        let laptop = tempfile::tempdir().unwrap();
        let plan = plan(Uuid::new_v4(), Uuid::new_v4());
        let mut source = Ledger::open(laptop.path(), plan.source.host).unwrap();
        source.prepare_source(&plan).unwrap();
        let prepared = prepare(&plan, "studio");
        source
            .bind_source_manifest(&plan, archive(&prepared))
            .unwrap();
        source
            .fence_source(plan.target_host, &prepared, None)
            .unwrap();

        let release = |thread: &str, result| MoveWriterRelease {
            thread: thread.into(),
            result,
        };
        assert!(
            source
                .accept_writer_release(&plan.source.scope, release("unrelated", Ok(())))
                .unwrap()
                .is_none()
        );
        assert!(
            source
                .accept_writer_release(&plan.target_scope, release("local", Ok(())))
                .unwrap()
                .is_none()
        );
        assert!(
            source
                .accept_writer_release(
                    &plan.source.scope,
                    release("local", Err("Native writer is still open".into()))
                )
                .is_err()
        );
        assert_eq!(source.status(plan.id).unwrap(), Some(Status::Fenced));
        let committed = source
            .accept_writer_release(&plan.source.scope, release("local", Ok(())))
            .unwrap()
            .unwrap();
        assert_eq!(committed.prepared.plan, plan);
        assert_eq!(source.status(plan.id).unwrap(), Some(Status::Committed));
        assert!(
            source
                .accept_writer_release(&plan.source.scope, release("local", Ok(())))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_queued_move_reserves_the_next_turn_and_waits_for_the_exact_completion() {
        let laptop = tempfile::tempdir().unwrap();
        let plan = plan(Uuid::new_v4(), Uuid::new_v4());
        let source = || Ledger::open(laptop.path(), plan.source.host).unwrap();
        assert_eq!(
            source().queue_source(&plan, "current").unwrap(),
            Status::Queued
        );
        assert_eq!(source().queued_turn(&plan).unwrap(), Some("current"));
        assert!(source().can_write(&plan.source.scope, "local"));
        assert!(!source().can_start_turn(&plan.source.scope, "local"));
        assert!(source().can_start_turn(&plan.source.scope, "other"));
        assert!(source().queue_source(&plan, "different").is_err());
        source().observe_queued_result(&plan, "older").unwrap();
        assert!(source().start_queued(&plan).is_err());
        assert_eq!(
            source().pending_sources().unwrap(),
            [(plan.clone(), Status::Queued)]
        );
        source().observe_queued_result(&plan, "current").unwrap();
        source()
            .observe_queued_result(&plan, "worker-notice")
            .unwrap();
        assert!(source().queued_result_seen(&plan).unwrap());
        source().start_queued(&plan).unwrap();
        assert_eq!(source().status(plan.id).unwrap(), Some(Status::Preparing));
        assert!(!source().can_write(&plan.source.scope, "local"));
        assert_eq!(source().queued_turn(&plan).unwrap(), None);
        source().cancel_source(&plan).unwrap();
        assert!(source().can_start_turn(&plan.source.scope, "local"));
    }

    #[test]
    fn restarts_and_lost_acknowledgements_never_make_both_hosts_writable() {
        let laptop = tempfile::tempdir().unwrap();
        let studio = tempfile::tempdir().unwrap();
        let plan = plan(Uuid::new_v4(), Uuid::new_v4());
        let source = || Ledger::open(laptop.path(), plan.source.host).unwrap();
        let target = || Ledger::open(studio.path(), plan.target_host).unwrap();
        let prepared = prepare(&plan, "studio-native");

        assert!(source().can_write(&plan.source.scope, "local"));
        assert_eq!(source().prepare_source(&plan).unwrap(), Status::Preparing);
        assert!(!source().can_write(&plan.source.scope, "local"));
        assert!(source().writer_released(&plan).is_err());
        assert!(source().commit_source(&plan).is_err());
        assert_eq!(
            target().prepared_receiver(plan.source.host, &plan).unwrap(),
            None
        );
        target()
            .prepare_receiver(plan.source.host, &prepared)
            .unwrap();
        assert_eq!(
            target().prepared_receiver(plan.source.host, &plan).unwrap(),
            Some(prepared.clone())
        );
        assert!(target().prepared_receiver(Uuid::new_v4(), &plan).is_err());
        assert_eq!(target().status(plan.id).unwrap(), Some(Status::Prepared));
        assert!(!target().can_write(&plan.target_scope, "studio-native"));
        assert!(
            source()
                .fence_source(plan.target_host, &prepared, None)
                .is_err()
        );
        source()
            .bind_source_manifest(&plan, archive(&prepared))
            .unwrap();
        let mut different = prepared.clone();
        different.stage.digest[0] ^= 1;
        assert!(
            source()
                .fence_source(plan.target_host, &different, None)
                .is_err()
        );
        assert!(
            source()
                .fence_source(Uuid::new_v4(), &prepared, None)
                .is_err()
        );
        assert_eq!(
            source()
                .fence_source(plan.target_host, &prepared, None)
                .unwrap(),
            Status::Fenced
        );
        assert!(source().cancel_source(&plan).is_err());
        assert!(source().commit_source(&plan).is_err());

        source().writer_released(&plan).unwrap();
        assert_eq!(source().status(plan.id).unwrap(), Some(Status::Released));
        let grant = source().commit_source(&plan).unwrap();
        assert_eq!(source().status(plan.id).unwrap(), Some(Status::Committed));
        assert_eq!(
            source().pending_sources().unwrap(),
            [(plan.clone(), Status::Committed)]
        );
        assert_eq!(source().commit_source(&plan).unwrap(), grant);
        assert!(!source().can_write(&plan.source.scope, "local"));
        assert!(!target().can_write(&plan.target_scope, "studio-native"));
        assert!(target().activate_receiver(Uuid::new_v4(), &grant).is_err());
        let mut changed = grant.clone();
        changed.prepared.stage.digest[0] ^= 1;
        assert!(
            target()
                .activate_receiver(plan.source.host, &changed)
                .is_err()
        );
        let activated = target()
            .activate_receiver(plan.source.host, &grant)
            .unwrap();
        assert!(target().can_write(&plan.target_scope, "studio-native"));
        assert!(!source().can_write(&plan.source.scope, "local"));
        assert_eq!(
            target()
                .activate_receiver(plan.source.host, &grant)
                .unwrap(),
            activated
        );
        assert_eq!(
            target()
                .prepare_receiver(plan.source.host, &prepared)
                .unwrap(),
            prepared
        );
        assert!(source().confirm_source(Uuid::new_v4(), &activated).is_err());
        source()
            .confirm_source(plan.target_host, &activated)
            .unwrap();
        source()
            .confirm_source(plan.target_host, &activated)
            .unwrap();
        assert_eq!(source().status(plan.id).unwrap(), Some(Status::Confirmed));
        assert!(source().pending_sources().unwrap().is_empty());
        assert_eq!(
            source().owner(plan.session).unwrap(),
            target().owner(plan.session).unwrap()
        );
    }

    #[test]
    fn a_session_can_circle_back_without_reopening_any_previous_writer() {
        let laptop = tempfile::tempdir().unwrap();
        let studio = tempfile::tempdir().unwrap();
        let office = tempfile::tempdir().unwrap();
        let first = plan(Uuid::new_v4(), Uuid::new_v4());
        let mut a = Ledger::open(laptop.path(), first.source.host).unwrap();
        let mut b = Ledger::open(studio.path(), first.target_host).unwrap();
        let office_id = Uuid::new_v4();
        let mut c = Ledger::open(office.path(), office_id).unwrap();
        let prepared = prepare(&first, "studio-native");
        a.prepare_source(&first).unwrap();
        a.bind_source_manifest(&first, archive(&prepared)).unwrap();
        b.prepare_receiver(first.source.host, &prepared).unwrap();
        a.fence_source(first.target_host, &prepared, None).unwrap();
        a.writer_released(&first).unwrap();
        let original = a.commit_source(&first).unwrap();
        b.activate_receiver(first.source.host, &original).unwrap();

        let (session, source) = b
            .source_identity(&first.target_scope, "studio-native")
            .unwrap();
        assert_eq!(session, first.session);
        let onward = Plan {
            id: Uuid::new_v4(),
            session,
            source,
            target_host: office_id,
            target_scope: scope("/office/ox"),
        };
        let prepared = prepare(&onward, "office-native");
        b.prepare_source(&onward).unwrap();
        b.bind_source_manifest(&onward, archive(&prepared)).unwrap();
        c.prepare_receiver(onward.source.host, &prepared).unwrap();
        b.fence_source(onward.target_host, &prepared, None).unwrap();
        b.writer_released(&onward).unwrap();
        let grant = b.commit_source(&onward).unwrap();
        c.activate_receiver(onward.source.host, &grant).unwrap();

        let (session, source) = c
            .source_identity(&onward.target_scope, "office-native")
            .unwrap();
        let back = Plan {
            id: Uuid::new_v4(),
            session,
            source,
            target_host: first.source.host,
            target_scope: first.source.scope.clone(),
        };
        let prepared = prepare(&back, "laptop-native-2");
        assert_eq!(a.owner(session).unwrap().unwrap().generation, 1);
        assert_eq!(back.source.generation, 2);
        assert!(
            a.prepared_receiver(back.source.host, &back)
                .unwrap()
                .is_none()
        );
        let stale = Plan {
            source: Owner {
                generation: 1,
                ..back.source.clone()
            },
            ..back.clone()
        };
        assert!(a.prepared_receiver(stale.source.host, &stale).is_err());
        assert!(a.prepared_receiver(onward.source.host, &back).is_err());
        c.prepare_source(&back).unwrap();
        c.bind_source_manifest(&back, archive(&prepared)).unwrap();
        assert!(
            a.prepare_receiver(back.source.host, &prepare(&back, "local"))
                .is_err()
        );
        a.prepare_receiver(back.source.host, &prepared).unwrap();
        assert!(!a.can_write(&back.target_scope, "laptop-native-2"));
        c.fence_source(back.target_host, &prepared, None).unwrap();
        c.writer_released(&back).unwrap();
        let grant = c.commit_source(&back).unwrap();
        a.activate_receiver(back.source.host, &grant).unwrap();
        b.activate_receiver(first.source.host, &original).unwrap();
        assert_eq!(a.owner(first.session).unwrap().unwrap().generation, 3);
        assert!(a.can_write(&first.source.scope, "laptop-native-2"));
        assert!(!a.can_write(&first.source.scope, "local"));
        assert!(!b.can_write(&first.target_scope, "studio-native"));
        assert!(!c.can_write(&onward.target_scope, "office-native"));
        let jumps_a_live_owner = Plan {
            id: Uuid::new_v4(),
            source: Owner {
                generation: 4,
                ..back.source.clone()
            },
            ..back
        };
        assert!(
            a.prepared_receiver(jumps_a_live_owner.source.host, &jumps_a_live_owner)
                .is_err()
        );
        assert!(
            b.source_identity(&first.target_scope, "studio-native")
                .is_err()
        );
        assert!(
            a.prepare_source(&Plan {
                id: Uuid::new_v4(),
                ..first
            })
            .is_err()
        );
    }

    #[test]
    fn cancellation_keeps_the_source_writable_and_cannot_be_replayed_into_activation() {
        let laptop = tempfile::tempdir().unwrap();
        let studio = tempfile::tempdir().unwrap();
        let plan = plan(Uuid::new_v4(), Uuid::new_v4());
        let mut source = Ledger::open(laptop.path(), plan.source.host).unwrap();
        let mut target = Ledger::open(studio.path(), plan.target_host).unwrap();
        let prepared = prepare(&plan, "stage");
        source.prepare_source(&plan).unwrap();
        target
            .prepare_receiver(plan.source.host, &prepared)
            .unwrap();
        assert!(
            source
                .prepare_source(&Plan {
                    target_scope: scope("/wrong/ox"),
                    ..plan.clone()
                })
                .is_err()
        );
        assert!(
            source
                .prepare_source(&Plan {
                    id: Uuid::new_v4(),
                    ..plan.clone()
                })
                .is_err()
        );
        let cancelled = source.cancel_source(&plan).unwrap();
        assert_eq!(source.cancel_source(&plan).unwrap(), cancelled);
        let mut source = Ledger::open(laptop.path(), plan.source.host).unwrap();
        assert_eq!(
            source.pending_cancellations().unwrap().as_slice(),
            std::slice::from_ref(&cancelled)
        );
        assert!(source.can_write(&plan.source.scope, "local"));
        assert!(
            source
                .fence_source(plan.target_host, &prepared, None)
                .is_err()
        );
        target
            .cancel_receiver(plan.source.host, &cancelled)
            .unwrap();
        source.confirm_cancellation(&cancelled).unwrap();
        source.confirm_cancellation(&cancelled).unwrap();
        let source_after_delivery = Ledger::open(laptop.path(), plan.source.host).unwrap();
        assert!(
            source_after_delivery
                .pending_cancellations()
                .unwrap()
                .is_empty()
        );
        assert_eq!(target.status(plan.id).unwrap(), Some(Status::Cancelled));
        assert!(!target.can_write(&plan.target_scope, "stage"));
        assert!(
            target
                .prepare_receiver(plan.source.host, &prepared)
                .is_err()
        );
        assert!(
            target
                .activate_receiver(plan.source.host, &Commit { prepared })
                .is_err()
        );

        let second = Plan {
            id: Uuid::new_v4(),
            ..plan
        };
        source.prepare_source(&second).unwrap();
        let cancelled = source.cancel_source(&second).unwrap();
        target
            .cancel_receiver(second.source.host, &cancelled)
            .unwrap();
        assert!(
            target
                .prepare_receiver(second.source.host, &prepare(&second, "new-stage"))
                .is_err()
        );
    }

    #[test]
    fn a_failed_write_fails_closed_and_does_not_create_a_receipt() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan(Uuid::new_v4(), Uuid::new_v4());
        let mut source = Ledger::open(root.path(), plan.source.host).unwrap();
        std::fs::create_dir(&source.path).unwrap();
        assert!(source.prepare_source(&plan).is_err());
        assert!(!source.can_write(&plan.source.scope, "local"));
        assert!(source.status(plan.id).is_err());
        std::fs::remove_dir(&source.path).unwrap();
        let mut recovered = Ledger::open(root.path(), plan.source.host).unwrap();
        assert_eq!(recovered.status(plan.id).unwrap(), None);
        assert!(recovered.can_write(&plan.source.scope, "local"));
        recovered.prepare_source(&plan).unwrap();
        assert!(Ledger::open(root.path(), plan.target_host).is_err());
    }
}
