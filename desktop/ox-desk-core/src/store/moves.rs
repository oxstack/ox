//! Release the native writer for one already-fenced session.
use super::{Call, Connection, Pending, Store};
use crate::{
    changed::Changed,
    codex::{commands::UnsubscribeStatus, history::ResumedThread, reply::Reply},
    host_protocol::TimelineItem,
    runtime::{Access, Billing},
};
use serde_json::Value;
use std::io;

#[derive(Debug, PartialEq, Eq)]
pub struct MoveWriterRelease {
    /// Exact OX thread key within the Store's project and billing route.
    pub thread: String,
    pub result: Result<(), String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct MoveWriterPreparation {
    pub operation: String,
    /// The target's durable native thread ID, usable after activation.
    pub result: Result<String, String>,
}

#[derive(Debug)]
pub(super) enum PrepareWriter {
    Open {
        operation: String,
        items: Vec<Value>,
    },
    Inject {
        operation: String,
        thread: String,
    },
}

impl PrepareWriter {
    pub(super) fn id(&self) -> &str {
        match self {
            Self::Open { operation, .. } | Self::Inject { operation, .. } => operation,
        }
    }
}

#[derive(Default)]
pub(super) struct CodexRelease {
    acknowledged: bool,
    closed: bool,
}

pub(super) struct History {
    pub(super) items: Vec<TimelineItem>,
    pub(super) previous: Option<usize>,
}

impl Store {
    /// Restore an idle source's history after its host stopped during preparation.
    /// The actor holds its write fence and no input is sent to the runtime.
    pub fn reopen_move_source_writer(&mut self, id: &str) -> io::Result<()> {
        if self.is_session_scoped() {
            let row = self
                .catalog
                .get(id)
                .ok_or_else(|| io::Error::other("The source session is outside this catalog."))?;
            self.check_route(row)?;
            return self.claude_resume(id, false);
        }
        self.select(id)
    }

    /// The owning helper starts in a private process group with its stock runtime.
    pub fn move_writer_group(&self, thread: &str) -> Option<u32> {
        match &self.link {
            crate::links::Link::Codex(link) => Some(link.process_group()),
            crate::links::Link::Claude(_) => self.claude_move_writer_group(thread),
            _ => None,
        }
    }
    pub fn move_writer_is_unpublished(&self, thread: &str) -> bool {
        self.unpublished_move_writers.contains(thread)
    }

    pub fn recover_prepared_move_writer(&mut self, operation: String, thread: &str) {
        self.moving_threads.insert(thread.into());
        self.unpublished_move_writers.insert(thread.into());
        self.prepared_move_writers.insert(operation, thread.into());
    }

    /// `Some(None)` means the full archive is already visible.
    pub fn moved_history_cursor(&self, thread: &str) -> Option<Option<usize>> {
        self.moved_history
            .get(thread)
            .map(|history| history.previous)
    }

    pub fn show_moved_history(
        &mut self,
        thread: &str,
        mut items: Vec<TimelineItem>,
        previous: Option<usize>,
    ) {
        if let Some(mut history) = self.moved_history.remove(thread) {
            items.append(&mut history.items);
        }
        self.moved_history
            .insert(thread.into(), History { items, previous });
        self.changed.replace_transcript(thread);
    }

    /// Reserve a Codex thread in the chosen target checkout and checkpoint the
    /// transferred context. No inference turn starts during this reservation.
    pub fn prepare_codex_move_writer(
        &mut self,
        operation: String,
        access: Option<Access>,
        items: Vec<Value>,
    ) -> io::Result<()> {
        if self.is_host_client()
            || self.is_session_scoped()
            || self.state != Connection::Ready
            || items.is_empty()
        {
            return Err(io::Error::other(
                "Open the target Codex route before preparing a move.",
            ));
        }
        if self.pending.values().any(|(_, pending)| {
            matches!(pending, Pending::PrepareMoveWriter(active) if active.id() == operation)
        }) {
            return Ok(());
        }
        if let Some(thread) = self.prepared_move_writers.get(&operation).cloned() {
            self.finish_move_writer_preparation(&operation, Ok(thread));
            return Ok(());
        }
        let model = match &self.profile.route.billing {
            Billing::Api(alias) => Some(alias.clone()),
            Billing::Subscription => None,
        };
        self.request(
            Call::New {
                preset: access,
                project: self.profile.project.clone(),
                model,
            },
            Pending::PrepareMoveWriter(PrepareWriter::Open { operation, items }),
        )
    }

