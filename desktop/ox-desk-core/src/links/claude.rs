//! One stock Claude Code process per session. OX selects the native account or relay.
use crate::claude::{Account, ControlReply, Decoder, Event as Frame};
pub use crate::runtime::Billing;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

pub mod context;
mod process;
pub(crate) use process::ReapStatus;
pub const INIT_ID: &str = "ox-init";

pub struct Launch {
    pub ox: PathBuf,
    pub path: OsString,
    pub project: PathBuf,
    pub billing: Billing,
    pub session: Session,
    /// `None` passes no `--permission-mode`: Claude Code's configured default.
    pub permission_mode: Option<String>,
    /// A worker launch carries the shared worker contract (tools, sandbox
    /// settings); an ordinary session passes `None`.
    pub contract: Option<crate::claude_worker_contract::Contract>,
    /// The selected subscription model or exact worker pin, passed as `--model` so the
    /// first inference already uses it. Ignored on an API route, whose alias is the model.
    pub model: Option<String>,
}
#[derive(Clone, Debug)]
pub enum Session {
    New(String),
    Resume(String),
    Fork { source: String, id: String },
}
impl Session {
    pub fn id(&self) -> &str {
        match self {
            Self::New(id) | Self::Resume(id) | Self::Fork { id, .. } => id,
        }
    }
}

#[derive(Debug)]
pub enum Event {
    Frame(Frame),
    SignInRequired,
    Closed { reason: String },
}

impl Launch {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.ox);
        cmd.current_dir(&self.project)
            .env("PATH", &self.path)
            .arg("claude");
        if std::env::var_os("OX_HOST_LIFETIME_SOCKET").is_some() {
            let tag = match self.billing {
                Billing::Api(_) => "api",
                Billing::Subscription => "subscription",
            };
            let route = crate::runtime::Route {
                runtime: crate::runtime::Runtime::Claude,
                billing: self.billing.clone(),
            };
            cmd.env(
                "OX_HOST_WORKER_ROUTE",
                serde_json::to_string(&route).expect("serializable billing route"),
            )
            .env(
                "OX_MEMORY_MAX_SENSITIVITY",
                crate::memory::route_sensitivity_ceiling(&route).as_str(),
            )
            .env(
                "OX_HOST_WORKER_PARENT",
                format!("claude:{tag}:{}", self.session.id()),
            );
        }
        match &self.billing {
            Billing::Subscription => {
                cmd.arg("--subscription");
                if let Some(model) = self
                    .model
                    .as_deref()
                    .filter(|m| crate::claude_models::valid_model(m))
                {
                    cmd.args(["--model", model]);
                }
            }
            Billing::Api(alias) => {
                let model = self
                    .model
                    .as_deref()
                    .filter(|m| crate::runtime::same_api_provider(alias, m))
                    .unwrap_or(alias);
                cmd.args(["--model", model]);
            }
        }
        cmd.args([
            "-p",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--replay-user-messages",
            "--permission-prompt-tool",
            "stdio",
        ]);
        if let Some(mode) = &self.permission_mode {
            cmd.args(["--permission-mode", mode]);
        }
        cmd.arg(
            if self.permission_mode.as_deref() == Some("bypassPermissions") {
                "--dangerously-skip-permissions"
            } else {
                "--allow-dangerously-skip-permissions"
            },
        );
        if let Some(contract) = &self.contract {
            cmd.args(contract.args());
        }
        match &self.session {
            Session::New(id) => {
                cmd.args(["--session-id", id]);
            }
            Session::Resume(id) => {
                cmd.args(["--resume", id]);
            }
            Session::Fork { source, id } => {
                cmd.args(["--resume", source, "--fork-session", "--session-id", id]);
            }
        }
        cmd
    }
}

pub struct Link {
    process: process::Process,
    ready: Arc<AtomicBool>,
    session_id: String,
    billing: Billing,
    seq: AtomicU64,
    /// Accepted submits awaiting their `UserPromptSubmit` callback, in stdin order.
    ledger: Arc<Mutex<context::Ledger>>,
}

