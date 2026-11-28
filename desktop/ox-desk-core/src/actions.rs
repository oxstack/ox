//! Every workbench action declares who owns its effect. The schema contract checks
//! the same Method values that the pipe writer uses.
use crate::{
    runtime::{Access, RequestId},
    store::Store,
};
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    OpenCatalogRow,
    SelectThread,
    RenameThread,
    ArchiveThread,
    ForkThread,
    LoadOlder,
    SearchThreads,
    MoreThreads,
    ExecuteCommand,
    PaletteCommand,
    Submit,
    Stop,
    ModelOptions,
    ReadAccountLimits,
    SetAccess,
    ResumeHere,
}

#[derive(Clone, Copy, Debug)]
pub enum Target {
    Local,
    /// The session host performs it across scopes.
    Host,
    Runtime(Operation),
    ServerReply,
    NativeCli,
    Terminal,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MoveSession {
    pub thread: String,
    pub destination: String,
    pub operation: String,
    pub after_turn: bool,
}

macro_rules! actions {
    ($($name:ident $(($($arg:ty),+))? => $target:expr),+ $(,)?) => {
        #[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
        #[cfg_attr(feature="schema",derive(schemars::JsonSchema))]
        #[serde(rename_all="snake_case")]
        pub enum Action { $($name $(($($arg),+))?),+ }
        pub const ACTION_TARGETS:&[(&str, Target)]=&[$((stringify!($name),$target)),+];
    }
}
// `YoloGrant` stores or revokes a session's grant for its workers and handoffs.
// `InjectHandoff` is host-internal: it opens a session with carried handoff
// context. Clients cannot send it; the host builds it from a validated handoff.
actions! {
    OpenCatalogRow(Box<crate::store::ThreadRow>)=>Target::Runtime(Operation::OpenCatalogRow),
    ConfirmAliasAndResume(Box<crate::store::ThreadRow>)=>Target::Runtime(Operation::OpenCatalogRow),
    SelectThread(String)=>Target::Runtime(Operation::SelectThread),
    NewDraft=>Target::Local,
    GenerateTitle(String)=>Target::NativeCli,
    RenameThread(String,String)=>Target::Runtime(Operation::RenameThread),
    ArchiveThread(String)=>Target::Runtime(Operation::ArchiveThread),
    ForkThread(String)=>Target::Runtime(Operation::ForkThread),
    DraftEdit(Option<String>,String)=>Target::Local,
    Mention(Option<String>,Option<String>)=>Target::Runtime(Operation::SearchThreads),
    PaletteSearch(String)=>Target::Runtime(Operation::SearchThreads),
    PaletteMore=>Target::Runtime(Operation::MoreThreads),
    LoadOlder(String)=>Target::Runtime(Operation::LoadOlder),
    SearchThreads(String)=>Target::Runtime(Operation::SearchThreads),
    MoreThreads=>Target::Runtime(Operation::MoreThreads),
    ExecuteCommand(Option<String>)=>Target::Runtime(Operation::ExecuteCommand),
    NativeCommand(Option<String>,String)=>Target::Runtime(Operation::PaletteCommand),
    PaletteCommand(Option<String>,String)=>Target::Runtime(Operation::PaletteCommand),
    StartWorker(crate::origin::WorkerStart)=>Target::Host,
    StartHandoff(crate::handoff::HandoffStart)=>Target::Host,
    MoveSession(MoveSession)=>Target::Host,
    CancelMove(String)=>Target::Host,
    CheckMove(String)=>Target::Host,
    RetryMove(String)=>Target::Host,
    YoloGrant(String,bool)=>Target::Host,
    InjectHandoff(String,Vec<serde_json::Value>)=>Target::Runtime(Operation::Submit),
    DismissNotice=>Target::Local,
    TerminalText(Option<String>,String)=>Target::Terminal,
    SendImages(Option<String>,String,Vec<crate::mentions::ImageInput>)=>Target::Runtime(Operation::Submit),
    SendText(Option<String>,String)=>Target::Runtime(Operation::Submit),
    Submit(Option<String>)=>Target::Runtime(Operation::Submit),
    Stop(String)=>Target::Runtime(Operation::Stop),
    EndSession(String)=>Target::Host,
    ForceEndSession(String)=>Target::Host,
    AnswerAsk(RequestId,usize)=>Target::ServerReply,
    AnswerQuestions(RequestId,Vec<String>)=>Target::ServerReply,
    SetNextAccess(Access)=>Target::Local,
    ModelOptions(Option<String>,crate::store::routes_thread::Choice)=>Target::Runtime(Operation::ModelOptions),
    ReadAccountLimits=>Target::Runtime(Operation::ReadAccountLimits),
    SetAccess(String,Access)=>Target::Runtime(Operation::SetAccess),
    Terminal(Option<String>)=>Target::Terminal,
    ResumeHere(String)=>Target::Runtime(Operation::ResumeHere),
}
pub enum Effect {
    Applied,

