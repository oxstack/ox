//! The source keeps ownership and delivery state. The runtime and its transcript
//! stay on the target. Network and Git work never run on the session actor.
use super::forward_queue::{Pending, path as queue_path};
use super::*;
use crate::workers::{Entry, Marks, Resolved, owns};
use ox_desk_core::modes::ModeRequest;
use ox_desk_core::origin::{LinkedParent, LinkedWorkerSession, LinkedWorkerSummary};
use ox_host_web::{
    linked_jobs::{self, JobRequest, JobResult, Operation},
    paired_hosts::{self, SavedHost},
    worker_checkout::Delivery,
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{collections::VecDeque, fs};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Job {
    pub(super) origin: Origin,
    host: String,
    pub(super) name: String,
    pairing: String,
    request: uuid::Uuid,
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    awaiting_start: bool,
    #[serde(default)]
    sync_notices: bool,
    status: String,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<LinkedWorkerSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ceiling: Option<Level>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actual_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<String>,
    #[serde(default)]
    recovered: bool,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changes: Option<ox_host_web::worker_result::Local>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changes_error: Option<String>,
    #[serde(default)]
    changes_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_branch: Option<String>,
}
impl Job {
    fn roster_entry(&self) -> LinkedWorkerSummary {
        LinkedWorkerSummary {
            job: self.origin.job_id,
            host: self.host.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            route: self.origin.route.clone(),
            status: self.status.clone(),
            ceiling: self.ceiling,
            actual_model: self.actual_model.clone(),
            target: self.target.clone(),
            phone_target: self.target.as_ref().and_then(|target| {
                ox_host_web::phone_link::session_target(&target.scope, &target.session).ok()
            }),
        }
    }

    fn observed_target(&self, source: &str, view: &Value) -> Option<LinkedWorkerSession> {
        // Only use the registered job's own lineage and actual session, never an
        // inventory match by title, a native session ID, or the source checkout.
        let linked: LinkedParent =
            serde_json::from_value(view.get("linked_parent")?.clone()).ok()?;
        if linked.host != source
            || linked.request_id != self.request.to_string()
            || linked.parent != self.origin.parent
        {
            return None;
        }
        let session = view["session"].as_str().filter(|id| !id.is_empty())?;
        let project = view["checkout"].as_str().map(PathBuf::from)?;
        if !project.is_absolute() {
            return None;
        }
        Some(LinkedWorkerSession {
            scope: Scope {
                project,
                route: self.origin.route.clone(),
            },
            session: session.to_owned(),
        })
    }

    pub(super) fn owned(&self, me: &Resolved) -> bool {
        owns(
            me,
            &Entry {
                origin: self.origin.clone(),
                marks: Marks::default(),
            },
        )
    }
    pub(super) fn preparing(&self) -> bool {
        self.status == "preparing"
    }
    pub(super) fn active(&self) -> bool {
        !matches!(
            self.status.as_str(),
            "completed" | "failed" | "cancelled" | "interrupted" | "declined" | "detached"
        )
    }
    pub(super) fn summary(&self) -> Value {
        self.decorate(
            json!({"job_id":self.origin.job_id,"status":self.status,"runtime":self.origin.runtime,
            "mode":self.origin.mode,"message":self.message,"can_steer":false,"owner":"host"}),
        )
    }
    fn decorate(&self, mut v: Value) -> Value {
        v["worker_host"] = json!({"id":self.host,"name":self.name});
        if let Some(c) = &self.checkout {
            v["snapshot_commit"] = json!(c);
        }
        if self.recovered {
            v["checkout_recovered"] = true.into();
        }
        if let Some(r) = &self.changes {
            v["worker_changes"] = json!(r);
        }
        if self.changes_pending {
            v["worker_changes_pending"] = true.into();
        }
        if let Some(error) = &self.changes_error {
            v["worker_changes_error"] = json!(error);
        }
        if self.cancel_requested {
            v["cancellation_pending"] = true.into();
        }
        v
    }
    fn collect_result(
        &self,
        host: &SavedHost,
        req: &JobRequest,
        view: &mut Value,
        returns: &Path,
    ) -> io::Result<()> {
        let object = view
            .as_object_mut()
            .ok_or_else(|| io::Error::other("Invalid result response."))?;
        // Only the parent may attach paths or ref names from its own checkout.
        object.remove("worker_changes");
        let Some(raw) = object.remove("change_result") else {
            return Ok(());
        };
        let report: ox_host_web::worker_result::Report = serde_json::from_value(raw)?;
        if self.checkout.as_deref() != Some(&report.summary.snapshot)
            || view["job_id"] != req.job
            || !view["status"].as_str().is_some_and(super::linked::terminal)
        {
            return Err(io::Error::other(
                "The worker result does not match the submitted snapshot or job.",
            ));
        }
        if let Some(previous) = &self.changes {
            if previous.report != report {
                return Err(io::Error::other("The worker returned conflicting results."));
            }
            return Ok(());
        }
        let project = self
            .project
            .as_deref()
            .ok_or_else(|| io::Error::other("This job has no source checkout."))?;
        let repo = self
            .repository
            .as_deref()
            .ok_or_else(|| io::Error::other("This job has no repository identity."))?;
        match ox_host_web::worker_result::collect(host, req, &report, project, repo, returns) {
            Ok(local) => view["worker_changes"] = json!(local),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::InvalidInput
                ) =>
            {
                view["worker_changes_error"] = json!(e.to_string())
            }
            Err(e) => {
                view["worker_changes_error"] = json!(format!(
                    "The worker finished. Receiving its changes will retry: {e}"
                ));
                view["worker_changes_pending"] = true.into();
            }
        }
        Ok(())
    }

    fn destination(&self, registry: &Path) -> io::Result<SavedHost> {
        let peers = paired_hosts::worker_links(registry)?;
        peers.into_iter().find(|host|host.id==self.host && host.pairing_id()==self.pairing).ok_or_else(|| {
            io::Error::new(io::ErrorKind::PermissionDenied,"The pairing that owns this worker was removed. Its session remains on the target.")
        })
    }
}

