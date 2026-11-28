use super::*;
use crate::worker_notices::Notice;

const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Reconciliation may need multiple pages. Retry a refused cursor with backoff,
/// then wait for a manual history load or a fresh parent connection.
#[derive(Default)]
pub(super) struct HistoryPage {
    cursor: Option<String>,
    tries: u32,
    next: Option<Instant>,
}

impl HistoryPage {
    fn request(&mut self, cursor: &str, now: Instant) -> bool {
        if self.cursor.as_deref() != Some(cursor) {
            self.cursor = Some(cursor.into());
            self.tries = 0;
            self.next = None;
        }
        if self.tries >= 3 || self.next.is_some_and(|at| at > now) {
            return false;
        }
        self.tries += 1;
        self.next = Some(now + Duration::from_secs(1 << self.tries));
        true
    }
}

/// A pre-upgrade completed job should not start an unsolicited turn. Jobs that
/// were running when a notice-aware host stopped need an interruption event.
pub(super) fn recover(ledger: &mut Ledger, workers: &Registry) -> io::Result<()> {
    ledger.change(|data| {
        for (_, _, e) in workers.jobs() {
            let o = &e.origin;
            if (o.linked.is_none() && (o.parent.scope.is_none() || o.parent.session.is_none()))
                || o.by_operator
            {
                continue;
            }
            // The outbox can reach disk just before the projected completion
            // record. An absent or older record cannot undo that transition.
            if data
                .worker_notices
                .observed
                .get(&o.job_id)
                .is_some_and(|s| {
                    matches!(
                        s.as_str(),
                        "completed" | "failed" | "interrupted" | "cancelled"
                    )
                })
                && !e.marks.cancelled
            {
                continue;
            }
            let (status, preview) = if e.marks.cancelled {
                ("cancelled", "")
            } else if let Some(last) = &e.marks.last {
                (
                    last.status.as_str(),
                    last.error
                        .as_deref()
                        .or(last.result.as_deref())
                        .unwrap_or_default(),
                )
            } else {
                (
                    "interrupted",
                    "The host restarted before the worker finished.",
                )
            };
            if data.worker_notices.initialized {
                data.worker_notices.observe(o, status, preview);
            } else {
                data.worker_notices.observed.insert(o.job_id, status.into());
            }
        }
        data.worker_notices.initialized = true;
    })
}

impl Engine {
    pub(super) fn apply_notice_receipts(&mut self, scope: &Scope) -> io::Result<()> {
        let Some(live) = self.running.get_mut(scope) else {
            return Ok(());
        };
        let receipts = live.store.take_worker_notice_receipts();
        self.record_notice_receipts(receipts)
    }

