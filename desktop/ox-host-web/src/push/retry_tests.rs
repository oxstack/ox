use super::{tests::fixture, *};
use serde_json::json;

#[test]
fn classifies_permanent_and_retryable_responses_and_both_retry_after_forms() {
    use DeliveryResult::{Done, Gone, Retry};
    let now = SystemTime::UNIX_EPOCH;
    let later = httpdate::fmt_http_date(now + Duration::from_secs(75));
    assert_eq!(
        response(429, Some("75"), now),
        Retry(Duration::from_secs(75))
    );
    assert_eq!(
        response(503, Some(&later), now),
        Retry(Duration::from_secs(75))
    );
    for status in [408, 429, 500, 502, 503, 504] {
        assert_eq!(
            response(status, Some("bad date"), now),
            Retry(Duration::ZERO)
        );
    }
    for status in [201, 301, 400, 401, 403, 413] {
        assert_eq!(response(status, Some("5"), now), Done);
    }
    for status in [404, 410] {
        assert_eq!(response(status, None, now), Gone);
    }
}

fn paired_phone() -> (tempfile::TempDir, App, String, Subscription) {
    use crate::state::State;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = State::load(dir.path()).unwrap();
    state
        .set_origin(Some("https://test.tail.ts.net".into()), 0)
        .unwrap();
    let (id, sub) = register_phone(&mut state);
    let app = App {
        jobs: None,
        moves: None,
        state: Arc::new(std::sync::Mutex::new(state)),
        host: Arc::new(dir.path().join("unused.sock")),
        slots: Arc::new(tokio::sync::Semaphore::new(1)),
        inventory: Default::default(),
        discovered: Default::default(),
    };
    (dir, app, id, sub)
}

fn register_phone(state: &mut crate::state::State) -> (String, Subscription) {
    let (sub, phone) = fixture();
    let invite = state.invite(Some("me".into()), false).unwrap();
    let code = invite.url.split_once("#pair=").unwrap().1;
    let sig: Signature = SigningKey::from(phone).sign(code.as_bytes());
    let cookie = state
        .pair(
            "me",
            code,
            &sub.keys.p256dh,
            &B64.encode(sig.to_bytes()),
            "Phone",
            crate::admin::DeviceClass::Phone,
        )
        .unwrap();
    let id = state.identity("me", &cookie).unwrap();
    state.set_push(&id, Some(sub.clone())).unwrap();
    (id, sub)
}
fn notice(payload: &[u8]) -> Notice {
    Notice {
        bytes: payload.to_owned(),
        goal_bytes: None,
        expires: Instant::now() + Duration::from_secs(300),
        event: Event::Session(NotificationKind::Finished),
        ask: None,
    }
}

#[test]
fn preferences_survive_restart_and_apply_only_to_the_paired_device() {
    let (dir, app, first, _) = paired_phone();
    let second = {
        let mut state = app.state.lock().unwrap();
        let (second, _) = register_phone(&mut state);
        let old = state.push_preferences(&first, None).unwrap();
        assert!(old.subscribed);
        assert_eq!(old.preferences, Preferences::default());
        let finished = PreferenceEdit {
            event: Preference::Finished,
            value: false,
        };
        state.push_preferences(&first, Some(finished)).unwrap();
        let master = |value| PreferenceEdit {
            event: Preference::Enabled,
            value,
        };
        state.push_preferences(&first, Some(master(false))).unwrap();
        assert!(!state.allows_push(&first, Event::Session(NotificationKind::Approval)));
        state.push_preferences(&first, Some(master(true))).unwrap();
        second
    };
    let state = crate::state::State::load(dir.path()).unwrap();
    assert!(state.allows_push(&first, Event::Session(NotificationKind::Approval)));
    assert!(!state.allows_push(&first, Event::Session(NotificationKind::Finished)));
    assert!(state.allows_push(&second, Event::Session(NotificationKind::Finished)));
    assert!(!state.allows_push(&second, Event::GoalReached));
}

