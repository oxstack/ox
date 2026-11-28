use super::{Connection, Notification, Route, Store};
use crate::changed::Changed;
use std::io;

impl Store {
    pub fn applied_access(&self, id: Option<&str>) -> Option<crate::runtime::Access> {
        if let Some(remote) = self.remote() {
            return id
                .and_then(|id| remote.access.get(id).copied())
                .or_else(|| id.is_none().then_some(remote.draft_access).flatten());
        }
        if self.is_antigravity() {
            self.antigravity_access(id)
        } else if self.is_session_scoped() {
            self.claude_applied_access(id)
        } else {
            self.transcript(id?).and_then(|t| t.access())
        }
    }
    /// The level this session grants a worker or handoff target. A Codex
    /// thread's comes from its effective settings and may exist for a
    /// configured mode; elsewhere only a confirmed preset grants one.
    pub fn applied_ceiling(&self, id: Option<&str>) -> Option<crate::modes::Level> {
        let runtime = self.profile.route.runtime;
        let preset = |access| crate::modes::Level::of(runtime, access);
        if self.remote().is_some() || self.is_session_scoped() || self.is_antigravity() {
            return self.applied_access(id).and_then(preset);
        }
        let t = self.transcript(id?)?;
        // Both come from one reply; a ceiling never outlives its access.
        t.access?;
        t.ceiling
    }
    pub fn use_host_policy(&mut self, policy: crate::host_policy::HostPolicy) {
        self.host_start = policy.starting_level();
        self.review_yolo_resumes = policy == crate::host_policy::HostPolicy::YoloAsksEveryTime;
        if !self.review_yolo_resumes && !self.held_yolo_resumes.is_empty() {
            for id in self.held_yolo_resumes.keys().cloned().collect::<Vec<_>>() {
                self.cancel_held_resume(&id);
            }
        }
    }
    pub fn new_resume_reviews(&mut self) -> Vec<String> {
        self.held_yolo_resumes
            .iter_mut()
            .filter(|(id, _)| {
                self.resume_sends.contains(id.as_str())
                    && self
                        .transcripts
                        .get(id.as_str())
                        .is_some_and(|t| t.ready && t.verified)
            })
            .filter_map(|(id, announced)| (!std::mem::replace(announced, true)).then(|| id.clone()))
            .collect()
    }
    pub fn cancel_held_resume(&mut self, id: &str) {
        self.held_yolo_resumes.remove(id);
        self.resume_sends.remove(id);
        self.resume_senders.remove(id);
        self.resume_commands.remove(id);
    }
    pub fn resume_held(&self, id: &str) -> bool {
        self.held_yolo_resumes.contains_key(id)
    }
    pub fn accept_resume_review(&mut self, id: &str) {
        self.held_yolo_resumes.remove(id);
    }
    pub fn decline_resume_review(&mut self, id: &str) -> io::Result<()> {
        self.cancel_held_resume(id);
        self.held_yolo_resumes.insert(id.into(), false);
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
    pub fn claude_resume_access(&self, id: &str) -> Option<crate::runtime::Access> {
        self.resume_access.get(id).copied()
    }
    pub fn choose_resume_access(&mut self, id: &str, mode: crate::runtime::Access) {
        self.resume_access.insert(id.into(), mode);
    }
    pub fn set_host_start(&mut self, level: Option<crate::modes::Level>) {
        self.host_start = level;
    }
    pub fn restore_worker_ceiling(&mut self, id: &str, level: crate::modes::Level) {
        self.worker_ceilings.insert(id.into(), level);
    }
    pub(super) fn host_start_access(&self, id: &str) -> Option<crate::runtime::Access> {
        let mut level = self.host_start?;
        if let Some(&cap) = self.worker_ceilings.get(id) {
            level = level.min(cap);
        }
        Some(level.preset(self.profile.route.runtime))
    }
    /// A fixed host start can be narrowed in the draft picker. Once open, the
    /// operator can use the session's regular mode control.
    pub(super) fn starting_access(
        &self,
        fixed: Option<crate::runtime::Access>,
        chosen: Option<crate::runtime::Access>,
    ) -> Option<crate::runtime::Access> {
        let Some(fixed) = fixed else { return chosen };
        let runtime = self.profile.route.runtime;
        let level = |access| crate::modes::Level::of(runtime, access);
        chosen
            .filter(|&mode| level(mode).is_some_and(|m| Some(m) <= level(fixed)))
            .or(Some(fixed))
    }
    /// Replace where new sessions read their start mode, for tests and embedders.
    pub fn set_start_mode(
        &mut self,
        source: fn(crate::runtime::Runtime) -> Option<crate::runtime::Access>,
    ) {
        self.start_mode = source;
    }
    /// The start mode for a new session nobody chose a mode for.
    pub(crate) fn start_mode(&self) -> Option<crate::runtime::Access> {
        (self.start_mode)(self.profile.route.runtime)
    }
    pub fn next_access(&self) -> Option<crate::codex::access::Access> {
        self.starting_access(
            self.host_start
                .map(|level| level.preset(self.profile.route.runtime)),
            self.next_preset,
        )
        .or_else(|| self.remote().is_none().then(|| self.start_mode()).flatten())
    }
    /// An incoming move uses this host's settings and its own optional mode
    /// selection. It must not consume a preset chosen for an unrelated draft.
    pub fn move_start_access(&self, id: Option<&str>) -> Option<crate::runtime::Access> {
        self.starting_access(
            self.host_start
                .map(|level| level.preset(self.profile.route.runtime)),
            id.and_then(|id| self.claude_resume_access(id)),
        )
        .or_else(|| self.start_mode())
    }
    pub fn set_next_access(&mut self, access: crate::codex::access::Access) {
        if !crate::runtime::Access::choices(self.profile.route.runtime).contains(&access)
            && access != crate::runtime::Access::Configured
        {
            return;
        }
        self.next_preset = (access != crate::codex::access::Access::Configured).then_some(access);
    }
    pub fn can_change_access(&self, id: &str) -> bool {
        if let Some(remote) = self.remote() {
            return remote.can(id, "access");
        }
        if self.is_antigravity() {
            return self.can_send(Some(id)) && !self.antigravity_access_choices(id).is_empty();
        }
        (self.can_send(Some(id))
            || self.can_configure_saved_claude(id)
            || (self.resume_held(id)
                && self.resume_sends.contains(id)
                && self.ready_to_send(Some(id))))
            && self.transcripts[id].active_turn().is_none()
            && !self.asks.iter().any(|a| a.thread.as_deref() == Some(id))
            && !self
                .pending
                .values()
                .any(|(_, p)| matches!(p,super::Pending::Access(t) if t==id))
    }
    pub fn set_access(&mut self, id: &str, preset: crate::codex::access::Access) -> io::Result<()> {
        if self.remote().is_some() {
            return self.forward(crate::actions::Action::SetAccess(id.into(), preset));
        }
        if !self.can_change_access(id)
            || !crate::runtime::Access::choices(self.profile.route.runtime).contains(&preset)
        {
            return Err(io::Error::other(
                "Permissions cannot be changed for this thread right now",
            ));
        }
        if self.is_antigravity() {
            return self.antigravity_set_access(id, preset);
        }
        if self.is_session_scoped() {
            if self.can_configure_saved_claude(id) {
                self.resume_access.insert(id.into(), preset);
                self.changed.transcript(id);
                return Ok(());
            }
            if self.applied_access(Some(id)) == Some(preset) {
                return Ok(());
            }
            return self.claude_mode(id, preset);
        }
        self.request(
            crate::codex::request::Call::Access {
                thread: id.into(),
                preset,
            },
            super::Pending::Access(id.into()),
        )
    }
    pub(super) fn notification(&mut self, notice: Notification) -> io::Result<()> {
        match notice {
            Notification::Started(row) => self.observed_row(*row),
            Notification::Status { thread, status } => self.activity_status(&thread, status),
            Notification::Goal {
                thread,
                turn,
                complete,
            } => {
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    t.update_goal(turn, complete);
                }
            }
            Notification::Settings { thread, settings } => {
                let pending_model=self.pending.values().any(|(_,p)|matches!(p,super::Pending::Options(owner,o) if owner==&thread && o.model==settings.model));
                let mismatch = self
                    .expected_alias(&thread)
                    .is_some_and(|model| model != settings.model)
                    && !pending_model;
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    t.model = Some(settings.model.clone());
                    t.tier = settings.service_tier;
                    t.effort = settings.effort;
                    if t.access != Some(settings.permissions.access) {
                        self.changed.mark(Changed::THREADS);
                    }
                    if settings
                        .permissions
                        .ceiling
                        .is_some_and(|level| !level.needs_person())
                    {
                        self.held_yolo_resumes.remove(&thread);
                    }
                    t.access = Some(settings.permissions.access);
                    t.ceiling = settings.permissions.ceiling;
                    if settings.provider != self.profile.provider || mismatch {
                        t.verified = false;
                        t.ready = false;
                        // Its TurnCompleted is dropped while unverified, so the running turn can never clear itself.
                        t.active_turn = None;
                        t.error=Some("Codex changed this thread’s billing route. Reopen it on its current route.".into());
                    }
                    self.changed.transcript(&thread);
                }
            }
            Notification::Tokens { thread, usage, .. } => {
                if let Some(window) = usage.model_context_window.filter(|w| *w > 0) {
                    let model = self.transcripts.get(&thread).and_then(|t| t.model.clone());
                    self.context_windows.push((model, window as u64));
                }
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    t.usage = Some(usage);
                    self.changed.transcript(&thread);
                }
            }
            Notification::RateLimits(limits) if self.profile.route == Route::CODEX_SUBSCRIPTION => {
                self.usage.update(limits);
                self.changed.mark(Changed::NOTICE);
            }
            Notification::Diff { thread, turn, diff } => {
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    t.reviews.turn(&turn, false).diff(diff);
                    self.changed.transcript(&thread);
                }
            }
            Notification::FilePatch {
                thread,
                turn,
                item,
                changes,
            } => {
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    t.reviews.turn(&turn, false).files(&item, &changes, false);
                    self.changed.transcript(&thread);
                }
            }
            Notification::Plan {
                thread,
                turn,
                explanation,
                steps,
            } => {
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    let plan = t.reviews.turn(&turn, false);
                    plan.explanation = explanation;
                    plan.steps = steps
                        .into_iter()
                        .map(|s| crate::transcript::review::Step {
                            text: s.step,
                            status: s.status,
                        })
                        .collect();
                    self.changed.transcript(&thread);
                }
            }
            Notification::Resolved(id) => {
                let before = self.asks.len();
                self.asks.retain(|a| a.id != id);
                if before != self.asks.len() {
                    self.changed.mark(Changed::ASK);
                }
            }
            Notification::Closed(id) => {
                if self.observe_codex_move_close(&id) {
                    return Ok(());
                }
                if let Some(row) = self.catalog.get_mut(&id) {
                    row.status = None;
                }
                for ask in self.asks.iter().filter(|a| a.thread.as_ref() == Some(&id)) {
                    self.link
                        .codex()?
                        .reject(ask.id.clone(), -32000, "Thread closed")?;
                }
                self.asks.retain(|a| a.thread.as_ref() != Some(&id));
                if let Some(t) = self.transcripts.get_mut(&id) {
                    t.verified = false;
                    t.ready = false;
                    t.active_turn = None;
                    t.error = Some("Codex closed this thread. Select it to reconnect.".into());
                }
                self.changed.mark(Changed::THREADS | Changed::ASK);
            }
            Notification::Renamed { thread, name } => {
                if let Some(row) = self.catalog.get_mut(&thread) {
                    row.name = name;
                    self.catalog_revision += 1;
                    if self.listing {
                        self.created_while_listing.insert(thread);
                    }
                    self.changed.mark(Changed::THREADS);
                }
            }
            Notification::Archived(id) => {
                self.cancel_held_resume(&id);
                self.archived_threads.insert(id.clone());
                self.catalog.remove(&id);
                self.catalog_revision += 1;
                if self.selected() == Some(&id) {
                    self.new_session();
                }
                self.changed.mark(Changed::THREADS);
            }
            Notification::Account { auth_mode }
                if self.profile.route == Route::CODEX_SUBSCRIPTION =>
            {
                let signed_in =
                    matches!(auth_mode.as_deref(), Some("chatgpt" | "chatgptAuthTokens"));
                let request = match self.state {
                    Connection::Ready if signed_in => Some(super::Pending::RefreshAccount),
                    Connection::Ready => Some(super::Pending::Account),
                    Connection::SignInRequired if signed_in => Some(super::Pending::Account),
                    _ => None,
                };
                if let Some(request) = request
                    && !self.pending.values().any(|(_, p)| {
                        matches!(p, super::Pending::Account | super::Pending::RefreshAccount)
                    })
                {
                    // A notification is a hint. Only account/read can authorize subscription usage.
                    self.request(crate::codex::request::Call::Account, request)?;
                }
            }
            Notification::Error {
                thread,
                message,
                will_retry: false,
                ..
            } => {
                if let Some(t) = self.transcripts.get_mut(&thread) {
                    t.error = Some(message);
                    self.changed.transcript(&thread);
                }
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{
        frame::{Frame, RequestId},
        reply::Reply,
    };
    use crate::links::codex::{Launch, Link, LinkEvent};
    use crate::store::{Pending, Profile};
    use serde_json::json;

    #[test]
    fn subscription_login_hint_waits_for_a_verified_account_and_coalesces_refreshes() {
        let project = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: project.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut store = Store::open(
            Profile {
                project,
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        store.state = Connection::SignInRequired;
        let notification = || Notification::Account {
            auth_mode: Some("chatgpt".into()),
        };
        let pending = |s: &Store| -> Vec<i64> {
            s.pending
                .iter()
                .filter_map(|(id, (_, p))| matches!(p, Pending::Account).then_some(*id))
                .collect()
        };
        for (kind, ready) in [("apiKey", false), ("chatgpt", true)] {
            store.notification(notification()).unwrap();
            store.notification(notification()).unwrap();
            let requests = pending(&store);
            assert_eq!(requests.len(), 1);
            assert_eq!(store.state(), &Connection::SignInRequired);
            let reply = serde_json::from_value(json!({"account":{"type":kind}})).unwrap();
            store.apply(LinkEvent::Frame(Frame::Reply {
                id: RequestId::Number(requests[0]),
                result: Ok(Reply::Account(reply)),
            }));
            assert_eq!(store.state() == &Connection::Ready, ready);
        }
        store.notification(notification()).unwrap();
        let id = store
            .pending
            .iter()
            .find_map(|(id, (_, p))| matches!(p, Pending::RefreshAccount).then_some(*id))
            .unwrap();
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(id),
            result: Ok(Reply::Account(
                serde_json::from_value(
                    json!({"account":{"type":"chatgpt","email":"second@example.invalid"}}),
                )
                .unwrap(),
            )),
        }));
        assert_eq!(store.state(), &Connection::Ready);
        store.profile.route = Route::codex_api("vendor/model".into());
        store.state = Connection::SignInRequired;
        store.notification(notification()).unwrap();
        assert!(pending(&store).is_empty());
    }

    /// The host reads a source's worker ceiling from the store. Configured
    /// stock settings verify one; a user-defined profile never does.
    #[test]
    fn a_codex_thread_ceiling_follows_its_effective_settings() {
        use crate::codex::{access::Permissions, notice::EffectiveSettings};
        use crate::modes::Level;
        use crate::runtime::Access;
        let project = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: project.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut store = Store::open(
            Profile {
                project,
                route: Route::CODEX_SUBSCRIPTION,
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        store.state = Connection::Ready;
        store.edit(None, "Start".into());
        store.submit(None).unwrap();
        // thread/start of `sandbox_mode = "danger-full-access"` with the desktop's on-request.
        let opened = serde_json::from_value(json!({
            "thread":{"id":"thread","modelProvider":"openai","cwd":std::env::temp_dir()},
            "modelProvider":"openai","model":"stock-model",
            "sandbox":{"type":"dangerFullAccess"},"approvalPolicy":"on-request",
            "approvalsReviewer":"user","activePermissionProfile":null
        }))
        .unwrap();
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(store.seq),
            result: Ok(Reply::Opened(opened)),
        }));
        let facts = |s: &Store| {
            (
                s.applied_access(Some("thread")),
                s.applied_ceiling(Some("thread")),
            )
        };
        assert_eq!(
            facts(&store),
            (Some(Access::Configured), Some(Level::Classifier))
        );
        let settings = |value: serde_json::Value| Notification::Settings {
            thread: "thread".into(),
            settings: EffectiveSettings {
                permissions: Permissions::effective(&value),
                model: "stock-model".into(),
                provider: "openai".into(),
                effort: None,
                service_tier: None,
            },
        };
        // thread/settings/updated after `permissions = "ci"`, a profile that denies /home.
        store
            .notification(settings(json!({
                "sandboxPolicy":{"type":"readOnly","networkAccess":false},
                "approvalPolicy":"on-request","approvalsReviewer":"user",
                "activePermissionProfile":{"id":"ci","extends":null}
            })))
            .unwrap();
        assert_eq!(facts(&store), (Some(Access::Configured), None));
        // An explicit preset again.
        store
            .notification(settings(json!({
                "sandboxPolicy":{"type":"workspaceWrite","writableRoots":[],"networkAccess":false,
                                 "excludeTmpdirEnvVar":false,"excludeSlashTmp":false},
                "approvalPolicy":"on-request","approvalsReviewer":"auto_review",
                "activePermissionProfile":null
            })))
            .unwrap();
        assert_eq!(
            facts(&store),
            (Some(Access::AutoReview), Some(Level::Classifier))
        );
        assert_eq!(store.applied_ceiling(Some("other")), None);
    }
}
