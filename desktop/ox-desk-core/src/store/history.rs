use super::{Connection, Pending, Store, ThreadRow};
use crate::{
    codex::{
        history::{ReadThread, ResumedThread, TurnPage},
        request::Call,
    },
    transcript::Transcript,
};

use std::io;

impl Store {
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    pub fn transcript(&self, id: &str) -> Option<&Transcript> {
        self.transcripts.get(id)
    }

    /// Loaded threads with a turn in progress, whether or not the catalog
    /// page lists them (workers, search results, later pages).
    pub fn active_turns(&self) -> usize {
        self.active_thread_ids().count()
    }

    pub fn active_thread_ids(&self) -> impl Iterator<Item = &str> {
        self.transcripts
            .iter()
            .filter_map(|(id, thread)| thread.active_turn().map(|_| id.as_str()))
    }

    pub fn take_remeasure(&mut self, id: &str) -> Vec<usize> {
        self.transcripts
            .get_mut(id)
            .map(Transcript::take_remeasure)
            .unwrap_or_default()
    }

    pub fn select(&mut self, id: &str) -> io::Result<()> {
        if self.remote().is_some() {
            return self
                .dispatch_remote(crate::actions::Action::SelectThread(id.into()))
                .map(|_| ());
        }
        let row = self.catalog.get(id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Thread is outside this catalog")
        })?;
        self.check_route(row)?;
        self.open_catalog_id(id)
    }

    pub(super) fn check_route(&self, row: &ThreadRow) -> io::Result<()> {
        let child = row.parent_thread_id.as_ref().is_some_and(|parent| {
            self.child_threads
                .get(&row.id)
                .is_some_and(|(p, dir)| p == parent && dir == &row.cwd)
        });
        if (row.cwd != self.profile.project
            && !(child && row.cwd.starts_with(&self.profile.project)))
            || row.model_provider != self.profile.provider
            || self
                .expected_alias(&row.id)
                .is_some_and(|alias| row.model.as_ref().is_some_and(|m| m != alias))
        {
            return Err(io::Error::other(
                "Select the saved thread’s project and billing route",
            ));
        }
        Ok(())
    }

    /// Open metadata from another connection to the same project. Read and resume
    /// still verify the identity and applied route with the owning server.
    pub fn select_from_catalog(&mut self, row: ThreadRow) -> io::Result<()> {
        self.select_catalog_origin(row, false)
    }
    /// Add a catalog row without opening the thread, so a send can resume a thread the
    /// freshly reopened route has not listed yet.
    pub fn adopt_catalog_row(&mut self, row: ThreadRow) -> io::Result<()> {
        if self.remote().is_some() || row.worker || self.catalog.get(&row.id).is_some() {
            return Ok(());
        }
        self.check_route(&row)?;
        self.record_lineage(&row);
        if self.listing {
            self.created_while_listing.insert(row.id.clone());
        }
        self.catalog.insert(row);
        self.catalog_revision += 1;
        Ok(())
    }
    pub fn select_catalog_origin(&mut self, row: ThreadRow, confirm_alias: bool) -> io::Result<()> {
        if self.remote().is_some() {
            let a = if confirm_alias {
                crate::actions::Action::ConfirmAliasAndResume(Box::new(row))
            } else {
                crate::actions::Action::OpenCatalogRow(Box::new(row))
            };
            return self.dispatch_remote(a).map(|_| ());
        }
        if confirm_alias
            && (!self.is_session_scoped()
                || !self.profile.route.is_api()
                || !row.needs_claude_alias())
        {
            return Err(io::Error::other(
                "The saved billing origin changed; select this session again",
            ));
        }
        if row.worker {
            if self.is_session_scoped()
                && self
                    .child_threads
                    .get(&row.id)
                    .is_some_and(|(parent, cwd)| {
                        Some(parent) == row.parent_thread_id.as_ref()
                            && cwd == &self.profile.project
                    })
                && self.transcript(&row.id).is_some()
            {
                return self.open_catalog_id(&row.id);
            }
            return Err(io::Error::other(
                "Open the worker on its originating parent connection",
            ));
        }
        self.check_route(&row)?;
        self.record_lineage(&row);
        let id = row.id.clone();
        if self.catalog.get(&id).is_none() {
            if self.listing {
                self.created_while_listing.insert(id.clone());
            }
            self.catalog.insert(row);
            self.catalog_revision += 1;
        } else {
            self.sync_title(&row);
        }
        self.open_catalog_alias(&id, confirm_alias)
    }
    pub(super) fn open_catalog_id(&mut self, id: &str) -> io::Result<()> {
        self.open_catalog_alias(id, false)
    }
    fn open_catalog_alias(&mut self, id: &str, confirm_alias: bool) -> io::Result<()> {
        if self.moving_threads.contains(id) {
            return Err(io::Error::other("This session is moving to another host."));
        }
        self.selection += 1;
        self.selected = Some(id.into());
        let entry = self.transcripts.entry(id.into()).or_default();
        entry.unread = false;
        if let Some(row) = self.catalog.get_mut(id) {
            row.unread = false;
        }
        if ((entry.ready || entry.loading) && !confirm_alias) || self.terminal_threads.contains(id)
        {
            return Ok(());
        }
        if self.state == Connection::Connecting {
            // Still connecting: keep the selection and open it once ready.
            self.queued_open = Some(id.into());
            self.mark_loading(id);
            return Ok(());
        }
        if self.state != Connection::Ready {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Reconnect to open this thread",
            ));
        }
        if self.is_antigravity() {
            return self.antigravity_open(id);
        }
        if self.is_session_scoped() {
            return self.claude_open_alias(id, confirm_alias);
        }
        self.request(Call::Read(id.into()), Pending::Read(id.into()))?;
        self.mark_loading(id);
        Ok(())
    }

    pub(super) fn mark_loading(&mut self, id: &str) {
        let transcript = self.transcripts.entry(id.into()).or_default();
        transcript.loading = true;
        transcript.error = None;
    }

    pub fn load_older(&mut self, id: &str) -> io::Result<()> {
        let Some(t) = self.transcripts.get(id) else {
            return Ok(());
        };
        if t.loading {
            return Ok(());
        }
        let Some(cursor) = t.cursor.clone() else {
            return Ok(());
        };
        self.request_history(id, Some(cursor))
    }

    fn request_history(&mut self, id: &str, cursor: Option<String>) -> io::Result<()> {
        self.request(
            Call::History {
                thread: id.into(),
                cursor,
            },
            Pending::History(id.into()),
        )?;
        self.mark_loading(id);
        Ok(())
    }

    fn verify_identity(&self, id: &str, identity: &ThreadRow) -> io::Result<()> {
        let profile = &self.profile;
        let directory_ok = match (&identity.parent_thread_id, self.child_threads.get(id)) {
            (None, None) => identity.cwd == profile.project,
            (Some(parent), Some((expected, cwd))) => {
                parent == expected
                    && cwd == &identity.cwd
                    && identity.cwd.starts_with(&profile.project)
            }
            _ => false,
        };
        if identity.id != id
            || !directory_ok
            || identity.model_provider != profile.provider
            || self
                .expected_alias(id)
                .is_some_and(|alias| identity.model.as_deref().is_some_and(|m| m != alias))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Thread belongs to a different project, provider, or model",
            ));
        }
        Ok(())
    }

    pub(super) fn checked_read(&mut self, id: &str, read: ReadThread) {
        let result = (|| -> io::Result<()> {
            let identity = read.thread;
            self.verify_identity(id, &identity)?;
            if let Some(expected) = self.expected_alias(id)
                && identity.model.is_none()
                && !self.authorized_models.contains_key(id)
                && self.catalog.get(id).and_then(|row| row.model.as_deref()) != Some(expected)
            {
                return Err(io::Error::other(
                    "The saved API model is unknown. Open a new session on the intended billing route.",
                ));
            }
            self.sync_title(&identity);
            self.transcripts.entry(id.into()).or_default().read_updated = identity.updated_at;
            self.request_resume(id, false)
        })();
        if let Err(err) = result {
            self.failed(Pending::Read(id.into()), err.to_string());
        }
    }

    fn request_resume(&mut self, id: &str, full_history: bool) -> io::Result<()> {
        self.request(
            Call::Resume {
                preset: if self.child_threads.contains_key(id)
                    || self.transcripts.get(id).is_some_and(|t| t.ready)
                {
                    None
                } else {
                    self.host_start_access(id).and_then(|fixed| {
                        self.starting_access(Some(fixed), self.resume_access.get(id).copied())
                    })
                },
                thread: id.into(),
                project: (!self.child_threads.contains_key(id))
                    .then(|| self.profile.project.clone()),
                full_history,
            },
            if full_history {
                Pending::ResumeFullHistory(id.into())
            } else {
                Pending::Resume(id.into())
            },
        )
    }

    /// Some Codex thread stores cannot page a newly discovered worker yet.
    /// Ask once for its full history, preserving the original read/write intent.
    pub(super) fn rpc_failed(&mut self, pending: Pending, error: crate::codex::frame::RpcError) {
        // A malformed success payload is ambiguous: Codex might already have
        // started the turn. An explicit RPC refusal can safely be retried.
        if let Pending::Compose(op) = &pending
            && let Some(id) = op.worker_notice()
            && !error.invalid_reply
        {
            self.worker_notice_receipts
                .push(super::WorkerNoticeReceipt::Rejected(id));
        }
        if error.code != -32601 || error.message != "list_turns is not supported yet" {
            return self.failed(pending, error.message);
        }
        let fallback = match &pending {
            Pending::Resume(id) => self.request_resume(id, true),
            Pending::History(id) => self.request(
                Call::ReadFullHistory(id.clone()),
                Pending::ReadFullHistory(id.clone()),
            ),
            Pending::ReadFullHistory(id) | Pending::ResumeFullHistory(id) => {
                let action = if self.transcript(id).is_some_and(Transcript::has_older) {
                    "Load earlier turns again"
                } else {
                    "Open the session again"
                };
                return self.failed(
                    pending,
                    format!(
                        "Codex could not read the session history. {action} to retry. ({})",
                        error.message
                    ),
                );
            }
            _ => return self.failed(pending, error.message),
        };
        if let Err(error) = fallback {
            self.failed(pending, error.to_string());
        }
    }

    pub(super) fn verify_opened(&self, id: &str, reply: &ResumedThread) -> io::Result<()> {
        self.verify_identity(id, &reply.thread)?;
        if reply.model_provider != self.profile.provider
            || self.expected_alias(id).is_some_and(|m| m != reply.model)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex opened this thread on a different model route than the one you chose. Reconnect, or pick the thread's original route.",
            ));
        }
        Ok(())
    }
    pub(super) fn checked_resume(&mut self, id: &str, reply: ResumedThread) {
        let result = (|| -> io::Result<()> {
            self.verify_opened(id, &reply)?;
            self.sync_title(&reply.thread);
            if self.profile.route.is_api() {
                self.authorized_models
                    .insert(id.into(), reply.model.clone());
            }
            if self.review_yolo_resumes
                && reply.permissions.ceiling == Some(crate::modes::Level::Yolo)
                && self.transcripts.get(id).is_none_or(|t| !t.ready)
            {
                self.held_yolo_resumes.entry(id.into()).or_insert(false);
            }
            let mut initial = reply.initial_turns_page;
            self.transcripts.entry(id.into()).or_default().verified = true;
            if let Some(page) = &mut initial {
                self.history_preview(id, page);
            }
            let t = self.transcripts.entry(id.into()).or_default();
            // We hold the write lock now: whatever held it before is gone.
            t.native_live = false;
            t.native_next = None;
            t.model = Some(reply.model);
            t.tier = reply.service_tier;
            t.effort = reply.reasoning_effort;
            t.access = Some(reply.permissions.access);
            t.ceiling = reply.permissions.ceiling;
            t.verified = true;
            if let Some(page) = initial {
                t.add_history(page)?;
            } else {
                self.request_history(id, None)?;
            }
            Ok(())
        })();
        if let Err(err) = result {
            self.failed(Pending::Resume(id.into()), err.to_string());
        }
    }

    /// Codex allows one writer per thread. A second resume is refused with this text, which
    /// is how OX learns that a native Codex client has the thread open right now.
    pub(super) const WRITER_LOCK: &'static str = "already has an active writer";

    /// Show the thread read-only from its rollout and keep checking until the other client
    /// lets go; the next successful resume then makes OX the writer.
    pub(super) fn native_locked(&mut self, id: String) {
        let now = std::time::Instant::now();
        let t = self.transcripts.entry(id.clone()).or_default();
        let fresh = !t.native_live || !t.ready || t.loaded_updated != t.read_updated;
        let was = t.native_live;
        t.native_live = true;
        t.native_next = Some(now + std::time::Duration::from_secs(4));
        t.error = None;
        t.loading = false;
        t.verified = true;
        if fresh {
            // The journal moved: replace the shown history rather than splice into it.
            let seen = t.read_updated;
            let next = t.native_next;
            *t = Transcript::default();
            t.native_live = true;
            t.native_next = next;
            t.read_updated = seen;
            t.loaded_updated = seen;
            t.verified = true;
            if let Err(err) = self.request_history(&id, None) {
                self.transcripts.entry(id.clone()).or_default().error = Some(err.to_string());
            }
        }
        if !was {
            self.changed.mark(super::Changed::THREADS);
        }
        self.changed.transcript(&id);
    }

    /// Fixture for host tests: the selected thread is held by another Codex client, its
    /// probe is overdue, and the read it sent is still waiting for the runtime.
    /// A store over an inert runtime process, for view tests that must stay
    /// runtime-neutral (they cannot name a runtime's link module).
    #[doc(hidden)]
    pub fn inert(profile: super::Profile) -> std::io::Result<Self> {
        let link = crate::links::codex::Link::spawn(
            crate::links::codex::Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: profile.project.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )?;
        Self::open(profile, link)
    }
    #[doc(hidden)]
    pub fn hold_native_read_for_test(&mut self, id: &str) {
        let t = self.transcripts.entry(id.into()).or_default();
        t.native_live = true;
        t.native_next = Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
        self.selected = Some(id.into());
        self.pending.insert(
            -1,
            (
                std::time::Instant::now() + std::time::Duration::from_secs(90),
                Pending::Read(id.into()),
            ),
        );
    }

    /// A read, resume or history request for the thread is still waiting for Codex.
    pub(super) fn native_request_outstanding(&self, id: &str) -> bool {
        self.pending.values().any(|(_, p)| p.loads_history_for(id))
    }

    /// Probe the selected natively-held Codex thread every few seconds: a resume that is
    /// accepted means the other client closed it; one that is refused refreshes the view.
    pub(super) fn refresh_native_codex(&mut self, now: std::time::Instant) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let due = self
            .transcripts
            .get(&id)
            .is_some_and(|t| t.native_live && t.native_next.is_some_and(|at| now >= at));
        if !due || self.native_request_outstanding(&id) || self.state != Connection::Ready {
            return;
        }
        if let Some(t) = self.transcripts.get_mut(&id) {
            t.native_next = Some(now + std::time::Duration::from_secs(4));
        }
        let _ = self.request(Call::Read(id.clone()), Pending::Read(id));
    }

    pub(super) fn full_history_read(&mut self, id: String, read: ReadThread, mut page: TurnPage) {
        if let Err(error) = self.verify_identity(&id, &read.thread) {
            return self.failed(Pending::ReadFullHistory(id), error.to_string());
        }
        if let Some(t) = self.transcripts.get(&id)
            && let Err(error) = t.trim_full_history_to_cursor(&mut page)
        {
            return self.failed(Pending::ReadFullHistory(id), error.to_string());
        }
        self.history_page(&id, page);
    }

    fn history_preview(&mut self, id: &str, page: &TurnPage) {
        self.prompt_preview(
            id,
            super::catalog::PromptSource::History {
                oldest_page: page.next_cursor.is_none(),
            },
            || {
                page.data
                    .iter()
                    .rev()
                    .flat_map(|turn| &turn.items)
                    .find_map(|row| row.item.user_prompt_line())
            },
        );
    }

    pub(super) fn history_page(&mut self, id: &str, page: TurnPage) {
        self.history_preview(id, &page);
        let result = self
            .transcripts
            .entry(id.into())
            .or_default()
            .add_history(page);
        if let Err(err) = result {
            self.failed(Pending::History(id.into()), err.to_string());
        }
    }
}
