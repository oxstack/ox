//! Explicit local enrollment for the stock OpenCode runtime experiment.
mod observer;

use observer::Observer;
use ox_desk_core::{
    host_protocol::{AskView, Scope, SessionView, Timeline},
    links::LinkEvent,
    runtime::Billing,
    store::{
        Profile as StoreProfile, Store,
        opencode::{Command, Event},
    },
};
use ox_host::opencode::{
    Decision, Error, Owner, Profile,
    runtime::{Phase, RouteRuntime},
};
use std::{
    collections::{HashMap, HashSet},
    env, io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, RecvTimeoutError, sync_channel},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const ACTIVE_REFRESH: Duration = Duration::from_millis(250);
const DISPLAY_REFRESH: Duration = Duration::from_millis(50);
const PROJECT: &str = "OX_EXPERIMENTAL_HOSTED_OPENCODE_PROJECT";
const ALIAS: &str = "OX_EXPERIMENTAL_HOSTED_OPENCODE_ALIAS";

pub(super) fn open(
    scope: &Scope,
    host_id: String,
    state: &Path,
    ox: PathBuf,
    receive: impl Fn(LinkEvent) + Send + 'static,
) -> io::Result<Store> {
    let Billing::Api(alias) = &scope.route.billing else {
        return Err(io::Error::other(
            "Hosted OpenCode requires an exact OX API alias",
        ));
    };
    let configured = env::var_os(PROJECT).map(PathBuf::from);
    if configured.as_deref() != Some(scope.project.as_path())
        || env::var(ALIAS).as_deref() != Ok(alias.as_str())
    {
        return Err(io::Error::other(
            "This project and API route are not enrolled in hosted OpenCode",
        ));
    }
    let owner = Owner::new(host_id, &scope.project, alias.clone()).map_err(io::Error::other)?;
    let profile = Profile::prepare(state, owner.clone()).map_err(io::Error::other)?;
    let (tx, rx) = sync_channel(32);
    thread::Builder::new()
        .name("ox-opencode-route".into())
        .spawn(move || {
            let deliver = |event| receive(LinkEvent::OpenCode(event));
            let result = RouteRuntime::start(&ox, profile).and_then(|runtime| {
                Worker {
                    runtime,
                    owner,
                    active: HashSet::new(),
                    cached: HashMap::new(),
                    observer: Observer::default(),
                    deliver: &deliver,
                }
                .run(&rx)
            });
            if let Err(error) = result {
                deliver(Event::Closed(error.to_string()));
            }
        })?;
    Ok(Store::open_opencode(
        StoreProfile {
            project: scope.project.clone(),
            route: scope.route.clone(),
            provider: "opencode".into(),
        },
        tx,
    ))
}

struct Cached {
    session: SessionView,
    timeline: Timeline,
    asks: Vec<AskView>,
    before: Option<String>,
}

struct Worker<'a> {
    runtime: RouteRuntime,
    owner: Owner,
    active: HashSet<Uuid>,
    cached: HashMap<Uuid, Cached>,
    observer: Observer,
    deliver: &'a dyn Fn(Event),
}

