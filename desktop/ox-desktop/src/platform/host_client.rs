//! The desktop owns only this subscription. Dropping it closes its socket and
//! leaves the daemon, native runtimes and outstanding requests alive.
pub(super) use ox_desk_core::host_protocol::{read_frame, write_frame};
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, ProcessView, Scope, Server, VERSION},
    links::LinkEvent,
    store::{Profile, Route, Store, ThreadRow, replica},
};
use ox_host_service::{Paths, ensure_running, is_running};
#[cfg(test)]
use std::io::BufRead as _;
use std::{
    collections::HashSet,
    io::{self, BufReader, BufWriter, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
#[derive(Clone, Default)]
pub(crate) enum HostLink {
    #[default]
    Local,
    Remote(super::hosts::SavedHost),
}
type StreamHello = (BufReader<UnixStream>, UnixStream, Vec<String>, u32);

impl HostLink {
    pub fn remote(&self) -> Option<&super::hosts::SavedHost> {
        match self {
            Self::Local => None,
            Self::Remote(host) => Some(host),
        }
    }
    pub fn open_shell(&self, project: PathBuf, cx: &mut gpui::App) -> Result<(), String> {
        match self {
            Self::Remote(host) => crate::terminal::open_remote(
                host.clone(),
                ox_host_web::terminal_protocol::OpenTerminal::Shell { project },
                cx,
            ),
            Self::Local => crate::terminal::open(project, None, |_, _| {}, cx),
        }
    }
    pub fn settings(&self) -> Result<crate::settings::Settings, String> {
        self.remote()
            .map_or_else(crate::settings::load, |h| h.snapshot())
    }
    pub fn models(&self) -> Result<Vec<String>, String> {
        self.remote()
            .map_or_else(crate::settings::discover_models, |h| h.models())
    }
    pub fn request<T: serde::de::DeserializeOwned>(
        &self,
        request: crate::settings::protocol::Request,
    ) -> Result<T, String> {
        match self.remote() {
            Some(host) => serde_json::from_value(
                host.settings(serde_json::to_value(request).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string()),
            None => crate::settings::request(request),
        }
    }
    pub fn apply(
        &self,
        request: &crate::settings::protocol::Request,
    ) -> Result<Result<crate::settings::Settings, String>, String> {
        use crate::settings::model::Outcome;
        let Some(host) = self.remote() else {
            return crate::settings::apply(request);
        };
        let reply = host
            .settings(serde_json::to_value(request).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        match serde_json::from_value(reply).map_err(|e| e.to_string())? {
            Outcome::Applied => self.settings().map(Ok),
            Outcome::ChangedOnDisk { revision } => Ok(Err(revision)),
        }
    }
    pub fn state_file(&self, file: &str) -> String {
        match self.remote() {
            Some(host) => format!("{}-{file}", host.id),
            None => file.to_owned(),
        }
    }
    pub(crate) fn worker_hosts(&self, project: PathBuf) -> io::Result<Option<serde_json::Value>> {
        let (mut input, mut socket, caps, _) = self.handshake()?;
        if !caps.iter().any(|c| c == "worker_placement") {
            return Ok(None);
        }
        socket.set_read_timeout(Some(Duration::from_secs(20)))?;
        write_frame(
            &mut socket,
            &Client::WorkerHosts {
                id: "sheet".into(),
                project,
            },
        )?;
        match read_frame(&mut input)? {
            Server::WorkerReply {
                result: Some(value),
                ..
            } => Ok(Some(value)),
            Server::WorkerReply {
                error: Some(error), ..
            }
            | Server::Error { message: error, .. } => Err(io::Error::other(error)),
            _ => Err(io::Error::other(
                "Could not read this project's worker hosts.",
            )),
        }
    }
    pub(crate) fn handoff(
        &self,
        source: Scope,
        start: ox_desk_core::handoff::HandoffStart,
        to: &crate::app::fleet::HandoffDestination,
        on_approval: impl FnMut(),
    ) -> io::Result<(Scope, String)> {
        let target = Scope {
            project: to.project.clone(),
            route: start.route(),
        };
        let key = serde_json::to_string(&(
            self.remote().map(|h| (&h.id, h.pairing_id())),
            to.link.remote().map(|h| h.pairing_id()),
            &source,
            &start,
            &to.id,
            &target,
        ))?;
        let pending = PendingTransfer::reserve(&key)?;
        let mut target_connection = to.link.handoff_connection()?;
        let target_info = handoff_call(
            &mut target_connection,
            Client::InspectHandoffTarget {
                id: pending.id.clone(),
                scope: target.clone(),
            },
            &pending.id,
            || {},
        )?;
        if target_info["host"].as_str() != Some(to.id.as_str())
            || target_info["repository"].as_str() != Some(to.repository.as_str())
        {
            return Err(io::Error::other(
                "The selected host or checkout changed. Choose the destination again.",
            ));
        }
        if let Some(session) = target_info["recovered"].as_str() {
            pending.finish()?;
            return Ok((target, session.to_owned()));
        }
        let mut source_connection = self.handoff_connection()?;
        let request = Client::PrepareRemoteHandoff {
            id: pending.id.clone(),
            scope: source,
            start,
            target_host: to.id.clone(),
            window: target_info["window"].as_u64(),
        };
        let mut packet: ox_desk_core::handoff::HandoffTransfer = serde_json::from_value(
            handoff_call(&mut source_connection, request, &pending.id, || {})?,
        )?;
        if packet.id != pending.id
            || packet.target_host != to.id
            || packet.start.route() != target.route
            || packet.repository != to.repository
        {
            return Err(io::Error::other(
                "The source or destination checkout changed. Choose the destination again.",
            ));
        }
        if !ox_desk_core::host_protocol::project_handoff_transfer(target_connection.3, &mut packet)
        {
            return Err(io::Error::other(
                "Update the destination host to use Antigravity handoffs.",
            ));
        }
        packet.source.history_broker = Some(history::binding(&to.link)?);
        let receipt = history::record(self, &to.link, &packet)?;
        let request = Client::ReceiveRemoteHandoff {
            id: "receive-handoff".into(),
            scope: target.clone(),
            packet: Box::new(packet),
        };
        let result = handoff_call(
            &mut target_connection,
            request,
            "receive-handoff",
            on_approval,
        );
        let result = match result {
            Ok(result) => result,
            Err(e) if e.kind() == io::ErrorKind::Other => {
                let _ = std::fs::remove_file(receipt);
                pending.finish()?;
                return Err(e);
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "{} may already have started the handoff. Retry the same request to reconnect. {e}",
                        to.name
                    ),
                ));
            }
        };
        let session = result["session"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| io::Error::other("The receiving host did not confirm a session."))?;
        pending.finish()?;
        Ok((target, session.to_owned()))
    }
    fn handoff_connection(&self) -> io::Result<StreamHello> {
        let (reader, socket, caps, version) = self.handshake()?;
        if !caps.iter().any(|c| c == "remote_handoff") {
            return Err(io::Error::other(
                "Update this host to use cross-host handoffs.",
            ));
        }
        Ok((reader, socket, caps, version))
    }
    fn handshake(&self) -> io::Result<StreamHello> {
        self.named_handshake().map(|(stream, _)| stream)
    }
    fn named_handshake(&self) -> io::Result<(StreamHello, Option<String>)> {
        match self {
            Self::Local => {
                let paths = Paths::discover()?;
                ensure_running(&paths)?;
                handshake_stream(UnixStream::connect(&paths.socket)?, None)
            }
            Self::Remote(host) => handshake_stream(host.connect()?, Some(&host.id)),
        }
    }
}
/// Keep only the id of an unacknowledged request across window and process restarts.
struct PendingTransfer {
    path: PathBuf,
    id: String,
    _lock: std::fs::File,
}
impl PendingTransfer {
    fn reserve(key: &str) -> io::Result<Self> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Seek};
        use std::os::unix::fs::OpenOptionsExt;
        let dir = ox_desk_core::desktop_behavior::directory()
            .ok_or_else(|| io::Error::other("Desktop state directory unavailable"))?
            .join("pending-handoffs");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{:x}", Sha256::digest(key.as_bytes())));
        let mut file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::other("This handoff is already starting."));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        let mut id = String::new();
        file.read_to_string(&mut id)?;
        if uuid::Uuid::parse_str(&id).is_err() {
            id = uuid::Uuid::new_v4().to_string();
            file.rewind()?;
            file.set_len(0)?;
            file.write_all(id.as_bytes())?;
            file.sync_all()?;
        }
        Ok(Self {
            path,
            id,
            _lock: file,
        })
    }
    fn finish(self) -> io::Result<()> {
        std::fs::remove_file(&self.path)
    }
}
fn handoff_call(
    (reader, socket, _, version): &mut StreamHello,
    request: Client,
    key: &str,
    mut on_approval: impl FnMut(),
) -> io::Result<serde_json::Value> {
    if !request.supported_by(*version) {
        return Err(io::Error::other(
            "Update this host to use Antigravity handoffs.",
        ));
    }
    let timeout = if key == "history" { 60 } else { 300 };
    socket.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    write_frame(socket, &request)?;
    loop {
        match read_frame(reader)? {
            Server::WorkerReply {
                id,
                result: Some(result),
                ..
            } if id == key => {
                if key == "receive-handoff" && result["awaiting_approval"] == true {
                    socket.set_read_timeout(None)?;
                    on_approval();
                } else {
                    return Ok(result);
                }
            }
            Server::WorkerReply {
                id,
                error: Some(error),
                ..
            } if id == key => return Err(io::Error::other(error)),
            Server::Error { id, message } if id.as_deref().is_none_or(|id| id == key) => {
                return Err(io::Error::other(message));
            }
            _ => (),
        }
    }
}
pub(crate) enum IdleReservation {
    Running(UnixStream),
    Stopped(ox_host_service::HostLock),
}