    pub(super) fn record_notice_receipts(
        &mut self,
        receipts: Vec<ox_desk_core::store::WorkerNoticeReceipt>,
    ) -> io::Result<()> {
        use ox_desk_core::store::WorkerNoticeReceipt::*;
        if receipts.is_empty() {
            return Ok(());
        }
        self.ledger.change(|d| {
            for receipt in receipts {
                match receipt {
                    Accepted(id) => {
                        d.worker_notices.pending.retain(|n| n.id != id);
                        self.notice_attempts.remove(&id);
                        self.notice_history.remove(&id);
                    }
                    Rejected(id) => {
                        if let Some(n) = d.worker_notices.pending.iter_mut().find(|n| n.id == id) {
                            n.submitted = false;
                        }
                    }
                }
            }
        })
    }
    pub(super) fn observe_worker_notices(&mut self, scope: &Scope) -> io::Result<()> {
        let Some(live) = self.running.get(scope) else {
            return Ok(());
        };
        let mut updates = Vec::new();
        for (id, entry) in self.workers.in_scope(scope) {
            let origin = &entry.origin;
            if origin.relation != ox_desk_core::origin::Relation::Worker
                || origin.by_operator
                || (origin.linked.is_none()
                    && (origin.parent.scope.is_none() || origin.parent.session.is_none()))
            {
                continue;
            }
            let row = live.state.sessions.iter().find(|s| s.id == id);
            let state = row.map(|r| r.status.as_str());
            let asking;
            let (status, preview) = if entry.marks.cancelled {
                (
                    "cancelled",
                    if entry.marks.expired {
                        "The worker reached its time limit."
                    } else {
                        ""
                    },
                )
            } else if state == Some("needs_input") {
                let requests = self.worker_requests(scope, id, origin);
                asking = requests
                    .iter()
                    .find(|r| r["parent_may_answer"] == true)
                    .map(|r| {
                        format!(
                            "The worker asks you: {} (request_id {}). Answer it with answer_ox_worker (job_id {}), or leave it for the user in OX.\n{}",
                            r["title"].as_str().unwrap_or("request"),
                            r["request_id"],
                            origin.job_id,
                            r["detail"].as_str().unwrap_or_default(),
                        )
                    });
                (
                    "waiting_for_operator",
                    asking.as_deref().unwrap_or(
                        "The worker is waiting for a person to answer an approval or question in OX.",
                    ),
                )
            } else if state == Some("running") {
                ("running", "")
            } else if let Some(last) = &entry.marks.last {
                (
                    last.status.as_str(),
                    last.error
                        .as_deref()
                        .or(last.result.as_deref())
                        .unwrap_or_default(),
                )
            } else if state == Some("failed") {
                ("failed", "")
            } else if state == Some("interrupted")
                || matches!(live.store.state(), Connection::Closed(_))
            {
                (
                    "interrupted",
                    "The worker's runtime closed before it finished.",
                )
            } else if !self
                .ledger
                .data
                .worker_notices
                .observed
                .contains_key(&origin.job_id)
            {
                ("running", "")
            } else {
                continue;
            };
            if self
                .ledger
                .data
                .worker_notices
                .observed
                .get(&origin.job_id)
                .is_none_or(|s| s != status)
            {
                updates.push((origin.clone(), status.to_owned(), preview.to_owned()));
            }
        }
        if !updates.is_empty() {
            self.ledger.change(|d| {
                for (origin, status, preview) in &updates {
                    d.worker_notices.observe(origin, status, preview);
                }
            })?;
            for (origin, status, _) in updates {
                self.prepare_worker_result(&origin, &status);
            }
        }
        Ok(())
    }

    pub(super) fn deliver_worker_notices(&mut self) -> io::Result<()> {
        if self.ledger.data.worker_notices.pending.is_empty() {
            return Ok(());
        }
        let pending = self.ledger.data.worker_notices.pending.clone();
        let mut parents = HashSet::new();
        for notice in pending {
            if !notice.announced {
                self.ledger.change(|d| {
                    if let Some(n) = d
                        .worker_notices
                        .pending
                        .iter_mut()
                        .find(|n| n.id == notice.id)
                    {
                        n.announced = true;
                    }
                })?;
                // Native worker approvals and turn completions already notify the
                // operator. A job without a session, and a stop with no finished
                // turn, still need an addressable notification on the parent.
                let has_worker = self.workers.job(notice.job).is_some();
                let remote = self.ledger.data.forwarded.get(&notice.job);
                let synthetic = remote.is_some()
                    || match notice.status.as_str() {
                        "failed" | "declined" => !has_worker,
                        "cancelled" | "interrupted" => true,
                        _ => false,
                    };
                let mut label =
                    format!("Worker #{} {}", notice.job, notice.status.replace('_', " "));
                if let Some(j) = remote {
                    label.push_str(&format!(" on {}", j.name));
                }
                if synthetic {
                    self.broadcast(
                        &notice.scope,
                        vec![Change::Notification {
                            thread: notice.session.clone(),
                            title: label.clone(),
                            event: match notice.status.as_str() {
                                "completed" | "cancelled" => wire::NotificationKind::Finished,
                                "waiting_for_operator" => wire::NotificationKind::Approval,
                                _ => wire::NotificationKind::Failed,
                            },
                            goal_reached: false,
                        }],
                    )?;
                }
                if self.running.contains_key(&notice.scope) {
                    self.process_scope(&notice.scope, vec![], true)?;
                } else if let Some(state) = self.cold.get_mut(&notice.scope) {
                    self.ledger.data.worker_notices.apply_to(state);
                    if let Some(row) = state
                        .sessions
                        .iter()
                        .find(|r| r.id == notice.session)
                        .cloned()
                    {
                        self.broadcast(&notice.scope, vec![Change::Session { session: row }])?;
                    }
                }
            }
            if self
                .ledger
                .data
                .forwarded
                .get(&notice.job)
                .is_some_and(|j| j.origin.by_operator)
            {
                self.ledger
                    .change(|d| d.worker_notices.pending.retain(|n| n.id != notice.id))?;
                continue;
            }
            let parent = (notice.scope.clone(), notice.session.clone());
            if !parents.contains(&parent) && !self.deliver_worker_notice(&notice)? {
                parents.insert(parent);
            }
        }
        Ok(())
    }

