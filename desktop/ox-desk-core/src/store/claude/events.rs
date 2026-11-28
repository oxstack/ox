//! Apply events only to the process generation and billing route that delivered them.
use super::*;
use crate::{slash::Capabilities, store::routes_thread::ModelInfo, transcript::DeltaTarget};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

#[derive(Default)]
pub(super) struct EventState {
    // Background tasks can finish after their spawning turn or during the next turn.
    pub(super) pending: HashMap<String, HashMap<String, String>>,
    alive: Arc<()>,
    awaiting_echo: Vec<(String, String)>,
    needs_sign_in: bool,
}
impl EventState {
    pub(super) fn lifetime(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.alive)
    }
    pub(super) fn take_awaiting_echo(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.awaiting_echo)
    }
    // A successful queue write alone cannot attest that the CLI read the prompt.
    pub(super) fn sent(&mut self, id: &str, text: &str) {
        self.awaiting_echo.push((id.into(), text.into()));
    }
}
impl Store {
    pub(in crate::store) fn claude_event(&mut self, update: Update) -> io::Result<()> {
        let Update {
            owner,
            generation,
            data,
        } = update;
        if !self
            .claude()
            .sessions
            .get(&owner)
            .is_some_and(|p| p.generation == generation)
        {
            return Ok(());
        }
        if matches!(&data, Data::Stock(pipe::Event::Closed { .. }))
            && self.finish_claude_handoff(&owner)
        {
            return Ok(());
        }
        if matches!(&data, Data::Stock(pipe::Event::Closed { .. }))
            && let Some(reaped) = self
                .claude_mut()
                .sessions
                .get_mut(&owner)
                .and_then(|process| process.move_release.take())
        {
            let result = if reaped.confirmed() {
                self.claude_end(&owner).map_err(|error| error.to_string())
            } else {
                let reason = "Could not confirm that Claude Code's process was reaped.".to_owned();
                self.claude_closed(&owner, reason.clone());
                Err(reason)
            };
            self.complete_move_writer_release(&owner, result);
            return Ok(());
        }
        let outcome = match data {
            Data::Failed(reason) | Data::Stock(pipe::Event::Closed { reason }) => {
                self.claude_closed(&owner, reason);
                Ok(())
            }
            Data::Stock(pipe::Event::SignInRequired) => {
                if self.claude().sessions[&owner].move_release.is_some() {
                    return Ok(()); // Wait for the process-reap result, which follows the sign-in event.
                }
                if self.profile.route.billing == Billing::Subscription {
                    self.claude_mut()
                        .sessions
                        .get_mut(&owner)
                        .unwrap()
                        .events
                        .needs_sign_in = true;
                    // Stock has already refused this account. Restore pending input
                    // before a client reconnect can replace the route.
                    self.claude_closed(
                        &owner,
                        "Sign in to Claude Code to use the subscription.".into(),
                    );
                }
                Ok(())
            }
            Data::Connected(link) => {
                if link.session_id() != stock_id(&owner)? {
                    Err(io::Error::other("Claude connected to another session"))
                } else {
                    self.claude_mut().sessions.get_mut(&owner).unwrap().link = Some(link);
                    self.claude_ready(&owner)
                }
            }
            Data::History(batch) => {
                batch.apply(self, &owner);
                Ok(())
            }
            Data::Tail(tail) => {
                if self.is_viewer(&owner) {
                    tail.apply(self);
                }
                Ok(())
            }
            Data::Viewed(follower) => {
                self.viewed(&owner, follower);
                Ok(())
            }
            Data::Stock(pipe::Event::Frame(event)) => self.claude_frame(&owner, event),
        };
        if let Err(err) = &outcome {
            self.claude_closed(&owner, err.to_string());
        }
        outcome
    }

    fn claude_ready(&mut self, owner: &str) -> io::Result<()> {
        let p = &self.claude().sessions[owner];
        if !p.initialized || p.link.is_none() {
            return Ok(());
        }
        let mode = p.mode;
        let model = p.capabilities.as_ref().map(|c| c.model.clone());
        let t = self.transcripts.entry(owner.into()).or_default();
        t.access = mode;
        t.error = None;
        self.resume_access.remove(owner);
        self.resume_models.remove(owner);
        self.unsaved_claude_drafts.remove(owner);
        t.model = model;
        self.state = Connection::Ready;
        self.changed.mark(Changed::CONNECTION);
        self.flush_claude(owner)?;
        self.changed.transcript(owner);
        self.reopen_claude_history();
        Ok(())
    }

    pub(in crate::store) fn reopen_claude_history(&mut self) {
        let reopen = std::mem::take(&mut self.reopen);
        for key in reopen {
            let parent = key.split('/').next().unwrap_or(&key);
            if self.claude().catalog.contains_key(parent)
                && !self.claude().sessions.contains_key(parent)
                && let Err(err) = self.claude_open(parent)
            {
                self.transcripts.entry(parent.into()).or_default().error = Some(err.to_string());
                self.changed.transcript(parent);
            }
        }
    }

    fn claude_closed(&mut self, owner: &str, reason: String) {
        let Some(mut p) = self.claude_mut().sessions.remove(owner) else {
            return;
        };
        if p.move_release.is_some() {
            self.complete_move_writer_release(owner, Err(reason.clone()));
        }
        if p.deferred_command.as_ref().is_some_and(|c| c.new_thread)
            && let Some((_, command)) = &p.first
        {
            self.publish_command_thread(owner, command);
        }
        self.restore_claude_input(owner, &mut p);
        let removed: HashSet<_> = self
            .claude()
            .asks
            .iter()
            .filter(|(_, v)| v.0 == owner)
            .map(|(k, _)| k.clone())
            .collect();
        self.asks.retain(|a| !removed.contains(&a.id));
        self.claude_mut().asks.retain(|k, _| !removed.contains(k));
        for (id, t) in &mut self.transcripts {
            if id == owner || id.strip_prefix(owner).is_some_and(|s| s.starts_with('/')) {
                t.verified = false;
                t.ready = false;
                t.loading = false;
                t.active_turn = None;
                t.error = Some(reason.clone());
                self.changed.transcript(id);
            }
        }
        if p.viewer.is_none()
            && !self
                .claude()
                .sessions
                .values()
                .any(|s| s.initialized && s.link.is_some())
        {
            self.state = if p.events.needs_sign_in || self.state == Connection::SignInRequired {
                Connection::SignInRequired
            } else {
                Connection::Closed(reason.clone())
            };
        }
        self.notice = Some(reason);
        self.changed
            .mark(Changed::CONNECTION | Changed::NOTICE | Changed::ASK | Changed::THREADS);
    }

