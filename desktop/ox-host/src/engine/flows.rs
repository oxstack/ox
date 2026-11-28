//! Host flows of the mode model: YOLO acceptance and grants, cross-runtime
//! handoffs, and `read_ox_thread`.
pub(super) mod history;
mod remote_handoff;
use super::*;
use crate::durable::{Grant, HandoffRecord, Outcome};
use crate::workers::{Entry, Marks, Resolved};
use ox_desk_core::{
    handoff::{self, HandoffSource},
    modes::{self, Decision},
    origin::{ParentRef, Relation},
};

/// Longest wait for a handoff source's older history or a read's transcript.
const LOAD_WAIT: Duration = Duration::from_secs(40);
/// Longest wait for the target model's context window before the default is used.
const WINDOW_WAIT: Duration = Duration::from_secs(12);
/// Bytes of item text one read_ox_thread page returns at most.
const PAGE_BYTES: usize = 256 * 1024;

struct HandoffLaunch {
    source: HandoffSource,
    selection: handoff::Selection,
    start: HandoffStart,
    resolved: modes::Resolved,
    yolo: Option<YoloAllowance>,
    target: Scope,
    surface: Option<ox_desk_core::surface::Surface>,
    record: String,
    owner: String,
}

impl Engine {
    /// The grant covering a YOLO child of this session: one stored with the
    /// session or an ancestor (worker parents and handoff sources), or the
    /// project default from `[workers] yolo_grant_projects`.
    pub(super) fn yolo_grant(&mut self, scope: &Scope, session: &str) -> Option<YoloAllowance> {
        let now = crate::durable::unix_now();
        if self.ledger.data.grants.iter().any(|g| !g.live(now)) {
            // Expired grants stop counting and are dropped from the ledger.
            let _ = self.ledger.change(|d| d.grants.retain(|g| g.live(now)));
        }
        let mut current = Some((scope.clone(), session.to_owned()));
        for _ in 0..64 {
            let Some((s, id)) = current.take() else { break };
            if self
                .ledger
                .data
                .grants
                .iter()
                .any(|g| g.scope == s && g.session == id && g.live(now))
            {
                return Some(YoloAllowance::Grant {
                    project: s.project.clone(),
                    session: Some(id),
                });
            }
            current = self.workers.get(&s, &id).and_then(|e| {
                Some((
                    e.origin.parent.scope.clone()?,
                    e.origin.parent.session.clone()?,
                ))
            });
        }
        let by_default = self
            .yolo_projects()
            .iter()
            .any(|p| p == &scope.project || p.canonicalize().ok().as_ref() == Some(&scope.project));
        if by_default {
            self.announce_yolo_default(scope);
        }
        by_default.then(|| YoloAllowance::Grant {
            project: scope.project.clone(),
            session: None,
        })
    }

    /// Tell the operator, once per project and host run, that a session started under the
    /// project's YOLO default, and log it.
    fn announce_yolo_default(&mut self, scope: &Scope) {
        if !self.yolo_announced.insert(scope.project.clone()) {
            return;
        }
        ox_host_service::errlog::record(&format!(
            "YOLO default applied for {}",
            scope.project.display()
        ));
        if let Some(live) = self.running.get_mut(scope) {
            live.store
                .post_notice("YOLO is on for this project by default. Change it in Settings.");
        }
    }

    /// Whether the worker of this job is running or waiting for the operator.
    fn worker_active(&self, job_id: u64) -> bool {
        self.job_status(job_id).is_ok_and(|s| {
            matches!(
                s["status"].as_str(),
                Some("running" | "waiting_for_operator")
            )
        })
    }

    /// Refuse a worker start when this parent already runs the maximum number of workers.
    pub(super) fn check_worker_cap(&self, parent: &ParentRef) -> Result<(), String> {
        self.check_worker_cap_with(
            parent,
            crate::workers::env_limit("OX_WORKER_MAX_PER_PARENT").map(|c| c as usize),
        )
    }

    /// The cap check with the limit given. No limit by default: it is opt-in.
    pub(super) fn check_worker_cap_with(
        &self,
        parent: &ParentRef,
        cap: Option<usize>,
    ) -> Result<(), String> {
        let Some(cap) = cap else {
            return Ok(());
        };
        let started = self
            .workers
            .jobs()
            .filter(|(_, _, e)| &e.origin.parent == parent)
            .filter(|(_, _, e)| self.worker_active(e.origin.job_id))
            .count();
        let starting = self
            .running
            .values()
            .filter_map(|l| l.launching.as_ref())
            .filter(|l| l.origin.job_id != 0 && &l.origin.parent == parent)
            .count()
            + self
                .asking
                .values()
                .filter(|a| &a.origin.parent == parent)
                .count();
        let remote = self
            .ledger
            .data
            .forwarded
            .values()
            .filter(|j| &j.origin.parent == parent && j.active())
            .count();
        if started + starting + remote >= cap {
            return Err(format!(
                "This session already runs {cap} workers. Wait for one to finish or cancel one. The limit is OX_WORKER_MAX_PER_PARENT."
            ));
        }
        Ok(())
    }

    /// A caution when another running worker can write in the same project.
    pub(super) fn write_overlap(
        &self,
        scope: &Scope,
        level: ox_desk_core::origin::Ceiling,
    ) -> Option<String> {
        if level < ox_desk_core::origin::Ceiling::Write {
            return None;
        }
        self.workers
            .jobs()
            .any(|(worker_scope, _, e)| {
                worker_scope.project == scope.project
                    && e.origin.ceiling >= ox_desk_core::origin::Ceiling::Write
                    && self.worker_active(e.origin.job_id)
            })
            .then(|| {
                "Another worker had write access to this checkout when this job started. Use a separate git worktree for independent edits."
                    .to_owned()
            })
    }

