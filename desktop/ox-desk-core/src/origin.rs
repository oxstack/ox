//! Immutable lineage of a host-owned session. A worker, its forks and its own
//! children carry the stamp written when the host accepted the launch. The same
//! record can later describe other links, such as a cross-runtime handoff,
//! through `relation`, without a second lineage model.
use crate::{
    handoff::HandoffSource,
    host_protocol::Scope,
    modes::{Decision, ModeRequest, YoloAllowance},
    runtime::{Access, Route, Runtime},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The most a child may ever be granted, recorded once at launch. It is the
/// permission [`Level`] of the source, inherited by forks, continuations and
/// nested workers. Older stamps recorded only `read` and `write`.
pub use crate::modes::Level as Ceiling;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkerMode {
    Session,
    Goal,
    Workflow,
}

/// How this session relates to `parent`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Started by the parent agent or by the operator on the parent's behalf.
    Worker,
    /// A native fork of a session that already carried this origin.
    Fork,
    /// A new native session that continues `parent` in another runtime, billing
    /// route or model, started by the operator. `handoff` names the source.
    Handoff,
}

/// The session that started a worker, as verified by the host.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ParentRef {
    pub project: PathBuf,
    /// The host scope of a hosted parent. Absent for a terminal session the host does not run.
    pub scope: Option<Scope>,
    /// The parent's session ID in that scope, or its native ID for a terminal session.
    pub session: Option<String>,
    /// Stock runtime of the parent, when known.
    pub runtime: Option<Runtime>,
}

/// A parent on another host. Its paths and session identifiers are meaningful only there.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LinkedParent {
    pub host: String,
    pub device: String,
    pub request_id: String,
    pub parent: ParentRef,
    pub verified_level: Ceiling,
    pub link_ceiling: Ceiling,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Origin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked: Option<Box<LinkedParent>>,
    pub relation: Relation,
    pub parent: ParentRef,
    /// Non-secret identity of the launching OX helper or operator device.
    pub launch: String,
    pub by_operator: bool,
    pub runtime: Runtime,
    pub route: Route,
    pub mode: WorkerMode,
    /// The parent-facing job number. Forks keep the original worker's number.
    pub job_id: u64,
    /// Access requested at launch. Never above `ceiling`.
    pub requested: Ceiling,
    pub ceiling: Ceiling,
    /// For a fork, the session it was forked from.
    pub source: Option<String>,
    /// The native preset the child started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<Access>,
    /// How that starting mode was decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<Decision>,
    /// For a YOLO or bypass child, the acceptance or grant that allowed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<YoloAllowance>,
    /// For a handoff, the source session and references to what it carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Box<HandoffSource>>,
    /// The subscription model the launcher asked for, exactly as asked
    /// (`claude-fable-5-2` or a picker value such as `fable`). The billing route
    /// is `route`; an API alias is recorded there, never here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// The exact model the runtime was pinned to after the account's picker
    /// offered it, before the first inference. Continuations keep it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_model: Option<String>,
}

impl Origin {
    /// The shared worker contract a hosted Claude Write worker starts under, with
    /// the same Bash permissions, sandbox settings and Workflow rule as a standalone worker.
    pub fn claude_contract(
        &self,
        mode: WorkerMode,
    ) -> Option<crate::claude_worker_contract::Contract> {
        (self.runtime == Runtime::Claude
            && matches!(self.relation, Relation::Worker | Relation::Fork)
            && self.handoff.is_none()
            && self.requested == Ceiling::Write)
            .then(|| {
                crate::claude_worker_contract::Contract::detect(true, mode == WorkerMode::Workflow)
            })
    }
    /// The stamp a fork of this session carries. It can only keep or narrow the ceiling.
    pub fn fork(&self, source: &str) -> Self {
        Self {
            relation: Relation::Fork,
            source: Some(source.to_owned()),
            ..self.clone()
        }
    }
    /// The most this session's children can receive because of its own stamp.
    /// Workers and their forks pass on their ceiling. A handoff target belongs
    /// to the operator, so only its live mode bounds its children.
    pub fn limits_children(&self) -> Option<Ceiling> {
        (self.relation != Relation::Handoff && self.handoff.is_none()).then_some(self.ceiling)
    }
    /// Whether the operator's mode changes on this session are bounded by the stamp.
    pub fn bounds_operator(&self) -> bool {
        self.limits_children().is_some()
    }
}