impl Worker<'_> {
    fn run(mut self, rx: &Receiver<(String, Command)>) -> Result<(), Error> {
        let rows = self.runtime.catalog(&self.owner)?;
        self.active
            .extend(rows.iter().filter_map(|(id, _, receipt)| {
                receipt
                    .filter(|receipt| in_progress(receipt.phase))
                    .map(|_| *id)
            }));
        (self.deliver)(Event::Ready(
            rows.into_iter().map(|(_, row, _)| row).collect(),
        ));
        let mut next_refresh = Instant::now();
        let mut next_display = next_refresh;
        loop {
            let message = if self.active.is_empty() {
                match rx.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                }
            } else {
                match rx.recv_timeout(
                    next_refresh
                        .min(next_display)
                        .saturating_duration_since(Instant::now()),
                ) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if let Some((key, command)) = message {
                let result = self.command(command).map_err(|error| error.to_string());
                (self.deliver)(Event::Finished { key, result });
            }
            if !self.active.is_empty() && Instant::now() >= next_display {
                if self.refresh_observations()? {
                    next_refresh = Instant::now() + ACTIVE_REFRESH;
                }
                next_display = Instant::now() + DISPLAY_REFRESH;
            }
            if !self.active.is_empty() && Instant::now() >= next_refresh {
                let active: Vec<_> = self.active.iter().copied().collect();
                for id in active {
                    self.refresh(id, false)?;
                }
                next_refresh = Instant::now() + ACTIVE_REFRESH;
            }
        }
        Ok(())
    }

    fn command(&mut self, cmd: Command) -> Result<Option<String>, Error> {
        let owner = &self.owner;
        match cmd {
            Command::Select(id) => {
                self.refresh(id.parse().map_err(|_| Error::UnknownSession)?, false)?
            }
            Command::LoadOlder(id) => {
                self.refresh(id.parse().map_err(|_| Error::UnknownSession)?, true)?
            }
            Command::Send(id, text) => {
                let new = id.is_none();
                let id = id
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| Error::UnknownSession)?
                    .unwrap_or_else(Uuid::new_v4);
                if new {
                    let title: String = text
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("New OpenCode session")
                        .trim()
                        .chars()
                        .take(100)
                        .collect();
                    self.runtime.create(owner, id, &title)?;
                }
                let previous = self
                    .runtime
                    .receipt(owner, id)?
                    .map_or(0, |receipt| receipt.sequence);
                let sequence = previous.checked_add(1).ok_or(Error::ConflictingSend)?;
                if self.observer.poll(&self.runtime, owner, &self.active) {
                    (self.deliver)(Event::StreamLost);
                }
                self.observer.connect(&self.runtime, owner);
                match self.runtime.send(owner, id, sequence, &text) {
                    Ok(_) | Err(Error::AmbiguousSend) => {
                        self.active.insert(id);
                        // The controller fenced the native request on disk. A dropped
                        // snapshot does not authorize another send on this session.
                        let _ = self.refresh(id, false);
                    }
                    Err(error) => {
                        if new {
                            let _ = self.refresh(id, false);
                        }
                        return Err(error);
                    }
                }
                return Ok(new.then(|| id.to_string()));
            }
            Command::Answer(thread, request, token, choice) => {
                let id = thread.parse().map_err(|_| Error::UnknownSession)?;
                let decision = match choice {
                    0 => Decision::AllowOnce,
                    1 => Decision::Reject,
                    _ => return Err(Error::UnknownPermission),
                };
                let result = self.runtime.answer(owner, id, &request, &token, decision);
                self.refresh(id, false)?;
                result?;
            }
            Command::Stop(id, sequence) => {
                let id = id.parse().map_err(|_| Error::UnknownSession)?;
                let result = self.runtime.stop(owner, id, sequence);
                self.refresh(id, false)?;
                result?;
            }
        }
        Ok(None)
    }

    fn refresh_observations(&mut self) -> Result<bool, Error> {
        let disconnected = self.observer.poll(&self.runtime, &self.owner, &self.active);
        if disconnected {
            let active: Vec<_> = self.active.iter().copied().collect();
            for id in active {
                self.refresh(id, false)?;
            }
            (self.deliver)(Event::StreamLost);
        }
        self.observer.connect(&self.runtime, &self.owner);
        let missing = self
            .observer
            .unknown_parts(|id, part| known_part(&self.cached, id, part));
        for id in missing {
            self.refresh(id, false)?;
        }
        self.observer.flush(
            |id, part| known_part(&self.cached, id, part),
            |id, part, delta| {
                (self.deliver)(Event::StreamText {
                    thread: id.to_string(),
                    part: part.to_owned(),
                    delta,
                })
            },
        );
        Ok(disconnected)
    }

    fn refresh(&mut self, id: Uuid, older: bool) -> Result<(), Error> {
        let mut snapshot = self.runtime.read(&self.owner, id)?;
        if snapshot
            .receipt
            .is_some_and(|receipt| in_progress(receipt.phase))
        {
            self.active.insert(id);
        } else {
            self.active.remove(&id);
        }
        let mut before = snapshot.before.take();
        if let Some(cached) = self.cached.get(&id) {
            before = extend_history(&mut snapshot.transcript, before, cached);
        }
        if older && let Some(cursor) = &before {
            let mut page = self.runtime.history(&self.owner, id, 50, Some(cursor))?;
            page.transcript.items.append(&mut snapshot.transcript.items);
            snapshot.transcript.items = page.transcript.items;
            before = page.before;
        }
        snapshot.transcript.detail["older"] = serde_json::json!(before.is_some());
        if self.cached.get(&id).is_some_and(|cached| {
            cached.session == snapshot.session
                && cached.timeline == snapshot.transcript
                && cached.asks == snapshot.asks
                && cached.before == before
        }) {
            return Ok(());
        }
        self.cached.insert(
            id,
            Cached {
                session: snapshot.session.clone(),
                timeline: snapshot.transcript.clone(),
                asks: snapshot.asks.clone(),
                before,
            },
        );
        (self.deliver)(Event::Snapshot {
            session: Box::new(snapshot.session),
            transcript: snapshot.transcript,
            asks: snapshot.asks,
            sequence: snapshot.receipt.map(|receipt| receipt.sequence),
        });
        Ok(())
    }
}

fn known_part(cached: &HashMap<Uuid, Cached>, id: Uuid, part: &str) -> bool {
    cached.get(&id).is_some_and(|cached| {
        cached.timeline.detail["active"]
            .as_str()
            .is_some_and(|turn| {
                cached.timeline.items.iter().rev().any(|p| {
                    p.id == part && p.role == "assistant" && p.turn == turn && p.text.is_empty()
                })
            })
    })
}

fn in_progress(phase: Phase) -> bool {
    matches!(phase, Phase::Pending | Phase::Cancelling)
}

fn extend_history(
    timeline: &mut Timeline,
    before: Option<String>,
    cached: &Cached,
) -> Option<String> {
    if let Some(first) = timeline.items.first()
        && let Some(index) = cached
            .timeline
            .items
            .iter()
            .position(|item| item.id == first.id && item.turn == first.turn)
    {
        timeline
            .items
            .splice(..0, cached.timeline.items[..index].iter().cloned());
        return cached.before.clone();
    }
    before
}