    /// Record how each worker's latest run ended, from the live store, while the
    /// answer is still readable. The record is only the final answer: the transcript
    /// stays in the runtime's own history. A run in progress clears it. Returns whether
    /// any record changed, so the caller can republish the rows that carry it.
    pub(super) fn capture_finals(&mut self, scope: &Scope) -> bool {
        let Some(live) = self.running.get(scope) else {
            return false;
        };
        let ready = live.store.state() == &Connection::Ready;
        let operators = self.operators();
        // The models that answered, recorded as the runtime reports them, so status,
        // the completion record and the worker view survive the process and restarts.
        let answered: Vec<(String, Vec<String>)> = self
            .workers
            .in_scope(scope)
            .filter_map(|(id, entry)| {
                let models = live.store.claude_answering_models(id);
                (!models.is_empty() && !entry.marks.has_models(models))
                    .then(|| (id.to_owned(), models.to_vec()))
            })
            .collect();
        for (id, models) in &answered {
            if let Some(entry) = self.workers.get_mut(scope, id) {
                entry.marks.record_models(models);
            }
        }
        let mut updates: Vec<(String, Option<crate::workers::Last>)> = Vec::new();
        for (id, entry) in self.workers.in_scope(scope) {
            if entry.origin.relation != Relation::Worker || entry.marks.cancelled {
                continue;
            }
            let view = live.state.sessions.iter().find(|v| v.id == id);
            let timeline = live.state.transcripts.get(id);
            let native = id.rsplit(':').next().unwrap_or(id);
            let status =
                crate::workers::status(entry, id, native, view, timeline, ready, operators);
            let next = match status["status"].as_str() {
                Some("completed" | "failed") if timeline.is_some() => {
                    Some(Some(crate::workers::Last {
                        status: status["status"].as_str().unwrap_or("completed").to_owned(),
                        result: status["result"].as_str().map(str::to_owned),
                        error: status["error"].as_str().map(str::to_owned),
                        model: status["actual_model"].as_str().map(str::to_owned),
                    }))
                }
                // A stop the runtime confirmed, with the partial text it left. A route that
                // merely closed is interrupted too, but says nothing about the run's end.
                Some("interrupted")
                    if timeline.is_some_and(|t| t.detail["interrupted"] == true) =>
                {
                    Some(Some(crate::workers::Last {
                        status: "interrupted".to_owned(),
                        result: status["result"].as_str().map(str::to_owned),
                        error: None,
                        model: status["actual_model"].as_str().map(str::to_owned),
                    }))
                }
                Some("running" | "waiting_for_operator") => Some(None),
                _ => None,
            };
            if let Some(next) = next
                && entry.marks.last != next
            {
                updates.push((id.to_owned(), next));
            }
        }
        let changed = !updates.is_empty() || !answered.is_empty();
        for (id, last) in updates {
            if let Some(entry) = self.workers.get_mut(scope, &id) {
                entry.marks.last = last;
            }
        }
        changed
    }

    /// Stop workers that ran past their wall-clock limit.
    pub(super) fn expire_workers(&mut self) -> io::Result<()> {
        let now = crate::durable::unix_now();
        let due: Vec<(Scope, String)> = self
            .workers
            .jobs()
            .filter(|(_, _, e)| {
                let m = &e.marks;
                !m.expired
                    && !m.cancelled
                    && m.max_minutes.is_some_and(|max| {
                        // The current run's start: a continued worker is measured from
                        // its continuation, not from the original launch.
                        let began = if m.run_started_at > 0 {
                            m.run_started_at
                        } else {
                            m.started_at
                        };
                        began > 0 && now - began >= i64::from(max) * 60
                    })
            })
            .filter(|(_, _, e)| self.worker_active(e.origin.job_id))
            .map(|(s, id, _)| (s.clone(), id.to_owned()))
            .collect();
        for (scope, id) in due {
            if let Some(live) = self.running.get_mut(&scope) {
                let _ = live.store.stop(&id);
            }
            if let Some(entry) = self.workers.get_mut(&scope, &id) {
                entry.marks.expired = true;
                entry.marks.cancelled = true;
            }
            self.persist_marks(&scope)?;
        }
        Ok(())
    }

    /// Drop the grants a session gave: they end with the session.
    pub(super) fn drop_grant(&mut self, scope: &Scope, session: &str) -> io::Result<()> {
        if self
            .ledger
            .data
            .grants
            .iter()
            .any(|g| &g.scope == scope && g.session == session)
        {
            self.ledger.change(|d| {
                d.grants
                    .retain(|g| !(&g.scope == scope && g.session == session))
            })?;
        }
        Ok(())
    }

    /// The project default from the settings snapshot. A failed read is retried once, and when
    /// it still fails the last value read is kept, so a transient failure cannot deny a project
    /// the operator already allowed.
    fn yolo_projects(&mut self) -> Vec<PathBuf> {
        if let Some(prepared) = (0..2).find_map(|_| self.config.prepare().ok()) {
            self.yolo_projects = prepared.yolo_grant_projects;
        }
        self.yolo_projects.clone()
    }

    /// Hold a YOLO child for the operator and tell the parent at once.
    pub(super) fn ask_yolo(&mut self, asking: Asking, reply: Replier) -> io::Result<()> {
        let parent = asking.origin.parent.scope.clone();
        let origin = asking.origin.clone();
        self.ledger.change(|d| {
            d.worker_notices.observe(
                &origin,
                "waiting_for_operator",
                "The operator needs to answer the worker’s access request.",
            );
            d.asking.push(origin);
        })?;
        let job = asking.origin.job_id;
        self.asking.insert(job, asking);
        let status = crate::yolo::status(&self.asking[&job], self.operators().connected);
        self.answer(reply, Ok(status))?;
        if let Some(parent) = parent.filter(|p| self.running.contains_key(p)) {
            self.process(&parent, vec![], true)?;
        }
        Ok(())
    }

