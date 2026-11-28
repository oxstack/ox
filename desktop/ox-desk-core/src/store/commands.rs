use super::{Connection, Pending, Store};
use crate::runtime::Billing;
use crate::{
    changed::Changed,
    codex::{
        reply::Reply,
        request::{Call, ThreadCommand},
    },
    slash::{Action, Capabilities, Goal, NativeCommand},
};
use std::{collections::HashSet, io};

#[derive(Default)]
pub(super) struct Catalog {
    pub(super) schema: Option<Capabilities>,
    pub(super) loading: bool,
    cursors: HashSet<String>,
}

#[derive(Debug)]
pub(super) enum Discovery {
    Features,
    Models,
    Config,
}

impl Store {
    pub fn has_command_schema(&self) -> bool {
        if let Some(remote) = self.remote() {
            return remote.schema;
        }
        self.is_session_scoped() || self.commands.schema.is_some()
    }
    pub fn commands_loading(&self) -> bool {
        self.commands.loading
    }
    pub fn install_command_schema(&mut self, mut caps: Capabilities) -> io::Result<()> {
        if self.remote().is_some() {
            return Ok(());
        }
        if self.has_command_schema() {
            return Ok(());
        }
        caps.provider.clone_from(&self.profile.provider);
        let features = caps.methods.contains("experimentalFeature/list");
        self.commands.schema = Some(caps);
        self.commands.loading = true;
        if features {
            self.request(Call::Features(None), Pending::Discover(Discovery::Features))
        } else {
            self.discover_models()
        }
    }
    fn discover_models(&mut self) -> io::Result<()> {
        self.commands.cursors.clear();
        if self
            .commands
            .schema
            .as_ref()
            .is_some_and(|s| s.methods.contains("model/list"))
        {
            self.request(
                Call::Models {
                    provider: self.profile.provider.clone(),
                    cursor: None,
                },
                Pending::Discover(Discovery::Models),
            )
        } else {
            self.discover_config()
        }
    }
    fn discover_config(&mut self) -> io::Result<()> {
        self.commands.cursors.clear();
        if self
            .commands
            .schema
            .as_ref()
            .is_some_and(|s| s.methods.contains("config/read"))
        {
            self.request(
                Call::Config(self.profile.project.clone()),
                Pending::Discover(Discovery::Config),
            )
        } else {
            self.finish_catalog();
            Ok(())
        }
    }
    fn finish_catalog(&mut self) {
        self.commands.loading = false;
        if let Billing::Api(alias) = &self.profile.route.billing
            && let Some(caps) = &mut self.commands.schema
        {
            caps.model.clone_from(alias);
        }
        // Discovery enables native commands and model changes on existing session rows too.
        self.changed.mark(Changed::NOTICE | Changed::THREADS);
    }
    pub(super) fn discovery_failed(&mut self, stage: Discovery, why: String) {
        self.notice = Some(why);
        // A failed optional method does not disable independent commands.
        if self.state != Connection::Ready {
            self.finish_catalog();
            return;
        }
        match stage {
            Discovery::Features => {
                let _ = self.discover_models();
            }
            Discovery::Models => {
                let _ = self.discover_config();
            }
            Discovery::Config => self.finish_catalog(),
        }
    }
    pub(super) fn discovery_reply(&mut self, stage: Discovery, reply: Reply) -> io::Result<()> {
        let Some(caps) = &mut self.commands.schema else {
            return Ok(());
        };
        match (stage, reply) {
            (Discovery::Features, Reply::Features(page)) => {
                caps.goals |= page.data.iter().any(|f| f.name == "goals" && f.enabled);
                if let Some(cursor) = page.next_cursor
                    && self.commands.cursors.insert(cursor.clone())
                {
                    self.request(
                        Call::Features(Some(cursor)),
                        Pending::Discover(Discovery::Features),
                    )
                } else {
                    self.discover_models()
                }
            }
            (Discovery::Models, Reply::Models(page)) => {
                for model in page.data {
                    caps.add_model(model);
                }
                if let Some(cursor) = page.next_cursor
                    && self.commands.cursors.insert(cursor.clone())
                {
                    self.request(
                        Call::Models {
                            provider: self.profile.provider.clone(),
                            cursor: Some(cursor),
                        },
                        Pending::Discover(Discovery::Models),
                    )
                } else {
                    self.discover_config()
                }
            }
            (Discovery::Config, Reply::Config(config)) => {
                if let Some(model) = config.config.model
                    && caps.model != model
                {
                    caps.model = model;
                    caps.tier = None;
                }
                if let Some(tier) = config.config.service_tier {
                    caps.tier = Some(tier);
                }
                self.finish_catalog();
                Ok(())
            }
            (stage, _) => {
                self.discovery_failed(stage, "Unexpected command discovery reply".into());
                Ok(())
            }
        }
    }
    pub fn command_caps(&self, id: Option<&str>) -> Capabilities {
        if let Some(remote) = self.remote() {
            if let Some(caps) = id.and_then(|id| remote.commands.get(id)) {
                return caps.clone();
            }
        } else if self.is_antigravity() {
            return self.antigravity_caps(id);
        } else if self.is_session_scoped() {
            return self.claude_caps(id);
        }
        let mut caps = self.commands.schema.clone().unwrap_or_default();
        if let Some(t) = id.and_then(|id| self.transcripts.get(id))
            && let Some(model) = &t.model
        {
            caps.model.clone_from(model);
            caps.tier.clone_from(&t.tier);
        }
        caps
    }
    /// An unknown command on a cold Claude route can be an installed project skill.
    /// Only its submission asks the runtime which commands this session supports.
    pub fn parse_command(&self, id: Option<&str>, text: &str) -> Option<crate::slash::Action> {
        if matches!(
            self.profile.route.runtime,
            crate::runtime::Runtime::Antigravity | crate::runtime::Runtime::OpenCode
        ) {
            return None;
        }
        let caps = self.command_caps(id);
        let action = crate::slash::parse(text, &caps)?;
        if action == Action::Terminal
            && self.is_session_scoped()
            && caps.session_commands.is_none()
            && text
                .split_whitespace()
                .next()
                .and_then(|s| s.strip_prefix('/'))
                .is_some_and(|name| {
                    // Account commands must reach the interactive client even before sign-in.
                    !matches!(name, "login" | "logout")
                        && crate::slash::possible_claude_command(name)
                })
        {
            Some(Action::DiscoverClaudeCommand)
        } else {
            Some(action)
        }
    }