/// Probe the package lock first so normal agent requests continue throughout a
/// background download. The installer rechecks it before committing the switch.
pub(crate) fn reserve_idle_update(store: &std::path::Path) -> io::Result<Option<IdleReservation>> {
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.join("update.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let paths = Paths::discover()?;
    if !is_running(&paths)? {
        return Ok(ox_host_service::reserve_stopped_host(&paths)?.map(IdleReservation::Stopped));
    }
    let (mut input, mut socket, caps, _) = open_handshake(&paths)?;
    if !caps
        .iter()
        .any(|c| c == ox_desk_core::host_protocol::IDLE_UPDATE)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "This session host cannot confirm when agents are idle. Choose Restart to update when you’re ready.",
        ));
    }
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    write_frame(&mut socket, &Client::ReserveIdleUpdate)?;
    match read_frame(&mut input)? {
        Server::IdleUpdate { ready } => Ok(ready.then_some(IdleReservation::Running(socket))),
        Server::Error { message, .. } => Err(io::Error::other(message)),
        _ => Err(io::Error::other(
            "The session host could not confirm whether agents are idle.",
        )),
    }
}
/// Shown when the host is not up. Polls treat it as a quiet state, not an error.
pub(crate) const HOST_NOT_RUNNING: &str = "Session host not running";