    pub fn take_move_writer_preparations(&mut self) -> Vec<MoveWriterPreparation> {
        std::mem::take(&mut self.move_writer_preparations)
    }

    /// The host calls this only after recording confirmed destination ownership.
    pub fn enable_prepared_move_writer(&mut self, operation: &str, id: &str) -> bool {
        if self
            .prepared_move_writers
            .get(operation)
            .map(String::as_str)
            != Some(id)
        {
            return false;
        }
        self.prepared_move_writers.remove(operation);
        self.moving_threads.remove(id);
        self.unpublished_move_writers.remove(id);
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        true
    }

    pub(super) fn apply_move_writer_reply(&mut self, pending: PrepareWriter, reply: Reply) {
        match (pending, reply) {
            (PrepareWriter::Open { operation, items }, Reply::Opened(opened)) => {
                if let Err(error) = self.checkpoint_move_writer(operation.clone(), items, *opened) {
                    self.finish_move_writer_preparation(&operation, Err(error.to_string()));
                }
            }
            (PrepareWriter::Inject { operation, thread }, Reply::Acknowledged) => {
                self.prepared_move_writers
                    .insert(operation.clone(), thread.clone());
                self.finish_move_writer_preparation(&operation, Ok(thread));
            }
            (pending, _) => self.finish_move_writer_preparation(
                pending.id(),
                Err("Unexpected Codex reply while preparing the move.".into()),
            ),
        }
    }

    fn checkpoint_move_writer(
        &mut self,
        operation: String,
        items: Vec<Value>,
        reply: ResumedThread,
    ) -> io::Result<()> {
        let thread = reply.thread.id.clone();
        self.moving_threads.insert(thread.clone());
        self.unpublished_move_writers.insert(thread.clone());
        self.verify_opened(&thread, &reply)?;
        let mut row = reply.thread;
        row.model = Some(reply.model.clone());
        self.record_row(row);
        let transcript = self.transcripts.entry(thread.clone()).or_default();
        transcript.model = Some(reply.model);
        transcript.tier = reply.service_tier;
        transcript.effort = reply.reasoning_effort;
        transcript.access = Some(reply.permissions.access);
        transcript.ceiling = reply.permissions.ceiling;
        transcript.ready = true;
        transcript.verified = true;
        self.request(
            Call::Inject {
                thread: thread.clone(),
                items,
            },
            Pending::PrepareMoveWriter(PrepareWriter::Inject { operation, thread }),
        )
    }

    pub(super) fn finish_move_writer_preparation(
        &mut self,
        operation: &str,
        result: Result<String, String>,
    ) {
        self.move_writer_preparations.push(MoveWriterPreparation {
            operation: operation.into(),
            result,
        });
    }

    /// Starts release of an idle native writer. The host keeps its durable move
    /// fence in force while it waits for the asynchronous result.
    pub fn release_writer_for_move(&mut self, id: &str) -> io::Result<()> {
        if self.pending_codex_moves.contains_key(id) {
            return Ok(());
        }
        if self.is_host_client()
            || self.state != Connection::Ready
            || self.in_terminal(id)
            || self.is_native_live(id)
            || !self
                .transcript(id)
                .is_some_and(|t| t.is_ready() && !t.is_loading())
        {
            return Err(io::Error::other(
                "Open the owned source session before releasing it.",
            ));
        }
        if self.thread_busy(id)
            || self.changing_options(id)
            || self.pending.values().any(|(_, p)| p.loads_history_for(id))
        {
            return Err(io::Error::other(
                "Wait for the source session's work to finish.",
            ));
        }
        if self.is_session_scoped() {
            self.release_claude_writer_for_move(id)?;
        } else {
            self.request(
                Call::Unsubscribe(id.into()),
                Pending::LeaveForMove(id.into()),
            )?;
            self.pending_codex_moves
                .insert(id.into(), CodexRelease::default());
        }
        self.moving_threads.insert(id.into());
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
        Ok(())
    }

    pub fn take_move_writer_releases(&mut self) -> Vec<MoveWriterRelease> {
        std::mem::take(&mut self.move_writer_releases)
    }