/// Minimum installed hardware. Available memory and load rank eligible machines.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct WorkerNeeds {
    pub memory_gb: Option<u32>,
    pub cpus: Option<u32>,
    pub gpu: bool,
    pub os: Option<String>,
}

impl WorkerNeeds {
    pub fn validate(&self) -> Result<(), String> {
        if self.memory_gb == Some(0) || self.cpus == Some(0) {
            return Err("Memory and CPU requirements must be positive.".into());
        }
        if self
            .os
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 128)
        {
            return Err("Provide an operating system such as linux or macos.".into());
        }
        Ok(())
    }
}

/// An operator's request to start a worker attached to a session in the subscribed scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkerStart {
    /// A paired worker host name or stable ID. Omit to run on the parent's host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<WorkerNeeds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
    pub parent: String,
    pub runtime: Runtime,
    pub model: Option<String>,
    /// Execution directory, independent of the parent session identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<PathBuf>,
    pub mode: WorkerMode,
    pub task: String,
    /// Legacy access request: false is read, true is write. Absent means inherit.
    #[serde(default)]
    pub write: Option<bool>,
    /// The requested permission mode. Wins over `write`. Absent means inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<ModeRequest>,
    /// An operator start whose client showed the YOLO confirmation.
    #[serde(default)]
    pub yolo_confirmed: bool,
    /// Wall-clock limit in minutes. Absent means no limit unless the host sets `OX_WORKER_MAX_MINUTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_minutes: Option<u32>,
    /// An exact Claude subscription model to pin before the first inference,
    /// such as `claude-fable-5-2`. Needs a Claude worker without an API alias.
    /// The host refuses the launch when the account's picker does not offer it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_model: Option<String>,
}