/// Connects to a host that is already up and never starts one, so a background poll
/// cannot revive a host the user stopped.
fn connect_existing(paths: &Paths) -> io::Result<BufReader<UnixStream>> {
    if !is_running(paths)? {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            HOST_NOT_RUNNING,
        ));
    }
    open_handshake(paths).map(|(input, _, _, _)| input)
}
fn open_handshake(paths: &Paths) -> io::Result<StreamHello> {
    handshake_stream(UnixStream::connect(&paths.socket)?, None).map(|(stream, _)| stream)
}
fn handshake_stream(
    mut socket: UnixStream,
    remote_id: Option<&str>,
) -> io::Result<(StreamHello, Option<String>)> {
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    write_frame(&mut socket, &Client::Hello { version: VERSION })?;
    let mut input = BufReader::new(socket.try_clone()?);
    let (caps, name, version) = match read_frame(&mut input)? {
        Server::Hello {
            version,
            capabilities,
            host,
            ..
        } if ox_desk_core::host_protocol::supported(version) => {
            if let Some(id) = remote_id
                && host.as_ref().and_then(|h| h.id.as_deref()) != Some(id)
            {
                return Err(io::Error::other(
                    "Host identity changed. Pair this host again.",
                ));
            }
            (capabilities, host.and_then(|h| h.name), version)
        }
        Server::Hello { version, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                ox_desk_core::host_protocol::version_mismatch(version, VERSION),
            ));
        }
        Server::Error { message, .. } => return Err(io::Error::other(message)),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The installed OX host uses an incompatible protocol",
            ));
        }
    };
    socket.set_read_timeout(None)?;
    if remote_id.is_none() {
        declare_surface(&mut socket, &caps)?;
    }
    Ok(((input, socket, caps, version), name))
}
/// Tell a host that negotiates it that a person on OX Desktop uses this connection. An
/// older host rejects the frame, so it is sent only when the hello lists the capability.
fn declare_surface(out: &mut impl Write, caps: &[String]) -> io::Result<()> {
    if caps.iter().any(|c| c == ox_desk_core::surface::CAPABILITY) {
        write_frame(
            out,
            &Client::Surface {
                surface: ox_desk_core::surface::Surface::Desktop,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn open_store(
    project: PathBuf,
    route: Route,
    receive: impl Fn(LinkEvent) + Send + Sync + 'static,
) -> io::Result<Store> {
    HostLink::Local.open_store(project, route, receive)
}
impl HostLink {
    pub(crate) fn open_store(
        &self,
        project: PathBuf,
        route: Route,
        receive: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Store> {
        let project = match self {
            Self::Local => project.canonicalize()?,
            Self::Remote(host) if project.is_absolute() => host.choose(&project, &route)?,
            Self::Remote(_) => {
                return Err(io::Error::other(
                    "Select an absolute project path on the host.",
                ));
            }
        };
        let scope = Scope {
            project: project.clone(),
            route: route.clone(),
        };
        let (mut input, mut socket, caps, version) = self.handshake()?;
        if !ox_desk_core::host_protocol::supports_route(version, &scope.route) {
            return Err(io::Error::other(
                "Update this OX host to open Antigravity sessions.",
            ));
        }
        let placement = caps.iter().any(|c| c == "worker_placement");
        write_frame(
            &mut socket,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: self.remote().is_none(),
            },
        )?;
        let client = getrandom::u64().map_err(io::Error::other)?;
        let (tx, rx) = mpsc::sync_channel::<(String, Action, Option<String>)>(256);
        let receive = Arc::new(receive);
        let writer_scope = scope.clone();
        let fail = receive.clone();
        thread::Builder::new()
            .name("ox-host-write".into())
            .spawn(move || {
                let mut out = BufWriter::new(socket);
                for (id, mut action, approval) in rx {
                    if !placement
                        && let Action::StartWorker(s) = &mut action
                        && s.host.as_deref() == Some("parent")
                        && s.needs.is_none()
                    {
                        s.host = None;
                    }
                    if let Err(e) = write_frame(
                        &mut out,
                        &Client::Act {
                            id,
                            scope: writer_scope.clone(),
                            action,
                            approval,
                        },
                    ) {
                        fail(LinkEvent::Closed {
                            reason: e.to_string(),
                            stderr_tail: vec![],
                        });
                        break;
                    }
                }
                let _ = out.get_ref().shutdown(Shutdown::Both);
            })?;
        thread::Builder::new()
            .name("ox-host-read".into())
            .spawn(move || {
                loop {
                    match read_frame(&mut input) {
                        Ok(frame) => receive(LinkEvent::Host(Box::new(frame))),
                        Err(error) => {
                            receive(LinkEvent::Closed {
                                reason: error.to_string(),
                                stderr_tail: vec![],
                            });
                            break;
                        }
                    }
                }
            })?;
        let seq = AtomicU64::new(0);
        let remote = self.remote().is_some();
        Ok(Store::remote_client(
            Profile {
                project,
                route,
                provider: String::new(),
            },
            move |action, approval| {
                // The native path revalidates slash commands on the host and cannot
                // fall through to a local terminal handoff.
                let action = match action {
                    Action::PaletteCommand(owner, text) if remote => {
                        Action::NativeCommand(owner, text)
                    }
                    other => other,
                };
                let id = format!(
                    "desktop-{client:016x}-{}",
                    seq.fetch_add(1, Ordering::Relaxed)
                );
                tx.try_send((id.clone(), action, approval))
                    .map_err(|e| io::Error::other(e.to_string()))?;
                Ok(id)
            },
        ))
    }
}
pub(crate) type CatalogResult = (Vec<(Scope, ThreadRow)>, Vec<String>);
impl HostLink {
    /// Re-read the target's checkouts and routes when choosing a shared project.
    pub(crate) fn check_project(
        &self,
        path: &std::path::Path,
    ) -> io::Result<ox_desk_core::host_protocol::ProjectDirectory> {
        let (mut input, mut stream, caps, _) = self.handshake()?;
        if !caps.iter().any(|c| c == "project_directory") {
            return Err(io::Error::other("Update the host to use shared projects."));
        }
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        write_frame(
            &mut stream,
            &Client::Discover {
                id: "projects".into(),
                projects: Some(ox_desk_core::host_protocol::ProjectQuery::Checkout(
                    path.into(),
                )),
            },
        )?;
        match read_frame(&mut input)? {
            Server::DiscoveredEnd {
                id,
                directory: Some(dir),
                ..
            } if id == "projects" => Ok(dir),
            Server::Error { message, .. } => Err(io::Error::other(message)),
            _ => Err(io::Error::other(
                "The host did not return its project directory.",
            )),
        }
    }
    pub(crate) fn discover(&self) -> io::Result<CatalogResult> {
        self.discover_rows(false)
    }
    pub(crate) fn discover_rows(&self, available_only: bool) -> io::Result<CatalogResult> {
        let (mut input, mut socket, _, _) = self.handshake()?;
        write_frame(
            &mut socket,
            &Client::Discover {
                id: "catalog".into(),
                projects: None,
            },
        )?;
        let mut rows = Vec::new();
        loop {
            match read_frame(&mut input)? {
                Server::Discovered {
                    id,
                    scope,
                    rows: page,
                } if id == "catalog" => {
                    rows.extend(
                        page.iter()
                            .filter(|view| {
                                !available_only || view.capabilities.iter().any(|c| c == "open")
                            })
                            .filter_map(replica::row)
                            .map(|row| (scope.clone(), row)),
                    );
                }
                Server::DiscoveredEnd { id, errors, .. } if id == "catalog" => {
                    return Ok((rows, errors));
                }
                Server::Error { message, .. } => return Err(io::Error::other(message)),
                _ => (),
            }
        }
    }
}
/// A sidebar reuses its process-list connection. Each new protocol handshake reads
/// the operator's route configuration, which an ordinary process poll does not need.
#[derive(Default)]
pub(crate) struct ProcessMonitor {
    socket: Mutex<Option<BufReader<UnixStream>>>,
}

impl ProcessMonitor {
    pub(crate) fn list(&self, link: &HostLink) -> io::Result<Vec<ProcessView>> {
        self.poll(|| match link {
            HostLink::Local => connect_existing(&Paths::discover()?),
            HostLink::Remote(_) => link.handshake().map(|(input, _, _, _)| input),
        })
    }

    #[cfg(test)]
    fn list_at(&self, paths: &Paths) -> io::Result<Vec<ProcessView>> {
        self.poll(|| connect_existing(paths))
    }
    fn poll(
        &self,
        connect: impl FnOnce() -> io::Result<BufReader<UnixStream>>,
    ) -> io::Result<Vec<ProcessView>> {
        let mut socket = self.socket.lock().unwrap();
        if let Some(mut connection) = socket.take()
            && let Ok(rows) = read_processes(&mut connection)
        {
            *socket = Some(connection);
            return Ok(rows);
        }
        // A host restart closes the old stream. Reconnect only if a host is running.
        let mut connection = connect()?;
        connection
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(15)))?;
        let rows = read_processes(&mut connection)?;
        *socket = Some(connection);
        Ok(rows)
    }
}

fn read_processes(input: &mut BufReader<UnixStream>) -> io::Result<Vec<ProcessView>> {
    write_frame(
        input.get_mut(),
        &Client::ListProcesses {
            id: "processes".into(),
        },
    )?;
    loop {
        match read_frame(input)? {
            Server::Processes { id, processes } if id == "processes" => return Ok(processes),
            Server::Error { id, message } if id.as_deref().is_none_or(|i| i == "processes") => {
                return Err(io::Error::other(message));
            }
            _ => (),
        }
    }
}

/// End the live process of each thread in `scope`. Acts need a subscription to the
/// scope on the same socket, so this subscribes first, then waits for every reply.
impl HostLink {
    pub(crate) fn end_threads(
        &self,
        scope: Scope,
        threads: Vec<String>,
        force: bool,
    ) -> io::Result<()> {
        if threads.is_empty() {
            return Ok(());
        }
        let (mut input, mut socket, _, _) = self.handshake()?;
        write_frame(
            &mut socket,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: false,
            },
        )?;
        let nonce = getrandom::u64().map_err(io::Error::other)?;
        let mut waiting = HashSet::new();
        for (i, thread) in threads.into_iter().enumerate() {
            let id = format!("end-{nonce:016x}-{i}");
            write_frame(
                &mut socket,
                &Client::Act {
                    id: id.clone(),
                    scope: scope.clone(),
                    action: if force {
                        Action::ForceEndSession(thread)
                    } else {
                        Action::EndSession(thread)
                    },
                    approval: None,
                },
            )?;
            waiting.insert(id);
        }
        let mut failure = None;
        while !waiting.is_empty() {
            match read_frame(&mut input)? {
                Server::Ack { id } => {
                    waiting.remove(&id);
                }
                Server::Error {
                    id: Some(id),
                    message,
                } if waiting.remove(&id) => {
                    failure.get_or_insert(message);
                }
                Server::Error { id: None, message } => return Err(io::Error::other(message)),
                _ => (),
            }
        }
        failure.map_or(Ok(()), |message| Err(io::Error::other(message)))
    }
}
mod catalog;
mod history;
mod session_moves;
pub(crate) use catalog::{CatalogEvent, CatalogWatch};
pub(crate) use history::clean_handoff_pairings;
pub(crate) use session_moves::{MoveDestination, MovePhase, MoveStatus};

#[cfg(test)]
mod process_monitor_tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn the_desktop_declares_itself_only_to_a_host_that_negotiates_it() {
        let mut out = Vec::new();
        declare_surface(&mut out, &["snapshot".into()]).unwrap();
        assert!(out.is_empty(), "an older host would reject the frame");
        declare_surface(&mut out, &[ox_desk_core::surface::CAPABILITY.into()]).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"type\":\"surface\",\"surface\":\"desktop\"}\n"
        );
    }

    #[test]
    fn adjacent_releases_exchange_hello_then_session_frames_on_the_selected_protocol() {
        // The previous release used v1 and returned no stable ID. A future release
        // selects our maximum and may advertise new capabilities and metadata.
        let hosts = [
            (
                None,
                serde_json::json!({"version":"0.1", "epoch":"old"}),
                vec!["snapshot"],
            ),
            (
                Some("stable-id"),
                serde_json::json!({"version":"next", "epoch":"new", "id":"stable-id", "load":0.5}),
                vec!["snapshot", "future_feature"],
            ),
        ];
        for (id, info, caps) in hosts {
            let (client, mut host) = UnixStream::pair().unwrap();
            let peer = thread::spawn(move || {
                let mut input = BufReader::new(host.try_clone().unwrap());
                let read = |input: &mut BufReader<UnixStream>| {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    serde_json::from_str::<Client>(&line).unwrap()
                };
                let Client::Hello { version } = read(&mut input) else {
                    panic!("expected hello")
                };
                let maximum = if id.is_some() { VERSION + 1 } else { 1 };
                writeln!(
                    host,
                    "{}",
                    serde_json::json!({"type":"hello", "version":maximum.min(version),
                    "routes":[], "projects":[], "capabilities":caps,"host":info})
                )
                .unwrap();
                assert!(matches!(
                    read(&mut input),
                    Client::Subscribe {
                        scope: None,
                        terminal: false,
                        ..
                    }
                ));
                writeln!(host, "{}", serde_json::json!({"type":"snapshot","cursor":{"epoch":"e","seq":0},"scopes":[]})).unwrap();
            });
            let ((mut input, mut out, _, _), _) = handshake_stream(client, id).unwrap();
            write_frame(
                &mut out,
                &Client::Subscribe {
                    scope: None,
                    after: None,
                    terminal: false,
                },
            )
            .unwrap();
            assert!(matches!(
                read_frame(&mut input).unwrap(),
                Server::Snapshot { .. }
            ));
            peer.join().unwrap();
        }
    }

    #[test]
    fn polls_reuse_the_handshake_and_reconnect_after_host_exit() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            socket: root.path().join("host.sock"),
            state_dir: root.path().to_owned(),
        };
        let monitor = ProcessMonitor::default();
        let start = |polls| {
            let listener = UnixListener::bind(&paths.socket).unwrap();
            thread::spawn(move || {
                // connect_existing checks that the socket belongs to a live host.
                drop(listener.accept().unwrap());
                let (mut out, _) = listener.accept().unwrap();
                out.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut input = BufReader::new(out.try_clone().unwrap());
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert!(matches!(
                    serde_json::from_str(&line),
                    Ok(Client::Hello { version: VERSION })
                ));
                writeln!(
                    out,
                    r#"{{"type":"hello","version":1,"capabilities":[],"routes":[],"projects":[]}}"#
                )
                .unwrap();
                for _ in 0..polls {
                    line.clear();
                    input.read_line(&mut line).unwrap();
                    assert!(matches!(
                        serde_json::from_str(&line),
                        Ok(Client::ListProcesses { .. })
                    ));
                    writeln!(
                        out,
                        r#"{{"type":"processes","id":"processes","processes":[]}}"#
                    )
                    .unwrap();
                }
            })
        };
        let first = start(2);
        assert!(monitor.list_at(&paths).unwrap().is_empty());
        assert!(monitor.list_at(&paths).unwrap().is_empty());
        first.join().unwrap();
        std::fs::remove_file(&paths.socket).unwrap();
        assert_eq!(
            monitor.list_at(&paths).unwrap_err().kind(),
            io::ErrorKind::NotConnected
        );
        let next = start(1);
        assert!(monitor.list_at(&paths).unwrap().is_empty());
        next.join().unwrap();
    }
}
