use super::{Live, Phase, Record, Snapshot, Turn};
use crate::opencode::{HistoryEntry, HistoryPage, PermissionRequest};
use ox_desk_core::{
    asks::CommandApproval,
    host_protocol::{AskView, SessionResult, SessionView, Timeline, TimelineItem},
    runtime::RequestId,
    transcript::item::{Content, Item},
};
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

fn latest_assistant<'a>(page: &'a HistoryPage, user: &str) -> Option<&'a HistoryEntry> {
    page.entries
        .iter()
        .rfind(|entry| entry.info["role"] == "assistant" && entry.info["parentID"] == user)
}

pub(super) fn started(page: &HistoryPage, user: &str) -> bool {
    latest_assistant(page, user).is_some()
}

pub(super) fn terminal(page: &HistoryPage, user: &str) -> Option<Phase> {
    let latest = latest_assistant(page, user)?;
    let info = &latest.info;
    if info["error"]["name"] == "MessageAbortedError" {
        return Some(Phase::Stopped);
    }
    if !info["error"].is_null() {
        return Some(Phase::Failed);
    }
    if info["time"]["completed"].is_number() {
        if info["finish"]
            .as_str()
            .is_some_and(|finish| !matches!(finish, "tool-calls" | "unknown"))
        {
            return Some(Phase::Completed);
        }
        // Stock can end the loop on a rejected native permission without a
        // final assistant response. Its terminal marker is the tool error.
        if latest.parts.iter().any(|part| {
            part["type"] == "tool"
                && part["state"]["status"] == "error"
                && part["state"]["error"]
                    == "The user rejected permission to use this specific tool call."
        }) {
            return Some(Phase::Stopped);
        }
    }
    None
}

pub(super) fn snapshot(
    id: Uuid,
    record: Record,
    page: HistoryPage,
    idle: bool,
    asks: Vec<AskView>,
) -> Snapshot {
    let receipt = record.turn.as_ref().map(Turn::receipt);
    let active = record
        .turn
        .as_ref()
        .filter(|turn| turn.phase.outcome().is_none());
    let mut transcript = timeline(&page);
    transcript.detail["active"] = json!(active.map(|turn| &turn.native_message));
    transcript.detail["interrupted"] =
        json!(receipt.is_some_and(|receipt| receipt.phase == Phase::Stopped));
    transcript.detail["last_finished"] = json!(record.turn.as_ref().and_then(|turn| {
        matches!(turn.phase, Phase::Completed | Phase::Failed).then_some(&turn.native_message)
    }));
    if receipt.is_some_and(|receipt| receipt.phase == Phase::Failed) {
        transcript.detail["error"] = json!("OpenCode reported a turn failure.");
    }
    Snapshot {
        session: session_view(id, &record, idle, !asks.is_empty()),
        transcript,
        asks,
        before: page.before,
        receipt,
    }
}

pub(super) fn session_view(
    id: Uuid,
    record: &Record,
    idle: bool,
    needs_input: bool,
) -> SessionView {
    let result = record.turn.as_ref().and_then(|turn| {
        Some(SessionResult {
            turn: turn.native_message.clone(),
            completed_at: record.updated_at,
            outcome: turn.phase.outcome()?,
        })
    });
    let status = if needs_input {
        "needs_input"
    } else if !idle || record.turn.is_some() && result.is_none() {
        "running"
    } else if record
        .turn
        .as_ref()
        .is_some_and(|turn| turn.phase == Phase::Failed)
    {
        "failed"
    } else {
        "idle"
    };
    SessionView {
        id: id.to_string(),
        title: record.title.clone(),
        parent: None,
        status: status.into(),
        updated_at: record.updated_at,
        capabilities: Vec::new(),
        detail: json!({"owned": true, "last_result": result}),
    }
}

pub(super) fn timeline(page: &HistoryPage) -> Timeline {
    let items = page
        .entries
        .iter()
        .flat_map(|entry| {
            let info = &entry.info;
            let role = info["role"].as_str();
            let turn = info["parentID"].as_str().or_else(|| info["id"].as_str());
            entry.parts.iter().filter_map(move |part| {
                let turn = turn?;
                let id = part["id"].as_str()?;
                let (role, text, detail) = match (part["type"].as_str()?, role) {
                    ("text", Some("user")) if part["synthetic"] != true => {
                        let text = part["text"].as_str()?.to_owned();
                        let item = Item::UserMessage {
                            content: vec![Content {
                                text: Some(text.clone()),
                                path: None,
                            }],
                        };
                        ("user", text, json!(item))
                    }
                    ("text", Some("assistant")) if part["synthetic"] != true => {
                        ("assistant", part["text"].as_str()?.to_owned(), Value::Null)
                    }
                    ("tool", Some("assistant")) => {
                        let tool = part["tool"].as_str()?;
                        let state = &part["state"];
                        let status = state["status"].as_str().unwrap_or("pending").to_owned();
                        let output = state["output"].as_str().or_else(|| state["error"].as_str());
                        let item = Item::ToolCall {
                            status: status.clone(),
                            tool: tool.to_owned(),
                        };
                        (
                            "tool",
                            output
                                .map_or_else(|| tool.into(), |output| format!("{tool}\n{output}")),
                            json!({"item":item, "status":status, "output":output}),
                        )
                    }
                    _ => return None,
                };
                Some(TimelineItem {
                    turn: turn.into(),
                    id: id.into(),
                    role: role.into(),
                    text,
                    detail,
                    revision: 0,
                })
            })
        })
        .collect();
    Timeline {
        items,
        detail: json!({"ready":true, "loading":false, "older":page.before.is_some()}),
    }
}

pub(super) fn asks(
    id: Uuid,
    turn: Option<&Turn>,
    live: &mut Live,
    pending: Vec<PermissionRequest>,
) -> Vec<AskView> {
    let Some(turn) = turn.filter(|turn| turn.phase == Phase::Pending) else {
        live.tokens.clear();
        live.answered.clear();
        return Vec::new();
    };
    let pending_ids: HashSet<_> = pending.iter().map(|ask| ask.id.as_str()).collect();
    live.tokens
        .retain(|request, _| pending_ids.contains(request.as_str()));
    live.answered
        .retain(|request| pending_ids.contains(request.as_str()));
    pending
        .into_iter()
        .filter(|ask| !live.answered.contains(&ask.id))
        .map(|ask| {
            let token = live
                .tokens
                .entry(ask.id.clone())
                .or_insert_with(Uuid::new_v4);
            let command = if ask.permission == "bash" {
                ask.metadata["command"]
                    .as_str()
                    .map(|command| CommandApproval {
                        command: command.into(),
                        cwd: ask.metadata["workdir"].as_str().map(Into::into),
                        decline: 1,
                    })
            } else {
                None
            };
            AskView {
                command,
                id: RequestId::Text(ask.id),
                token: token.to_string(),
                thread: Some(id.to_string()),
                turn: Some(turn.native_message.clone()),
                item: None,
                title: format!("Allow {}?", ask.permission),
                detail: ask.patterns.join("\n"),
                choices: vec!["Allow once".into(), "Reject".into()],
                choice_details: vec![None, None],
                questions: Vec::new(),
            }
        })
        .collect()
}
