//! Source-side move transitions. The transport supplies authenticated staging
//! receipts. The actor retains the next turn and releases the native writer.
use super::{Action, Connection, Engine, Scope};
use crate::{
    move_archive::Archive,
    move_ledger::{Activated, ArchivePin, Commit, Ledger, Plan, Prepared, Status},
    move_writer::WriterGroup,
};
use ox_desk_core::host_protocol::Timeline;
use std::{collections::HashMap, io, path::Path};
use uuid::Uuid;

pub(super) struct SourceMoves {
    pub(super) ledger: Ledger,
    /// The store generation which took responsibility for a native release.
    releases: HashMap<Uuid, ReleaseAttempt>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReleaseAttempt {
    Awaiting(u64),
    Refused(u64),
}

impl SourceMoves {
    pub(super) fn open(directory: &Path, host: Uuid) -> io::Result<Self> {
        Ok(Self {
            ledger: Ledger::open(directory, host)?,
            releases: HashMap::new(),
        })
    }

    pub(super) fn observe_native_results(
        &mut self,
        scope: &Scope,
        store: &ox_desk_core::store::Store,
    ) -> io::Result<()> {
        for (plan, status) in self.ledger.pending_sources()? {
            if status == Status::Queued
                && &plan.source.scope == scope
                && let Some(result) = store.last_result(&plan.source.native)
            {
                self.ledger.observe_queued_result(&plan, &result.turn)?;
            }
        }
        Ok(())
    }

    pub(super) fn retry_refused_release(&mut self, id: Uuid) {
        if matches!(self.releases.get(&id), Some(ReleaseAttempt::Refused(_))) {
            self.releases.remove(&id);
        }
    }
}

enum Effect<'a> {
    /// The operator can steer or finish work already running on the queued turn.
    CurrentTurn(Option<&'a str>),
    /// Opening a live thread for inspection can reuse its existing subscription.
    Open,
    Next,
}

impl Engine {
    pub(super) fn prepare_source_move(
        &mut self,
        plan: &Plan,
        after_current_turn: bool,
    ) -> io::Result<Status> {
        let scope = &plan.source.scope;
        let thread = &plan.source.native;
        if self.source_moves.ledger.status(plan.id)?.is_some() {
            return self.source_moves.ledger.prepare_source(plan);
        }
        if !after_current_turn {
            if !self.move_source_quiescent(plan) {
                return Err(io::Error::other(
                    "Wait for the source session and its workers to finish.",
                ));
            }
            return self.source_moves.ledger.prepare_source(plan);
        }
        let live = self
            .running
            .get(scope)
            .ok_or_else(|| io::Error::other("Open the source session first."))?;
        if live.store.state() != &Connection::Ready
            || live.store.sending(Some(thread))
            || live.store.in_terminal(thread)
            || live.store.is_native_live(thread)
        {
            return Err(io::Error::other(
                "Wait for the source session's current turn to be confirmed.",
            ));
        }
        let turn = live
            .store
            .transcript(thread)
            .filter(|t| t.is_ready())
            .and_then(|t| t.active_turn())
            .ok_or_else(|| io::Error::other("The source session has no confirmed running turn."))?;
        self.queue_move_for_turn(plan, turn.to_owned())
    }

    /// Network preflight can complete after the accepted turn ended. Keep the
    /// exact turn in that case and use its confirmed result, if it exists.
    pub(super) fn queue_move_for_turn(&mut self, plan: &Plan, turn: String) -> io::Result<Status> {
        let live = self
            .running
            .get(&plan.source.scope)
            .ok_or_else(|| io::Error::other("Open the source session first."))?;
        let thread = &plan.source.native;
        let active = live
            .store
            .transcript(thread)
            .and_then(|view| view.active_turn());
        let finished = live
            .store
            .last_result(thread)
            .is_some_and(|result| result.turn == turn);
        if active != Some(turn.as_str()) && !finished {
            return Err(io::Error::other(
                "The selected turn could not be confirmed. Check the session before moving it.",
            ));
        }
        let status = self.source_moves.ledger.queue_source(plan, &turn)?;
        if finished {
            self.source_moves
                .ledger
                .observe_queued_result(plan, &turn)?;
        }
        Ok(status)
    }

