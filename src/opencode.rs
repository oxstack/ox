//! Standalone OpenCode over OX's authenticated Messages gateway.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::net::Ipv4Addr;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

use crate::catalog::BundledModels;
use crate::provider::{CodeoxConfig, RelayError, SavedKeys, TOKEN_ENV};
use crate::{App, Catalog, MAX_REQUEST_BYTES, clients, random_token, serve, upstream_client};

const MESSAGES_SDK: &str = "@ai-sdk/anthropic";
const KEY_REFERENCE: &str = "{env:CODEOX_TOKEN}";

pub(crate) async fn run(args: &[OsString], home: &Path) -> Result<u8, RelayError> {
    let config_path = CodeoxConfig::path(home);
    if !config_path.try_exists()? {
        return Err("no ox API provider configured for OpenCode\n  fix: ox setup".into());
    }
    let config = crate::load_config(&config_path)?;
    let saved = SavedKeys::for_config(home, &config)?;
    let env = |name: &str| saved.get(name);
    let env_refs = config.env_refs();
    let token = random_token()?;
    let mut relay = App::codeox(
        config,
        BundledModels::default(),
        &token,
        upstream_client()?,
        MAX_REQUEST_BYTES,
        &env,
        None,
        Catalog::Shared,
    )
    .await?;
    // OpenCode uses the Messages wire. Shared catalog discovery avoids the
    // stock Codex and Claude harness filters.
    relay.client_kind = clients::ClientKind::Claude;
    let available: BTreeSet<_> = relay
        .launch_models()
        .into_iter()
        .map(|(alias, _)| alias)
        .collect();
    if available.is_empty() {
        return Err(
            "no configured models are available to OpenCode\n  fix: ox doctor   (check provider keys and model listings)".into(),
        );
    }
    if matches!(args, [subcommand] if subcommand == "models") {
        for alias in available {
            println!("ox/{alias}");
        }
        return Ok(0);
    }
    if let Some(alias) = clients::explicit_model(args).and_then(|model| model.strip_prefix("ox/")) {
        relay.prewarm(alias);
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let favorites = crate::picker::read_favorites(home).unwrap_or_default();
    let default_model = favorites
        .iter()
        .map(String::as_str)
        .find(|alias| available.contains(alias))
        .or_else(|| available.first().copied())
        .expect("available aliases were checked above");
    let models = models(&relay, &available);
    let config = json!({
        "enabled_providers": ["ox"],
        "disabled_providers": [],
        "model": format!("ox/{default_model}"),
        "provider": {
            "ox": {
                "id": "ox",
                "name": "OX API",
                "npm": MESSAGES_SDK,
                "env": [TOKEN_ENV],
                "whitelist": available,
                "blacklist": [],
                "options": {
                    "baseURL": format!("http://{}/v1", listener.local_addr()?),
                    "apiKey": KEY_REFERENCE,
                },
                "models": models,
            }
        },
    });
    let profile = clients::opencode_profile(home)?;
    // The stock config directory is merged after the project config. It keeps
    // the managed API route authoritative without disabling project settings.
    // Put large model catalogs on disk rather than in a single env variable.
    let launch_config = tempfile::Builder::new()
        .prefix(".ox-open-")
        .tempdir_in(profile.join("config"))?;
    serde_json::to_writer(
        File::create(launch_config.path().join("opencode.json"))?,
        &config,
    )?;
    let mut command = command(args, &profile, env_refs);
    command
        .env(TOKEN_ENV, token)
        .env("OPENCODE_CONFIG_DIR", launch_config.path())
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1");
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut child = command
        .spawn()
        .map_err(|error| crate::client_launch_error("opencode", error))?;
    let cancelled = async {
        tokio::select! {
            _ = terminate.recv() => libc::SIGTERM,
            _ = interrupt.recv() => libc::SIGINT,
        }
    };
    tokio::select! {
        biased;
        signal = cancelled => {
            if let Some(pid) = child.id() {
                // SAFETY: this is our child and we have not reaped it.
                unsafe { libc::kill(pid as libc::pid_t, signal) };
            }
            // Allow the stock client to flush its session state before
            // reclaiming a child that did not act on the signal.
            match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                Ok(status) => { status?; }
                Err(_) => child.kill().await?,
            }
            Ok(128 + signal as u8)
        }
        status = child.wait() => {
            let status = status?;
            Ok(status.code().or_else(|| status.signal().map(|signal| 128 + signal)).unwrap_or(1) as u8)
        }
        result = serve(listener, Arc::new(relay)) => Err(result.unwrap_err().into()),
    }
}

fn command(args: &[OsString], profile: &Path, env_refs: BTreeSet<String>) -> Command {
    let mut command = Command::new("opencode");
    crate::exempt_loopback_proxy(&mut command);
    for name in env_refs {
        command.env_remove(name);
    }
    command
        .args(args)
        .kill_on_drop(true)
        .env("XDG_CACHE_HOME", profile.join("cache"))
        .env("XDG_CONFIG_HOME", profile.join("config"))
        .env("XDG_DATA_HOME", profile.join("data"))
        .env("XDG_STATE_HOME", profile.join("state"))
        .env_remove("OPENCODE_DB")
        .env_remove("OPENCODE_CONFIG")
        .env_remove("OPENCODE_CONFIG_CONTENT")
        .env_remove(crate::daemon::KEY_ENV);
    crate::host_lifetime::isolate_stock(&mut command);
    command
}

fn models(relay: &App, available: &BTreeSet<&str>) -> Map<String, Value> {
    relay
        .visible_models()
        .filter_map(|entry| {
            let alias = entry["slug"].as_str()?;
            if !available.contains(alias) {
                return None;
            }
            let context = entry["context_window"].as_u64().unwrap_or(64_000);
            let output = entry["max_output_tokens"]
                .as_u64()
                .unwrap_or(crate::adapters::anthropic::DEFAULT_MAX_TOKENS.min(context));
            let image = entry["input_modalities"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value == "image"));
            Some((
                alias.to_owned(),
                json!({
                    "id": alias,
                    "name": format!(
                        "{} · {}",
                        entry["display_name"].as_str()?,
                        entry["description"].as_str()?,
                    ),
                    "provider": {"npm": MESSAGES_SDK},
                    "headers": {"x-api-key": KEY_REFERENCE},
                    // OX's catalog excludes discovered models that declare
                    // no tool support. Static models inherit OX's tool policy.
                    "tool_call": true,
                    "attachment": image,
                    "modalities": {
                        "input": if image { vec!["text", "image"] } else { vec!["text"] },
                        "output": ["text"],
                    },
                    "limit": {"context": context, "output": output},
                }),
            ))
        })
        .collect()
}