    pub(super) fn restore_claude_input(&mut self, owner: &str, p: &mut Process) {
        let mut unsent = p.events.take_awaiting_echo();
        unsent.extend(p.first.take());
        if let Some((uuid, _)) = p.worker_notice.take() {
            // The host outbox owns uncertain notices. Only human messages belong in the composer.
            unsent.retain(|(id, _)| id != &uuid);
        }
        if !unsent.is_empty() {
            self.ended_threads.insert(owner.into());
            if p.started_new {
                self.unsaved_claude_drafts.insert(owner.into());
            }
            if let Some(mode) = p
                .mode
                .or_else(|| p.launch_mode.as_deref().and_then(Access::from_mode))
            {
                self.resume_access.insert(owner.into(), mode);
            }
            if let Some(model) = p.selected_model.take() {
                self.resume_models.insert(owner.into(), model);
            }
        }
        self.restore_claude_drafts(owner, unsent);
    }

    pub(super) fn restore_claude_drafts(&mut self, owner: &str, messages: Vec<(String, String)>) {
        if let Some((id, _)) = messages.last() {
            let id = id.clone();
            let mut unsent: Vec<_> = messages.into_iter().map(|(_, text)| text).collect();
            let draft = self.draft(Some(owner));
            if !draft.is_empty() && unsent.last().is_none_or(|s| s != draft) {
                unsent.push(draft.to_owned());
            }
            let text = unsent.join("\n");
            self.restored_drafts
                .insert(owner.to_owned(), (id, text.clone()));
            self.edit(Some(owner), text);
            self.changed.mark(Changed::THREADS);
        }
    }

