use super::{Connection, Pending, Route, Store};
use crate::runtime::Billing;
use crate::{
    changed::Changed,
    codex::{
        history::ResumedThread,
        reply::Reply,
        request::{Call, ThreadCommand},
    },
};
use std::io;

const UNCONFIRMED_INPUT: &str = "The session changed before Codex confirmed delivery.";

#[derive(Default)]
pub(super) struct Draft {
    pub text: String,
    images: Vec<crate::mentions::ImageInput>,
    sent: Option<SentInput>,
    delivery: InputDelivery,
}

#[derive(Default)]
enum InputDelivery {
    #[default]
    Untracked,
    Waiting,
    Complete(Result<(), String>),
}

impl Draft {
    fn complete_delivery(&mut self, result: Result<(), String>) {
        if matches!(self.delivery, InputDelivery::Waiting) {
            self.delivery = InputDelivery::Complete(result);
        }
    }

    /// What a delivered copy of this draft looks like in a transcript: trimmed text and
    /// its attachments. A pasted image has no identity the transcript echoes, so two
    /// different pastes look alike (`None`).
    pub(super) fn signature(&self) -> (String, Vec<Option<String>>) {
        let images = self
            .images
            .iter()
            .map(|i| match i {
                crate::mentions::ImageInput::File(p) => Some(p.to_string_lossy().into_owned()),
                crate::mentions::ImageInput::Clipboard(..) => None,
            })
            .collect();
        (self.text.trim().to_owned(), images)
    }
    pub(super) fn restore_before(mut self, newer: &mut Draft) {
        if !self.text.is_empty() && !newer.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&newer.text);
        self.images.append(&mut newer.images);
        newer.text = self.text;
        newer.images = self.images;
    }
}
enum SentInput {
    Draft(String, Vec<crate::mentions::ImageInput>),
    PaletteCommand,
}
impl SentInput {
    fn text(&self) -> Option<&str> {
        match self {
            Self::Draft(text, _) => Some(text),
            Self::PaletteCommand => None,
        }
    }
}
#[derive(Debug)]
pub(super) struct Submit {
    opening_preset: Option<crate::runtime::Access>,
    options: Option<super::routes_thread::Options>,
    owner: Option<String>,
    expected_turn: Option<String>,
    selection: u64,
    command: Option<ThreadCommand>,
    /// Handoff items injected after thread/start and before the first turn.
    inject: Vec<serde_json::Value>,
    /// The sender statement for this message, fixed when the host accepted it.
    context: Option<String>,
    memory_attachment: Option<crate::memory::Attachment>,
    worker_notice: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerNoticeReceipt {
    Accepted(u64),
    Rejected(u64),
}

#[derive(Debug)]
pub(super) enum Operation {
    New(Submit),
    Account(Submit),
    Send(Submit),
    Inject(Submit),
    Stop(String),
}
impl Operation {
    pub(super) fn worker_notice(&self) -> Option<u64> {
        match self {
            Self::Send(s) | Self::Account(s) => s.worker_notice,
            _ => None,
        }
    }
    pub(super) fn thread(&self) -> Option<&str> {
        match self {
            Self::New(s) | Self::Account(s) | Self::Send(s) | Self::Inject(s) => s.owner.as_deref(),
            Self::Stop(id) => Some(id),
        }
    }
}
impl Store {
    /// Let the host reply to the submitting connection after Codex confirms this input.
    /// An existing thread that first needs to resume still uses the existing open flow.
    pub fn track_codex_input(&mut self, thread: &str) -> bool {
        if let Some(draft) = self.drafts.get_mut(&Some(thread.to_owned()))
            && matches!(draft.sent, Some(SentInput::Draft(..)))
        {
            draft.delivery = InputDelivery::Waiting;
            true
        } else {
            false
        }
    }

    pub fn take_codex_input_delivery(&mut self, thread: &str) -> Option<Result<(), String>> {
        match self.drafts.get_mut(&Some(thread.to_owned())) {
            Some(draft) => match std::mem::take(&mut draft.delivery) {
                InputDelivery::Waiting => {
                    draft.delivery = InputDelivery::Waiting;
                    None
                }
                InputDelivery::Complete(result) => Some(result),
                InputDelivery::Untracked => Some(Err(UNCONFIRMED_INPUT.into())),
            },
            None => Some(Err(UNCONFIRMED_INPUT.into())),
        }
    }

    pub(super) fn worker_notice_id(&self) -> Option<u64> {
        match self.sender {
            Some(crate::surface::Sender::WorkerNotice { id }) => Some(id),
            _ => None,
        }
    }

    pub fn take_worker_notice_receipts(&mut self) -> Vec<WorkerNoticeReceipt> {
        std::mem::take(&mut self.worker_notice_receipts)
    }

    /// Dispatch an action the host accepted from `sender`. A message this submits, now or
    /// later from a queue or a deferred resume, carries that sender's statement.
    pub fn dispatch_as(
        &mut self,
        action: crate::actions::Action,
        sender: crate::surface::Sender,
    ) -> io::Result<crate::actions::Effect> {
        let previous = self.sender.replace(sender);
        let result = self.dispatch(action);
        self.sender = previous;
        result
    }
    /// Enable budgeted Tier-1 memory injection on the first turn of new sessions and workers.
    pub fn enable_memory(&mut self, store_path: std::path::PathBuf, host_id: String) {
        self.memory = Some(crate::memory::MemoryConfig {
            store_path,
            host_id,
            brief_override: None,
        });
    }
    /// Set or clear a pre-filtered Tier-1 brief override (used for remote worker jobs).
    pub fn set_memory_brief_override(&mut self, brief: Option<String>) {
        if let Some(cfg) = &mut self.memory {
            cfg.brief_override = brief;
        }
    }
    /// The statement for a message submitted now, from the host's current stamp.
    pub(super) fn sender_context(&self) -> Option<String> {
        self.sender
            .as_ref()
            .map(|sender| sender.context(&self.profile.route))
    }
    /// Sender context plus the budgeted Tier-1 pinned memory brief, attached once on
    /// the first turn of a new session or worker so subsequent turns stay cache-stable.
    pub(super) fn first_turn_context(&self) -> (Option<String>, Option<crate::memory::Attachment>) {
        let sender = self.sender_context();
        if let Some(config) = &self.memory
            && let Some(brief) =
                crate::memory::first_turn_brief(config, &self.profile.project, &self.profile.route)
        {
            let entries = brief.lines().filter(|line| line.starts_with("- [")).count();
            let attachment = (entries > 0).then(|| crate::memory::Attachment {
                entries,
                from_parent_host: config
                    .brief_override
                    .as_deref()
                    .is_some_and(|brief| !brief.trim().is_empty()),
            });
            let context = match sender {
                Some(sender) => format!("{sender}\n\n{brief}"),
                None => brief,
            };
            (Some(context), attachment)
        } else {
            (sender, None)
        }
    }

    /// Record an acknowledged startup brief or restore its saved metadata without opening a thread.
    pub fn remember_memory_attachment(&mut self, id: &str, attachment: crate::memory::Attachment) {
        self.memory_attachments.insert(id.to_owned(), attachment);
        self.changed.mark(Changed::THREADS);
    }

