//! Project the authoritative store onto the device protocol. Credentials, process
//! handles and the runtime-specific payload of an approval never leave the owner.
use super::{Connection, Store, ThreadRow, moves::History};
use crate::{
    changed::Changed,
    host_protocol::*,
    transcript::{Entry, ItemKey, Row, Transcript, item::Item},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

fn keys(t: &Transcript) -> impl Iterator<Item = &ItemKey> {
    (0..t.len())
        .filter_map(|index| t.row(index))
        .flat_map(|row| {
            let (first, second) = match row {
                Row::Message(key) => (std::slice::from_ref(key), &[][..]),
                Row::Tools(group) => group.as_slices(),
            };
            first.iter().chain(second)
        })
}
fn item_view(k: &ItemKey, e: &Entry) -> TimelineItem {
    let (role, text, detail) = match &e.item {
        Item::AgentMessage { text, is_update } => {
            ("assistant", text.clone(), json!({"update":is_update}))
        }
        Item::Plan { text } => ("plan", text.clone(), Value::Null),
        Item::UserMessage { content } => (
            "user",
            content
                .iter()
                .filter_map(|p| p.text.as_deref().or(p.path.as_deref()))
                .collect::<Vec<_>>()
                .join("\n"),
            json!(e.item),
        ),
        Item::Reasoning { summary } => ("thinking", summary.join("\n"), json!(e.item)),
        Item::CommandExecution {
            command,
            status,
            aggregated_output,
            exit_code,
            ..
        } => (
            "tool",
            format!(
                "{command}\n{}",
                aggregated_output.as_deref().unwrap_or(&e.output)
            ),
            json!({"item":e.item,"output":e.output,"status":status,"exit":exit_code}),
        ),
        item => {
            let label = match item {
                Item::FileChange { .. } => "File changes",
                Item::AgentOperation { tool, .. }
                | Item::ToolCall { tool, .. }
                | Item::McpToolCall { tool, .. } => tool,
                Item::WebSearch { query } => query,
                Item::Unknown { kind, .. } => kind,
                _ => unreachable!(),
            };
            ("tool", label.into(), json!({"item":item,"output":e.output}))
        }
    };
    TimelineItem {
        turn: k.turn.clone(),
        id: k.item.clone(),
        role: role.into(),
        text,
        detail,
        revision: e.revision,
    }
}
fn timeline_detail(t: &Transcript, prior: Option<&History>) -> Value {
    let older = t.has_older() || prior.is_some_and(|history| history.previous.is_some());
    json!({"active":t.active_turn(),"last_finished":t.last_finished,"interrupted":t.interrupted,"error":t.error(),"ready":t.is_ready(),"loading":t.is_loading(),"older":older,"access":t.access(),"reviews":t.reviews,"usage":t.usage,"command_notice":t.command_notice,"model":t.model,"effort":t.effort,"tier":t.tier,"unread":t.unread()})
}
fn timeline(t: &Transcript, prior: Option<&History>) -> Timeline {
    Timeline {
        items: prior
            .into_iter()
            .flat_map(|history| history.items.iter().cloned())
            .chain(keys(t).filter_map(|k| t.entry(k).map(|e| item_view(k, e))))
            .collect(),
        detail: timeline_detail(t, prior),
    }
}
// Send the same top-level completions the desktop composer displays. Individual
// sessions can have a different command inventory, particularly Claude skills.
#[derive(serde::Serialize)]
struct ComposerSchema {
    #[serde(flatten)]
    caps: crate::slash::Capabilities,
    choices: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    goal_choices: Vec<(String, String)>,
}
fn composer_schema(caps: crate::slash::Capabilities) -> ComposerSchema {
    let choices = crate::slash::completions("/", &caps);
    let goal_choices = if caps.supports_goal() {
        crate::slash::completions("/goal", &caps)
    } else {
        Vec::new()
    };
    ComposerSchema {
        caps,
        choices,
        goal_choices,
    }
}
impl Store {
    fn scope_status(&self) -> String {
        match self.state() {
            Connection::Connecting => "connecting",
            Connection::Ready => "ready",
            Connection::SignInRequired => "sign_in_required",
            Connection::Closed(_) => "closed",
        }
        .into()
    }
    fn composer_caps(&self, id: Option<&str>, caps: &mut Vec<String>) {
        let schema = self.command_caps(id);
        if schema.methods.contains("fuzzyFileSearch") {
            caps.push("mention".into());
        }
        if !self.is_antigravity()
            && !self.is_session_scoped()
            && schema
                .models
                .get(&self.options(id).model)
                .and_then(|m| m.input_modalities.as_ref())
                .is_none_or(|kinds| kinds.iter().any(|kind| kind == "image"))
        {
            caps.push("attach_image".into());
        }
    }
    fn scope_caps(&self) -> Vec<String> {
        let mut caps = Vec::new();
        if self.can_send(None) {
            caps.extend(["start".into(), "access".into()]);
            self.composer_caps(None, &mut caps);
            if self.has_command_schema() && !self.commands_loading() {
                caps.extend(["model_options".into(), "native_commands".into()]);
            }
        }
        if self.can_edit_thread_metadata() {
            caps.push("edit_metadata".into());
        }
        caps
    }
    fn host_detail(&self) -> Value {
        let closed = match self.state() {
            Connection::Closed(e) => Some(e),
            _ => None,
        };
        json!({"provider":self.profile.provider,"selected":self.selected(),"error":closed,"notice":self.notice(),
           "next_access":self.next_access(),"applied_draft_access":self.applied_access(None),"default_options":self.options(None),"schema":self.has_command_schema(),"loading_commands":self.commands_loading(),
           "commands":composer_schema(self.command_caps(None)),"usage":self.usage,"archived":self.archived_threads(),"mention":self.mention,
           "palette_query":self.palette_query(),"palette_more":self.palette_has_more(),"palette_error":self.palette_search_error(),"palette_rows":self.palette_rows(),
           "more":self.has_more(),"next_sending":self.sending(None),"author":self.runtime_name(),"access_choices":crate::runtime::Access::choices(self.profile.route.runtime)})
    }
    fn host_asks(&self) -> Vec<AskView> {
        self.asks()
            .iter()
            .map(|a| AskView {
                command: a.command.clone(),
                id: a.id.clone(),
                token: String::new(),
                thread: a.thread.clone(),
                turn: a.turn.clone(),
                item: a.item.clone(),
                choice_details: a.decisions().iter().map(|v| v.detail.clone()).collect(),
                title: a.title().into(),
                detail: a.detail.clone(),
                choices: a.decisions().iter().map(|v| v.label.clone()).collect(),
                questions: a.questions.clone(),
            })
            .collect()
    }
    fn session_view(&self, r: &ThreadRow) -> SessionView {
        let id = r.id.as_str();
        let t = self.transcript(id);
        let running = t.is_some_and(|t| t.active_turn().is_some());
        let mut caps = vec!["open".to_owned()];
        let status = {
            if self.can_send(Some(id)) {
                caps.push(if running { "steer" } else { "send" }.into());
                self.composer_caps(Some(id), &mut caps);
            } else if self.can_resume_send(id) {
                // An ended thread keeps its composer: a send reopens it, so what the
                // composer could attach before the reap stays attachable.
                caps.push("send".into());
                self.composer_caps(Some(id), &mut caps);
            }
            if running {
                caps.push("stop".into());
            }
            if self.is_native_live(id) {
                caps.push("native_live".into());
            }
            if self.has_live_session(id) {
                caps.push("end".into());
            } else if self.is_ended(id) {
                caps.push("ended".into());
            }
            if (self.can_send(Some(id)) || self.can_configure_saved_claude(id))
                && self.has_command_schema()
                && !self.commands_loading()
            {
                caps.push("native_commands".into());
                if !self.changing_options(id)
                    && (self.is_session_scoped()
                        || self
                            .command_caps(Some(id))
                            .methods
                            .contains("thread/settings/update"))
                {
                    caps.push("model_options".into());
                }
            }
            if self.is_antigravity()
                && self.can_send(Some(id))
                && !self.command_caps(Some(id)).models.is_empty()
            {
                caps.push("model_options".into());
            }
            if r.parent_thread_id.is_none() && self.can_change_access(id) {
                caps.push("access".into());
            }
            if self.can_manage_thread(id) {
                caps.push("fork".into());
                if self.can_edit_thread_metadata() {
                    // Codex accepts a rename from a non-owner but refuses Archive.
                    if !self.is_native_live(id) {
                        caps.push("archive".into());
                    }
                    caps.push("rename".into());
                }
            }
            if self.can_generate_title(id) {
                caps.push("generate_title".into());
            }
            if self.is_native_live(id) {
                "native_live"
            } else if self.asks().iter().any(|a| a.thread.as_deref() == Some(id)) {
                "needs_input"
            } else if running || r.status.as_ref().is_some_and(|s| s.kind == "active") {
                "running"
            } else if t.and_then(|t| t.error()).is_some()
                || r.status.as_ref().is_some_and(|s| s.kind == "systemError")
            {
                "failed"
            } else {
                "idle"
            }
        };
        let resume_access = self.resume_access.get(id).copied().or_else(|| {
            (self.is_session_scoped()
                && self.moved_history.contains_key(id)
                && !self.has_live_session(id))
            .then(|| t.and_then(|transcript| transcript.access()))
            .flatten()
        });
        let detail = json!({"row":r,"worker":r.worker,"unread":r.unread,"order":r.activity_order.to_string(),"generated":r.generated_title,
            "owned":t.is_some(),"native_live":self.is_native_live(id).then(||self.native_app()),"changing":self.changing_options(id),"terminal":self.in_terminal(id),"sending":self.sending(Some(id)),
            "route":self.thread_route(Some(id)),"options":t.map(|_|self.options(Some(id))),"access":self.applied_access(Some(id)),"resume_access":resume_access,"restored_draft":self.restored_drafts.get(id).map(|(message,text)|json!({"message":message,"text":text})),"command_caps":((self.is_session_scoped() || self.is_antigravity()) && t.is_some()).then(||composer_schema(self.command_caps(Some(id))))});
        let mut detail = detail;
        if self.is_antigravity() {
            detail["access_choices"] = json!(self.antigravity_access_choices(id));
        }
        let mut parent = r.parent_thread_id.clone();
        if let Some((nest, extra)) = self.host_annotations.get(id) {
            if let (Some(target), Some(fields)) = (detail.as_object_mut(), extra.as_object()) {
                target.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            parent = parent.or_else(|| nest.clone());
        }
        if let Some(attachment) = self.memory_attachments.get(id) {
            detail["memory_attachment"] = json!(attachment);
        }
        if let Some(result) = t.and_then(|t| t.last_result.as_ref()) {
            detail["last_result"] = json!(result);
        }
        SessionView {
            id: r.id.clone(),
            title: r.title().into(),
            parent,
            status: status.into(),
            updated_at: r.updated_at,
            capabilities: caps,
            detail,
        }
    }
    pub fn host_snapshot(&self, scope: &Scope) -> ScopeState {
        ScopeState {
            scope: scope.clone(),
            connection: self.scope_status(),
            capabilities: self.scope_caps(),
            sessions: self
                .threads()
                .iter()
                .filter(|row| !self.move_writer_is_unpublished(&row.id))
                .map(|r| self.session_view(r))
                .collect(),
            asks: self.host_asks(),
            transcripts: self
                .transcripts
                .iter()
                .filter(|(id, _)| !self.move_writer_is_unpublished(id))
                .map(|(id, t)| (id.clone(), timeline(t, self.moved_history.get(id))))
                .collect::<BTreeMap<_, _>>(),
            detail: self.host_detail(),
        }
    }
    pub fn host_project(
        &self,
        _scope: &Scope,
        changed: &Changed,
        state: &mut ScopeState,
    ) -> Vec<Change> {
        self.host_project_rows(changed, state, |_| Ok::<_, std::convert::Infallible>(()))
            .unwrap()
    }

    /// Include host-owned row metadata in the comparison before emitting a delta.
    pub fn host_project_rows<E>(
        &self,
        changed: &Changed,
        state: &mut ScopeState,
        mut decorate: impl FnMut(&mut SessionView) -> Result<(), E>,
    ) -> Result<Vec<Change>, E> {
        let mut out = Vec::new();
        let scopes_changed = changed.surfaces() != 0;
        if scopes_changed {
            let detail = self.host_detail();
            let capabilities = self.scope_caps();
            let connection = self.scope_status();
            if detail != state.detail
                || connection != state.connection
                || capabilities != state.capabilities
            {
                state.detail = detail.clone();
                state.capabilities = capabilities.clone();
                state.connection = connection.clone();
                out.push(Change::State {
                    detail,
                    connection,
                    capabilities,
                });
            }
            let asks = self.host_asks();
            if asks.len() != state.asks.len()
                || asks.iter().zip(&state.asks).any(|(a, b)| {
                    a.id != b.id
                        || a.thread != b.thread
                        || a.command != b.command
                        || a.choices != b.choices
                        || a.questions != b.questions
                        || a.detail != b.detail
                })
            {
                for ask in &asks {
                    if !state.asks.iter().any(|a| a.id == ask.id)
                        && let Some(thread) = ask.thread.as_ref()
                    {
                        out.push(self.note(
                            thread,
                            if ask.questions.is_empty() {
                                NotificationKind::Approval
                            } else {
                                NotificationKind::Question
                            },
                            false,
                        ));
                    }
                }
                state.asks = asks.clone();
                out.push(Change::Asks { asks });
            }
        }
        let refresh_sessions =
            changed.affects_catalog() || self.threads().len() != state.sessions.len();
        if refresh_sessions {
            let previous_results: HashMap<_, _> = state
                .sessions
                .iter()
                .filter_map(|row| row.detail.get("last_result").map(|r| (&row.id, r)))
                .collect();
            let sessions: Vec<_> = self
                .threads()
                .iter()
                .filter(|row| !self.move_writer_is_unpublished(&row.id))
                .map(|r| {
                    let mut row = self.session_view(r);
                    if let Some(previous) = previous_results.get(&row.id) {
                        carry_result(&mut row, previous);
                    }
                    decorate(&mut row)?;
                    Ok(row)
                })
                .collect::<Result<_, E>>()?;
            if sessions != state.sessions {
                out.extend(session_changes(&state.sessions, &sessions));
                state.sessions = sessions;
            }
        }
        for id in changed.dirty_threads() {
            if self.move_writer_is_unpublished(id) {
                if state.transcripts.remove(id).is_some() {
                    out.push(Change::Retired { thread: id.clone() });
                }
                continue;
            }
            let prior = self.moved_history.get(id);
            let Some(t) = self.transcript(id) else {
                if state.transcripts.remove(id).is_some() {
                    out.push(Change::Retired { thread: id.clone() });
                }
                continue;
            };
            if !refresh_sessions
                && let Some(row) = self.catalog_row(id)
                && let Some(prev) = state.sessions.iter_mut().find(|r| r.id == *id)
            {
                let mut next = self.session_view(row);
                carry_result(&mut next, &prev.detail["last_result"]);
                decorate(&mut next)?;
                if next != *prev {
                    *prev = next.clone();
                    out.push(Change::Session { session: next });
                }
            }
            let Some(previous) = state.transcripts.get_mut(id) else {
                let timeline = timeline(t, prior);
                state.transcripts.insert(id.clone(), timeline.clone());
                out.push(Change::Transcript {
                    thread: id.clone(),
                    timeline,
                });
                continue;
            };
            if changed.replaced_threads().contains(id) {
                // A reread can correct text with the same message ID and revision.
                let current = timeline(t, prior);
                if *previous != current {
                    *previous = current.clone();
                    out.push(Change::Transcript {
                        thread: id.clone(),
                        timeline: current,
                    });
                }
                continue;
            }
            let detail = timeline_detail(t, prior);
            if previous.detail["last_finished"] != detail["last_finished"]
                && !detail["last_finished"].is_null()
            {
                let event = if t.error().is_some() {
                    NotificationKind::Failed
                } else {
                    NotificationKind::Finished
                };
                out.push(self.note(
                    id,
                    event,
                    event == NotificationKind::Finished && t.finished_goal(),
                ));
            }
            if detail != previous.detail {
                previous.detail = detail.clone();
                out.push(Change::TimelineState {
                    thread: id.clone(),
                    detail,
                });
            }
            let mut count = prior.map_or(0, |history| history.items.len());
            let mut reset = false;
            let mut items = Vec::new();
            for k in keys(t) {
                let Some(entry) = t.entry(k) else {
                    continue;
                };
                match previous.items.get_mut(count) {
                    Some(old) if old.turn != k.turn || old.id != k.item => {
                        reset = true;
                        break;
                    }
                    Some(old) if old.revision != entry.revision => {
                        if let Item::AgentMessage { text, is_update } = &entry.item
                            && old.role == "assistant"
                            && old.detail["update"] == *is_update
                            && let Some(suffix) = text.strip_prefix(&old.text)
                        {
                            old.text.push_str(suffix);
                            old.revision = entry.revision;
                            out.push(Change::Append {
                                thread: id.clone(),
                                turn: k.turn.clone(),
                                id: k.item.clone(),
                                text: suffix.to_owned(),
                                revision: entry.revision,
                            });
                        } else {
                            let item = item_view(k, entry);
                            *old = item.clone();
                            items.push(item);
                        }
                    }
                    Some(_) => (),
                    None => {
                        let item = item_view(k, entry);
                        previous.items.push(item.clone());
                        items.push(item);
                    }
                }
                count += 1;
            }
            if reset || count < previous.items.len() {
                let timeline = timeline(t, prior);
                *previous = timeline.clone();
                out.push(Change::Transcript {
                    thread: id.clone(),
                    timeline,
                });
            } else if !items.is_empty() {
                out.push(Change::Items {
                    thread: id.clone(),
                    items,
                });
            }
        }
        Ok(out)
    }
    fn note(&self, thread: &str, event: NotificationKind, goal_reached: bool) -> Change {
        let title = self
            .catalog_row(thread)
            .and_then(|r| r.name.as_ref().or(r.generated_title.as_ref()))
            .map_or("OX session", String::as_str);
        Change::Notification {
            thread: thread.into(),
            title: title.into(),
            event,
            goal_reached,
        }
    }
}

// A reopen can replace the runtime's transcript while the catalog remains live.
// Retain the last result and its original timestamp across catalog refreshes.
fn carry_result(row: &mut SessionView, previous: &Value) {
    if !previous.is_null()
        && (row.detail["last_result"].is_null()
            || row.detail["last_result"]["turn"] == previous["turn"])
    {
        row.detail["last_result"] = previous.clone();
    }
}

/// The changes that turn `old` into `new`. A page of older threads only appends
/// rows, so when every old row keeps its place, send just the appended and
/// edited rows: each client inserts an unknown row at the end, in order. Any
/// other change (a removal or a reorder) sends the whole list. Re-sending the
/// whole list for every page made catalog traffic grow with the square of its
/// length, tens of megabytes for 5,000 threads. A desktop that fell behind
/// reloaded its catalog from the first page and paged everything again (the
/// G6 gate's intermittent failure).
fn session_changes(old: &[SessionView], new: &[SessionView]) -> Vec<Change> {
    let appended_only = new.len() >= old.len()
        && old
            .iter()
            .zip(new)
            .all(|(before, after)| before.id == after.id);
    if !appended_only || old.is_empty() {
        return vec![Change::Sessions {
            sessions: new.to_vec(),
        }];
    }
    new.iter()
        .enumerate()
        .filter(|(i, row)| old.get(*i) != Some(*row))
        .map(|(_, row)| Change::Session {
            session: row.clone(),
        })
        .collect()
}

#[cfg(test)]
mod session_change_tests {
    use super::*;

    fn row(id: &str, title: &str) -> SessionView {
        SessionView {
            id: id.into(),
            title: title.into(),
            parent: None,
            updated_at: 0,
            status: "idle".into(),
            capabilities: vec![],
            detail: serde_json::Value::Null,
        }
    }

    #[test]
    fn streaming_text_does_not_republish_a_decorated_session_row() {
        use crate::{links::codex, runtime::Route, store::Profile, transcript::DeltaTarget};
        let scope = Scope {
            project: "/test".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let link =
            codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
        let mut store = Store::open(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        store.catalog.insert(
            serde_json::from_value(json!({
                "id":"lead","cwd":scope.project,"modelProvider":"openai","name":"Project lead",
            }))
            .unwrap(),
        );
        let transcript = store.transcripts.entry("lead".into()).or_default();
        transcript.ready = true;
        transcript.verified = true;
        transcript.observe_delta(
            "turn".into(),
            "reply".into(),
            DeltaTarget::Agent,
            "First".into(),
        );
        let mut state = store.host_snapshot(&scope);
        let decorate = |row: &mut SessionView| {
            row.capabilities.push("move_session".into());
            row.detail["logical_session_id"] = json!("existing-move");
            Ok::<_, std::convert::Infallible>(())
        };
        let mut catalog = Changed::default();
        catalog.mark(Changed::THREADS);
        store
            .host_project_rows(&catalog, &mut state, decorate)
            .unwrap();
        let transcript = store.transcripts.get_mut("lead").unwrap();
        transcript.observe_delta(
            "turn".into(),
            "reply".into(),
            DeltaTarget::Agent,
            " more".into(),
        );
        let mut streaming = Changed::default();
        streaming.transcript("lead");
        let changes = store
            .host_project_rows(&streaming, &mut state, decorate)
            .unwrap();
        assert!(matches!(changes.as_slice(), [Change::Append { text, .. }] if text == " more"));
        assert!(
            state.sessions[0]
                .capabilities
                .iter()
                .any(|cap| cap == "move_session")
        );
    }

    #[test]
    fn a_catalog_refresh_keeps_a_result_until_a_different_turn_finishes() {
        let mut next = row("a", "A");
        next.detail = json!({});
        let saved = json!({"turn":"first","completed_at":12,"outcome":"completed"});
        carry_result(&mut next, &saved);
        assert_eq!(next.detail["last_result"], saved);
        next.detail["last_result"]["completed_at"] = 13.into();
        carry_result(&mut next, &saved);
        assert_eq!(next.detail["last_result"], saved);
        next.detail["last_result"] = json!({"turn":"second","completed_at":14,"outcome":"failed"});
        carry_result(&mut next, &saved);
        assert_eq!(next.last_result().unwrap().turn, "second");
    }

    #[test]
    fn a_new_page_sends_only_its_rows_and_anything_else_sends_the_list() {
        let first = vec![row("a", "A"), row("b", "B")];
        let paged = vec![row("a", "A"), row("b", "B"), row("c", "C"), row("d", "D")];
        let ids = |changes: Vec<Change>| -> Vec<String> {
            changes
                .into_iter()
                .map(|c| match c {
                    Change::Session { session } => session.id,
                    Change::Sessions { sessions } => format!("all:{}", sessions.len()),
                    _ => "other".into(),
                })
                .collect()
        };
        assert_eq!(ids(session_changes(&first, &paged)), ["c", "d"]);
        let renamed = vec![row("a", "A"), row("b", "B2"), row("c", "C")];
        assert_eq!(ids(session_changes(&first, &renamed)), ["b", "c"]);
        let reordered = vec![row("b", "B"), row("a", "A")];
        assert_eq!(ids(session_changes(&first, &reordered)), ["all:2"]);
        let removed = vec![row("a", "A")];
        assert_eq!(ids(session_changes(&first, &removed)), ["all:1"]);
        assert_eq!(ids(session_changes(&[], &first)), ["all:2"]);
    }

    #[test]
    fn only_a_confirmed_goal_on_the_matching_successful_turn_changes_the_alert() {
        use crate::{codex::frame::Frame, links::codex, runtime::Route, store::Profile};
        let scope = Scope {
            project: "/test".into(),
            route: Route::CODEX_SUBSCRIPTION,
        };
        let link =
            codex::Link::from_command(std::process::Command::new("/bin/cat"), |_| {}).unwrap();
        let mut store = Store::open(
            Profile {
                project: scope.project.clone(),
                route: scope.route.clone(),
                provider: "openai".into(),
            },
            link,
        )
        .unwrap();
        let t = store.transcripts.entry("thread".into()).or_default();
        t.verified = true;
        t.ready = true;
        let mut state = store.host_snapshot(&scope);
        let mut notices = |events: Vec<Value>| {
            for event in events {
                let frame = Frame::decode(&serde_json::to_vec(&event).unwrap(), |_| None).unwrap();
                store.apply(codex::LinkEvent::Frame(frame));
            }
            store
                .host_project(&scope, &store.changed, &mut state)
                .into_iter()
                .filter_map(|change| match change {
                    Change::Notification {
                        event,
                        goal_reached,
                        ..
                    } => Some((event, goal_reached)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let update = |turn: Option<&str>, status: &str| json!({"method":"thread/goal/updated","params":{"threadId":"thread","turnId":turn,"goal":{"status":status}}});
        let clear = || json!({"method":"thread/goal/cleared","params":{"threadId":"thread"}});
        let complete = |turn: &str, status: &str| json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":turn,"status":status}}});
        use NotificationKind::{Failed, Finished};
        assert_eq!(
            notices(vec![
                update(None, "complete"),
                complete("resume", "completed")
            ]),
            [(Finished, false)]
        );
        assert!(notices(vec![clear(), update(Some("goal"), "complete")]).is_empty());
        assert_eq!(
            notices(vec![
                update(Some("goal"), "complete"),
                complete("goal", "completed")
            ]),
            [(Finished, true)]
        );
        assert!(notices(vec![complete("goal", "completed")]).is_empty());
        assert_eq!(
            notices(vec![
                update(Some("later"), "complete"),
                complete("later", "completed")
            ]),
            [(Finished, false)],
        );
        assert_eq!(
            notices(vec![
                clear(),
                update(Some("another"), "complete"),
                complete("unrelated", "completed")
            ]),
            [(Finished, false)],
        );
        assert_eq!(
            notices(vec![
                clear(),
                update(Some("stop"), "complete"),
                complete("stop", "interrupted")
            ]),
            [(Finished, false)],
        );
        assert_eq!(
            notices(vec![
                update(None, "active"),
                update(Some("failed"), "complete"),
                complete("failed", "failed")
            ]),
            [(Failed, false)],
        );
    }
}
