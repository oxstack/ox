#[path = "project_picker.rs"]
mod project_picker;
use crate::{
    admin::{Reply, Request},
    bridge, push,
    state::{self, Purpose, State},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, State as Extract, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Request as HttpRequest, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    fs, io,
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, UnixListener, UnixStream},
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};

#[derive(Clone)]
pub(crate) struct App {
    pub state: Arc<Mutex<State>>,
    pub host: Arc<PathBuf>,
    pub inventory: Arc<crate::inventory::Cache>,
    pub slots: Arc<Semaphore>,
    pub jobs: Option<crate::linked_jobs::Dispatch>,
    pub moves: Option<crate::move_transport::Receiver>,
    pub discovered: Arc<Mutex<std::collections::HashSet<ox_desk_core::host_protocol::Scope>>>,
}
#[path = "key_transfer.rs"]
mod key_transfer;
#[path = "operator.rs"]
mod operator;
#[path = "operator_handoff.rs"]
mod operator_handoff;
#[path = "remote_settings.rs"]
mod remote_settings;
#[path = "weather.rs"]
mod weather;
#[path = "worker_links.rs"]
mod worker_links;
pub(crate) fn background_keychain() -> Result<(), &'static str> {
    // This runs inside ox-host, which has no window to answer Keychain prompts.
    #[cfg(target_os = "macos")]
    {
        use security_framework::os::macos::keychain::{KeychainUserInteractionLock, SecKeychain};
        static NONINTERACTIVE: std::sync::OnceLock<Option<KeychainUserInteractionLock>> =
            std::sync::OnceLock::new();
        if NONINTERACTIVE
            .get_or_init(|| SecKeychain::disable_user_interaction().ok())
            .is_none()
        {
            return Err("The background host could not open Keychain without a prompt.");
        }
    }
    Ok(())
}
type ErrCode = StatusCode;
const DENY: StatusCode = StatusCode::FORBIDDEN;
pub fn run(state_dir: PathBuf, host_socket: PathBuf) -> io::Result<()> {
    run_with_jobs(state_dir, host_socket, None)
}
pub fn run_with_jobs(
    state_dir: PathBuf,
    host_socket: PathBuf,
    jobs: Option<crate::linked_jobs::Dispatch>,
) -> io::Result<()> {
    run_with_receivers(state_dir, host_socket, jobs, None)
}
pub fn run_with_receivers(
    state_dir: PathBuf,
    host_socket: PathBuf,
    jobs: Option<crate::linked_jobs::Dispatch>,
    moves: Option<crate::move_transport::Receiver>,
) -> io::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(state_dir, host_socket, jobs, moves))
}
async fn serve(
    dir: PathBuf,
    socket: PathBuf,
    jobs: Option<crate::linked_jobs::Dispatch>,
    moves: Option<crate::move_transport::Receiver>,
) -> io::Result<()> {
    let store = State::load(&dir)?;
    crate::worker_checkout::receiver::recover(&store.worker_directory())?;
    if moves.is_some() {
        state::move_staging::recover(&dir)?;
    }
    let app = App {
        jobs,
        moves,
        state: Arc::new(Mutex::new(store)),
        host: Arc::new(socket),
        slots: Arc::new(Semaphore::new(64)),
        inventory: Default::default(),
        discovered: Default::default(),
    };
    let local = crate::admin::socket_path(&dir);
    state::private_dir(local.parent().expect("admin socket directory"))?;
    if let Ok(meta) = fs::symlink_metadata(&local) {
        if !meta.file_type().is_socket()
            || meta.uid() != unsafe { libc::geteuid() }
            || std::os::unix::net::UnixStream::connect(&local).is_ok()
        {
            return Err(io::Error::other("Remote administration socket is occupied"));
        }
        fs::remove_file(&local)?;
    }
    let admin = UnixListener::bind(&local)?;
    fs::set_permissions(&local, fs::Permissions::from_mode(0o600))?;
    let mut listener = None;
    let prior = app.state.lock().unwrap().configuration();
    if let Some((origin, port)) = prior
        && let Ok((_, active)) = listen(app.clone(), origin, port).await
    {
        listener = Some(active);
    }
    tokio::spawn(push::watch(app.clone()));
    let mut staged: Option<Staged> = None;
    let (released, mut ended) = tokio::sync::mpsc::unbounded_channel::<String>();
    loop {
        let mut stream = tokio::select! {
            biased;
            Some(ticket) = ended.recv() => {
                if staged.as_ref().is_some_and(|s| s.ticket == ticket) { staged = None; }
                continue;
            }
            result = admin.accept() => result?.0,
        };
        if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let Ok(Ok(req)) =
            tokio::time::timeout(Duration::from_secs(3), read_request(&mut stream)).await
        else {
            continue;
        };
        if matches!(req, Request::Update { .. } | Request::WorkerLinks { .. }) {
            let active = app.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || -> io::Result<Reply> {
                    let mut reply = active.state.lock().unwrap().reply(None);
                    match req {
                        Request::Update { command } => {
                            reply.update = Some(crate::host_updates::control(command)?)
                        }
                        Request::WorkerLinks { command } => {
                            reply.worker_links =
                                Some(worker_links::execute(&active, None, command)?)
                        }
                        _ => unreachable!(),
                    }
                    Ok(reply)
                })
                .await
                .map_err(|_| "Host settings service stopped".to_owned())
                .and_then(|s| s.map_err(|e| e.to_string()));
                if let Ok(mut bytes) = serde_json::to_vec(&result) {
                    bytes.push(b'\n');
                    let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(&bytes))
                        .await;
                }
            });
            continue;
        }
        let result = administer(&app, &mut listener, &mut staged, req)
            .await
            .map_err(|e| e.to_string());
        let lease = result.as_ref().ok().and_then(|r| r.reservation.clone());
        if let Ok(mut bytes) = serde_json::to_vec(&result) {
            bytes.push(b'\n');
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(&bytes)).await;
        }
        if let Some(ticket) = lease {
            let released = released.clone();
            tokio::spawn(async move {
                let _ = stream.read(&mut [0u8]).await;
                let _ = released.send(ticket);
            });
        }
    }
}
async fn read_request(stream: &mut UnixStream) -> io::Result<Request> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take(16 * 1024 + 1))
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() > 16 * 1024 || !bytes.ends_with(b"\n") {
        return Err(io::Error::other("Invalid request"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
struct Listener {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}
async fn shutdown(listener: &mut Option<Listener>) {
    if let Some(l) = listener.take() {
        let _ = l.stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(1), l.task).await;
    }
}
enum ServeChange {
    Enable {
        origin: String,
        socket: Option<TcpListener>,
    },
    Remove,
}
struct Staged {
    ticket: String,
    change: ServeChange,
}
async fn administer(
    app: &App,
    listener: &mut Option<Listener>,
    staged: &mut Option<Staged>,
    req: Request,
) -> io::Result<Reply> {
    let mut pairing = None;
    let mut reservation = None;
    if staged.is_some()
        && matches!(
            req,
            Request::Enable { .. }
                | Request::Disable
                | Request::Reserve { .. }
                | Request::ReserveRemoval { .. }
        )
    {
        return Err(io::Error::other(
            "Another Serve setup is already in progress.",
        ));
    }
    match req {
        Request::WorkerAccess {
            id,
            before,
            may_launch,
            ceiling,
            may_sync_memory,
            may_accept_moves,
        } => app.state.lock().unwrap().edit_worker(
            &id,
            &before,
            may_launch,
            ceiling,
            may_sync_memory,
            may_accept_moves,
        )?,
        Request::Update { .. } | Request::WorkerLinks { .. } => {
            unreachable!("handled before Serve administration")
        }
        Request::HostProfile { edit } => {
            let state = app.state.lock().unwrap();
            let mut reply = state.reply(None);
            reply.profile = Some(state.host_profile(edit)?);
            return Ok(reply);
        }
        Request::Status => {}
        Request::WorkerInvitation { login } => {
            if listener.is_none() || staged.is_some() {
                return Err(io::Error::other(
                    "Enable remote access before inviting a worker host",
                ));
            }
            pairing = Some(app.state.lock().unwrap().local_worker_invite(login)?);
        }
        Request::OpenWindow { login } => {
            if listener.is_none() || staged.is_some() {
                return Err(io::Error::other(
                    "Enable remote access before opening a pairing window",
                ));
            }
            app.state.lock().unwrap().open_window(login)?;
        }
        Request::CloseWindow => app.state.lock().unwrap().close_window(),
        Request::ApprovePending { id } => app
            .state
            .lock()
            .unwrap()
            .approve_pending(&id)
            .ok_or_else(|| io::Error::other("That pairing request expired."))?,
        Request::DenyPending { id } => app.state.lock().unwrap().deny_pending(&id),
        Request::Pair { login, terminal } => {
            if listener.is_none() || staged.is_some() {
                return Err(io::Error::other(
                    "Enable the local remote access listener first",
                ));
            }
            pairing = Some(app.state.lock().unwrap().invite(login, terminal)?);
        }
        Request::TerminalGrant { id, enabled } => {
            app.state.lock().unwrap().terminal_grant(&id, enabled)?
        }
        Request::Revoke { id } => app.state.lock().unwrap().revoke(&id)?,
        Request::Disable => {
            app.state.lock().unwrap().set_origin(None, 0)?;
            shutdown(listener).await;
        }
        Request::Reserve { origin, port } => {
            let origin =
                state::origin(&origin).ok_or_else(|| io::Error::other("Invalid Serve origin"))?;
            let socket = if listener.is_some() {
                if app.state.lock().unwrap().configuration() != Some((origin.clone(), port)) {
                    return Err(io::Error::other(
                        "Disable the existing Serve address first.",
                    ));
                }
                None
            } else {
                Some(bind(port).await?)
            };
            let ticket = state::random();
            reservation = Some(ticket.clone());
            *staged = Some(Staged {
                ticket,
                change: ServeChange::Enable { origin, socket },
            });
        }
        Request::ReserveRemoval {
            origin,
            expected_enabled,
        } => {
            // Retain the existing authenticated listener until Serve confirms its mapping is gone.
            let origin =
                state::origin(&origin).ok_or_else(|| io::Error::other("Invalid Serve origin"))?;
            let prior = app.state.lock().unwrap().configuration();
            if listener.is_some() != expected_enabled
                || prior.is_some_and(|(active, _)| active != origin)
            {
                return Err(io::Error::other(
                    "Remote access changed. Refresh before disabling it.",
                ));
            }
            let ticket = state::random();
            reservation = Some(ticket.clone());
            *staged = Some(Staged {
                ticket,
                change: ServeChange::Remove,
            });
        }
        Request::Commit { ticket } => {
            if staged.as_ref().is_none_or(|s| s.ticket != ticket) {
                return Err(io::Error::other(
                    "The Serve setup expired or was cancelled.",
                ));
            }
            match staged.take().unwrap().change {
                ServeChange::Enable {
                    origin,
                    socket: Some(socket),
                } => {
                    let port = socket.local_addr()?.port();
                    app.state
                        .lock()
                        .unwrap()
                        .set_origin(Some(origin.clone()), port)?;
                    *listener = Some(start_listener(app.clone(), origin, socket));
                }
                ServeChange::Enable { socket: None, .. } => {}
                ServeChange::Remove => {
                    app.state.lock().unwrap().set_origin(None, 0)?;
                    shutdown(listener).await;
                }
            }
        }
        Request::Enable { origin, port } => {
            let origin = state::origin(&origin)
                .ok_or_else(|| io::Error::other("Enter an HTTPS Tailscale Serve hostname"))?;
            let existing = app.state.lock().unwrap().configuration();
            if existing != Some((origin.clone(), port)) || listener.is_none() {
                if listener.is_some() {
                    return Err(io::Error::other(
                        "Disable the current Serve address before changing it",
                    ));
                }
                let (port, l) = listen(app.clone(), origin.clone(), port).await?;
                if let Err(e) = app.state.lock().unwrap().set_origin(Some(origin), port) {
                    let _ = l.stop.send(());
                    return Err(e);
                }
                *listener = Some(l);
            }
        }
    }
    let mut reply = app.state.lock().unwrap().reply(pairing);
    reply.enabled = listener.is_some();
    reply.reservation = reservation;
    Ok(reply)
}
/// The only way the remote listener opens a TCP socket. Tailscale Serve is the sole
/// path in, so the address is loopback whatever origin or port is configured.
async fn bind(port: u16) -> io::Result<TcpListener> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AddrInUse {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "Local listener port {port} is already in use on 127.0.0.1. Run `ox host pair --listen-port <port>` to pick another local port."
                    ),
                )
            } else {
                e
            }
        })?;
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(io::Error::other("Remote listener must bind to loopback"));
    }
    Ok(listener)
}
async fn listen(app: App, origin: String, port: u16) -> io::Result<(u16, Listener)> {
    let listener = bind(port).await?;
    let actual = listener.local_addr()?.port();
    Ok((actual, start_listener(app, origin, listener)))
}
fn start_listener(app: App, origin: String, listener: TcpListener) -> Listener {
    let router = Router::new()
        .route(
            "/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_bytes!("../assets/index.html"),
                )
            }),
        )
        .route(
            "/composer-tools.js",
            get(|| async {
                asset(
                    "text/javascript",
                    include_bytes!("../assets/composer-tools.js"),
                )
            }),
        )
        .route(
            "/activity.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/activity.js")) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/app.js")) }),
        )
        .route(
            "/wire.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/wire.js")) }),
        )
        .route(
            "/settings.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/settings.js")) }),
        )
        .route(
            "/settings.css",
            get(|| async { asset("text/css", include_bytes!("../assets/settings.css")) }),
        )
        .route(
            "/sky.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/sky.js")) }),
        )
        .route(
            "/controls.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/controls.js")) }),
        )
        .route(
            "/move-controls.js",
            get(|| async {
                asset(
                    "text/javascript",
                    include_bytes!("../assets/move-controls.js"),
                )
            }),
        )
        .route(
            "/model.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/model.js")) }),
        )
        .route(
            "/pair-camera.js",
            get(|| async {
                asset(
                    "text/javascript",
                    include_bytes!("../assets/pair-camera.js"),
                )
            }),
        )
        .route(
            "/vendor/marked-18.3.0.js",
            get(|| async {
                asset(
                    "text/javascript",
                    include_bytes!("../assets/vendor/marked-18.3.0.js"),
                )
            }),
        )
        .route(
            "/vendor/jsQR-1.4.0.js",
            get(|| async {
                asset(
                    "text/javascript",
                    include_bytes!("../assets/vendor/jsQR-1.4.0.js"),
                )
            }),
        )
        .route(
            "/vendor/jsQR.LICENSE",
            get(|| async {
                asset(
                    "text/plain",
                    include_bytes!("../assets/vendor/jsQR.LICENSE"),
                )
            }),
        )
        .route(
            "/device.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/device.js")) }),
        )
        .route(
            "/style.css",
            get(|| async { asset("text/css", crate::cartridges::stylesheet()) }),
        )
        .route(
            "/sw.js",
            get(|| async {
                const SCRIPT: &str = concat!(
                    "const shell = \"ox-remote-shell-",
                    env!("OX_PHONE_SHELL_REVISION"),
                    "\";\n",
                    include_str!("../assets/sw.js"),
                );
                asset("text/javascript", SCRIPT.as_bytes())
            }),
        )
        .route(
            "/marquee.png",
            get(|| async { asset("image/png", include_bytes!("../assets/marquee.png")) }),
        )
        .route(
            "/horizon.png",
            get(|| async { asset("image/png", include_bytes!("../assets/horizon.png")) }),
        )
        .route(
            "/cartridges.png",
            get(|| async { asset("image/png", include_bytes!("../assets/cartridges.png")) }),
        )
        .route(
            "/icon.png",
            get(|| async { asset("image/png", include_bytes!("../assets/icon.png")) }),
        )
        .route(
            "/icon-192.png",
            get(|| async { asset("image/png", include_bytes!("../assets/icon-192.png")) }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async {
                asset(
                    "application/manifest+json",
                    include_bytes!("../assets/manifest.json"),
                )
            }),
        )
        .route(
            "/api/settings",
            post(remote_settings::settings).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/api/pair", post(pair))
        .route("/api/pair/window", post(pair_window))
        .route("/api/pair/window/status", post(pair_window_status))
        .route("/api/pair/window/poll", post(pair_window_poll))
        .route("/api/pair/window/cancel", post(pair_window_cancel))
        .route("/api/challenge", post(challenge))
        .route("/api/worker-link", post(worker_link))
        .route(
            "/api/worker-job",
            post(crate::linked_jobs::handle).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/api/memory/sync",
            post(crate::memory_sync::handle).layer(DefaultBodyLimit::max(
                crate::memory_sync::MAX_SYNC_BODY_BYTES,
            )),
        )
        .route(
            "/api/worker-checkout",
            get(crate::worker_checkout::receiver::upgrade),
        )
        .route(
            "/api/worker-result",
            get(crate::worker_result::transport::upgrade),
        )
        .route(
            "/api/move-reserve",
            post(crate::move_transport::reserve).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/api/move-stage", get(crate::move_transport::prepare))
        .route(
            "/api/move-activate",
            post(crate::move_transport::activate).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/api/move-cancel",
            post(crate::move_transport::cancel).layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/api/push", post(subscribe))
        .route("/api/weather", post(weather::weather))
        .route("/api/operator", get(operator::upgrade))
        .route(
            "/operator.js",
            get(|| async { asset("text/javascript", include_bytes!("../assets/operator.js")) }),
        )
        .route("/api/socket", get(upgrade))
        .layer(DefaultBodyLimit::max(8192))
        .layer(middleware::from_fn_with_state(origin, guard))
        .with_state(app);
    let (stop, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = rx.await;
        })
        .await;
    });
    Listener { stop, task }
}
fn asset(kind: &'static str, bytes: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, kind)], bytes).into_response()
}
fn one<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    let mut values = headers.get_all(key).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}
fn login(headers: &HeaderMap) -> Option<&str> {
    one(headers, "tailscale-user-login").filter(|s| {
        !s.is_empty() && s.len() <= 254 && !s.bytes().any(|b| b.is_ascii_control() || b == b' ')
    })
}
/// Tailscale marks requests that arrive from the public internet through Funnel.
const FUNNEL: &str = "tailscale-funnel-request";
const FUNNEL_REFUSED: &str =
    "OX remote access is private to your tailnet. Turn off Tailscale Funnel for this port.";