    /// The operator's answer to a YOLO request. The parent agent has no path here.
    pub(super) fn answer_yolo(
        &mut self,
        client: u64,
        scope: &Scope,
        job: u64,
        choice: usize,
    ) -> io::Result<()> {
        let answer = crate::yolo::Answer::of(choice)
            .ok_or_else(|| io::Error::other("Choose Accept YOLO, a lower mode or Decline."))?;
        let a = self
            .asking
            .get(&job)
            .filter(|a| a.origin.parent.scope.as_ref() == Some(scope))
            .ok_or_else(|| io::Error::other("This YOLO request was already answered."))?;
        let mut origin = a.origin.clone();
        // The worker starts only for a parent that is still live.
        let parent_live = a.origin.parent.session.as_deref().is_some_and(|t| {
            self.running
                .get(scope)
                .is_some_and(|l| l.store.catalog_row(t).is_some() && !l.store.is_ended(t))
        });
        if !parent_live {
            self.withdraw_with(job, ENDED_PARENT)?;
            return Err(io::Error::other(ENDED_PARENT));
        }
        match answer {
            crate::yolo::Answer::Decline => {
                self.asking.remove(&job);
                self.ledger.change(|d| {
                    d.asking.retain(|o| o.job_id != job);
                    d.record_outcome(Outcome {
                        origin,
                        status: "declined".into(),
                        message: "The operator declined to start this worker in YOLO.".into(),
                    });
                })?;
            }
            accept @ (crate::yolo::Answer::Accept | crate::yolo::Answer::Lower) => {
                if matches!(accept, crate::yolo::Answer::Accept) {
                    origin.yolo = Some(YoloAllowance::Accepted {
                        request: format!("{}{job}", crate::yolo::PREFIX),
                        device: format!("client-{client}"),
                    });
                } else {
                    let level = origin.ceiling.below_yolo();
                    origin.requested = level;
                    origin.ceiling = level;
                    origin.started = Some(level.preset(origin.runtime));
                    origin.decided = Some(Decision::Capped);
                }
                let target = a.target.clone();
                let start = a.start.clone();
                let memory_brief = a.memory_brief.clone();
                // A busy route keeps the request pending so the operator can retry.
                let mut l = self
                    .prepare_launch(&target, origin, Payload::Worker(start))
                    .map_err(io::Error::other)?;
                l.detached = true;
                l.memory_brief = memory_brief;
                self.asking.remove(&job);
                self.ledger
                    .change(|d| d.asking.retain(|o| o.job_id != job))?;
                self.running
                    .get_mut(&target)
                    .expect("ensured target")
                    .launching = Some(l);
                self.process(&target, vec![], false)?;
            }
        }
        if self.running.contains_key(scope) {
            self.process(scope, vec![], true)?;
        }
        Ok(())
    }

    /// Store or revoke a session's YOLO grant. Revoking affects new spawns only.
    pub(super) fn set_grant(&mut self, scope: &Scope, session: &str, on: bool) -> io::Result<()> {
        let live = self.running.get_mut(scope).expect("validated live scope");
        if live.store.catalog_row(session).is_none() {
            return Err(io::Error::other(
                "Open the session before changing its YOLO grant.",
            ));
        }
        if !on {
            live.store.annotate_session(
                session,
                None,
                json!({"yolo_grant": false, "yolo_children": []}),
            );
        }
        let grant = Grant {
            scope: scope.clone(),
            session: session.to_owned(),
            granted_at: crate::durable::unix_now(),
        };
        if on && let Some(live) = self.running.get_mut(scope) {
            live.store
                .post_notice("YOLO grant on for this session. It expires after 12 hours or when the session ends.");
        }
        self.ledger.change(|d| {
            d.grants
                .retain(|g| !(g.scope == grant.scope && g.session == grant.session));
            if on {
                d.grants.push(grant);
            }
        })
    }

    /// Why a session cannot be handed off right now, naming what is pending.
    fn handoff_blocker(&self, scope: &Scope, source: &str) -> Option<String> {
        if scope.route.runtime == ox_desk_core::runtime::Runtime::OpenCode {
            return Some("Hosted OpenCode does not support session handoff.".into());
        }
        let live = self.running.get(scope)?;
        if live.store.catalog_row(source).is_none() || live.store.transcript(source).is_none() {
            return Some("Open the session before handing it off.".into());
        }
        let mut reasons = Vec::new();
        if live
            .store
            .transcript(source)
            .is_some_and(|t| t.active_turn().is_some())
        {
            reasons.push("it is running a turn".to_owned());
        }
        let asks: Vec<_> = live
            .store
            .asks()
            .iter()
            .filter(|a| a.thread.as_deref() == Some(source))
            .map(|a| a.title().to_owned())
            .chain(
                self.asking
                    .values()
                    .filter(|a| {
                        a.origin.parent.scope.as_ref() == Some(scope)
                            && a.origin.parent.session.as_deref() == Some(source)
                    })
                    .map(|a| crate::yolo::view(a).title),
            )
            .collect();
        if !asks.is_empty() {
            reasons.push(format!(
                "it has {} pending request{}: {}",
                asks.len(),
                if asks.len() == 1 { "" } else { "s" },
                asks.join("; ")
            ));
        }
        if self
            .preparing
            .iter()
            .any(|p| &p.source == scope && p.start.source == source)
        {
            reasons.push("a handoff from it is already starting".into());
        }
        (!reasons.is_empty()).then(|| {
            format!(
                "This session cannot be handed off yet: {}. Stop or finish it first.",
                reasons.join(", and ")
            )
        })
    }