pub(super) fn roster(data: &crate::durable::Owned, scope: &Scope) -> BTreeMap<String, Value> {
    let mut roster: BTreeMap<String, Vec<LinkedWorkerSummary>> = BTreeMap::new();
    for job in data.forwarded.values() {
        let parent = &job.origin.parent;
        if parent.scope.as_ref() == Some(scope)
            && let Some(session) = &parent.session
        {
            roster
                .entry(session.clone())
                .or_default()
                .push(job.roster_entry());
        }
    }
    roster
        .into_iter()
        .map(|(id, jobs)| (id, json!(jobs)))
        .collect()
}

pub(super) fn restore_roster(data: &crate::durable::Owned, state: &mut ScopeState) {
    let mut roster = roster(data, &state.scope);
    for row in &mut state.sessions {
        if let Some(jobs) = roster.remove(&row.id) {
            row.detail["linked_workers"] = jobs;
        }
    }
}
#[derive(Default)]
pub(super) struct State {
    busy: HashSet<u64>,
    preparing: HashMap<u64, Arc<AtomicBool>>,
    next: HashMap<u64, Instant>,
    failures: HashMap<u64, u32>,
    queue: HashMap<u64, VecDeque<(ParentCall, Replier)>>,
}
pub(crate) enum Event {
    Prepared(u64, io::Result<(Delivery, Box<Operation>)>),
    Reply(u64, Kind, Option<Replier>, io::Result<JobResult>),
}
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Start,
    Status,
    Steer,
    Cancel,
}