/// Identity headers count only on requests Tailscale Serve forwarded over loopback to
/// the configured origin. Funnel traffic comes from the public internet and is refused.
fn permitted(origin: &str, peer: SocketAddr, path: &str, headers: &HeaderMap) -> bool {
    peer.ip().is_loopback()
        && !headers.contains_key(FUNNEL)
        && login(headers).is_some()
        && one(headers, "host") == origin.strip_prefix("https://")
        && (!path.starts_with("/api/") || one(headers, "origin") == Some(origin))
}
async fn guard(
    Extract(origin): Extract<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: HttpRequest<Body>,
    next: Next,
) -> Response {
    if !permitted(&origin, peer, req.uri().path(), req.headers()) {
        if req.headers().contains_key(FUNNEL) {
            return (DENY, FUNNEL_REFUSED).into_response();
        }
        return DENY.into_response();
    }
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    for (k, v) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        (
            "permissions-policy",
            "camera=(self), microphone=(), geolocation=()",
        ),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    let ws = origin.replacen("https://", "wss://", 1);
    let csp = format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; media-src 'self' blob:; connect-src 'self' {ws}; worker-src 'self'; manifest-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'"
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp).expect("validated Serve origin"),
    );
    response
}
const COOKIE: &str = "__Host-ox-device";
fn token(h: &HeaderMap) -> Result<&str, ErrCode> {
    let cookie = one(h, "cookie").ok_or(DENY)?;
    let mut values = cookie
        .split(';')
        .filter_map(|v| v.trim().split_once('='))
        .filter(|(k, _)| *k == COOKIE);
    let token = values.next().ok_or(DENY)?.1;
    if values.next().is_some() {
        return Err(DENY);
    }
    Ok(token)
}
fn device(app: &App, h: &HeaderMap) -> Result<String, ErrCode> {
    let id = paired_identity(app, h)?;
    if app.state.lock().unwrap().device_class(&id) == Some(crate::admin::DeviceClass::Host) {
        return Err(DENY);
    }
    Ok(id)
}
pub(crate) fn paired_identity(app: &App, h: &HeaderMap) -> Result<String, ErrCode> {
    let token = token(h)?;
    app.state
        .lock()
        .unwrap()
        .identity(login(h).ok_or(DENY)?, token)
        .ok_or(DENY)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    #[serde(default)]
    class: crate::admin::DeviceClass,
    source: Option<String>,
    code: String,
    key: String,
    signature: String,
    label: String,
}
async fn pair(
    Extract(app): Extract<App>,
    h: HeaderMap,
    Json(p): Json<Pair>,
) -> Result<Response, ErrCode> {
    let token = app
        .state
        .lock()
        .unwrap()
        .pair(
            login(&h).ok_or(DENY)?,
            &p.code,
            &p.key,
            &p.signature,
            &p.label,
            state::Enrollment {
                class: p.class,
                source: p.source.as_deref(),
            },
        )
        .ok_or(DENY)?;
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "{COOKIE}={token}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
                state::INACTIVE
            ),
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowPair {
    #[serde(default)]
    class: crate::admin::DeviceClass,
    source: Option<String>,
    key: String,
    label: String,
}
/// Enrol through an open pairing window. The operator opened the window on
/// this host; the caller proves nothing but that Serve authenticated it.
async fn pair_window(
    Extract(app): Extract<App>,
    h: HeaderMap,
    Json(p): Json<WindowPair>,
) -> Result<Response, ErrCode> {
    let request = app
        .state
        .lock()
        .unwrap()
        .request_window(
            login(&h).ok_or(DENY)?,
            &p.key,
            &p.label,
            state::Enrollment {
                class: p.class,
                source: p.source.as_deref(),
            },
        )
        .ok_or(DENY)?;
    // Accepted by the host, not by the operator: the device waits for the
    // decision on this host and collects its credential from the poll.
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"request": request.id, "expires_at": request.expires_at})),
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowPoll {
    request: String,
}
/// The waiting device's own view of its request. The request id is its
/// secret, so no cookie is needed yet.
async fn pair_window_poll(
    Extract(app): Extract<App>,
    Json(p): Json<WindowPoll>,
) -> Result<Response, ErrCode> {
    let state = app.state.lock().unwrap().poll_window(&p.request);
    match state {
        state::PollState::Pending => Ok(StatusCode::ACCEPTED.into_response()),
        state::PollState::Approved(token) => Ok((
            [(
                header::SET_COOKIE,
                format!(
                    "{COOKIE}={token}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
                    state::INACTIVE
                ),
            )],
            StatusCode::NO_CONTENT,
        )
            .into_response()),
        state::PollState::Unknown => Err(DENY),
    }
}
/// What another host may learn about this one before it pairs: whether a
/// window is open. The operator's chosen login stays private.
async fn pair_window_status(Extract(app): Extract<App>) -> Json<serde_json::Value> {
    let state = app.state.lock().unwrap();
    let name = state.host_profile(None).map(|p| p.name).unwrap_or_default();
    let status = match state.window() {
        Some(window) => serde_json::json!({
            "open": true,
            "expires_at": window.expires_at,
            "paired": window.paired,
            "name": name,
        }),
        None => serde_json::json!({"open": false}),
    };
    Json(status)
}
/// The asking device stopped waiting; its request id is its own secret.
async fn pair_window_cancel(Extract(app): Extract<App>, Json(p): Json<WindowPoll>) -> StatusCode {
    app.state.lock().unwrap().cancel_window(&p.request);
    StatusCode::NO_CONTENT
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    purpose: Purpose,
}
async fn challenge(
    Extract(app): Extract<App>,
    h: HeaderMap,
    Json(c): Json<Challenge>,
) -> Result<Response, ErrCode> {
    let id = paired_identity(&app, &h)?;
    let mut s = app.state.lock().unwrap();
    if (s.device_class(&id) == Some(crate::admin::DeviceClass::Host))
        != (c.purpose == Purpose::Worker)
    {
        return Err(DENY);
    }
    let terminal = s.terminal_generation(&id).is_some();
    if c.purpose == Purpose::Terminal && !terminal {
        return Err(DENY);
    }
    let nonce = s
        .challenge(&id, c.purpose)
        .ok_or(StatusCode::TOO_MANY_REQUESTS)?;
    let vapid = if c.purpose == Purpose::Push {
        Some(
            s.public_key()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        )
    } else {
        None
    };
    let cookie = format!(
        "{COOKIE}={}; Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        token(&h)?,
        state::INACTIVE
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"nonce":nonce,"vapid":vapid,"terminal":terminal})),
    )
        .into_response())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Proof {
    pub nonce: String,
    pub signature: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerEnvelope {
    proof: Proof,
    request: String,
}

async fn worker_link(
    Extract(app): Extract<App>,
    headers: HeaderMap,
    Json(body): Json<WorkerEnvelope>,
) -> Result<Json<serde_json::Value>, ErrCode> {
    let id = paired_identity(&app, &headers)?;
    #[derive(Deserialize)]
    #[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
    enum Call {
        Info,
        Inventory {
            project: Option<String>,
            #[serde(default)]
            fresh: bool,
        },
        RevokeSelf,
    }
    let call = serde_json::from_str(&body.request).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (project, fresh) = {
        let mut state = app.state.lock().unwrap();
        if !state.prove_worker(&id, &body.proof.nonce, &body.proof.signature, &body.request) {
            return Err(DENY);
        }
        match call {
            Call::Inventory { project, fresh } => (project, fresh),
            Call::Info => {
                let profile = state
                    .host_profile(None)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                return Ok(Json(
                    json!({"id":state.host_id().map_err(|_|StatusCode::INTERNAL_SERVER_ERROR)?, "name":profile.name, "labels":profile.labels,
                "policy":state.mode_policy(None).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?, "grant":state.worker_grant(&id)}),
                ));
            }
            Call::RevokeSelf => {
                state
                    .revoke(&id)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                return Ok(Json(json!({"revoked":true})));
            }
        }
    };
    if project.as_ref().is_some_and(|id| {
        id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let _slot = app
        .slots
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    let inventory = crate::inventory::local(&app, project, fresh)
        .await
        .map_err(|e| match e.kind() {
            io::ErrorKind::Unsupported => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        })?;
    let state = app.state.lock().unwrap();
    let grant = state.worker_grant(&id).ok_or(DENY)?;
    if inventory.host.id.as_deref()
        != Some(
            &state
                .host_id()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        )
    {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    Ok(Json(
        crate::inventory::report(inventory, grant.clone())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscribe {
    proof: Proof,
    subscription: Option<push::Subscription>,
}
async fn subscribe(
    Extract(app): Extract<App>,
    h: HeaderMap,
    Json(input): Json<Subscribe>,
) -> Result<StatusCode, ErrCode> {
    let id = device(&app, &h)?;
    let mut s = app.state.lock().unwrap();
    if !s.prove(
        &id,
        Purpose::Push,
        &input.proof.nonce,
        &input.proof.signature,
    ) {
        return Err(DENY);
    }
    if input.subscription.as_ref().is_some_and(|sub| !sub.valid()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    s.set_push(&id, input.subscription)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn upgrade(
    Extract(app): Extract<App>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ErrCode> {
    let id = device(&app, &h)?;
    let slot = app
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(ws
        .max_frame_size(9 * 1024 * 1024)
        .max_message_size(9 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            bridge::session(app, id, socket).await
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serve_identity_from_a_non_loopback_peer_or_foreign_origin_is_rejected() {
        let origin = "https://machine.test.ts.net:8443";
        let mut h = HeaderMap::new();
        h.insert("host", "machine.test.ts.net:8443".parse().unwrap());
        h.insert("origin", origin.parse().unwrap());
        h.insert("tailscale-user-login", "test@example.test".parse().unwrap());
        for remote in ["192.168.1.44:3434", "100.64.0.2:1024", "[2001:db8::1]:443"] {
            assert!(!permitted(
                origin,
                remote.parse().unwrap(),
                "/api/socket",
                &h
            ));
        }
        let local = "127.0.0.1:41234".parse().unwrap();
        assert!(permitted(origin, local, "/api/socket", &h));
        h.append("origin", "https://elsewhere.invalid".parse().unwrap());
        assert!(!permitted(origin, local, "/api/socket", &h));
        h.remove("origin");
        assert!(!permitted(origin, local, "/api/pair", &h));
        h.insert("origin", origin.parse().unwrap());
        assert!(permitted(origin, local, "/api/pair", &h));
        // Reaching the loopback port directly, not through the Serve hostname.
        let mut direct = h.clone();
        direct.insert("host", "127.0.0.1:48162".parse().unwrap());
        assert!(!permitted(origin, local, "/", &direct));
        // Public Funnel traffic is refused even with an identity header attached.
        let mut funnel = h.clone();
        funnel.insert("tailscale-funnel-request", "?1".parse().unwrap());
        assert!(!permitted(origin, local, "/", &funnel));
        h.remove("tailscale-user-login");
        assert!(!permitted(origin, local, "/", &h));
    }

    #[tokio::test]
    async fn listener_binds_loopback_whatever_the_configured_port() {
        // bind() is the only way the service opens its listener; it binds
        // 127.0.0.1 and refuses anything else, so the address it returns is
        // the address every request arrives on.
        let listener = bind(0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::from(Ipv4Addr::LOCALHOST));
    }

    mod live {
        use super::super::*;
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
        use p256::{
            ecdsa::{Signature, SigningKey, signature::Signer},
            elliptic_curve::rand_core::OsRng,
        };
        use tokio::net::TcpStream;

        pub(super) const ORIGIN: &str = "https://machine.test.ts.net:8443";
        pub(super) const HOST: &str = "machine.test.ts.net:8443";
        pub(super) const LOGIN: &str = "me@example.test";

        pub(super) struct Live {
            _dir: tempfile::TempDir,
            pub app: App,
            pub addr: SocketAddr,
            pub key: SigningKey,
            pub cookie: String,
            pub id: String,
            pub host_connected: tokio::sync::mpsc::UnboundedReceiver<()>,
            _listener: Listener,
        }
        fn sign(key: &SigningKey, message: &str) -> String {
            let sig: Signature = key.sign(message.as_bytes());
            B64.encode(sig.to_bytes())
        }
        /// A real remote listener in front of a silent stand-in session host, with one paired device.
        pub(super) async fn start() -> Live {
            start_with_moves(None).await
        }

        pub(super) async fn start_with_moves(
            moves: Option<crate::move_transport::Receiver>,
        ) -> Live {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let mut state = State::load(dir.path()).unwrap();
            state.set_origin(Some(ORIGIN.into()), 0).unwrap();
            let key = SigningKey::random(&mut OsRng);
            let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
            let invite = state.invite(Some(LOGIN.into()), false).unwrap();
            let code = invite.url.split_once("#pair=").unwrap().1;
            let cookie = state
                .pair(
                    LOGIN,
                    code,
                    &public,
                    &sign(&key, code),
                    "Phone",
                    crate::admin::DeviceClass::Phone,
                )
                .unwrap();
            let id = state.identity(LOGIN, &cookie).unwrap();
            let socket = dir.path().join("host.sock");
            let host = UnixListener::bind(&socket).unwrap();
            let (tx, host_connected) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((stream, _)) = host.accept().await {
                    held.push(stream);
                    let _ = tx.send(());
                }
            });
            let app = App {
                jobs: None,
                moves,
                state: Arc::new(Mutex::new(state)),
                host: Arc::new(socket),
                slots: Arc::new(Semaphore::new(4)),
                inventory: Default::default(),
                discovered: Default::default(),
            };
            let tcp = bind(0).await.unwrap();
            let addr = tcp.local_addr().unwrap();
            let listener = start_listener(app.clone(), ORIGIN.into(), tcp);
            Live {
                _dir: dir,
                app,
                addr,
                key,
                cookie,
                id,
                host_connected,
                _listener: listener,
            }
        }

        pub(super) fn pair_worker(live: &Live, source: &str, target: &str) -> String {
            fs::write(live._dir.path().join("host-id"), target).unwrap();
            let mut state = live.app.state.lock().unwrap();
            let code = state
                .local_worker_invite(Some(LOGIN.into()))
                .unwrap()
                .short_code
                .unwrap();
            let public = B64.encode(live.key.verifying_key().to_encoded_point(false).as_bytes());
            state
                .pair(
                    LOGIN,
                    &code,
                    &public,
                    &sign(&live.key, &format!("{code}.host.{source}")),
                    "Source host",
                    state::Enrollment {
                        class: crate::admin::DeviceClass::Host,
                        source: Some(source),
                    },
                )
                .unwrap()
        }

        pub(super) fn signed_move(live: &Live, device: &str, request: &str) -> String {
            use sha2::{Digest, Sha256};

            let nonce = live
                .app
                .state
                .lock()
                .unwrap()
                .challenge(device, Purpose::Worker)
                .unwrap();
            let digest = B64.encode(Sha256::digest(request.as_bytes()));
            let signature = sign(&live.key, &format!("{nonce}.{digest}"));
            json!({"request": request, "proof": {"nonce": nonce, "signature": signature}})
                .to_string()
        }
        pub(super) struct Req<'a> {
            pub method: &'a str,
            pub path: &'a str,
            pub host: &'a str,
            pub login: Option<&'a str>,
            pub cookie: Option<&'a str>,
            pub extra: &'a [(&'a str, &'a str)],
            pub body: &'a str,
        }
        impl Default for Req<'_> {
            fn default() -> Self {
                Req {
                    method: "GET",
                    path: "/",
                    host: HOST,
                    login: Some(LOGIN),
                    cookie: None,
                    extra: &[],
                    body: "",
                }
            }
        }
        /// Send a raw HTTP/1.1 request the way the Serve proxy would and return the status.
        pub(super) async fn send(addr: SocketAddr, r: Req<'_>, close: bool) -> (u16, TcpStream) {
            let mut text = format!(
                "{} {} HTTP/1.1\r\nhost: {}\r\norigin: {ORIGIN}\r\n",
                r.method, r.path, r.host
            );
            if let Some(login) = r.login {
                text += &format!("tailscale-user-login: {login}\r\n");
            }
            if let Some(cookie) = r.cookie {
                text += &format!("cookie: __Host-ox-device={cookie}\r\n");
            }
            for (k, v) in r.extra {
                text += &format!("{k}: {v}\r\n");
            }
            if !r.body.is_empty() {
                text += &format!(
                    "content-type: application/json\r\ncontent-length: {}\r\n",
                    r.body.len()
                );
            }
            if close {
                text += "connection: close\r\n";
            }
            text += "\r\n";
            text += r.body;
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(text.as_bytes()).await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0u8];
                if stream.read(&mut byte).await.unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head);
            let status = head
                .split(' ')
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            (status, stream)
        }
        pub(super) fn proof(live: &Live) -> String {
            let nonce = live
                .app
                .state
                .lock()
                .unwrap()
                .challenge(&live.id, Purpose::Connect)
                .unwrap();
            json!({"nonce": nonce, "signature": sign(&live.key, &nonce)}).to_string()
        }
        /// One masked client text frame.
        pub(super) fn text_frame(payload: &str) -> Vec<u8> {
            let mask = [0x1b, 0x2c, 0x3d, 0x4e];
            let mut out = vec![0x81];
            let len = payload.len();
            if len < 126 {
                out.push(0x80 | len as u8);
            } else {
                out.push(0x80 | 126);
                out.extend_from_slice(&(len as u16).to_be_bytes());
            }
            out.extend_from_slice(&mask);
            out.extend(payload.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            out
        }
    }

    #[tokio::test]
    async fn signed_move_routes_require_destination_permission_and_an_exact_reservation() {
        use crate::{
            move_manifest::{CHECKOUT, FileSpec, Manifest, PLAN, TRANSCRIPT},
            move_transport::{Cancellation, Incoming, Offer, Reservation},
            state::move_staging::send_files_for_prepare,
            worker_checkout::connection::Connection,
        };
        use live::{HOST, LOGIN, ORIGIN, Req, send, signed_move};
        use sha2::{Digest, Sha256};
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::TcpStream;
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

        const SOURCE: &str = "74b2c731-b9f0-4b05-8672-3c42062b190a";
        const TARGET: &str = "06f2bbe4-12bb-4243-ab20-657f19c02d36";
        const TRANSFER: &str = "ddcba183-5073-4e9a-9825-e8730784ce5e";
        const PLAN_JSON: &str = r#"{"session":"original"}"#;
        let prepared = Arc::new(AtomicBool::new(false));
        let marker = prepared.clone();
        let receiver = Arc::new(move |request| match request {
            Incoming::Reserve(plan) => {
                assert_eq!(plan.proposal, "fixture proposal");
                Ok(json!(PLAN_JSON))
            }
            Incoming::Prepare(staged) => {
                assert_eq!(staged.offer.transfer, TRANSFER);
                assert_eq!(
                    staged.expected_plan,
                    Some(Sha256::digest(PLAN_JSON.as_bytes()).into())
                );
                marker.store(true, Ordering::SeqCst);
                Ok(json!({"native": "studio"}))
            }
            Incoming::Activate(activation) => activation.commit(|source, transfer, _, commit| {
                assert_eq!((source, transfer), (SOURCE, TRANSFER));
                if !marker.load(Ordering::SeqCst) || commit != json!({"native": "studio"}) {
                    return Err(io::Error::other(
                        "The destination has not prepared this writer.",
                    ));
                }
                Ok(json!({"ready": true}))
            }),
            Incoming::Cancel(cancellation) => cancellation.cancel(|_| {
                Err::<serde_json::Value, _>(io::Error::other(
                    "The destination already accepted the move.",
                ))
            }),
        });
        let live = live::start_with_moves(Some(receiver)).await;
        let cookie = live::pair_worker(&live, SOURCE, TARGET);
        let device = live
            .app
            .state
            .lock()
            .unwrap()
            .identity(LOGIN, &cookie)
            .unwrap();
        let reserve = serde_json::to_string(&Reservation {
            source: SOURCE.into(),
            target: TARGET.into(),
            transfer: TRANSFER.into(),
            proposal: "fixture proposal".into(),
        })
        .unwrap();
        let post = |path, body| Req {
            method: "POST",
            path,
            cookie: Some(&cookie),
            body,
            ..Req::default()
        };
        let denied = signed_move(&live, &device, &reserve);
        assert_eq!(
            send(live.addr, post("/api/move-reserve", &denied), true)
                .await
                .0,
            403
        );
        {
            let mut state = live.app.state.lock().unwrap();
            let before = state.worker_grant(&device).unwrap().clone();
            state
                .edit_worker(&device, &before, false, before.ceiling, None, Some(true))
                .unwrap();
        }
        let approved = signed_move(&live, &device, &reserve);
        assert_eq!(
            send(live.addr, post("/api/move-reserve", &approved), true)
                .await
                .0,
            200
        );
        assert_eq!(
            send(live.addr, post("/api/move-reserve", &approved), true)
                .await
                .0,
            403
        );

        let files = tempfile::tempdir().unwrap();
        let spec = |name: &str, data: &[u8]| {
            fs::write(files.path().join(name), data).unwrap();
            FileSpec {
                bytes: data.len() as u64,
                sha256: Sha256::digest(data).into(),
            }
        };
        let offer = Offer {
            source: SOURCE.into(),
            target: TARGET.into(),
            transfer: TRANSFER.into(),
            manifest: Manifest {
                plan: spec(PLAN, PLAN_JSON.as_bytes()),
                transcript: spec(TRANSCRIPT, b"conversation"),
                checkout: spec(CHECKOUT, b"project"),
            },
        };
        let commit = json!({"offer": offer, "commit": {"native": "studio"}}).to_string();
        let early_commit = signed_move(&live, &device, &commit);
        assert_eq!(
            send(live.addr, post("/api/move-activate", &early_commit), true)
                .await
                .0,
            403
        );

        let mut request = format!("ws://{HOST}/api/move-stage")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", ORIGIN.parse().unwrap());
        request
            .headers_mut()
            .insert("tailscale-user-login", LOGIN.parse().unwrap());
        request.headers_mut().insert(
            "cookie",
            format!("__Host-ox-device={cookie}").parse().unwrap(),
        );
        let (socket, _) =
            tokio_tungstenite::client_async(request, TcpStream::connect(live.addr).await.unwrap())
                .await
                .unwrap();
        let mut socket = Connection::open(socket);
        socket
            .send(Message::Text(
                signed_move(&live, &device, &serde_json::to_string(&offer).unwrap()).into(),
            ))
            .await
            .unwrap();
        let receipt: serde_json::Value = send_files_for_prepare(&mut socket, &offer, files.path())
            .await
            .unwrap();
        assert_eq!(receipt, json!({"native": "studio"}));
        assert!(prepared.load(Ordering::SeqCst));
        let final_commit = signed_move(&live, &device, &commit);
        let (status, mut response) =
            send(live.addr, post("/api/move-activate", &final_commit), true).await;
        assert_eq!(status, 200);
        let mut body = String::new();
        response.read_to_string(&mut body).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            json!({"receipt": {"ready": true}})
        );
        let cancelled = serde_json::to_string(&Cancellation {
            source: SOURCE.into(),
            target: TARGET.into(),
            transfer: TRANSFER.into(),
            cancelled_plan: PLAN_JSON.into(),
        })
        .unwrap();
        let cancelled = signed_move(&live, &device, &cancelled);
        let (_, mut response) = send(live.addr, post("/api/move-cancel", &cancelled), true).await;
        body.clear();
        response.read_to_string(&mut body).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            json!({"error": "The destination already accepted the move."})
        );
        let retry = signed_move(&live, &device, &commit);
        assert_eq!(
            send(live.addr, post("/api/move-activate", &retry), true)
                .await
                .0,
            200
        );
    }

    #[tokio::test]
    async fn identity_headers_count_only_on_the_serve_path_and_never_alone() {
        use live::{LOGIN, Req, send, start};
        let live = start().await;
        assert_eq!(send(live.addr, Req::default(), true).await.0, 200);
        // Direct loopback access with a forged identity header: wrong Host.
        let direct = format!("127.0.0.1:{}", live.addr.port());
        let forged = Req {
            host: &direct,
            ..Req::default()
        };
        assert_eq!(send(live.addr, forged, true).await.0, 403);
        let funnel = Req {
            extra: &[("tailscale-funnel-request", "?1")],
            ..Req::default()
        };
        let (status, mut refused) = send(live.addr, funnel, true).await;
        assert_eq!(status, 403);
        let mut body = String::new();
        refused.read_to_string(&mut body).await.unwrap();
        assert!(body.contains("Funnel"), "{body}");
        let duplicated = Req {
            extra: &[("tailscale-user-login", "other@example.test")],
            ..Req::default()
        };
        assert_eq!(send(live.addr, duplicated, true).await.0, 403);
        let challenge = |login, cookie| Req {
            method: "POST",
            path: "/api/challenge",
            login,
            cookie,
            body: r#"{"purpose":"connect"}"#,
            ..Req::default()
        };
        // The login header alone grants nothing.
        assert_eq!(
            send(live.addr, challenge(Some(LOGIN), None), true).await.0,
            403
        );
        // A paired device's cookie under another Tailscale identity grants nothing.
        let other = challenge(Some("other@example.test"), Some(&live.cookie));
        assert_eq!(send(live.addr, other, true).await.0, 403);
        // The cookie without a Serve identity grants nothing.
        let anonymous = challenge(None, Some(&live.cookie));
        assert_eq!(send(live.addr, anonymous, true).await.0, 403);
        // Positive control: the paired device on the Serve path.
        let paired = challenge(Some(LOGIN), Some(&live.cookie));
        assert_eq!(send(live.addr, paired, true).await.0, 200);
    }

    #[tokio::test]
    async fn revoking_a_device_closes_its_live_socket_immediately() {
        use live::{Req, send, start, text_frame};
        let mut live = start().await;
        let cookie = live.cookie.clone();
        let upgrade = Req {
            path: "/api/socket",
            cookie: Some(&cookie),
            extra: &[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-version", "13"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ],
            ..Req::default()
        };
        let (status, mut ws) = send(live.addr, upgrade, false).await;
        assert_eq!(status, 101);
        ws.write_all(&text_frame(&live::proof(&live)))
            .await
            .unwrap();
        // The bridge opens its session-host connection only after the proof verifies.
        tokio::time::timeout(Duration::from_secs(5), live.host_connected.recv())
            .await
            .expect("device authenticated")
            .unwrap();
        let mut first = [0u8; 2];
        assert!(
            tokio::time::timeout(Duration::from_millis(300), ws.read(&mut first))
                .await
                .is_err(),
            "socket stays open while the device is paired"
        );
        live.app.state.lock().unwrap().revoke(&live.id).unwrap();
        // No further client traffic: the host closes the socket on its own.
        let n = tokio::time::timeout(Duration::from_secs(10), ws.read(&mut first))
            .await
            .expect("revocation did not close the live socket")
            .unwrap();
        assert!(n == 0 || first[0] & 0x0f == 0x8, "expected a close frame");
    }
}