#[tokio::test(start_paused = true)]
async fn a_goal_completion_delivers_the_receivers_preferred_event_once() {
    let (_dir, app, id, _sub) = paired_phone();
    let (tx, rx) = mpsc::channel(32);
    let (log, mut attempts) = mpsc::unbounded_channel();
    let sender = tokio::spawn(deliveries(
        app.clone(),
        rx,
        async move |_: &Subscription, _: &SecretKey, bytes: &[u8], _: &str, _: Duration| {
            let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            log.send(value["kind"].as_str().unwrap().to_owned())
                .unwrap();
            DeliveryResult::Done
        },
    ));
    let turn = |seq, goal_reached| {
        let frame = json!({"type":"delta","cursor":{"epoch":"epoch","seq":seq},
            "scope":{"project":"/one","route":{"runtime":"codex","billing":"subscription"}},
            "changes":[{"kind":"notification","thread":"thread","title":"session",
                "event":"finished","goal_reached":goal_reached}]});
        let Notices::Delta(_, mut notes) = notices(
            &serde_json::to_vec(&frame).unwrap(),
            None,
            &mut ActiveAsks::default(),
        )
        .unwrap() else {
            panic!("expected delta")
        };
        assert_eq!(notes.len(), 1);
        notes.pop().unwrap()
    };
    tx.send(turn(1, true)).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), "finished");
    app.state
        .lock()
        .unwrap()
        .push_preferences(
            &id,
            Some(PreferenceEdit {
                event: Preference::GoalReached,
                value: true,
            }),
        )
        .unwrap();
    tx.send(turn(2, true)).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), "goal_reached");
    tx.send(turn(3, false)).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), "finished");
    drop(tx);
    sender.await.unwrap();
    assert!(attempts.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn muted_events_skip_new_deliveries_and_pending_retries() {
    let (_dir, app, id, _sub) = paired_phone();
    let (tx, rx) = mpsc::channel(32);
    let (log, mut attempts) = mpsc::unbounded_channel();
    let sender = tokio::spawn(deliveries(
        app.clone(),
        rx,
        async move |_: &Subscription, _: &SecretKey, bytes: &[u8], _: &str, _: Duration| {
            log.send(bytes.to_owned()).unwrap();
            DeliveryResult::Retry(Duration::ZERO)
        },
    ));
    tx.send(notice(b"before")).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), b"before");
    {
        let mut state = app.state.lock().unwrap();
        state
            .push_preferences(
                &id,
                Some(PreferenceEdit {
                    event: Preference::Finished,
                    value: false,
                }),
            )
            .unwrap();
    }
    tx.send(notice(b"muted")).await.unwrap();
    let mut approved = notice(b"approved");
    approved.event = Event::Session(NotificationKind::Approval);
    tx.send(approved).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), b"approved");
    app.state
        .lock()
        .unwrap()
        .push_preferences(
            &id,
            Some(PreferenceEdit {
                event: Preference::Enabled,
                value: false,
            }),
        )
        .unwrap();
    tokio::time::advance(Duration::from_secs(12)).await;
    tokio::task::yield_now().await;
    assert!(attempts.try_recv().is_err());
    drop(tx);
    sender.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn rate_limited_endpoint_honors_cooldown_across_notices_and_stops_at_its_budget() {
    let (_dir, app, _id, _sub) = paired_phone();
    let (tx, rx) = mpsc::channel(32);
    let (log, mut attempts) = mpsc::unbounded_channel();
    let sender = tokio::spawn(deliveries(
        app,
        rx,
        async move |_: &Subscription, _: &SecretKey, bytes: &[u8], _: &str, _: Duration| {
            log.send((bytes.to_owned(), Instant::now())).unwrap();
            if bytes == b"busy" {
                DeliveryResult::Retry(Duration::from_secs(10))
            } else {
                DeliveryResult::Done
            }
        },
    ));
    let start = Instant::now();
    tx.send(notice(b"busy")).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), (b"busy".to_vec(), start));
    tx.send(notice(b"next")).await.unwrap();
    for n in 1..=3 {
        assert_eq!(
            attempts.recv().await.unwrap(),
            (b"busy".to_vec(), start + Duration::from_secs(n * 10))
        );
    }
    // New deliveries to this endpoint must respect its service cooldown too.
    assert_eq!(
        attempts.recv().await.unwrap(),
        (b"next".to_vec(), start + Duration::from_secs(40))
    );
    tokio::time::advance(Duration::from_secs(400)).await;
    tokio::task::yield_now().await;
    assert!(attempts.try_recv().is_err());
    drop(tx);
    sender.await.unwrap();
}

fn asks_delta(
    asks: &mut ActiveAsks,
    project: &str,
    token: Option<&str>,
    event: Option<NotificationKind>,
) -> Vec<Notice> {
    let mut changes = vec![];
    if let Some(event) = event {
        changes
            .push(json!({"kind":"notification","thread":"thread","title":"session","event":event}));
    }
    let live = token.map(|token| json!({"thread":"thread", "token":token,
        "questions": if event == Some(NotificationKind::Question) { vec!["private question"] } else { vec![] }
    }));
    changes.push(json!({"kind":"asks","asks":live.into_iter().collect::<Vec<_>>()}));
    let frame = json!({"type":"delta","cursor":{"epoch":"epoch","seq":1},
        "scope":{"project":project,"route":{"runtime":"claude","billing":"subscription"}},"changes":changes});
    let Notices::Delta(_, notes) =
        notices(&serde_json::to_vec(&frame).unwrap(), None, asks).unwrap()
    else {
        panic!("expected delta")
    };
    notes
}