pub(super) fn recover(ledger: &Ledger, state: &Path) -> io::Result<()> {
    // The owning engine has not started any return downloads yet.
    ox_host_web::worker_result::discard_stages(&state.join("linked-work/returns"), &[".return-"])?;
    let queue = state.join("linked-work/outgoing/requests");
    let entries = match fs::read_dir(queue) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let keep: HashSet<_> = ledger
        .data
        .forwarded
        .values()
        .filter(|j| j.awaiting_start)
        .map(|j| j.request.to_string())
        .collect();
    for e in entries {
        let e = e?;
        let name = e.file_name();
        let name = name.to_string_lossy();
        if e.file_type()?.is_dir()
            && (name.starts_with(".new-")
                || (uuid::Uuid::parse_str(&name).is_ok() && !keep.contains(&*name)))
        {
            fs::remove_dir_all(e.path())?;
        }
    }
    Ok(())
}
fn retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
    )
}
impl Engine {
    pub(super) fn forward_launch(
        &mut self,
        caller: Caller,
        mut start: WorkerStart,
        selector: &str,
    ) -> JobResult {
        start.validate()?;
        let (project, parent, source, external) = self.worker_source(&caller)?;
        let resolved =
            crate::workers::grant(source, external, start.mode_request(), start.runtime)?;
        let parent_ref = crate::workers::parent_ref(&caller, parent.clone());
        self.check_worker_cap(&parent_ref)?;
        let project = ox_desk_core::worker_checkout::resolve(
            &project,
            start.checkout.take().as_deref(),
            source == Some(Level::Yolo),
        )?;
        let peers = paired_hosts::worker_links(&self.state_dir.join("worker-links.json"))
            .map_err(|e| e.to_string())?;
        let host = peers
            .into_iter()
            .find(|h| h.id == selector)
            .ok_or_else(|| {
                format!("Worker host {selector:?} is no longer paired. See list_ox_hosts.")
            })?;
        let level = source.unwrap_or(crate::workers::TERMINAL_CEILING);
        if external && start.mode_request() == ModeRequest::Inherit {
            start.permission = Some(ModeRequest::Level(resolved.level));
        }
        // The target owns the operator's policy decision.
        start.yolo_confirmed = false;
        let job = self.new_job_id()?;
        let origin = crate::workers::origin(&caller, parent, &start, job, resolved);
        let saved = Job {
            origin,
            host: host.id.clone(),
            name: host.name.clone(),
            pairing: host.pairing_id().into(),
            request: uuid::Uuid::new_v4(),
            cursor: 0,
            awaiting_start: true,
            sync_notices: false,
            status: "preparing".into(),
            message: "Preparing the parent checkout.".into(),
            target: None,
            title: None,
            ceiling: None,
            actual_model: None,
            checkout: None,
            recovered: false,
            cancel_requested: false,
            changes: None,
            changes_error: None,
            changes_pending: false,
            source_branch: ox_host_service::checkout::results::source_branch(&project),
            project: Some(project.clone()),
            repository: ox_host_service::checkout::repository_id(&project),
        };
        let queue = queue_path(&self.state_dir, &saved.request);
        use crate::worker_placement::{RESULT_DEFAULTS, document, read_document, result_settings};
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")));
        let global = home
            .map(|dir| read_document(&dir.join("codeox.toml")))
            .transpose()?
            .flatten();
        let defaults = result_settings(
            global.as_ref().and_then(|d| d.get("workers")),
            RESULT_DEFAULTS,
        )?;
        let doc = document(&project)?;
        let (dest, max_age_days) =
            result_settings(doc.as_ref().and_then(|d| d.get("workers")), defaults)?;
        let destination = serde_json::from_value(json!(dest)).map_err(|e| e.to_string())?;
        Pending {
            policy: Some(ox_host_web::worker_result::Policy {
                destination,
                max_age_days,
            }),
            operation: {
                let brief_ceiling = ox_desk_core::memory::route_sensitivity_ceiling(&start.route())
                    .min(ox_memory::Sensitivity::Private);
                let project_id = ox_memory::canonical_project_id(&project);
                let memory_brief =
                    ox_memory::MemoryStore::open(ox_memory::store_path_for(&self.state_dir))
                        .tier1_brief(
                            Some(&project_id),
                            None,
                            brief_ceiling,
                            ox_memory::DEFAULT_BRIEF_CHARS,
                        )
                        .ok()
                        .flatten();
                let memory_brief_sensitivity = memory_brief.as_ref().map(|_| brief_ceiling);
                Operation::Start {
                    results: None,
                    memory_brief,
                    memory_brief_sensitivity,
                    parent: parent_ref,
                    level,
                    start: Box::new(start),
                }
            },
            project,
        }
        .save(&queue)
        .map_err(|e| e.to_string())?;
        let response = saved.summary();
        if let Err(e) = self.ledger.change(|d| {
            d.forwarded.insert(job, saved);
        }) {
            let _ = fs::remove_dir_all(&queue);
            return Err(e.to_string());
        }
        self.prepare_remote(job);
        self.publish_forwarded(job).map_err(|e| e.to_string())?;
        Ok(response)
    }

