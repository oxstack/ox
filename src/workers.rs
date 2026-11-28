//! Launch-scoped MCP tools for sending a task to an API model from a
//! subscription session. The stock client retains its own inference and tools.

mod jobs;
mod picker;

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, ORIGIN};
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{OnceCell, Semaphore};

use crate::clients::ClientKind;
use crate::provider::{CodeoxConfig, RelayError, SavedKeys};
use crate::{App, Mode, RelayBody};

const TOKEN_ENV: &str = "OX_WORKER_SESSION";
const SERVER: &str = "ox_workers";
const MAX_CALL_BYTES: usize = 128 * 1024;
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 64 * 1024;
const PAGE_SIZE: usize = 40;
const WORKER_TIMEOUT: Duration = Duration::from_secs(90);
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

pub(crate) struct Relay {
    listener: TcpListener,
    app: Option<App>,
    token: String,
}

impl Relay {
    pub(crate) fn new(listener: TcpListener, app: App, token: String) -> Self {
        Self {
            listener,
            app: Some(app),
            token,
        }
    }

    pub(crate) async fn load(home: &Path) -> Result<Option<Self>, RelayError> {
        if crate::native::title_generation() {
            return Ok(None);
        }
        let path = CodeoxConfig::path(home);
        if !crate::host_lifetime::hosted() && !path.try_exists()? {
            return Ok(None);
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        Ok(Some(Self {
            listener,
            app: None,
            token: crate::random_token()?,
        }))
    }

    pub(crate) async fn launch(
        self,
        mut command: Command,
        args: &[OsString],
        home: &Path,
        client: ClientKind,
    ) -> Result<u8, RelayError> {
        let (mcp, gateway) = self.attach(args, home, client, Tools::All)?;
        command.env(TOKEN_ENV, &gateway.token);
        command.args(mcp);
        command.args(args);
        crate::exempt_loopback_proxy(&mut command);
        if gateway.service.app.get().is_some() || CodeoxConfig::path(home).is_file() {
            eprintln!(
                "ox: API workers enabled. Ask for the ox model list or name an API model to delegate to."
            );
        } else {
            eprintln!(
                "ox: Workers and OX Memory enabled. Ask for a second opinion or save a project preference."
            );
        }
        tokio::select! {
            result = crate::native::launch(command) => result,
            result = gateway.serve() => Err(result),
        }
    }

    /// Prepare this gateway for a client launch: the stock client's MCP
    /// arguments, and the gateway to serve while the client runs. The client
    /// also needs `TOKEN_ENV` set to [`Gateway::token`].
    pub(crate) fn attach(
        self,
        args: &[OsString],
        home: &Path,
        client: ClientKind,
        tools: Tools,
    ) -> Result<(Vec<OsString>, Gateway), RelayError> {
        let origin = format!("http://{}", self.listener.local_addr()?);
        let url = format!("{origin}/mcp");
        let mcp = match client {
            ClientKind::Codex => [
                ("url", json!(url)),
                ("bearer_token_env_var", json!(TOKEN_ENV)),
                ("tool_timeout_sec", json!(120)),
            ]
            .into_iter()
            .flat_map(|(field, value)| {
                [
                    OsString::from("-c"),
                    OsString::from(format!("mcp_servers.{SERVER}.{field}={value}")),
                ]
            })
            .collect(),
            ClientKind::Claude => {
                let config = json!({
                    "mcpServers": {
                        (SERVER): {
                            "type": "http",
                            "url": url,
                            "headers": {"Authorization": format!("Bearer ${{{TOKEN_ENV}}}")},
                        }
                    }
                });
                vec![OsString::from(format!("--mcp-config={config}"))]
            }
        };
        let token = self.token.clone();
        let service = Arc::new(Service {
            app: OnceCell::new_with(self.app.map(|app| Ok(prepare_app(app, home, &self.token)))),
            local_auth: crate::provider::bearer(&self.token)?,
            token: self.token,
            origin,
            home: home.to_owned(),
            cwd: project_directory(args, client)?,
            session: session_hint(args, client),
            observed: Mutex::new(None),
            unbound: std::sync::atomic::AtomicBool::new(false),
            host_policy: CodeoxConfig::load(&CodeoxConfig::path(home))
                .map(|config| config.workers_host)
                .unwrap_or_default(),
            // Private to this launch. The host stores only a digest of it.
            launch: crate::random_token()?,
            client,
            slots: Semaphore::new(4),
            jobs: Mutex::new(jobs::Jobs::default()),
            tools,
        });
        Ok((
            mcp,
            Gateway {
                listener: self.listener,
                service,
                token,
            },
        ))
    }
}

/// Hosted API sessions can manage their own workers and read handoff history.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tools {
    All,
    Hosted,
}

pub(crate) struct Gateway {
    listener: TcpListener,
    service: Arc<Service>,
    pub(crate) token: String,
}

impl Gateway {
    /// The environment variable and value the client needs for this gateway.
    pub(crate) fn env(&self) -> (&'static str, &str) {
        (TOKEN_ENV, &self.token)
    }
    pub(crate) async fn serve(self) -> RelayError {
        let service = self.service;
        crate::serve_requests(self.listener, move |request| {
            let service = Arc::clone(&service);
            async move { service.handle(request).await }
        })
        .await
        .unwrap_err()
        .into()
    }
}

fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The native session a terminal launch resumes, from its own arguments. The host
/// uses it only to let a resumed parent see the workers it started earlier.
fn session_hint(args: &[OsString], client: ClientKind) -> Option<String> {
    let args: Vec<&str> = args
        .iter()
        .take_while(|arg| *arg != "--")
        .filter_map(|a| a.to_str())
        .collect();
    let valid = |id: &&str| {
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    match client {
        ClientKind::Claude => args
            .windows(2)
            .find_map(|w| {
                matches!(w[0], "--resume" | "-r" | "--session-id")
                    .then_some(w[1])
                    .filter(valid)
                    .map(str::to_owned)
            })
            .or_else(|| {
                args.iter().find_map(|a| {
                    a.strip_prefix("--resume=")
                        .or_else(|| a.strip_prefix("--session-id="))
                        .filter(valid)
                        .map(str::to_owned)
                })
            }),
        ClientKind::Codex => args
            .windows(2)
            .find(|w| w[0] == "resume" && valid(&w[1]))
            .map(|w| w[1].to_owned()),
    }
}

fn project_directory(args: &[OsString], client: ClientKind) -> Result<PathBuf, RelayError> {
    let cwd = std::env::current_dir()?;
    if !matches!(client, ClientKind::Codex) {
        return Ok(cwd);
    }
    let mut selected = None;
    let mut args = args.iter().take_while(|arg| *arg != "--");
    while let Some(arg) = args.next() {
        if arg == "-C" || arg == "--cd" {
            selected = args.next().map(PathBuf::from);
        } else if let Some(path) = arg.to_str().and_then(|arg| arg.strip_prefix("--cd=")) {
            selected = Some(PathBuf::from(path));
        }
    }
    match selected {
        Some(path) => Ok(cwd.join(path).canonicalize()?),
        None => Ok(cwd),
    }
}

struct Service {
    app: OnceCell<Result<App, String>>,
    local_auth: HeaderValue,
    token: String,
    origin: String,
    home: PathBuf,
    cwd: PathBuf,
    session: Option<String>,
    /// The native session first seen in a stock client's MCP request.
    observed: Mutex<Option<String>>,
    /// A host-owned worker was launched before this launch knew its session.
    unbound: std::sync::atomic::AtomicBool,
    host_policy: crate::provider::WorkersHost,
    launch: String,
    client: ClientKind,
    slots: Semaphore,
    jobs: Mutex<jobs::Jobs>,
    tools: Tools,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListModels {
    #[serde(default)]
    query: String,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RecallMemory {
    #[serde(default)]
    query: String,
    scope: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveMemory {
    text: Option<String>,
    id: Option<String>,
    #[serde(default)]
    forget: bool,
    scope: Option<String>,
    kind: Option<String>,
    sensitivity: Option<String>,
    #[serde(default)]
    pin: bool,
    #[serde(default)]
    tags: Vec<String>,
}

fn memory_sensitivity_ceiling() -> ox_memory::Sensitivity {
    let local_only = std::env::var("OX_MEMORY_ALLOW_LOCAL_ONLY").ok();
    let cap = std::env::var("OX_MEMORY_MAX_SENSITIVITY").ok();
    recall_ceiling(local_only.as_deref(), cap.as_deref())
}

fn recall_ceiling(local_only: Option<&str>, cap: Option<&str>) -> ox_memory::Sensitivity {
    use ox_memory::Sensitivity;

    let assumed = if local_only.is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes")) {
        Sensitivity::LocalOnly
    } else {
        Sensitivity::Private
    };
    cap.and_then(|v| Sensitivity::parse(v).ok())
        .map_or(assumed, |cap| assumed.min(cap))
}

impl Service {
    async fn recall_memory(
        &self,
        req: RecallMemory,
        thread: Option<&str>,
    ) -> Result<Value, String> {
        let report = crate::host_lifetime::MemoryReceipt::begin(&self.cwd, thread).await;
        let cwd = self.cwd.clone();
        let ceiling = memory_sensitivity_ceiling();
        tokio::task::spawn_blocking(move || {
            let store = ox_memory::MemoryStore::open_default().map_err(|e| e.to_string())?;
            let project = ox_memory::canonical_project_id(&cwd);
            let host = ox_memory::default_host_id();
            let include_all_scopes = req.scope.as_deref() == Some("all");
            let scope = match req.scope.as_deref() {
                None | Some("all") => None,
                Some("global") => Some(ox_memory::Scope::Global),
                Some("project") => Some(ox_memory::Scope::Project(project.clone())),
                Some("host") => Some(ox_memory::Scope::Host(host.clone())),
                Some(other) => {
                    return Err(format!(
                        "Invalid scope '{other}'. Expected all, global, project, or host."
                    ));
                }
            };
            let kind = req
                .kind
                .as_deref()
                .map(ox_memory::Kind::parse)
                .transpose()
                .map_err(|e| e.to_string())?;
            let limit = req.limit.unwrap_or(6).clamp(1, 20);
            let hits = store
                .recall(&ox_memory::RecallQuery {
                    text: req.query,
                    project: Some(project.clone()),
                    host: Some(host),
                    scope,
                    include_all_scopes,
                    kind,
                    max_sensitivity: ceiling,
                    limit,
                })
                .map_err(|e| e.to_string())?;
            let memories: Vec<Value> = hits
                .into_iter()
                .map(|h| {
                    let e = h.entry;
                    json!({
                        "id": e.id,
                        "scope": e.scope.short_label(),
                        "kind": e.kind.as_str(),
                        "sensitivity": e.sensitivity.as_str(),
                        "pin": e.pin,
                        "tags": e.tags,
                        "text": e.text,
                    })
                })
                .collect();
            if let Some(report) = report {
                report.finish(ox_memory::Receipt::Recalled {
                    count: memories.len() as u8,
                });
            }
            Ok(json!({
                "project": project,
                "memories": memories,
            }))
        })
        .await
        .map_err(|_| "Memory recall task failed.".to_string())?
    }

    async fn save_memory(&self, req: SaveMemory, thread: Option<&str>) -> Result<Value, String> {
        let report = crate::host_lifetime::MemoryReceipt::begin(&self.cwd, thread).await;
        let secrets = self.worker_secrets().await.unwrap_or_default();
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let store = ox_memory::MemoryStore::open_default().map_err(|e| e.to_string())?;
            let host = ox_memory::default_host_id();
            if req.forget {
                let id = req
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "Provide id when forget is true.".to_string())?;
                let removed = store.forget(id, &host).map_err(|e| e.to_string())?;
                if let Some(report) = report {
                    report.finish(ox_memory::Receipt::Forgotten {
                        scope: (&removed.scope).into(),
                    });
                }
                return Ok(json!({
                    "action": "forgotten",
                    "id": removed.id,
                }));
            }
            let raw_text = req
                .text
                .as_deref()
                .ok_or_else(|| "Provide text to save a memory.".to_string())?;
            let text = crate::upstream::redact(raw_text, &secrets).into_owned();
            let tags = req
                .tags
                .iter()
                .map(|t| crate::upstream::redact(t, &secrets).into_owned())
                .collect();
            let project = ox_memory::canonical_project_id(&cwd);
            let scope = match req.scope.as_deref().unwrap_or("project") {
                "global" => ox_memory::Scope::Global,
                "project" => ox_memory::Scope::Project(project),
                "host" => ox_memory::Scope::Host(host.clone()),
                other => {
                    return Err(format!(
                        "Invalid scope '{other}'. Expected global, project, or host."
                    ));
                }
            };
            let kind = req
                .kind
                .as_deref()
                .map(ox_memory::Kind::parse)
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or(ox_memory::Kind::Fact);
            let sensitivity = req
                .sensitivity
                .as_deref()
                .map(ox_memory::Sensitivity::parse)
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or(ox_memory::Sensitivity::Private);
            let outcome = store
                .upsert(ox_memory::NewMemory {
                    id: req.id,
                    scope,
                    kind,
                    sensitivity,
                    text,
                    tags,
                    pin: req.pin,
                    source_host: host,
                })
                .map_err(|e| e.to_string())?;
            let e = outcome.entry();
            if let Some(report) = report {
                use ox_memory::{Receipt, UpsertOutcome};
                let scope = (&e.scope).into();
                let receipt = match outcome {
                    UpsertOutcome::Created(_) => Receipt::Saved { scope },
                    UpsertOutcome::Updated(_) => Receipt::Updated { scope },
                    UpsertOutcome::Deduplicated(_) => Receipt::Deduplicated { scope },
                };
                report.finish(receipt);
            }
            Ok(json!({
                "action": outcome.action(),
                "id": e.id,
                "scope": e.scope.short_label(),
                "kind": e.kind.as_str(),
                "sensitivity": e.sensitivity.as_str(),
                "pin": e.pin,
            }))
        })
        .await
        .map_err(|_| "Memory save task failed.".to_string())?
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadThread {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    model: String,
    task: String,
}

impl Service {
    /// A subscription launch can use host-owned jobs and memory without an API registry.
    async fn configured_app(&self) -> Result<Option<&App>, String> {
        if !matches!(self.app.get(), Some(Ok(_)))
            && !CodeoxConfig::path(&self.home)
                .try_exists()
                .map_err(|error| error.to_string())?
        {
            return Ok(None);
        }
        self.app().await.map(Some)
    }

    async fn worker_secrets(&self) -> Result<Arc<Vec<String>>, String> {
        Ok(self
            .configured_app()
            .await?
            .map(|app| Arc::clone(&app.secrets))
            .unwrap_or_default())
    }

    async fn app(&self) -> Result<&App, String> {
        self.app
            .get_or_init(|| async {
                let result = async {
                    let config = CodeoxConfig::load(&CodeoxConfig::path(&self.home))?;
                    let keys = SavedKeys::for_config(&self.home, &config)?;
                    App::codeox(
                        config,
                        Default::default(),
                        &self.token,
                        crate::upstream_client()?,
                        MAX_CALL_BYTES,
                        &|name| keys.get(name),
                        None,
                        crate::Catalog::Shared,
                    )
                    .await
                }
                .await;
                result
                    .map(|app| prepare_app(app, &self.home, &self.token))
                    .map_err(|_| {
                        "ox could not load the API provider registry. Run ox doctor.".to_owned()
                    })
            })
            .await
            .as_ref()
            .map_err(Clone::clone)
    }

    async fn handle(&self, request: Request<Incoming>) -> Response<RelayBody> {
        // Stock Claude Code may name its session on outgoing requests. Treat it
        // only as an observation of this launch's own session.
        let header_session = request
            .headers()
            .get("x-claude-code-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| valid_session(s))
            .map(str::to_owned);
        if request.uri().path() != "/mcp" {
            return crate::error_response(StatusCode::NOT_FOUND, "Unknown worker endpoint");
        }
        if request
            .headers()
            .get_all(ORIGIN)
            .iter()
            .any(|origin| origin != self.origin.as_str())
        {
            return crate::error_response(StatusCode::FORBIDDEN, "Worker origin rejected");
        }
        if !request
            .headers()
            .get(AUTHORIZATION)
            .is_some_and(|auth| auth.as_bytes().ct_eq(self.local_auth.as_bytes()).into())
        {
            return crate::error_response(StatusCode::UNAUTHORIZED, "Worker authentication failed");
        }
        if request.method() != Method::POST {
            let mut response = crate::bytes_response(
                StatusCode::METHOD_NOT_ALLOWED,
                Bytes::new(),
                "application/json",
            );
            response
                .headers_mut()
                .insert(hyper::header::ALLOW, HeaderValue::from_static("POST"));
            return response;
        }
        if request
            .headers()
            .get("mcp-protocol-version")
            .is_some_and(|version| {
                !version
                    .to_str()
                    .is_ok_and(|version| PROTOCOL_VERSIONS.contains(&version))
            })
        {
            return crate::error_response(
                StatusCode::BAD_REQUEST,
                "Unsupported MCP protocol version",
            );
        }
        let bytes = match crate::read_bounded(request.into_body(), MAX_CALL_BYTES).await {
            Ok(bytes) => bytes,
            Err(response) => return response,
        };
        let Ok(call) = serde_json::from_slice::<Value>(&bytes) else {
            return rpc_error(Value::Null, -32700, "Invalid JSON");
        };
        let id = &call["id"];
        let Some(method) = call["method"].as_str().filter(|_| call["jsonrpc"] == "2.0") else {
            return rpc_error(id.clone(), -32600, "Invalid JSON-RPC request");
        };
        if call.get("id").is_none() {
            return crate::bytes_response(StatusCode::ACCEPTED, Bytes::new(), "application/json");
        }
        let params = &call["params"];
        // Stock Codex 0.171.0 and later put the calling thread in every tools/call
        // `_meta.threadId`. It identifies the parent exactly on a shared app-server.
        let thread = params["_meta"]["threadId"]
            .as_str()
            .filter(|s| valid_session(s))
            .map(str::to_owned)
            .or(header_session);
        if method == "tools/call" {
            self.observe(thread.as_deref()).await;
        }
        let thread = thread.as_deref();
        let result = match method {
            "initialize" => initialize(params),
            "ping" => json!({}),
            "tools/list" if self.tools == Tools::Hosted => {
                let mut all = tools();
                if let Some(list) = all["tools"].as_array_mut() {
                    list.retain(|t| t["name"] != "delegate_to_ox_model");
                }
                all
            }
            "tools/list" => tools(),
            "tools/call"
                if self.tools == Tools::Hosted
                    && params["name"].as_str() == Some("delegate_to_ox_model") =>
            {
                return rpc_error(id.clone(), -32602, "Unknown worker tool");
            }
            "tools/call" => match params["name"].as_str() {
                Some("list_ox_models") => match serde_json::from_value(
                    params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                ) {
                    Ok(filters) => tool_result(self.list_models(filters).await),
                    Err(_) => return rpc_error(id.clone(), -32602, "Invalid model list filters"),
                },
                Some("delegate_to_ox_model") => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(task) => tool_result(self.delegate(task).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Provide a model alias and a task",
                            );
                        }
                    }
                }
                Some("start_ox_worker") => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(request) => tool_result(self.start_job(request, thread).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Specify a runtime, mode, and task",
                            );
                        }
                    }
                }
                Some("list_ox_hosts") => tool_result(self.list_hosts(thread).await),
                Some("list_ox_workers") => tool_result(self.list_jobs(thread).await),
                Some("read_ox_thread") => {
                    match serde_json::from_value::<ReadThread>(
                        params
                            .get("arguments")
                            .cloned()
                            .unwrap_or_else(|| json!({})),
                    ) {
                        Ok(read) => tool_result(self.read_thread(read, thread).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Use session, item, after and limit to read a handoff source",
                            );
                        }
                    }
                }
                Some("steer_ox_worker") => {
                    let result = async {
                        let request: jobs::Steer =
                            serde_json::from_value(params["arguments"].clone())
                                .map_err(|_| "Provide a worker job ID and message.".to_string())?;
                        let secrets = self.worker_secrets().await?;
                        if !self.jobs.lock().unwrap().has(request.job_id) {
                            let text = crate::upstream::redact(&request.text, &secrets);
                            return self
                                .host_call(
                                    json!({"op":"steer","job_id":request.job_id,"text":text}),
                                    thread,
                                )
                                .await;
                        }
                        let received = self.jobs.lock().unwrap().steer(request, &secrets)?;
                        tokio::time::timeout(Duration::from_secs(30), received)
                            .await
                            .map_err(|_| "Timed out waiting for the worker's acknowledgement.")?
                            .map_err(
                                |_| "The worker finished before acknowledging the instruction.",
                            )?
                    }
                    .await;
                    tool_result(result)
                }
                Some("answer_ox_worker") => {
                    let result = async {
                        let request: jobs::Answer =
                            serde_json::from_value(params["arguments"].clone()).map_err(|_| {
                                "Provide job_id, request_id and either choice or answers."
                                    .to_string()
                            })?;
                        if self.jobs.lock().unwrap().has(request.job_id) {
                            return Err("This worker runs without the OX session host and has no requests to answer: it declines approvals itself.".to_string());
                        }
                        let secrets = self.worker_secrets().await?;
                        let answers = request.answers.map(|answers| {
                            answers
                                .iter()
                                .map(|a| crate::upstream::redact(a, &secrets).into_owned())
                                .collect::<Vec<_>>()
                        });
                        self.host_call(
                            json!({"op":"answer","job_id":request.job_id,"request":request.request_id,
                                "choice":request.choice,"answers":answers}),
                            thread,
                        )
                        .await
                    }
                    .await;
                    tool_result(result)
                }
                Some(tool @ ("ox_worker_status" | "cancel_ox_worker")) => {
                    match serde_json::from_value(params["arguments"].clone()) {
                        Ok(job) => {
                            let job: jobs::JobId = job;
                            let local = self.jobs.lock().unwrap().has(job.job_id);
                            let result = if !local {
                                let op = if tool == "cancel_ox_worker" {
                                    "cancel"
                                } else {
                                    "status"
                                };
                                self.host_call(json!({"op":op,"job_id":job.job_id}), thread)
                                    .await
                            } else {
                                let mut jobs = self.jobs.lock().unwrap();
                                if tool == "cancel_ox_worker" {
                                    jobs.cancel(job)
                                } else {
                                    jobs.status(job)
                                }
                            };
                            tool_result(result)
                        }
                        Err(_) => {
                            return rpc_error(id.clone(), -32602, "Provide the worker job ID");
                        }
                    }
                }
                Some("recall_ox_memory") => match serde_json::from_value::<RecallMemory>(
                    params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                ) {
                    Ok(req) => tool_result(self.recall_memory(req, thread).await),
                    Err(_) => {
                        return rpc_error(
                            id.clone(),
                            -32602,
                            "Use query, scope, kind, and limit to recall OX memories",
                        );
                    }
                },
                Some("save_ox_memory") => {
                    match serde_json::from_value::<SaveMemory>(params["arguments"].clone()) {
                        Ok(req) => tool_result(self.save_memory(req, thread).await),
                        Err(_) => {
                            return rpc_error(
                                id.clone(),
                                -32602,
                                "Provide text (or id with forget: true) to save or remove an OX memory",
                            );
                        }
                    }
                }
                _ => return rpc_error(id.clone(), -32602, "Unknown worker tool"),
            },
            _ => return rpc_error(id.clone(), -32601, "Method not found"),
        };
        crate::json_response(
            StatusCode::OK,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )
    }

    async fn start_job(
        &self,
        mut request: jobs::Start,
        thread: Option<&str>,
    ) -> Result<Value, String> {
        request.validate()?;

        let host = crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?;
        let rule = if request.host.is_none() && request.needs.is_none() {
            crate::worker_placement::rule(&self.cwd, request.placement.as_deref())?
        } else {
            Default::default()
        };
        let target = request.host.as_deref().or(rule.host.as_deref());
        let mut remote =
            target.is_some_and(|h| h != "parent") || request.needs.is_some() || rule.os.is_some();
        if host.is_none()
            && request.needs.is_none()
            && matches!(target, None | Some("auto" | "parent"))
            && rule
                .os
                .as_ref()
                .is_none_or(|os| os.eq_ignore_ascii_case(std::env::consts::OS))
        {
            remote = false;
        }
        if request.placement.is_none() && (rule.host.is_some() || rule.os.is_some()) && remote {
            request.placement = Some("default".into());
        }
        if !remote && let Some(model) = &request.subscription_model {
            self.check_subscription_model(model).await?;
        }
        let secrets = if !remote && let Some(model) = &request.model {
            let app = self.configured_app().await?.ok_or(
                "No API provider is configured on this host. Use a subscription worker or configure an API provider in OX Settings.",
            )?;
            if let Mode::Codeox {
                vendors, routes, ..
            } = &app.mode
            {
                crate::resolve_model(vendors, routes, model).map_err(|(_, error)| {
                    crate::upstream::redact(&error, &app.secrets).into_owned()
                })?;
            }
            Arc::clone(&app.secrets)
        } else {
            self.worker_secrets().await?
        };
        // A running session host owns the worker, so it outlives this helper and
        // its approvals reach the human clients. Without one, run it here as before.
        if let Some(host) = host {
            let start = request.host_start(&secrets);
            let caller = self.caller(thread);
            let unbound = caller["session"].is_null();
            let reply = host
                .send(
                    self.launch.clone(),
                    caller,
                    thread.map(str::to_owned),
                    Some(start),
                    None,
                )
                .await
                .map_err(|e| crate::upstream::redact(&e, &secrets).into_owned())?;
            if unbound {
                self.unbound
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            let mut reply = redact_value(reply, &secrets);
            reply["approvals"] = json!(
                "This worker is owned by the OX session host. Its approvals and questions appear in OX Desktop and on paired phones. When ox_worker_status lists requests with parent_may_answer, you may answer them with answer_ox_worker; otherwise tell the user where to answer."
            );
            return Ok(reply);
        }
        if remote {
            return Err("Starting a worker on another host requires the parent's OX host. Start OX Desktop or run ox host install.".into());
        }
        self.jobs
            .lock()
            .unwrap()
            .start(request, &self.home, &self.cwd, secrets)
    }

    /// Refuse a subscription model the account's picker does not offer, before
    /// anything starts. A stale cache is refreshed once before refusing. When the
    /// picker cannot be read here, the worker's own process still checks the pin
    /// before its first turn, so nothing runs on another model either way.
    async fn check_subscription_model(&self, model: &str) -> Result<(), String> {
        if !crate::native::has_saved_account(ClientKind::Claude, &self.home) {
            return Err("Run ox login claude on this host to use a Claude subscription worker. Nothing was started.".into());
        }
        for refresh in [false, true] {
            match picker::picker(&self.home, refresh).await {
                Ok(models) => match crate::claude_models::resolve(&models, model) {
                    Ok(_) => return Ok(()),
                    Err(error) if refresh => return Err(error),
                    Err(_) => {}
                },
                Err(_) if !refresh => {}
                Err(_) => return Ok(()),
            }
        }
        Ok(())
    }

    /// The session this launch names for a call. Codex reports the exact calling
    /// thread, which can be a subagent. A Claude launch has one session, fixed by
    /// its `--session-id` or `--resume` argument before the first turn. A later
    /// ID for the same Claude launch, such as after `/clear`, never replaces it;
    /// the launch token still covers that work while the launch lives.
    fn caller(&self, thread: Option<&str>) -> Value {
        let thread = thread.map(str::to_owned);
        let session = match self.client {
            ClientKind::Codex => thread.or_else(|| self.session.clone()),
            ClientKind::Claude => self.session.clone().or(thread),
        }
        .or_else(|| self.observed.lock().unwrap().clone());
        json!({
            "project": self.cwd,
            "runtime": match self.client {
                ClientKind::Codex => "codex",
                ClientKind::Claude => "claude",
            },
            "session": session,
            "launch": self.launch,
        })
    }

    /// Record this launch's native session the first time a request shows it,
    /// and bind it to host-owned workers that were launched before it was known.
    async fn observe(&self, thread: Option<&str>) {
        let Some(thread) = thread else { return };
        if self.session.is_some() {
            return;
        }
        {
            let mut observed = self.observed.lock().unwrap();
            if observed.is_some() {
                return;
            }
            *observed = Some(thread.to_owned());
        }
        if self
            .unbound
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            && let Ok(Some(host)) =
                crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)
            && matches!(host, crate::host_lifetime::HostLink::External { .. })
        {
            let _ = host
                .send(
                    self.launch.clone(),
                    self.caller(Some(thread)),
                    Some(thread.to_owned()),
                    None,
                    Some(json!({"op":"bind","session":thread})),
                )
                .await;
        }
    }

    async fn host_call(&self, call: Value, thread: Option<&str>) -> Result<Value, String> {
        let secrets = self.worker_secrets().await?;
        let host = crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?
            .ok_or("Unknown worker job in this ox launch.")?;
        let reply = host
            .send(
                self.launch.clone(),
                self.caller(thread),
                thread.map(str::to_owned),
                None,
                Some(call),
            )
            .await
            .map_err(|e| crate::upstream::redact(&e, &secrets).into_owned())?;
        Ok(redact_value(reply, &secrets))
    }

    /// Read this session's handoff sources through the session host, which
    /// resolves the caller exactly as for every parent operation.
    async fn read_thread(&self, read: ReadThread, thread: Option<&str>) -> Result<Value, String> {
        if crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?.is_none() {
            return Err("read_ox_thread needs the OX session host, which ran the handoff.".into());
        }
        if read.limit.is_some_and(|l| l == 0 || l > 100)
            || [&read.host, &read.session, &read.item, &read.after]
                .into_iter()
                .flatten()
                .any(|s| s.is_empty() || s.len() > 256)
        {
            return Err("Use a limit of 1 to 100 and IDs of up to 256 bytes.".into());
        }
        let mut call = json!({"op":"read_thread","session":read.session,"item":read.item,"after":read.after,"limit":read.limit});
        if let Some(host) = read.host {
            call["host"] = host.into();
        }
        self.host_call(call, thread).await
    }

    async fn list_hosts(&self, thread: Option<&str>) -> Result<Value, String> {
        if crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?.is_none() {
            return Ok(
                json!({"hosts": [], "message": "This launch is running without an OX session host."}),
            );
        }
        self.host_call(json!({"op":"list_hosts"}), thread).await
    }

    async fn list_jobs(&self, thread: Option<&str>) -> Result<Value, String> {
        let mut local = self.jobs.lock().unwrap().list();
        if crate::host_lifetime::HostLink::find(&self.cwd, self.host_policy)?.is_some() {
            let hosted = self.host_call(json!({"op":"list"}), thread).await?;
            if let (Some(jobs), Some(more)) =
                (local["jobs"].as_array_mut(), hosted["jobs"].as_array())
            {
                jobs.extend(more.iter().cloned());
            }
        }
        Ok(local)
    }

    async fn list_models(&self, filters: ListModels) -> Result<Value, String> {
        let query = filters.query.trim().to_lowercase();
        let mut total = 0;
        let mut models = Vec::new();
        // Every listed model can run. The user's starred models come first, in
        // star order, so agents see the preferred choices on the first page.
        let favorites = crate::picker::read_favorites(&self.home).unwrap_or_default();
        let rank = |alias: &str| favorites.iter().position(|f| f == alias);
        let app = self.configured_app().await?;
        let contexts: std::collections::HashMap<String, u64> =
            app.into_iter().flat_map(App::model_contexts).collect();
        let mut catalog = app.map(App::launch_models).unwrap_or_default();
        catalog.sort_by_key(|(alias, _)| rank(alias).unwrap_or(usize::MAX));
        for (alias, provider) in catalog {
            if !alias.to_lowercase().contains(&query) && !provider.to_lowercase().contains(&query) {
                continue;
            }
            if total >= filters.offset && models.len() < PAGE_SIZE {
                models.push(json!({
                    "model": alias,
                    "provider": provider,
                    "auth": "api_key",
                    "billing": format!("API key · {provider}"),
                    "starred": rank(alias).is_some(),
                    "context_window": contexts.get(alias),
                }));
            }
            total += 1;
        }
        let next = filters.offset.saturating_add(models.len());
        let subscriptions: Vec<_> = [
            (ClientKind::Codex, "codex", "openai", "ChatGPT subscription"),
            (ClientKind::Claude, "claude", "anthropic", "Claude subscription"),
        ]
        .into_iter()
        .filter_map(|(client, runtime, provider, name)| {
            let account = crate::native::saved_subscription(client, &self.home)?;
            if !format!("{name} {runtime} {provider}").to_lowercase().contains(&query) {
                return None;
            }
            Some(json!({
                "name": name,
                "provider": provider,
                "auth": "subscription",
                "account": account,
                "runtime": runtime,
                "main_client": client == self.client,
                "worker_available": true,
                "model_picker": "Use /model in the stock subscription client to see account model choices",
            }))
        })
        .collect();
        // The account's exact subscription models, from its own picker (cached; probed
        // without a turn when missing), next to the API aliases a search matched.
        let (subscription_models, picker_error) = if crate::native::has_saved_account(
            ClientKind::Claude,
            &self.home,
        ) {
            match picker::picker(&self.home, false).await {
                    Ok(picker) => (
                        crate::claude_models::search(&picker, &query)
                            .into_iter()
                            .map(|m| {
                                json!({
                                    "model": m.exact(),
                                    "display_name": m.display,
                                    "picker_value": m.value,
                                    "description": m.description,
                                    "runtime": "claude",
                                    "auth": "subscription",
                                    "billing": "Claude subscription",
                                    "start_with": {"runtime": "claude", "subscription_model": m.exact()},
                                })
                            })
                            .collect::<Vec<_>>(),
                        None,
                    ),
                    Err(error) => (Vec::new(), Some(error)),
                }
        } else {
            (Vec::new(), None)
        };
        Ok(json!({
            "models": models,
            "subscription_models": subscription_models,
            "subscription_models_error": picker_error,
            "subscriptions": subscriptions,
            "total": total,
            "next_offset": (next < total).then_some(next),
        }))
    }

    async fn delegate(&self, input: Delegate) -> Result<Value, String> {
        let model = input.model.trim();
        let task = input.task.trim();
        if model.is_empty() || model.len() > 256 || task.is_empty() || task.len() > 96 * 1024 {
            return Err(
                "Supply a model alias of up to 256 bytes and a task of up to 96 KiB.".into(),
            );
        }
        let _slot = self
            .slots
            .try_acquire()
            .map_err(|_| "All ox workers are busy. Wait for one to finish.")?;
        let app = self.configured_app().await?.ok_or(
            "No API provider is configured. Use start_ox_worker for a subscription worker, or configure an API provider in OX Settings.",
        )?;
        let task = crate::upstream::redact(task, &app.secrets);
        let payload = json!({
            "model": model,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": task}]}],
            "stream": true,
            "store": false,
            "max_output_tokens": 2000,
        });
        let request = Request::post("/v1/responses")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload.to_string())))
            .expect("static worker request");
        let text = tokio::time::timeout(WORKER_TIMEOUT, async {
            let reply = app.inference(request).await;
            if !reply.status().is_success() {
                return Err(format!("ox model request returned HTTP {}. Check the alias with list_ox_models or run ox doctor.", reply.status().as_u16()));
            }
            worker_text(reply.into_body()).await
        })
        .await
        .map_err(|_| "The ox worker timed out.")??;
        let text = crate::upstream::redact(&text, &app.secrets);
        Ok(json!({"model": model, "answer": text}))
    }
}