impl WorkerStart {
    /// The mode the launcher asked for.
    pub fn mode_request(&self) -> ModeRequest {
        self.permission.unwrap_or(ModeRequest::legacy(self.write))
    }
    /// The composer form: `/worker [@host] <codex|claude>[:<api model>] [--write]
    /// [--mode <mode>] [--goal|--workflow] <task>`. A Claude worker without a model uses the
    /// Claude subscription. Returns None when the text is not a worker command.
    pub fn parse_command(parent: &str, text: &str) -> Option<Result<Self, String>> {
        let rest = text.trim_start().strip_prefix("/worker")?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let usage = "Use /worker [@host] codex[:<model>] [--mode <mode>] [--goal] <task> or /worker [@host] claude[:<model>] [--mode <mode>] [--workflow] <task>.";
        let rest = rest.trim_start();
        let (host, rest) = match rest.strip_prefix('@') {
            Some(named) => {
                let Some((host, rest)) = named.split_once(char::is_whitespace) else {
                    return Some(Err(usage.into()));
                };
                (Some(host.to_owned()), rest.trim_start())
            }
            None => (None, rest),
        };
        let mut words = rest.splitn(2, char::is_whitespace);
        let target = words.next().filter(|w| !w.is_empty());
        let Some(target) = target else {
            return Some(Err(usage.into()));
        };
        let (runtime, model) = match target.split_once(':') {
            Some((r, m)) => (r, Some(m.to_owned()).filter(|m| !m.is_empty())),
            None => (target, None),
        };
        let runtime = match runtime {
            "codex" => Runtime::Codex,
            "claude" => Runtime::Claude,
            "antigravity" => Runtime::Antigravity,
            _ => return Some(Err(usage.into())),
        };
        let mut task = words.next().unwrap_or("").trim_start();
        let (mut write, mut mode, mut permission) = (None, WorkerMode::Session, None);
        loop {
            let (flag, after) = task.split_once(char::is_whitespace).unwrap_or((task, ""));
            match flag {
                "--write" => write = Some(true),
                "--mode" => {
                    let after = after.trim_start();
                    let (name, rest) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
                    match ModeRequest::parse(name).filter(|_| !name.is_empty()) {
                        Some(m) => permission = Some(m),
                        None => return Some(Err(format!("Unknown mode {name:?}. {usage}"))),
                    }
                    task = rest.trim_start();
                    continue;
                }
                "--goal" => mode = WorkerMode::Goal,
                "--workflow" => mode = WorkerMode::Workflow,
                _ => break,
            }
            task = after.trim_start();
        }
        let start = Self {
            host,
            needs: None,
            placement: None,
            parent: parent.to_owned(),
            runtime,
            model,
            mode,
            task: task.to_owned(),
            write,
            permission,
            checkout: None,
            yolo_confirmed: false,
            max_minutes: None,
            subscription_model: None,
        };
        Some(start.validate().map(|()| start))
    }
    /// The checks the standalone OX gateway applies, shared by every launcher.
    pub fn validate(&self) -> Result<(), String> {
        if self.runtime == Runtime::OpenCode {
            return Err("Hosted OpenCode does not accept worker launches.".into());
        }
        if !self.runtime.supports_workers() {
            return Err(
                "Antigravity sessions start from the composer or an operator handoff.".into(),
            );
        }
        if self
            .host
            .as_ref()
            .is_some_and(|h| h.trim().is_empty() || h.len() > 256)
        {
            return Err("Provide a worker host name or ID.".into());
        }
        if let Some(needs) = &self.needs {
            needs.validate()?;
        }
        if self
            .placement
            .as_deref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 128 || s == "os")
        {
            return Err("Provide a named project placement rule.".into());
        }
        if self.max_minutes.is_some_and(|m| m == 0 || m > 7 * 24 * 60) {
            return Err("max_minutes must be between 1 and 10080.".into());
        }
        if self.task.trim().is_empty() || self.task.len() > 32 * 1024 {
            return Err("Provide a task of up to 32 KiB.".into());
        }
        if self.mode == WorkerMode::Goal
            && (self.runtime != Runtime::Codex || self.task.chars().count() > 4000)
        {
            return Err(
                "Goal mode requires Codex and an objective of at most 4,000 characters.".into(),
            );
        }
        if self.mode == WorkerMode::Workflow && self.runtime != Runtime::Claude {
            return Err("Workflow mode requires Claude Code.".into());
        }
        if self.model.as_ref().is_some_and(|model| {
            model.trim().is_empty() || model.len() > 256 || model.starts_with('-')
        }) {
            return Err("Provide an ox model alias of up to 256 bytes.".into());
        }
        if let Some(pinned) = &self.subscription_model {
            if self.runtime != Runtime::Claude {
                return Err("subscription_model pins a Claude subscription model. Leave it out to use the ChatGPT subscription with Codex.".into());
            }
            if self.model.is_some() {
                return Err("Choose either an ox API model alias (billed by its provider) or a subscription_model (billed to the Claude subscription), not both.".into());
            }
            if !crate::claude_models::valid_model(pinned) {
                return Err(
                    "Provide a Claude subscription model ID such as claude-fable-5-2.".into(),
                );
            }
        }
        Ok(())
    }
    pub fn route(&self) -> Route {
        self.runtime.route(self.model.clone())
    }
    /// The first message sent to the worker session.
    pub fn prompt(&self) -> String {
        let task = self.task.trim();
        match self.mode {
            WorkerMode::Workflow => format!(
                "Run this task as a Claude Code workflow using your Workflow tool. If the tool is unavailable, explain that and stop. Objective:\n{task}"
            ),
            WorkerMode::Goal => format!("/goal {task}"),
            WorkerMode::Session => task.to_owned(),
        }
    }
}

/// The worker view's model line from the host's `worker_model` report: who pays, then
/// the model that answered, or the requested model while the runtime has not answered.
/// "Claude subscription · claude-fable-5-2" or "Claude subscription · fable (pending)".
pub fn model_line(report: &serde_json::Value) -> Option<String> {
    let billing = report["billing_label"].as_str()?;
    let shown = |key: &str| report[key].as_str().filter(|s| !s.is_empty());
    let model = match report["model_state"].as_str() {
        Some("confirmed" | "reported") => shown("actual_model").map(str::to_owned),
        Some("differs") => shown("actual_model").map(|a| {
            let pinned = shown("pinned_model").unwrap_or("the pinned model");
            format!("{a}, not {pinned}")
        }),
        // An API alias already names itself in the billing label.
        _ => shown("pinned_model")
            .or_else(|| shown("requested_model").filter(|_| report["billing"] == "subscription"))
            .map(|m| format!("{m} (pending)")),
    };
    Some(match model {
        Some(model) => format!("{billing} · {model}"),
        None => billing.to_owned(),
    })
}

