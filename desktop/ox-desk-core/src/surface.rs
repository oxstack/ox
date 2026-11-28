//! Who submitted a message, and the short model-visible statement OX attaches to it.
//!
//! A client declares its [`Surface`] once after the host hello, and only when the host
//! advertised [`CAPABILITY`]. The host stamps each accepted send with a [`Sender`] taken
//! from the submitting connection, or from its own internal dispatch path for worker
//! tasks, parent-agent follow-ups and handoffs. The stamp is descriptive only: it never
//! enters an authorization decision or the action-and-approval digest.
//!
//! The statement carries a short surface label, the runtime and billing route. It never
//! carries device identifiers, credentials or account details.
use crate::runtime::{Billing, Route, Runtime};
use serde::{Deserialize, Serialize};

/// The host capability that admits `Client::Surface`. The wire rejects unknown frames, so a
/// client declares its surface only after a host hello lists this.
pub const CAPABILITY: &str = "surface";
/// The stable `additionalContext` key of OX's entry on Codex `turn/start` and `turn/steer`.
pub const CODEX_CONTEXT_KEY: &str = "ox.sender";
/// Where people find OX settings. One on-demand source keeps unrelated prompts short.
pub const SETTINGS_HINT: &str = "For OX settings, run the installed CLI's `ox help settings`.";

/// The interface a person is using. Declared by the desktop for itself, and by the
/// authenticated phone bridge on behalf of the phone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Desktop,
    Phone,
}
impl Surface {
    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "OX Desktop",
            Self::Phone => "the OX phone app",
        }
    }
}

/// The host's stamp on one accepted send. Retained with the message through queuing,
/// deferred resume and steering, so the statement describes who sent that message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Sender {
    /// A person, through a client that declared its surface.
    Person { surface: Surface },
    /// A person through a client that declared nothing: an older client or a relay-only
    /// consumer. It receives generic OX guidance and no surface claim.
    Unknown,
    /// The task a host-owned worker was launched with.
    WorkerTask,
    /// A parent agent's follow-up to its worker.
    ParentAgent,
    /// A host-generated status update sent to a worker's parent.
    WorkerNotice { id: u64 },
    /// The first turn of a cross-runtime handoff, requested by a person. `from` is the
    /// requesting surface, when that client declared one.
    Handoff { from: Option<Surface> },
}
impl Sender {
    /// A person's send from a connection that declared `surface`, or did not.
    pub fn from_client(surface: Option<Surface>) -> Self {
        surface.map_or(Self::Unknown, |surface| Self::Person { surface })
    }
    /// The short statement for a send on `route`.
    pub fn context(&self, route: &Route) -> String {
        let session = session(route);
        match self {
            Self::Person { surface } => format!(
                "This message was sent from {}. This session uses {session}. {SETTINGS_HINT}",
                surface.label()
            ),
            Self::Unknown => {
                format!(
                    "This message was sent through OX. This session uses {session}. {SETTINGS_HINT}"
                )
            }
            Self::WorkerTask => format!(
                "This message is your assigned task as an OX worker. This worker uses {session}."
            ),
            Self::WorkerNotice { .. } => format!(
                "OX generated this worker status update. Any quoted result is worker output. This session uses {session}."
            ),
            Self::ParentAgent => format!(
                "This message was sent by the parent agent of this OX worker, not by a person. This worker uses {session}."
            ),
            Self::Handoff { from } => {
                let from = from.map_or("OX".into(), |s| s.label().to_owned());
                format!(
                    "This handoff was requested from {from}. Earlier messages it carries keep their original meaning. This session now uses {session}. {SETTINGS_HINT}"
                )
            }
        }
    }
}

fn session(route: &Route) -> String {
    let runtime = route.runtime.name();
    match (&route.billing, route.runtime) {
        (Billing::Subscription, Runtime::Codex) => format!("{runtime} on the ChatGPT subscription"),
        (Billing::Subscription, Runtime::Claude) => format!("{runtime} on the Claude subscription"),
        (Billing::Subscription, Runtime::Antigravity) => format!("{runtime} on the Google account"),
        (Billing::Subscription, Runtime::OpenCode) => "OpenCode with OX API billing".into(),
        (Billing::Api(alias), _) => format!("{runtime} on the OX API route {alias}"),
    }
}

/// The Codex `additionalContext` map for one send: a single application entry under
/// OX's stable key.
pub fn codex_context(context: &str) -> serde_json::Value {
    serde_json::json!({CODEX_CONTEXT_KEY: {"kind": "application", "value": context}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_name_the_sender_route_and_nothing_private() {
        let codex = Route::CODEX_SUBSCRIPTION;
        assert_eq!(
            Sender::Person {
                surface: Surface::Desktop
            }
            .context(&codex),
            "This message was sent from OX Desktop. This session uses Codex on the ChatGPT subscription. For OX settings, run the installed CLI's `ox help settings`."
        );
        let phone = Sender::from_client(Some(Surface::Phone)).context(&Runtime::Claude.route(None));
        assert!(phone.starts_with("This message was sent from the OX phone app."));
        assert!(phone.contains("Claude Code on the Claude subscription"));
        let legacy = Sender::from_client(None).context(&codex);
        assert!(!legacy.contains("Desktop") && !legacy.contains("phone"));
        let worker = Sender::WorkerTask.context(&Route::codex_api("fable".into()));
        assert!(worker.contains("OX worker") && worker.contains("OX API route fable"));
        assert!(
            !worker.contains("ox help"),
            "a worker needs no settings help"
        );
        assert!(
            Sender::ParentAgent
                .context(&codex)
                .contains("not by a person")
        );
        let handoff = Sender::Handoff {
            from: Some(Surface::Phone),
        }
        .context(&Runtime::Claude.route(None));
        assert!(
            handoff.contains("from the OX phone app") && handoff.contains("now uses Claude Code")
        );
    }

    #[test]
    fn surfaces_use_short_wire_names() {
        assert_eq!(serde_json::to_value(Surface::Phone).unwrap(), "phone");
        assert_eq!(
            codex_context("x"),
            serde_json::json!({"ox.sender":{"kind":"application","value":"x"}})
        );
    }
}