    fn publish_forwarded(&mut self, id: u64) -> io::Result<()> {
        let parent = &self.ledger.data.forwarded[&id].origin.parent;
        let Some((scope, session)) = parent.scope.clone().zip(parent.session.clone()) else {
            return Ok(());
        };
        if self.running.contains_key(&scope) {
            self.process_scope(&scope, vec![], true)?;
        } else if let Some(state) = self.cold.get_mut(&scope) {
            restore_roster(&self.ledger.data, state);
            if let Some(session) = state.sessions.iter().find(|row| row.id == session).cloned() {
                self.broadcast(&scope, vec![Change::Session { session }])?;
            }
        }
        Ok(())
    }
    fn prepare_remote(&mut self, id: u64) {
        let j = self.ledger.data.forwarded[&id].clone();
        let queue = queue_path(&self.state_dir, &j.request);
        let staging = self.state_dir.join("linked-work/outgoing");
        let registry = self.state_dir.join("worker-links.json");
        let source = self.host_id.to_string();
        let tx = self.tx.clone();
        self.forward.busy.insert(id);
        let stop = Arc::new(AtomicBool::new(false));
        self.forward.preparing.insert(id, stop.clone());
        thread::spawn(move || {
            let prepare = || -> io::Result<_> {
                let mut p = Pending::load(&queue)?;
                let host = j.destination(&registry)?;
                let Operation::Start {
                    start,
                    level,
                    results,
                    memory_brief,
                    memory_brief_sensitivity,
                    ..
                } = &mut p.operation
                else {
                    return Err(io::Error::other("Invalid queued worker request."));
                };
                if !queue.join("snapshot").is_dir() {
                    let launch = ox_host_web::inventory::check_launch(
                        &host, &source, &p.project, start, *level,
                    )?;
                    // A receiver from before the brief existed rejects the
                    // optional fields, so omit them instead of failing the launch.
                    if !launch.memory_brief {
                        *memory_brief = None;
                        *memory_brief_sensitivity = None;
                    }
                    if launch.linked_write {
                        *results = Some(p.policy.clone().unwrap_or_default());
                    }
                    ox_desk_core::state_file::write_json(&queue.join("start.json"), &p)?;
                }
                let sender = ox_host_web::worker_checkout::Sender {
                    host: &host,
                    source: &source,
                    project: &p.project,
                    staging: &staging,
                    job: id,
                    request_id: &j.request.to_string(),
                };
                let delivery = sender.run(Some(&queue), || stop.load(Ordering::Acquire))?;
                Ok((delivery, Box::new(p.operation)))
            };
            let _ = tx.send(Message::Outbound(Box::new(Event::Prepared(id, prepare()))));
        });
    }
    pub(super) fn dispatch_parent_call(
        &mut self,
        caller: &Caller,
        call: ParentCall,
        reply: Replier,
    ) -> io::Result<()> {
        let id = match &call {
            ParentCall::Status { job_id }
            | ParentCall::Steer { job_id, .. }
            | ParentCall::Cancel { job_id }
            | ParentCall::Answer { job_id, .. } => Some(*job_id),
            _ => None,
        };
        let Some((id, j)) = id.and_then(|id| self.ledger.data.forwarded.get(&id).map(|j| (id, j)))
        else {
            let result = self.parent_call(caller, call);
            return self.answer(reply, result);
        };
        if !self.resolve(caller).is_ok_and(|me| j.owned(&me)) {
            return self.answer(
                reply,
                Err("Unknown worker job for this parent session.".into()),
            );
        }
        if j.status == "detached" {
            return self.answer(
                reply,
                match call {
                    ParentCall::Status { .. } | ParentCall::Cancel { .. } => Ok(j.summary()),
                    _ => Err(j.message.clone()),
                },
            );
        }
        if matches!(call, ParentCall::Answer { .. }) {
            return self.answer(
                reply,
                Err(format!("Answer this worker’s requests on {}.", j.name)),
            );
        }
        if j.status == "preparing" || (j.checkout.is_none() && !j.active()) {
            let result = match call {
                ParentCall::Cancel { .. } if j.active() => {
                    if let Some(stop) = self.forward.preparing.get(&id) {
                        stop.store(true, Ordering::Release);
                    }
                    self.record_forward(id, "cancelled", "Cancelled before the worker started.")?;
                    Ok(self.ledger.data.forwarded[&id].summary())
                }
                ParentCall::Steer { .. } => Err("The remote worker has not started.".into()),
                _ => Ok(j.summary()),
            };
            return self.answer(reply, result);
        }
        if matches!(call, ParentCall::Cancel { .. }) && j.active() {
            self.ledger.change(|d| {
                d.forwarded.get_mut(&id).unwrap().cancel_requested = true;
            })?;
        }
        if self.forward.busy.contains(&id) {
            if matches!(call, ParentCall::Status { .. }) {
                return self.answer(reply, Ok(self.ledger.data.forwarded[&id].summary()));
            }
            self.forward
                .queue
                .entry(id)
                .or_default()
                .push_back((call, reply));
        } else {
            self.remote_control(id, call, Some(reply));
        }
        Ok(())
    }
    fn record_forward(&mut self, id: u64, status: &str, message: &str) -> io::Result<()> {
        self.ledger.change(|d| {
            let j = d.forwarded.get_mut(&id).expect("owned remote job");
            j.status = status.into();
            j.message = message.into();
            if !j.active() {
                j.awaiting_start = false;
                j.sync_notices = false;
                j.changes_pending = false;
                j.cancel_requested = false;
            }
            if !matches!(status, "starting" | "preparing") {
                d.worker_notices.observe_on(
                    &j.origin,
                    status,
                    &format!("Worker on {}. {message}", j.name),
                    Some(&j.name),
                );
            }
        })?;
        self.publish_forwarded(id)
    }
    fn remote_control(&mut self, id: u64, call: ParentCall, reply: Option<Replier>) {
        let kind = match &call {
            ParentCall::Cancel { .. } => Kind::Cancel,
            ParentCall::Steer { .. } => Kind::Steer,
            _ => Kind::Status,
        };
        self.send_remote(id, Operation::Control { call }, kind, reply);
    }
    fn send_remote(&mut self, id: u64, op: Operation, kind: Kind, reply: Option<Replier>) {
        let j = self.ledger.data.forwarded[&id].clone();
        let registry = self.state_dir.join("worker-links.json");
        let req = JobRequest {
            source: self.host_id.to_string(),
            target: j.host.clone(),
            request_id: j.request.to_string(),
            job: id,
            acknowledged: j.cursor,
            operation: op,
        };
        self.forward.busy.insert(id);
        let tx = self.tx.clone();
        let returns = self.state_dir.join("linked-work/returns");
        thread::spawn(move || {
            let result = j.destination(&registry).and_then(|host| {
                let mut result = linked_jobs::send(&host, &req)?;
                if let Ok(view) = &mut result {
                    j.collect_result(&host, &req, view, &returns)?;
                }
                Ok(result)
            });
            let _ = tx.send(Message::Outbound(Box::new(Event::Reply(
                id, kind, reply, result,
            ))));
        });
    }
    pub(super) fn maintain_worker_results(&mut self) {
        if self
            .result_maintenance
            .as_ref()
            .is_some_and(|t| !t.is_finished())
        {
            return;
        }
        let root = self.state_dir.join("linked-work");
        let registry = self.state_dir.join("worker-links.json");
        let source = self.host_id.to_string();
        let jobs: Vec<_> = self
            .ledger
            .data
            .forwarded
            .values()
            .filter(|j| {
                j.changes
                    .as_ref()
                    .is_some_and(|r| r.report.remote_ref.is_some())
            })
            .cloned()
            .collect();
        self.result_maintenance = Some(thread::spawn(move || {
            for j in jobs {
                let Some(local) = &j.changes else { continue };
                let marker = root
                    .join("returns")
                    .join(j.request.to_string())
                    .join("cleanup-ack");
                if marker.exists() {
                    continue;
                }
                let Some(project) = &j.project else { continue };
                let Some(branch) = &j.source_branch else {
                    continue;
                };
                if ox_host_service::checkout::repository_id(project) != j.repository
                    || !ox_host_service::checkout::results::merged(
                        project,
                        branch,
                        &local.report.summary.commit,
                    )
                    .unwrap_or(false)
                {
                    continue;
                }
                let req = JobRequest {
                    source: source.clone(),
                    target: j.host.clone(),
                    request_id: j.request.to_string(),
                    job: j.origin.job_id,
                    acknowledged: j.cursor,
                    operation: Operation::ReleaseResult {
                        commit: local.report.summary.commit.clone(),
                    },
                };
                if j.destination(&registry)
                    .and_then(|h| linked_jobs::send(&h, &req))
                    .is_ok_and(|v| v.is_ok_and(|reply| reply["released"] == true))
                {
                    // A lost local marker only repeats the idempotent release.
                    let _ = fs::write(marker, []);
                }
            }
            if let Err(e) = ox_host_web::worker_result::sweep(&root.join("jobs")) {
                eprintln!("ox host: could not inspect worker refs for cleanup: {e}");
            }
        }));
    }