    pub(super) fn acknowledge_codex_move_release(&mut self, id: &str, status: UnsubscribeStatus) {
        if let Some(release) = self.pending_codex_moves.get_mut(id) {
            release.acknowledged = true;
            // The app-server can leave a runtime loaded after the last unsubscribe.
            // Wait for its thread/closed signal unless it is already unloaded.
            if release.closed || matches!(status, UnsubscribeStatus::NotLoaded) {
                self.complete_move_writer_release(id, Ok(()));
            }
        }
    }

    pub(super) fn observe_codex_move_close(&mut self, id: &str) -> bool {
        let Some(release) = self.pending_codex_moves.get_mut(id) else {
            return false;
        };
        release.closed = true;
        if release.acknowledged {
            self.complete_move_writer_release(id, Ok(()));
        }
        true
    }

    pub(super) fn complete_move_writer_release(&mut self, id: &str, result: Result<(), String>) {
        self.pending_codex_moves.remove(id);
        if result.is_ok()
            && let Some(t) = self.transcripts.get_mut(id)
        {
            t.ready = false;
            t.verified = false;
            t.active_turn = None;
        }
        self.move_writer_releases.push(MoveWriterRelease {
            thread: id.into(),
            result,
        });
        self.changed.transcript(id);
        self.changed.mark(Changed::THREADS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codex::{
            frame::{Frame, RequestId},
            names::Method,
        },
        links::LinkEvent,
        runtime::Route,
        store::Profile,
    };
    use serde_json::{Value, json};

    fn fixture() -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::inert(Profile {
            project: root.path().into(),
            route: Route::CODEX_SUBSCRIPTION,
            provider: "openai".into(),
        })
        .unwrap();
        store.state = Connection::Ready;
        for id in ["first", "second", "unrelated"] {
            let t = store.transcripts.entry(id.into()).or_default();
            t.ready = true;
            t.verified = true;
        }
        (root, store)
    }

    fn reply(store: &mut Store, id: i64, response: Value) {
        let mut response = response;
        response["id"] = id.into();
        store.apply(LinkEvent::Frame(
            Frame::decode(&serde_json::to_vec(&response).unwrap(), |_| {
                Some(Method::Unsubscribe)
            })
            .unwrap(),
        ));
    }

    fn closed(store: &mut Store, id: &str) {
        let frame = json!({"method":"thread/closed","params":{"threadId":id}});
        store.apply(LinkEvent::Frame(
            Frame::decode(&serde_json::to_vec(&frame).unwrap(), |_| None).unwrap(),
        ));
    }

    #[test]
    fn incoming_mode_uses_destination_settings_without_consuming_the_draft() {
        use crate::host_policy::HostPolicy;

        let (_root, mut store) = fixture();
        store.set_start_mode(|_| None);
        store.set_next_access(Access::Yolo);
        assert_eq!(store.move_start_access(None), None);
        assert_eq!(store.next_access(), Some(Access::Yolo));

        store.set_start_mode(|_| Some(Access::Workspace));
        assert_eq!(store.move_start_access(None), Some(Access::Workspace));
        store.use_host_policy(HostPolicy::AlwaysRead);
        assert_eq!(store.move_start_access(None), Some(Access::ReadOnly));
        store.use_host_policy(HostPolicy::AlwaysWrite);
        assert_eq!(store.move_start_access(None), Some(Access::Workspace));
    }