    pub(super) fn start_handoff(
        &mut self,
        client: u64,
        key: &str,
        scope: &Scope,
        start: HandoffStart,
    ) -> io::Result<()> {
        self.start_handoff_checked(client, key, scope, start, None)
    }
    pub(super) fn start_handoff_checked(
        &mut self,
        client: u64,
        key: &str,
        scope: &Scope,
        start: HandoffStart,
        mode_approval: Option<YoloAllowance>,
    ) -> io::Result<()> {
        if start.host.is_some() {
            return Err(io::Error::other(
                "Choose this handoff’s destination in OX Desktop.",
            ));
        }
        start.validate().map_err(io::Error::other)?;
        if let Some(reason) = self.handoff_blocker(scope, &start.source) {
            return Err(io::Error::other(reason));
        }
        let policy = HostPolicy::read(&self.state_dir)?;
        if mode_approval.is_none() && policy.starting_level().is_none() {
            let source = &start.source;
            let current = self.running[scope].store.applied_ceiling(Some(source));
            let stamp = self.workers.get(scope, source).map(|e| &e.origin);
            let verified = crate::workers::source_level(current, stamp);
            let resolved = modes::resolve_operator(verified, start.mode, start.runtime)
                .map_err(io::Error::other)?;
            if resolved.level.needs_person()
                && policy == HostPolicy::FollowInterface
                && !start.yolo_confirmed
                && self.yolo_grant(scope, source).is_none()
            {
                return Err(io::Error::other(
                    "This handoff would start in YOLO. Choose YOLO to confirm it, or select a lower mode.",
                ));
            }
            if resolved.level.needs_person() && policy == HostPolicy::YoloAsksEveryTime {
                return self.ask_activation(
                    scope,
                    mode_requests::Activation::Handoff {
                        client,
                        key: key.into(),
                        start,
                    },
                );
            }
        }
        // The helper may run provider model discovery, so the catalog lookup
        // never blocks the engine. A slow or failed lookup uses the default budget.
        let window = std::sync::Arc::new(std::sync::Mutex::new(None));
        let route = start.route();
        let windows = self.ledger.data.windows.clone();
        match &route.billing {
            ox_desk_core::runtime::Billing::Api(alias) => {
                let (slot, alias, config) = (window.clone(), alias.clone(), self.config.clone());
                let route = route.clone();
                let (tx, source) = (self.tx.clone(), scope.clone());
                let generation = self.running.get(scope).map(|l| l.generation);
                thread::spawn(move || {
                    let found = ox_desk_core::ports::ox::request::<BTreeMap<String, u64>>(
                        &config.ox,
                        &config.path,
                        ox_desk_core::settings_protocol::Request::ModelContexts,
                    )
                    .ok()
                    .and_then(|w| w.get(&alias).copied());
                    *slot.lock().unwrap() =
                        Some(crate::durable::handoff_window(&windows, &route, found));
                    if let Some(generation) = generation {
                        let _ = tx.send(Message::Runtime(source, generation));
                    }
                });
            }
            ox_desk_core::runtime::Billing::Subscription => {
                *window.lock().unwrap() =
                    Some(crate::durable::handoff_window(&windows, &route, None))
            }
        }
        self.preparing.push(Preparing {
            export_to: None,
            source: scope.clone(),
            start,
            reply: Replier::Client(client, key.to_owned()),
            device: format!("client-{client}"),
            surface: self.clients.get(&client).and_then(|c| c.surface),
            deadline: Instant::now() + LOAD_WAIT,
            window,
            window_deadline: Instant::now() + WINDOW_WAIT,
            mode_approval,
        });
        Ok(())
    }

