//! Reviewed actions and screen updates exchanged on the operator WebSocket.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Serialize)]
#[serde(tag = "preset", rename_all = "kebab-case")]
pub enum OpenTerminal {
    Shell {
        project: PathBuf,
    },
    NativeSession {
        scope: ox_desk_core::host_protocol::Scope,
        thread: String,
    },
    CodexLogin,
    ClaudeLogin,
    AntigravityLogin,
}

impl OpenTerminal {
    pub fn login(runtime: ox_desk_core::runtime::Runtime) -> Option<Self> {
        use ox_desk_core::runtime::Runtime;
        match runtime {
            Runtime::Codex => Some(Self::CodexLogin),
            Runtime::Claude => Some(Self::ClaudeLogin),
            Runtime::Antigravity => Some(Self::AntigravityLogin),
            Runtime::OpenCode => None,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Input {
    Run,
    Keys { text: String },
    Resize { rows: u16, cols: u16 },
    Cancel,
}
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Event {
    Review {
        review: String,
        cwd: PathBuf,
    },
    Running {
        running: bool,
    },
    Screen {
        screen: String,
        echo: Option<bool>,
    },
    Error {
        error: String,
    },
    Exit {
        #[serde(deserialize_with = "exit_code")]
        exit: Option<i32>,
    },
    Unknown(serde::de::IgnoredAny),
}

fn exit_code<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
    Option::deserialize(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_events_accept_new_metadata_without_treating_unknown_events_as_exit() {
        assert!(matches!(
            serde_json::from_str::<Event>(r#"{"screen":"ok","echo":true,"revision":1}"#).unwrap(),
            Event::Screen { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<Event>(r#"{"heartbeat":42}"#).unwrap(),
            Event::Unknown(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Event>(r#"{"exit":null}"#).unwrap(),
            Event::Exit { exit: None }
        ));
    }
}