fn redact_value(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(crate::upstream::redact(&text, secrets).into_owned()),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| redact_value(v, secrets))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_value(v, secrets)))
                .collect(),
        ),
        other => other,
    }
}

fn prepare_app(mut app: App, home: &Path, token: &str) -> App {
    let favorites = crate::picker::read_favorites(home).unwrap_or_default();
    if let Mode::Codeox { catalog, .. } = &mut app.mode {
        crate::catalog::prioritize_favorites(catalog, &favorites);
    }
    Arc::make_mut(&mut app.secrets).push(token.to_owned());
    app
}

fn initialize(params: &Value) -> Value {
    let version = params["protocolVersion"]
        .as_str()
        .filter(|version| PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "serverInfo": {"name": SERVER, "version": crate::VERSION},
        "capabilities": {"tools": {}},
        "instructions": "For an ordinary implementation worker, omit permission_mode to inherit the verified hosted parent mode and pass checkout for a separate workspace. Without a host, inherit is read; request write for edits. Answer requests for your own assessment directly. Suggest a second reviewer when it would resolve a concrete uncertainty. A request for a second opinion authorizes one read-only review. Use the established model and billing preference or clarify if the choice changes who pays. Consult list_ox_models, pass subscription_model for a named Claude subscription model or model for an API alias, and report unavailability before choosing another. A how-to question calls for an explanation. Hosted parent sessions receive worker outcomes and open worker requests at a turn boundary. Answer a request marked parent_may_answer with answer_ox_worker when it serves the task; leave the rest to the user. Use ox_worker_status for full results or to check a job from a terminal session. Summarize the results. Use delegate_to_ox_model for a short text-only API request. For handoffs use read_ox_thread to recover omitted source messages. Use recall_ox_memory for saved user/project preferences or provider routing rules, and save_ox_memory to store durable preferences or facts.",
    })
}

