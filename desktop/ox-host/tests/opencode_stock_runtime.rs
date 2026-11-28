//! Opt-in end-to-end test driven by desktop/tests/opencode_host_runtime.py.
use ox_host::opencode::{
    Decision, Error, Events, Owner, Profile,
    runtime::{Phase, RouteRuntime, Snapshot},
};
use std::{
    env, fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "stock OpenCode fixture timed out"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn event(stream: &mut Events, kind: &str) {
    loop {
        let event = stream.read_event().unwrap().expect("event stream ended");
        if event.name == kind {
            return;
        }
    }
}

fn text(snapshot: &Snapshot, role: &str, expected: &str) -> usize {
    snapshot
        .transcript
        .items
        .iter()
        .filter(|item| item.role == role && item.text == expected)
        .count()
}

fn terminal(runtime: &mut RouteRuntime, owner: &Owner, id: Uuid) -> Snapshot {
    let mut result = None;
    wait_for(|| {
        let snapshot = runtime.read(owner, id).unwrap();
        if snapshot.session.status != "running" && snapshot.session.status != "needs_input" {
            result = Some(snapshot);
        }
        result.is_some()
    });
    result.unwrap()
}

#[test]
#[ignore = "requires a stock OpenCode binary and the loopback gateway fixture"]
fn reconciles_stock_sessions_approvals_and_restart() {
    let path = |name: &str| PathBuf::from(env::var_os(name).unwrap());
    let ox = path("OX_TEST_OPENCODE_GATEWAY");
    let project = path("OX_TEST_OPENCODE_PROJECT");
    let state = path("OX_TEST_OPENCODE_STATE");
    let owner = Owner::new("test-host".into(), &project, "fixture/local".into()).unwrap();
    let other_route =
        Owner::new("test-host".into(), &project, "fixture/wrong-route".into()).unwrap();
    let other_project = project.parent().unwrap().join("other");
    let wrong_project =
        Owner::new("test-host".into(), &other_project, "fixture/local".into()).unwrap();
    let start =
        || RouteRuntime::start(&ox, Profile::prepare(&state, owner.clone()).unwrap()).unwrap();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let mut runtime = start();
    assert!(RouteRuntime::start(&ox, Profile::prepare(&state, owner.clone()).unwrap()).is_err());
    for (id, title) in [(first, "first"), (second, "second")] {
        runtime.create(&owner, id, title).unwrap();
        runtime.create(&owner, id, title).unwrap();
    }
    assert_eq!(runtime.sessions(&owner).unwrap().len(), 2);
    assert!(matches!(
        runtime.read(&other_route, first),
        Err(Error::OwnerMismatch)
    ));
    assert!(matches!(
        runtime.read(&wrong_project, first),
        Err(Error::OwnerMismatch)
    ));
    assert!(matches!(
        runtime.read(&owner, Uuid::new_v4()),
        Err(Error::UnknownSession)
    ));

    let mut stream = runtime.events(&owner, first).unwrap();
    event(&mut stream, "server.connected");
    assert_eq!(
        runtime.send(&owner, first, 1, "reconnect").unwrap().phase,
        Phase::Pending
    );
    assert!(matches!(
        runtime.send(&owner, first, 1, "replacement"),
        Err(Error::ConflictingSend)
    ));
    assert!(matches!(
        runtime.send(&owner, first, 2, "overlap"),
        Err(Error::TurnInProgress)
    ));
    event(&mut stream, "message.part.delta");
    drop(stream);
    fs::write(project.join("allow-stream"), b"").unwrap();
    let final_turn = terminal(&mut runtime, &owner, first);
    assert_eq!(final_turn.receipt.unwrap().phase, Phase::Completed);
    assert_eq!(text(&final_turn, "assistant", "first restored"), 1);
    assert_eq!(text(&final_turn, "user", "reconnect"), 1);
    let newest = runtime.history(&owner, first, 1, None).unwrap();
    assert_eq!(newest.transcript.items, final_turn.transcript.items[1..]);
    let previous = runtime
        .history(&owner, first, 1, newest.before.as_deref())
        .unwrap();
    assert_eq!(previous.transcript.items, final_turn.transcript.items[..1]);
    assert!(previous.before.is_none());
    assert_eq!(
        runtime.send(&owner, first, 1, "reconnect").unwrap().phase,
        Phase::Completed
    );

    // A project rule allowing Bash cannot bypass the host's session rule.
    for (sequence, prompt, decision) in [
        (2, "allow", Decision::AllowOnce),
        (3, "deny", Decision::Reject),
    ] {
        let mut stream = runtime.events(&owner, first).unwrap();
        event(&mut stream, "server.connected");
        runtime.send(&owner, first, sequence, prompt).unwrap();
        event(&mut stream, "permission.asked");
        drop(stream);
        let snapshot = runtime.read(&owner, first).unwrap();
        assert_eq!(snapshot.session.status, "needs_input");
        assert_eq!(snapshot.asks.len(), 1);
        let ask = &snapshot.asks[0];
        let target = project.join(format!("{prompt}.txt"));
        assert!(!target.exists());
        assert_eq!(
            ask.command.as_ref().unwrap().command,
            format!("printf OK >> {prompt}.txt")
        );
        assert_eq!(
            runtime.read(&owner, first).unwrap().asks[0].token,
            ask.token
        );
        assert!(matches!(
            runtime.answer(&owner, second, &ask.id, &ask.token, decision),
            Err(Error::UnknownPermission)
        ));
        assert!(matches!(
            runtime.answer(&owner, first, &ask.id, "forged", decision),
            Err(Error::UnknownPermission)
        ));
        assert!(matches!(
            runtime.answer(&wrong_project, first, &ask.id, &ask.token, decision),
            Err(Error::OwnerMismatch)
        ));
        runtime
            .answer(&owner, first, &ask.id, &ask.token, decision)
            .unwrap();
        assert!(matches!(
            runtime.answer(&owner, first, &ask.id, &ask.token, decision),
            Err(Error::UnknownPermission)
        ));
        assert!(terminal(&mut runtime, &owner, first).asks.is_empty());
        if prompt == "allow" {
            assert_eq!(fs::read(&target).unwrap(), b"OK");
        } else {
            assert!(!target.exists());
        }
    }

    let mut stream = runtime.events(&owner, first).unwrap();
    event(&mut stream, "server.connected");
    runtime.send(&owner, first, 4, "cancel approval").unwrap();
    event(&mut stream, "permission.asked");
    drop(stream);
    let ask = runtime.read(&owner, first).unwrap().asks.remove(0);
    assert_eq!(
        runtime.stop(&owner, first, 4).unwrap().phase,
        Phase::Stopped
    );
    assert!(runtime.read(&owner, first).unwrap().asks.is_empty());
    assert!(matches!(
        runtime.answer(&owner, first, &ask.id, &ask.token, Decision::AllowOnce),
        Err(Error::UnknownPermission)
    ));
    assert!(!project.join("cancel.txt").exists());
    runtime.send(&owner, first, 5, "cancel approval").unwrap();
    assert_eq!(
        runtime.stop(&owner, first, 5).unwrap().phase,
        Phase::Stopped
    );
    assert!(!project.join("cancel.txt").exists());

    let mut stream = runtime.events(&owner, second).unwrap();
    event(&mut stream, "server.connected");
    runtime.send(&owner, second, 1, "cancel approval").unwrap();
    event(&mut stream, "permission.asked");
    drop(stream);
    let pre_restart = runtime.read(&owner, second).unwrap().asks.remove(0);
    drop(runtime);

    let mut restarted = start();
    assert_eq!(restarted.sessions(&owner).unwrap().len(), 2);
    let ready = restarted.read(&owner, first).unwrap();
    assert_eq!(ready.session.id, first.to_string());
    assert_eq!(ready.receipt.unwrap().phase, Phase::Stopped);
    assert_eq!(text(&ready, "assistant", "first restored"), 1);
    let resumed = restarted.read(&owner, second).unwrap();
    assert_eq!(resumed.receipt.unwrap().phase, Phase::Stopped);
    assert!(resumed.asks.is_empty());
    assert!(matches!(
        restarted.answer(
            &owner,
            second,
            &pre_restart.id,
            &pre_restart.token,
            Decision::AllowOnce
        ),
        Err(Error::UnknownPermission)
    ));
    assert!(!project.join("cancel.txt").exists());
    assert!(matches!(
        restarted.stop(&owner, first, 4),
        Err(Error::ConflictingSend)
    ));
    assert_eq!(
        restarted
            .send(&owner, first, 5, "cancel approval")
            .unwrap()
            .phase,
        Phase::Stopped
    );
    restarted.send(&owner, first, 6, "next").unwrap();
    let ready = terminal(&mut restarted, &owner, first);
    assert_eq!(text(&ready, "assistant", "fresh"), 1, "{ready:#?}");
    assert_eq!(text(&ready, "assistant", "first restored"), 1);
    assert_eq!(ready.receipt.unwrap().phase, Phase::Completed);
    assert!(!project.join("cancel.txt").exists());

    let mut stream = restarted.events(&owner, first).unwrap();
    event(&mut stream, "server.connected");
    restarted.send(&owner, first, 7, "read tool").unwrap();
    event(&mut stream, "permission.asked");
    drop(stream);
    let ask = restarted.read(&owner, first).unwrap().asks.remove(0);
    assert_eq!(ask.title, "Allow read?");
    assert!(ask.detail.contains("test-read.txt"));
    restarted
        .answer(&owner, first, &ask.id, &ask.token, Decision::AllowOnce)
        .unwrap();
    let read = terminal(&mut restarted, &owner, first);
    assert_eq!(read.receipt.unwrap().phase, Phase::Completed);
    assert!(
        read.transcript.items.iter().any(|item| {
            item.role == "tool" && item.text.contains("The fixture read this file.")
        })
    );
    restarted.send(&owner, first, 8, "provider error").unwrap();
    let failed = terminal(&mut restarted, &owner, first);
    assert_eq!(failed.receipt.unwrap().phase, Phase::Failed);
    assert_eq!(failed.session.status, "failed");
}
