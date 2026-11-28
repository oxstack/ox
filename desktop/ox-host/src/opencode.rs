//! The stock OpenCode HTTP contract used by the OX host. Use on an I/O worker. An SSE
//! subscription is live only; a caller must reconcile history and approvals
//! after disconnect. The OX host owns the durable session registry.

mod bootstrap;
mod events;
mod profile;
pub mod runtime;
#[path = "../../../src/sse.rs"]
mod sse;

pub use bootstrap::Hosted;
pub use events::{Event, Events};
pub use profile::{Owner, Profile};

use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, RequestBuilder, Response},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::{self, Read as _},
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

pub(super) const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAGE: usize = 100;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
// Stock sends a heartbeat every ten seconds.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum Error {
    OwnerMismatch,
    InvalidInput(&'static str),
    InvalidResponse(&'static str),
    NativeStatus {
        operation: &'static str,
        status: StatusCode,
    },
    IncompatibleReply(&'static str),
    BlockedByCancellation,
    UnknownSession,
    TurnInProgress,
    ConflictingSend,
    AmbiguousSend,
    RegistryUnavailable,
    UnknownPermission,
    Transport(reqwest::Error),
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerMismatch => {
                out.write_str("OpenCode session belongs to another host, project, or billing route")
            }
            Self::InvalidInput(detail) | Self::InvalidResponse(detail) => out.write_str(detail),
            Self::NativeStatus { operation, status } => {
                write!(out, "OpenCode could not {operation} (HTTP {status})")
            }
            Self::IncompatibleReply(operation) => {
                write!(
                    out,
                    "Installed OpenCode returned an incompatible response while attempting to {operation}"
                )
            }
            Self::BlockedByCancellation => {
                out.write_str("OpenCode cancellation is still being reconciled")
            }
            Self::UnknownSession => {
                out.write_str("OpenCode session is absent from this host route")
            }
            Self::TurnInProgress => out.write_str("An OpenCode turn is still in progress"),
            Self::ConflictingSend => {
                out.write_str("OpenCode send ID conflicts with the host receipt")
            }
            Self::AmbiguousSend => out.write_str(
                "OpenCode did not confirm the send; reconcile or stop the turn before trying again",
            ),
            Self::RegistryUnavailable => {
                out.write_str("OpenCode host registry must be reopened after a storage error")
            }
            Self::UnknownPermission => {
                out.write_str("OpenCode permission is no longer pending in this session")
            }
            Self::Transport(error) => error.fmt(out),
            Self::Io(error) => error.fmt(out),
            Self::Json(error) => error.fmt(out),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(error.without_url())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Created or recovered by the owning adapter. A raw native identifier is
/// useful for persistence, but clients should never submit it as authority.
#[derive(Clone, Debug)]
pub struct Session {
    id: String,
    owner: Owner,
    profile_root: PathBuf,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Deserialize)]
struct NativeSession {
    id: String,
    directory: PathBuf,
    permission: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryEntry {
    pub info: Value,
    pub parts: Vec<Value>,
}

#[derive(Debug)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub before: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub tool: Option<Value>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Copy, Debug)]
pub enum Decision {
    AllowOnce,
    Reject,
}

pub struct Adapter {
    client: Client,
    origin: Url,
    password: String,
    profile: Profile,
    /// Keep a fence after timeouts or ambiguous cancellation. A caller can
    /// continue reconciliation by calling cancel again from the same scope.
    fenced: HashSet<String>,
}

impl Adapter {
    /// The caller owns the child process and supplies the endpoint it observed
    /// at startup. Start it using all four XDG roots from this profile.
    pub fn connect(profile: Profile, address: SocketAddrV4, password: String) -> Result<Self> {
        if *address.ip() != Ipv4Addr::LOCALHOST || address.port() == 0 || password.is_empty() {
            return Err(Error::InvalidInput(
                "OpenCode requires an authenticated IPv4 loopback endpoint",
            ));
        }
        let origin = Url::parse(&format!("http://{address}/"))
            .map_err(|_| Error::InvalidInput("Invalid OpenCode endpoint"))?;
        let adapter = Self {
            client: Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .connect_timeout(IO_TIMEOUT)
                .timeout(IO_TIMEOUT)
                .build()?,
            origin,
            password,
            profile,
            fenced: HashSet::new(),
        };
        let health: Health = adapter.json(
            "check server health",
            Method::GET,
            &["global", "health"],
            &[],
            None,
        )?;
        if !health.healthy {
            return Err(Error::InvalidResponse("OpenCode server reported unhealthy"));
        }
        let paths: NativePaths = adapter.json(
            "attest the route's profile",
            Method::GET,
            &["path"],
            &[],
            None,
        )?;
        if paths.directory != adapter.profile.owner().project()
            || paths.state != adapter.profile.state_home().join("opencode")
            || paths.config != adapter.profile.config_home().join("opencode")
        {
            return Err(Error::OwnerMismatch);
        }
        // These reads must work before a turn can be safely tracked or approved.
        // Session-specific operations verify their own replies when used.
        adapter.session_statuses()?;
        adapter.verify_authentication()?;
        adapter.permission_requests()?;
        Ok(adapter)
    }

    pub fn create(&self, owner: &Owner, title: &str) -> Result<Session> {
        self.create_with_permissions(owner, title, None)
    }

    fn create_with_permissions(
        &self,
        owner: &Owner,
        title: &str,
        permission: Option<Value>,
    ) -> Result<Session> {
        self.check_owner(owner)?;
        let mut body = json!({ "title": title });
        if let Some(permission) = &permission {
            body["permission"] = permission.clone();
        }
        let native: NativeSession = self.json(
            "create a session",
            Method::POST,
            &["session"],
            &[],
            Some(body),
        )?;
        check_permissions(native.permission.as_ref(), permission.as_ref())?;
        self.session(native)
    }

    /// Recover a session ID from the host's trusted durable registry.
    pub fn reopen_registered(&self, owner: &Owner, native_id: &str) -> Result<Session> {
        self.reopen_with_permissions(owner, native_id, None)
    }

    fn reopen_with_permissions(
        &self,
        owner: &Owner,
        native_id: &str,
        permission: Option<Value>,
    ) -> Result<Session> {
        self.check_owner(owner)?;
        check_id(native_id, "ses_")?;
        let native: NativeSession = self.json(
            "reopen the session",
            Method::GET,
            &["session", native_id],
            &[],
            None,
        )?;
        if native.id != native_id {
            return Err(Error::InvalidResponse(
                "OpenCode returned a different session",
            ));
        }
        check_permissions(native.permission.as_ref(), permission.as_ref())?;
        self.session(native)
    }

    pub fn push_text(
        &self,
        owner: &Owner,
        session: &Session,
        message_id: &str,
        text: &str,
    ) -> Result<()> {
        let id = self.mutable_session(owner, session)?;
        check_id(message_id, "msg_")?;
        self.request(
            "send a prompt",
            Method::POST,
            &["session", id, "prompt_async"],
            &[],
            Some(json!({
                "messageID": message_id,
                "model": { "providerID": "ox", "modelID": self.profile.owner().billing_alias() },
                "parts": [{ "type": "text", "text": text }]
            })),
            (StatusCode::NO_CONTENT, IO_TIMEOUT),
        )?;
        Ok(())
    }

    pub fn history(
        &self,
        owner: &Owner,
        session: &Session,
        limit: usize,
        before: Option<&str>,
    ) -> Result<HistoryPage> {
        let id = self.check_session(owner, session)?;
        if !(1..=MAX_PAGE).contains(&limit) || before.is_some_and(|cursor| cursor.len() > 4096) {
            return Err(Error::InvalidInput(
                "OpenCode history requires a bounded page and valid cursor",
            ));
        }
        let count = limit.to_string();
        let mut query = vec![("limit", count.as_str())];
        if let Some(before) = before {
            query.push(("before", before));
        }
        let operation = "read session history";
        let response = self.request(
            operation,
            Method::GET,
            &["session", id, "message"],
            &query,
            None,
            (StatusCode::OK, IO_TIMEOUT),
        )?;
        let before = response
            .headers()
            .get("x-next-cursor")
            .map(|header| {
                header
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| Error::InvalidResponse("Invalid OpenCode history cursor"))
            })
            .transpose()?;
        let entries: Vec<HistoryEntry> = read_json(response, operation)?;
        if entries.len() > limit
            || entries.iter().any(|entry| {
                !entry.info["id"]
                    .as_str()
                    .is_some_and(|message| check_id(message, "msg_").is_ok())
                    || entry.info["sessionID"] != id
                    || entry.parts.iter().any(|part| {
                        part["sessionID"] != id || part["messageID"] != entry.info["id"]
                    })
            })
        {
            return Err(Error::InvalidResponse(
                "OpenCode history contained records outside the requested page or session",
            ));
        }
        Ok(HistoryPage { entries, before })
    }

    pub fn events(&self, owner: &Owner, session: &Session) -> Result<Events> {
        let id = self.check_session(owner, session)?;
        self.open_events(Some(id.to_owned()))
    }

    pub fn route_events(&self, owner: &Owner) -> Result<Events> {
        self.check_owner(owner)?;
        self.open_events(None)
    }

    fn open_events(&self, session: Option<String>) -> Result<Events> {
        let response = self.request(
            "open the native event stream",
            Method::GET,
            &["event"],
            &[],
            None,
            (StatusCode::OK, EVENT_TIMEOUT),
        )?;
        Events::new(response, session)
    }

    pub fn pending(&self, owner: &Owner, session: &Session) -> Result<Vec<PermissionRequest>> {
        let id = self.mutable_session(owner, session)?;
        self.pending_in(id)
    }

    pub fn is_idle(&self, owner: &Owner, session: &Session) -> Result<bool> {
        let id = self.check_session(owner, session)?;
        Ok(self
            .session_statuses()?
            .get(id)
            .is_none_or(|status| status.kind == "idle"))
    }

    fn session_statuses(&self) -> Result<HashMap<String, NativeStatus>> {
        self.json(
            "read session status",
            Method::GET,
            &["session", "status"],
            &[],
            None,
        )
    }

    pub fn answer(
        &self,
        owner: &Owner,
        session: &Session,
        request_id: &str,
        decision: Decision,
    ) -> Result<()> {
        let id = self.mutable_session(owner, session)?;
        check_id(request_id, "per_")?;
        if !self.pending_in(id)?.iter().any(|ask| ask.id == request_id) {
            return Err(Error::UnknownPermission);
        }
        match self.reply(request_id, decision) {
            Err(Error::NativeStatus {
                status: StatusCode::NOT_FOUND,
                ..
            }) => Err(Error::UnknownPermission),
            result => result,
        }
    }

    /// Stop the turn and resolve any native asks left behind by stock abort.
    /// The host must also fence its own receipts before calling this method.
    pub fn cancel(&mut self, owner: &Owner, session: &Session) -> Result<()> {
        let id = self.check_session(owner, session)?.to_owned();
        self.fenced.insert(id.clone());
        let aborted: bool = self.json(
            "abort the turn",
            Method::POST,
            &["session", &id, "abort"],
            &[],
            Some(json!({})),
        )?;
        if !aborted {
            return Err(Error::InvalidResponse(
                "OpenCode did not acknowledge cancellation",
            ));
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        while !self.is_idle(owner, session)? {
            if Instant::now() >= deadline {
                return Err(Error::BlockedByCancellation);
            }
            thread::sleep(Duration::from_millis(25));
        }
        for ask in self.pending_in(&id)? {
            if Instant::now() >= deadline {
                return Err(Error::BlockedByCancellation);
            }
            if let Err(error) = self.reply(&ask.id, Decision::Reject)
                && !matches!(
                    error,
                    Error::NativeStatus {
                        status: StatusCode::NOT_FOUND,
                        ..
                    }
                )
            {
                return Err(error);
            }
        }
        if !self.pending_in(&id)?.is_empty() {
            return Err(Error::BlockedByCancellation);
        }
        self.fenced.remove(&id);
        Ok(())
    }

    fn check_owner(&self, owner: &Owner) -> Result<()> {
        if owner != self.profile.owner() {
            return Err(Error::OwnerMismatch);
        }
        Ok(())
    }

    fn session(&self, native: NativeSession) -> Result<Session> {
        check_id(&native.id, "ses_")?;
        if native.directory != self.profile.owner().project() {
            return Err(Error::OwnerMismatch);
        }
        Ok(Session {
            id: native.id,
            owner: self.profile.owner().clone(),
            profile_root: self.profile.root().to_owned(),
        })
    }

    fn check_session<'a>(&self, owner: &Owner, session: &'a Session) -> Result<&'a str> {
        self.check_owner(owner)?;
        if &session.owner != owner || session.profile_root != self.profile.root() {
            return Err(Error::OwnerMismatch);
        }
        Ok(&session.id)
    }

    fn mutable_session<'a>(&self, owner: &Owner, session: &'a Session) -> Result<&'a str> {
        let id = self.check_session(owner, session)?;
        if self.fenced.contains(id) {
            return Err(Error::BlockedByCancellation);
        }
        Ok(id)
    }

    fn pending_in(&self, session_id: &str) -> Result<Vec<PermissionRequest>> {
        let scoped: Vec<_> = self
            .permission_requests()?
            .into_iter()
            .filter(|ask| ask.session_id == session_id)
            .collect();
        for ask in &scoped {
            check_id(&ask.id, "per_")?;
        }
        Ok(scoped)
    }

    fn permission_requests(&self) -> Result<Vec<PermissionRequest>> {
        self.json(
            "list pending permissions",
            Method::GET,
            &["permission"],
            &[],
            None,
        )
    }

    fn reply(&self, request: &str, decision: Decision) -> Result<()> {
        let reply = match decision {
            Decision::AllowOnce => "once",
            Decision::Reject => "reject",
        };
        let ok: bool = self.json(
            "resolve a permission",
            Method::POST,
            &["permission", request, "reply"],
            &[],
            Some(json!({ "reply": reply })),
        )?;
        if ok {
            Ok(())
        } else {
            Err(Error::InvalidResponse(
                "OpenCode did not acknowledge the permission reply",
            ))
        }
    }

    fn json<T: DeserializeOwned>(
        &self,
        operation: &'static str,
        method: Method,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<T> {
        let response = self.request(
            operation,
            method,
            path,
            query,
            body,
            (StatusCode::OK, IO_TIMEOUT),
        )?;
        read_json(response, operation)
    }

    fn verify_authentication(&self) -> Result<()> {
        let response = self
            .request_builder(Method::GET, &["session", "status"], &[])?
            .send()?;
        if !matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(Error::InvalidResponse(
                "Installed OpenCode did not require authentication for session status",
            ));
        }
        Ok(())
    }

    fn request_builder(
        &self,
        method: Method,
        path: &[&str],
        query: &[(&str, &str)],
    ) -> Result<RequestBuilder> {
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| Error::InvalidInput("Invalid OpenCode endpoint"))?
            .pop_if_empty()
            .extend(path);
        url.query_pairs_mut()
            .append_pair("directory", self.profile.owner().directory())
            .extend_pairs(query.iter().copied());
        Ok(self.client.request(method, url))
    }

    fn request(
        &self,
        operation: &'static str,
        method: Method,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
        (status, timeout): (StatusCode, Duration),
    ) -> Result<Response> {
        let mut request = self
            .request_builder(method, path, query)?
            .basic_auth("opencode", Some(&self.password))
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send()?;
        if response.status() != status {
            return Err(Error::NativeStatus {
                operation,
                status: response.status(),
            });
        }
        Ok(response)
    }
}

#[derive(Deserialize)]
struct Health {
    healthy: bool,
}

#[derive(Deserialize)]
struct NativeStatus {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct NativePaths {
    directory: PathBuf,
    state: PathBuf,
    config: PathBuf,
}

fn read_json<T: DeserializeOwned>(response: Response, operation: &'static str) -> Result<T> {
    let mut body = Vec::new();
    response
        .take((MAX_WIRE_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_WIRE_BYTES {
        return Err(Error::InvalidResponse(
            "OpenCode JSON response exceeds the host transport limit",
        ));
    }
    serde_json::from_slice(&body).map_err(|_| Error::IncompatibleReply(operation))
}

fn check_id(id: &str, prefix: &str) -> Result<()> {
    if id.len() > 256
        || !id.strip_prefix(prefix).is_some_and(|tail| {
            !tail.is_empty()
                && tail
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
    {
        return Err(Error::InvalidInput("Invalid OpenCode identifier"));
    }
    Ok(())
}

fn check_permissions(actual: Option<&Value>, expected: Option<&Value>) -> Result<()> {
    if expected.is_some() && actual != expected {
        return Err(Error::InvalidResponse(
            "OpenCode did not attest the host's session permissions",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