fn tools() -> Value {
    json!({
        "tools": [
            {
                "name": "list_ox_models",
                "description": "List ox API models, the signed-in Claude subscription's exact models, and detected subscription routes. models: API aliases with model, provider, auth (api_key), billing, starred (the user's favourites, listed first), context_window (tokens, null when unknown). subscription_models: exact Claude subscription models (model such as claude-fable-5-2, display_name, billing Claude subscription), read from the account's own model picker without running a turn; start one with start_ox_worker runtime claude and subscription_model, no model. subscriptions: runtime, account, main_client (this launch's client), worker_available (can run start_ox_worker without a model). query filters by alias, provider, runtime or model name, such as fable. Pages of API models hold 40 rows: when next_offset is not null, pass it as offset for more. Use a model alias for delegate_to_ox_model.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "offset": {"type": "integer", "minimum": 0}
                    },
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": true, "openWorldHint": false},
            },
            {
                "name": "delegate_to_ox_model",
                "description": "Send one independent text request to the named API model, which the provider bills. Include the context it needs. Times out after 90 seconds; task up to 96 KiB. No files or shell: for multi-step work or files, use start_ox_worker.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "model": {"type": "string", "description": "Exact ox model alias returned by list_ox_models."},
                        "task": {"type": "string", "description": "Task and only the context the chosen provider needs. Up to 96 KiB."}
                    },
                    "required": ["model", "task"],
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true},
            },
            {
                "name": "start_ox_worker",
                "description": "Start a background coding job in this project and return its job ID at once. Choose by fit: a Codex goal for one long objective with a checkable stop condition; a Claude workflow for work Claude Code splits into steps with its Workflow tool and subagents; a session for ordinary coding on either runtime. Task up to 32 KiB. There is no time or count limit by default; set max_minutes per job, or OX_WORKER_MAX_MINUTES and OX_WORKER_MAX_PER_PARENT, to add one. A write worker starting while another writes in the same project returns a warning: consider a separate git worktree. Without the OX session host they stop when this launch exits. Inherit uses your mode only with the session host; without it, inherit is read. Lower with permission_mode, never raise it. For ordinary work, omit permission_mode to inherit the hosted parent’s verified mode, including YOLO. Request read for a read-only review. Explicit YOLO requests can require operator acceptance. Pin a Claude subscription model with subscription_model. Status reports the actual_model that answered.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "runtime": {"enum": ["codex", "claude"]},
                        "mode": {"enum": ["session", "goal", "workflow"], "description": "session: background coding session on either runtime. goal: Codex only; task becomes the Codex goal objective, at most 4,000 characters. workflow: Claude Code only; the job fails if the Workflow tool is not used."},
                        "model": {"type": "string", "description": "Exact ox API model alias, billed by its provider. Leave out to use the selected runtime’s subscription."},
                        "billing": {"enum": ["api", "subscription"], "description": "Who pays. api: the provider of model. subscription: the user's ChatGPT or Claude subscription for the selected runtime. Optional: a model means api, none means subscription. A conflicting model is refused."},
                        "subscription_model": {"type": "string", "description": "Exact Claude subscription model to pin, such as claude-fable-5-2, from list_ox_models subscription_models. Claude runtime only, without model. Checked against the account's model picker before the first turn: an unoffered model is refused and nothing runs or bills. Leave out for the subscription's default model."},
                        "task": {"type": "string", "description": "The job objective, desired validation, and stopping condition."},
                        "permission_mode": {"enum": ["inherit", "read", "write", "classifier", "yolo"], "default": "inherit", "description": "read: Codex Read only or Claude Plan. Standalone Claude read workers get only read tools. write: workspace edits and shell access. Codex uses its Workspace sandbox. Claude Code pre-approves Bash and uses its own sandbox when available. Without it, commands can run unsandboxed. classifier: Codex Auto-review / Claude Auto. yolo: Codex YOLO / Claude bypass. Above your mode is refused."},
                        "max_minutes": {"type": "integer", "minimum": 1, "maximum": 10080, "description": "Optional wall-clock limit for this worker in minutes. No limit by default (OX_WORKER_MAX_MINUTES sets one)."},
                        "needs": {"type":"object","additionalProperties":false,"properties":{"memory_gb":{"type":"integer","minimum":1},"cpus":{"type":"integer","minimum":1},"gpu":{"type":"boolean"},"os":{"type":"string"}},"description":"Minimum installed hardware. Auto selects by current load and free memory among eligible hosts."},
                        "placement":{"type":"string","description":"Select a named [workers.placement] project rule, such as heavy."},
                        "host": {"type":"string","description":"Host name or ID, parent, label:LABEL, or auto. Explicit choices override project rules."},
                        "checkout": {"type":"string","description":"Existing execution directory, absolute or relative to the parent workspace. The current thread remains the worker’s parent."},
                        "workspace_write": {"type": "boolean", "description": "Deprecated: false means read, true means write. Use permission_mode."},
                    },
                    "required": ["runtime", "mode", "task"],
                    "additionalProperties": false,
                },
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true},
            },
            {
                "name":"read_ox_thread",
                "description":"Read the session this one was handed off from (and earlier sources in its chain), read-only and within this project. The handoff carried a budgeted selection of real messages; use this to read omitted or cut items in full. Pages by item ID: item returns one item, after continues after an item. Omit session for the nearest source.",
                "inputSchema":{"type":"object","properties":{
                    "host":{"type":"string","description":"Stable source host ID from the handoff note. Use with a session ID shared by multiple hosts."},
                    "session":{"type":"string","description":"A source session ID from the handoff note. Omit for the nearest source."},
                    "item":{"type":"string","description":"Return this item in full."},
                    "after":{"type":"string","description":"Start after this item ID. Use next_after from the previous page."},
                    "limit":{"type":"integer","minimum":1,"maximum":100,"default":20}
                },"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"openWorldHint":false}
            },
            {
                "name":"list_ox_hosts",
                "description":"List the parent and its paired worker hosts with live resources, labels, installed runtimes, configured billing routes, link permissions and whether each host has this repository. Unreachable hosts remain listed.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"openWorldHint":false}
            },
            {
                "name":"list_ox_workers",
                "description":"List workers started by this session or OX launch and their current status. Workers owned by the OX session host keep running after this launch ends.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":true,"openWorldHint":false}
            },
            {
                "name":"steer_ox_worker",
                "description":"Deliver a follow-up instruction to a running worker which reports can_steer. It keeps its original model, workspace, and permissions.",
                "inputSchema":{"type":"object","properties":{"job_id":{"type":"integer","minimum":1},"text":{"type":"string"}},"required":["job_id","text"],"additionalProperties":false},
                "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true}
            },
            {
                "name": "ox_worker_status",
                "description": "Get a worker's running or terminal status, native session ID, progress, and result. Jobs keep running between tool calls. Status waiting_for_operator means the worker waits on an approval or question, listed in requests. Answer those marked parent_may_answer with answer_ox_worker; for the rest tell the user where to answer, using waiting_for_operator.message.",
                "inputSchema": {"type":"object","properties":{"job_id":{"type":"integer","minimum":1}},"required":["job_id"],"additionalProperties":false},
                "annotations": {"readOnlyHint":true,"openWorldHint":false},
            },
            {
                "name": "answer_ox_worker",
                "description": "Answer an approval or question of a host-owned worker you started, in place of the user. Only requests that ox_worker_status lists with parent_may_answer true are accepted: questions, and approvals when your own mode already runs actions without a person (Auto, Auto-review or YOLO) and the worker is not read-only. Pick only an index listed in the request's parent_choices: choices that set a lasting rule stay with the user. Approve only what serves the task you gave the worker and that you would run yourself; deny anything else. Raising a worker's mode always stays with the user.",
                "inputSchema": {"type":"object","properties":{
                    "job_id":{"type":"integer","minimum":1},
                    "request_id":{"type":["integer","string"],"description":"request_id from the worker's requests."},
                    "choice":{"type":"integer","minimum":0,"description":"Index into the request's choices, for an approval."},
                    "answers":{"type":"array","items":{"type":"string"},"description":"One answer per question, in order, for a question form."}
                },"required":["job_id","request_id"],"additionalProperties":false},
                "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true},
            },
            {
                "name": "cancel_ox_worker",
                "description": "Stop a running worker in this ox launch. Work already written by the worker stays in the project.",
                "inputSchema": {"type":"object","properties":{"job_id":{"type":"integer","minimum":1}},"required":["job_id"],"additionalProperties":false},
                "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":false},
            },
            {
                "name": "recall_ox_memory",
                "description": "Search persistent OX memories (provider routing policies, user preferences, project facts). Omit query to list pinned/top memories for this workspace.",
                "inputSchema": {"type":"object","properties":{
                    "query":{"type":"string"},
                    "scope":{"enum":["all","global","project","host"]},
                    "kind":{"enum":["routing_policy","preference","fact"]},
                    "limit":{"type":"integer","minimum":1,"maximum":20,"default":6}
                },"additionalProperties":false},
                "annotations": {"readOnlyHint":true,"openWorldHint":false},
            },
            {
                "name": "save_ox_memory",
                "description": "Save, update (by id), or forget (id + forget:true) a concise atomic OX memory (max 500 chars). Never store secrets or credentials.",
                "inputSchema": {"type":"object","properties":{
                    "text":{"type":"string"},
                    "id":{"type":"string"},
                    "forget":{"type":"boolean"},
                    "scope":{"enum":["global","project","host"],"default":"project"},
                    "kind":{"enum":["routing_policy","preference","fact"],"default":"fact"},
                    "sensitivity":{"enum":["public","private","local_only"],"default":"private"},
                    "pin":{"type":"boolean","default":false},
                    "tags":{"type":"array","items":{"type":"string"},"maxItems":12}
                },"additionalProperties":false},
                "annotations": {"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false},
            },
        ]
    })
}