impl Link {
    pub fn spawn(
        launch: Launch,
        receive: impl Fn(Event) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let ready = Arc::new(AtomicBool::new(false));
        let initialized = ready.clone();
        let billing = launch.billing.clone();
        let session_id = launch.session.id().to_owned();
        let project = launch.project.canonicalize()?;
        let receive = Arc::new(receive);
        let output = receive.clone();
        let mut decoder = Decoder::default();
        let mut project_checked = false;
        let ledger = Arc::new(Mutex::new(context::Ledger::default()));
        let answers = ledger.clone();
        let writer = Arc::new(OnceLock::<mpsc::Sender<Value>>::new());
        let hook_writer = writer.clone();
        let process = process::Process::spawn(
            launch.command(),
            move |line| {
                let row: Value = serde_json::from_slice(line).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid Claude stream JSON")
                })?;
                if let Some((id, prompt)) = context_request(&row) {
                    // Answer on receipt: stock Claude drops the prompt if this times out.
                    let answer = answers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .answer(prompt);
                    let sent = hook_writer.get().is_some_and(|w| {
                        w.send(
                            json!({"type":"control_response","response":{"subtype":"success",
                            "request_id":id,"response":context::reply(&answer)}}),
                        )
                        .is_ok()
                    });
                    return if sent {
                        Ok(())
                    } else {
                        Err(io::Error::other("Claude Code writer closed"))
                    };
                }
                for frame in decoder.decode_value(row)? {
                    if let Frame::ControlReply(reply) = &frame
                        && reply.request_id == INIT_ID
                    {
                        if let Err(error) = verify_initialization(reply, &billing) {
                            if billing == Billing::Subscription
                                && error.kind() == io::ErrorKind::PermissionDenied
                            {
                                output(Event::SignInRequired);
                            }
                            return Err(error);
                        }
                        initialized.store(true, Ordering::Release);
                    }
                    if !initialized.load(Ordering::Acquire) {
                        continue;
                    }
                    if let Frame::Init(init) = &frame {
                        // Claude Code repeats init on every turn and reports the Bash
                        // tool's current directory, which follows `cd`. Only the
                        // first init must match the project; later ones only the session.
                        let first = !project_checked;
                        project_checked = true;
                        if init.session_id != session_id
                            || first && std::path::Path::new(&init.cwd).canonicalize()? != project
                        {
                            return Err(io::Error::other(
                                "Claude Code opened a different project or session",
                            ));
                        }
                    }
                    output(Event::Frame(frame));
                }
                Ok(())
            },
            ready.clone(),
            move |reason, needs_login| {
                if needs_login {
                    receive(Event::SignInRequired);
                }
                receive(Event::Closed { reason });
            },
        )?;
        let _ = writer.set(process.writer());
        let mut request = json!({"subtype":"initialize","forwardSubagentText":true});
        let fields = context::initialize_fields(&launch.billing, launch.contract.is_some());
        request
            .as_object_mut()
            .expect("initialize object")
            .extend(fields.as_object().expect("context fields").clone());
        process.send(json!({"type":"control_request","request_id":INIT_ID,"request":request}))?;
        Ok(Self {
            process,
            ready,
            session_id: launch.session.id().to_owned(),
            billing: launch.billing,
            seq: AtomicU64::new(1),
            ledger,
        })
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub(crate) fn reap_status(&self) -> ReapStatus {
        self.process.reap_status()
    }
    pub(crate) fn process_group(&self) -> u32 {
        self.process.group
    }
    /// Sends a user message without a known submitting interface; the model is
    /// told the interface is unknown.
    pub fn send_user(&self, uuid: &str, content: Vec<Value>) -> io::Result<()> {
        self.submit(uuid, content, None)
    }
    /// Sends a user message and delivers `context` (for example which OX
    /// interface sent it) to the model alongside that message only. The user's
    /// content is sent unchanged; the context never enters it or its replay.
    pub fn send_user_with_context(
        &self,
        uuid: &str,
        content: Vec<Value>,
        context: &str,
    ) -> io::Result<()> {
        self.submit(uuid, content, Some(context))
    }
    fn submit(&self, uuid: &str, content: Vec<Value>, context: Option<&str>) -> io::Result<()> {
        // Record and write under one lock so the ledger order is the stdin order.
        let mut ledger = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        let seq = ledger.submit(&content, context);
        let sent = self.send(
            json!({"type":"user","uuid":uuid,"session_id":self.session_id,
            "parent_tool_use_id":Value::Null,"message":{"role":"user","content":content}}),
        );
        if sent.is_err() {
            ledger.withdraw(seq);
        }
        sent
    }
    pub fn interrupt(&self) -> io::Result<String> {
        self.control(json!({"subtype":"interrupt"}))
    }
    pub fn set_mode(&self, mode: &str) -> io::Result<String> {
        self.control(json!({"subtype":"set_permission_mode","mode":mode}))
    }
    pub fn set_model(&self, model: &str) -> io::Result<String> {
        if let Billing::Api(alias) = &self.billing
            && !crate::runtime::same_api_provider(alias, model)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Open the selected OX API route to change its billing alias",
            ));
        }
        self.control(json!({"subtype":"set_model","model":model}))
    }
    fn control(&self, request: Value) -> io::Result<String> {
        let id = format!("ox-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        self.send(json!({"type":"control_request","request_id":id,"request":request}))?;
        Ok(id)
    }
    pub fn respond(&self, id: &str, payload: Value) -> io::Result<()> {
        self.send(json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":payload}}))
    }
    pub fn reject_request(&self, id: &str, message: &str) -> io::Result<()> {
        self.send(json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":message}}))
    }
    fn send(&self, value: Value) -> io::Result<()> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Claude Code is connecting",
            ));
        }
        self.process.send(value)
    }
}