    pub(super) fn forward_deadline(&self) -> Option<Instant> {
        self.ledger
            .data
            .forwarded
            .iter()
            .filter(|(id, j)| {
                (j.active() || j.sync_notices || j.changes_pending)
                    && !self.forward.busy.contains(id)
            })
            .map(|(id, _)| {
                self.forward
                    .next
                    .get(id)
                    .copied()
                    .unwrap_or_else(Instant::now)
            })
            .min()
    }
    pub(super) fn poll_forwarded(&mut self, now: Instant) -> io::Result<()> {
        let due: Vec<_> = self
            .ledger
            .data
            .forwarded
            .iter()
            .filter(|(id, j)| {
                (j.active() || j.sync_notices || j.changes_pending)
                    && !self.forward.busy.contains(id)
                    && self.forward.next.get(id).is_none_or(|at| *at <= now)
            })
            .map(|(id, j)| (*id, j.cancel_requested))
            .collect();
        for (job_id, cancel) in due {
            let job = &self.ledger.data.forwarded[&job_id];
            if job.status == "preparing" {
                self.prepare_remote(job_id);
                continue;
            }
            if job.awaiting_start && !cancel {
                match Pending::load(&queue_path(&self.state_dir, &job.request)) {
                    Ok(p) => self.send_remote(job_id, p.operation, Kind::Start, None),
                    Err(e) => self.record_forward(
                        job_id,
                        "failed",
                        &format!("Cannot recover this queued launch: {e}"),
                    )?,
                }
                continue;
            }
            self.remote_control(
                job_id,
                if cancel {
                    ParentCall::Cancel { job_id }
                } else {
                    ParentCall::Status { job_id }
                },
                None,
            );
        }
        Ok(())
    }
    fn accept_remote_reply(&mut self, id: u64, kind: Kind, view: Value) -> io::Result<JobResult> {
        self.forward.failures.remove(&id);
        self.forward
            .next
            .insert(id, Instant::now() + Duration::from_secs(4));
        let Some(status) = view["status"].as_str().filter(|_| view["job_id"] == id) else {
            return Ok(Err("The worker host returned an invalid job status.".into()));
        };
        let message = view["message"]
            .as_str()
            .or_else(|| view["result"].as_str())
            .unwrap_or_default();
        let (status, message) = if status == "not_started" {
            ("interrupted", "The target has no launch for this worker.")
        } else {
            (status, message)
        };
        let notices = match view
            .get("linked_notices")
            .cloned()
            .map(serde_json::from_value::<Vec<crate::worker_notices::LinkedNotice>>)
            .transpose()
        {
            Ok(v) => v.unwrap_or_default(),
            Err(_) => {
                return Ok(Err(
                    "The target returned incompatible worker notices.".into()
                ));
            }
        };
        let job = &self.ledger.data.forwarded[&id];
        let target = job.observed_target(&self.host_id.to_string(), &view);
        let before = job.roster_entry();
        if status == "interrupted" && view["status"] == "not_started" {
            if job.awaiting_start && job.active() {
                return Ok(Ok(job.summary()));
            }
            if !job.active() {
                return Ok(Ok(job.summary()));
            }
        }
        let local = view
            .get("worker_changes")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        self.ledger.change(|d| {
            let j = d.forwarded.get_mut(&id).unwrap();
            let recovered = j.changes_error.is_some() && j.changes.is_none() && local.is_some();
            j.changes_pending = view["worker_changes_pending"] == true;
            if local.is_some() {
                j.changes_error = None;
                j.changes = local;
            }
            if let Some(error) = view["worker_changes_error"].as_str() {
                j.changes_error = Some(error.into());
            }
            let old = j.cursor;
            for n in notices {
                if n.job == id && n.id > j.cursor {
                    let mut preview = format!("Worker on {}. ", j.name);
                    if super::linked::terminal(&n.status) {
                        if let Some(local) = &j.changes {
                            preview.push_str(&format!(
                                "{} {}. ",
                                local.reference, local.report.summary.diffstat
                            ));
                            if let Some(note) = &local.report.note {
                                preview.push_str(note);
                                preview.push(' ');
                            }
                        }
                        if let Some(error) = &j.changes_error {
                            preview.push_str(error);
                            preview.push(' ');
                        }
                    }
                    preview.push_str(&n.preview);
                    d.worker_notices
                        .observe_on(&j.origin, &n.status, &preview, Some(&j.name));
                    j.cursor = n.id;
                }
            }
            if recovered
                && old == j.cursor
                && let Some(changes) = &j.changes
            {
                let message = format!(
                    "Worker on {}. {} {}. {}",
                    j.name,
                    changes.reference,
                    changes.report.summary.diffstat,
                    changes.report.note.as_deref().unwrap_or("")
                );
                d.worker_notices
                    .observe_on(&j.origin, "changes_ready", &message, Some(&j.name));
            }
            j.sync_notices = kind == Kind::Start || j.cursor != old || view["more_notices"] == true;
            j.awaiting_start = false;
            j.status = status.into();
            j.message = crate::worker_notices::excerpt(message);
            if let Some(target) = target
                && j.target.as_ref().is_none_or(|previous| previous == &target)
            {
                j.target = Some(target);
                if let Some(title) = view["title"]
                    .as_str()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    j.title = Some(title.chars().take(160).collect());
                }
                if let Ok(ceiling) = serde_json::from_value::<Level>(view["access_ceiling"].clone())
                {
                    j.ceiling = Some(ceiling);
                }
                if let Some(model) = view["actual_model"].as_str().filter(|m| !m.is_empty()) {
                    j.actual_model = Some(model.chars().take(128).collect());
                }
            }
            if !j.active() {
                j.cancel_requested = false;
            }
            if status == "interrupted" && view["linked_notices"].is_null() {
                d.worker_notices
                    .observe_on(&j.origin, status, &j.message, Some(&j.name));
            }
        })?;
        if self.ledger.data.forwarded[&id].roster_entry() != before {
            self.publish_forwarded(id)?;
        }
        if view["worker_changes_pending"] == true && view["worker_changes_error"].is_string() {
            self.forward
                .next
                .insert(id, Instant::now() + Duration::from_secs(60));
        }
        let mut view = view;
        if let Some(o) = view.as_object_mut() {
            o.remove("linked_notices");
            o.remove("more_notices");
        }
        Ok(Ok(self.ledger.data.forwarded[&id].decorate(view)))
    }
    fn backoff(&mut self, id: u64) {
        let failures = self.forward.failures.entry(id).or_default();
        *failures = (*failures + 1).min(5);
        self.forward.next.insert(
            id,
            Instant::now() + Duration::from_secs(2u64.pow(*failures)),
        );
    }
    pub(super) fn outbound(&mut self, event: Event) -> io::Result<()> {
        let id = match event {
            Event::Prepared(id, result) => {
                self.forward.busy.remove(&id);
                self.forward.preparing.remove(&id);
                let queue = queue_path(&self.state_dir, &self.ledger.data.forwarded[&id].request);
                if self.ledger.data.forwarded[&id].status == "cancelled" {
                    let _ = fs::remove_dir_all(&queue);
                    return Ok(());
                }
                match result {
                    Ok((delivery, start)) => {
                        self.ledger.change(|d| {
                            let j = d.forwarded.get_mut(&id).unwrap();
                            j.checkout = Some(delivery.receipt.commit);
                            j.recovered = delivery.recovered;
                            j.status = "starting".into();
                            j.message = "Starting on the worker host.".into();
                        })?;
                        self.publish_forwarded(id)?;
                        let _ = fs::remove_dir_all(queue.join("snapshot"));
                        self.send_remote(id, *start, Kind::Start, None);
                    }
                    Err(e) if retryable(&e) && queue.join("snapshot").is_dir() => {
                        self.backoff(id);
                    }
                    Err(e) => {
                        self.record_forward(id, "failed", &e.to_string())?;
                        let _ = fs::remove_dir_all(queue);
                    }
                }
                return self.deliver_worker_notices();
            }
            Event::Reply(id, kind, reply, result) => {
                self.forward.busy.remove(&id);
                let result = match result {
                    Ok(Ok(view)) => self.accept_remote_reply(id, kind, view)?,
                    Ok(Err(error)) => {
                        if kind == Kind::Start {
                            self.record_forward(id, "failed", &error)?;
                        }
                        self.forward
                            .next
                            .insert(id, Instant::now() + Duration::from_secs(15));
                        Err(error)
                    }
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                        self.record_forward(id,"detached","The worker link is no longer available. Open the receiving host to inspect or stop the worker.")?;
                        Ok(self.ledger.data.forwarded[&id].summary())
                    }
                    Err(error) => {
                        self.backoff(id);
                        let mut view = self.ledger.data.forwarded[&id].summary();
                        view["connection"] = "unavailable".into();
                        view["connection_error"] = error.to_string().into();
                        if kind == Kind::Steer {
                            Err(format!("The target did not confirm the steer: {error}"))
                        } else {
                            Ok(view)
                        }
                    }
                };
                let j = &self.ledger.data.forwarded[&id];
                if !j.awaiting_start {
                    let _ = fs::remove_dir_all(queue_path(&self.state_dir, &j.request));
                }
                if let Some(reply) = reply {
                    self.answer(reply, result)?;
                }
                id
            }
        };
        if let Some((call, reply)) = self
            .forward
            .queue
            .get_mut(&id)
            .and_then(VecDeque::pop_front)
        {
            self.remote_control(id, call, Some(reply));
        }
        self.deliver_worker_notices()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::runtime::{Route, Runtime};

    fn saved_job(job: u64, scope: &Scope, session: &str) -> Job {
        // An older ledger has no receiving session, ceiling or actual model.
        serde_json::from_value(json!({
            "origin": {
                "relation": "worker", "parent": {
                    "project": scope.project, "scope": scope, "session": session, "runtime": "codex"
                },
                "launch": "operator:fixture", "by_operator": true, "runtime": "codex",
                "route": Route::codex_api("fixture/demo".into()), "mode": "session", "job_id": job,
                "requested": "read", "ceiling": "write", "source": null,
            },
            "host": "studio", "name": "Studio", "pairing": "fixture-pairing",
            "request": uuid::Uuid::new_v4(), "status": "starting", "message": "Starting"
        }))
        .unwrap()
    }

    #[test]
    fn linked_roster_updates_only_the_recorded_parent_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap();
        let scope = Scope {
            project: project.clone(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let other = Scope {
            project: project.clone(),
            route: Route::codex_api("other/demo".into()),
        };
        let config = RuntimeConfig {
            ox: "/usr/bin/false".into(),
            path: "/bin".into(),
            desktop: project.clone(),
        };
        let (tx, _rx) = std::sync::mpsc::sync_channel(8);
        let mut engine = Engine::open(config.clone(), dir.path(), tx.clone()).unwrap();
        engine
            .ledger
            .change(|data| {
                for scope in [&scope, &other] {
                    let mut state = ScopeState {
                        scope: scope.clone(),
                        connection: "ready".into(),
                        capabilities: vec![],
                        sessions: vec![],
                        asks: vec![],
                        transcripts: Default::default(),
                        detail: json!({}),
                    };
                    for id in ["lead", "sibling"] {
                        state.sessions.push(wire::SessionView {
                            id: id.into(),
                            title: id.into(),
                            parent: None,
                            status: "idle".into(),
                            updated_at: 0,
                            capabilities: vec![],
                            detail: json!({"owned": true}),
                        });
                    }
                    data.scopes.push(SavedScope::from_state(&state));
                }
                data.forwarded.insert(11, saved_job(11, &scope, "lead"));
                data.forwarded.insert(12, saved_job(12, &scope, "lead"));
                data.forwarded.insert(13, saved_job(13, &other, "lead"));
            })
            .unwrap();
        drop(engine);
        let mut engine = Engine::open(config.clone(), dir.path(), tx.clone()).unwrap();
        let project = project.join("studio-checkout");
        let job = &engine.ledger.data.forwarded[&11];
        let mut view = json!({
            "job_id": 11, "status": "running", "checkout": project, "session": "studio-run",
            "title": "Review retry edge cases", "actual_model": "synthetic-model",
            "runtime": Runtime::Codex, "access_ceiling": "read",
            "linked_parent": {
                "host": engine.host_id, "device": "linked-device", "request_id": job.request,
                "parent": job.origin.parent, "verified_level": "write", "link_ceiling": "read"
            },
        });
        let mut mistaken = view.clone();
        mistaken["linked_parent"]["parent"]["session"] = "sibling".into();
        assert!(
            job.observed_target(&engine.host_id.to_string(), &mistaken)
                .is_none()
        );
        assert!(
            engine
                .accept_remote_reply(11, Kind::Start, view.clone())
                .unwrap()
                .is_ok()
        );
        view["status"] = "completed".into();
        assert!(
            engine
                .accept_remote_reply(11, Kind::Status, view)
                .unwrap()
                .is_ok()
        );
        for engine in [engine, Engine::open(config, dir.path(), tx).unwrap()] {
            let rows = &engine.cold[&scope].sessions;
            let workers =
                rows.iter().find(|row| row.id == "lead").unwrap().detail["linked_workers"].clone();
            assert_eq!(workers.as_array().unwrap().len(), 2);
            assert_eq!(workers[0]["status"], "completed");
            assert_eq!(workers[0]["host"], "studio");
            assert_eq!(workers[0]["title"], "Review retry edge cases");
            assert_eq!(workers[0]["target"]["scope"]["project"], json!(project));
            assert_eq!(workers[0]["target"]["session"], "studio-run");
            assert_eq!(workers[0]["actual_model"], "synthetic-model");
            assert_eq!(workers[0]["ceiling"], "read");
            assert!(
                workers[0]["phone_target"]
                    .as_str()
                    .unwrap()
                    .starts_with("studio-run~")
            );
            assert_eq!(workers[1]["status"], "starting");
            assert!(workers[1]["target"].is_null());
            assert!(
                rows.iter().find(|row| row.id == "sibling").unwrap().detail["linked_workers"]
                    .is_null()
            );
            assert_eq!(
                engine.cold[&other].sessions[0].detail["linked_workers"][0]["job"],
                13
            );
            assert!(catalog_row(&rows[0]).detail["linked_workers"].is_null());
        }
    }
}