    /// Resolve, select and open the target. Nothing starts on an error.
    fn launch_handoff(&mut self, prep: Preparing) -> Result<(), (Replier, String)> {
        let Preparing {
            export_to,
            source: src,
            start,
            reply,
            device,
            surface,
            mode_approval,
            window,
            ..
        } = prep;
        let window = window.lock().unwrap().unwrap_or_else(|| {
            crate::durable::handoff_window(&self.ledger.data.windows, &start.route(), None)
        });
        macro_rules! fail {
            ($e:expr) => {
                return Err((reply, $e))
            };
        }
        if let Some(reason) = self.handoff_blocker(&src, &start.source) {
            fail!(reason);
        }
        let live = &self.running[&src];
        let Some(timeline) = live.state.transcripts.get(&start.source) else {
            fail!("The session's history is not loaded. Open it and try again.".into());
        };
        let mut items = handoff::handoff_items(timeline);
        if let Some((target, id)) = &export_to
            && let Err(e) =
                self.restore_export_boundary(*id, target, &src, &start.source, &mut items)
        {
            fail!(e.to_string());
        }
        let title = live
            .store
            .catalog_row(&start.source)
            .map(|r| r.title().to_owned())
            .unwrap_or_default();
        let native = live
            .store
            .terminal_session_id(&start.source)
            .ok()
            .map(str::to_owned)
            .filter(|n| n != &start.source);
        let access = live.store.applied_access(Some(&start.source));
        let ceiling = live.store.applied_ceiling(Some(&start.source));
        let src_origin = self
            .workers
            .get(&src, &start.source)
            .map(|e| e.origin.clone());
        let level = crate::workers::source_level(ceiling, src_origin.as_ref());
        // A source that was itself handed off carries only its own messages;
        // the earlier chain stays readable through read_ox_thread.
        let chain = self.handoff_chain(&src, &start.source);
        let ancestors = chain
            .last()
            .filter(|s| {
                s.host
                    .as_ref()
                    .is_some_and(|h| h.id != self.host_id.to_string())
            })
            .map(|s| s.selection.earlier.clone())
            .unwrap_or_default();
        let earlier = chain
            .into_iter()
            .map(|h| handoff::Earlier {
                host: h.host,
                session: h.session,
                title: h.title,
                runtime: h.runtime,
                route: h.route,
            })
            .chain(ancestors)
            .take(32)
            .collect();
        let selection = match handoff::select_chained(items, &start.request, window, earlier) {
            Ok(s) => s,
            Err(e) => fail!(e),
        };
        let mut source = HandoffSource {
            history_broker: None,
            host: Some(self.handoff_host()),
            scope: src.clone(),
            session: start.source.clone(),
            native,
            title,
            runtime: src.route.runtime,
            route: src.route.clone(),
            source_access: access,
            source_level: level,
            decision: Decision::Inherited,
            selection: selection.refs.clone(),
        };
        if let Some((target_host, transfer)) = export_to {
            let Some(repository) = ox_host_service::checkout::repository_id(&src.project) else {
                fail!(
                    "The source project needs a Git origin to match its destination checkout."
                        .into()
                );
            };
            let mut packet = handoff::HandoffTransfer {
                id: transfer.to_string(),
                target_host,
                repository,
                start,
                source,
                carried: selection.carried,
                child_ceiling: src_origin.as_ref().and_then(Origin::limits_children),
            };
            if let Replier::Client(client, _) = &reply
                && let Some(version) = self.clients.get(client).and_then(|client| client.version)
                && !wire::project_handoff_transfer(version, &mut packet)
            {
                fail!(
                    "Update this client to transfer a conversation with Antigravity history."
                        .into()
                );
            }
            if let Err(e) = self.record_export(&packet) {
                fail!(e.to_string());
            }
            if let Err(e) = self.answer(reply, Ok(json!(packet))) {
                tracing_error(&e);
            }
            return Ok(());
        }
        let policy = match HostPolicy::read(&self.state_dir) {
            Ok(policy) => policy,
            Err(error) => fail!(format!("Could not read the host mode policy: {error}")),
        };
        let resolved = if let Some(fixed) = policy.starting_level() {
            let ceiling = src_origin
                .as_ref()
                .and_then(Origin::limits_children)
                .unwrap_or(modes::Level::Yolo);
            let start_level = match start.mode {
                modes::ModeRequest::Inherit => fixed,
                modes::ModeRequest::Level(requested) => requested.min(fixed),
            }
            .min(ceiling);
            modes::Resolved::new(start_level, start.runtime, Decision::HostPolicy)
        } else {
            match modes::resolve_operator(level, start.mode, start.runtime) {
                Ok(r) => r,
                Err(e) => fail!(e),
            }
        };
        let yolo = if resolved.level.needs_person() && policy == HostPolicy::AlwaysYolo {
            Some(YoloAllowance::HostPolicy)
        } else if resolved.level.needs_person() && policy == HostPolicy::YoloAsksEveryTime {
            match mode_approval {
                Some(accepted) => Some(accepted),
                None => fail!(
                    "The host’s mode policy changed. Start this handoff again to review YOLO."
                        .into()
                ),
            }
        } else if resolved.level.needs_person() {
            match self.yolo_grant(&src, &start.source) {
                Some(grant) => Some(grant),
                None if start.yolo_confirmed => Some(YoloAllowance::Operator {
                    device: device.clone(),
                }),
                None => fail!(
                    "Confirm YOLO for the new session first. It would run without permission prompts."
                        .into()
                ),
            }
        } else {
            None
        };
        let target = match canonical_scope(Scope {
            project: src.project.clone(),
            route: start.route(),
        }) {
            Ok(t) => t,
            Err(e) => fail!(e.to_string()),
        };
        source.decision = resolved.decision;
        let record = match nonce() {
            Ok(n) => n,
            Err(e) => fail!(e.to_string()),
        };
        self.begin_handoff(
            reply,
            HandoffLaunch {
                source,
                selection,
                start,
                resolved,
                yolo,
                target,
                surface,
                record,
                owner: format!("operator:{device}"),
            },
        )
    }

    fn begin_handoff(
        &mut self,
        reply: Replier,
        launch: HandoffLaunch,
    ) -> Result<(), (Replier, String)> {
        let HandoffLaunch {
            source,
            selection,
            start,
            resolved,
            yolo,
            target,
            surface,
            record,
            owner,
        } = launch;
        macro_rules! fail {
            ($e:expr) => {
                return Err((reply, $e))
            };
        }
        let (text, carried) = match start.runtime {
            ox_desk_core::runtime::Runtime::Codex => (
                start.request.clone(),
                handoff::codex_items(&source, &selection),
            ),
            ox_desk_core::runtime::Runtime::Claude => (
                handoff::claude_turn(&source, &selection, &start.request),
                vec![],
            ),
            ox_desk_core::runtime::Runtime::Antigravity => (
                handoff::context_turn(&source, &selection, &start.request, false),
                vec![],
            ),
            ox_desk_core::runtime::Runtime::OpenCode => {
                fail!("Hosted OpenCode does not support session handoff.".into())
            }
        };
        let origin = Origin {
            linked: None,
            relation: Relation::Handoff,
            parent: ParentRef {
                project: source.scope.project.clone(),
                scope: Some(source.scope.clone()),
                session: Some(start.source.clone()),
                runtime: Some(source.scope.route.runtime),
            },
            launch: owner,
            by_operator: true,
            runtime: start.runtime,
            route: target.route.clone(),
            mode: WorkerMode::Session,
            job_id: 0,
            requested: resolved.level,
            ceiling: resolved.level,
            source: None,
            started: Some(resolved.access),
            decided: Some(resolved.decision),
            yolo,
            handoff: Some(Box::new(source)),
            requested_model: None,
            pinned_model: None,
        };
        let payload = Payload::Handoff {
            text,
            items: carried,
            record: record.clone(),
            from: surface,
        };
        let mut l = match self.prepare_launch(&target, origin.clone(), payload) {
            Ok(l) => l,
            Err(e) => fail!(e),
        };
        if let Err(e) = self.ledger.change(|d| {
            d.handoffs.push(HandoffRecord {
                id: record,
                target: target.clone(),
                origin,
                status: "starting".into(),
                message: None,
            })
        }) {
            fail!(e.to_string());
        }
        l.reply = reply;
        self.running
            .get_mut(&target)
            .expect("ensured target")
            .launching = Some(l);
        if let Err(e) = self.process(&target, vec![], false) {
            // The ledger failed; the engine stops on the next ledger check.
            tracing_error(&e);
        }
        Ok(())
    }