fn tool_result(result: Result<Value, String>) -> Value {
    match result {
        Ok(result) => json!({
            "content": [{"type": "text", "text": result.to_string()}],
            "structuredContent": result,
        }),
        Err(message) => json!({
            "isError": true,
            "content": [{"type":"text", "text": message}],
        }),
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response<RelayBody> {
    crate::json_response(
        StatusCode::OK,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

async fn worker_text(mut body: RelayBody) -> Result<String, String> {
    let mut framer = crate::sse::SseFramer::default();
    let mut text = String::new();
    let mut bytes = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| "The provider stream ended unexpectedly.")?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        bytes += data.len();
        if bytes > MAX_REPLY_BYTES {
            return Err("The provider response exceeded the worker limit.".into());
        }
        for event in framer.feed(&data)? {
            if event.data == "[DONE]" {
                continue;
            }
            let value: Value = serde_json::from_str(&event.data)
                .map_err(|_| "The provider sent an invalid stream event.")?;
            match value["type"].as_str().or(event.event.as_deref()) {
                Some("response.output_text.delta" | "response.refusal.delta") => {
                    if let Some(delta) = value["delta"].as_str() {
                        if text.len() + delta.len() > MAX_ANSWER_BYTES {
                            return Err("The provider answer exceeded the worker limit.".into());
                        }
                        text.push_str(delta);
                    }
                }
                Some("response.completed") if !text.is_empty() => return Ok(text),
                Some("response.completed") => {
                    let text = value["response"]["output"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|item| item["content"].as_array().into_iter().flatten())
                        .filter_map(|content| {
                            content["text"]
                                .as_str()
                                .or_else(|| content["refusal"].as_str())
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    return if text.len() > MAX_ANSWER_BYTES {
                        Err("The provider answer exceeded the worker limit.".into())
                    } else if text.is_empty() {
                        Err("The provider returned no text.".into())
                    } else {
                        Ok(text)
                    };
                }
                Some("response.failed" | "response.incomplete" | "error") => {
                    return Err("The provider did not complete the worker request.".into());
                }
                _ => {}
            }
        }
    }
    Err("The provider stream ended before completing the worker request.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }
    /// Stock Codex's MCP client (rmcp) logs every 4xx other than 405 from a
    /// streamable-HTTP server as a fatal worker error. The gateway issues no
    /// `Mcp-Session-Id` and offers no SSE stream, so a GET or DELETE on its
    /// endpoint must be 405 with `Allow: POST`, never 404.
    #[tokio::test]
    async fn the_gateway_answers_stock_mcp_transport_requests_rmcp_accepts() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let token = "gateway-status-fixture".to_owned();
        let service = Arc::new(Service {
            app: OnceCell::new(),
            local_auth: crate::provider::bearer(&token).unwrap(),
            token: token.clone(),
            origin: origin.clone(),
            home: PathBuf::from("."),
            cwd: PathBuf::from("."),
            session: None,
            observed: Mutex::new(None),
            unbound: std::sync::atomic::AtomicBool::new(false),
            host_policy: Default::default(),
            launch: "launch".to_owned(),
            client: ClientKind::Codex,
            slots: Semaphore::new(1),
            jobs: Mutex::new(jobs::Jobs::default()),
            tools: Tools::All,
        });
        let server = tokio::spawn(crate::serve_requests(listener, move |request| {
            let service = Arc::clone(&service);
            async move { service.handle(request).await }
        }));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("{origin}/mcp");
        let send = |method: reqwest::Method| {
            client
                .request(method, &url)
                .bearer_auth(&token)
                .header("mcp-protocol-version", "2025-06-18")
        };

        let initialize = send(reqwest::Method::POST)
            .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                          "params": {"protocolVersion": "2025-06-18"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(initialize.status(), StatusCode::OK);
        assert!(initialize.headers().get("mcp-session-id").is_none());

        let initialized = send(reqwest::Method::POST)
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .unwrap();
        assert_eq!(initialized.status(), StatusCode::ACCEPTED);

        for (method, session) in [
            (reqwest::Method::GET, None),
            (reqwest::Method::DELETE, None),
            (reqwest::Method::GET, Some("unknown-session")),
            (reqwest::Method::DELETE, Some("unknown-session")),
        ] {
            let mut request = send(method.clone()).header("accept", "text/event-stream");
            if let Some(session) = session {
                request = request.header("mcp-session-id", session);
            }
            let reply = request.send().await.unwrap();
            assert_eq!(
                reply.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {session:?}"
            );
            assert_eq!(reply.headers()["allow"], "POST");
        }
        server.abort();
    }

    #[test]
    fn a_new_claude_launch_names_its_session_before_the_first_turn() {
        // The ID OX adds is the one the worker gateway records for the launch.
        let (rewritten, id) = crate::clients::claude_session_args(args(&["-p", "fix it"])).unwrap();
        let id = id.unwrap();
        assert_eq!(
            session_hint(&rewritten, ClientKind::Claude).as_deref(),
            Some(id.as_str())
        );
        // A later resume of that session names the same ID, so it reclaims the workers.
        let resumed = crate::clients::claude_session_args(args(&["--resume", &id])).unwrap();
        assert_eq!(resumed.1, None);
        assert_eq!(
            session_hint(&resumed.0, ClientKind::Claude).as_deref(),
            Some(id.as_str())
        );
    }

    #[test]
    fn worker_permission_modes_keep_workspace_write_compatible() {
        let start = |v: Value| serde_json::from_value::<jobs::Start>(v).unwrap();
        let base = json!({"runtime":"codex","mode":"session","model":"m","task":"t"});
        let with = |extra: Value| {
            let mut v = base.clone();
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            start(v)
        };
        // Inherit by default; the host resolves it against the parent's mode.
        let inherit = with(json!({}));
        assert_eq!(inherit.host_start(&[])["permission"], Value::Null);
        assert_eq!(inherit.host_start(&[])["write"], Value::Null);
        // Old callers keep working: false is read, true is write.
        let old = with(json!({"workspace_write": true}));
        assert_eq!(old.host_start(&[])["write"], true);
        assert_eq!(old.writes(), Ok(true));
        assert_eq!(with(json!({"workspace_write": false})).writes(), Ok(false));
        assert_eq!(
            inherit.writes(),
            Ok(false),
            "standalone inherit starts read"
        );
        // A lower named level, by level or native name.
        let lower = with(json!({"permission_mode": "read-only"}));
        assert_eq!(lower.host_start(&[])["permission"], "read");
        assert!(lower.validate().is_ok());
        assert_eq!(
            with(json!({"permission_mode": "accept_edits"})).writes(),
            Ok(true)
        );
        assert!(with(json!({"permission_mode": "root"})).validate().is_err());
        // Without a host nothing can ask the operator, so YOLO and classifiers are refused.
        assert!(with(json!({"permission_mode": "yolo"})).writes().is_err());
        assert!(with(json!({"permission_mode": "auto"})).writes().is_err());
        let tools = tools();
        let names: Vec<_> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"read_ox_thread"));
        let start_tool = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "start_ox_worker")
            .unwrap();
        assert!(
            start_tool["description"]
                .as_str()
                .unwrap()
                .contains("Inherit uses your mode only with the session host")
        );
        assert!(start_tool["inputSchema"]["properties"]["workspace_write"].is_object());
    }

    #[test]
    fn subscription_models_and_billing_are_explicit_and_never_conflict() {
        let start = |v: Value| serde_json::from_value::<jobs::Start>(v).unwrap();
        let pinned = start(json!({"runtime":"claude","mode":"session","task":"review",
            "permission_mode":"read","subscription_model":"claude-fable-5-2"}));
        assert_eq!(pinned.validate(), Ok(()));
        assert_eq!(
            pinned.host_start(&[])["subscription_model"],
            "claude-fable-5-2"
        );
        assert!(pinned.host_start(&[])["model"].is_null());
        // Explicit billing that agrees is accepted; a conflicting alias is refused.
        let explicit = start(json!({"runtime":"claude","mode":"session","task":"t",
            "billing":"subscription","subscription_model":"claude-fable-5-2"}));
        assert_eq!(explicit.validate(), Ok(()));
        for refused in [
            json!({"runtime":"claude","mode":"session","task":"t","model":"alias","subscription_model":"claude-fable-5-2"}),
            json!({"runtime":"claude","mode":"session","task":"t","model":"alias","billing":"subscription"}),
            json!({"runtime":"claude","mode":"session","task":"t","billing":"api"}),
            json!({"runtime":"codex","mode":"session","task":"t","model":"m","subscription_model":"claude-fable-5-2"}),
            json!({"runtime":"claude","mode":"session","task":"t","subscription_model":"--settings"}),
        ] {
            assert!(start(refused.clone()).validate().is_err(), "{refused}");
        }
        // Old calls keep working and reach an older host unchanged.
        let old = start(json!({"runtime":"claude","mode":"session","task":"t"}));
        assert_eq!(old.validate(), Ok(()));
        assert!(old.host_start(&[]).get("subscription_model").is_none());
        let api = start(json!({"runtime":"claude","mode":"session","task":"t","model":"alias"}));
        assert_eq!(api.validate(), Ok(()));
        // The tool tells the agent how to name the exact model and who pays.
        let tools = tools();
        let schema = &tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "start_ox_worker")
            .unwrap()["inputSchema"]["properties"];
        assert!(schema["subscription_model"].is_object());
        assert_eq!(schema["billing"]["enum"], json!(["api", "subscription"]));
    }

    #[test]
    fn only_plausible_native_session_ids_are_observed() {
        assert!(valid_session("019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b"));
        assert!(!valid_session(""));
        assert!(!valid_session("../etc"));
        assert!(!valid_session("a b"));
        assert!(!valid_session(&"a".repeat(129)));
    }

    #[test]
    fn with_workers_host_never_a_terminal_launch_keeps_workers_in_process() {
        // Without the host's private endpoint, `never` skips host discovery even
        // when a host socket is configured.
        if std::env::var_os("OX_HOST_LIFETIME_SOCKET").is_none() {
            let found = crate::host_lifetime::HostLink::find(
                std::path::Path::new("."),
                crate::provider::WorkersHost::Never,
            )
            .unwrap();
            assert!(found.is_none());
        }
    }

    #[test]
    fn a_resumed_terminal_launch_names_its_native_session() {
        let id = "0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60";
        assert_eq!(
            session_hint(&args(&["--resume", id]), ClientKind::Claude).as_deref(),
            Some(id)
        );
        assert_eq!(
            session_hint(&args(&[&format!("--session-id={id}")]), ClientKind::Claude).as_deref(),
            Some(id)
        );
        assert_eq!(
            session_hint(&args(&["resume", id]), ClientKind::Codex).as_deref(),
            Some(id)
        );
        assert_eq!(
            session_hint(&args(&["--", "--resume", id]), ClientKind::Claude),
            None
        );
        assert_eq!(
            session_hint(&args(&["--resume", "../x"]), ClientKind::Claude),
            None
        );
        assert_eq!(session_hint(&args(&[]), ClientKind::Codex), None);
    }

    #[test]
    fn recall_defaults_to_private_and_requires_local_inference_opt_in() {
        use ox_memory::Sensitivity::*;

        assert_eq!(recall_ceiling(None, None), Private);
        assert_eq!(recall_ceiling(None, Some("local_only")), Private);
        assert_eq!(recall_ceiling(Some("1"), None), LocalOnly);
        assert_eq!(recall_ceiling(Some("1"), Some("public")), Public);
    }

    #[tokio::test]
    async fn memory_mcp_tools_save_recall_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.json");
        let _guard = EnvGuard::set("OX_MEMORY_PATH", store_path.to_str().unwrap());

        let service = Service {
            home: dir.path().to_path_buf(),
            cwd: dir.path().to_path_buf(),
            app: OnceCell::new(),
            token: "secret".to_owned(),
            local_auth: HeaderValue::from_static("Bearer secret"),
            origin: "http://127.0.0.1:1".to_owned(),
            session: None,
            observed: Mutex::new(None),
            unbound: std::sync::atomic::AtomicBool::new(false),
            host_policy: Default::default(),
            launch: "launch".to_owned(),
            client: ClientKind::Codex,
            slots: Semaphore::new(1),
            jobs: Mutex::new(jobs::Jobs::default()),
            tools: Tools::All,
        };

        assert!(service.worker_secrets().await.unwrap().is_empty());
        let models = service.list_models(ListModels::default()).await.unwrap();
        assert_eq!(models["models"], json!([]));
        let listed = tools();
        let names: Vec<&str> = listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"recall_ox_memory"));
        assert!(names.contains(&"save_ox_memory"));

        let saved = service
            .save_memory(
                SaveMemory {
                    text: Some("Use Claude subscription for confidential banking work.".to_owned()),
                    id: None,
                    forget: false,
                    scope: Some("global".to_owned()),
                    kind: Some("routing_policy".to_owned()),
                    sensitivity: Some("private".to_owned()),
                    pin: true,
                    tags: vec!["routing".to_owned(), "claude".to_owned()],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(saved["action"], "created");
        assert!(service.worker_secrets().await.unwrap().is_empty());
        assert_eq!(
            service.list_models(ListModels::default()).await.unwrap()["models"],
            json!([])
        );
        let id = saved["id"].as_str().unwrap().to_owned();

        let recalled = service
            .recall_memory(
                RecallMemory {
                    query: "which provider for confidential banking".to_owned(),
                    scope: None,
                    kind: None,
                    limit: Some(5),
                },
                None,
            )
            .await
            .unwrap();
        let memories = recalled["memories"].as_array().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0]["id"], id);
        assert_eq!(memories[0]["kind"], "routing_policy");

        let forgotten = service
            .save_memory(
                SaveMemory {
                    text: None,
                    id: Some(id),
                    forget: true,
                    scope: None,
                    kind: None,
                    sensitivity: None,
                    pin: false,
                    tags: Vec::new(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(forgotten["action"], "forgotten");

        let after = service
            .recall_memory(RecallMemory::default(), None)
            .await
            .unwrap();
        assert!(after["memories"].as_array().unwrap().is_empty());
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, val) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }
}