/// The only operations a parent agent has on its workers. Permission changes
/// belong to the human operator and have no variant here. A parent answers a
/// worker's approval or question only where the host's policy lets it stand in
/// for the operator.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParentCall {
    ListHosts,
    List,
    Status {
        job_id: u64,
    },
    Steer {
        job_id: u64,
        text: String,
    },
    Cancel {
        job_id: u64,
    },
    /// Answer one open request of the worker: `choice` picks one of its listed
    /// choices, `answers` fills its questions in order.
    Answer {
        job_id: u64,
        request: crate::runtime::RequestId,
        #[serde(default)]
        choice: Option<usize>,
        #[serde(default)]
        answers: Option<Vec<String>>,
    },
    /// Read this session's own handoff sources, read-only. `session` names a
    /// source in the caller's chain; absent means the nearest one. Pages by item
    /// ID: `after` starts after that item, `item` returns one item in full.
    ReadThread {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        item: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Sent by a terminal gateway, never by the agent: the gateway observed its
    /// own native session. The host records it once on the launch's workers so
    /// a later resume of that session can reach them.
    Bind {
        session: String,
    },
}

/// A worker gateway the host does not run, such as `ox claude` in a terminal.
/// The host can verify only the OS user. `launch` is a random token private to
/// that gateway; the host keeps only its digest. `session` is the native session
/// the gateway has observed, from its resume arguments or the stock client's own
/// MCP request metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ExternalCaller {
    pub project: PathBuf,
    pub runtime: Runtime,
    pub session: Option<String>,
    pub launch: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_composer_worker_command_is_validated_before_it_reaches_the_host() {
        let start =
            WorkerStart::parse_command("p", "/worker codex:fixture/demo --write fix the tests")
                .unwrap()
                .unwrap();
        assert_eq!(
            start.route(),
            Runtime::Codex.route(Some("fixture/demo".into()))
        );
        assert_eq!(start.write, Some(true));
        assert_eq!(start.mode_request(), ModeRequest::Level(Ceiling::Write));
        assert_eq!(start.task, "fix the tests");
        let lowered = WorkerStart::parse_command("p", "/worker codex:a --mode read-only --goal go")
            .unwrap()
            .unwrap();
        assert_eq!(lowered.mode_request(), ModeRequest::Level(Ceiling::Read));
        assert_eq!(lowered.mode, WorkerMode::Goal);
        assert!(
            WorkerStart::parse_command("p", "/worker codex:a --mode root go")
                .unwrap()
                .is_err()
        );
        assert_eq!(start.parent, "p");
        let claude = WorkerStart::parse_command("p", "/worker claude --workflow review")
            .unwrap()
            .unwrap();
        assert_eq!(claude.route(), Runtime::Claude.route(None));
        assert_eq!(claude.mode, WorkerMode::Workflow);
        assert_eq!(claude.mode_request(), ModeRequest::Inherit);
        let native = WorkerStart::parse_command("p", "/worker codex task")
            .unwrap()
            .unwrap();
        assert_eq!(native.route(), Route::CODEX_SUBSCRIPTION);
        assert!(
            WorkerStart::parse_command("p", "/worker claude --goal x")
                .unwrap()
                .is_err()
        );
        let named =
            WorkerStart::parse_command("p", "/worker @build claude --mode read inspect my changes")
                .unwrap()
                .unwrap();
        assert_eq!(named.host.as_deref(), Some("build"));
        assert_eq!(named.task, "inspect my changes");
        assert!(
            WorkerStart::parse_command("p", "/worker @ claude review")
                .unwrap()
                .is_err()
        );
        assert!(WorkerStart::parse_command("p", "/workers x").is_none());
        assert!(WorkerStart::parse_command("p", "hello").is_none());
    }

    #[test]
    fn a_subscription_model_needs_a_claude_subscription_worker() {
        let start = |v: serde_json::Value| serde_json::from_value::<WorkerStart>(v).unwrap();
        let pinned = start(serde_json::json!({
            "parent":"p","runtime":"claude","model":null,"mode":"session","task":"review",
            "subscription_model":"claude-fable-5-2"
        }));
        assert_eq!(pinned.validate(), Ok(()));
        assert_eq!(pinned.route(), Runtime::Claude.route(None));
        // Who pays is never ambiguous: an API alias and a subscription model conflict.
        let both = start(serde_json::json!({
            "parent":"p","runtime":"claude","model":"alias","mode":"session","task":"t",
            "subscription_model":"claude-fable-5-2"
        }));
        assert!(both.validate().unwrap_err().contains("not both"));
        let codex = start(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t",
            "subscription_model":"claude-fable-5-2"
        }));
        assert!(codex.validate().is_err());
        let flag = start(serde_json::json!({
            "parent":"p","runtime":"claude","model":null,"mode":"session","task":"t",
            "subscription_model":"--dangerously-skip-permissions"
        }));
        assert!(flag.validate().is_err());
        // An unpinned start serializes exactly as before, so an older host accepts it.
        let plain = WorkerStart::parse_command("p", "/worker claude review")
            .unwrap()
            .unwrap();
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("subscription_model")
                .is_none()
        );
    }

    #[test]
    fn the_worker_view_names_who_pays_and_which_model_answered() {
        use serde_json::json;
        let report = |state: &str, actual: Option<&str>| {
            json!({"billing":"subscription","billing_label":"Claude subscription","requested_model":"fable",
                "pinned_model":"claude-fable-5-2","actual_model":actual,"model_state":state})
        };
        assert_eq!(
            model_line(&report("pending", None)).as_deref(),
            Some("Claude subscription · claude-fable-5-2 (pending)")
        );
        assert_eq!(
            model_line(&report("confirmed", Some("claude-fable-5-2"))).as_deref(),
            Some("Claude subscription · claude-fable-5-2")
        );
        assert_eq!(
            model_line(&report("differs", Some("claude-opus-5-5"))).as_deref(),
            Some("Claude subscription · claude-opus-5-5, not claude-fable-5-2")
        );
        let api = json!({"billing":"api","billing_label":"OX API · fixture","requested_model":"fixture","model_state":"pending"});
        assert_eq!(model_line(&api).as_deref(), Some("OX API · fixture"));
        let unpinned = json!({"billing":"subscription","billing_label":"Claude subscription","model_state":"reported","actual_model":"claude-opus-5-5"});
        assert_eq!(
            model_line(&unpinned).as_deref(),
            Some("Claude subscription · claude-opus-5-5")
        );
        assert_eq!(model_line(&json!({})), None);
    }

    #[test]
    fn old_worker_starts_and_stamps_still_decode() {
        let old: WorkerStart = serde_json::from_value(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t","write":false
        }))
        .unwrap();
        assert_eq!(old.mode_request(), ModeRequest::Level(Ceiling::Read));
        let new: WorkerStart = serde_json::from_value(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t","permission":"write"
        }))
        .unwrap();
        assert_eq!(new.mode_request(), ModeRequest::Level(Ceiling::Write));
        let omitted: WorkerStart = serde_json::from_value(serde_json::json!({
            "parent":"p","runtime":"codex","model":"m","mode":"session","task":"t"
        }))
        .unwrap();
        assert_eq!(omitted.mode_request(), ModeRequest::Inherit);
        let stamp: Origin = serde_json::from_value(serde_json::json!({
            "relation":"worker","parent":{"project":"/p","scope":null,"session":null,"runtime":"codex"},
            "launch":"l","by_operator":false,"runtime":"codex","route":{"runtime":"codex","billing":{"api":"m"}},
            "mode":"session","job_id":3,"requested":"read","ceiling":"write","source":null
        }))
        .unwrap();
        assert_eq!(stamp.ceiling, Ceiling::Write);
        assert_eq!(stamp.limits_children(), Some(Ceiling::Write));
        assert!(stamp.started.is_none() && stamp.handoff.is_none());
        assert!(stamp.requested_model.is_none() && stamp.pinned_model.is_none());
    }
}

#[cfg(test)]
mod checkout_tests {
    use crate::worker_checkout::resolve;
    use std::path::Path;
    #[test]
    fn canonical_checkout_respects_workspace_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::os::unix::fs::symlink(temp.path(), root.join("outside")).unwrap();
        assert_eq!(
            resolve(&root, Some(Path::new("child")), false).unwrap(),
            root.join("child").canonicalize().unwrap()
        );
        assert!(resolve(&root, Some(Path::new("outside")), false).is_err());
        assert!(resolve(&root, Some(Path::new("../missing")), true).is_err());
        assert_eq!(
            resolve(&root, Some(Path::new("outside")), true).unwrap(),
            temp.path().canonicalize().unwrap()
        );
    }
}
