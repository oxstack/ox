//! Bind a destination runtime checkpoint to the authenticated staged archive.
use super::{Action, Change, Engine, Scope, ScopeState};
use crate::{
    move_ledger::{Activated, Commit, Prepared},
    move_target::VerifiedTarget,
};
use ox_desk_core::{
    handoff::{HandoffSource, SourceItem},
    host_policy::HostPolicy,
    host_protocol::SessionView,
    modes::Level,
    store::{Connection, MoveWriterPreparation},
};
use std::{collections::HashMap, io, sync::mpsc::Sender};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct TargetMoves {
    warming: HashMap<Uuid, Warming>,
    pending: HashMap<Uuid, Pending>,
    displayed: HashMap<Scope, HashMap<String, MoveDisplay>>,
}

struct MoveDisplay {
    logical: serde_json::Value,
    title: String,
}

struct Warming {
    destination: VerifiedTarget,
    source: Uuid,
    reply: Sender<io::Result<Prepared>>,
}

struct Pending {
    destination: VerifiedTarget,
    source: Uuid,
    generation: u64,
    reply: Sender<io::Result<Prepared>>,
}

impl Engine {
    pub(super) fn view_reserved_claude_move(
        &mut self,
        scope: &Scope,
        thread: &str,
    ) -> io::Result<bool> {
        if scope.route.runtime != ox_desk_core::runtime::Runtime::Claude {
            return Ok(false);
        }
        let ledger = &self.source_moves.ledger;
        let Some(owned) = ledger.active_receivers()?.into_iter().find(|prepared| {
            &prepared.plan.target_scope == scope && prepared.stage.native == thread
        }) else {
            return Ok(false);
        };
        let operation = owned.plan.id.to_string();
        let live = self.running.get_mut(scope).expect("live scope");
        live.store
            .use_host_policy(HostPolicy::read(&self.state_dir)?);
        match live.store.prepare_claude_move_writer(&operation) {
            Ok(native) if native == thread => (),
            Ok(_) => return Err(io::Error::other("The reserved Claude writer changed.")),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error),
        }
        let row = VerifiedTarget::load(&self.state_dir, &owned.plan)?
            .catalog_row_for(ledger, scope, thread)?;
        let row = serde_json::from_value(row.detail["row"].clone())?;
        live.store.show_reserved_claude_move(&operation, row)?;
        Ok(true)
    }

    fn remember_moved_row(&mut self, scope: &Scope, row: &SessionView) {
        self.target_moves
            .displayed
            .entry(scope.clone())
            .or_default()
            .entry(row.id.clone())
            .or_insert_with(|| MoveDisplay {
                logical: row.detail["logical_session_id"].clone(),
                title: row.title.clone(),
            });
    }

    /// Load the destination's saved page into the same timeline as new turns.
    pub(super) fn load_moved_history(
        &mut self,
        scope: &Scope,
        thread: &str,
        older: bool,
    ) -> io::Result<bool> {
        let Some(live) = self.running.get(scope) else {
            return Ok(false);
        };
        let cursor = match (live.store.moved_history_cursor(thread), older) {
            (None, false) => None,
            (Some(Some(cursor)), true) => {
                if live
                    .store
                    .transcript(thread)
                    .is_some_and(|t| t.has_older() || t.is_loading())
                {
                    return Ok(false);
                }
                Some(cursor)
            }
            _ => return Ok(false),
        };
        let ledger = &self.source_moves.ledger;
        let Some(owned) = ledger.active_receivers()?.into_iter().find(|prepared| {
            &prepared.plan.target_scope == scope && prepared.stage.native == thread
        }) else {
            return Ok(false);
        };
        let (items, previous) = VerifiedTarget::load(&self.state_dir, &owned.plan)?
            .history_for(ledger, scope, thread)?
            .previous(cursor, 128)?;
        self.running
            .get_mut(scope)
            .expect("live scope")
            .store
            .show_moved_history(thread, items, previous);
        Ok(true)
    }

    pub(super) fn read_moved_history(
        &self,
        scope: &Scope,
        native: &str,
        host: Option<&str>,
        session: Option<&str>,
    ) -> io::Result<Option<(HandoffSource, Vec<SourceItem>)>> {
        let ledger = &self.source_moves.ledger;
        for prepared in ledger.active_receivers()? {
            let plan = &prepared.plan;
            if &plan.target_scope == scope
                && prepared.stage.native == native
                && host.is_none_or(|id| plan.source.host.to_string() == id)
                && session
                    .is_none_or(|id| id == plan.source.native || plan.session.to_string() == id)
            {
                return VerifiedTarget::load(&self.state_dir, plan)?
                    .moved_history_for(ledger, scope, native)
                    .map(Some);
            }
        }
        Ok(None)
    }

    pub(super) fn recover_move_targets(mut self) -> io::Result<Self> {
        for prepared in self.source_moves.ledger.active_receivers()? {
            let plan = &prepared.plan;
            let restored = VerifiedTarget::load(&self.state_dir, plan).and_then(|target| {
                target.catalog_row_for(
                    &self.source_moves.ledger,
                    &plan.target_scope,
                    &prepared.stage.native,
                )
            });
            let mut row = match restored {
                Ok(row) => row,
                Err(error) => {
                    eprintln!(
                        "ox host: cannot restore the moved session {}: {error}",
                        plan.session
                    );
                    continue;
                }
            };
            let scope = plan.target_scope.clone();
            let state = self
                .cold
                .entry(scope.clone())
                .or_insert_with(|| ScopeState {
                    scope: scope.clone(),
                    connection: "disconnected".into(),
                    capabilities: Vec::new(),
                    sessions: Vec::new(),
                    asks: Vec::new(),
                    transcripts: Default::default(),
                    detail: serde_json::json!({}),
                });
            if let Some(previous) = state
                .sessions
                .iter_mut()
                .find(|previous| previous.id == row.id)
            {
                previous.detail["logical_session_id"] = row.detail["logical_session_id"].clone();
                row.title.clone_from(&previous.title);
            } else {
                state.sessions.push(row.clone());
            }
            self.remember_moved_row(&scope, &row);
        }
        Ok(self)
    }

    pub(super) fn prepare_move_target(
        &mut self,
        source: Uuid,
        target: VerifiedTarget,
        reply: Sender<io::Result<Prepared>>,
    ) -> io::Result<()> {
        let plan = target.plan();
        if let Some(prepared) = self.source_moves.ledger.prepared_receiver(source, plan)? {
            if !target.matches_prepared(&prepared) {
                return Err(io::Error::other(
                    "The target reservation has a different archive.",
                ));
            }
            let _ = reply.send(Ok(prepared));
            return Ok(());
        }
        let scope = &plan.target_scope;
        if !self.running.contains_key(scope) {
            self.ensure(scope, None)?;
        }
        let live = self
            .running
            .get_mut(scope)
            .ok_or_else(|| io::Error::other("Open the selected target checkout."))?;
        if live.store.state() == &Connection::Connecting {
            if self
                .target_moves
                .warming
                .get(&plan.id)
                .is_some_and(|waiting| {
                    waiting.destination.plan() != plan || waiting.source != source
                })
            {
                return Err(io::Error::other(
                    "A different destination reservation is still pending.",
                ));
            }
            self.target_moves.warming.insert(
                plan.id,
                Warming {
                    destination: target,
                    source,
                    reply,
                },
            );
            return Ok(());
        }
        if live.store.state() != &Connection::Ready {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Open the destination’s runtime and billing route, then try the move again.",
            ));
        }
        let id = plan.id;
        if live.store.is_session_scoped() {
            let native = live.store.prepare_claude_move_writer(&id.to_string())?;
            let prepared = target.prepare(&mut self.source_moves.ledger, source, native)?;
            let _ = reply.send(Ok(prepared));
            return Ok(());
        }
        if let Some(pending) = self.target_moves.pending.get_mut(&id) {
            if pending.destination.plan() != plan || pending.source != source {
                return Err(io::Error::other(
                    "A different destination reservation is still pending.",
                ));
            }
            if pending.generation == live.generation {
                pending.reply = reply;
                return Ok(());
            }
        }
        let policy = HostPolicy::read(&self.state_dir)?;
        live.store.use_host_policy(policy);
        let runtime = scope.route.runtime;
        let access = match live.store.move_start_access(None) {
            Some(mode)
                if policy == HostPolicy::YoloAsksEveryTime
                    && Level::of(runtime, mode) == Some(Level::Yolo) =>
            {
                // No operator is present on this host during reservation.
                // The session's mode control can request YOLO after arrival.
                Some(Level::Yolo.below_yolo().preset(runtime))
            }
            access => access,
        };
        live.store.prepare_codex_move_writer(
            id.to_string(),
            access,
            target.codex_checkpoint(None)?,
        )?;
        self.target_moves.pending.insert(
            id,
            Pending {
                destination: target,
                source,
                generation: live.generation,
                reply,
            },
        );
        Ok(())
    }

    pub(super) fn collect_target_move_preparations(&mut self, scope: &Scope) {
        if self
            .running
            .get(scope)
            .is_some_and(|live| live.store.state() != &Connection::Connecting)
        {
            let warmed: Vec<_> = self
                .target_moves
                .warming
                .iter()
                .filter_map(|(id, request)| {
                    (&request.destination.plan().target_scope == scope).then_some(*id)
                })
                .collect();
            for id in warmed {
                if let Some(request) = self.target_moves.warming.remove(&id)
                    && let Err(error) = self.prepare_move_target(
                        request.source,
                        request.destination,
                        request.reply.clone(),
                    )
                {
                    let _ = request.reply.send(Err(error));
                }
            }
        }
        let Some(live) = self.running.get_mut(scope) else {
            return;
        };
        let generation = live.generation;
        for preparation in live.store.take_move_writer_preparations() {
            self.record_target_checkpoint(scope, generation, preparation);
        }
    }

    fn record_target_checkpoint(
        &mut self,
        scope: &Scope,
        generation: u64,
        preparation: MoveWriterPreparation,
    ) {
        let Ok(id) = Uuid::parse_str(&preparation.operation) else {
            return;
        };
        if !self.target_moves.pending.get(&id).is_some_and(|pending| {
            pending.generation == generation && &pending.destination.plan().target_scope == scope
        }) {
            return;
        }
        let pending = self
            .target_moves
            .pending
            .remove(&id)
            .expect("matched preparation");
        let prepared = preparation
            .result
            .map_err(io::Error::other)
            .and_then(|thread| {
                pending
                    .destination
                    .prepare(&mut self.source_moves.ledger, pending.source, thread)
            });
        let _ = pending.reply.send(prepared);
    }

    pub(super) fn activate_move_target(
        &mut self,
        source: Uuid,
        commit: &Commit,
    ) -> io::Result<Activated> {
        let plan = &commit.prepared.plan;
        let native = &commit.prepared.stage.native;
        let ledger = &mut self.source_moves.ledger;
        if ledger.status(plan.id)? == Some(crate::move_ledger::Status::Active)
            && ledger
                .owner(plan.session)?
                .is_some_and(|owner| owner.generation > plan.source.generation.saturating_add(1))
        {
            // This receipt was durable before a later move took ownership. It
            // remains retrievable without touching the previous writer or checkout.
            return ledger.activate_receiver(source, commit);
        }
        let target = VerifiedTarget::load(&self.state_dir, plan)?;
        target.check_activation(&commit.prepared)?;
        let activated = ledger.activate_receiver(source, commit)?;
        let row = target.catalog_row_for(&self.source_moves.ledger, &plan.target_scope, native)?;
        self.remember_moved_row(&plan.target_scope, &row);
        let logical = &row.detail["logical_session_id"];
        let session;
        if let Some(live) = self.running.get_mut(&plan.target_scope) {
            live.store
                .enable_prepared_move_writer(&plan.id.to_string(), native);
            session = &mut live.state;
        } else {
            session = self
                .cold
                .entry(plan.target_scope.clone())
                .or_insert_with(|| ScopeState {
                    scope: plan.target_scope.clone(),
                    connection: "disconnected".into(),
                    capabilities: Vec::new(),
                    sessions: Vec::new(),
                    asks: Vec::new(),
                    transcripts: Default::default(),
                    detail: serde_json::json!({}),
                });
        }
        if !session
            .sessions
            .iter()
            .any(|saved| saved.id == row.id && &saved.detail["logical_session_id"] == logical)
        {
            session.sessions.retain(|saved| saved.id != row.id);
            session.sessions.push(row.clone());
            self.ledger
                .update(crate::durable::SavedScope::from_state(session))?;
            self.broadcast(&plan.target_scope, vec![Change::Session { session: row }])?;
        }
        Ok(activated)
    }

    pub(super) fn moved_claude_input(
        &self,
        scope: &Scope,
        action: &Action,
    ) -> io::Result<Option<(Uuid, String, String)>> {
        if scope.route.runtime != ox_desk_core::runtime::Runtime::Claude {
            return Ok(None);
        }
        let live = &self.running[scope];
        let (thread, text, has_images) = match action {
            Action::SendText(Some(thread), text) => (thread, text.as_str(), false),
            Action::SendImages(Some(thread), text, images) => {
                (thread, text.as_str(), !images.is_empty())
            }
            Action::Submit(Some(thread)) => (
                thread,
                live.store.draft(Some(thread)),
                !live.store.draft_images(Some(thread)).is_empty(),
            ),
            _ => return Ok(None),
        };
        if live.store.has_live_session(thread) {
            return Ok(None);
        }
        let Some(prepared) = self
            .source_moves
            .ledger
            .active_receivers()?
            .into_iter()
            .find(|move_| &move_.plan.target_scope == scope && &move_.stage.native == thread)
        else {
            return Ok(None);
        };
        let plan = &prepared.plan;
        match live.store.prepare_claude_move_writer(&plan.id.to_string()) {
            Ok(native) if native == *thread => {}
            Ok(_) => return Err(io::Error::other("The reserved Claude writer changed.")),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
            Err(error) => return Err(error),
        }
        if has_images {
            return Err(io::Error::other(
                "Remove the attachment before submitting to Claude Code",
            ));
        }
        if text.trim().is_empty() {
            return Err(io::Error::other("Enter a message"));
        }
        let target = VerifiedTarget::load(&self.state_dir, plan)?;
        let text = target.claude_request(&self.source_moves.ledger, scope, thread, text)?;
        Ok(Some((plan.id, thread.clone(), text)))
    }
}

impl TargetMoves {
    pub(super) fn stamp_row(&self, scope: &Scope, row: &mut SessionView) {
        let Some(display) = self.displayed.get(scope).and_then(|rows| rows.get(&row.id)) else {
            return;
        };
        row.detail["logical_session_id"] = display.logical.clone();
        row.detail["owned"] = serde_json::Value::Bool(true);
        if row.detail["row"]["name"].is_null() {
            row.title.clone_from(&display.title);
            if let Some(runtime_row) = row.detail["row"].as_object_mut() {
                runtime_row.insert("name".into(), serde_json::json!(display.title));
            }
        }
    }
}
