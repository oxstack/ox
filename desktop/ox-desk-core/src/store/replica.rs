//! In-memory presentation state for a subscribed client. Native I/O exists only
//! on the publishing store. The caller provides one ordered, nonblocking writer.
use super::*;
use crate::{
    actions::{Action, Effect},
    asks::Decision,
    host_protocol::{Change, ScopeState, Server, SessionResult, SessionView, TimelineItem},
    runtime::{Access, RequestId, Route},
    slash::Capabilities,
    transcript::{DeltaTarget, item::Item},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
type Writer = Box<dyn Fn(Action, Option<String>) -> io::Result<String> + Send + Sync>;
pub struct Remote {
    writer: Writer,
    pub(super) scope_caps: Vec<String>,
    caps: HashMap<String, Vec<String>>,
    tokens: HashMap<RequestId, String>,
    pub(super) routes: HashMap<String, Route>,
    pub(super) access: HashMap<String, Access>,
    pub(super) changing: HashSet<String>,
    pub(super) unconfirmed_options: HashMap<String, String>,
    pub(super) commands: HashMap<String, Capabilities>,
    pub(super) sending: HashSet<String>,
    pub(super) schema: bool,
    pub(super) options: HashMap<String, routes_thread::Options>,
    pub(super) draft_access: Option<Access>,
    /// The newest host draft restore applied per session.
    restored_seen: HashMap<String, String>,
    /// Input sent but not yet acknowledged by the host: the owner, the draft it came from
    /// and how many identical user messages the transcript held when it was sent.
    pending: HashMap<String, (Option<String>, Option<compose::Draft>, usize)>,
    /// Input a replacement replica got back from its predecessor because the host never
    /// acknowledged it. The host may still have received it, so it waits in the composer
    /// until the transcript shows it (then it is cleared) and is never sent again for
    /// the user.
    unconfirmed: Vec<Unconfirmed>,
    pub(super) origins: HashMap<String, crate::origin::Origin>,
    /// The runtime-confirmed model each session answered with, when the host has
    /// observed one. Kept apart from the origin stamp, which records what was
    /// requested before the first turn.
    /// Host marks on a session: handoffs from it and its YOLO grant.
    pub(super) marks: HashMap<String, Value>,
    results: HashMap<String, SessionResult>,
    /// The request that opens the session this client is waiting for. Only its
    /// own `Opened` selects, and only its own `Error` stops the wait.
    pub(super) await_new: Option<String>,
    /// Rows the catalog held before a fresh snapshot replaced it, and the
    /// loaded length at the last page request made to get back to them.
    pub(super) restore: Option<(usize, Option<usize>)>,
}
struct Unconfirmed {
    owner: Option<String>,
    text: String,
    attachments: Vec<Option<String>>,
    seen: usize,
}
impl Remote {
    pub(super) fn can(&self, id: &str, cap: &str) -> bool {
        self.caps
            .get(id)
            .is_some_and(|c| c.iter().any(|v| v == cap))
    }
}
fn bits<T: DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}
pub fn row(view: &SessionView) -> Option<ThreadRow> {
    let mut row: ThreadRow = bits(&view.detail["row"])?;
    row.parent_thread_id = view.parent.clone().or(row.parent_thread_id);
    row.worker = view.detail["worker"] == true;
    row.generated_title = bits(&view.detail["generated"]);
    row.unread = view.detail["unread"] == true;
    row.activity_order = view.detail["order"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    Some(row)
}
impl Store {
    pub fn remote_client(
        profile: Profile,
        send: impl Fn(Action, Option<String>) -> io::Result<String> + Send + Sync + 'static,
    ) -> Self {
        let remote = Remote {
            writer: Box::new(send),
            scope_caps: vec![],
            caps: HashMap::new(),
            tokens: HashMap::new(),
            routes: HashMap::new(),
            access: HashMap::new(),
            commands: HashMap::new(),
            changing: HashSet::new(),
            unconfirmed_options: HashMap::new(),
            sending: HashSet::new(),
            schema: false,
            draft_access: None,
            restored_seen: HashMap::new(),
            options: HashMap::new(),
            await_new: None,
            restore: None,
            pending: HashMap::new(),
            unconfirmed: vec![],
            origins: HashMap::new(),
            marks: HashMap::new(),
            results: HashMap::new(),
        };
        Self::with_link(profile, Link::Host(Box::new(remote)))
    }
    /// The host's immutable lineage stamp for a worker session or its fork.
    pub fn origin(&self, id: &str) -> Option<&crate::origin::Origin> {
        self.remote()?.origins.get(id)
    }
    pub fn last_result(&self, id: &str) -> Option<&SessionResult> {
        if let Some(remote) = self.remote() {
            remote.results.get(id)
        } else {
            self.transcript(id)?.last_result.as_ref()
        }
    }
    /// The model the runtime itself confirmed for a session, once it has answered.
    /// The launch stamp keeps the requested choice; this is what actually ran.
    pub fn actual_model(&self, id: &str) -> Option<&str> {
        self.session_mark(id, "worker_model")?["actual_model"].as_str()
    }
    /// A host mark on a session: `handed_off_to`, `handoff_attempts`,
    /// `yolo_grant`, `yolo_children` or `worker_model`.
    pub fn session_mark(&self, id: &str, key: &str) -> Option<&Value> {
        self.remote()?
            .marks
            .get(id)
            .and_then(|m| m.get(key))
            .filter(|v| !v.is_null())
    }
    pub fn is_host_client(&self) -> bool {
        self.remote().is_some()
    }
    pub(crate) fn remote(&self) -> Option<&Remote> {
        if let Link::Host(r) = &self.link {
            Some(r)
        } else {
            None
        }
    }
    pub(super) fn remote_mut(&mut self) -> &mut Remote {
        match &mut self.link {
            Link::Host(r) => r,
            _ => unreachable!("remote state requires a host subscription"),
        }
    }
    /// A local confirmation must still refer to the exact incarnation the user reviewed.
    pub fn command_review_token(&self, id: &RequestId) -> Option<&str> {
        self.remote()?.tokens.get(id).map(String::as_str)
    }
    pub fn decline_reviewed_command(&self, id: &RequestId, token: &str) -> io::Result<String> {
        let decline = self
            .asks()
            .iter()
            .find(|ask| &ask.id == id)
            .and_then(|ask| ask.command.as_ref())
            .filter(|_| self.command_review_token(id) == Some(token))
            .ok_or_else(|| {
                io::Error::other("The command request changed or was already answered.")
            })?
            .decline;
        (self.remote().expect("verified host token").writer)(
            Action::AnswerAsk(id.clone(), decline),
            Some(token.to_owned()),
        )
    }
    pub(crate) fn forward(&self, action: Action) -> io::Result<()> {
        let r = self
            .remote()
            .ok_or_else(|| io::Error::other("No host connection"))?;
        let token = match &action {
            Action::AnswerAsk(id, _) | Action::AnswerQuestions(id, _) => Some(
                r.tokens
                    .get(id)
                    .cloned()
                    .ok_or_else(|| io::Error::other("This request has already been resolved"))?,
            ),
            _ => None,
        };
        (r.writer)(action, token).map(|_| ())
    }
    pub(super) fn pending_input(&self, owner: Option<&str>) -> bool {
        self.remote()
            .is_some_and(|r| r.pending.values().any(|(id, ..)| id.as_deref() == owner))
    }
    fn send_input(
        &mut self,
        action: Action,
        owner: Option<String>,
        from_draft: bool,
    ) -> io::Result<String> {
        let id = (self.remote().unwrap().writer)(action, None)?;
        let draft = if from_draft {
            self.drafts.remove(&owner)
        } else {
            None
        };
        let seen = draft
            .as_ref()
            .zip(owner.as_deref().and_then(|o| self.transcripts.get(o)))
            .map_or(0, |(d, t)| {
                let (text, attachments) = d.signature();
                t.user_messages(&text, &attachments)
            });
        self.remote_mut()
            .pending
            .insert(id.clone(), (owner, draft, seen));
        Ok(id)
    }
    /// Send an action that has no approval token and return its request id.
    fn forward_id(&self, action: Action) -> io::Result<String> {
        let r = self
            .remote()
            .ok_or_else(|| io::Error::other("No host connection"))?;
        (r.writer)(action, None)
    }
    fn remote_command(
        &mut self,
        owner: Option<String>,
        text: String,
        from_draft: bool,
    ) -> io::Result<Effect> {
        use crate::slash::Action as Command;
        match self.parse_command(owner.as_deref(), &text) {
            Some(Command::New) => {
                self.dispatch_remote(Action::NewDraft)?;
                if from_draft {
                    self.edit(owner.as_deref(), String::new());
                }
            }
            Some(Command::Resume) => {
                if from_draft {
                    self.edit(owner.as_deref(), String::new());
                }
                return Ok(Effect::FocusSearch);
            }
            Some(Command::Terminal) => self.forward(Action::TerminalText(owner, text))?,
            Some(Command::Hint(message)) => return Err(io::Error::other(message)),
            command => {
                let opens = owner.is_none() && command.is_some_and(|c| c.starts_thread());
                let id = self.send_input(
                    Action::PaletteCommand(owner.clone(), text),
                    owner,
                    from_draft,
                )?;
                if opens {
                    self.remote_mut().await_new = Some(id);
                }
            }
        }
        Ok(Effect::Applied)
    }
    fn select_replica(&mut self, id: &str) {
        self.selected = Some(id.to_owned());
        {
            let t = self.transcripts.entry(id.to_owned()).or_default();
            if !t.is_ready() {
                t.loading = true;
            }
        }
        self.changed.transcript(id);
    }
    pub(crate) fn dispatch_remote(&mut self, action: Action) -> io::Result<Effect> {
        match action {
            Action::DraftEdit(id, text) => self.edit(id.as_deref(), text),
            Action::ForkThread(_) => {
                let id = self.forward_id(action)?;
                self.remote_mut().await_new = Some(id);
            }
            Action::NewDraft => {
                self.selected = None;
                self.remote_mut().await_new = None;
                self.forward(Action::NewDraft)?;
            }
            Action::SelectThread(ref id) => {
                let row = self.catalog_row(id).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "Thread is outside this catalog")
                })?;
                self.check_route(row)?;
                let id = id.clone();
                self.forward(action)?;
                self.select_replica(&id);
                self.remote_mut().await_new = None;
            }
            Action::OpenCatalogRow(ref row) | Action::ConfirmAliasAndResume(ref row) => {
                if !row.worker {
                    self.check_route(row)?;
                }
                let id = row.id.clone();
                self.forward(action)?;
                self.select_replica(&id);
                self.remote_mut().await_new = None;
            }
            Action::Submit(owner) => {
                if !self.can_send(owner.as_deref()) || self.sending(owner.as_deref()) {
                    return Err(io::Error::other("Wait for the thread to be ready"));
                }
                if self.draft(owner.as_deref()).trim().is_empty()
                    && self.draft_images(owner.as_deref()).is_empty()
                {
                    return Err(io::Error::other("Enter a message"));
                }
                if let Some(warning) = self.image_model_warning(owner.as_deref()) {
                    return Err(io::Error::other(warning));
                }

                let text = self.draft(owner.as_deref()).to_string();
                let images = self.draft_images(owner.as_deref());
                let request = if images.is_empty() {
                    Action::SendText(owner.clone(), text)
                } else {
                    Action::SendImages(owner.clone(), text, images.to_vec())
                };
                let opens = owner.is_none();
                let id = self.send_input(request, owner, true)?;
                if opens {
                    self.remote_mut().await_new = Some(id);
                }
            }
            Action::ExecuteCommand(owner) => {
                let text = self.draft(owner.as_deref()).to_owned();
                return self.remote_command(owner, text, true);
            }
            Action::PaletteCommand(owner, text) => return self.remote_command(owner, text, false),
            Action::ModelOptions(_, routes_thread::Choice::Model(ref model))
                if !self.same_model_route(model) =>
            {
                return Err(io::Error::other(
                    "Open a new session on this model’s billing route.",
                ));
            }
            Action::ModelOptions(Some(ref id), _) => {
                let id = id.clone();
                let receipt = (self.remote().unwrap().writer)(action, None)?;
                self.remote_mut().unconfirmed_options.insert(receipt, id);
            }
            Action::Terminal(owner) => {
                let text = self.draft(owner.as_deref()).to_owned();
                self.forward(Action::TerminalText(owner, text))?;
            }
            Action::DismissNotice => {
                self.notice = None;
                self.forward(action)?;
            }
            Action::InjectHandoff(..) => {
                let id = self.forward_id(action)?;
                self.remote_mut().await_new = Some(id);
            }
            action => self.forward(action)?,
        }
        Ok(Effect::Applied)
    }
    pub(super) fn apply_host(&mut self, frame: Server) -> io::Result<()> {
        if self.remote().is_none() {
            return Err(io::Error::other("Unexpected client frame on runtime store"));
        }
        match frame {
            Server::Handoff {
                argv,
                scope,
                thread,
                command,
                id,
            } if scope.project == self.profile.project && scope.route == self.profile.route => {
                if self.remote().unwrap().await_new.as_deref() == Some(&id) {
                    self.remote_mut().await_new = None;
                }
                self.handoff_ready = Some(commands::Handoff {
                    thread,
                    command,
                    argv,
                });
                self.changed.mark(Changed::NOTICE);
            }
            Server::Opened { id, scope, thread }
                if scope.project == self.profile.project && scope.route == self.profile.route =>
            {
                if self.remote().unwrap().await_new.as_deref() == Some(id.as_str()) {
                    self.select_replica(&thread);
                    self.remote_mut().await_new = None;
                    self.changed.mark(Changed::THREADS);
                }
                self.remote_mut().sending.remove("");
            }
            Server::WorkerReply { result, error, .. } => {
                let handoff = result
                    .as_ref()
                    .filter(|r| r["relation"] == "handoff" && error.is_none())
                    .cloned();
                if let Some(r) = handoff {
                    let scope: Option<crate::host_protocol::Scope> =
                        serde_json::from_value(r["scope"].clone()).ok();
                    let session = r["session"].as_str().unwrap_or_default().to_owned();
                    let label = scope
                        .as_ref()
                        .map_or("another route".into(), |s| s.route.label());
                    self.notice = Some(format!("Continued in {label}."));
                    if scope.is_some_and(|s| {
                        s.project == self.profile.project && s.route == self.profile.route
                    }) && self.catalog_row(&session).is_some()
                    {
                        self.select_replica(&session);
                    }
                    self.changed.mark(Changed::NOTICE | Changed::THREADS);
                    return Ok(());
                }
                // A worker waiting for YOLO is already an ask on this store; the
                // notice would only repeat it. Keep it when the ask has not arrived.
                if let Some(status) = result
                    .as_ref()
                    .filter(|s| error.is_none() && s["status"] == "waiting_for_operator")
                {
                    let ask_id = RequestId::Text(format!(
                        "{}{}",
                        crate::asks::YOLO_REQUEST_PREFIX,
                        status["job_id"].to_string().trim_matches('"')
                    ));
                    if self.asks.iter().any(|a| a.id == ask_id) {
                        self.notice = None;
                        self.changed.mark(Changed::NOTICE);
                        return Ok(());
                    }
                }
                self.notice = Some(match (result, error) {
                    (_, Some(error)) => error,
                    (Some(status), None) if status["status"] == "preparing" => format!(
                        "Preparing worker {} on {}.",
                        status["job_id"],
                        status["worker_host"]["name"]
                            .as_str()
                            .unwrap_or("the linked host")
                    ),
                    (Some(status), None) if status["status"] == "waiting_for_operator" => format!(
                        "OX worker {} waits for you to accept YOLO on its parent session.",
                        status["job_id"]
                    ),
                    (Some(status), None) => format!(
                        "Started OX worker {} on {}.",
                        status["job_id"],
                        status["model"].as_str().unwrap_or("its route")
                    ),
                    (None, None) => "The worker request finished.".into(),
                });
                self.changed.mark(Changed::NOTICE);
            }
            Server::Snapshot { scopes, .. } => {
                for scope in scopes {
                    self.apply_scope(scope)?;
                }
                self.continue_restore();
                self.reconcile_unconfirmed();
            }
            Server::Delta { changes, .. } => {
                for change in changes {
                    self.apply_change(change)?;
                }
                self.continue_restore();
                self.reconcile_unconfirmed();
            }
            Server::Ack { id } => {
                if let Some(thread) = self.remote_mut().unconfirmed_options.remove(&id) {
                    self.changed.transcript(&thread);
                }
                if self.remote_mut().pending.remove(&id).is_some() {
                    self.changed.mark(Changed::NOTICE);
                }
            }
            Server::Error { id, message } => {
                let error_id = id.clone();
                if let Some(thread) = id
                    .as_ref()
                    .and_then(|id| self.remote_mut().unconfirmed_options.remove(id))
                {
                    self.changed.transcript(&thread);
                }
                if let Some((owner, Some(draft), _)) =
                    id.and_then(|id| self.remote_mut().pending.remove(&id))
                {
                    draft.restore_before(self.drafts.entry(owner).or_default());
                }
                self.notice = Some(message);
                // An unrelated host error must not cancel the auto-select of the
                // session being opened. Only the opening request's own error does.
                if error_id.is_some() && self.remote().unwrap().await_new == error_id {
                    self.remote_mut().await_new = None;
                }
                self.changed.mark(Changed::NOTICE);
            }
            _ => (),
        }
        Ok(())
    }
    /// The replacement of a dead replica carries what the old one had not finished:
    /// the paging target, and input the host never acknowledged (the connection closed
    /// before or after the host took it, so delivery is unknown).
    pub(super) fn recover_replica(&mut self, previous: &mut Self) {
        let kept = previous
            .remote()
            .and_then(|r| r.restore)
            .map_or(0, |(t, _)| t);
        let loaded = previous.threads().len().max(kept);
        if loaded > 0 {
            self.remote_mut().restore = Some((loaded, None));
        }
        let Link::Host(old) = &mut previous.link else {
            return;
        };
        self.remote_mut().restored_seen = std::mem::take(&mut old.restored_seen);
        let mut unsent: Vec<_> = std::mem::take(&mut old.pending).into_values().collect();
        // Input the old replica was already holding back for the same reason.
        let waiting = std::mem::take(&mut old.unconfirmed);
        self.remote_mut().unconfirmed = waiting;
        unsent.sort_by_key(|(_, _, seen)| *seen);
        let mut restored = false;
        for (owner, draft, seen) in unsent {
            let Some(draft) = draft else { continue };
            let (text, attachments) = draft.signature();
            draft.restore_before(self.drafts.entry(owner.clone()).or_default());
            self.remote_mut().unconfirmed.push(Unconfirmed {
                owner,
                text,
                attachments,
                seen,
            });
            restored = true;
        }
        if restored {
            self.notice = Some(
                "The connection dropped while sending. Your message is back in the composer; check the conversation before sending it again.".into(),
            );
            self.changed.mark(Changed::NOTICE);
        }
    }
    /// A restored message the host did take shows up in its thread as a new identical
    /// user message: clear the composer copy so it is not sent twice. Whatever the
    /// transcript does not show stays, the safe side of an unknown delivery.
    fn reconcile_unconfirmed(&mut self) {
        let waiting = std::mem::take(&mut self.remote_mut().unconfirmed);
        for u in waiting {
            let still_unsent = self
                .drafts
                .get(&u.owner)
                .is_some_and(|d| d.signature() == (u.text.clone(), u.attachments.clone()));
            if !still_unsent {
                continue;
            }
            // A pasted image cannot be told from another one of the same kind in the
            // transcript: that is not proof this message arrived, so it stays.
            let delivered = u.attachments.iter().all(Option::is_some)
                && u.owner
                    .as_deref()
                    .and_then(|o| self.transcripts.get(o))
                    .is_some_and(|t| {
                        t.is_ready() && t.user_messages(&u.text, &u.attachments) > u.seen
                    });
            if delivered {
                self.drafts.remove(&u.owner);
                self.notice = None;
                self.changed.mark(Changed::NOTICE);
                if let Some(o) = &u.owner {
                    self.changed.transcript(o);
                }
            } else {
                self.remote_mut().unconfirmed.push(u);
            }
        }
    }
    /// A fresh snapshot carries only the host's own loaded catalog. When that is
    /// shorter than what this client had paged in (the host evicted a slow
    /// client past its replay window, or restarted), page back up to the old
    /// length instead of making the user scroll through every page again.
    fn continue_restore(&mut self) {
        let loaded = self.threads().len();
        let more = self.cursor.is_some();
        let Some((target, asked)) = self.remote().and_then(|r| r.restore) else {
            return;
        };
        if loaded >= target || !more {
            self.remote_mut().restore = None;
            return;
        }
        // One request per loaded length: a page in flight is not asked for twice.
        if asked == Some(loaded) {
            return;
        }
        self.remote_mut().restore = Some((target, Some(loaded)));
        if self.forward(Action::MoreThreads).is_err() {
            self.remote_mut().restore = None;
        }
    }
    fn apply_scope(&mut self, s: ScopeState) -> io::Result<()> {
        let before = self.threads().len();
        for id in self.transcripts.keys() {
            self.changed.replace_transcript(id);
        }
        self.transcripts.clear();
        self.apply_change(Change::State {
            connection: s.connection,
            capabilities: s.capabilities,
            detail: s.detail,
        })?;
        self.apply_change(Change::Sessions {
            sessions: s.sessions,
        })?;
        if before > self.threads().len() && self.cursor.is_some() {
            let kept = self.remote().and_then(|r| r.restore).map_or(0, |(t, _)| t);
            self.remote_mut().restore = Some((before.max(kept), None));
        }
        self.apply_change(Change::Asks { asks: s.asks })?;
        for (thread, timeline) in s.transcripts {
            self.apply_change(Change::Transcript { thread, timeline })?;
        }
        Ok(())
    }
    fn apply_change(&mut self, change: Change) -> io::Result<()> {
        match change {
            Change::State {
                connection,
                capabilities,
                detail,
            } => {
                self.state = match connection.as_str() {
                    "ready" => Connection::Ready,
                    "sign_in_required" => Connection::SignInRequired,
                    "closed" => Connection::Closed(bits(&detail["error"]).unwrap_or_default()),
                    _ => Connection::Connecting,
                };
                self.profile.provider = bits(&detail["provider"]).unwrap_or_default();
                if detail["next_sending"] == true {
                    self.remote_mut().sending.insert(String::new());
                } else {
                    self.remote_mut().sending.remove("");
                }
                self.next_options = bits(&detail["default_options"]);
                self.next_preset = bits(&detail["next_access"]);
                self.notice = bits(&detail["notice"]);
                self.commands.schema = bits(&detail["commands"]);
                self.commands.loading = detail["loading_commands"] == true;
                self.usage = bits(&detail["usage"]).unwrap_or_default();
                self.mention = bits(&detail["mention"]).unwrap_or_default();
                self.archived_threads = bits(&detail["archived"]).unwrap_or_default();
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|id| self.archived_threads.contains(id))
                {
                    self.selected = None;
                }
                self.cursor = (detail["more"] == true).then(String::new);
                self.palette_search.query = bits(&detail["palette_query"]).unwrap_or_default();
                self.palette_search.cursor = (detail["palette_more"] == true).then(String::new);
                self.palette_search.error = bits(&detail["palette_error"]);
                self.palette_search.rows.clear();
                for row in bits::<Vec<ThreadRow>>(&detail["palette_rows"]).unwrap_or_default() {
                    self.palette_search.rows.insert(row);
                }
                self.remote_mut().draft_access = bits(&detail["applied_draft_access"]);
                self.remote_mut().schema = detail["schema"] == true;
                self.remote_mut().scope_caps = capabilities;
                self.changed.mark(Changed::CONNECTION | Changed::NOTICE);
            }
            Change::Sessions { sessions } => {
                self.catalog.clear();
                self.terminal_threads.clear();
                let r = self.remote_mut();
                r.caps.clear();
                r.origins.clear();
                r.marks.clear();
                r.results.clear();
                r.routes.clear();
                r.commands.clear();
                r.options.clear();
                r.access.clear();
                for session in sessions {
                    self.apply_session(session);
                }
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            Change::Session { session } => {
                self.apply_session(session);
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            Change::Asks { asks } => {
                self.remote_mut().tokens.clear();
                self.asks = asks
                    .into_iter()
                    .map(|v| {
                        self.remote_mut().tokens.insert(v.id.clone(), v.token);
                        Ask {
                            command: v.command,
                            id: v.id,
                            thread: v.thread,
                            turn: v.turn,
                            item: v.item,
                            title: v.title,
                            detail: v.detail,
                            questions: v.questions,
                            decisions: v
                                .choices
                                .into_iter()
                                .enumerate()
                                .map(|(index, label)| {
                                    let mut d = Decision::new(label, Value::Null);
                                    d.detail = v.choice_details.get(index).cloned().flatten();
                                    d
                                })
                                .collect(),
                        }
                    })
                    .collect();
                self.changed.mark(Changed::ASK);
            }
            Change::Transcript { thread, timeline } => {
                let t = self.transcripts.entry(thread.clone()).or_default();
                let old_len = t.len();
                *t = Transcript::default();
                for item in timeline.items {
                    apply_item(t, item)?;
                }
                t.host_history_replaced(old_len);
                apply_meta(t, timeline.detail);
                if old_len > 0 {
                    self.changed.replace_transcript(&thread);
                } else {
                    self.changed.transcript(&thread);
                }
            }
            Change::TimelineState { thread, detail } => {
                apply_meta(self.transcripts.entry(thread.clone()).or_default(), detail);
                self.changed.transcript(&thread);
            }
            Change::Items { thread, items } => {
                let t = self.transcripts.entry(thread.clone()).or_default();
                for item in items {
                    apply_item(t, item)?;
                }
                self.changed.transcript(&thread);
            }
            Change::Append {
                thread,
                turn,
                id,
                text,
                revision,
            } => {
                let t = self.transcripts.entry(thread.clone()).or_default();
                let key = crate::transcript::ItemKey {
                    turn: turn.clone(),
                    item: id.clone(),
                };
                t.observe_delta(turn, id, DeltaTarget::Agent, text);
                t.host_revision(&key, revision);
                self.changed.transcript(&thread);
            }
            Change::Retired { thread } => {
                self.transcripts.remove(&thread);
                self.changed.replace_transcript(&thread);
            }
            Change::Notification { .. } => (),
        }
        Ok(())
    }
    fn apply_session(&mut self, s: SessionView) {
        let result = s.last_result();
        let id = s.id;
        let data = &s.detail;
        if let Some(mut row) = bits::<ThreadRow>(&data["row"]) {
            row.worker = data["worker"] == true;
            row.unread = data["unread"] == true;
            row.activity_order = data["order"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let generated = bits(&data["generated"]);
            self.record_lineage(&row);
            self.catalog.insert(row);
            if let Some(r) = self.catalog.get_mut(&id) {
                r.generated_title = generated;
            }
        }
        if data["terminal"] == true {
            self.terminal_threads.insert(id.clone());
        } else {
            self.terminal_threads.remove(&id);
        }
        if let (Some(message), Some(text)) = (
            data["restored_draft"]["message"].as_str(),
            data["restored_draft"]["text"].as_str(),
        ) && self
            .remote()
            .is_some_and(|r| r.restored_seen.get(&id).is_none_or(|seen| seen != message))
        {
            self.remote_mut()
                .restored_seen
                .insert(id.clone(), message.to_owned());
            // The host took the send and then lost it: the text is back in its
            // composer, so it comes back here too, before anything typed since.
            let current = self.drafts.entry(Some(id.clone())).or_default();
            if current.text != text {
                current.text = if current.text.is_empty() {
                    text.to_owned()
                } else {
                    format!("{text}\n{}", current.text)
                };
            }
            self.changed.mark(Changed::NOTICE);
        }
        let r = self.remote_mut();
        r.caps.insert(id.clone(), s.capabilities);
        if let Some(result) = result {
            r.results.insert(id.clone(), result);
        } else {
            r.results.remove(&id);
        }
        if data["changing"] == true {
            r.changing.insert(id.clone());
        } else {
            r.changing.remove(&id);
        }
        if let Some(route) = bits(&data["route"]) {
            r.routes.insert(id.clone(), route);
        }
        if let Some(access) = bits(&data["access"]) {
            r.access.insert(id.clone(), access);
        }
        if let Some(opts) = bits(&data["options"]) {
            r.options.insert(id.clone(), opts);
        }
        if let Some(caps) = bits(&data["command_caps"]) {
            r.commands.insert(id.clone(), caps);
        }
        if data["sending"] == true {
            r.sending.insert(id.clone());
        } else {
            r.sending.remove(&id);
        }
        let marks: serde_json::Map<String, Value> = [
            "handed_off_to",
            "handoff_attempts",
            "yolo_grant",
            "yolo_grant_expires",
            "yolo_children",
            "worker_model",
            "memory_attachment",
        ]
        .into_iter()
        .filter_map(|k| Some((k.to_owned(), data.get(k)?.clone())))
        .collect();
        if marks.is_empty() {
            r.marks.remove(&id);
        } else {
            r.marks.insert(id.clone(), Value::Object(marks));
        }
        match bits(&data["origin"]) {
            Some(origin) => {
                r.origins.insert(id.clone(), origin);
            }
            None => {
                r.origins.remove(&id);
            }
        }
    }
}
pub(super) fn apply_meta(t: &mut Transcript, d: Value) {
    t.active_turn = bits(&d["active"]);
    t.last_finished = bits(&d["last_finished"]);
    t.interrupted = d["interrupted"] == true;
    t.ready = d["ready"] == true;
    t.verified = t.ready;
    t.loading = d["loading"] == true;
    t.error = bits(&d["error"]);
    t.cursor = (d["older"] == true).then(String::new);
    t.access = bits(&d["access"]);
    t.reviews = bits(&d["reviews"]).unwrap_or_default();
    t.usage = bits(&d["usage"]);
    t.command_notice = bits(&d["command_notice"]);
    t.model = bits(&d["model"]);
    t.effort = bits(&d["effort"]);
    t.tier = bits(&d["tier"]);
    t.unread = d["unread"] == true;
}
pub(super) fn apply_item(t: &mut Transcript, i: TimelineItem) -> io::Result<()> {
    let item = match i.role.as_str() {
        "assistant" => Item::AgentMessage {
            text: i.text,
            is_update: i.detail["update"] == true,
        },
        "plan" => Item::Plan { text: i.text },
        "tool" => serde_json::from_value(i.detail["item"].clone())?,
        _ => serde_json::from_value(i.detail.clone())?,
    };
    let key = crate::transcript::ItemKey {
        turn: i.turn.clone(),
        item: i.id.clone(),
    };
    t.observe_item(i.turn, i.id, item, true, bits(&i.detail["output"]));
    t.host_revision(&key, i.revision);
    Ok(())
}

#[cfg(test)]
mod local_command_tests {
    use super::*;
    use crate::{
        asks::CommandApproval,
        host_protocol::{AskView, Change},
    };
    #[test]
    fn worker_replies_name_handoffs_and_yolo_waits() {
        let route = crate::runtime::Runtime::Claude.route(None);
        let mut store = Store::remote_client(
            Profile {
                project: "/p".into(),
                route: route.clone(),
                provider: "anthropic".into(),
            },
            |_, _| Ok("sent".into()),
        );
        let reply = |result: Value, error: Option<&str>| {
            LinkEvent::Host(Box::new(Server::WorkerReply {
                id: "k".into(),
                result: Some(result),
                error: error.map(Into::into),
            }))
        };
        let target = crate::runtime::Runtime::Codex.route(Some("fixture/demo".into()));
        store.apply(reply(
            serde_json::json!({"relation":"handoff","session":"t","scope":{"project":"/p","route":target}}),
            None,
        ));
        assert_eq!(store.notice(), Some("Continued in OX API · fixture/demo."));
        store.apply(reply(
            serde_json::json!({"status":"waiting_for_operator","job_id":9}),
            None,
        ));
        assert!(store.notice().unwrap().contains("accept YOLO"));
        // Once the host's YOLO ask is on the store, the reply adds no duplicate notice.
        store
            .apply_change(Change::Asks {
                asks: vec![AskView {
                    id: RequestId::Text("ox-yolo-9".into()),
                    token: "t".into(),
                    thread: Some("t".into()),
                    turn: None,
                    item: None,
                    choices: vec![
                        "Accept YOLO".into(),
                        "Start in Auto".into(),
                        "Decline".into(),
                    ],
                    choice_details: vec![],
                    command: None,
                    title: "Start a Codex worker in YOLO?".into(),
                    detail: String::new(),
                    questions: vec![],
                }],
            })
            .unwrap();
        store.apply(reply(
            serde_json::json!({"status":"waiting_for_operator","job_id":9}),
            None,
        ));
        assert_eq!(store.notice(), None);
        assert!(store.asks()[0].waits_for_yolo());
        store.apply(reply(
            serde_json::json!({}),
            Some("This session cannot be handed off yet"),
        ));
        assert_eq!(
            store.notice(),
            Some("This session cannot be handed off yet")
        );
    }
    #[test]
    fn terminal_handoff_sends_only_the_verified_stock_decline() {
        let project = PathBuf::from("/project");
        let id = RequestId::Number(7);
        let (tx, received) = std::sync::mpsc::channel();
        let mut store = Store::remote_client(
            Profile {
                project,
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            move |action, token| {
                tx.send((action, token)).unwrap();
                Ok("my-local-receipt".into())
            },
        );
        let mut view = AskView {
            id: id.clone(),
            token: "new-ask".into(),
            thread: Some("t".into()),
            turn: None,
            item: None,
            choices: vec!["Cancel turn".into(), "Allow once".into(), "Decline".into()],
            choice_details: vec![],
            command: Some(CommandApproval {
                command: "sudo id".into(),
                cwd: Some("/tool-directory".into()),
                decline: 2,
            }),
            title: "Run command".into(),
            detail: String::new(),
            questions: vec![],
        };
        store
            .apply_change(Change::Asks {
                asks: vec![view.clone()],
            })
            .unwrap();
        assert!(store.decline_reviewed_command(&id, "stale-ask").is_err());
        assert!(received.try_recv().is_err());
        assert_eq!(
            store.decline_reviewed_command(&id, "new-ask").unwrap(),
            "my-local-receipt"
        );
        let (action, token) = received.try_recv().unwrap();
        assert!(matches!(action, Action::AnswerAsk(RequestId::Number(7), 2)));
        assert_eq!(token.as_deref(), Some("new-ask"));
        view.command = None;
        store
            .apply_change(Change::Asks { asks: vec![view] })
            .unwrap();
        assert!(store.decline_reviewed_command(&id, "new-ask").is_err());
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn only_the_opening_requests_own_error_cancels_the_auto_select() {
        let route = Route::CODEX_SUBSCRIPTION;
        let mut store = Store::remote_client(
            Profile {
                project: "/p".into(),
                route: route.clone(),
                provider: "openai".into(),
            },
            |_, _| Ok("opening".into()),
        );
        let scope = crate::host_protocol::Scope {
            project: "/p".into(),
            route,
        };
        store.dispatch(Action::ForkThread("source".into())).unwrap();
        assert!(store.opening_thread());
        for id in [None, Some("another-request".to_owned())] {
            store
                .apply_host(Server::Error {
                    id,
                    message: "unrelated".into(),
                })
                .unwrap();
            assert!(store.opening_thread(), "an unrelated error keeps waiting");
        }
        store
            .apply_host(Server::Opened {
                id: "another-request".into(),
                scope: scope.clone(),
                thread: "wrong".into(),
            })
            .unwrap();
        assert_eq!(
            store.selected(),
            None,
            "another request's session is not selected"
        );
        store
            .apply_host(Server::Opened {
                id: "opening".into(),
                scope: scope.clone(),
                thread: "new".into(),
            })
            .unwrap();
        assert_eq!(store.selected(), Some("new"));
        assert!(!store.opening_thread());
        store.dispatch(Action::ForkThread("source".into())).unwrap();
        store
            .apply_host(Server::Error {
                id: Some("opening".into()),
                message: "refused".into(),
            })
            .unwrap();
        assert!(!store.opening_thread(), "its own error stops the wait");
    }

    fn page(from: usize, to: usize) -> Vec<SessionView> {
        (from..to)
            .map(|n| SessionView {
                id: format!("t{n:05}"),
                title: format!("T{n}"),
                parent: None,
                updated_at: 0,
                status: "idle".into(),
                capabilities: vec![],
                detail: serde_json::json!({
                    "row": {"id": format!("t{n:05}"), "cwd": "/p", "modelProvider": "openai",
                        "model": null, "name": null, "preview": format!("T{n}"),
                        "updatedAt": 0, "status": null, "parentThreadId": null,
                        "agentNickname": null},
                    "order": (100_000 - n).to_string(),
                }),
            })
            .collect()
    }
    fn scope_state(sessions: Vec<SessionView>) -> ScopeState {
        let route = Route::CODEX_SUBSCRIPTION;
        ScopeState {
            scope: crate::host_protocol::Scope {
                project: "/p".into(),
                route,
            },
            connection: "ready".into(),
            capabilities: vec![],
            sessions,
            asks: vec![],
            transcripts: Default::default(),
            detail: serde_json::json!({"more": true}),
        }
    }
    #[test]
    fn a_fresh_snapshot_keeps_the_paged_in_catalog_length() {
        use std::sync::{Arc, Mutex};
        let asked = Arc::new(Mutex::new(0usize));
        let counter = asked.clone();
        let mut store = Store::remote_client(
            Profile {
                project: "/p".into(),
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            move |action, _| {
                if matches!(action, Action::MoreThreads) {
                    *counter.lock().unwrap() += 1;
                }
                Ok("sent".into())
            },
        );
        let snapshot = |sessions| Server::Snapshot {
            cursor: crate::host_protocol::Cursor {
                epoch: "e".into(),
                seq: 1,
            },
            scopes: vec![scope_state(sessions)],
        };
        let delta = |changes| Server::Delta {
            cursor: crate::host_protocol::Cursor {
                epoch: "e".into(),
                seq: 2,
            },
            scope: crate::host_protocol::Scope {
                project: "/p".into(),
                route: Route::CODEX_SUBSCRIPTION,
            },
            changes,
        };
        store.apply_host(snapshot(page(0, 50))).unwrap();
        for n in 1..5 {
            let rows = page(n * 50, n * 50 + 50);
            store
                .apply_host(delta(
                    rows.into_iter()
                        .map(|session| Change::Session { session })
                        .collect(),
                ))
                .unwrap();
        }
        assert_eq!(store.threads().len(), 250);
        assert_eq!(*asked.lock().unwrap(), 0);
        // The client fell behind the replay window: the host sends a fresh snapshot.
        store.apply_host(snapshot(page(0, 50))).unwrap();
        assert_eq!(store.threads().len(), 50, "the snapshot itself is short");
        assert_eq!(*asked.lock().unwrap(), 1, "asks for the next page at once");
        // A delta that is not a page does not ask again while the page is in flight.
        store.apply_host(delta(vec![])).unwrap();
        assert_eq!(*asked.lock().unwrap(), 1);
        for n in 1..5 {
            let rows = page(n * 50, n * 50 + 50);
            store
                .apply_host(delta(
                    rows.into_iter()
                        .map(|session| Change::Session { session })
                        .collect(),
                ))
                .unwrap();
        }
        let ids: Vec<_> = store.threads().iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids.len(), 250, "no row lost or duplicated");
        assert_eq!(ids[249], "t00249");
        assert_eq!(
            *asked.lock().unwrap(),
            4,
            "one request per missing page, then stop"
        );
        store.apply_host(delta(vec![])).unwrap();
        assert_eq!(*asked.lock().unwrap(), 4);
    }

    fn profile() -> Profile {
        Profile {
            project: "/p".into(),
            route: Route::CODEX_SUBSCRIPTION,
            provider: "openai".into(),
        }
    }
    fn snapshot(seq: u64, sessions: Vec<SessionView>) -> Server {
        Server::Snapshot {
            cursor: crate::host_protocol::Cursor {
                epoch: "e".into(),
                seq,
            },
            scopes: vec![scope_state(sessions)],
        }
    }
    /// The outbound action was queued, then the client discovered the host had gone
    /// away. A closed peer can be observed by either half of the duplex transport.
    fn store_whose_host_closed_the_socket() -> Store {
        use std::io::{Read, Write};
        let (mut client, host) = std::os::unix::net::UnixStream::pair().unwrap();
        // macOS can reject socket options once the peer has disconnected.
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel::<(String, Action)>(16);
        let seq = std::sync::atomic::AtomicU64::new(0);
        let mut old = Store::remote_client(profile(), move |action, _| {
            let n = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = format!("a-{n}");
            tx.try_send((id.clone(), action))
                .map_err(|e| io::Error::other(e.to_string()))?;
            Ok(id)
        });
        let mut rows = page(0, 150);
        rows[0].capabilities = vec!["send".into()];
        old.apply_host(snapshot(1, rows)).unwrap();
        old.cursor = Some("more".into());
        old.select_replica("t00000");
        // Shutdown, not just drop: a process forked by a parallel test may briefly hold a copy.
        host.shutdown(std::net::Shutdown::Both).unwrap();
        drop(host);
        old.edit(Some("t00000"), "do not lose me".into());
        old.attach_image(
            Some("t00000"),
            crate::mentions::ImageInput::File("/tmp/shot.png".into()),
        );
        old.submit(Some("t00000")).unwrap();
        assert!(old.sending(Some("t00000")));
        assert_eq!(
            old.draft(Some("t00000")),
            "",
            "pending, not in the composer"
        );
        let (id, action) = rx.try_recv().expect("send was queued on the transport");
        assert!(matches!(action, Action::SendImages(..)));
        let mut frame = serde_json::to_vec(&(id, action)).unwrap();
        frame.push(b'\n');
        // A write can enter a socket buffer even after the peer shuts down. The real
        // host client also reports EOF on its independent reader in that case.
        let reason = match client.write_all(&frame) {
            Err(error) => error.to_string(),
            Ok(()) => {
                assert_eq!(client.read(&mut [0]).expect("read after peer shutdown"), 0);
                "Session host disconnected".into()
            }
        };
        old.apply(LinkEvent::Closed {
            reason,
            stderr_tail: vec![],
        });
        old
    }
    fn counting_replica() -> (Store, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let more = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = more.clone();
        let store = Store::remote_client(profile(), move |action, _| {
            match action {
                Action::MoreThreads => counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                // A resend of the input would be a duplicate.
                Action::SendText(..) | Action::SendImages(..) => panic!("auto-sent {action:?}"),
                _ => 0,
            };
            Ok("n".into())
        });
        (store, more)
    }
    #[test]
    fn a_replacement_replica_repages_and_keeps_input_the_host_never_acknowledged() {
        let mut old = store_whose_host_closed_the_socket();
        let (mut fresh, more) = counting_replica();
        fresh.recover_from(&mut old);
        assert_eq!(fresh.draft(Some("t00000")), "do not lose me");
        assert_eq!(fresh.draft_images(Some("t00000")).len(), 1);
        assert!(
            !fresh.sending(Some("t00000")),
            "nothing is sent for the user"
        );
        assert!(
            fresh
                .notice()
                .is_some_and(|n| n.contains("check the conversation"))
        );
        // The host's replacement snapshot is shorter than what the old store had paged in.
        fresh.apply_host(snapshot(1, page(0, 50))).unwrap();
        assert_eq!(fresh.threads().len(), 50);
        assert_eq!(more.load(std::sync::atomic::Ordering::SeqCst), 1);
        for n in 1..3 {
            let changes = page(n * 50, n * 50 + 50)
                .into_iter()
                .map(|session| Change::Session { session })
                .collect();
            fresh
                .apply_host(Server::Delta {
                    cursor: crate::host_protocol::Cursor {
                        epoch: "e".into(),
                        seq: 1 + n as u64,
                    },
                    scope: crate::host_protocol::Scope {
                        project: "/p".into(),
                        route: Route::CODEX_SUBSCRIPTION,
                    },
                    changes,
                })
                .unwrap();
        }
        assert_eq!(fresh.threads().len(), 150, "back to the old length");
        assert_eq!(more.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(fresh.draft(Some("t00000")), "do not lose me");
    }
    /// The host took the message but its acknowledgement was lost with the socket. The
    /// replacement's transcript shows it, so the restored composer copy is dropped
    /// instead of waiting to be sent twice; a transcript without it keeps the copy.
    #[test]
    fn a_message_the_host_received_is_reconciled_by_its_echo() {
        let echo = |text: &str| {
            let mut scope = scope_state(page(0, 150));
            scope.sessions[0].capabilities = vec!["send".into()];
            scope.transcripts.insert(
                "t00000".into(),
                crate::host_protocol::Timeline {
                    items: vec![TimelineItem {
                        turn: "turn-1".into(),
                        id: "item-1".into(),
                        role: "user".into(),
                        text: text.into(),
                        revision: 1,
                        detail: serde_json::to_value(Item::UserMessage {
                            content: vec![
                                crate::transcript::item::Content {
                                    text: Some(text.into()),
                                    path: None,
                                },
                                crate::transcript::item::Content {
                                    text: None,
                                    path: Some("/tmp/shot.png".into()),
                                },
                            ],
                        })
                        .unwrap(),
                    }],
                    detail: serde_json::json!({"ready": true}),
                },
            );
            Server::Snapshot {
                cursor: crate::host_protocol::Cursor {
                    epoch: "e".into(),
                    seq: 1,
                },
                scopes: vec![scope],
            }
        };
        let mut old = store_whose_host_closed_the_socket();
        let (mut fresh, _) = counting_replica();
        fresh.recover_from(&mut old);
        fresh.apply_host(echo("something else")).unwrap();
        assert_eq!(
            fresh.draft(Some("t00000")),
            "do not lose me",
            "delivery unknown: preserved"
        );
        fresh.apply_host(echo("do not lose me")).unwrap();
        assert_eq!(fresh.draft(Some("t00000")), "");
        assert!(fresh.draft_images(Some("t00000")).is_empty());
    }
    /// The same text arrives with a different image (a second device): that is not the
    /// message this client lost, so its composer keeps text and image A.
    #[test]
    fn the_same_text_with_a_different_image_is_not_the_lost_message() {
        let echo = |path: &str| {
            let mut scope = scope_state(page(0, 150));
            scope.sessions[0].capabilities = vec!["send".into()];
            scope.transcripts.insert(
                "t00000".into(),
                crate::host_protocol::Timeline {
                    items: vec![TimelineItem {
                        turn: "turn-1".into(),
                        id: "item-1".into(),
                        role: "user".into(),
                        text: "do not lose me".into(),
                        revision: 1,
                        detail: serde_json::to_value(Item::UserMessage {
                            content: vec![
                                crate::transcript::item::Content {
                                    text: Some("do not lose me".into()),
                                    path: None,
                                },
                                crate::transcript::item::Content {
                                    text: None,
                                    path: Some(path.into()),
                                },
                            ],
                        })
                        .unwrap(),
                    }],
                    detail: serde_json::json!({"ready": true}),
                },
            );
            Server::Snapshot {
                cursor: crate::host_protocol::Cursor {
                    epoch: "e".into(),
                    seq: 1,
                },
                scopes: vec![scope],
            }
        };
        let mut old = store_whose_host_closed_the_socket();
        let (mut fresh, _) = counting_replica();
        fresh.recover_from(&mut old);
        fresh.apply_host(echo("/tmp/other-device.png")).unwrap();
        assert_eq!(fresh.draft(Some("t00000")), "do not lose me");
        assert_eq!(fresh.draft_images(Some("t00000")).len(), 1);
        // Its own echo (same path) still reconciles.
        fresh.apply_host(echo("/tmp/shot.png")).unwrap();
        assert_eq!(fresh.draft(Some("t00000")), "");
    }
}
