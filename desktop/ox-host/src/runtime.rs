use ox_desk_core::{
    claude::catalog::Profiles,
    host_protocol::Scope,
    links::{LinkEvent, codex},
    ports::ox,
    runtime::{Billing, Route, Runtime},
    settings_protocol::{ClaudeProfiles, Request},
    store::{
        Profile, Store,
        claude::{API_PROVIDER, Launcher, SUBSCRIPTION_PROVIDER},
    },
};
use ox_host_service::{RouteInputs, route_inputs};
use std::{
    env,
    ffi::OsString,
    io,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct Prepared {
    alias: Vec<String>,
    provider: Option<String>,
    pub routes: Vec<Route>,
    pub yolo_grant_projects: Vec<PathBuf>,
    inputs: Option<RouteInputs>,
    when: std::time::Instant,
}
impl Prepared {
    pub(crate) fn has_api_provider(&self) -> bool {
        self.provider.is_some()
    }
    fn current(&self) -> bool {
        self.when.elapsed() < std::time::Duration::from_secs(5)
            && self.inputs.is_some()
            && self.inputs == route_inputs()
    }
}
#[derive(Default)]
struct CatalogCache {
    key: Option<(PathBuf, Option<RouteInputs>)>,
    at: Option<std::time::Instant>,
    models: Vec<String>,
    loading: bool,
}
static CATALOG: std::sync::Mutex<CatalogCache> = std::sync::Mutex::new(CatalogCache {
    key: None,
    at: None,
    models: Vec::new(),
    loading: false,
});
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    pub ox: PathBuf,
    pub path: OsString,
    pub(crate) desktop: PathBuf,
}
impl RuntimeConfig {
    pub(crate) fn discover() -> io::Result<Self> {
        let mut paths: Vec<_> = env::var_os("PATH")
            .as_deref()
            .map(env::split_paths)
            .into_iter()
            .flatten()
            .filter(|p| p.is_absolute())
            .collect();
        let home = env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = &home {
            paths.extend(
                [
                    ".local/bin",
                    ".cargo/bin",
                    ".local/share/mise/shims",
                    ".local/share/mise/command-wrappers/bin",
                    ".npm-global/bin",
                    ".volta/bin",
                ]
                .map(|p| home.join(p)),
            );
        }
        paths
            .extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
        let sibling = env::current_exe()?.with_file_name("ox");
        let ox = std::iter::once(sibling)
            .chain(paths.iter().map(|p| p.join("ox")))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Install the matching OX helper next to ox-host",
                )
            })?;
        let base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                home.map(|p| {
                    p.join(if cfg!(target_os = "macos") {
                        "Library/Application Support"
                    } else {
                        ".local/state"
                    })
                })
            })
            .ok_or_else(|| io::Error::other("No OX state directory"))?;
        Ok(Self {
            ox,
            path: env::join_paths(paths).map_err(io::Error::other)?,
            desktop: base.join("ox/desktop"),
        })
    }
    pub(crate) fn projects(&self) -> Vec<PathBuf> {
        let Ok(f) = std::fs::File::open(self.desktop.join("workspace.json")) else {
            return vec![];
        };
        let Ok(v) = serde_json::from_reader::<_, serde_json::Value>(f) else {
            return vec![];
        };
        std::iter::once(&v)
            .chain(v["recent"].as_array().into_iter().flatten())
            .filter_map(|r| r["project"].as_str())
            .map(PathBuf::from)
            .filter_map(|p| p.canonicalize().ok())
            .filter(|p| p.is_dir())
            .collect()
    }
    pub(crate) fn installed_runtimes(&self) -> Vec<Runtime> {
        Runtime::ALL
            .into_iter()
            .filter(|r| {
                env::split_paths(&self.path).any(|p| {
                    p.join(r.executable())
                        .metadata()
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
            })
            .collect()
    }
    pub(crate) fn prepare(&self) -> io::Result<Prepared> {
        let before = route_inputs();
        let s = ox::snapshot(&self.ox, &self.path).map_err(io::Error::other)?;
        let after = route_inputs();
        // Stars only order: the snapshot's static and starred aliases first, then every other
        // catalog alias. Discovery asks the providers, so it runs off the caller's thread and
        // the list fills in once it lands.
        let models = self.catalog(&s.models, after);
        let routes = Runtime::routes(&models)
            .filter(|r| {
                r.is_api()
                    || match r.runtime {
                        Runtime::Codex => s.chatgpt_account.is_some(),
                        Runtime::Claude => s.claude_account,
                        Runtime::OpenCode => false,
                        Runtime::Antigravity => {
                            ox_desk_core::links::antigravity::executable(&self.path).is_some()
                        }
                    }
            })
            .collect();
        Ok(Prepared {
            alias: models,
            provider: s.api_provider,
            routes,
            yolo_grant_projects: s.yolo_grant_projects,
            inputs: if before == after { after } else { None },
            when: std::time::Instant::now(),
        })
    }
    /// `base` followed by the cached discovered catalog; stale or missing entries start a
    /// background refresh (one at a time) and never block the caller.
    fn catalog(&self, base: &[String], inputs: Option<RouteInputs>) -> Vec<String> {
        const FRESH: std::time::Duration = std::time::Duration::from_secs(120);
        let mut cache = CATALOG.lock().unwrap_or_else(|e| e.into_inner());
        let key = (self.ox.clone(), inputs);
        let usable = cache.key.as_ref() == Some(&key);
        let fresh = usable && cache.at.is_some_and(|at| at.elapsed() < FRESH);
        if !fresh && !cache.loading && inputs.is_some() {
            cache.loading = true;
            let (ox, path) = (self.ox.clone(), self.path.clone());
            let key = key.clone();
            std::thread::spawn(move || {
                let found: Vec<String> =
                    ox::request(&ox, &path, Request::Models).unwrap_or_default();
                let mut cache = CATALOG.lock().unwrap_or_else(|e| e.into_inner());
                *cache = CatalogCache {
                    key: Some(key),
                    at: Some(std::time::Instant::now()),
                    models: found,
                    loading: false,
                };
            });
        }
        ox_desk_core::ports::ox::merge_catalog(
            base,
            if usable { cache.models.clone() } else { vec![] },
        )
    }
    pub(crate) fn open(
        &self,
        scope: &Scope,
        prepared: Option<Prepared>,
        receive: impl Fn(LinkEvent) + Send + Sync + 'static,
    ) -> io::Result<Store> {
        if scope.route.is_api() && !scope.route.runtime.supports_api() {
            return Err(io::Error::other(
                "Antigravity uses its native Google account.",
            ));
        }
        let settings = if scope.route.is_api() || scope.route.runtime == Runtime::Claude {
            Some(match prepared.filter(|s| s.current()) {
                Some(s) => s,
                None => self.prepare()?,
            })
        } else {
            None
        };
        // Any catalog model may open, starred or not: stars only order the lists.
        // `ox --model` validates the alias against the configured providers at
        // launch and names the closest match, so only a missing provider stops here.
        if scope.route.is_api() && settings.as_ref().is_none_or(|s| s.provider.is_none()) {
            return Err(io::Error::other(
                "Configure an OX API provider in Settings first.",
            ));
        }
        let receive = Arc::new(receive);
        let deliver = receive.clone();
        let project = scope.project.clone();
        let route = scope.route.clone();
        let mut store = match route.runtime {
            Runtime::Codex => {
                let provider = match &route.billing {
                    Billing::Subscription => "openai".to_owned(),
                    Billing::Api(_) => settings.and_then(|s| s.provider).ok_or_else(|| {
                        io::Error::other("Configure an OX API provider in Settings first.")
                    })?,
                };
                let mut args = ox_desk_core::codex::launch_args(&route, &provider);
                args.extend(
                    ["--no-daemon", "app-server", "--listen", "stdio://"].map(String::from),
                );
                let sub = route == Route::CODEX_SUBSCRIPTION;
                let saw = AtomicBool::new(false);
                let config = self.clone();
                let mut command = std::process::Command::new(&self.ox);
                command
                    .args(args)
                    .current_dir(&project)
                    .env("PATH", &self.path)
                    .env("OX_HOST_WORKER_ROUTE", serde_json::to_string(&route)?)
                    .env(
                        "OX_MEMORY_MAX_SENSITIVITY",
                        ox_desk_core::memory::route_sensitivity_ceiling(&route).as_str(),
                    )
                    .env_remove("OX_HOST_WORKER_PARENT");
                let link = codex::Link::from_command(command, move |e| {
                    match &e {
                        LinkEvent::Frame(_) => saw.store(true, Ordering::Release),
                        LinkEvent::Closed { .. }
                            if sub
                                && !saw.load(Ordering::Acquire)
                                && ox::snapshot(&config.ox, &config.path)
                                    .is_ok_and(|s| s.chatgpt_account.is_none()) =>
                        {
                            deliver(LinkEvent::SignInRequired)
                        }
                        _ => (),
                    }
                    deliver(e);
                })?;
                Store::open(
                    Profile {
                        project,
                        route,
                        provider,
                    },
                    link,
                )
            }
            Runtime::Claude => {
                let paths: ClaudeProfiles =
                    ox::request(&self.ox, &self.path, Request::ClaudeProfiles)
                        .map_err(io::Error::other)?;
                let aliases = settings.expect("Claude preflight").alias;
                let provider = if route.is_api() {
                    API_PROVIDER
                } else {
                    SUBSCRIPTION_PROVIDER
                }
                .into();
                Store::open_claude(
                    Profile {
                        project,
                        route,
                        provider,
                    },
                    Launcher {
                        ox: self.ox.clone(),
                        path: self.path.clone(),
                        profiles: Profiles {
                            subscription: paths.subscription,
                            api: paths.api,
                        },
                        index: self.desktop.join("claude-routes.json"),
                        aliases,
                    },
                    move |e| deliver(e),
                )
            }
            Runtime::OpenCode => {
                return Err(io::Error::other(
                    "Hosted OpenCode requires an explicitly enrolled local route",
                ));
            }
            Runtime::Antigravity => {
                use ox_desk_core::{links::antigravity, store::antigravity::PROVIDER};
                if antigravity::executable(&self.path).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "Install the official Antigravity CLI on this host, then check the connection.",
                    ));
                }
                Store::open_antigravity(
                    Profile {
                        project,
                        route,
                        provider: PROVIDER.into(),
                    },
                    self.path.clone(),
                    self.desktop.join("antigravity-sessions.json"),
                    move |e| deliver(e),
                )
            }
        }?;
        if scope.route.runtime == Runtime::Antigravity {
            return Ok(store);
        }
        store.enable_naming(
            ox_desk_core::titles::Launcher {
                ox: self.ox.clone(),
                path: self.path.clone(),
                directory: self.desktop.join("titles"),
            },
            move |e| receive(e),
        );
        Ok(store)
    }
}
pub(crate) fn canonical_scope(scope: Scope) -> io::Result<Scope> {
    if !scope.project.is_absolute() {
        return Err(io::Error::other("Open an absolute project directory"));
    }
    let project = scope.project.canonicalize()?;
    if !project.is_dir() {
        return Err(io::Error::other("Select a project directory"));
    }
    Ok(Scope { project, ..scope })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_desk_core::settings_protocol::{VERSION, model::Snapshot};

    const STOCK: &str = "stock/model";
    const UNSTARRED: &str = "fixture/unstarred-only";

    fn script(path: &std::path::Path, body: &str) {
        crate::executable::write(path, &format!("#!/usr/bin/env python3\n{body}"));
    }

    /// A fixture provider: settings list only the stock alias, discovery also lists an
    /// unstarred alias that no stock id shares. A fake Codex app-server holds saved history on it.
    fn fixture(dir: &std::path::Path) -> RuntimeConfig {
        let snapshot = Snapshot {
            version: VERSION,
            models: vec![STOCK.into()],
            api_provider: Some("codeox".into()),
            claude_account: true,
            chatgpt_account: Some("me".into()),
            ..Default::default()
        };
        let home = dir.join("api-home");
        std::fs::create_dir_all(&home).unwrap();
        script(
            &dir.join("ox"),
            &format!(
                r#"import json, sys
req = json.load(sys.stdin)
a = req["action"]
if a == "read_rows": print({snap:?})
elif a == "models": print(json.dumps(["{STOCK}", "{UNSTARRED}"]))
elif a == "codex_profiles": print(json.dumps({{"subscription": None, "api": "{home}"}}))
else: sys.exit("unsupported")
"#,
                snap = serde_json::to_string(&snapshot).unwrap(),
                home = home.display(),
            ),
        );
        script(
            &dir.join("codex"),
            &format!(
                r#"import json, sys
def reply(r, result): print(json.dumps({{"id": r["id"], "result": result}}), flush=True)
for line in sys.stdin:
    r = json.loads(line)
    m = r.get("method")
    if m == "initialize": reply(r, {{"userAgent": "codex/0.171.0"}})
    elif m == "thread/list":
        reply(r, {{"data": [{{"id": "saved", "cwd": "/work/project", "modelProvider": "codeox", "model": "{UNSTARRED}"}}], "nextCursor": None}})
"#
            ),
        );
        RuntimeConfig {
            ox: dir.join("ox"),
            path: std::env::join_paths([dir, std::path::Path::new("/usr/bin"), "/bin".as_ref()])
                .unwrap(),
            desktop: dir.join("state"),
        }
    }

    #[test]
    fn an_unstarred_catalog_alias_is_a_route_everywhere_and_reopens_saved_history() {
        let dir = tempfile::tempdir().unwrap();
        let config = fixture(dir.path());
        let route = |runtime: Runtime| runtime.route(Some(UNSTARRED.into()));
        // Start, worker and hand-off targets come from the host's routes. The catalog lands
        // from the background discovery; the next prepare lists it without any star.
        let mut routes = vec![];
        for _ in 0..100 {
            let prepared = config.prepare().unwrap();
            routes = prepared.routes;
            if routes.contains(&route(Runtime::Codex)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        for runtime in [Runtime::Codex, Runtime::Claude] {
            assert!(
                routes.contains(&route(runtime)),
                "{runtime:?} lacks the alias"
            );
        }
        // The starred order stays first: the stock alias precedes the discovered one.
        let at = |r: Route| routes.iter().position(|x| *x == r).unwrap();
        assert!(at(route(Runtime::Codex).clone()) > at(Runtime::Codex.route(Some(STOCK.into()))));

        // "Restart": a fresh discovery of saved history, nothing starred.
        let mut found = vec![];
        let errors = ox_desk_core::native_catalog::discover(
            &config.ox,
            &config.path,
            &dir.path().join("claude-routes.json"),
            &dir.path().join("titles"),
            |scope, views| {
                found.push((scope, views));
                true
            },
        );
        let (scope, views) = found
            .iter()
            .find(|(s, _)| s.route.runtime == Runtime::Codex)
            .unwrap_or_else(|| panic!("no Codex history: {errors:?}"));
        assert_eq!(scope.route, route(Runtime::Codex));
        assert_eq!(views[0].capabilities, ["open"]);
        // Claude's saved row validates against the same full catalog.
        let row: ox_desk_core::store::ThreadRow = serde_json::from_value(serde_json::json!({
            "id": "c", "cwd": "/work/project", "modelProvider": ox_desk_core::store::claude::API_PROVIDER, "model": UNSTARRED
        }))
        .unwrap();
        let full = ox_desk_core::ports::ox::catalog(&config.ox, &config.path, &[STOCK.into()]);
        assert_eq!(row.saved_runtime_route(&[STOCK.into()]), None);
        assert_eq!(row.saved_runtime_route(&full), Some(route(Runtime::Claude)));
    }

    /// Discovery-only provider: no starred or static models, no accounts, empty cache.
    fn discovery_only(dir: &std::path::Path) -> RuntimeConfig {
        let snapshot = Snapshot {
            version: VERSION,
            models: vec![],
            api_provider: Some("codeox".into()),
            claude_account: false,
            chatgpt_account: None,
            ..Default::default()
        };
        script(
            &dir.join("ox"),
            &format!(
                r#"import json, sys, time
req = json.load(sys.stdin)
a = req["action"]
if a == "read_rows": print({snap:?})
elif a == "models":
    time.sleep(0.3)
    print(json.dumps(["{UNSTARRED}"]))
else: sys.exit("unsupported")
"#,
                snap = serde_json::to_string(&snapshot).unwrap(),
            ),
        );
        RuntimeConfig {
            ox: dir.join("ox"),
            path: std::env::join_paths([dir, std::path::Path::new("/usr/bin"), "/bin".as_ref()])
                .unwrap(),
            desktop: dir.join("state"),
        }
    }

    #[test]
    fn cold_discovery_only_host_lists_no_routes_first_then_both_runtimes_without_stars() {
        let dir = tempfile::tempdir().unwrap();
        let config = discovery_only(dir.path());
        // The first Hello can only be empty: nothing starred, no subscription, empty cache.
        let first = config.prepare().unwrap();
        assert!(first.routes.is_empty(), "{:?}", first.routes);
        // Discovery lands in the background; the same host then lists the alias for both
        // runtimes with no pre-existing route.
        let mut routes = vec![];
        for _ in 0..100 {
            routes = config.prepare().unwrap().routes;
            if !routes.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let want = [Runtime::Codex, Runtime::Claude].map(|r| r.route(Some(UNSTARRED.into())));
        assert_eq!(routes, want);
        // Opening needs only the provider, not a listed route: a codex open of a route that
        // was never in any Hello passes the preflight.
        let scope = Scope {
            project: dir.path().to_path_buf(),
            route: want[0].clone(),
        };
        let store = config.open(&scope, None, |_| ());
        assert!(store.is_ok(), "{:?}", store.err().map(|e| e.to_string()));
    }
}