#[tokio::test(start_paused = true)]
async fn resolved_asks_and_replaced_subscriptions_cancel_retries() {
    for kind in [NotificationKind::Approval, NotificationKind::Question] {
        let (_dir, app, id, mut sub) = paired_phone();
        let mut asks = ActiveAsks::default();
        let mut original = asks_delta(&mut asks, "/one", Some("first"), Some(kind))
            .pop()
            .unwrap();
        asks_delta(&mut asks, "/two", None, None);
        assert!(original.ask.as_mut().unwrap().has_changed().is_ok());
        let (tx, rx) = mpsc::channel(32);
        let (log, mut attempts) = mpsc::unbounded_channel();
        let sender = tokio::spawn(deliveries(
            app.clone(),
            rx,
            async move |_: &Subscription, _: &SecretKey, bytes: &[u8], _: &str, _: Duration| {
                log.send(bytes.to_owned()).unwrap();
                DeliveryResult::Retry(Duration::from_secs(10))
            },
        ));
        tx.send(original).await.unwrap();
        attempts.recv().await.unwrap();
        // A replacement on the same thread cannot keep the first ask alive.
        let replacement = asks_delta(&mut asks, "/one", Some("second"), Some(kind))
            .pop()
            .unwrap();
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        assert!(attempts.try_recv().is_err());
        tx.send(replacement).await.unwrap();
        attempts.recv().await.unwrap();
        // An endpoint may be renewed with new encryption keys during backoff.
        sub.keys.auth = B64.encode([23; 16]);
        app.state.lock().unwrap().set_push(&id, Some(sub)).unwrap();
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::task::yield_now().await;
        assert!(attempts.try_recv().is_err());
        drop(tx);
        sender.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn timed_out_post_recovers_after_backoff_and_gone_removes_the_subscription() {
    let (_dir, app, id, sub) = paired_phone();
    let (tx, rx) = mpsc::channel(32);
    let (log, mut attempts) = mpsc::unbounded_channel();
    let sender = tokio::spawn(deliveries(app.clone(), rx, {
        let first = std::sync::atomic::AtomicBool::new(true);
        async move |_: &Subscription, _: &SecretKey, _: &[u8], _: &str, _: Duration| {
            log.send(Instant::now()).unwrap();
            if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                std::future::pending().await
            } else {
                DeliveryResult::Gone
            }
        }
    }));
    let start = Instant::now();
    tx.send(notice(b"finished")).await.unwrap();
    assert_eq!(attempts.recv().await.unwrap(), start);
    assert_eq!(
        attempts.recv().await.unwrap(),
        start + Duration::from_secs(9)
    );
    tokio::task::yield_now().await;
    assert!(!app.state.lock().unwrap().is_subscribed(&id, &sub));
    drop(tx);
    sender.await.unwrap();
}

#[test]
fn a_draft_push_targets_one_live_request_without_the_draft_or_approval_token() {
    let mut asks = ActiveAsks::default();
    let mut frame = json!({
        "type":"delta", "cursor":{"epoch":"epoch","seq":1},
        "scope":{"project":"/private/project","route":{"runtime":"codex","billing":"subscription"}},
        "changes":[{"kind":"asks","asks":[{"id":"ox-yolo-18","thread":null,"token":"private-token","detail":"private prompt"}]}]
    });
    let mut deliver = |frame: &serde_json::Value| {
        let Notices::Delta(_, notices) =
            notices(&serde_json::to_vec(frame).unwrap(), None, &mut asks).unwrap()
        else {
            panic!("expected delta")
        };
        notices
    };
    let notice = deliver(&frame).pop().unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&notice.bytes).unwrap();
    assert_eq!(payload["kind"], "approval");
    assert!(
        payload["thread"]
            .as_str()
            .unwrap()
            .starts_with("ask:ox-yolo-18~")
    );
    assert!(!String::from_utf8(notice.bytes).unwrap().contains("private"));
    assert!(deliver(&frame).is_empty());
    frame["changes"][0]["asks"] = json!([]);
    assert!(deliver(&frame).is_empty());
    assert!(notice.ask.unwrap().has_changed().is_err());
}