    pub fn new_session(&mut self) {
        if self.remote().is_some() {
            if let Err(e) = self.dispatch_remote(crate::actions::Action::NewDraft) {
                self.notice = Some(e.to_string());
            }
            return;
        }
        self.selection += 1;
        self.selected = None;
    }
    pub fn draft(&self, id: Option<&str>) -> &str {
        self.drafts
            .get(&id.map(str::to_owned))
            .map_or("", |d| d.text.as_str())
    }
    pub fn image_model_warning(&self, id: Option<&str>) -> Option<&'static str> {
        if self.draft_images(id).is_empty() {
            return None;
        }
        let chosen = self.options(id);
        self.command_caps(id).models.get(&chosen.model)?.input_modalities.as_ref()
            .filter(|modalities| !modalities.iter().any(|m| m=="image"))
            .map(|_| "This model accepts text only. Choose an image-capable model or remove the attachment.")
    }
    pub fn draft_images(&self, id: Option<&str>) -> &[crate::mentions::ImageInput] {
        self.drafts
            .get(&id.map(str::to_owned))
            .map_or(&[], |d| d.images.as_slice())
    }
    pub fn attach_image(&mut self, id: Option<&str>, image: crate::mentions::ImageInput) {
        self.drafts
            .entry(id.map(str::to_owned))
            .or_default()
            .images
            .push(image);
    }
    pub fn remove_image(&mut self, id: Option<&str>, index: usize) {
        if let Some(d) = self.drafts.get_mut(&id.map(str::to_owned))
            && index < d.images.len()
        {
            d.images.remove(index);
        }
    }
    pub fn edit(&mut self, id: Option<&str>, text: String) {
        self.drafts.entry(id.map(str::to_owned)).or_default().text = text;
    }
    /// Includes the interval between accepting a send and receiving turn/started.
    pub fn has_pending_turn(&self) -> bool {
        self.antigravity_opening()
            || !self.resume_sends.is_empty()
            || self.drafts.values().any(|d| d.sent.is_some())
            || self
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::Compose(_)))
    }

    pub fn sending(&self, id: Option<&str>) -> bool {
        if let Some(remote) = self.remote() {
            return remote.sending.contains(id.unwrap_or("")) || self.pending_input(id);
        }
        id.is_some_and(|id| self.resume_sends.contains(id))
            || self
                .drafts
                .get(&id.map(str::to_owned))
                .is_some_and(|d| d.sent.is_some())
    }
    pub fn can_send(&self, id: Option<&str>) -> bool {
        if let Some(remote) = self.remote() {
            return match id {
                None => remote.scope_caps.iter().any(|s| s == "start"),
                Some(id) => remote.can(id, "send") || remote.can(id, "steer"),
            };
        }
        !self.sending(id) && self.ready_to_send(id)
    }
    pub(super) fn ready_to_send(&self, id: Option<&str>) -> bool {
        self.state == Connection::Ready
            && (!self.is_antigravity() || self.antigravity_can_submit(id))
            && (!self.is_session_scoped() || self.claude_can_submit(id))
            && id.is_none_or(|thread| !self.changing_options(thread))
            && id.is_none_or(|id| {
                !self.in_terminal(id)
                    && !self.moving_threads.contains(id)
                    && !self.is_native_live(id)
                    && self
                        .transcripts
                        .get(id)
                        .is_some_and(|t| t.ready && t.verified)
            })
    }
    /// The composer is usable: the thread can take a message now, or it is ended and a
    /// send reopens it first. Ending or reaping a process never disables the composer.
    pub fn can_compose(&self, id: Option<&str>) -> bool {
        self.can_send(id) || id.is_some_and(|id| self.can_resume_send(id))
    }
    /// An ended thread (no live process) that a send can reopen. Local stores only; a
    /// remote client reads the host's `send` capability instead.
    pub(super) fn can_resume_send(&self, id: &str) -> bool {
        self.remote().is_none()
            && self.ended_threads.contains(id)
            && !self.moving_threads.contains(id)
            && !self.is_native_live(id)
            && matches!(self.state, Connection::Ready | Connection::Connecting)
            && !self.has_live_session(id)
            && !self.in_terminal(id)
            && !self.sending(Some(id))
            && self.catalog.get(id).is_some()
            // A Claude route still connecting can start the resumed process itself: it
            // never waits on the blank draft process, so a reaped route's first send works.
            && !(self.is_session_scoped() && id.contains('/'))
    }
    /// Reopen an ended thread and keep the draft to send once it is ready. Claude resumes
    /// with the message as the process's first input; Codex resumes, then `flush_resume_sends`
    /// submits the draft when the thread is verified.
    fn resume_and_send(&mut self, id: &str) -> io::Result<()> {
        let draft = self.drafts.get(&Some(id.to_owned()));
        let text = draft.map_or("", |d| d.text.as_str());
        let has_images = draft.is_some_and(|d| !d.images.is_empty());
        if text.trim().is_empty() && !has_images {
            return Err(io::Error::other("Enter a message"));
        }
        if self.is_session_scoped() {
            if has_images {
                return Err(io::Error::other(
                    "Remove the attachment before submitting to Claude Code",
                ));
            }
            return self.claude_resume_send(id, None);
        }
        if !self.is_antigravity() && text.trim_start().starts_with('/') {
            return Err(io::Error::other("Open this command in stock Codex"));
        }
        self.ended_threads.remove(id);
        self.resume_sends.insert(id.into());
        if let Some(sender) = self.sender.clone() {
            self.resume_senders.insert(id.into(), sender);
        }
        let opened = if self.state == Connection::Ready {
            self.open_catalog_id(id)
        } else {
            // The runtime is still connecting; `connected` reopens what is listed here.
            self.selection += 1;
            self.selected = Some(id.into());
            self.reopen.push(id.into());
            self.mark_loading(id);
            Ok(())
        };
        if let Err(error) = opened {
            self.resume_sends.remove(id);
            self.resume_senders.remove(id);
            self.ended_threads.insert(id.into());
            return Err(error);
        }
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
    /// Submit drafts waiting on a reopened thread once it can take them. A reopen that
    /// failed drops the wait and leaves the draft in the composer.
    pub(super) fn flush_resume_sends(&mut self) {
        if self.resume_sends.is_empty() {
            return;
        }
        let waiting: Vec<String> = self.resume_sends.iter().cloned().collect();
        for id in waiting {
            if self.held_yolo_resumes.contains_key(&id) {
                continue;
            }
            self.resume_sends.remove(&id);
            if self.can_send(Some(&id)) {
                // The message goes out with the stamp it was accepted with, not with
                // whatever the host is dispatching when the thread becomes ready.
                let previous = std::mem::replace(&mut self.sender, self.resume_senders.remove(&id));
                let submitted = match self.resume_commands.remove(&id) {
                    Some((command, from_palette)) => {
                        self.submit_operation(Some(&id), Some(command), from_palette)
                    }
                    None => self.submit(Some(&id)),
                };
                self.sender = previous;
                if let Err(error) = submitted {
                    self.notice = Some(error.to_string());
                    self.changed.mark(Changed::NOTICE);
                }
                continue;
            }
            let t = self.transcripts.get(&id);
            let opening = matches!(self.state, Connection::Connecting)
                || t.is_some_and(|t| t.loading || (t.ready && !t.verified));
            if opening && t.is_none_or(|t| t.error.is_none()) {
                self.resume_sends.insert(id);
            } else {
                self.resume_senders.remove(&id);
                self.resume_commands.remove(&id);
                self.ended_threads.insert(id.clone());
                self.notice = Some(
                    "The thread did not resume. Your message is still in the composer.".into(),
                );
                self.changed.mark(Changed::NOTICE);
                self.changed.transcript(&id);
            }
        }
    }
    pub(crate) fn send_client_message(
        &mut self,
        owner: Option<String>,
        text: String,
        images: Vec<crate::mentions::ImageInput>,
    ) -> io::Result<()> {
        if self.sending(owner.as_deref()) {
            return Err(io::Error::other("A message is already being submitted"));
        }
        let previous = self.drafts.insert(
            owner.clone(),
            Draft {
                text,
                images,
                ..Draft::default()
            },
        );
        match self.submit(owner.as_deref()) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.drafts.remove(&owner);
                if let Some(d) = previous {
                    self.drafts.insert(owner, d);
                }
                Err(error)
            }
        }
    }
    /// Open a new session that continues another one. Codex receives `items`
    /// through thread/inject_items before the first turn; a Claude `text`
    /// already carries its context block, so `items` must be empty there.
    pub fn start_handoff(&mut self, text: String, items: Vec<serde_json::Value>) -> io::Result<()> {
        if (self.is_session_scoped() || self.is_antigravity()) && !items.is_empty() {
            return Err(io::Error::other("This runtime has no history injection"));
        }
        if self.is_antigravity() {
            return self.antigravity_handoff(text);
        }
        self.handoff_items = (!items.is_empty()).then_some(items);
        self.claude_handoff = self.is_session_scoped();
        let result = self.send_client_message(None, text, vec![]);
        self.handoff_items = None;
        self.claude_handoff = false;
        result
    }
    pub fn submit(&mut self, id: Option<&str>) -> io::Result<()> {
        if self.remote().is_some() {
            return self
                .dispatch_remote(crate::actions::Action::Submit(id.map(str::to_owned)))
                .map(|_| ());
        }
        if let Some(thread) = id
            && self.can_resume_send(thread)
        {
            return self.resume_and_send(thread);
        }
        if self.is_antigravity() {
            return self.antigravity_submit(id);
        }
        if self.is_session_scoped() {
            return self.claude_submit(id, None);
        }
        self.submit_operation(id, None, false)
    }
    pub(super) fn submit_operation(
        &mut self,
        id: Option<&str>,
        command: Option<ThreadCommand>,
        from_palette: bool,
    ) -> io::Result<()> {
        if !self.can_send(id) {
            return Err(io::Error::other("Wait for the thread to be ready"));
        }
        if command.is_none()
            && !from_palette
            && let Some(warning) = self.image_model_warning(id)
        {
            return Err(io::Error::other(warning));
        }
        let owner = id.map(str::to_owned);
        let draft = self.drafts.entry(owner.clone()).or_default();
        if !from_palette && draft.text.trim().is_empty() && draft.images.is_empty() {
            return Err(io::Error::other("Enter a message"));
        }
        if command.is_none() && draft.text.trim_start().starts_with('/') {
            return Err(io::Error::other("Open this command in stock Codex"));
        }
        if let Some(id) = id
            && self.held_yolo_resumes.contains_key(id)
        {
            self.resume_sends.insert(id.into());
            if let Some(sender) = self.sender.clone() {
                self.resume_senders.insert(id.into(), sender);
            }
            if let Some(command) = command {
                self.resume_commands
                    .insert(id.into(), (command, from_palette));
            }
            self.changed.transcript(id);
            self.changed.mark(Changed::THREADS);
            return Ok(());
        }
        draft.sent = Some(if from_palette {
            SentInput::PaletteCommand
        } else {
            SentInput::Draft(
                std::mem::take(&mut draft.text),
                if command.is_none() {
                    std::mem::take(&mut draft.images)
                } else {
                    vec![]
                },
            )
        });
        let (context, memory_attachment) = if id.is_none() {
            self.first_turn_context()
        } else {
            (self.sender_context(), None)
        };
        let submit = Submit {
            opening_preset: self.next_preset,
            options: if let Some(id) = id {
                self.transcripts[id]
                    .send_options
                    .then(|| self.options(Some(id)))
            } else {
                self.next_options.clone()
            },
            owner,
            expected_turn: id.and_then(|id| self.transcripts[id].active_turn.clone()),
            selection: self.selection,
            command,
            inject: if id.is_none() {
                self.handoff_items.take().unwrap_or_default()
            } else {
                vec![]
            },
            context,
            memory_attachment,
            worker_notice: self.worker_notice_id(),
        };
        if id.is_none() {
            let model = submit
                .options
                .as_ref()
                .map(|s| s.model.clone())
                .or_else(|| match &self.profile.route.billing {
                    Billing::Api(alias) => Some(alias.clone()),
                    Billing::Subscription => None,
                });
            let preset = self.next_access();
            self.next_preset = None;
            self.request(
                Call::New {
                    preset,
                    project: self.profile.project.clone(),
                    model,
                },
                Pending::Compose(Operation::New(submit)),
            )
        } else {
            self.authorize_send(submit)
        }
    }

    fn authorize_send(&mut self, submit: Submit) -> io::Result<()> {
        if self.profile.route == Route::CODEX_SUBSCRIPTION {
            self.request(Call::Account, Pending::Compose(Operation::Account(submit)))
        } else {
            self.transmit(submit)
        }
    }
    fn transmit(&mut self, mut submit: Submit) -> io::Result<()> {
        let id = submit.owner.as_deref().expect("opened thread");
        let t = &self.transcripts[id];
        if !t.ready
            || !t.verified
            || match &submit.command {
                Some(ThreadCommand::Goal(_) | ThreadCommand::Tier(_)) => false,
                Some(_) => t.active_turn.is_some(),
                None => t.active_turn != submit.expected_turn,
            }
            || self.state != Connection::Ready
        {
            let message = "The active turn or billing route changed before sending";
            if let Some(id) = submit.worker_notice {
                self.worker_notice_receipts
                    .push(WorkerNoticeReceipt::Rejected(id));
            }
            self.compose_failed(Operation::Send(submit), message.into());
            return Err(io::Error::other(message));
        }
        if submit.command.is_none() && submit.expected_turn.is_none() && !submit.inject.is_empty() {
            let items = std::mem::take(&mut submit.inject);
            return self.request(
                Call::Inject {
                    thread: id.into(),
                    items,
                },
                Pending::Compose(Operation::Inject(submit)),
            );
        }
        if let Some(command) = &submit.command {
            return self.request(
                Call::Command {
                    thread: id.into(),
                    command: command.clone(),
                },
                Pending::Compose(Operation::Send(submit)),
            );
        }
        let text = self.drafts[&submit.owner]
            .sent
            .as_ref()
            .and_then(SentInput::text)
            .expect("pending draft")
            .to_owned();
        let images = match &self.drafts[&submit.owner].sent {
            Some(SentInput::Draft(_, images)) => images.clone(),
            _ => vec![],
        };
        let call = match &submit.expected_turn {
            Some(turn) => Call::Steer {
                images,
                thread: id.into(),
                turn: turn.clone(),
                text,
                context: submit.context.clone(),
            },
            None => Call::Start {
                options: submit.options.clone(),
                images,
                thread: id.into(),
                text,
                context: submit.context.clone(),
            },
        };
        self.request(call, Pending::Compose(Operation::Send(submit)))
    }
    pub fn stop(&mut self, id: &str) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::Stop(id.into()));
        }
        if self.is_antigravity() {
            return self.antigravity_stop(id);
        }
        if self.is_session_scoped() {
            return self.claude_stop(id);
        }
        let turn = self
            .transcripts
            .get(id)
            .and_then(|t| t.active_turn())
            .ok_or_else(|| io::Error::other("This thread has no active turn"))?
            .to_owned();
        let turn = turn.as_str();
        if let Some(t) = self.transcripts.get_mut(id) {
            t.stop_requested = Some(turn.to_owned());
        }
        self.request(
            Call::Stop {
                thread: id.into(),
                turn: turn.into(),
            },
            Pending::Compose(Operation::Stop(id.into())),
        )
    }

    /// End this client's live process for a thread. The thread stays in the
    /// catalog and opens again (Claude `--resume`, Codex `thread/resume`) on the
    /// next select. A Claude process is dropped, which kills its process group.
    /// A Codex thread is unsubscribed; the host drops the app-server once no
    /// thread in the scope is left.
    pub fn end_session(&mut self, id: &str) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::EndSession(id.into()));
        }
        if self.is_antigravity() {
            return self.antigravity_end(id);
        }
        if self.is_session_scoped() {
            self.claude_end(id)?;
            self.ended_threads.insert(id.into());
            return Ok(());
        }
        if self.state != Connection::Ready {
            return Err(io::Error::other("Reconnect to end this session"));
        }
        // Another client holds the writer lock: OX only views it, so there is nothing of
        // OX's to end (an unsubscribe would only answer notLoaded).
        if self.is_native_live(id) {
            return Err(io::Error::other(
                "Another client is running this thread; OX only follows it",
            ));
        }
        if !self
            .transcripts
            .get(id)
            .is_some_and(|t| t.ready || t.loading)
        {
            return Err(io::Error::other("This thread has no live session"));
        }
        if self.in_terminal(id) {
            return Err(io::Error::other("This thread is open in a terminal"));
        }
        if self
            .transcripts
            .get(id)
            .is_some_and(|t| t.active_turn().is_some())
        {
            // Ending a running turn: interrupt it; the unsubscribe follows anyway.
            let _ = self.stop(id);
        }
        self.cancel_held_resume(id);
        self.cancel_tracked_codex_input(id);
        if let Some(access) = self.applied_access(Some(id)) {
            self.resume_access.insert(id.into(), access);
        }
        let t = self.transcripts.get_mut(id).expect("checked above");
        t.ready = false;
        t.verified = false;
        t.loading = false;
        t.active_turn = None;
        t.error = None;
        self.ended_threads.insert(id.into());
        // A Read, Resume or History still in flight must not reopen the thread when its
        // reply lands after the End: a sibling keeps this scope's runtime up, so nothing
        // else would close it again. The Unsubscribe below follows them on the wire.
        self.pending.retain(|_, (_, p)| !p.loads_history_for(id));
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        self.request(Call::Unsubscribe(id.into()), Pending::Unsubscribe)
    }

    fn cancel_tracked_codex_input(&mut self, thread: &str) {
        if !self
            .drafts
            .get(&Some(thread.to_owned()))
            .is_some_and(|draft| matches!(draft.delivery, InputDelivery::Waiting))
        {
            return;
        }
        let submission = self.pending.iter().find_map(|(&seq, (_, pending))| {
            matches!(pending, Pending::Compose(op @ (Operation::Send(_) | Operation::Account(_)))
                if op.thread() == Some(thread))
            .then_some(seq)
        });
        if let Some(seq) = submission
            && let Some((_, pending)) = self.pending.remove(&seq)
        {
            // A late acceptance must not reactivate an ended session when other
            // threads keep this app-server alive.
            self.link.forget(seq);
            self.failed(
                pending,
                "The session ended before Codex confirmed delivery.".into(),
            );
        }
    }
    /// Threads with a live process or subscription in this local store. A
    /// Claude process is one session; the blank draft process is not listed.
    pub fn live_threads(&self) -> Vec<String> {
        if self.remote().is_some() {
            return vec![];
        }
        if self.is_antigravity() {
            return self.antigravity_live();
        }
        if self.is_session_scoped() {
            return self.claude_live();
        }
        self.transcripts
            .iter()
            .filter(|(id, t)| {
                (t.ready || t.loading) && !t.native_live && !self.terminal_threads.contains(*id)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
    /// The thread belongs to a worker launch waiting for its first input.
    pub fn is_draft(&self, id: &str) -> bool {
        self.remote().is_none() && self.is_session_scoped() && self.claude_is_blank(id)
    }
    /// Whether this client can end the thread's live process (the `end` capability).
    pub fn can_end_session(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "end");
        }
        self.has_live_session(id)
    }
    /// Mark a thread as having no live process (the host seeds this for a route it reopened
    /// after reaping it), so a send to it resumes it.
    pub fn mark_ended(&mut self, id: &str) {
        if self.remote().is_none() {
            self.ended_threads.insert(id.into());
        }
    }
    /// Restore a thread the host knew before it reaped or replaced this route's runtime.
    /// The thread is ended (a send resumes it) even when discovery has not listed it:
    /// Codex gets a minimal row in this project and route, whose resume verifies identity
    /// with the server. `access` is the Claude mode the user had chosen.
    /// Remember a worker session's launch contract (from the host's persisted origin)
    /// unless the live store already knows the exact one it launched under.
    pub fn restore_worker_contract(
        &mut self,
        id: &str,
        contract: crate::claude_worker_contract::Contract,
    ) {
        self.worker_contracts.entry(id.into()).or_insert(contract);
    }
    /// Keep a pinned Claude worker on its exact model when it is resumed or forked.
    /// The host's validated exact ID replaces the picker value the launch passed, so
    /// a continuation cannot drift when that value later points at another model.
    pub fn restore_worker_model(&mut self, id: &str, model: &str) {
        if crate::claude_models::valid_model(model) {
            self.worker_models.insert(id.into(), model.to_owned());
        }
    }
    pub fn restore_ended(
        &mut self,
        id: &str,
        title: &str,
        updated_at: i64,
        access: Option<crate::runtime::Access>,
    ) {
        if self.remote().is_some() {
            return;
        }
        self.ended_threads.insert(id.into());
        if let Some(access) = access {
            self.resume_access.insert(id.into(), access);
        }
        if !self.is_session_scoped() && !self.is_antigravity() && self.catalog.get(id).is_none() {
            self.catalog.insert(crate::codex::history::ThreadRow {
                worker: false,
                parent_thread_id: None,
                agent_nickname: None,
                id: id.into(),
                cwd: self.profile.project.clone(),
                model_provider: self.profile.provider.clone(),
                model: None,
                name: (!title.trim().is_empty()).then(|| title.to_owned()),
                preview: String::new(),
                generated_title: None,
                updated_at,
                status: None,
                unread: false,
                activity_order: 0,
            });
            self.catalog_revision += 1;
            self.changed.mark(Changed::THREADS);
        }
    }
    /// The live process was ended and nothing has started it again. The row stays
    /// in the catalog; selecting it resumes the thread. A remote client reads the
    /// host's `ended` capability.
    pub fn is_ended(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "ended");
        }
        self.ended_threads.contains(id) && !self.has_live_session(id)
    }
    /// This thread has a live process or subscription to end.
    pub fn has_live_session(&self, id: &str) -> bool {
        if self.remote().is_some() {
            return false;
        }
        if self.is_antigravity() {
            return self.antigravity_has_live(id);
        }
        if self.is_session_scoped() {
            return self.claude_has_live(id);
        }
        self.transcripts.get(id).is_some_and(|t| {
            (t.ready || t.loading) && !t.native_live && !self.terminal_threads.contains(id)
        })
    }
    /// Something in this thread would be lost by ending it now.
    pub fn thread_busy(&self, id: &str) -> bool {
        self.transcript(id)
            .is_some_and(|t| t.active_turn().is_some())
            || self.sending(Some(id))
            || self.asks.iter().any(|a| a.thread.as_deref() == Some(id))
    }

    pub(super) fn compose_failed(&mut self, op: Operation, reason: String) {
        if let Operation::New(s) = &op
            && s.selection == self.selection
            && self.next_preset.is_none()
        {
            // A failed open leaves its input available for retry. Preserve the
            // mode chosen for that same draft, including an explicitly lower one.
            self.next_preset = s.opening_preset;
        }
        let owner = match op {
            Operation::New(s)
            | Operation::Account(s)
            | Operation::Send(s)
            | Operation::Inject(s) => {
                if let Some(d) = self.drafts.get_mut(&s.owner)
                    && let Some(SentInput::Draft(mut sent, mut images)) = d.sent.take()
                    && s.worker_notice.is_none()
                {
                    if matches!(d.delivery, InputDelivery::Waiting) {
                        // The replica keeps its submitted draft and receives the
                        // private result. Do not restore it to the shared host store
                        // or project a native error that may echo its content.
                        d.complete_delivery(Err(reason));
                        return;
                    }
                    images.append(&mut d.images);
                    d.images = images;
                    if !d.text.is_empty() {
                        if !sent.is_empty() {
                            sent.push('\n');
                        }
                        sent.push_str(&d.text);
                    }
                    d.text = sent;
                }
                s.owner
            }
            Operation::Stop(id) => {
                // A refused Stop leaves the turn running: when it completes it did so on
                // its own, and must not render as Stopped.
                if let Some(t) = self.transcripts.get_mut(&id) {
                    t.stop_requested = None;
                }
                Some(id)
            }
        };
        if let Some(t) = owner.and_then(|id| self.transcripts.get_mut(&id)) {
            t.error = Some(reason);
        } else {
            self.notice = Some(reason);
        }
        self.changed.mark(Changed::NOTICE);
    }
    pub(super) fn compose_reply(&mut self, op: Operation, reply: Reply) -> io::Result<()> {
        match (op, reply) {
            (Operation::New(s), Reply::Opened(reply)) => self.first_send(s, *reply),
            (Operation::Inject(s), Reply::Acknowledged) => self.transmit(s),
            (Operation::Account(s), Reply::Account(account)) if account.chatgpt() => {
                self.transmit(s)
            }
            (Operation::Account(s), Reply::Account(_)) => {
                if let Some(id) = s.worker_notice {
                    self.worker_notice_receipts
                        .push(WorkerNoticeReceipt::Rejected(id));
                }
                self.state = Connection::SignInRequired;
                self.compose_failed(
                    Operation::Account(s),
                    "Sign in to ChatGPT before sending".into(),
                );
                Ok(())
            }
            (Operation::Send(s), reply) if s.command.is_some() => {
                let id = s.owner.as_deref().expect("command thread");
                let cmd = s.command.as_ref().unwrap();
                let notice = match (cmd, reply) {
                    (ThreadCommand::Goal(crate::slash::Goal::Clear), Reply::Cleared(v)) => {
                        if v.cleared {
                            "Goal cleared.".into()
                        } else {
                            "This session has no goal.".into()
                        }
                    }
                    (ThreadCommand::Goal(_), Reply::Goal(v)) => v.goal.map_or_else(
                        || "This session has no goal.".into(),
                        |g| format!("Goal ({}): {}", g.status, g.objective),
                    ),
                    (ThreadCommand::Tier(tier), Reply::Acknowledged) => {
                        self.transcripts.get_mut(id).unwrap().tier.clone_from(tier);
                        format!(
                            "Session speed set to {}.",
                            tier.as_deref().unwrap_or("default")
                        )
                    }
                    (ThreadCommand::Compact, Reply::Acknowledged) => {
                        "Codex started compacting the session.".into()
                    }
                    (ThreadCommand::Rename(name), Reply::Acknowledged) => {
                        if let Some(row) = self.catalog.get_mut(id) {
                            row.name = Some(name.clone());
                        }
                        format!("Session renamed to {name}.")
                    }
                    _ => {
                        self.compose_failed(
                            Operation::Send(s),
                            "Unexpected reply to thread command".into(),
                        );
                        return Ok(());
                    }
                };
                self.transcripts.get_mut(id).unwrap().command_notice = Some(notice);
                self.drafts.get_mut(&s.owner).unwrap().sent = None;
                Ok(())
            }
            (Operation::Send(s), Reply::Turn(turn)) if s.expected_turn.is_none() => {
                if let Some(id) = s.worker_notice {
                    self.worker_notice_receipts
                        .push(WorkerNoticeReceipt::Accepted(id));
                }
                let id = s.owner.as_deref().unwrap();
                if let Some(attachment) = s.memory_attachment {
                    self.remember_memory_attachment(id, attachment);
                }
                let t = self.transcripts.get_mut(id).unwrap();
                if turn.status == "inProgress" && t.last_finished.as_ref() != Some(&turn.id) {
                    t.active_turn = Some(turn.id);
                }
                let draft = self.drafts.get_mut(&s.owner).unwrap();
                draft.sent = None;
                draft.complete_delivery(Ok(()));
                Ok(())
            }
            (Operation::Send(s), Reply::Steered(turn))
                if s.expected_turn.as_ref() == Some(&turn) =>
            {
                let draft = self.drafts.get_mut(&s.owner).unwrap();
                draft.sent = None;
                draft.complete_delivery(Ok(()));
                Ok(())
            }
            (Operation::Stop(_), Reply::Acknowledged) => Ok(()),
            (op, _) => {
                self.compose_failed(op, "Unexpected response to the turn request".into());
                Ok(())
            }
        }
    }
    fn first_send(&mut self, mut s: Submit, reply: ResumedThread) -> io::Result<()> {
        let id = reply.thread.id.clone();
        if let Some(o) = &s.options
            && self.profile.route.is_api()
        {
            self.authorized_models.insert(id.clone(), o.model.clone());
        }
        if let Err(e) = self.verify_opened(&id, &reply).and_then(|()| {
            if s.options
                .as_ref()
                .is_some_and(|wanted| wanted.model != reply.model)
            {
                Err(io::Error::other(
                    "The runtime opened a different model. Choose the intended billing route.",
                ))
            } else {
                Ok(())
            }
        }) {
            self.authorized_models.remove(&id);
            self.compose_failed(Operation::New(s), e.to_string());
            return Ok(());
        }
        if self.profile.route.is_api() {
            self.authorized_models
                .insert(id.clone(), reply.model.clone());
        }
        self.host_opened = Some(id.clone());
        let mut draft = self.drafts.remove(&None).unwrap_or_default();
        if self.selection == s.selection && self.selected.is_none() {
            self.selected = Some(id.clone());
            self.next_options = None;
        } else {
            // The composer moved on. Its text and images stay together with the new chat.
            let composer = self.drafts.entry(None).or_default();
            composer.text = std::mem::take(&mut draft.text);
            composer.images = std::mem::take(&mut draft.images);
        }
        let mut row = reply.thread;
        row.model = Some(reply.model.clone());
        row.updated_at = row.updated_at.max(super::catalog::now_secs());
        self.record_row(row);
        if s.command.is_none() {
            self.prompt_preview(&id, super::catalog::PromptSource::FirstTurn, || {
                draft
                    .sent
                    .as_ref()
                    .and_then(SentInput::text)
                    .and_then(crate::codex::history::first_line)
            });
        }
        if let Some(text) = draft.sent.as_ref().and_then(SentInput::text) {
            self.first_title_prompt(&id, text);
        }
        let t = self.transcripts.entry(id.clone()).or_default();
        t.model = Some(reply.model);
        t.tier = reply.service_tier;
        t.effort = reply.reasoning_effort;
        if let Some(o) = &s.options {
            t.send_options = true;
            t.effort = o.effort.clone();
            t.tier = o.tier.clone();
        }
        t.access = Some(reply.permissions.access);
        t.ceiling = reply.permissions.ceiling;
        t.verified = true;
        t.ready = true;
        s.owner = Some(id);
        self.drafts.insert(s.owner.clone(), draft);
        self.authorize_send(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::frame::{Frame, RequestId, RpcError};
    use crate::links::codex::{Launch, Link, LinkEvent};
    use crate::mentions::ImageInput;
    use crate::store::Profile;

    fn ready() -> Store {
        let directory = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: directory.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut store = Store::open(
            Profile {
                project: directory,
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        store.state = Connection::Ready;
        store
    }

    #[test]
    fn ending_a_ready_codex_thread_unsubscribes_and_marks_it_not_ready() {
        let mut store = ready();
        let id = "thread-ready".to_owned();
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        t.active_turn = Some("turn-1".into());
        assert!(store.can_end_session(&id));
        store.end_session(&id).unwrap();
        let t = store.transcripts.get(&id).unwrap();
        assert!(!t.ready && !t.verified && !t.loading);
        assert!(t.active_turn().is_none(), "an ended turn is not running");
        assert!(
            store
                .pending
                .values()
                .any(|(_, pending)| matches!(pending, Pending::Unsubscribe)),
            "the host must receive the unsubscribe"
        );
        assert!(store.is_ended(&id));
        assert!(!store.can_end_session(&id));
        assert!(store.end_session(&id).is_err());
    }

    #[test]
    fn text_only_model_preserves_image_for_the_next_choice() {
        let mut store = ready();
        let mut caps = crate::slash::Capabilities::default();
        caps.add_model(
            serde_json::from_value(serde_json::json!({
                "id":"fixture/plain","isDefault":true,"inputModalities":["text"]
            }))
            .unwrap(),
        );
        store.install_command_schema(caps).unwrap();
        store.attach_image(None, ImageInput::File("/tmp/art.png".into()));
        let error = store.submit(None).unwrap_err();
        assert!(error.to_string().contains("accepts text only"));
        assert_eq!(store.draft_images(None)[0].label(), "art.png");
        assert!(!store.sending(None));
    }

    #[test]
    fn new_chat_images_stay_with_its_text_when_the_thread_opens_elsewhere() {
        let mut store = ready();
        let project = store.profile.project.clone();
        let submit = Submit {
            opening_preset: None,
            options: None,
            owner: None,
            expected_turn: None,
            selection: store.selection,
            command: None,
            inject: vec![],
            context: None,
            memory_attachment: None,
            worker_notice: None,
        };
        // While the thread opens, the new-chat composer gets text and an image; the operator
        // then selects another thread, so the opened thread must not take either.
        store.edit(None, "typed while pending".into());
        store.attach_image(None, ImageInput::File("/tmp/next.png".into()));
        store.selection += 1;
        let reply: ResumedThread = serde_json::from_value(serde_json::json!({
            "thread": {"id":"new-thread","cwd":project,"modelProvider":"openai"},
            "modelProvider":"openai",
            "model":"fixture/model",
            "serviceTier":null,
            "reasoningEffort":null,
            "initialTurnsPage":null
        }))
        .unwrap();
        store.first_send(submit, reply).unwrap();
        assert_eq!(store.draft(None), "typed while pending");
        assert_eq!(
            store
                .draft_images(None)
                .iter()
                .map(ImageInput::label)
                .collect::<Vec<_>>(),
            ["next.png"]
        );
        assert_eq!(store.draft(Some("new-thread")), "");
        assert!(store.draft_images(Some("new-thread")).is_empty());
    }

    #[test]
    fn worker_notice_rejection_and_user_edits_are_independent() {
        let mut store = ready();
        let owner = "parent";
        store.profile.route = Route::codex_api("fixture".into());
        let t = store.transcripts.entry(owner.into()).or_default();
        t.ready = true;
        t.verified = true;
        for invalid_reply in [false, true] {
            store
                .dispatch_as(
                    crate::actions::Action::SendText(Some(owner.into()), "worker update".into()),
                    crate::surface::Sender::WorkerNotice { id: 42 },
                )
                .unwrap();
            store.edit(Some(owner), "my draft".into());
            store.apply(LinkEvent::Frame(Frame::Reply {
                id: RequestId::Number(store.seq),
                result: Err(RpcError {
                    invalid_reply,
                    code: -32603,
                    message: "refused".into(),
                    data: None,
                }),
            }));
            assert_eq!(store.draft(Some(owner)), "my draft");
            store.edit(Some(owner), String::new());
            let receipts = store.take_worker_notice_receipts();
            if invalid_reply {
                assert!(receipts.is_empty());
            } else {
                assert_eq!(receipts, [WorkerNoticeReceipt::Rejected(42)]);
            }
        }
    }

    #[test]
    fn image_only_failure_restores_sent_files_before_edits_made_in_flight() {
        let mut store = ready();
        store.attach_image(None, ImageInput::File("/tmp/first.png".into()));
        store.submit(None).unwrap();
        assert!(store.draft_images(None).is_empty());
        store.edit(None, "in flight".into());
        store.attach_image(None, ImageInput::File("/tmp/second.png".into()));
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(store.seq),
            result: Err(RpcError {
                invalid_reply: false,
                code: -1,
                message: "fixture".into(),
                data: None,
            }),
        }));
        assert_eq!(store.draft(None), "in flight");
        assert_eq!(
            store
                .draft_images(None)
                .iter()
                .map(ImageInput::label)
                .collect::<Vec<_>>(),
            ["first.png", "second.png"]
        );
        store.remove_image(None, 0);
        assert_eq!(store.draft_images(None)[0].label(), "second.png");
        assert!(store.draft_images(Some("another-thread")).is_empty());
    }

    #[test]
    fn a_tracked_codex_input_reports_the_native_outcome_once() {
        use crate::codex::reply::TurnStatus;
        let mut store = ready();
        let owner = "existing";
        store.profile.route = Route::codex_api("fixture".into());
        let thread = store.transcripts.entry(owner.into()).or_default();
        thread.ready = true;
        thread.verified = true;
        for expected in [Err("native rejected the send".to_owned()), Ok(())] {
            store
                .send_client_message(Some(owner.into()), "my review draft".into(), vec![])
                .unwrap();
            assert!(store.track_codex_input(owner));
            assert_eq!(store.take_codex_input_delivery(owner), None);
            let result = match &expected {
                Ok(()) => Ok(Reply::Turn(TurnStatus {
                    id: "turn".into(),
                    status: "inProgress".into(),
                })),
                Err(message) => Err(RpcError {
                    invalid_reply: false,
                    code: -32603,
                    message: message.clone(),
                    data: None,
                }),
            };
            store.apply(LinkEvent::Frame(Frame::Reply {
                id: RequestId::Number(store.seq),
                result,
            }));
            assert!(store.draft(Some(owner)).is_empty());
            assert_eq!(store.take_codex_input_delivery(owner), Some(expected));
            assert_eq!(store.transcript(owner).unwrap().error(), None);
            assert_eq!(
                store.take_codex_input_delivery(owner),
                Some(Err(UNCONFIRMED_INPUT.into()))
            );
        }
    }

    #[test]
    fn ending_a_tracked_codex_send_ignores_late_account_and_turn_replies() {
        use crate::codex::reply::TurnStatus;
        let account =
            serde_json::from_value(serde_json::json!({"account":{"type":"chatgpt"}})).unwrap();
        for (route, late_reply) in [
            (Route::CODEX_SUBSCRIPTION, Reply::Account(account)),
            (
                Route::codex_api("fixture".into()),
                Reply::Turn(TurnStatus {
                    id: "late-turn".into(),
                    status: "inProgress".into(),
                }),
            ),
        ] {
            let mut store = ready();
            store.profile.route = route;
            let owner = "ended";
            for id in [owner, "sibling"] {
                let thread = store.transcripts.entry(id.into()).or_default();
                thread.ready = true;
                thread.verified = true;
            }
            store
                .send_client_message(Some(owner.into()), "review context".into(), vec![])
                .unwrap();
            assert!(store.track_codex_input(owner));
            let send = store.seq;
            store.edit(Some(owner), "edits made while sending".into());
            store.end_session(owner).unwrap();
            assert_eq!(
                store.take_codex_input_delivery(owner),
                Some(Err(
                    "The session ended before Codex confirmed delivery.".into()
                ))
            );
            assert_eq!(store.draft(Some(owner)), "edits made while sending");
            let sequence_after_end = store.seq;
            store.apply(LinkEvent::Frame(Frame::Reply {
                id: RequestId::Number(send),
                result: Ok(late_reply),
            }));
            assert_eq!(store.seq, sequence_after_end);
            assert!(!store.thread_busy(owner));
            assert!(!store.has_live_session(owner));
            assert!(store.has_live_session("sibling"));
        }
    }

    #[test]
    fn discovery_updates_the_open_composer_during_a_turn() {
        let mut store = ready();
        let row = serde_json::from_value(serde_json::json!({
            "id": "active", "cwd": store.profile.project, "modelProvider": "openai"
        }))
        .unwrap();
        store.catalog.insert(row);
        let t = store.transcripts.entry("active".into()).or_default();
        t.ready = true;
        t.verified = true;
        t.active_turn = Some("turn".into());
        let scope = crate::host_protocol::Scope {
            project: store.profile.project.clone(),
            route: store.profile.route.clone(),
        };
        let mut state = store.host_snapshot(&scope);
        assert!(
            !state.sessions[0]
                .capabilities
                .iter()
                .any(|cap| cap == "model_options")
        );
        store.changed = Changed::default();
        let mut caps = crate::slash::Capabilities {
            goals: true,
            ..Default::default()
        };
        std::sync::Arc::make_mut(&mut caps.methods).extend(
            [
                "thread/settings/update",
                "thread/goal/get",
                "thread/goal/set",
                "thread/goal/clear",
            ]
            .map(str::to_owned),
        );
        store.install_command_schema(caps).unwrap();
        store.host_project(&scope, &store.changed, &mut state);
        let row = &state.sessions[0];
        for cap in ["model_options", "native_commands", "steer"] {
            assert!(row.capabilities.iter().any(|c| c == cap), "{cap}");
        }
        let commands = state.detail["commands"]["choices"].as_array().unwrap();
        for name in ["/goal", "/model"] {
            assert!(commands.iter().any(|row| row[0] == name), "{name}");
        }
    }

    #[test]
    fn sending_to_an_ended_codex_thread_reopens_it_then_delivers_the_message() {
        let mut store = ready();
        let id = "thread-ended".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai", "preview": "hi"
        }))
        .unwrap();
        store.catalog.insert(row);
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        store.end_session(&id).unwrap();
        // The composer stays usable, and a plain send is accepted rather than refused.
        assert!(store.is_ended(&id) && !store.can_send(Some(&id)));
        assert!(store.can_compose(Some(&id)));
        store.edit(Some(&id), "back again".into());
        store.submit(Some(&id)).unwrap();
        assert!(store.sending(Some(&id)), "the message waits for the resume");
        assert!(!store.is_ended(&id), "live again, so no ended marker");
        assert_eq!(store.draft(Some(&id)), "back again");
        assert!(
            store
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::Read(t) if t == &id)),
            "the thread is read and resumed"
        );
        // Still resuming: nothing is sent yet.
        store.drain(vec![], std::time::Instant::now());
        assert!(store.sending(Some(&id)));
        // Once the thread is verified and ready the queued message is submitted.
        let t = store.transcripts.get_mut(&id).unwrap();
        t.loading = false;
        t.ready = true;
        t.verified = true;
        store.drain(vec![], std::time::Instant::now());
        assert!(
            store.pending.values().any(|(_, p)| matches!(
                p,
                Pending::Compose(Operation::Account(s)) if s.owner.as_deref() == Some(id.as_str())
            )),
            "the draft is submitted"
        );
        assert_eq!(store.draft(Some(&id)), "");
    }

    fn phone() -> crate::surface::Sender {
        crate::surface::Sender::Person {
            surface: crate::surface::Surface::Phone,
        }
    }
    fn submitted_context(store: &Store, id: Option<&str>) -> Option<Option<String>> {
        store.pending.values().find_map(|(_, p)| match p {
            Pending::Compose(Operation::Account(s) | Operation::Send(s) | Operation::New(s))
                if s.owner.as_deref() == id =>
            {
                Some(s.context.clone())
            }
            _ => None,
        })
    }

    #[test]
    fn a_send_and_a_steer_carry_the_stamp_they_were_accepted_with() {
        let mut store = ready();
        let id = "thread-live".to_owned();
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        store
            .dispatch_as(
                crate::actions::Action::SendText(Some(id.clone()), "where are settings?".into()),
                phone(),
            )
            .unwrap();
        let context = submitted_context(&store, Some(&id)).unwrap().unwrap();
        assert!(context.starts_with("This message was sent from the OX phone app."));
        assert!(store.sender.is_none(), "the stamp is per action");
        // The same stamp reaches the wire as one application entry; the user text is intact.
        let Some(Pending::Compose(Operation::Account(submit))) = store
            .pending
            .drain()
            .map(|(_, (_, p))| p)
            .find(|p| matches!(p, Pending::Compose(Operation::Account(_))))
        else {
            panic!("account check");
        };
        store.transmit(submit).unwrap();
        let Some(Pending::Compose(Operation::Send(submit))) =
            store.pending.drain().map(|(_, (_, p))| p).next()
        else {
            panic!("turn/start");
        };
        assert_eq!(submit.context.as_deref(), Some(context.as_str()));

        // A steer from the desktop while the turn runs keeps the desktop's own stamp.
        store.drafts.remove(&Some(id.clone()));
        let t = store.transcripts.get_mut(&id).unwrap();
        t.active_turn = Some("turn-1".into());
        store
            .dispatch_as(
                crate::actions::Action::SendText(Some(id.clone()), "also check phone".into()),
                crate::surface::Sender::Person {
                    surface: crate::surface::Surface::Desktop,
                },
            )
            .unwrap();
        let steer = store
            .pending
            .values()
            .find_map(|(_, p)| match p {
                Pending::Compose(Operation::Account(s)) => Some(s),
                _ => None,
            })
            .unwrap();
        assert_eq!(steer.expected_turn.as_deref(), Some("turn-1"));
        assert!(steer.context.as_deref().unwrap().contains("OX Desktop"));
    }

    #[test]
    fn an_unstamped_send_attaches_no_context() {
        let mut store = ready();
        store.edit(None, "hello".into());
        store.submit(None).unwrap();
        assert_eq!(submitted_context(&store, None), Some(None));
    }

    #[test]
    fn fixed_host_start_honors_a_lower_draft_and_never_restores_a_broader_preset() {
        use crate::{host_policy::HostPolicy, runtime::Access};
        let mut store = ready();
        store.set_start_mode(|_| Some(Access::Yolo));
        assert_eq!(store.next_access(), Some(Access::Yolo));
        store.use_host_policy(HostPolicy::AlwaysRead);
        assert_eq!(store.next_access(), Some(Access::ReadOnly));
        store.use_host_policy(HostPolicy::AlwaysWrite);
        store.set_next_access(Access::Yolo);
        assert_eq!(store.next_access(), Some(Access::Workspace));
        store.set_next_access(Access::ReadOnly);
        assert_eq!(store.next_access(), Some(Access::ReadOnly));
        store.use_host_policy(HostPolicy::FollowInterface);
        store.set_next_access(Access::Yolo);
        assert_eq!(store.next_access(), Some(Access::Yolo));
    }

    #[test]
    fn a_failed_new_session_restores_its_chosen_lower_mode_for_retry() {
        use crate::{host_policy::HostPolicy, runtime::Access};
        let mut store = ready();
        store.use_host_policy(HostPolicy::AlwaysYolo);
        store.set_next_access(Access::ReadOnly);
        store.edit(None, "retry me".into());
        store.submit(None).unwrap();
        let seq = store.seq;
        let (_, pending) = store.pending.remove(&seq).unwrap();
        store.failed(pending, "fixture refused to open".into());
        assert_eq!(store.next_access(), Some(Access::ReadOnly));
        assert_eq!(store.draft(None), "retry me");
    }

    #[test]
    fn a_resumed_codex_yolo_turn_waits_with_its_original_draft_until_approved() {
        use crate::{
            codex::history::{ResumedThread, TurnPage},
            host_policy::HostPolicy,
            modes::Level,
        };
        let mut store = ready();
        store.use_host_policy(HostPolicy::YoloAsksEveryTime);
        let id = "saved-bypass".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai", "preview":"work"
        }))
        .unwrap();
        store.catalog.insert(row.clone());
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        store.end_session(&id).unwrap();
        store.edit(Some(&id), "continue this session".into());
        store
            .dispatch_as(crate::actions::Action::Submit(Some(id.clone())), phone())
            .unwrap();
        let reply = ResumedThread {
            permissions: crate::codex::access::Permissions {
                access: crate::runtime::Access::Yolo,
                ceiling: Some(Level::Yolo),
            },
            thread: row,
            model_provider: "openai".into(),
            model: "test".into(),
            service_tier: None,
            reasoning_effort: None,
            initial_turns_page: Some(TurnPage {
                data: vec![],
                next_cursor: None,
            }),
        };
        store.checked_resume(&id, reply);
        store.drain(vec![], std::time::Instant::now());
        assert!(store.resume_sends.contains(&id));
        assert_eq!(store.new_resume_reviews(), vec![id.clone()]);
        assert!(store.new_resume_reviews().is_empty());
        assert!(submitted_context(&store, Some(&id)).is_none());
        store.accept_resume_review(&id);
        store.drain(vec![], std::time::Instant::now());
        assert!(
            submitted_context(&store, Some(&id))
                .unwrap()
                .unwrap()
                .contains("OX phone app")
        );
    }

    #[test]
    fn a_deferred_resume_send_keeps_its_own_stamp() {
        let mut store = ready();
        let id = "thread-ended".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai", "preview": "hi"
        }))
        .unwrap();
        store.catalog.insert(row);
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        store.end_session(&id).unwrap();
        store.edit(Some(&id), "from the phone".into());
        store
            .dispatch_as(crate::actions::Action::Submit(Some(id.clone())), phone())
            .unwrap();
        assert!(store.resume_senders.contains_key(&id));
        let t = store.transcripts.get_mut(&id).unwrap();
        t.loading = false;
        t.ready = true;
        t.verified = true;
        // The thread becomes ready while the host dispatches something from the desktop.
        store.sender = Some(crate::surface::Sender::Person {
            surface: crate::surface::Surface::Desktop,
        });
        store.drain(vec![], std::time::Instant::now());
        store.sender = None;
        let context = submitted_context(&store, Some(&id)).unwrap().unwrap();
        assert!(context.contains("OX phone app"), "{context}");
        assert!(store.resume_senders.is_empty());
    }

    /// The host reaped the route and reopened it for a send: the replacement store is
    /// connecting, and the thread (fresh, so absent from discovery) exists only as a
    /// recovered id.
    #[test]
    fn a_connecting_codex_store_resumes_a_recovered_thread_absent_from_discovery() {
        let mut store = ready();
        store.state = Connection::Connecting;
        store.restore_ended("fresh", "Fresh thread", 7, None);
        assert!(store.can_compose(Some("fresh")));
        // The send arrives while the runtime is still connecting.
        store.edit(Some("fresh"), "after the reap".into());
        store.submit(Some("fresh")).unwrap();
        assert!(store.sending(Some("fresh")));
        assert_eq!(
            store.draft(Some("fresh")),
            "after the reap",
            "kept until delivery"
        );
        // The route comes up and lists the project; discovery does not know the thread.
        store.connected().unwrap();
        let list = store
            .pending
            .iter()
            .find_map(|(id, (_, p))| matches!(p, Pending::List { .. }).then_some(*id))
            .expect("first listing");
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(list),
            result: Ok(Reply::Threads(crate::codex::history::ThreadPage {
                data: vec![],
                next_cursor: None,
            })),
        }));
        assert!(store.catalog.get("fresh").is_some(), "the listing keeps it");
        assert!(
            store
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::Read(t) if t == "fresh"))
        );
        let t = store.transcripts.get_mut("fresh").unwrap();
        t.loading = false;
        t.ready = true;
        t.verified = true;
        store.drain(vec![], std::time::Instant::now());
        let delivered = store
            .pending
            .values()
            .filter(|(_, p)| matches!(p, Pending::Compose(Operation::Account(s) | Operation::Send(s)) if s.owner.as_deref() == Some("fresh")))
            .count();
        assert_eq!(delivered, 1, "exactly one delivery");
        assert_eq!(store.draft(Some("fresh")), "");
    }

    /// End with a sibling keeping the runtime up: a Read, Resume or History that was
    /// already in flight must not bring the ended thread back when its reply lands.
    #[test]
    fn replies_in_flight_when_a_codex_thread_ends_do_not_reopen_it() {
        let mut store = ready();
        let t = store.transcripts.entry("gone".into()).or_default();
        t.ready = true;
        t.verified = true;
        for (n, p) in [
            Pending::Read("gone".into()),
            Pending::Resume("gone".into()),
            Pending::History("gone".into()),
            Pending::ReadFullHistory("gone".into()),
            Pending::ResumeFullHistory("gone".into()),
            Pending::Read("sibling".into()),
        ]
        .into_iter()
        .enumerate()
        {
            store.pending.insert(
                900 + n as i64,
                (
                    std::time::Instant::now() + std::time::Duration::from_secs(60),
                    p,
                ),
            );
        }
        store.end_session("gone").unwrap();
        assert!(
            store
                .pending
                .values()
                .all(|(_, p)| !p.loads_history_for("gone")),
            "late replies for the ended thread are dropped"
        );
        assert!(
            store
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::Read(t) if t == "sibling")),
            "other threads keep their requests"
        );
        assert!(!store.has_live_session("gone"));
    }

    #[test]
    fn a_recovered_thread_on_the_api_route_is_delivered_once_without_an_account_check() {
        let mut store = ready();
        store.profile.route = crate::runtime::Runtime::Codex.route(Some("alias".into()));
        store.state = Connection::Connecting;
        store.restore_ended("fresh", "Fresh", 1, None);
        store.edit(Some("fresh"), "api send".into());
        store.submit(Some("fresh")).unwrap();
        store.connected().unwrap();
        let t = store.transcripts.get_mut("fresh").unwrap();
        t.loading = false;
        t.ready = true;
        t.verified = true;
        store.drain(vec![], std::time::Instant::now());
        let sends: Vec<_> = store
            .pending
            .values()
            .filter(|(_, p)| matches!(p, Pending::Compose(_)))
            .collect();
        assert_eq!(sends.len(), 1, "exactly one delivery");
        assert!(matches!(sends[0].1, Pending::Compose(Operation::Send(_))));
    }

    #[test]
    fn an_image_only_draft_resumes_an_ended_codex_thread_and_the_ended_row_keeps_attach_image() {
        let mut store = ready();
        store.restore_ended("pics", "Pictures", 1, None);
        let view = store.host_snapshot(&crate::host_protocol::Scope {
            project: store.profile.project.clone(),
            route: store.profile.route.clone(),
        });
        let caps = &view
            .sessions
            .iter()
            .find(|s| s.id == "pics")
            .unwrap()
            .capabilities;
        assert!(caps.iter().any(|c| c == "send") && caps.iter().any(|c| c == "ended"));
        assert!(caps.iter().any(|c| c == "attach_image"), "{caps:?}");
        store.attach_image(Some("pics"), ImageInput::File("/tmp/shot.png".into()));
        store.submit(Some("pics")).unwrap();
        assert!(store.sending(Some("pics")), "an image alone is a message");
        assert_eq!(
            store.draft_images(Some("pics")).len(),
            1,
            "kept until delivery"
        );
    }

    #[test]
    fn a_failed_resume_keeps_the_message_in_the_composer() {
        let mut store = ready();
        let id = "thread-gone".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai"
        }))
        .unwrap();
        store.catalog.insert(row);
        store.mark_ended(&id);
        store.edit(Some(&id), "keep me".into());
        store.submit(Some(&id)).unwrap();
        let t = store.transcripts.get_mut(&id).unwrap();
        t.loading = false;
        t.error = Some("gone".into());
        store.drain(vec![], std::time::Instant::now());
        assert!(!store.sending(Some(&id)));
        assert_eq!(store.draft(Some(&id)), "keep me");
        assert!(store.is_ended(&id));
    }

    #[test]
    fn a_codex_thread_held_by_another_client_is_read_only_and_forkable() {
        let mut store = ready();
        let id = "thread-native".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai"
        }))
        .unwrap();
        store.catalog.insert(row);
        store.selected = Some(id.clone());
        store.transcripts.entry(id.clone()).or_default().loading = true;
        // Codex refuses a second writer with this text; it is not a failure of the thread.
        store.failed(
            Pending::Resume(id.clone()),
            format!("thread {id} already has an active writer"),
        );
        let t = store.transcript(&id).unwrap();
        assert!(t.error().is_none());
        assert!(store.is_native_live(&id));
        assert!(!store.can_send(Some(&id)) && !store.can_compose(Some(&id)));
        assert!(store.can_manage_thread(&id), "forkable into a new session");
        assert!(
            store
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::History(t) if t == &id)),
            "the history is read without resuming"
        );
        assert_eq!(store.native_app(), "Codex");
        assert!(store.native_live_hint().contains("running in Codex"));
    }

    #[test]
    fn a_viewed_codex_thread_offers_only_what_a_viewer_can_own() {
        let mut store = ready();
        let id = "thread-viewed".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": store.profile.project, "modelProvider": "openai"
        }))
        .unwrap();
        store.catalog.insert(row);
        store.selected = Some(id.clone());
        store.transcripts.entry(id.clone()).or_default().loading = true;
        store.failed(
            Pending::Resume(id.clone()),
            format!("thread {id} already has an active writer"),
        );
        // The rollout read lands: the viewer now has a ready transcript.
        let page: crate::codex::history::TurnPage = serde_json::from_value(serde_json::json!({
            "data": [], "nextCursor": null
        }))
        .unwrap();
        store.history_page(&id, page);
        let t = store.transcripts.get_mut(&id).unwrap();
        t.ready = true;
        t.loading = false;
        assert!(store.is_native_live(&id));
        // Not a process OX owns: no End, not counted, not reaped as a process.
        assert!(!store.has_live_session(&id) && !store.can_end_session(&id));
        assert!(store.live_threads().is_empty());
        assert!(store.end_session(&id).is_err());
        assert!(
            store
                .pending
                .values()
                .all(|(_, p)| !matches!(p, Pending::Unsubscribe)),
            "no unsubscribe is sent for a thread OX does not hold"
        );
        // Fork stays; Archive is refused by Codex for a non-owner, Rename is accepted.
        assert!(store.can_manage_thread(&id));
        assert!(
            store
                .manage_thread(&id, crate::store::ThreadOperation::Archive)
                .is_err()
        );
        let scope = crate::host_protocol::Scope {
            project: store.profile.project.clone(),
            route: store.profile.route.clone(),
        };
        let view = store.host_snapshot(&scope);
        let caps = &view
            .sessions
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .capabilities;
        assert!(caps.iter().any(|c| c == "fork") && caps.iter().any(|c| c == "rename"));
        assert!(
            !caps
                .iter()
                .any(|c| c == "archive" || c == "end" || c == "send"),
            "{caps:?}"
        );
    }

    #[test]
    fn an_outstanding_native_read_does_not_leave_an_overdue_deadline() {
        let mut store = ready();
        let id = "thread-slow";
        store.hold_native_read_for_test(id);
        let overdue = |s: &Store| {
            s.next_deadline()
                .is_some_and(|at| at <= std::time::Instant::now())
        };
        assert!(
            !overdue(&store),
            "the host must sleep, not spin, on a slow read"
        );
        store.refresh_native(std::time::Instant::now(), false);
        assert!(!overdue(&store));
        // Once the read is answered the probe is due again at once.
        store.pending.clear();
        assert!(overdue(&store));
    }

    #[test]
    fn an_ended_selected_thread_loads_its_history_after_a_rebuild() {
        let mut old = ready();
        let id = "thread-ended".to_owned();
        let row: crate::codex::history::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": id, "cwd": old.profile.project, "modelProvider": "openai", "preview": "hi"
        }))
        .unwrap();
        old.catalog.insert(row);
        old.selected = Some(id.clone());
        old.mark_ended(&id);
        old.transcripts.entry(id.clone()).or_default();
        let mut store = ready();
        store.recover_from(&mut old);
        assert!(store.is_ended(&id));
        store.connected().unwrap();
        assert!(
            store
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::Read(t) if t == &id)),
            "the selected ended thread is read again"
        );
        assert!(store.transcript(&id).unwrap().is_loading());
        let page: crate::codex::history::TurnPage = serde_json::from_value(serde_json::json!({
            "data": [], "nextCursor": null
        }))
        .unwrap();
        store.history_page(&id, page);
        assert!(!store.transcript(&id).unwrap().is_loading());
        // An ended thread that is not selected stays closed.
        let mut other = ready();
        other.mark_ended("quiet");
        other.transcripts.entry("quiet".into()).or_default().loading = true;
        let mut next = ready();
        next.recover_from(&mut other);
        next.connected().unwrap();
        assert!(
            next.pending
                .values()
                .all(|(_, p)| !matches!(p, Pending::Read(_)))
        );
    }

    #[test]
    fn a_refused_stop_finishes_done() {
        use crate::codex::activity::{Activity, Change};
        let mut store = ready();
        let id = "thread-stop".to_owned();
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        t.active_turn = Some("turn-1".into());
        store.stop(&id).unwrap();
        store.compose_failed(Operation::Stop(id.clone()), "stop refused".into());
        let t = store.transcripts.get_mut(&id).unwrap();
        t.apply_activity(Activity {
            thread: id.clone(),
            turn: "turn-1".into(),
            change: Change::TurnCompleted {
                error: None,
                interrupted: false,
            },
        });
        assert!(
            !t.interrupted() && t.active_turn().is_none(),
            "rejected stop must finish normally"
        );
        t.apply_activity(Activity {
            thread: id.clone(),
            turn: "turn-2".into(),
            change: Change::TurnStarted,
        });
        assert!(!t.interrupted(), "a new turn clears the marker");
        t.apply_activity(Activity {
            thread: id,
            turn: "turn-2".into(),
            change: Change::TurnCompleted {
                error: None,
                interrupted: false,
            },
        });
        assert!(!t.interrupted(), "a finished turn is done");
    }
    #[test]
    fn a_stopped_turn_completes_as_interrupted_not_done() {
        use crate::codex::activity::{Activity, Change};
        let mut store = ready();
        let id = "thread-stop".to_owned();
        let t = store.transcripts.entry(id.clone()).or_default();
        t.ready = true;
        t.verified = true;
        t.active_turn = Some("turn-1".into());
        store.stop(&id).unwrap();
        let t = store.transcripts.get_mut(&id).unwrap();
        t.apply_activity(Activity {
            thread: id.clone(),
            turn: "turn-1".into(),
            change: Change::TurnCompleted {
                error: None,
                interrupted: false,
            },
        });
        assert!(t.interrupted() && t.active_turn().is_none());
        t.apply_activity(Activity {
            thread: id.clone(),
            turn: "turn-2".into(),
            change: Change::TurnStarted,
        });
        assert!(!t.interrupted(), "a new turn clears the marker");
        t.apply_activity(Activity {
            thread: id,
            turn: "turn-2".into(),
            change: Change::TurnCompleted {
                error: None,
                interrupted: false,
            },
        });
        assert!(!t.interrupted(), "a finished turn is done");
    }

    #[test]
    fn first_turn_attaches_tier1_memory_brief_once_and_not_on_followup() {
        let dir = tempfile::tempdir().unwrap();
        let mem_path = dir.path().join("memory.json");
        let mem_store = ox_memory::MemoryStore::open(&mem_path);
        mem_store
            .upsert(ox_memory::NewMemory {
                id: None,
                scope: ox_memory::Scope::Global,
                kind: ox_memory::Kind::RoutingPolicy,
                sensitivity: ox_memory::Sensitivity::Private,
                text: "Use Claude subscription for confidential work.".into(),
                tags: vec!["routing".into()],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();

        let mut store = ready();
        store.enable_memory(mem_path, "host-a".into());
        store.sender = Some(crate::surface::Sender::Person {
            surface: crate::surface::Surface::Desktop,
        });

        let (turn1, attachment) = store.first_turn_context();
        let turn1 = turn1.unwrap();
        assert_eq!(
            attachment,
            Some(crate::memory::Attachment {
                entries: 1,
                from_parent_host: false
            })
        );
        assert!(turn1.contains("OX Desktop"));
        assert!(turn1.contains("Use Claude subscription for confidential work."));

        let turn2 = store.sender_context().unwrap();
        assert!(turn2.contains("OX Desktop"));
        assert!(!turn2.contains("Use Claude subscription for confidential work."));
    }
}