    FocusSearch,
}
impl Store {
    pub fn dispatch(&mut self, action: Action) -> io::Result<Effect> {
        if self.remote().is_some() {
            return self.dispatch_remote(action);
        }
        match action {
            Action::ConfirmAliasAndResume(row) => self.select_catalog_origin(*row, true)?,
            Action::OpenCatalogRow(row) => self.select_from_catalog(*row)?,
            Action::SelectThread(id) => self.select(&id)?,
            Action::GenerateTitle(id) => self.generate_thread_title(&id)?,
            Action::NewDraft => self.new_session(),
            Action::RenameThread(id, name) => {
                self.manage_thread(&id, super::store::ThreadOperation::Rename(name))?
            }
            Action::ArchiveThread(id) => {
                self.manage_thread(&id, super::store::ThreadOperation::Archive)?
            }
            Action::ForkThread(id) => {
                self.manage_thread(&id, super::store::ThreadOperation::Fork)?
            }

            Action::DraftEdit(id, text) => self.edit(id.as_deref(), text),
            Action::Mention(id, query) => self.find_mention(id.as_deref(), query.as_deref())?,
            Action::PaletteSearch(query) => self.palette_search(&query)?,
            Action::PaletteMore => self.palette_more()?,
            Action::LoadOlder(id) => self.load_older(&id)?,
            Action::SearchThreads(query) => self.search(query)?,
            Action::MoreThreads => self.list_more()?,
            Action::ExecuteCommand(id) => return self.run_command(id.as_deref(), None),
            Action::NativeCommand(id, text) => {
                // This action can never fall through to a terminal on the host. Check the
                // current route schema immediately before applying the native command.
                match self.parse_command(id.as_deref(), &text) {
                    Some(crate::slash::Action::DiscoverClaudeCommand) => {
                        self.defer_claude_command(id.as_deref(), &text, false)?;
                    }
                    Some(
                        crate::slash::Action::Native(_) | crate::slash::Action::SessionCommand,
                    ) => return self.run_command(id.as_deref(), Some(&text)),
                    Some(crate::slash::Action::Hint(message)) => {
                        return Err(io::Error::other(message));
                    }
                    _ => {
                        let name = text.split_whitespace().next().unwrap_or("This command");
                        return Err(io::Error::other(format!(
                            "{name} is not available on this model route. Type /status to see what this session supports."
                        )));
                    }
                }
            }
            Action::PaletteCommand(id, text) => {
                return self.run_command(id.as_deref(), Some(&text));
            }
            Action::StartWorker(_) | Action::StartHandoff(_) | Action::YoloGrant(..) => {
                return Err(io::Error::other(
                    "Workers and handoffs start through the OX session host.",
                ));
            }
            Action::MoveSession(_)
            | Action::CancelMove(_)
            | Action::CheckMove(_)
            | Action::RetryMove(_) => {
                return Err(io::Error::other(
                    "Session moves require the OX session host.",
                ));
            }
            Action::InjectHandoff(text, items) => self.start_handoff(text, items)?,
            Action::DismissNotice => self.dismiss_notice(),
            Action::TerminalText(id, text) => self.handoff_text(id, text)?,
            Action::SendImages(owner, text, images) => {
                self.send_client_message(owner, text, images)?
            }
            Action::SendText(owner, text) => self.send_client_message(owner, text, vec![])?,
            Action::Submit(id) => self.submit(id.as_deref())?,
            Action::Stop(id) => self.stop(&id)?,
            Action::EndSession(id) | Action::ForceEndSession(id) => self.end_session(&id)?,
            Action::AnswerAsk(id, choice) => self.answer(&id, choice)?,
            Action::AnswerQuestions(id, answers) => self.answer_questions(&id, &answers)?,
            Action::SetNextAccess(preset) => self.set_next_access(preset),
            Action::ModelOptions(id, choice) => self.choose(id.as_deref(), choice)?,
            Action::ReadAccountLimits => self.read_limits()?,
            Action::SetAccess(id, preset) => self.set_access(&id, preset)?,
            Action::Terminal(id) => {
                if id.as_deref() != self.selected() {
                    return Err(io::Error::other("The selected thread changed"));
                }
                self.handoff(id)?;
            }
            Action::ResumeHere(id) => self.resume_here(&id)?,
        }
        Ok(Effect::Applied)
    }
}