    pub(super) fn stage_source_move(
        &mut self,
        authenticated_target: Uuid,
        prepared: &Prepared,
    ) -> io::Result<Status> {
        let plan = &prepared.plan;
        let writer = if self.source_moves.ledger.status(plan.id)? == Some(Status::Preparing) {
            let fingerprint = Archive::fingerprint(plan, self.move_source_timeline(plan)?)?;
            if !self
                .source_moves
                .ledger
                .source_archive(plan)?
                .is_some_and(|archive| archive.transcript == fingerprint)
            {
                return Err(io::Error::other(
                    "The source history changed before the move was committed.",
                ));
            }
            let group = self.running[&plan.source.scope]
                .store
                .move_writer_group(&plan.source.native)
                .ok_or_else(|| {
                    io::Error::other("Could not identify the source session’s writer.")
                })?;
            Some(WriterGroup::record(group)?)
        } else {
            None
        };
        let phase =
            self.source_moves
                .ledger
                .fence_source(authenticated_target, prepared, writer)?;
        if phase == Status::Fenced {
            self.source_moves.retry_refused_release(plan.id);
            self.begin_move_writer_release(plan);
        }
        Ok(phase)
    }

    pub(super) fn move_source_timeline(&self, plan: &Plan) -> io::Result<&Timeline> {
        if self.source_moves.ledger.source_status(plan)? != Status::Preparing {
            return Err(io::Error::other("The session is not ready to move."));
        }
        if !self.move_source_quiescent(plan) {
            return Err(io::Error::other("The source session is still active."));
        }
        self.running[&plan.source.scope]
            .state
            .transcripts
            .get(&plan.source.native)
            .ok_or_else(|| io::Error::other("Open the source session's history before moving."))
    }

    pub(super) fn bind_move_manifest(
        &mut self,
        plan: &Plan,
        archive: ArchivePin,
    ) -> io::Result<()> {
        if Archive::fingerprint(plan, self.move_source_timeline(plan)?)? != archive.transcript {
            return Err(io::Error::other(
                "The source history changed while preparing the move.",
            ));
        }
        self.source_moves.ledger.bind_source_manifest(plan, archive)
    }

    pub(super) fn source_move_status(&self, id: Uuid) -> io::Result<Option<Status>> {
        self.source_moves.ledger.status(id)
    }

    pub(super) fn committed_move(&mut self, plan: &Plan) -> io::Result<Commit> {
        if !matches!(
            self.source_move_status(plan.id)?,
            Some(Status::Committed | Status::Confirmed)
        ) {
            return Err(io::Error::other(
                "Wait for the source's native writer to close.",
            ));
        }
        self.source_moves.ledger.commit_source(plan)
    }

    pub(super) fn confirm_move(
        &mut self,
        authenticated_target: Uuid,
        receipt: &Activated,
    ) -> io::Result<()> {
        self.source_moves
            .ledger
            .confirm_source(authenticated_target, receipt)
    }

    pub(super) fn hold_move_source_scope(&self, scope: &Scope) -> io::Result<bool> {
        Ok(self
            .source_moves
            .ledger
            .pending_sources()?
            .into_iter()
            .any(|(plan, status)| {
                matches!(status, Status::Queued | Status::Preparing | Status::Fenced)
                    && &plan.source.scope == scope
            }))
    }

    fn move_source_quiescent(&self, plan: &Plan) -> bool {
        let scope = &plan.source.scope;
        let thread = &plan.source.native;
        let Some(live) = self.running.get(scope) else {
            return false;
        };
        if live.store.state() != &Connection::Ready
            || !live
                .store
                .transcript(thread)
                .is_some_and(|t| t.is_ready() && !t.is_loading())
            || live.store.is_native_live(thread)
            || live.store.changing_options(thread)
            || self.thread_busy(scope, thread)
            || active_native_descendant(&live.store, thread)
            || self
                .workers
                .children_of(scope, thread)
                .any(|(scope, child)| {
                    self.running.contains_key(scope) && self.thread_busy(scope, child)
                })
            || self.running.values().any(|worker| {
                worker.launching.as_ref().is_some_and(|start| {
                    start.origin.parent.scope.as_ref() == Some(scope)
                        && start.origin.parent.session.as_deref() == Some(thread)
                })
            })
        {
            return false;
        }
        let parent = crate::workers::Resolved::Hosted {
            scope: scope.clone(),
            session: thread.clone(),
        };
        !self
            .ledger
            .data
            .forwarded
            .values()
            .any(|job| job.owned(&parent) && job.active())
            && !self
                .ledger
                .data
                .worker_notices
                .pending
                .iter()
                .any(|notice| &notice.scope == scope && &notice.session == thread)
    }

