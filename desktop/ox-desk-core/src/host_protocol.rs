//! Shared device protocol. Only this versioned projection crosses the host boundary.
use crate::{
    actions::Action,
    asks::Question,
    runtime::{RequestId, Route},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

pub const IDLE_UPDATE: &str = "idle_update";
pub const VERSION: u32 = 3;
pub const MIN_VERSION: u32 = 1;
const ANTIGRAVITY_VERSION: u32 = 2;

/// The hello advertises a maximum. Clients check the selected version against
/// their own floor before sending any actions. Features are capability gated.
pub fn negotiate(client_max: u32) -> Option<u32> {
    let selected = client_max.min(VERSION);
    (selected >= MIN_VERSION).then_some(selected)
}
pub fn supported(selected: u32) -> bool {
    (MIN_VERSION..=VERSION).contains(&selected)
}

pub fn supports_runtime(version: u32, runtime: crate::runtime::Runtime) -> bool {
    use crate::runtime::Runtime;
    match runtime {
        Runtime::Antigravity => version >= ANTIGRAVITY_VERSION,
        Runtime::OpenCode => version >= 3,
        Runtime::Codex | Runtime::Claude => true,
    }
}

pub fn supports_route(version: u32, route: &Route) -> bool {
    supports_runtime(version, route.runtime)
}

fn handoff_runtimes_supported(version: u32, packet: &crate::handoff::HandoffTransfer) -> bool {
    supports_runtime(version, packet.start.runtime)
        && supports_runtime(version, packet.source.runtime)
        && packet
            .source
            .selection
            .earlier
            .iter()
            .all(|earlier| supports_runtime(version, earlier.runtime))
}

/// Keep the plan text readable on hosts that only recognize the original transfer item kinds.
pub fn project_handoff_transfer(
    version: u32,
    packet: &mut crate::handoff::HandoffTransfer,
) -> bool {
    if !handoff_runtimes_supported(version, packet) {
        return false;
    }
    if version < 2 {
        use crate::handoff::Kind;
        for item in &mut packet.carried {
            if item.kind == Kind::Plan {
                item.kind = Kind::Outcome;
                item.text.insert_str(0, "[Plan from the source session]\n");
            }
        }
    }
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Scope {
    pub project: PathBuf,
    pub route: Route,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Client {
    Hello {
        version: u32,
    },
    /// The interface this connection's person uses. Send it once, after a host hello that
    /// lists `surface::CAPABILITY`; an older host rejects the frame. The phone bridge
    /// declares the phone itself and never forwards a phone's own declaration. A
    /// connection that declares nothing is stamped as an unknown OX client.
    Surface {
        surface: crate::surface::Surface,
    },
    /// Reserve a quiescent host until this same-user connection closes.
    ReserveIdleUpdate,
    WatchCatalog,
    /// List the execution hosts linked to this host for a project. Answers WorkerReply.
    WorkerHosts {
        id: String,
        project: PathBuf,
    },
    WatchHandoffReads {
        proof: String,
    },
    ReadHandoffPage {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        broker: Option<String>,
        source_host: String,
        target_host: String,
        scope: Scope,
        transfer: String,
        page: HandoffPage,
    },
    FinishHandoffRead {
        token: String,
        result: Result<Value, String>,
    },
    InspectHandoffTarget {
        id: String,
        scope: Scope,
    },
    PrepareRemoteHandoff {
        id: String,
        scope: Scope,
        start: crate::handoff::HandoffStart,
        target_host: String,
        window: Option<u64>,
    },
    ReceiveRemoteHandoff {
        id: String,
        scope: Scope,
        packet: Box<crate::handoff::HandoffTransfer>,
    },
    /// Opt in to host-wide samples, including when no projects are open.
    WatchHost,
    TerminalPreview {
        id: String,
        scope: Scope,
        thread: String,
    },
    Discover {
        id: String,
        /// Request the project directory without scanning session history. Requires project_directory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        projects: Option<ProjectQuery>,
    },
    Subscribe {
        scope: Option<Scope>,
        after: Option<Cursor>,
        /// The client is a desktop window that can take a terminal handoff held for its
        /// scope. Phones and other gateways never set it, and the web bridge refuses it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        terminal: bool,
    },
    Act {
        id: String,
        scope: Scope,
        action: Action,
        approval: Option<String>,
    },
    /// A same-user OX gateway that the host does not run asks it to own a worker.
    WorkerLaunch {
        id: String,
        caller: crate::origin::ExternalCaller,
        start: crate::origin::WorkerStart,
    },
    /// Ask for the live processes the host runs. Answered by `Server::Processes`.
    /// Only clients that send this receive that frame, so older clients are unaffected.
    ListProcesses {
        id: String,
    },
    /// The parent-agent tools of such a gateway.
    WorkerCall {
        id: String,
        caller: crate::origin::ExternalCaller,
        call: crate::origin::ParentCall,
    },
}
impl Client {
    pub fn supported_by(&self, version: u32) -> bool {
        let scope = match self {
            Self::Act { scope, action, .. } => {
                if let Action::StartHandoff(start) = action
                    && !supports_runtime(version, start.runtime)
                {
                    return false;
                }
                Some(scope)
            }
            Self::Subscribe { scope, .. } => scope.as_ref(),
            Self::PrepareRemoteHandoff { scope, start, .. } => {
                if !supports_runtime(version, start.runtime) {
                    return false;
                }
                Some(scope)
            }
            Self::ReceiveRemoteHandoff { scope, packet, .. } => {
                if !handoff_runtimes_supported(version, packet)
                    || (version < 2
                        && packet
                            .carried
                            .iter()
                            .any(|item| item.kind == crate::handoff::Kind::Plan))
                {
                    return false;
                }
                Some(scope)
            }
            Self::ReadHandoffPage { scope, .. }
            | Self::InspectHandoffTarget { scope, .. }
            | Self::TerminalPreview { scope, .. } => Some(scope),
            _ => None,
        };
        scope.is_none_or(|scope| supports_route(version, &scope.route))
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostResources {
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub gpus: Vec<String>,
    pub cpus: Option<usize>,
    /// Memory sizes in bytes.
    pub total_memory: u64,
    pub available_memory: u64,
    pub load_one_minute: f64,
    pub running_workers: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HostInfo {
    #[serde(
        default,
        deserialize_with = "crate::usage::reported::deserialize_windows",
        skip_serializing_if = "std::collections::BTreeMap::is_empty"
    )]
    #[cfg_attr(
        feature = "schema",
        schemars(
            with = "std::collections::BTreeMap<String, Vec<crate::usage::reported::ReportedWindow>>"
        )
    )]
    pub subscription_limits:
        std::collections::BTreeMap<String, Vec<crate::usage::reported::ReportedWindow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_policy: Option<crate::host_policy::HostPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<HostResources>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Stable across host restarts. Legacy hosts supply only version and epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub version: String,
    pub epoch: String,
}

/// Which side to update when a client and host speak different protocol versions.
pub fn version_mismatch(host: u32, client: u32) -> String {
    if host < client {
        format!(
            "The running OX session host speaks protocol {host}, older than this client's {client}. Run `ox host restart` to start the installed host, or rerun the installer with --desktop."
        )
    } else {
        format!(
            "This client speaks OX protocol {client}, older than the session host's {host}. Update OX Desktop by rerunning the installer with --desktop; on a phone, reload the OX page."
        )
    }
}

/// A position in the conversation selected by the operator for a handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HandoffPage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<HistoryRef>,
    pub item: Option<String>,
    pub after: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HistoryRef {
    pub host: String,
    pub session: String,
}

/// A checkout query revalidates a specific known path after a user selects it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProjectQuery {
    List,
    Checkout(PathBuf),
}

/// Operator-visible checkouts. The path belongs to the reporting host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectCheckout {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectDirectory {
    pub checkouts: Vec<ProjectCheckout>,
    pub routes: Vec<Route>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Server {
    RelayHandoffRead {
        token: String,
        transfer: String,
        target_host: String,
        page: HandoffPage,
    },
    HostStatus {
        host: HostInfo,
    },
    IdleUpdate {
        ready: bool,
    },
    Opened {
        id: String,
        scope: Scope,
        thread: String,
    },
    Hello {
        version: u32,
        capabilities: Vec<String>,
        routes: Vec<Route>,
        projects: Vec<PathBuf>,
        /// The host's build version and boot epoch. Older hosts omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<HostInfo>,
    },
    Discovered {
        id: String,
        scope: Scope,
        rows: Vec<SessionView>,
    },
    DiscoveredEnd {
        id: String,
        errors: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<ProjectDirectory>,
    },
    Snapshot {
        cursor: Cursor,
        scopes: Vec<ScopeState>,
    },
    Delta {
        cursor: Cursor,
        scope: Scope,
        changes: Vec<Change>,
    },
    TerminalPreview {
        id: String,
        scope: Scope,
        thread: String,
        argv: Vec<String>,
    },
    Handoff {
        #[serde(default)]
        argv: Vec<String>,
        id: String,
        scope: Scope,
        thread: Option<String>,
        command: String,
    },
    Ack {
        id: String,
    },
    WorkerReply {
        id: String,
        result: Option<Value>,
        error: Option<String>,
    },
    /// The live processes of this host, newest first by start. Answers `Client::ListProcesses`.
    Processes {
        id: String,
        processes: Vec<ProcessView>,
    },
    Error {
        id: Option<String>,
        message: String,
    },
}
/// One live process the host runs for a project and billing route.
///
/// A Claude process serves one thread. A Codex app-server serves a scope and lists
/// every thread subscribed on it. To end a process, send `Action::EndSession(thread)`
/// for each id in `threads` (scoped to `scope`). The host refuses `EndSession` on a busy
/// thread; `Action::ForceEndSession(thread)` ends it anyway, after the user confirmed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProcessView {
    /// `antigravity`, `claude`, `codex` or `worker` (every thread of the last kind is host-owned).
    pub kind: String,
    pub scope: Scope,
    pub project: PathBuf,
    pub threads: Vec<String>,
    /// The scope's billing route label, such as the model alias.
    pub route: String,
    pub model: Option<String>,
    /// Unix seconds when the host started or first saw the process.
    pub started_at: i64,
    /// Unix seconds of the last event or action on any of its threads.
    pub last_activity: i64,
    /// A turn is in progress or an approval is pending, so ending would lose work.
    pub running: bool,
    /// The blank process kept warm for new sessions (the sign-in and version probe), not a
    /// conversation. `kind` stays `claude`. End all idle skips it, since it respawns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ScopeState {
    pub scope: Scope,
    pub connection: String,
    pub capabilities: Vec<String>,
    pub sessions: Vec<SessionView>,
    pub asks: Vec<AskView>,
    pub transcripts: BTreeMap<String, Timeline>,
    pub detail: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionView {
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    pub status: String,
    pub updated_at: i64,
    pub capabilities: Vec<String>,
    pub detail: Value,
}
impl SessionView {
    pub fn last_result(&self) -> Option<SessionResult> {
        serde_json::from_value(self.detail["last_result"].clone()).ok()
    }
}

/// The last terminal turn observed live by the session host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResult {
    pub turn: String,
    pub completed_at: i64,
    pub outcome: SessionOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcome {
    Completed,
    Failed,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AskView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<crate::asks::CommandApproval>,
    pub id: RequestId,
    pub token: String,
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub item: Option<String>,
    pub choice_details: Vec<Option<String>>,
    pub title: String,
    pub detail: String,
    pub choices: Vec<String>,
    pub questions: Vec<Question>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Timeline {
    pub items: Vec<TimelineItem>,
    pub detail: Value,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TimelineItem {
    pub turn: String,
    pub id: String,
    pub role: String,
    pub text: String,
    pub revision: u64,
    pub detail: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    Notification {
        thread: String,
        title: String,
        event: NotificationKind,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        goal_reached: bool,
    },
    Session {
        session: SessionView,
    },
    Sessions {
        sessions: Vec<SessionView>,
    },
    Asks {
        asks: Vec<AskView>,
    },
    Transcript {
        thread: String,
        timeline: Timeline,
    },
    Items {
        thread: String,
        items: Vec<TimelineItem>,
    },
    Append {
        thread: String,
        turn: String,
        id: String,
        text: String,
        revision: u64,
    },
    TimelineState {
        thread: String,
        detail: Value,
    },
    State {
        connection: String,
        capabilities: Vec<String>,
        detail: Value,
    },
    Retired {
        thread: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Finished,
    Failed,
    Approval,
    Question,
}

#[cfg(test)]
#[test]
fn goal_reached_is_additive_to_the_v1_finished_notification() {
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum LegacyChange {
        Notification {
            thread: String,
            title: String,
            event: NotificationKind,
        },
    }
    let legacy = serde_json::json!({"kind":"notification","thread":"t","title":"Example","event":"finished"});
    let parsed: Change = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(
        parsed,
        Change::Notification {
            goal_reached: false,
            ..
        }
    ));
    assert_eq!(serde_json::to_value(parsed).unwrap(), legacy);
    let next = serde_json::to_value(Change::Notification {
        thread: "t".into(),
        title: "Example".into(),
        event: NotificationKind::Finished,
        goal_reached: true,
    })
    .unwrap();
    assert_eq!(next["goal_reached"], true);
    let LegacyChange::Notification {
        thread,
        title,
        event,
    } = serde_json::from_value(next).unwrap();
    assert_eq!(
        (thread.as_str(), title.as_str(), event),
        ("t", "Example", NotificationKind::Finished)
    );
}

use std::io::{self, BufRead as _};
const MAX_FRAME: u64 = 64 * 1024 * 1024;

pub fn write_frame(out: &mut impl std::io::Write, request: &Client) -> io::Result<()> {
    serde_json::to_writer(&mut *out, request)?;
    out.write_all(b"\n")?;
    out.flush()
}
pub fn read_frame(input: &mut impl std::io::BufRead) -> io::Result<Server> {
    let mut bytes = Vec::new();
    let n = std::io::Read::take(input, MAX_FRAME).read_until(b'\n', &mut bytes)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Session host disconnected",
        ));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Incomplete or oversized host frame",
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod project_discovery_tests {
    use super::*;
    #[test]
    fn ordinary_discovery_preserves_the_v1_wire_format() {
        let frame = serde_json::json!({"type":"discover","id":"sessions"});
        let client: Client = serde_json::from_value(frame.clone()).unwrap();
        assert!(matches!(client, Client::Discover { projects: None, .. }));
        assert_eq!(serde_json::to_value(client).unwrap(), frame);
        let end = serde_json::json!({"type":"discovered_end","id":"sessions","errors":[]});
        let reply: Server = serde_json::from_value(end.clone()).unwrap();
        assert!(matches!(
            reply,
            Server::DiscoveredEnd {
                directory: None,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(reply).unwrap(), end);
    }
}