    #[test]
    fn staging_a_codex_writer_checkpoints_context_without_sending_a_turn() {
        let (_root, mut store) = fixture();
        let items = vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Earlier work"}]}),
        ];
        store
            .prepare_codex_move_writer("move".into(), None, items.clone())
            .unwrap();
        let started = store.seq;
        store
            .prepare_codex_move_writer("move".into(), None, items.clone())
            .unwrap();
        assert_eq!(store.seq, started);
        let opened = serde_json::from_value(json!({
            "thread":{"id":"studio","modelProvider":"openai","cwd":store.profile.project},
            "modelProvider":"openai","model":"stock-model",
            "sandbox":{"type":"dangerFullAccess"},"approvalPolicy":"on-request",
            "approvalsReviewer":"user","activePermissionProfile":null,
        }))
        .unwrap();
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(started),
            result: Ok(Reply::Opened(opened)),
        }));
        assert_ne!(store.seq, started);
        assert!(store.take_move_writer_preparations().is_empty());
        assert!(!store.can_send(Some("studio")));
        assert!(store.can_send(Some("first")));
        assert!(!store.enable_prepared_move_writer("move", "studio"));
        let checkpoint = store.seq;
        store.apply(LinkEvent::Frame(Frame::Reply {
            id: RequestId::Number(checkpoint),
            result: Ok(Reply::Acknowledged),
        }));
        assert_eq!(store.seq, checkpoint);
        let prepared = MoveWriterPreparation {
            operation: "move".into(),
            result: Ok("studio".into()),
        };
        assert_eq!(store.take_move_writer_preparations(), [prepared]);
        assert!(!store.has_pending_turn());
        assert!(!store.can_send(Some("studio")));
        assert!(!store.enable_prepared_move_writer("other operation", "studio"));
        assert!(store.enable_prepared_move_writer("move", "studio"));
        assert!(store.can_send(Some("studio")));
    }

    #[test]
    fn codex_release_waits_for_the_matching_unsubscribe_reply() {
        let (_root, mut store) = fixture();
        store.release_writer_for_move("first").unwrap();
        let first = store.seq;
        store.release_writer_for_move("first").unwrap();
        assert_eq!(
            store.seq, first,
            "joining an in-flight release sends no extra request"
        );
        store.release_writer_for_move("second").unwrap();
        let second = store.seq;
        assert!(store.take_move_writer_releases().is_empty());
        assert!(store.take_handoff().is_none());
        assert!(!store.can_send(Some("first")));
        assert!(store.can_send(Some("unrelated")));
        assert!(store.open_catalog_id("first").is_err());

        reply(
            &mut store,
            second,
            json!({"error":{"code":-32000,"message":"Release refused"}}),
        );
        let failed = store.take_move_writer_releases();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].thread, "second");
        assert!(
            failed[0]
                .result
                .as_ref()
                .unwrap_err()
                .contains("Release refused")
        );
        assert!(store.transcript("second").unwrap().is_ready());
        assert!(!store.can_send(Some("second")));

        closed(&mut store, "first");
        assert!(store.take_move_writer_releases().is_empty());
        reply(
            &mut store,
            first,
            json!({"result":{"status":"unsubscribed"}}),
        );
        assert_eq!(
            store.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: "first".into(),
                result: Ok(())
            }]
        );
        assert!(!store.transcript("first").unwrap().is_ready());
        assert!(store.can_send(Some("unrelated")));
        assert!(store.take_handoff().is_none());

        store.release_writer_for_move("second").unwrap();
        let retry = store.seq;
        reply(
            &mut store,
            retry,
            json!({"result":{"status":"notSubscribed"}}),
        );
        closed(&mut store, "first");
        assert!(store.take_move_writer_releases().is_empty());
        closed(&mut store, "second");
        assert_eq!(
            store.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: "second".into(),
                result: Ok(())
            }]
        );
    }

    #[test]
    fn busy_and_closed_codex_writers_never_report_release_success() {
        let (_root, mut store) = fixture();
        store.transcripts.get_mut("first").unwrap().active_turn = Some("running".into());
        assert!(store.release_writer_for_move("first").is_err());
        assert!(!store.moving_threads.contains("first"));
        store.release_writer_for_move("second").unwrap();
        let request = store.seq;
        reply(
            &mut store,
            request,
            json!({"result":{"status":"unsubscribed"}}),
        );
        assert!(store.take_move_writer_releases().is_empty());
        store.apply(LinkEvent::Closed {
            reason: "App-server disconnected before thread closure".into(),
            stderr_tail: vec![],
        });
        let result = store.take_move_writer_releases();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].thread, "second");
        assert!(result[0].result.is_err());
    }

    #[test]
    fn an_unloaded_codex_reply_confirms_release_without_an_extra_close() {
        let (_root, mut store) = fixture();
        store.release_writer_for_move("first").unwrap();
        let id = store.seq;
        reply(&mut store, id, json!({"result":{"status":"notLoaded"}}));
        assert_eq!(
            store.take_move_writer_releases(),
            [MoveWriterRelease {
                thread: "first".into(),
                result: Ok(())
            }]
        );
    }
}