    pub(super) fn advance_source_moves(&mut self) -> io::Result<()> {
        for (plan, phase) in self.source_moves.ledger.pending_sources()? {
            match phase {
                Status::Queued => {
                    let thread = &plan.source.native;
                    let scope = &plan.source.scope;
                    if let Some(result) = self.running.get(scope).and_then(|live| {
                        live.state
                            .sessions
                            .iter()
                            .find(|row| &row.id == thread)?
                            .last_result()
                    }) {
                        self.source_moves
                            .ledger
                            .observe_queued_result(&plan, &result.turn)?;
                    }
                    if !self.source_moves.ledger.queued_result_seen(&plan)? {
                        continue;
                    }
                    let history_ready = self
                        .running
                        .get(scope)
                        .and_then(|live| live.store.transcript(thread))
                        .is_some_and(|view| view.is_ready());
                    if !history_ready && !self.restore_source_move_history(&plan) {
                        continue;
                    }
                    if self.move_source_quiescent(&plan) {
                        self.source_moves.ledger.start_queued(&plan)?;
                    }
                }
                Status::Fenced => {
                    let awaiting_native_receipt =
                        self.running.get(&plan.source.scope).is_some_and(|live| {
                            live.store.state() == &Connection::Ready
                                && self.source_moves.releases.get(&plan.id)
                                    == Some(&ReleaseAttempt::Awaiting(live.generation))
                        });
                    if !awaiting_native_receipt
                        && self
                            .source_moves
                            .ledger
                            .fenced_writer(&plan)?
                            .is_some_and(WriterGroup::retired)
                    {
                        self.source_moves.ledger.writer_released(&plan)?;
                        self.source_moves.ledger.commit_source(&plan)?;
                        self.source_moves.releases.remove(&plan.id);
                        self.move_send.errors.remove(&plan.id);
                    } else {
                        self.begin_move_writer_release(&plan);
                    }
                }
                Status::Released => {
                    self.source_moves.ledger.commit_source(&plan)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn begin_move_writer_release(&mut self, plan: &Plan) {
        if !self.move_source_quiescent(plan) {
            return;
        }
        let live = self
            .running
            .get_mut(&plan.source.scope)
            .expect("checked above");
        let generation = live.generation;
        if self
            .source_moves
            .releases
            .get(&plan.id)
            .is_some_and(|attempt| {
                *attempt == ReleaseAttempt::Awaiting(generation)
                    || *attempt == ReleaseAttempt::Refused(generation)
            })
        {
            return;
        }
        let attempt = match live.store.release_writer_for_move(&plan.source.native) {
            Ok(()) => ReleaseAttempt::Awaiting(generation),
            Err(error) => {
                eprintln!("ox host: could not release move {}: {error}", plan.id);
                self.move_send.errors.insert(plan.id, error.to_string());
                ReleaseAttempt::Refused(generation)
            }
        };
        self.source_moves.releases.insert(plan.id, attempt);
    }

    pub(super) fn collect_source_move_releases(&mut self, scope: &Scope) -> io::Result<()> {
        let Some(live) = self.running.get_mut(scope) else {
            return Ok(());
        };
        let generation = live.generation;
        let releases = live.store.take_move_writer_releases();
        if releases.is_empty() {
            return Ok(());
        }
        let moves = self.source_moves.ledger.pending_sources()?;
        for release in releases {
            let Some((plan, _)) = moves.iter().find(|(plan, phase)| {
                *phase == Status::Fenced
                    && &plan.source.scope == scope
                    && plan.source.native == release.thread
                    && self.source_moves.releases.get(&plan.id)
                        == Some(&ReleaseAttempt::Awaiting(generation))
            }) else {
                continue;
            };
            if let Err(error) = &release.result {
                self.source_moves
                    .releases
                    .insert(plan.id, ReleaseAttempt::Refused(generation));
                eprintln!(
                    "ox host: native release for move {} was not confirmed: {error}",
                    plan.id
                );
                self.move_send.errors.insert(plan.id, error.clone());
                continue;
            }
            self.source_moves
                .ledger
                .accept_writer_release(scope, release)?;
            self.source_moves.releases.remove(&plan.id);
        }
        Ok(())
    }

    pub(super) fn check_source_move_action(
        &self,
        scope: &Scope,
        action: &Action,
    ) -> io::Result<()> {
        let (thread, effect) = match action {
            Action::SelectThread(thread) => (thread.as_str(), Effect::Open),
            Action::OpenCatalogRow(row) => (row.id.as_str(), Effect::Open),
            Action::ConfirmAliasAndResume(row) => (row.id.as_str(), Effect::Next),
            Action::SendText(Some(thread), _)
            | Action::SendImages(Some(thread), _, _)
            | Action::Submit(Some(thread))
            | Action::DraftEdit(Some(thread), _)
            | Action::Stop(thread) => (thread.as_str(), Effect::CurrentTurn(None)),
            Action::AnswerAsk(id, _) | Action::AnswerQuestions(id, _) => {
                let Some(ask) = self
                    .running
                    .get(scope)
                    .and_then(|live| live.state.asks.iter().find(|a| &a.id == id))
                else {
                    return Ok(());
                };
                let Some(thread) = ask.thread.as_deref() else {
                    return Ok(());
                };
                (thread, Effect::CurrentTurn(ask.turn.as_deref()))
            }
            Action::StartWorker(start) => (start.parent.as_str(), Effect::Next),
            Action::StartHandoff(start) => (start.source.as_str(), Effect::Next),
            Action::GenerateTitle(thread)
            | Action::RenameThread(thread, _)
            | Action::ArchiveThread(thread)
            | Action::ForkThread(thread)
            | Action::EndSession(thread)
            | Action::ForceEndSession(thread)
            | Action::SetAccess(thread, _)
            | Action::ResumeHere(thread)
            | Action::YoloGrant(thread, _)
            | Action::Terminal(Some(thread))
            | Action::TerminalText(Some(thread), _)
            | Action::ExecuteCommand(Some(thread))
            | Action::NativeCommand(Some(thread), _)
            | Action::PaletteCommand(Some(thread), _)
            | Action::ModelOptions(Some(thread), _) => (thread.as_str(), Effect::Next),
            _ => return Ok(()),
        };
        self.check_move_effect(scope, thread, effect)
    }

    pub(super) fn check_move_launch(&self, caller: &crate::workers::Caller) -> io::Result<()> {
        if let Ok(crate::workers::Resolved::Hosted { scope, session }) = self.resolve(caller) {
            self.check_move_effect(&scope, &session, Effect::CurrentTurn(None))?;
        }
        Ok(())
    }

    pub(super) fn can_deliver_move_worker_notice(
        &self,
        scope: &Scope,
        thread: &str,
    ) -> io::Result<bool> {
        let ledger = &self.source_moves.ledger;
        if !ledger.can_write(scope, thread)
            || self
                .running
                .get(scope)
                .is_some_and(|live| live.store.move_writer_is_unpublished(thread))
        {
            return Ok(false);
        }
        if ledger.can_start_turn(scope, thread) {
            return Ok(true);
        }
        for (plan, status) in ledger.pending_sources()? {
            if status == Status::Queued
                && &plan.source.scope == scope
                && plan.source.native == thread
            {
                // Settle already-owned children before copying the conversation.
                return ledger.queued_result_seen(&plan);
            }
        }
        Ok(false)
    }

    fn check_move_effect(&self, scope: &Scope, thread: &str, effect: Effect<'_>) -> io::Result<()> {
        let ledger = &self.source_moves.ledger;
        if !ledger.can_write(scope, thread)
            || self
                .running
                .get(scope)
                .is_some_and(|live| live.store.move_writer_is_unpublished(thread))
        {
            return Err(io::Error::other(
                "This session's writer belongs to a host move.",
            ));
        }
        if ledger.can_start_turn(scope, thread) {
            return Ok(());
        }
        let live = self.running.get(scope);
        let transcript = live.and_then(|live| live.store.transcript(thread));
        match effect {
            Effect::Open if transcript.is_some_and(|t| t.is_ready()) => return Ok(()),
            Effect::CurrentTurn(ask_turn) => {
                if let Some((plan, _)) =
                    ledger.pending_sources()?.into_iter().find(|(plan, phase)| {
                        *phase == Status::Queued
                            && &plan.source.scope == scope
                            && plan.source.native == thread
                    })
                    && let Some(active) = transcript.and_then(|t| t.active_turn())
                    && (ledger.queued_turn(&plan)? == Some(active)
                        || ledger.queued_result_seen(&plan)?)
                    && ask_turn.is_none_or(|turn| turn == active)
                {
                    return Ok(());
                }
            }
            _ => {}
        }
        Err(io::Error::other(
            "The next turn is reserved for the host move.",
        ))
    }
}

/// Native subagents are not in the OX worker registry. Follow loaded catalog
/// ancestry without treating a busy sibling on the same app-server as ours.
fn active_native_descendant(store: &ox_desk_core::store::Store, thread: &str) -> bool {
    let rows = store.threads();
    rows.iter().any(|row| {
        row.id != thread
            && store.thread_busy(&row.id)
            && std::iter::successors(row.parent_thread_id.as_deref(), |parent| {
                store.catalog_row(parent)?.parent_thread_id.as_deref()
            })
            .take(rows.len())
            .any(|parent| parent == thread)
    })
}