    fn claude_frame(&mut self, owner: &str, event: wire::Event) -> io::Result<()> {
        use wire::Event;
        if let Event::SubscriptionLimit { info, session_id } = event {
            if let Some(id) = self.claude().sessions[owner].limit_reporter
                && session_id == stock_id(owner)?
            {
                use crate::usage::reported::{Observation, WorkerReport};
                self.usage.worker_report =
                    Some(WorkerReport::windows(id, Observation::claude(info)));
            }
            return Ok(());
        }
        if !self.claude().sessions[owner].initialized
            && !matches!(&event,Event::ControlReply(reply) if reply.request_id=="ox-init")
        {
            return Err(io::Error::other(
                "Claude produced session activity before account verification",
            ));
        }
        let session = match &event {
            Event::Init(i) => Some(&i.session_id),
            Event::Title { session_id, .. } | Event::Model { session_id, .. } => Some(session_id),
            Event::Result(r) => Some(&r.session_id),
            Event::Item { scope, .. } | Event::Delta { scope, .. } => Some(&scope.session_id),
            _ => None,
        };
        if let Some(session) = session
            && session != stock_id(owner)?
        {
            return Err(io::Error::other("Claude reported an unexpected session ID"));
        }
        match event {
            Event::ControlReply(r) => self.claude_control(owner, r)?,
            Event::SubscriptionLimit { .. } => unreachable!("handled above"),
            Event::Model { model, .. } => {
                let p = self.claude_mut().sessions.get_mut(owner).unwrap();
                // Each distinct model in the order it first answered, so a fallback
                // to another model mid-run stays visible next to the first.
                if !p.answering.contains(&model) && p.answering.len() < 8 {
                    p.answering.push(model);
                }
            }
            Event::Title { name, .. } => {
                if let Some(row) = self.claude_mut().catalog.get_mut(owner) {
                    row.name = Some(name.clone());
                }
                if let Some(row) = self.catalog.get_mut(owner) {
                    row.name = Some(name);
                }
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            Event::Init(init) => {
                let cwd = Path::new(&init.cwd);
                let first = !std::mem::replace(
                    &mut self
                        .claude_mut()
                        .sessions
                        .get_mut(owner)
                        .unwrap()
                        .project_checked,
                    true,
                );
                if first
                    && cwd != self.profile.project
                    && cwd
                        .canonicalize()
                        .ok()
                        .zip(self.profile.project.canonicalize().ok())
                        .is_none_or(|(reported, expected)| reported != expected)
                {
                    return Err(io::Error::other("Claude opened a different project"));
                }
                if let Billing::Api(alias) = &self.profile.route.billing
                    && self.claude().sessions[owner]
                        .selected_model
                        .as_ref()
                        .unwrap_or(alias)
                        != &init.model
                {
                    return Err(io::Error::other(
                        "Claude opened a different API model alias",
                    ));
                }
                let mode = Access::from_mode(&init.permission_mode);
                let p = self.claude_mut().sessions.get_mut(owner).unwrap();
                p.mode = mode;
                if let Some(caps) = &mut p.capabilities {
                    // The stream reports the concrete model the symbolic default resolves to.
                    if p.selected_model.is_none()
                        && caps.model == "default"
                        && init.model != "default"
                    {
                        if let Some(info) = Arc::make_mut(&mut caps.models).get_mut("default") {
                            info.resolved = Some(init.model.clone());
                        }
                        let name = caps.model_display("default");
                        if let Some(info) = Arc::make_mut(&mut caps.models).get_mut("default") {
                            info.label = name;
                        }
                    }
                    caps.model = p
                        .selected_model
                        .clone()
                        .filter(|m| {
                            m == &init.model
                                || caps.models.get(m).and_then(|info| info.resolved.as_deref())
                                    == Some(&init.model)
                        })
                        .unwrap_or_else(|| init.model.clone());
                    let offered: HashSet<_> = init
                        .slash_commands
                        .iter()
                        .map(|s| s.trim_start_matches('/'))
                        .collect();
                    if let Some(commands) = &mut caps.session_commands {
                        Arc::make_mut(commands)
                            .retain(|name, _| offered.contains(name.trim_start_matches('/')));
                    }
                }
                let displayed_model = p
                    .capabilities
                    .as_ref()
                    .map_or(&init.model, |c| &c.model)
                    .clone();
                let t = self.transcripts.entry(owner.into()).or_default();
                t.model = Some(displayed_model);
                t.access = mode;
                let route_model = match &self.profile.route.billing {
                    Billing::Api(alias) => alias.clone(),
                    Billing::Subscription => init.model,
                };
                if let Some(row) = self.claude_mut().catalog.get_mut(owner) {
                    row.model = Some(route_model.clone());
                }
                if let Some(row) = self.catalog.get_mut(owner) {
                    row.model = Some(route_model);
                }
                self.changed.transcript(owner);
                self.changed.mark(Changed::THREADS);
            }
            Event::Ask(ask) => {
                let display = ask
                    .parent_tool_use_id
                    .as_deref()
                    .map(|id| child_key(owner, id))
                    .unwrap_or_else(|| owner.into());
                if display != owner && !self.claude().catalog.contains_key(&display) {
                    self.claude_child(owner, owner, &display, "Claude agent");
                }
                let key = self.claude_ask_id(owner, &ask.request_id);
                if !self.claude().asks.contains_key(&key) {
                    self.asks.push(ask.as_ask(key.clone(), display));
                    self.claude_mut().asks.insert(key, (owner.into(), ask));
                    self.changed.mark(Changed::ASK);
                }
            }
            Event::CancelAsk(id) => {
                let key = self.claude_ask_id(owner, &id);
                self.claude_mut().asks.remove(&key);
                self.asks.retain(|a| a.id != key);
                self.changed.mark(Changed::ASK);
            }
            Event::UnsupportedRequest { request_id, .. } => {
                let link = self.claude().sessions[owner].link.as_ref().ok_or_else(|| {
                    io::Error::other("Claude sent an unsupported request before connecting")
                })?;
                link.reject_request(&request_id, "This request is unsupported by OX Desktop")?;
            }
            Event::Item {
                scope,
                id,
                mut item,
                completed,
                output,
            } => {
                let display = self.claude_display(owner, scope);
                let is_user = matches!(item, Item::UserMessage { .. });
                if display == owner && is_user {
                    let process = self.claude_mut().sessions.get_mut(owner).unwrap();
                    process.events.awaiting_echo.retain(|(uuid, _)| uuid != &id);
                    let notice = process.worker_notice.take_if(|(uuid, _)| uuid == &id);
                    let attachment = process.memory_attachment.take_if(|(uuid, _)| uuid == &id);
                    if let Some((_, notice)) = notice {
                        self.worker_notice_receipts
                            .push(crate::store::WorkerNoticeReceipt::Accepted(notice));
                    }
                    if let Some((_, attachment)) = attachment {
                        self.remember_memory_attachment(owner, attachment);
                    }
                }
                rebase_agents(owner, &display, &mut item, |child, tool, status| {
                    self.claude_child(owner, &display, child, tool);
                    if completed {
                        let t = self.transcripts.entry(child.into()).or_default();
                        let turn = t.active_turn.take();
                        t.record_result(
                            turn.as_deref().unwrap_or(&id),
                            status == "failed",
                            matches!(status, "stopped" | "interrupted"),
                        );
                        if turn.is_some() {
                            t.last_finished = turn;
                        }
                        if status == "failed" {
                            t.error = output.clone();
                        }
                        self.changed.transcript(child);
                        self.claude_status(
                            child,
                            if status == "failed" {
                                "systemError"
                            } else {
                                "idle"
                            },
                        );
                    }
                });
                let turn = self.claude_turn(owner, &display, &id, completed);
                self.transcripts
                    .entry(display.clone())
                    .or_default()
                    .observe_item(turn, id, item, completed, output);
                self.claude_activity(&display);
            }
            Event::Delta {
                scope,
                id,
                target,
                text,
            } => {
                let display = self.claude_display(owner, scope);
                let turn = self.claude_turn(owner, &display, &id, false);
                let target = match target {
                    wire::DeltaTarget::Text => DeltaTarget::Agent,
                    wire::DeltaTarget::Thinking => DeltaTarget::Summary(0),
                };
                self.transcripts
                    .entry(display.clone())
                    .or_default()
                    .observe_delta(turn, id, target, text);
                self.claude_activity(&display);
            }
            Event::Result(r) => {
                for (model, usage) in &r.model_usage {
                    if let Some(window) = usage.context_window.filter(|w| *w > 0) {
                        self.context_windows.push((Some(model.clone()), window));
                    }
                }
                if !r.is_error {
                    self.title_after_turn(owner);
                }
                let t = self.transcripts.entry(owner.into()).or_default();
                let finished = t.active_turn.take();
                t.interrupted = finished.is_some() && finished == t.stop_requested.take();
                if let Some(turn) = finished {
                    t.record_result(&turn, r.is_error, t.interrupted);
                    t.last_finished = Some(turn);
                }
                let interrupted = t.interrupted;
                if r.is_error && !interrupted {
                    t.error = Some(if r.errors.is_empty() {
                        format!("Claude turn ended ({})", r.subtype)
                    } else {
                        r.errors.join("\n")
                    });
                }
                self.claude_status(
                    owner,
                    if r.is_error && !interrupted {
                        "systemError"
                    } else {
                        "idle"
                    },
                );
                self.claude_activity(owner);
                self.changed.mark(Changed::THREADS);
            }
        }
        Ok(())
    }
    fn claude_ask_id(&self, owner: &str, id: &str) -> RequestId {
        RequestId::Text(format!(
            "{owner}:{}:{id}",
            self.claude().sessions[owner].generation
        ))
    }
    fn claude_display(&mut self, owner: &str, scope: wire::Scope) -> String {
        let Some(tool) = scope.parent_tool_use_id else {
            return owner.into();
        };
        let display = child_key(owner, &tool);
        if !self.claude().catalog.contains_key(&display) {
            self.claude_child(owner, owner, &display, "Claude agent");
        }
        display
    }
    fn claude_child(&mut self, owner: &str, parent: &str, id: &str, label: &str) {
        if self.claude().catalog.get(id).is_none_or(|r| {
            r.parent_thread_id.as_deref() != Some(parent)
                || r.agent_nickname.as_deref() != Some(label)
        }) {
            let mut child = row(
                &self.profile,
                id.into(),
                label.into(),
                self.claude().catalog.get(owner).map_or(0, |r| r.updated_at),
            );
            child.parent_thread_id = Some(parent.into());
            child.agent_nickname = Some(label.into());
            child.worker = true;
            self.remember_claude(child);
        }
        let authorized = self.claude().sessions[owner].initialized;
        let t = self.transcripts.entry(id.into()).or_default();
        t.ready = authorized;
        t.verified = authorized;
        t.loading = false;
        self.changed.transcript(id);
    }
    fn claude_turn(&mut self, owner: &str, display: &str, id: &str, completed: bool) -> String {
        let events = &mut self.claude_mut().sessions.get_mut(owner).unwrap().events;
        if let Some(pending) = events.pending.get_mut(display)
            && let Some(turn) = if completed {
                pending.remove(id)
            } else {
                pending.get(id).cloned()
            }
        {
            return turn;
        }
        let parent = self
            .transcripts
            .get(owner)
            .and_then(|t| t.active_turn.clone().or_else(|| t.last_finished.clone()));
        let t = self.transcripts.entry(display.into()).or_default();
        let turn = t
            .active_turn
            .get_or_insert_with(|| {
                if display == owner {
                    id.to_owned()
                } else {
                    parent.unwrap_or_else(|| id.to_owned())
                }
            })
            .clone();
        if !completed {
            self.claude_mut()
                .sessions
                .get_mut(owner)
                .unwrap()
                .events
                .pending
                .entry(display.into())
                .or_default()
                .insert(id.into(), turn.clone());
        }
        turn
    }
    fn claude_status(&mut self, id: &str, kind: &str) {
        let Some(r) = self.catalog.get_mut(id) else {
            return;
        };
        if r.status.as_ref().is_some_and(|s| s.kind == kind) {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        r.updated_at = now.as_secs() as i64;
        r.activity_order = now.as_nanos();
        let status = r.status.get_or_insert_with(Default::default);
        status.kind = kind.into();
        let row = r.clone();
        self.claude_mut().catalog.insert(id.into(), row);
        self.catalog_revision += 1;
        self.changed.mark(Changed::THREADS);
    }
    fn claude_activity(&mut self, display: &str) {
        if self
            .transcripts
            .get(display)
            .is_some_and(|t| t.active_turn.is_some())
        {
            self.claude_status(display, "active");
        }
        self.changed.transcript(display);
        if self.front && self.selected.as_deref() == Some(display) {
            return;
        }
        if let Some(t) = self.transcripts.get_mut(display) {
            t.unread = true;
        }
        if let Some(row) = self.catalog.get_mut(display)
            && !row.unread
        {
            row.unread = true;
            self.changed.mark(Changed::THREADS);
        }
        if let Some(row) = self.claude_mut().catalog.get_mut(display) {
            row.unread = true;
        }
    }

    fn claude_control(&mut self, owner: &str, r: wire::ControlReply) -> io::Result<()> {
        if r.request_id == "ox-init" {
            let native = r
                .capabilities()
                .ok_or_else(|| io::Error::other("Invalid Claude initialization reply"))?;
            let mut caps = Capabilities {
                provider: self.profile.provider.clone(),
                ..Default::default()
            };
            let mut commands = BTreeMap::new();
            // Upstream resolves a marked builtin before a same-name project command.
            let builtins: HashSet<_> = native
                .commands
                .iter()
                .filter(|cmd| cmd.builtin)
                .map(|cmd| cmd.name.clone())
                .collect();
            for cmd in native.commands {
                if !cmd.builtin && builtins.contains(&cmd.name) {
                    continue;
                }
                let name = cmd.name.trim_start_matches('/');
                if crate::slash::possible_claude_command(name) {
                    commands.insert(format!("/{name}"), cmd.description);
                }
            }
            caps.session_commands = Some(Arc::new(commands));
            let models = Arc::make_mut(&mut caps.models);
            match &self.profile.route.billing {
                Billing::Api(alias) => {
                    caps.model = self.claude().sessions[owner]
                        .selected_model
                        .clone()
                        .unwrap_or_else(|| alias.clone());
                    for alias in &self.claude().launcher.aliases {
                        models.insert(
                            alias.clone(),
                            ModelInfo {
                                label: alias.clone(),
                                ..Default::default()
                            },
                        );
                    }
                }
                Billing::Subscription => {
                    for (rank, model) in native.models.into_iter().enumerate() {
                        let is_default = model.value == "default";
                        if is_default {
                            caps.model = model.value.clone();
                        }
                        models.insert(
                            model.value,
                            ModelInfo {
                                label: model.display_name,
                                rank: Some(rank as u32),
                                is_default,
                                resolved: model.resolved_model,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
            // The symbolic default is listed under the model it resolves to.
            if caps
                .models
                .get("default")
                .is_some_and(|m| m.resolved.is_some())
            {
                let name = caps.model_display("default");
                if let Some(info) = Arc::make_mut(&mut caps.models).get_mut("default") {
                    info.label = name;
                }
            }
            let mode = native
                .current_permission_mode
                .as_deref()
                .and_then(Access::from_mode);
            self.commands.schema = Some(caps.clone());
            let p = self.claude_mut().sessions.get_mut(owner).unwrap();
            p.capabilities = Some(caps);
            p.mode = mode;
            p.initialized = true;
            if !self.profile.route.is_api() {
                use crate::usage::reported::{Reporter, WorkerReport};
                let id = Reporter::verified(
                    native.account.email.as_deref(),
                    r.payload
                        .as_ref()
                        .ok()
                        .and_then(|v| v["account"]["organization"].as_str()),
                    None,
                );
                self.claude_mut()
                    .sessions
                    .get_mut(owner)
                    .unwrap()
                    .limit_reporter = Some(id);
                self.usage.worker_report = Some(WorkerReport::select(id));
            }
            return self.claude_ready(owner);
        }
        let Some(control) = self
            .claude_mut()
            .sessions
            .get_mut(owner)
            .unwrap()
            .controls
            .remove(&r.request_id)
        else {
            return Ok(());
        };
        let result = match r.payload {
            Ok(r) => r,
            Err(reason) => {
                // A refused interrupt leaves the turn running: it must not later read as
                // Stopped, which would also hide the error a failing turn reports.
                if matches!(control, Control::Interrupt)
                    && let Some(t) = self.transcripts.get_mut(owner)
                {
                    t.stop_requested = None;
                }
                self.notice = Some(reason);
                self.changed.mark(Changed::NOTICE);
                return Ok(());
            }
        };
        match control {
            Control::Mode(mode) if result["mode"].as_str() == mode.mode() => {
                self.claude_mut().sessions.get_mut(owner).unwrap().mode = Some(mode);
                self.transcripts.entry(owner.into()).or_default().access = Some(mode);
            }
            Control::Model(model)
                if result["model"] == model
                    || self.claude().sessions[owner]
                        .capabilities
                        .as_ref()
                        .and_then(|caps| caps.models.get(&model))
                        .and_then(|info| info.resolved.as_deref())
                        .is_some_and(|resolved| result["model"] == resolved) =>
            {
                self.record_claude_model(owner, &model)?;
                let p = self.claude_mut().sessions.get_mut(owner).unwrap();
                p.selected_model = Some(model.clone());
                if let Some(c) = &mut p.capabilities {
                    c.model = model.clone();
                }
                self.transcripts.entry(owner.into()).or_default().model = Some(model.clone());
                let route_model = match &self.profile.route.billing {
                    Billing::Api(alias) => alias.clone(),
                    Billing::Subscription => model,
                };
                if let Some(row) = self.catalog.get_mut(owner) {
                    row.model = Some(route_model.clone());
                }
                if let Some(row) = self.claude_mut().catalog.get_mut(owner) {
                    row.model = Some(route_model);
                }
            }
            // The interrupt acknowledgment does not complete the turn. Claude's result does.
            Control::Interrupt => {
                // The receipt names queued prompts that survived Stop, and (on supporting
                // CLIs) uuids that were expressly cancelled. Keep editable text for the latter.
                let cancelled: HashSet<_> = result["cancelled"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if !cancelled.is_empty() {
                    let process = self.claude_mut().sessions.get_mut(owner).unwrap();
                    let notice = process
                        .worker_notice
                        .take_if(|(uuid, _)| cancelled.contains(uuid.as_str()));
                    if process
                        .memory_attachment
                        .take_if(|(uuid, _)| cancelled.contains(uuid.as_str()))
                        .is_some()
                    {
                        process.memory_brief_pending = true;
                    }
                    let pending = &mut process.events.awaiting_echo;
                    let (mut cancelled, survivors): (Vec<_>, _) = std::mem::take(pending)
                        .into_iter()
                        .partition(|(id, _)| cancelled.contains(id.as_str()));
                    *pending = survivors;
                    if let Some((uuid, notice)) = notice {
                        self.worker_notice_receipts
                            .push(crate::store::WorkerNoticeReceipt::Rejected(notice));
                        cancelled.retain(|(id, _)| id != &uuid);
                    }
                    self.restore_claude_drafts(owner, cancelled);
                }
                if let Some(count) = result["still_queued"]
                    .as_array()
                    .map(Vec::len)
                    .filter(|n| *n > 0)
                {
                    self.transcripts
                        .entry(owner.into())
                        .or_default()
                        .command_notice =
                        Some(format!("Claude kept {count} queued message(s) after Stop."));
                }
            }
            _ => {
                self.notice = Some("Claude did not confirm the requested setting".into());
                self.changed.mark(Changed::NOTICE);
            }
        }
        self.changed.transcript(owner);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }
}
pub(super) fn rebase_agents(
    owner: &str,
    display: &str,
    item: &mut Item,
    mut child: impl FnMut(&str, &str, &str),
) {
    if let Item::AgentOperation {
        tool,
        status,
        sender_thread_id,
        receiver_thread_ids,
    } = item
    {
        *sender_thread_id = display.to_owned();
        for raw in receiver_thread_ids {
            let key = child_key(owner, raw);
            child(&key, tool, status);
            *raw = key;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    fn fixture() -> (Store, String, String) {
        let profile = Profile {
            project: "/fixture".into(),
            route: crate::runtime::Route {
                runtime: Runtime::Claude,
                billing: Billing::Subscription,
            },
            provider: SUBSCRIPTION_PROVIDER.into(),
        };
        let a = session_key(&profile.route.billing, A);
        let b = session_key(&profile.route.billing, B);
        let launcher = Launcher {
            ox: PathBuf::new(),
            path: Default::default(),
            profiles: Profiles {
                subscription: PathBuf::new(),
                api: PathBuf::new(),
            },
            index: PathBuf::new(),
            aliases: vec![],
        };
        let project = Project {
            launcher: Arc::new(launcher),
            deliver: Arc::new(|_| panic!("test must not launch a process")),
            sessions: Default::default(),
            catalog: Default::default(),
            asks: Default::default(),
            dispatching_worker: None,
            worker: None,
            worker_blank: None,
            worker_model: None,
            serial: 2,
            native: Default::default(),
        };
        let mut store = Store::with_link(profile, links::Link::Claude(Box::new(project)));
        for k in [&a, &b] {
            store.claude_mut().sessions.insert(
                k.clone(),
                Process {
                    generation: 2,
                    initialized: true,
                    project_checked: false,
                    ..Default::default()
                },
            );
            store.remember_claude(row(&store.profile, k.clone(), "Session".into(), 1));
        }
        (store, a, b)
    }
    fn update(store: &mut Store, owner: &str, generation: u64, data: Data) -> io::Result<()> {
        store.claude_event(Update {
            owner: owner.into(),
            generation,
            data,
        })
    }
    fn decode(value: Value) -> wire::Event {
        wire::Decoder::default()
            .decode_value(value)
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn move_release_reaps_only_the_selected_claude_process() {
        use crate::store::MoveWriterRelease;
        use std::{sync::mpsc, time::Duration};
        let (mut s, a, b) = fixture();
        let root = tempfile::tempdir().unwrap();
        s.profile.project = root.path().to_owned();
        std::fs::write(
            root.path().join("initialize.response"),
            format!(
                "{}\n",
                json!({
                    "type":"control_response","response":{
                        "subtype":"success","request_id":"ox-init","response":{
                            "claude_code_version":pipe::minimum(),
                            "account":{"apiProvider":"firstParty","subscriptionType":"Claude Max"},
                            "commands":[],"models":[]
                        }
                    }
                })
            ),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        for owner in [&a, &b] {
            let sender = tx.clone();
            let id = owner.to_owned();
            let link = pipe::Link::spawn(
                pipe::Launch {
                    ox: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-link.sh"),
                    path: "/usr/bin:/bin".into(),
                    project: root.path().into(),
                    billing: Billing::Subscription,
                    session: pipe::Session::New(stock_id(owner).unwrap().to_owned()),
                    permission_mode: None,
                    contract: None,
                    model: None,
                },
                move |event| {
                    let _ = sender.send((id.clone(), event));
                },
            )
            .unwrap();
            update(&mut s, owner, 2, Data::Connected(link)).unwrap();
        }
        for _ in 0..2 {
            let (owner, event) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                event,
                pipe::Event::Frame(wire::Event::ControlReply(_))
            ));
            update(&mut s, &owner, 2, Data::Stock(event)).unwrap();
        }
        let other = s.claude().sessions[&b].link.as_ref().unwrap().reap_status();
        s.transcripts.get_mut(&b).unwrap().active_turn = Some("independent-turn".into());
        let child = format!("{a}/subagent");
        s.transcripts.entry(child.clone()).or_default().active_turn = Some("child-turn".into());
        assert!(s.release_writer_for_move(&a).is_err());
        assert!(s.can_send(Some(&a)));
        s.transcripts.get_mut(&child).unwrap().active_turn = None;

        s.release_writer_for_move(&a).unwrap();
        s.release_writer_for_move(&a).unwrap();
        assert!(s.take_move_writer_releases().is_empty());
        assert!(!s.can_send(Some(&a)));
        let (owner, event) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(owner, a);
        assert!(matches!(event, pipe::Event::Closed { .. }));
        update(&mut s, &owner, 2, Data::Stock(event)).unwrap();
        assert_eq!(
            s.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: a.clone(),
                result: Ok(())
            }]
        );
        assert!(!s.has_live_session(&a));
        assert_eq!(
            s.transcript(&b).unwrap().active_turn(),
            Some("independent-turn")
        );
        assert!(s.has_live_session(&b));
        assert!(!other.confirmed());
        assert!(s.take_handoff().is_none());
    }

    #[test]
    fn a_stale_or_unverified_claude_close_cannot_confirm_release() {
        let (mut s, a, b) = fixture();
        s.claude_mut().sessions.get_mut(&a).unwrap().move_release = Some(Default::default());
        let closed = || {
            Data::Stock(pipe::Event::Closed {
                reason: "Exited".into(),
            })
        };
        update(&mut s, &a, 1, closed()).unwrap();
        assert!(s.take_move_writer_releases().is_empty());
        update(&mut s, &a, 2, closed()).unwrap();
        let result = s.take_move_writer_releases();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].thread, a);
        assert!(result[0].result.is_err());
        assert!(s.has_live_session(&b));
    }

    #[test]
    fn a_confirmed_claude_result_reaches_the_catalog_and_replica() {
        use crate::host_protocol::{Cursor, Scope, Server, SessionOutcome};
        let (mut host, a, _) = fixture();
        let scope = Scope {
            project: host.profile.project.clone(),
            route: host.profile.route.clone(),
        };
        let user = decode(
            json!({"type":"user","session_id":A,"uuid":"prompt","message":{"content":"start"}}),
        );
        update(&mut host, &a, 2, Data::Stock(pipe::Event::Frame(user))).unwrap();
        let mut state = host.host_snapshot(&scope);
        assert!(state.sessions.iter().all(|row| row.last_result().is_none()));
        let result =
            decode(json!({"type":"result","subtype":"success","is_error":false,"session_id":A}));
        update(&mut host, &a, 2, Data::Stock(pipe::Event::Frame(result))).unwrap();
        host.host_project(&scope, &host.changed, &mut state);
        let mut replica = Store::remote_client(host.profile.clone(), |_, _| unreachable!());
        replica
            .apply_host(Server::Snapshot {
                cursor: Cursor {
                    epoch: "result".into(),
                    seq: 1,
                },
                scopes: vec![state],
            })
            .unwrap();
        let result = replica.last_result(&a).unwrap();
        assert_eq!(result.turn, "prompt");
        assert_eq!(result.outcome, SessionOutcome::Completed);
    }

    /// The replica clears its composer when the host acknowledges a resume send. When the
    /// native startup then fails, the host restores the text; that restore must reach
    /// the replica (by delta and by a later full snapshot) instead of leaving it empty.
    #[test]
    fn a_resume_that_fails_after_the_ack_puts_the_text_back_in_the_replica_composer() {
        use crate::actions::Action;
        use crate::host_protocol::{Cursor, Scope, Server};
        let (mut host, a, _b) = fixture();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        host.claude_mut().deliver = Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        });
        host.state = Connection::Ready;
        host.end_session(&a).unwrap();
        let profile = host.profile.clone();
        let scope = Scope {
            project: profile.project.clone(),
            route: profile.route.clone(),
        };
        let (actions, requests) = std::sync::mpsc::channel();
        let mut replica = Store::remote_client(profile, move |action, _| {
            actions.send(action).unwrap();
            Ok("a1".into())
        });
        let snapshot = |host: &Store, seq| Server::Snapshot {
            cursor: Cursor {
                epoch: "e".into(),
                seq,
            },
            scopes: vec![host.host_snapshot(&scope)],
        };
        replica.apply_host(snapshot(&host, 1)).unwrap();
        replica.edit(Some(&a), "back again".into());
        replica.submit(Some(&a)).unwrap();
        let action = requests.recv().unwrap();
        assert!(matches!(action, Action::SendText(..)));
        host.dispatch(action).unwrap();
        replica.apply_host(Server::Ack { id: "a1".into() }).unwrap();
        assert_eq!(replica.draft(Some(&a)), "", "cleared on the ack");
        let e = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        host.apply(e);
        assert_eq!(host.draft(Some(&a)), "back again");
        replica.edit(Some(&a), "please come back again tomorrow".into());
        replica.apply_host(snapshot(&host, 2)).unwrap();
        assert_eq!(
            replica.draft(Some(&a)),
            "back again\nplease come back again tomorrow"
        );
        replica.edit(Some(&a), "back again".into());
        // The same snapshot again (reconnect) does not duplicate it, and text typed in
        // the meantime is kept after it.
        replica.apply_host(snapshot(&host, 3)).unwrap();
        assert_eq!(replica.draft(Some(&a)), "back again");
        replica.edit(Some(&a), "back again\nmore".into());
        replica.apply_host(snapshot(&host, 4)).unwrap();
        assert_eq!(replica.draft(Some(&a)), "back again\nmore");
    }
    #[test]
    fn sending_to_an_ended_session_resumes_it_with_the_message_as_first_input() {
        let (mut s, a, _b) = fixture();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        s.claude_mut().deliver = Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        });
        s.state = Connection::Ready;
        s.end_session(&a).unwrap();
        assert!(s.is_ended(&a) && !s.can_send(Some(&a)) && s.can_compose(Some(&a)));
        s.edit(Some(&a), "back again".into());
        s.submit(Some(&a)).unwrap();
        // A Resume process now exists and holds the message until it initialises.
        let process = &s.claude().sessions[&a];
        assert!(!process.initialized);
        assert_eq!(
            process.first.as_ref().map(|f| f.1.as_str()),
            Some("back again")
        );
        assert!(!s.is_ended(&a) && s.has_live_session(&a));
        assert!(s.transcript(&a).unwrap().is_loading());
        assert_eq!(s.draft(Some(&a)), "");
        // The fixture has no saved session to resume, so the launch fails (proving the
        // Resume path ran). The unsent message returns to the composer, not lost.
        let event = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        s.apply(event);
        assert_eq!(s.draft(Some(&a)), "back again");
        assert!(!s.claude().sessions.contains_key(&a));
    }
    #[test]
    fn a_resumed_claude_send_waits_with_its_sender_statement() {
        let (mut s, a, _b) = fixture();
        s.claude_mut().deliver = Arc::new(|_| ());
        s.state = Connection::Ready;
        s.end_session(&a).unwrap();
        s.edit(Some(&a), "from the phone".into());
        s.dispatch_as(
            crate::actions::Action::Submit(Some(a.clone())),
            crate::surface::Sender::Person {
                surface: crate::surface::Surface::Phone,
            },
        )
        .unwrap();
        let process = &s.claude().sessions[&a];
        assert_eq!(
            process.first.as_ref().map(|f| f.1.as_str()),
            Some("from the phone"),
            "the user text is unchanged"
        );
        assert!(
            process
                .first_context
                .as_deref()
                .is_some_and(|c| c.contains("OX phone app")),
            "{:?}",
            process.first_context
        );
    }
    /// A host reaps an idle route and reopens it for the next send: the new store is still
    /// connecting, its catalog holds the saved row and no process exists for it yet.
    #[test]
    fn a_connecting_claude_store_resumes_a_saved_row_on_the_first_send() {
        let (mut s, a, _b) = fixture();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        s.claude_mut().deliver = Arc::new(move |event| {
            let _ = tx.lock().unwrap().send(event);
        });
        s.claude_mut().sessions.remove(&a);
        s.mark_ended(&a);
        assert_eq!(s.state, Connection::Connecting);
        assert!(s.can_compose(Some(&a)));
        s.edit(Some(&a), "after the reap".into());
        s.submit(Some(&a)).unwrap();
        assert_eq!(
            s.claude().sessions[&a].first.as_ref().map(|f| f.1.as_str()),
            Some("after the reap")
        );
        assert_eq!(s.draft(Some(&a)), "");
        // The saved session is missing in this fixture, so the launch fails and the
        // message comes back to the composer exactly once.
        let event = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        s.apply(event);
        assert_eq!(s.draft(Some(&a)), "after the reap");
        assert!(rx.try_recv().is_err());
    }
    /// A send that reopens an ended session runs its first turn in the access mode the
    /// user had chosen, whether they ended the session or the host reaped the route
    /// (the replacement store is seeded through `restore_ended`). Opening the session from
    /// the list reads its history without consuming the chosen mode.
    #[test]
    fn browsing_is_passive_and_a_resumed_send_keeps_the_access_mode() {
        let (mut s, a, b) = fixture();
        s.claude_mut().deliver = Arc::new(|_| ());
        for k in [&a, &b] {
            s.claude_mut().sessions.remove(k);
            s.restore_ended(k, "Session", 1, Some(Access::Plan));
        }
        s.edit(Some(&a), "go on".into());
        s.submit(Some(&a)).unwrap();
        assert_eq!(s.claude().sessions[&a].launch_mode.as_deref(), Some("plan"));
        s.claude_open(&b).unwrap();
        assert!(s.is_viewer(&b));
        assert!(!s.has_live_session(&b));
        assert_eq!(s.resume_access[&b], Access::Plan);
        // A resume that fails keeps the mode for the retry.
        s.claude_mut().sessions.remove(&a);
        s.mark_ended(&a);
        s.edit(Some(&a), "again".into());
        s.submit(Some(&a)).unwrap();
        assert_eq!(s.claude().sessions[&a].launch_mode.as_deref(), Some("plan"));
    }
    /// A new session nobody chose a mode for starts in the "New sessions start in"
    /// setting, or with no `--permission-mode` at all: Claude Code's own default.
    #[test]
    fn a_new_session_starts_in_the_setting_or_the_terminal_default() {
        for (setting, expected) in [(None, None), (Some(Access::Auto), Some("auto"))] {
            let (mut s, _, _) = fixture();
            s.claude_mut().deliver = Arc::new(|_| ());
            s.set_start_mode(if setting.is_some() {
                |_| Some(Access::Auto)
            } else {
                |_| None
            });
            let id = s
                .spawn_claude(
                    Session::New("00000000-0000-4000-8000-00000000000c".into()),
                    None,
                    false,
                )
                .unwrap();
            assert_eq!(s.claude().sessions[&id].launch_mode.as_deref(), expected);
            // A mode chosen in the composer still wins for its session.
            s.set_next_access(Access::Plan);
            let id = s
                .spawn_claude(
                    Session::New("00000000-0000-4000-8000-00000000000d".into()),
                    None,
                    false,
                )
                .unwrap();
            assert_eq!(
                s.claude().sessions[&id].launch_mode.as_deref(),
                Some("plan")
            );
        }
    }
    #[test]
    fn a_later_init_after_cd_keeps_the_session_but_a_first_init_elsewhere_does_not() {
        let (mut s, a, b) = fixture();
        let init = |cwd: &str| {
            decode(
                json!({"type":"system","subtype":"init","session_id":A,"uuid":"u",
                "model":"m","permissionMode":"default","slash_commands":[],"tools":[],"cwd":cwd}),
            )
        };
        // Claude Code repeats init each turn with the Bash tool's current directory.
        update(
            &mut s,
            &a,
            2,
            Data::Stock(pipe::Event::Frame(init("/fixture"))),
        )
        .unwrap();
        update(
            &mut s,
            &a,
            2,
            Data::Stock(pipe::Event::Frame(init("/fixture/sub"))),
        )
        .unwrap();
        assert!(s.claude().sessions.contains_key(&a));
        // The first init of a session must still be in the project.
        let elsewhere = decode(
            json!({"type":"system","subtype":"init","session_id":B,"uuid":"u",
            "model":"m","permissionMode":"default","slash_commands":[],"tools":[],"cwd":"/elsewhere"}),
        );
        assert!(update(&mut s, &b, 2, Data::Stock(pipe::Event::Frame(elsewhere))).is_err());
    }
    #[test]
    fn the_installed_stream_inventory_owns_claude_commands() {
        for name in ["goal", "effort", "mcp", "project:lint", "future-command"] {
            assert!(crate::slash::possible_claude_command(name));
        }
        for name in [
            "model",
            "clear",
            "__remote-workflow",
            "workflow-launch-exec",
        ] {
            assert!(!crate::slash::possible_claude_command(name));
        }
    }
    #[test]
    fn failure_retires_its_generation_only_and_restores_unsent_drafts() {
        let (mut s, a, b) = fixture();
        let request = decode(
            json!({"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"Read","tool_use_id":"tool","input":{"file_path":"a"}}}),
        );
        for owner in [&a, &b] {
            update(
                &mut s,
                owner,
                2,
                Data::Stock(pipe::Event::Frame(request.clone())),
            )
            .unwrap();
        }
        assert_ne!(s.asks[0].id, s.asks[1].id);
        s.claude_mut().sessions.get_mut(&a).unwrap().first = Some(("m".into(), "unsent".into()));
        let pending = &mut s.claude_mut().sessions.get_mut(&a).unwrap().events;
        pending.sent("delivered", "already confirmed");
        pending.sent("lost", "writer accepted, not echoed");
        let user = decode(
            json!({"type":"user","session_id":A,"uuid":"delivered","message":{"content":"already confirmed"}}),
        );
        update(&mut s, &a, 2, Data::Stock(pipe::Event::Frame(user))).unwrap();
        s.edit(Some(&a), "later edit".into());
        update(&mut s, &a, 1, Data::Failed("stale close".into())).unwrap();
        assert_eq!(s.asks.len(), 2);
        update(&mut s, &a, 2, Data::Failed("real close".into())).unwrap();
        assert!(s.transcript(&a).unwrap().last_result.is_none());
        assert_eq!(s.asks.len(), 1);
        assert_eq!(s.asks[0].thread.as_deref(), Some(b.as_str()));
        assert_eq!(
            s.draft(Some(&a)),
            "writer accepted, not echoed\nunsent\nlater edit"
        );
        assert!(!s.claude().sessions.contains_key(&a));
        assert!(s.claude().sessions.contains_key(&b));
        update(&mut s, &a, 2, Data::Stock(pipe::Event::Frame(request))).unwrap();
        assert_eq!(s.asks.len(), 1);
    }

    #[test]
    fn verified_fork_history_is_rebased_but_foreign_live_frames_close_only_their_process() {
        let (mut s, a, b) = fixture();
        let user = decode(
            json!({"type":"user","session_id":A,"uuid":"source-prompt","message":{"content":"prefix"}}),
        );
        let batch = history::fixture(&s.profile, &b, vec![user.clone()]);
        update(&mut s, &b, 2, Data::History(batch)).unwrap();
        assert_eq!(s.transcript(&b).unwrap().len(), 1);
        assert!(s.transcript(&a).is_none());
        assert!(s.transcript(&b).unwrap().active_turn().is_none());
        assert!(update(&mut s, &b, 2, Data::Stock(pipe::Event::Frame(user))).is_err());
        assert!(!s.claude().sessions.contains_key(&b));
        assert!(s.claude().sessions.contains_key(&a));
    }

    #[test]
    fn late_agent_completion_updates_its_spawn_turn_during_a_new_parent_turn() {
        let (mut s, a, _) = fixture();
        let mut d = wire::Decoder::default();
        let mut tool = None;
        for line in include_str!("../../../tests/fixtures/claude-agent.jsonl").lines() {
            let mut r: Value = serde_json::from_str(line).unwrap();
            r["session_id"] = json!(A);
            if r["type"] == "system" && r["subtype"] == "init" {
                r["cwd"] = json!("/fixture");
            }
            if r["subtype"] == "task_started" {
                tool = r["tool_use_id"].as_str().map(str::to_owned);
            }
            if r["subtype"] == "task_notification" {
                s.transcripts.get_mut(&a).unwrap().active_turn = Some("later-turn".into());
            }
            for e in d.decode_value(r).unwrap() {
                update(&mut s, &a, 2, Data::Stock(pipe::Event::Frame(e))).unwrap();
            }
        }
        let tool = tool.unwrap();
        let child = child_key(&a, &tool);
        assert_eq!(
            s.catalog_row(&child).unwrap().parent_thread_id.as_deref(),
            Some(a.as_str())
        );
        let transcript = s.transcript(&child).unwrap();
        assert!(transcript.active_turn().is_none());
        assert_eq!(
            transcript.last_result.as_ref().unwrap().outcome,
            crate::host_protocol::SessionOutcome::Completed
        );
        let t = s.transcript(&a).unwrap();
        let first = t
            .entry(&crate::transcript::ItemKey {
                turn: tool.clone(),
                item: tool.clone(),
            })
            .unwrap();
        assert!(
            matches!(&first.item,Item::AgentOperation{status,receiver_thread_ids,..} if status=="completed" && receiver_thread_ids==&[child])
        );
        assert!(
            t.entry(&crate::transcript::ItemKey {
                turn: "later-turn".into(),
                item: tool
            })
            .is_none()
        );
    }

    #[test]
    fn ending_a_session_drops_its_process_and_keeps_it_resumable() {
        let (mut s, a, b) = fixture();
        let alive = s.claude().sessions[&a].events.lifetime();
        assert!(alive.upgrade().is_some());
        assert_eq!(s.live_threads(), {
            let mut both = vec![a.clone(), b.clone()];
            both.sort();
            both
        });
        assert!(s.has_live_session(&a));
        let request = decode(
            json!({"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"Read","tool_use_id":"tool","input":{"file_path":"a"}}}),
        );
        for owner in [&a, &b] {
            update(
                &mut s,
                owner,
                2,
                Data::Stock(pipe::Event::Frame(request.clone())),
            )
            .unwrap();
        }
        assert_eq!(s.asks.len(), 2);
        s.claude_mut().sessions.get_mut(&a).unwrap().first = Some(("m".into(), "unsent".into()));
        s.end_session(&a).unwrap();
        // The process (and with it its link and process group) is gone.
        assert!(alive.upgrade().is_none());
        assert!(!s.claude().sessions.contains_key(&a));
        assert!(s.claude().sessions.contains_key(&b));
        assert_eq!(s.live_threads(), vec![b.clone()]);
        assert!(!s.has_live_session(&a));
        assert!(s.is_ended(&a) && !s.is_ended(&b));
        // Still listed, closed for sending, its unsent text back in the draft, no stale approval.
        assert!(s.catalog_row(&a).is_some());
        assert!(!s.can_send(Some(&a)));
        assert_eq!(s.draft(Some(&a)), "unsent");
        assert_eq!(s.asks.len(), 1);
        assert_eq!(s.asks[0].thread.as_deref(), Some(b.as_str()));
        assert_eq!(
            s.state(),
            &Connection::Connecting,
            "ending does not close the route"
        );
        assert!(s.transcript(&a).is_none_or(|t| t.error().is_none()));
        // A second end, and the other session, are unaffected.
        assert!(s.end_session(&a).is_err());
        assert!(s.has_live_session(&b));
    }
}