    fn deliver_worker_notice(&mut self, n: &Notice) -> io::Result<bool> {
        if !self.can_deliver_move_worker_notice(&n.scope, &n.session)? {
            return Ok(false);
        }
        let update_reserved = self.update_reserved();
        let Some(live) = self.running.get_mut(&n.scope) else {
            return Ok(false);
        };
        let store = &mut live.store;
        if n.submitted
            && !self.notice_attempts.contains_key(&n.id)
            && store
                .transcript(&n.session)
                .is_some_and(|t| t.is_ready() && t.user_messages(&n.text, &[]) != 0)
        {
            self.ledger
                .change(|d| d.worker_notices.pending.retain(|p| p.id != n.id))?;
            self.notice_attempts.remove(&n.id);
            self.notice_history.remove(&n.id);
            return Ok(true);
        }
        let verified = store.transcript(&n.session).is_some_and(|t| t.is_ready());
        if matches!(store.state(), Connection::Closed(_))
            || (store.is_ended(&n.session) && !verified)
        {
            self.notice_attempts.remove(&n.id);
            self.notice_history.remove(&n.id);
            return Ok(false);
        }
        // Only a verified open parent can be woken. Closing the UI leaves its
        // process running; explicitly ending the process holds the event until reopen.
        if self.draining.is_some()
            || update_reserved
            || store.thread_busy(&n.session)
            || !verified
            || !store.can_compose(Some(&n.session))
        {
            return Ok(false);
        }
        let draft = store.draft(Some(&n.session));
        if (!draft.is_empty() && draft != n.text)
            || !store.draft_images(Some(&n.session)).is_empty()
        {
            return Ok(false);
        }
        if let Some(last) = self.notice_attempts.get(&n.id) {
            // Native refusals clear submitted independently of the user's draft.
            // A timeout or disconnect cannot prove the turn was refused.
            if last.elapsed() < RETRY_AFTER || n.submitted {
                return Ok(false);
            }
        } else if n.submitted {
            // After a restart, a crash between the native send and the ledger
            // acknowledgment is reconciled against all pages before retrying.
            let transcript = store.transcript(&n.session).expect("verified above");
            if transcript.is_loading() {
                return Ok(false);
            }
            if let Some(cursor) = transcript.older_cursor() {
                if self
                    .notice_history
                    .entry(n.id)
                    .or_default()
                    .request(cursor, Instant::now())
                {
                    let _ = store.dispatch(Action::LoadOlder(n.session.clone()));
                }
                return Ok(false);
            }
            self.notice_history.remove(&n.id);
        }
        self.ledger.change(|d| {
            if let Some(p) = d.worker_notices.pending.iter_mut().find(|p| p.id == n.id) {
                p.submitted = true;
            }
        })?;
        self.notice_attempts.insert(n.id, Instant::now());
        if store
            .dispatch_as(
                Action::SendText(Some(n.session.clone()), n.text.clone()),
                Sender::WorkerNotice { id: n.id },
            )
            .is_err()
        {
            self.ledger.change(|d| {
                if let Some(p) = d.worker_notices.pending.iter_mut().find(|p| p.id == n.id) {
                    p.submitted = false;
                }
            })?;
        } else {
            self.process_scope(&n.scope, vec![], true)?;
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_reconciliation_bounds_retries_per_cursor() {
        let mut page = HistoryPage::default();
        let now = Instant::now();
        assert!(page.request("older", now));
        assert!(!page.request("older", now));
        let second = now + Duration::from_secs(2);
        assert!(page.request("older", second));
        assert!(!page.request("older", second + Duration::from_secs(1)));
        assert!(page.request("older", second + Duration::from_secs(4)));
        assert!(!page.request("older", now + Duration::from_secs(600)));
        assert!(page.request("earlier", now + Duration::from_secs(600)));
    }
}