    /// Advance handoffs waiting for history and reads waiting for transcripts.
    pub(super) fn advance_flows(&mut self) -> io::Result<()> {
        if self.in_flows {
            return Ok(());
        }
        self.in_flows = true;
        let result = self
            .advance_flows_inner()
            .and_then(|()| self.deliver_worker_notices());
        self.in_flows = false;
        result
    }
    fn advance_flows_inner(&mut self) -> io::Result<()> {
        let now = Instant::now();
        self.expire_history(now)?;
        for prep in std::mem::take(&mut self.preparing) {
            let Some(live) = self.running.get_mut(&prep.source) else {
                self.answer(prep.reply, Err("The source route closed.".into()))?;
                continue;
            };
            let Some(t) = live.store.transcript(&prep.start.source) else {
                self.answer(
                    prep.reply,
                    Err("Open the session before handing it off.".into()),
                )?;
                continue;
            };
            let waiting_window =
                prep.window.lock().unwrap().is_none() && prep.window_deadline > now;
            if t.has_older() || t.is_loading() || waiting_window {
                if prep.deadline <= now {
                    self.answer(
                        prep.reply,
                        Err(
                            "The session's earlier history did not load in time. Try again.".into(),
                        ),
                    )?;
                    continue;
                }
                if t.has_older() && !t.is_loading() {
                    let _ = live
                        .store
                        .dispatch(Action::LoadOlder(prep.start.source.clone()));
                }
                self.preparing.push(prep);
                continue;
            }
            if let Err((reply, e)) = self.launch_handoff(prep) {
                self.answer(reply, Err(e))?;
            }
        }
        for read in std::mem::take(&mut self.reading) {
            match self.advance_read(read, now) {
                Step::Wait(read) => self.reading.push(read),
                Step::Answer(reply, result) => self.answer(reply, result)?,
            }
        }
        Ok(())
    }

    fn handoff_host(&self) -> handoff::SourceHost {
        let name = ox_host_web::host_profile::HostProfile::read(&self.state_dir)
            .ok()
            .map(|p| p.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.host_name.clone());
        handoff::SourceHost {
            id: self.host_id.to_string(),
            name,
        }
    }

    /// The handoff sources of a session, nearest first.
    pub(super) fn handoff_chain(&self, scope: &Scope, session: &str) -> Vec<HandoffSource> {
        let local_host = self.handoff_host();
        let mut chain: Vec<HandoffSource> = Vec::new();
        let mut current = (scope.clone(), session.to_owned());
        for _ in 0..32 {
            let Some(h) = self
                .workers
                .get(&current.0, &current.1)
                .and_then(|e| e.origin.handoff.as_deref())
            else {
                break;
            };
            let mut source = h.clone();
            let host = source.host.get_or_insert_with(|| local_host.clone());
            let local = host.id == local_host.id;
            if chain.iter().any(|c| {
                c.host.as_ref().map(|h| &h.id) == Some(&host.id)
                    && c.scope == source.scope
                    && c.session == source.session
            }) {
                break;
            }
            current = (h.scope.clone(), h.session.clone());
            chain.push(source);
            if !local {
                break;
            }
        }
        chain
    }

    /// read_ox_thread: a session reads its own handoff sources, read-only and
    /// within its project. Resolved by the same exact-caller rules as every
    /// parent operation.
    pub(super) fn read_thread(
        &mut self,
        caller: &Caller,
        call: ParentCall,
        reply: Replier,
    ) -> io::Result<()> {
        let ParentCall::ReadThread {
            host,
            session,
            item,
            after,
            limit,
        } = call
        else {
            return self.answer(reply, Err("Not a read.".into()));
        };
        let (scope, owner) = match self.resolve(caller) {
            Ok(Resolved::Hosted { scope, session }) => (scope, session),
            Ok(_) => {
                let message = "read_ox_thread reads the handoff sources of sessions the OX session host runs. This launch has none.";
                return self.answer(reply, Err(message.into()));
            }
            Err(error) => return self.answer(reply, Err(error)),
        };
        let page = wire::HandoffPage {
            item,
            after,
            limit,
            ancestor: None,
        };
        self.read_history_of(
            &scope,
            &owner,
            host.as_deref(),
            session.as_deref(),
            page,
            reply,
            None,
        )
    }

    fn load_source_page(
        &mut self,
        src: HandoffSource,
        page: wire::HandoffPage,
        reply: Replier,
        frozen: bool,
    ) -> io::Result<()> {
        let wire::HandoffPage {
            item, after, limit, ..
        } = page;
        if let Err(e) = self.ensure(&src.scope, None) {
            return self.answer(
                reply,
                Err(format!("The source route cannot be opened: {e}")),
            );
        }
        self.reading.push(Reading {
            frozen,
            scope: src.scope.clone(),
            session: src.session.clone(),
            source: Box::new(src),
            args: ReadArgs {
                item,
                after,
                limit: limit.unwrap_or(20).clamp(1, 100) as usize,
            },
            reply,
            deadline: Instant::now() + LOAD_WAIT,
            selected: false,
        });
        self.advance_flows()
    }