/// The request ID and prompt of an OX context callback.
fn context_request(row: &Value) -> Option<(&str, &str)> {
    let request = &row["request"];
    if row["type"] != "control_request"
        || request["subtype"] != "hook_callback"
        || request["callback_id"] != context::CALLBACK_ID
        || request["input"]["hook_event_name"] != "UserPromptSubmit"
    {
        return None;
    }
    Some((
        row["request_id"].as_str()?,
        request["input"]["prompt"].as_str().unwrap_or(""),
    ))
}

pub fn minimum() -> &'static str {
    include_str!("../../../gate.toml")
        .split_once("[claude]")
        .expect("Claude gate section")
        .1
        .lines()
        .map(str::trim)
        .find_map(|s| {
            s.strip_prefix("minimum_version = ")?
                .strip_prefix('"')?
                .strip_suffix('"')
        })
        .expect("Claude version floor")
}
fn version(s: &str) -> Option<([u32; 3], bool)> {
    let s = s.split('+').next()?;
    let (numbers, pre) = s.split_once('-').map_or((s, false), |(n, _)| (n, true));
    let mut p = numbers.split('.');
    let n = [
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    ];
    p.next().is_none().then_some((n, pre))
}

fn verify_initialization(reply: &ControlReply, billing: &Billing) -> io::Result<()> {
    let required = version(minimum()).expect("valid Claude version floor").0;
    let caps = reply
        .capabilities()
        .ok_or_else(|| io::Error::other("Claude Code did not initialize the stream controls"))?;
    let actual = caps.claude_code_version.as_deref().and_then(version);
    if !actual.is_some_and(|(n, pre)| n > required || n == required && !pre) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            match caps.claude_code_version.as_deref() {
                Some(found) => format!(
                    "Claude Code {found} found; OX Desktop needs {} or later. Update Claude Code, then press Reconnect.",
                    minimum()
                ),
                None => format!(
                    "OX Desktop needs Claude Code {} or later. Update Claude Code, then press Reconnect.",
                    minimum()
                ),
            },
        ));
    }
    if !matches_account(billing, &caps.account) {
        let key = caps
            .account
            .api_key_source
            .as_deref()
            .filter(|s| *s != "none")
            .or_else(|| {
                caps.account
                    .token_source
                    .as_deref()
                    .filter(|s| *s != "none")
                    .filter(|_| *billing == Billing::Subscription)
            });
        // A key in the environment is not fixed by signing in, so it is not a sign-in error.
        if let (Some(source), Billing::Subscription) = (key, billing) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Claude Code is using {source} from your environment instead of your Claude subscription. Unset it, or choose an API model route, then press Reconnect."
                ),
            ));
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            match billing {
                Billing::Subscription => {
                    "Claude Code is not signed in to a Claude subscription. Sign in, then press Reconnect."
                }
                Billing::Api(_) => {
                    "Claude Code is not using the saved OX API key for this model route. Press Reconnect, or choose another model route."
                }
            },
        ));
    }
    Ok(())
}
fn matches_account(billing: &Billing, account: &Account) -> bool {
    if account.api_provider.as_deref() != Some("firstParty")
        || account
            .api_key_source
            .as_deref()
            .is_some_and(|s| s != "none")
    {
        return false;
    }
    let source = account.token_source.as_deref();
    match billing {
        Billing::Subscription => {
            matches!(source, None | Some("none"))
                && account
                    .subscription_type
                    .as_deref()
                    .is_some_and(|s| !s.is_empty())
        }
        Billing::Api(_) => source == Some("ANTHROPIC_AUTH_TOKEN"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_worker_contract::Contract;

    fn args(billing: Billing, contract: Option<Contract>) -> Vec<String> {
        let launch = Launch {
            ox: "ox".into(),
            path: "/usr/bin".into(),
            project: "/".into(),
            billing,
            session: Session::New("id".into()),
            permission_mode: Some("acceptEdits".into()),
            contract,
            model: None,
        };
        launch
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The hosted launch carries exactly the policy arguments the standalone
    /// builder passes, for both billing routes; an ordinary session carries none.
    #[test]
    fn hosted_write_launch_matches_the_shared_contract() {
        for sandboxed in [true, false] {
            let contract = Contract::with_support(true, false, sandboxed);
            for billing in [Billing::Subscription, Billing::Api("alias".into())] {
                let hosted = args(billing.clone(), Some(contract));
                let policy = contract.args();
                assert!(
                    hosted.windows(policy.len()).any(|w| w == policy.as_slice()),
                    "{hosted:?}"
                );
                // A write worker has its shell and the sandbox settings either way;
                // Claude Code decides whether the sandbox runs.
                assert!(hosted.iter().any(|a| a == "--settings"));
                let tools = hosted.iter().position(|a| a == "--tools").unwrap();
                assert!(hosted[tools + 1].contains("Bash"));
                let plain = args(billing, None);
                assert!(!plain.iter().any(|a| a == "--tools" || a == "--settings"));
            }
        }
        // A pinned subscription worker runs its exact model from the first turn;
        // an API route never takes a second model.
        let pinned = |billing| {
            Launch {
                ox: "ox".into(),
                path: "/usr/bin".into(),
                project: "/".into(),
                billing,
                session: Session::New("id".into()),
                permission_mode: Some("plan".into()),
                contract: None,
                model: Some("claude-fable-5-2".into()),
            }
            .command()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };
        let sub = pinned(Billing::Subscription);
        let at = sub.iter().position(|a| a == "--model").unwrap();
        assert_eq!(sub[at + 1], "claude-fable-5-2");
        assert_eq!(sub[at - 1], "--subscription");
        let api = pinned(Billing::Api("alias".into()));
        assert_eq!(api.iter().filter(|a| *a == "--model").count(), 1);
        assert!(!api.iter().any(|a| a == "claude-fable-5-2"));
        let mode = args(Billing::Subscription, None);
        let at = mode.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(mode[at + 1], "acceptEdits");
    }
}