    pub fn command_notice(&self) -> Option<&str> {
        self.selected()
            .and_then(|id| self.transcript(id))
            .and_then(|t| t.command_notice.as_deref())
            .or(self.notice())
    }
    pub fn dismiss_notice(&mut self) {
        if self.remote().is_some() {
            let _ = self.dispatch_remote(crate::actions::Action::DismissNotice);
            return;
        }
        self.notice = None;
        if let Some(t) = self
            .selected
            .as_ref()
            .and_then(|id| self.transcripts.get_mut(id))
        {
            t.command_notice = None;
        }
    }
    pub(crate) fn run_command(
        &mut self,
        id: Option<&str>,
        palette_text: Option<&str>,
    ) -> io::Result<super::super::actions::Effect> {
        use crate::actions::Effect;
        if (palette_text.is_none() && self.selected() != id)
            || id.is_some_and(|id| self.transcript(id).is_none())
        {
            return Err(io::Error::other("The selected thread changed"));
        }
        let caps = self.command_caps(id);
        let text = palette_text.unwrap_or_else(|| self.draft(id)).trim();
        let action = self
            .parse_command(id, text)
            .ok_or_else(|| io::Error::other("Enter a slash command"))?;
        if !matches!(
            action,
            Action::New | Action::Resume | Action::Native(NativeCommand::Status)
        ) && (self.commands_loading() || !self.has_command_schema())
        {
            return Err(io::Error::other(
                "Discovering commands from the installed runtime",
            ));
        }
        let message = match action {
            Action::New => {
                if palette_text.is_none() {
                    self.edit(id, String::new());
                }
                self.new_session();
                return Ok(Effect::Applied);
            }
            Action::Resume => {
                if palette_text.is_none() {
                    self.edit(id, String::new());
                }
                return Ok(Effect::FocusSearch);
            }
            Action::Terminal => {
                self.handoff_text(id.map(str::to_owned), text.to_owned())?;
                return Ok(Effect::Applied);
            }
            Action::DiscoverClaudeCommand => {
                let command = text.to_owned();
                self.defer_claude_command(id, &command, true)?;
                if palette_text.is_none() {
                    self.edit(id, String::new());
                }
                return Ok(Effect::Applied);
            }
            Action::SessionCommand => {
                let command = text.to_owned();
                self.claude_submit(id, Some(&command))?;
                if palette_text.is_none() {
                    self.edit(id, String::new());
                }
                return Ok(Effect::Applied);
            }
            Action::Hint(message) => return Err(io::Error::other(message)),
            Action::Native(NativeCommand::Model(model)) => {
                self.choose(id, super::routes_thread::Choice::Model(model))?;
                if palette_text.is_none() {
                    self.edit(id, String::new());
                }
                return Ok(Effect::Applied);
            }
            Action::Native(NativeCommand::Status) => {
                format!(
                    "{} · {} · {} speed · {}",
                    self.thread_route(id).label(),
                    caps.model,
                    caps.tier.as_deref().unwrap_or("default"),
                    self.profile.project.display()
                )
            }
            Action::Native(NativeCommand::Speed(None)) => {
                let tiers = std::iter::once("default")
                    .chain(caps.tiers().iter().map(|t| t.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Current speed: {}. Available: {tiers}.",
                    caps.tier.as_deref().unwrap_or("default")
                )
            }
            Action::Native(NativeCommand::Goal(Goal::Show)) if id.is_none() => {
                "This new thread has no goal.".into()
            }
            Action::Native(command) => {
                if id.is_none()
                    && !matches!(
                        command,
                        NativeCommand::Speed(_) | NativeCommand::Goal(Goal::Set(_))
                    )
                {
                    return Err(io::Error::other("Open a session first"));
                }
                if !command.allowed_during_turn()
                    && id
                        .and_then(|id| self.transcript(id))
                        .is_some_and(|t| t.active_turn.is_some())
                {
                    return Err(io::Error::other("Wait for the active turn or stop it"));
                }
                let cmd = match command {
                    NativeCommand::Goal(goal) => ThreadCommand::Goal(goal),
                    NativeCommand::Speed(Some(name)) => ThreadCommand::Tier(
                        caps.speed_choice(&name)
                            .map_err(io::Error::other)?
                            .map(str::to_owned),
                    ),
                    NativeCommand::Rename(name) => ThreadCommand::Rename(name),
                    NativeCommand::Compact => ThreadCommand::Compact,
                    _ => unreachable!(),
                };
                self.submit_operation(id, Some(cmd), palette_text.is_some())?;
                return Ok(Effect::Applied);
            }
        };
        if palette_text.is_none() {
            self.edit(id, String::new());
        }
        if let Some(t) = id.and_then(|id| self.transcripts.get_mut(id)) {
            t.command_notice = Some(message);
        } else {
            self.notice = Some(message);
        }
        Ok(Effect::Applied)
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Handoff {
    #[serde(default)]
    pub argv: Vec<String>,
    pub thread: Option<String>,
    pub command: String,
}

impl Store {
    pub fn handoff_argv(&self, thread: Option<&str>) -> io::Result<Vec<String>> {
        if self.profile.route.runtime == crate::runtime::Runtime::OpenCode {
            return Err(io::Error::other(
                "Hosted OpenCode sessions stay on their owning host",
            ));
        }
        if self.profile.route.runtime == crate::runtime::Runtime::Antigravity {
            return Err(io::Error::other(
                "This Antigravity session runs through ACP.",
            ));
        }
        let mut route = self.thread_route(thread);
        if self.is_session_scoped()
            && let crate::runtime::Billing::Api(alias) = &mut route.billing
        {
            let selected = self.options(thread).model;
            if crate::runtime::same_api_provider(alias, &selected) {
                *alias = selected;
            }
        }
        let native = thread.map(|id| self.terminal_session_id(id)).transpose()?;
        Ok(crate::runtime::terminal_args(
            &route,
            &self.profile.provider,
            native,
        ))
    }
    pub fn in_terminal(&self, id: &str) -> bool {
        self.terminal_threads.contains(id)
    }
    pub fn opening_terminal(&self) -> bool {
        self.handing_off_claude()
            || self
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::LeaveForTerminal(_)))
    }
    pub fn take_handoff(&mut self) -> Option<Handoff> {
        self.handoff_ready.take()
    }
    pub(crate) fn handoff(&mut self, thread: Option<String>) -> io::Result<()> {
        let command = self.draft(thread.as_deref()).trim().to_owned();
        self.handoff_text(thread, command)
    }
    pub(crate) fn handoff_text(
        &mut self,
        thread: Option<String>,
        command: String,
    ) -> io::Result<()> {
        let saved_claude = thread
            .as_deref()
            .is_some_and(|id| self.can_configure_saved_claude(id));
        if !saved_claude && !self.can_send(thread.as_deref()) {
            return Err(io::Error::other("Wait for this thread to be ready"));
        }
        if self.handing_off_claude()
            || self.handoff_ready.is_some()
            || self
                .pending
                .values()
                .any(|(_, p)| matches!(p, Pending::LeaveForTerminal(_)))
        {
            return Err(io::Error::other("A terminal handoff is already starting"));
        }
        let handoff = Handoff {
            argv: self.handoff_argv(thread.as_deref())?,
            thread: thread.clone(),
            command,
        };
        if let Some(id) = thread {
            if self
                .transcript(&id)
                .is_some_and(|t| t.active_turn().is_some())
                || self.asks.iter().any(|a| a.thread.as_ref() == Some(&id))
                || (self.is_session_scoped()
                    && (id.contains('/')
                        || self.transcripts.iter().any(|(child, t)| {
                            child.strip_prefix(&id).is_some_and(|p| p.starts_with('/'))
                                && (t.active_turn().is_some()
                                    || self.asks.iter().any(|a| a.thread.as_ref() == Some(child)))
                        })))
            {
                return Err(io::Error::other(
                    "Finish the current turn and its workers before opening the parent session in the terminal",
                ));
            }
            if saved_claude && self.native_live_now(&id) {
                return Err(io::Error::other(self.native_live_hint()));
            }
            self.terminal_threads.insert(id.clone());
            if saved_claude {
                self.handoff_ready = Some(handoff);
                Ok(())
            } else if self.is_session_scoped() {
                self.release_claude(&id, handoff)
            } else {
                self.request(Call::Unsubscribe(id), Pending::LeaveForTerminal(handoff))
            }
        } else {
            self.handoff_ready = Some(handoff);
            Ok(())
        }
    }
    pub(crate) fn resume_here(&mut self, id: &str) -> io::Result<()> {
        if self
            .pending
            .values()
            .any(|(_, p)| matches!(p,Pending::LeaveForTerminal(h) if h.thread.as_deref()==Some(id)))
        {
            return Err(io::Error::other(
                "Codex is still closing its terminal for this thread. Wait a few seconds, then try again.",
            ));
        }
        if self.handing_off_claude() {
            return Err(io::Error::other(
                "Claude Code is still finishing the previous turn for this session. Wait a few seconds, then try again.",
            ));
        }
        if !self.terminal_threads.remove(id) {
            return Ok(());
        }
        // The terminal can have appended to the rollout. Load its authoritative tail.
        self.transcripts.remove(id);
        if self.is_session_scoped() {
            self.claude_resume(id, false)
        } else {
            self.open_catalog_id(id)
        }
    }
}