    fn advance_read(&mut self, mut r: Reading, now: Instant) -> Step {
        let late = r.deadline <= now;
        let Some(live) = self.running.get_mut(&r.scope) else {
            return Step::Answer(r.reply, Err("The source route closed.".into()));
        };
        let wait = |r: Reading, why: &str| {
            if late {
                Step::Answer(r.reply, Err(format!("{why} Try again shortly.")))
            } else {
                Step::Wait(r)
            }
        };
        if live.store.state() != &Connection::Ready {
            return wait(r, "The source route is still connecting.");
        }
        let Some(t) = live.store.transcript(&r.session) else {
            if live.store.catalog_row(&r.session).is_some() && !r.selected {
                r.selected = true;
                let _ = live.store.dispatch(Action::SelectThread(r.session.clone()));
            }
            return wait(r, "The source session is not loaded yet.");
        };
        if t.has_older() || t.is_loading() {
            if !t.is_loading() {
                let _ = live.store.dispatch(Action::LoadOlder(r.session.clone()));
            }
            return wait(r, "The source session's history is still loading.");
        }
        match live.state.transcripts.get(&r.session) {
            Some(timeline) => {
                let page = read_source_page(&r.source, timeline, &r.args, r.frozen);
                Step::Answer(r.reply, page)
            }
            None => wait(r, "The source session is not loaded yet."),
        }
    }

    /// What a parent agent reads for a job that is not (yet) a session.
    pub(super) fn unstarted_job(&self, me: &Resolved, job_id: u64) -> Option<Value> {
        let owned = |origin: &Origin| {
            crate::workers::owns(
                me,
                &Entry {
                    origin: origin.clone(),
                    marks: Marks::default(),
                },
            )
        };
        if let Some(a) = self.asking.get(&job_id).filter(|a| owned(&a.origin)) {
            return Some(crate::yolo::status(a, self.operators().connected));
        }
        if let Some(o) = self
            .ledger
            .data
            .outcomes
            .iter()
            .rev()
            .find(|o| o.origin.job_id == job_id && owned(&o.origin))
        {
            return Some(crate::yolo::outcome(o));
        }
        self.running
            .values()
            .filter_map(|l| l.launching.as_ref())
            .find(|l| l.origin.job_id == job_id && l.origin.job_id != 0 && owned(&l.origin))
            .map(|l| {
                json!({"job_id": job_id, "status": "starting", "runtime": l.origin.runtime,
                    "mode": l.origin.mode, "owner": "host", "can_steer": false,
                    "access_ceiling": l.origin.ceiling, "messages": []})
            })
    }
    /// Job numbers a parent owns that are not sessions yet.
    pub(super) fn unstarted_jobs(&self, me: &Resolved) -> Vec<u64> {
        let mut jobs: Vec<u64> = self
            .asking
            .keys()
            .copied()
            .chain(self.ledger.data.outcomes.iter().map(|o| o.origin.job_id))
            .chain(
                self.running
                    .values()
                    .filter_map(|l| l.launching.as_ref())
                    .map(|l| l.origin.job_id)
                    .filter(|j| *j != 0),
            )
            .filter(|j| self.workers.job(*j).is_none() && self.unstarted_job(me, *j).is_some())
            .collect();
        jobs.sort_unstable();
        jobs.dedup();
        jobs
    }
    /// A parent may withdraw its own YOLO request before the operator answers.
    pub(super) fn withdraw(&mut self, job_id: u64) -> io::Result<Option<Value>> {
        self.withdraw_with(
            job_id,
            "The parent withdrew the request before the operator answered.",
        )
    }
    /// Withdraw a pending YOLO request, recording `message` as why it was cancelled.
    fn withdraw_with(&mut self, job_id: u64, message: &str) -> io::Result<Option<Value>> {
        let Some(a) = self.asking.remove(&job_id) else {
            return Ok(None);
        };
        let origin = a.origin.clone();
        self.ledger.change(|d| {
            d.asking.retain(|o| o.job_id != job_id);
            d.record_outcome(Outcome {
                origin,
                status: "cancelled".into(),
                message: message.into(),
            });
        })?;
        if let Some(parent) = a
            .origin
            .parent
            .scope
            .clone()
            .filter(|p| self.running.contains_key(p))
        {
            self.process(&parent, vec![], true)?;
        }
        Ok(Some(json!({"job_id": job_id, "status": "cancelled"})))
    }
    /// Withdraw every pending YOLO request whose parent is this session or one of its
    /// workers (at any depth), so an ended session leaves no request to answer.
    pub(super) fn withdraw_asks_under(&mut self, scope: &Scope, thread: &str) -> io::Result<()> {
        let mut parents = vec![(scope.clone(), thread.to_owned())];
        let mut next = 0;
        while next < parents.len() {
            let (s, t) = parents[next].clone();
            let kids: Vec<(Scope, String)> = self
                .workers
                .children_of(&s, &t)
                .map(|(cs, cid)| (cs.clone(), cid.to_owned()))
                .collect();
            for kid in kids {
                if !parents.contains(&kid) {
                    parents.push(kid);
                }
            }
            next += 1;
        }
        let under = |o: &Origin| match (o.parent.scope.as_ref(), o.parent.session.as_deref()) {
            (Some(s), Some(t)) => parents.iter().any(|(ps, pt)| ps == s && pt == t),
            _ => false,
        };
        let jobs: Vec<u64> = self
            .asking
            .iter()
            .filter(|(_, a)| under(&a.origin))
            .map(|(job, _)| *job)
            .collect();
        for job in jobs {
            self.withdraw_with(job, ENDED_PARENT)?;
        }
        // A request recorded in the ledger but not held in memory is cancelled too.
        let stale: Vec<Origin> = self
            .ledger
            .data
            .asking
            .iter()
            .filter(|o| under(o))
            .cloned()
            .collect();
        if !stale.is_empty() {
            self.ledger.change(|d| {
                d.asking.retain(|o| !under(o));
                for origin in stale {
                    d.record_outcome(Outcome {
                        origin,
                        status: "cancelled".into(),
                        message: ENDED_PARENT.into(),
                    });
                }
            })?;
        }
        Ok(())
    }
}

