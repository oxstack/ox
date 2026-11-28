use crate::{
    server::{App, Proof},
    state::Purpose,
};
use axum::extract::ws::{Message, WebSocket};
use ox_desk_core::{
    actions::Action,
    host_protocol::{Client, Scope, Server},
    runtime::Route,
};
use serde::Serialize;
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

pub(crate) async fn connect(path: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path).await?;
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("Invalid host peer"));
    }
    Ok(stream)
}
pub(crate) async fn write<T: Serialize>(
    w: &mut (impl AsyncWrite + Unpin),
    v: &T,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(v)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await
}
/// Keep the partial frame in the caller. Cancellation (including inbound WS traffic) loses no bytes.
pub(crate) async fn line(
    reader: &mut (impl AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
) -> io::Result<Vec<u8>> {
    loop {
        let part = reader.fill_buf().await?;
        if part.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let end = part.iter().position(|&b| b == b'\n');
        let len = end.unwrap_or(part.len());
        if buf.len() + len > 64 * 1024 * 1024 {
            return Err(io::Error::other("Oversized session-host frame"));
        }
        buf.extend_from_slice(&part[..len]);
        reader.consume(len + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(std::mem::take(buf));
        }
    }
}
// Attach only bytes supplied by this device. Local paths refer to files on the host.
fn valid_upload(image: &ox_desk_core::mentions::ImageInput) -> bool {
    let ox_desk_core::mentions::ImageInput::Clipboard(mime, data) = image else {
        return false;
    };
    matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) && data
        .strip_prefix(&format!("data:{mime};base64,"))
        .is_some_and(|b64| {
            !b64.is_empty()
                && b64.len() <= 8 * 1024 * 1024
                && b64
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        })
}
#[derive(Default)]
struct Access {
    class: crate::admin::DeviceClass,
    hello: Option<u32>,
    ready: bool,
    projects: HashSet<PathBuf>,
    routes: HashSet<Route>,
    selected: Option<Scope>,
    discovery: Option<String>,
}
impl Access {
    /// Track a host frame. Returns the frame the bridge itself must send the host next:
    /// the phone's surface, once a host hello lists that it negotiates one.
    fn host(&mut self, app: &App, bytes: &[u8]) -> io::Result<Option<Client>> {
        if !self.ready {
            match serde_json::from_slice::<Server>(bytes)? {
                Server::Hello {
                    version,
                    projects,
                    routes,
                    capabilities,
                    ..
                } if self.hello.is_some_and(|offer| version <= offer)
                    && ox_desk_core::host_protocol::supported(version) =>
                {
                    self.ready = true;
                    self.projects = projects.into_iter().collect();
                    self.routes = routes.into_iter().collect();
                    // The bridge authenticated the device, so it states the surface. A
                    // phone's own declaration is never forwarded (see `permit`).
                    if capabilities
                        .iter()
                        .any(|c| c == ox_desk_core::surface::CAPABILITY)
                    {
                        return Ok(Some(Client::Surface {
                            surface: match self.class {
                                crate::admin::DeviceClass::Host => {
                                    return Err(io::Error::other(
                                        "Worker hosts use the worker link",
                                    ));
                                }
                                crate::admin::DeviceClass::Phone => {
                                    ox_desk_core::surface::Surface::Phone
                                }
                                crate::admin::DeviceClass::Desktop => {
                                    ox_desk_core::surface::Surface::Desktop
                                }
                            },
                        }));
                    }
                }
                Server::Error { .. } => {}
                _ => return Err(io::Error::other("Incompatible session host")),
            }
        } else if self.discovery.is_some() {
            match serde_json::from_slice::<Server>(bytes)? {
                Server::Discovered { id, scope, .. } if Some(&id) == self.discovery.as_ref() => {
                    let mut known = app.discovered.lock().unwrap();
                    if known.len() < 8192 {
                        known.insert(scope);
                    }
                }
                Server::DiscoveredEnd { id, .. } if Some(&id) == self.discovery.as_ref() => {
                    self.discovery = None
                }
                _ => {}
            }
        }
        Ok(None)
    }
    /// Whether a scope may be subscribed: listed in the hello, or discovered on this host.
    fn reachable(&self, app: &App, scope: &Scope) -> bool {
        // Any catalog alias may run on either runtime, starred or not, and the Hello can
        // predate model discovery (no routes at all), so a new scope for an API route needs
        // no listed route: the host validates the provider and alias at launch. Subscription
        // routes still have to be listed.
        (self.projects.contains(&scope.project)
            && (self.routes.contains(&scope.route) || scope.route.is_api()))
            || app.discovered.lock().unwrap().contains(scope)
    }
    /// A request that passed auth but was refused for its content: the request id, if it has
    /// one, and the reason. Those are answered with an error frame. Protocol and auth violations
    /// return `None` and still close the socket.
    fn invalid(&self, app: &App, req: &Client) -> Option<(Option<String>, String)> {
        if let Client::InspectHandoffTarget { id, scope }
        | Client::PrepareRemoteHandoff { id, scope, .. }
        | Client::ReceiveRemoteHandoff { id, scope, .. } = req
            && self.ready
            && !self.reachable(app, scope)
        {
            return Some((
                Some(id.clone()),
                "This checkout or route is no longer available on this host.".into(),
            ));
        }
        if let Client::Subscribe {
            scope: Some(scope), ..
        } = req
        {
            if !self.ready || self.reachable(app, scope) {
                return None;
            }
            return Some((
                None,
                "This session's route is not available on this host. Check that the host can read its settings, then try again."
                    .into(),
            ));
        }
        let Client::Act {
            id,
            scope,
            action,
            approval,
        } = req
        else {
            return None;
        };
        if !self.ready
            || self.selected.as_ref() != Some(scope)
            || id.is_empty()
            || id.len() > 96
            || approval.is_some()
        {
            return None;
        }
        let message = match action {
            Action::StartWorker(start) => start.validate().err()?,
            Action::StartHandoff(start) => start.validate().err()?,
            Action::YoloGrant(grant, _) if grant.is_empty() || grant.len() > 512 => {
                "This approval target is not valid. Reopen the session and try again.".into()
            }
            Action::Mention(_, Some(query)) if query.len() > 512 => {
                "That mention search is too long. Shorten it and try again.".into()
            }
            Action::SendImages(_, text, images) => {
                if text.len() > 128 * 1024 {
                    "This message is too long to send with images. Shorten it and try again.".into()
                } else if images.is_empty() || images.len() > 4 || !images.iter().all(valid_upload)
                {
                    "Attach up to 4 images pasted from this device, then send again.".into()
                } else {
                    return None;
                }
            }
            Action::SetNextAccess(mode) | Action::SetAccess(_, mode)
                if !ox_desk_core::runtime::Access::choices(scope.route.runtime).contains(mode) =>
            {
                "That access mode is not available for this route. Choose another and try again."
                    .into()
            }
            _ if self.class == crate::admin::DeviceClass::Desktop => {
                "This action is unavailable over a remote desktop connection.".into()
            }
            _ => return None,
        };
        Some((Some(id.clone()), message))
    }
    fn permit(&mut self, app: &App, req: &Client) -> bool {
        match req {
            Client::Hello { version } if self.hello.is_none() => {
                self.hello = Some(*version);
                true
            }
            Client::Discover { id, .. }
                if self.ready && self.discovery.is_none() && id.len() <= 96 && !id.is_empty() =>
            {
                self.discovery = Some(id.clone());
                true
            }
            Client::WatchHost if self.ready => true,
            Client::WatchCatalog if self.ready => {
                self.selected = None;
                true
            }
            Client::WatchHandoffReads { .. } | Client::FinishHandoffRead { .. } => {
                self.ready && self.class == crate::admin::DeviceClass::Desktop
            }
            Client::ReadHandoffPage { id, scope, .. } => {
                self.ready
                    && self.class == crate::admin::DeviceClass::Desktop
                    && !id.is_empty()
                    && id.len() <= 96
                    && self.reachable(app, scope)
            }
            Client::InspectHandoffTarget { id, scope } => {
                self.ready && !id.is_empty() && id.len() <= 96 && self.reachable(app, scope)
            }
            Client::PrepareRemoteHandoff {
                id, scope, start, ..
            } => {
                self.ready
                    && !id.is_empty()
                    && id.len() <= 96
                    && self.reachable(app, scope)
                    && start.validate().is_ok()
            }
            Client::ReceiveRemoteHandoff { id, scope, packet } => {
                self.ready
                    && !id.is_empty()
                    && id.len() <= 96
                    && self.reachable(app, scope)
                    && packet.start.validate().is_ok()
            }
            Client::WorkerHosts { id, project }
                if self.ready && !id.is_empty() && id.len() <= 96 =>
            {
                self.projects.contains(project)
                    || app
                        .discovered
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|s| &s.project == project)
            }
            // A read-only listing of the host's live processes, answered by `Server::Processes`.
            Client::ListProcesses { id } if self.ready && id.len() <= 96 && !id.is_empty() => true,
            Client::Subscribe {
                scope, terminal, ..
            } if self.ready && !terminal => {
                if scope.as_ref().is_some_and(|s| !self.reachable(app, s)) {
                    return false;
                }
                self.selected = scope.clone();
                true
            }
            Client::Act {
                id,
                scope,
                action,
                approval,
            } if self.ready
                && self.selected.as_ref() == Some(scope)
                && id.len() <= 96
                && !id.is_empty() =>
            {
                match action {
                    Action::SelectThread(_)
                    | Action::GenerateTitle(_)
                    | Action::RenameThread(..)
                    | Action::ArchiveThread(_)
                    | Action::ForkThread(_)
                    | Action::ReadAccountLimits
                    | Action::NativeCommand(..)
                    | Action::ModelOptions(..)
                    | Action::LoadOlder(_)
                    | Action::Stop(_)
                    | Action::EndSession(_)
                    | Action::ForceEndSession(_)
                    | Action::SendText(..) => approval.is_none(),
                    Action::NewDraft
                    | Action::DismissNotice
                    | Action::PaletteMore
                    | Action::MoreThreads
                    | Action::OpenCatalogRow(_)
                    | Action::ConfirmAliasAndResume(_)
                    | Action::ResumeHere(_)
                        if self.class == crate::admin::DeviceClass::Desktop =>
                    {
                        approval.is_none()
                    }
                    Action::SearchThreads(query) | Action::PaletteSearch(query)
                        if self.class == crate::admin::DeviceClass::Desktop =>
                    {
                        approval.is_none() && query.len() <= 4096
                    }
                    Action::StartWorker(start) => approval.is_none() && start.validate().is_ok(),
                    Action::StartHandoff(start) => approval.is_none() && start.validate().is_ok(),
                    Action::MoveSession(start) => {
                        approval.is_none()
                            && !start.thread.is_empty()
                            && start.thread.len() <= 256
                            && crate::paired_hosts::valid_id(&start.destination)
                            && crate::paired_hosts::valid_id(&start.operation)
                    }
                    Action::CancelMove(id) | Action::CheckMove(id) | Action::RetryMove(id) => {
                        approval.is_none() && crate::paired_hosts::valid_id(id)
                    }
                    Action::YoloGrant(id, _) => {
                        approval.is_none() && !id.is_empty() && id.len() <= 512
                    }
                    Action::Mention(_, query) => {
                        approval.is_none() && query.as_ref().is_none_or(|q| q.len() <= 512)
                    }
                    Action::SendImages(_, text, images) => {
                        approval.is_none()
                            && text.len() <= 128 * 1024
                            && !images.is_empty()
                            && images.len() <= 4
                            && images.iter().all(valid_upload)
                    }
                    Action::SetNextAccess(mode) | Action::SetAccess(_, mode) => {
                        approval.is_none()
                            && ox_desk_core::runtime::Access::choices(scope.route.runtime)
                                .contains(mode)
                    }
                    Action::AnswerAsk(..) | Action::AnswerQuestions(..) => approval
                        .as_ref()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 256),
                    _ => false,
                }
            }
            _ => false,
        }
    }
}
pub(crate) async fn session(app: App, id: String, mut ws: WebSocket) {
    let Ok(Some(Ok(Message::Text(frame)))) = timeout(Duration::from_secs(5), ws.recv()).await
    else {
        return;
    };
    let Ok(proof) = serde_json::from_str::<Proof>(&frame) else {
        return;
    };
    let (mut changes, class) = {
        let mut s = app.state.lock().unwrap();
        if !s.prove(&id, Purpose::Connect, &proof.nonce, &proof.signature) {
            return;
        }
        let Some(class) = s.device_class(&id) else {
            return;
        };
        (s.changed.subscribe(), class)
    };
    let Ok(host) = connect(&app.host).await else {
        return;
    };
    let (r, mut w) = host.into_split();
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    let mut access = Access {
        class,
        ..Access::default()
    };
    loop {
        if !app.state.lock().unwrap().active_device(&id) {
            break;
        }
        tokio::select! {biased;
            _=changes.changed()=>{},
            msg=ws.recv()=>{
                let Some(Ok(msg))=msg else{break};
                match msg{
                    Message::Text(s)=>{
                        let Ok(req)=serde_json::from_str::<Client>(&s) else{break};
                        if !access.permit(&app,&req) {
                            // Answer the refused request with its reason and keep the socket open.
                            let Some((request,message))=access.invalid(&app,&req) else{break};
                            if !app.state.lock().unwrap().alive(&id){break}
                            let Ok(frame)=serde_json::to_string(&Server::Error{id:request,message}) else{break};
                            if !matches!(timeout(Duration::from_secs(8),ws.send(Message::Text(frame.into()))).await,Ok(Ok(()))){break}
                            continue;
                        }
                        if !app.state.lock().unwrap().alive(&id){break}
                        if !matches!(timeout(Duration::from_secs(5),write(&mut w,&req)).await,Ok(Ok(()))){break}
                    }
                    Message::Ping(_) | Message::Pong(_)=>{},
                    _=>break
                }
            },
            frame=line(&mut r,&mut buf)=>{
                let Ok(bytes)=frame else{break};
                let Ok(declare)=access.host(&app,&bytes) else{break};
                if !app.state.lock().unwrap().alive(&id){break}
                if let Some(declare)=declare
                    && !matches!(timeout(Duration::from_secs(5),write(&mut w,&declare)).await,Ok(Ok(()))){break}
                let Ok(s)=String::from_utf8(bytes) else{break};
                if !matches!(timeout(Duration::from_secs(8),ws.send(Message::Text(s.into()))).await,Ok(Ok(()))){break}
            },
        }
    }
    let _ = timeout(Duration::from_secs(1), ws.send(Message::Close(None))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::host_protocol::VERSION;
    use ox_desk_core::runtime::Runtime;
    #[test]
    fn the_bridge_declares_the_phone_itself_and_never_forwards_a_declaration() {
        use ox_desk_core::surface::{CAPABILITY, Surface};
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        let hello = |capabilities: Vec<String>| {
            serde_json::to_vec(&Server::Hello {
                version: VERSION,
                projects: vec![],
                routes: vec![],
                capabilities,
                host: None,
            })
            .unwrap()
        };
        // An older host: nothing to declare, so the phone gets generic guidance.
        let mut old = Access::default();
        assert!(old.permit(&app, &Client::Hello { version: VERSION }));
        assert!(old.host(&app, &hello(vec![])).unwrap().is_none());
        // A host that negotiates surfaces gets the phone's from the bridge.
        let mut a = Access::default();
        let declare = Client::Surface {
            surface: Surface::Desktop,
        };
        assert!(!a.permit(&app, &declare), "not before the hello either");
        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let frame = a.host(&app, &hello(vec![CAPABILITY.into()])).unwrap();
        assert!(matches!(
            frame,
            Some(Client::Surface {
                surface: Surface::Phone
            })
        ));
        assert!(
            !a.permit(&app, &declare),
            "a browser cannot claim to be the desktop"
        );
        assert!(a.invalid(&app, &declare).is_none(), "it closes the socket");
        let mut desktop = Access {
            class: crate::admin::DeviceClass::Desktop,
            ..Default::default()
        };
        assert!(desktop.permit(
            &app,
            &Client::Hello {
                version: VERSION + 1
            }
        ));
        assert!(matches!(
            desktop.host(&app, &hello(vec![CAPABILITY.into()])).unwrap(),
            Some(Client::Surface {
                surface: Surface::Desktop
            })
        ));
        assert!(!desktop.permit(&app, &declare));
        assert!(!desktop.permit(
            &app,
            &Client::Subscribe {
                scope: None,
                after: None,
                terminal: true
            }
        ));
    }

    #[test]
    fn browser_actions_use_subscribed_route_and_native_permission_modes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        let mut a = Access::default();
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let mut other = scope.clone();
        other.project = "/other".into();
        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        assert!(!a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(other.clone()),
                after: None,
                terminal: false,
            }
        ));
        assert!(a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: false,
            }
        ));
        let act = |s: Scope, action, approval| Client::Act {
            id: "request".into(),
            scope: s,
            action,
            approval,
        };
        assert!(!a.permit(&app, &act(other, Action::Stop("thread".into()), None)));
        assert!(a.permit(
            &app,
            &act(
                scope.clone(),
                Action::SetNextAccess(ox_desk_core::runtime::Access::Yolo),
                None
            )
        ));
        assert!(!a.permit(
            &app,
            &act(
                scope.clone(),
                Action::SetNextAccess(ox_desk_core::runtime::Access::BypassPermissions),
                None
            )
        ));
        assert!(!a.permit(
            &app,
            &act(
                scope.clone(),
                Action::PaletteCommand(None, "/terminal".into()),
                None
            )
        ));
        assert!(!a.permit(&app, &act(scope.clone(), Action::Terminal(None), None)));
        assert!(!a.permit(
            &app,
            &act(
                scope,
                Action::AnswerAsk(ox_desk_core::runtime::RequestId::Number(1), 0),
                None
            )
        ));
    }

    #[test]
    fn refused_content_answers_its_request_and_keeps_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let mut a = Access::default();
        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route.clone()],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        assert!(a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: false,
            }
        ));
        let act = |action, approval: Option<String>| Client::Act {
            id: "request".into(),
            scope: scope.clone(),
            action,
            approval,
        };
        let image = || {
            ox_desk_core::mentions::ImageInput::Clipboard(
                "image/png".into(),
                "data:image/png;base64,AAAA".into(),
            )
        };
        let refused = [
            act(
                Action::SendImages(None, "x".repeat(128 * 1024 + 1), vec![image()]),
                None,
            ),
            act(Action::SendImages(None, "hi".into(), vec![]), None),
            act(
                Action::SendImages(
                    None,
                    "hi".into(),
                    vec![ox_desk_core::mentions::ImageInput::File(
                        "/etc/passwd".into(),
                    )],
                ),
                None,
            ),
            act(Action::Mention(None, Some("q".repeat(513))), None),
            act(Action::YoloGrant(String::new(), true), None),
            act(
                Action::SetAccess(
                    "thread".into(),
                    ox_desk_core::runtime::Access::BypassPermissions,
                ),
                None,
            ),
            act(
                Action::SetNextAccess(ox_desk_core::runtime::Access::BypassPermissions),
                None,
            ),
        ];
        for req in &refused {
            assert!(!a.permit(&app, req));
            let (id, message) = a.invalid(&app, req).expect("content refusal is answered");
            assert_eq!(id.as_deref(), Some("request"));
            assert!(!message.is_empty());
            let frame = serde_json::to_value(Server::Error { id, message }).unwrap();
            assert_eq!(frame["type"], "error");
            assert_eq!(frame["id"], "request");
        }

        // Content that passes is not answered as a refusal.
        assert!(a.permit(
            &app,
            &act(Action::SendImages(None, "hi".into(), vec![image()]), None)
        ));
        assert!(
            a.invalid(
                &app,
                &act(Action::SendImages(None, "hi".into(), vec![image()]), None)
            )
            .is_none()
        );

        // Protocol violations still close: an approval on a content refusal is not answered.
        let approved = act(
            Action::SendImages(None, "hi".into(), vec![]),
            Some("yes".into()),
        );
        assert!(!a.permit(&app, &approved));
        assert!(a.invalid(&app, &approved).is_none());
    }

    #[test]
    fn refused_start_answers_its_request_and_keeps_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let blank_handoff = || Client::Act {
            id: "handoff-1".into(),
            scope: scope.clone(),
            action: Action::StartHandoff(ox_desk_core::handoff::HandoffStart {
                host: None,
                source: "thread".into(),
                runtime: route.runtime,
                model: None,
                mode: Default::default(),
                request: "  \n".into(),
                yolo_confirmed: false,
            }),
            approval: None,
        };

        // Before the device has said hello and subscribed, nothing is answered: close.
        let mut a = Access::default();
        assert!(a.invalid(&app, &blank_handoff()).is_none());

        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route.clone()],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        assert!(a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: false,
            }
        ));

        // The refused start is a validation failure: it gets the reason keyed by its id.
        let req = blank_handoff();
        assert!(!a.permit(&app, &req));
        let (id, message) = a
            .invalid(&app, &req)
            .expect("validation refusal is answered");
        assert_eq!(id.as_deref(), Some("handoff-1"));
        assert_eq!(message, "Write the request for the new session.");
        let frame = serde_json::to_value(Server::Error { id, message }).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["id"], "handoff-1");

        // A start that carries an approval is a protocol violation: close, do not answer.
        let mut approved = blank_handoff();
        if let Client::Act { approval, .. } = &mut approved {
            *approval = Some("yes".into());
        }
        assert!(!a.permit(&app, &approved));
        assert!(a.invalid(&app, &approved).is_none());
    }

    #[test]
    fn refused_subscribe_answers_with_an_error_and_keeps_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let subscribe = |s: &Scope| Client::Subscribe {
            scope: Some(s.clone()),
            after: None,
            terminal: false,
        };
        let mut unlisted = scope.clone();
        unlisted.project = "/unlisted".into();

        // Before the hello is accepted, a subscribe is a protocol violation: close, do not answer.
        let mut a = Access::default();
        assert!(a.invalid(&app, &subscribe(&scope)).is_none());

        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route.clone()],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        assert!(a.permit(&app, &subscribe(&scope)));

        // A scope outside the hello is refused with a reason that carries no request id.
        let req = subscribe(&unlisted);
        assert!(!a.permit(&app, &req));
        let (id, message) = a
            .invalid(&app, &req)
            .expect("refused subscribe is answered");
        assert_eq!(id, None);
        assert!(message.contains("not available on this host"));
        let frame = serde_json::to_value(Server::Error { id, message }).unwrap();
        assert_eq!(frame["type"], "error");
        assert!(frame.get("id").is_none_or(|v| v.is_null()));

        // A discovered scope stays subscribable even when the hello does not list it.
        app.discovered.lock().unwrap().insert(unlisted.clone());
        assert!(a.permit(&app, &req));
    }

    #[test]
    fn end_session_follows_stop_and_process_listing_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        let route = Route::CODEX_SUBSCRIPTION;
        let scope = Scope {
            project: "/approved".into(),
            route: route.clone(),
        };
        let mut other = scope.clone();
        other.project = "/other".into();
        let list = || Client::ListProcesses {
            id: "processes".into(),
        };
        let act = |s: Scope, action, approval| Client::Act {
            id: "request".into(),
            scope: s,
            action,
            approval,
        };

        // Nothing is allowed before the host's hello.
        let mut a = Access::default();
        assert!(!a.permit(&app, &list()));
        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        assert!(!a.permit(&app, &list()));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec![scope.project.clone()],
            routes: vec![route],
            capabilities: vec!["end".into(), "processes".into()],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();

        // The process listing needs no subscription, and an empty id is refused.
        assert!(a.permit(&app, &list()));
        assert!(!a.permit(&app, &Client::ListProcesses { id: String::new() }));

        // Ending needs the subscribed scope and no approval, as Stop does.
        assert!(!a.permit(
            &app,
            &act(scope.clone(), Action::EndSession("thread".into()), None)
        ));
        assert!(a.permit(
            &app,
            &Client::Subscribe {
                scope: Some(scope.clone()),
                after: None,
                terminal: false,
            }
        ));
        assert!(!a.permit(&app, &act(other, Action::EndSession("thread".into()), None)));
        assert!(!a.permit(
            &app,
            &act(
                scope.clone(),
                Action::EndSession("thread".into()),
                Some("yes".into())
            )
        ));
        assert!(a.permit(
            &app,
            &act(scope.clone(), Action::EndSession("thread".into()), None)
        ));
        assert!(a.permit(
            &app,
            &act(
                scope.clone(),
                Action::ForceEndSession("thread".into()),
                None
            )
        ));
        assert!(a.permit(&app, &list()));
    }

    fn cold_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let app = App {
            jobs: None,
            moves: None,
            state: std::sync::Arc::new(std::sync::Mutex::new(
                crate::state::State::load(dir.path()).unwrap(),
            )),
            host: Default::default(),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            inventory: Default::default(),
            discovered: Default::default(),
        };
        (dir, app)
    }

    #[test]
    fn a_cold_hello_without_routes_still_accepts_any_catalog_alias_for_a_new_scope() {
        let (_dir, app) = cold_app();
        let mut a = Access::default();
        assert!(a.permit(&app, &Client::Hello { version: VERSION }));
        let hello = Server::Hello {
            version: VERSION,
            projects: vec!["/approved".into()],
            routes: vec![],
            capabilities: vec![],
            host: None,
        };
        a.host(&app, &serde_json::to_vec(&hello).unwrap()).unwrap();
        let subscribe = |project: &str, route: Route| Client::Subscribe {
            scope: Some(Scope {
                project: project.into(),
                route,
            }),
            after: None,
            terminal: false,
        };
        // Discovery finished after the Hello: an unstarred alias opens on both runtimes.
        for runtime in [Runtime::Codex, Runtime::Claude] {
            assert!(a.permit(
                &app,
                &subscribe("/approved", runtime.route(Some("fixture/unstarred".into())))
            ));
        }
        // A subscription route nobody listed, and a project nobody approved, stay refused.
        assert!(!a.permit(&app, &subscribe("/approved", Route::CODEX_SUBSCRIPTION)));
        assert!(!a.permit(
            &app,
            &subscribe(
                "/other",
                Runtime::Claude.route(Some("fixture/unstarred".into()))
            )
        ));
    }
}