/// Why a YOLO request is refused or cancelled once the session that asked for it is gone.
pub(super) const ENDED_PARENT: &str = "The session that asked for this worker has ended.";

enum Step {
    Wait(Reading),
    Answer(Replier, Result<Value, String>),
}

fn tracing_error(e: &io::Error) {
    ox_host_service::errlog::record(&e.to_string());
}

/// Never let a resumed read or retried export include messages added after the handoff.
fn freeze(items: &mut Vec<handoff::SourceItem>, source: &HandoffSource) -> Result<(), String> {
    let count =
        match source.selection.items.last() {
            None => 0,
            Some(last) => {
                items.iter().position(|i| &i.id == last).ok_or(
                    "The original end of this handoff is no longer in the source transcript.",
                )? + 1
            }
        };
    items.truncate(count);
    Ok(())
}
/// One page of a handoff source for read_ox_thread.
fn read_source_page(
    src: &HandoffSource,
    timeline: &wire::Timeline,
    args: &ReadArgs,
    frozen: bool,
) -> Result<Value, String> {
    let mut items = if src.selection.includes_plans {
        handoff::handoff_items(timeline)
    } else {
        handoff::items(timeline)
    };
    if frozen {
        freeze(&mut items, src)?;
    }
    let view = |i: &handoff::SourceItem| {
        json!({
            "id": i.id,
            "turn": i.turn,
            "role": i.kind,
            "text": i.text,
            "carried": src.selection.items.contains(&i.id),
            "cut_in_handoff": src.selection.cut.contains(&i.id),
        })
    };
    let header = json!({
        "host": src.host,
        "session": src.session,
        "native_session": src.native,
        "title": src.title,
        "runtime": src.runtime,
        "route": src.route.label(),
        "total_items": items.len(),
        "omitted_ranges": src.selection.ranges,
        "cut_items": src.selection.cut,
    });
    if let Some(id) = &args.item {
        let item = items
            .iter()
            .find(|i| &i.id == id)
            .ok_or("No item with that ID in the source session.")?;
        return Ok(json!({"source": header, "items": [view(item)], "next_after": null}));
    }
    let start = match &args.after {
        Some(a) => {
            items
                .iter()
                .position(|i| &i.id == a)
                .ok_or("No item with that ID in the source session.")?
                + 1
        }
        None => 0,
    };
    let mut page = Vec::new();
    let mut bytes = 0;
    for item in &items[start.min(items.len())..] {
        if page.len() >= args.limit || (!page.is_empty() && bytes + item.text.len() > PAGE_BYTES) {
            break;
        }
        bytes += item.text.len();
        page.push(item);
    }
    let next = (start + page.len() < items.len()).then(|| page.last().map(|i| i.id.clone()));
    Ok(json!({
        "source": header,
        "items": page.into_iter().map(view).collect::<Vec<_>>(),
        "next_after": next.flatten(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::host_protocol::TimelineItem;

    fn timeline(n: usize) -> wire::Timeline {
        wire::Timeline {
            items: (0..n)
                .map(|i| TimelineItem {
                    turn: format!("t{i}"),
                    id: format!("i{i}"),
                    role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                    text: format!("text {i}"),
                    revision: 1,
                    detail: Value::Null,
                })
                .collect(),
            detail: Value::Null,
        }
    }

    #[test]
    fn read_pages_by_item_id_and_returns_whole_items() {
        let src = HandoffSource {
            history_broker: None,
            host: None,
            scope: Scope {
                project: "/p".into(),
                route: ox_desk_core::runtime::Route::CODEX_SUBSCRIPTION,
            },
            session: "s".into(),
            native: None,
            title: "t".into(),
            runtime: ox_desk_core::runtime::Runtime::Codex,
            route: ox_desk_core::runtime::Route::CODEX_SUBSCRIPTION,
            source_access: None,
            source_level: None,
            decision: Decision::Inherited,
            selection: handoff::SelectionRefs {
                items: vec!["i0".into(), "i9".into()],
                cut: vec!["i9".into()],
                ..Default::default()
            },
        };
        let t = timeline(10);
        let args = |item: Option<&str>, after: Option<&str>, limit| ReadArgs {
            item: item.map(Into::into),
            after: after.map(Into::into),
            limit,
        };
        let later = timeline(14);
        let frozen = read_source_page(&src, &later, &args(None, None, 100), true).unwrap();
        assert_eq!(frozen["source"]["total_items"], 10);
        assert_eq!(frozen["items"][9]["text"], "text 9");
        assert!(read_source_page(&src, &later, &args(Some("i11"), None, 1), true).is_err());
        assert!(read_source_page(&src, &timeline(2), &args(None, None, 1), true).is_err());
        let first = read_source_page(&src, &t, &args(None, None, 4), false).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 4);
        assert_eq!(first["items"][0]["carried"], true);
        assert_eq!(first["next_after"], "i3");
        let next = read_source_page(&src, &t, &args(None, Some("i3"), 100), false).unwrap();
        assert_eq!(next["items"][0]["id"], "i4");
        assert!(next["next_after"].is_null());
        let one = read_source_page(&src, &t, &args(Some("i9"), None, 1), false).unwrap();
        assert_eq!(one["items"][0]["text"], "text 9");
        assert_eq!(one["items"][0]["cut_in_handoff"], true);
        assert!(read_source_page(&src, &t, &args(Some("nope"), None, 1), false).is_err());
        assert!(read_source_page(&src, &t, &args(None, Some("nope"), 1), false).is_err());
    }
}
