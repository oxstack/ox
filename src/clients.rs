//! Client selection and the stock Claude Code launch command.
//!
//! `ox` is one binary with two launch modes. Bare `ox` and every unprefixed
//! argument pair keep the historical Codex relay launch, `ox codex` selects the
//! same launch explicitly, and `ox claude` launches stock `claude` against the
//! same loopback relay's Anthropic Messages endpoint.
//!
//! # Proven Claude Code behaviour
//!
//! Verified against `claude` 2.1.330 with a disposable `HOME`, a disposable
//! `CLAUDE_CONFIG_DIR`, only disposable user/project settings, and
//! loopback-only mock servers (no public traffic, no live API, no real auth or
//! credentials):
//!
//! * `ANTHROPIC_BASE_URL` is an origin; Claude Code appends `/v1/messages`
//!   itself (the probe saw `POST /v1/messages?beta=true`). Supplying `/v1` in
//!   the variable would produce `/v1/v1/messages`, so [`claude_command`] strips
//!   a trailing `/v1`.
//! * `ANTHROPIC_AUTH_TOKEN` is sent as `Authorization: Bearer <token>`. This
//!   matches the relay's existing bearer check and is therefore the coordinated
//!   credential, not `x-api-key`.
//! * Configuring `ANTHROPIC_API_KEY` as well made Claude Code send both
//!   `Authorization` and `x-api-key`; the personal key would leak into the
//!   relay child. [`claude_command`] removes it.
//! * `--model` values are forwarded verbatim, including multi-segment
//!   provider-qualified IDs such as `openrouter/anthropic/claude-sonnet-4`.
//!   The same is true for `ANTHROPIC_MODEL` and
//!   `ANTHROPIC_CUSTOM_MODEL_OPTION`.
//! * An explicit `--model` argument outranks `ANTHROPIC_MODEL`.
//! * Built-in Claude aliases (`default`, `best`, `opus`, `sonnet`, `haiku`,
//!   `fable`, `opusplan`, `sonnet[1m]`, `opus[1m]`) are resolved client-side:
//!   `--model sonnet` arrived at the loopback endpoint as
//!   `claude-sonnet-5-5`. This layer therefore skips reserved names when it
//!   chooses the default and picker values; normal `<provider>/<model>` aliases
//!   are forwarded unchanged, and an explicit `--model` is still passed through
//!   exactly as the user wrote it.
//! * `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` remaps the stock picker
//!   aliases to arbitrary, including provider-qualified, model IDs.
//!
//! ## Settings files outrank the process environment
//!
//! The settings document is a second, higher-priority source of `env` values.
//! With a user settings file containing
//! `{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:<other>",
//! "ANTHROPIC_AUTH_TOKEN":"...","ANTHROPIC_MODEL":"..."}}` and a different
//! `ANTHROPIC_BASE_URL`/token/model in the process environment, stock Claude
//! Code sent `POST /v1/messages?beta=true` to the settings origin with the
//! settings bearer and model. Scrubbing the child process environment alone
//! therefore cannot keep a launch on the relay.
//!
//! A single `--settings <file>` document outranks user, project, and local
//! settings `env` values while the other settings sources stay loaded:
//!
//! * The probe's user settings `env` selected a second origin; adding
//!   `--settings` with the relay origin moved the request to the relay origin
//!   and relay bearer, while the user document's unrelated `env` keys and
//!   `model` setting still applied.
//! * Project settings `env` stayed loaded and effective too; only the keys the
//!   overlay sets are replaced.
//! * Empty strings in the overlay disable settings-provided switches and
//!   credentials: `ANTHROPIC_API_KEY:""` removed the `x-api-key` header, and
//!   `CLAUDE_CODE_USE_BEDROCK:""` moved a Bedrock-configured settings document
//!   back to the standard Messages endpoint. `ANTHROPIC_UNIX_SOCKET:""` and
//!   `ANTHROPIC_CUSTOM_HEADERS:""` likewise had no effect.
//! * A settings `apiKeyHelper` ran and added `x-api-key`; setting
//!   `"apiKeyHelper":""` in the overlay neutralized it.
//! * When `--settings` is repeated, stock Claude Code honours only the last
//!   value; the earlier document supplied neither `env` nor other settings.
//!
//! Managed settings remain authoritative: an administrator document is not
//! overridable by `--settings`, and a host may also pass `--managed-settings`.
//! [`claude_command`] therefore refuses to launch when it detects managed
//! routing/credential `env` values or a host-managed settings flag, instead of
//! silently sending the session somewhere the relay cannot see. A managed
//! `ANTHROPIC_MODEL` or picker remap is left authoritative: the session still
//! reaches the relay, which then reports a model it cannot serve. Managed
//! permissions and hooks still apply; this layer never removes a settings
//! source or changes permission flags.
//!
//! ## What this layer injects
//!
//! [`claude_command`] writes one launch-scoped settings document to a new
//! `0600` file with a random name in the temporary directory and passes it with
//! `--settings`. [`ClaudeLaunchSettings`] owns that file and removes it when
//! the launch ends, so the relay bearer is never in argv, never in a persistent
//! settings file, and never in the user's Claude configuration. A caller must
//! keep the guard alive until the child has exited. Normal exit removes the
//! file; an abrupt `SIGKILL` (or a supervisor that kills `ox` without unwinding)
//! can leave one behind, so it is written with the `0600` mode and a random
//! name and holds only the launch bearer, which is dead once that relay exits.
//!
//! The document preserves the effective user `--settings` document (the last
//! occurrence, matching stock behaviour), including its permissions, hooks, and
//! unrelated `env` keys, then overlays:
//!
//! * `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`: relay origin and bearer.
//! * `ANTHROPIC_MODEL` plus the custom picker entry: the first configured alias
//!   that is not a Claude built-in; an explicit `--model` still wins.
//! * `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`: configured relay aliases so
//!   the stock picker rows cannot resolve to raw Anthropic model IDs.
//! * `apiKeyHelper:""` and empty values for other backend switches, backend
//!   base URLs, alternate transports, and personal/vendor credentials that
//!   settings could otherwise supply.
//!
//! Nothing here writes login state, passes vendor keys, or changes
//! permission/approval flags: argv is forwarded unchanged apart from the
//! `--settings` overlay, and stock Claude Code keeps its normal approval
//! controls.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tokio::process::Command;

use crate::provider::{DEFAULT_SEARCH_ENV_KEY, RelayError, TOKEN_ENV};

/// Longest model alias accepted from `codeox.toml`, matching `provider`.
const MAX_MODEL_VALUE_BYTES: usize = 256;
/// Longest relay bearer accepted into the child environment.
const MAX_TOKEN_BYTES: usize = 512;
/// Largest `--settings` or managed-settings document read at launch.
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

/// A parsed settings object (or an `env` object inside one).
type SettingsMap = Map<String, Value>;

/// Environment variables that would move Claude Code off the loopback relay,
/// carry a personal/vendor credential into the child, or replace the relay's
/// model selection. They are removed from the child process environment after
/// the configured vendor references and before this layer sets its own values.
///
/// The same list drives the settings overlay: every entry not explicitly set by
/// [`claude_command`] is written as an empty string, because process
/// environment removal does not cover values supplied by a settings file.
const SCRUBBED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_CUSTOM_MODEL_OPTION",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_SESSIONS_OAUTH_TOKEN",
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_GATEWAY",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_GOOGLE_CLOUD_BASE_URL",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_AWS_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_GATEWAY_TOKEN",
];

/// The subset of [`SCRUBBED_ENV`] that selects a backend, transport, or
/// credential. Managed settings control these above the overlay, so the launch
/// cannot be routed and must fail. Model-selection variables are deliberately
/// absent: a managed model still reaches the relay, which then reports an
/// unserved model instead of the launcher refusing a valid alias mapping.
pub(crate) const ROUTING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_SESSIONS_OAUTH_TOKEN",
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_GATEWAY",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_GOOGLE_CLOUD_BASE_URL",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_AWS_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_GATEWAY_TOKEN",
];

/// Which stock client one `ox` invocation launches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientKind {
    Codex,
    Claude,
}

/// Splits the first exact `codex`/`claude` argument off `args`.
///
/// Unprefixed arguments, including the historical `ox --model ...` form, keep
/// launching Codex. Only the first argument is inspected, so
/// `ox codex claude` runs Codex with `claude` as its own argument.
pub(crate) fn select(mut args: Vec<OsString>) -> (ClientKind, Vec<OsString>) {
    match args.first().and_then(|arg| arg.to_str()) {
        Some("codex") => {
            args.remove(0);
            (ClientKind::Codex, args)
        }
        Some("claude") => {
            args.remove(0);
            (ClientKind::Claude, args)
        }
        _ => (ClientKind::Codex, args),
    }
}

/// Owns the launch-scoped `--settings` file and removes it on drop.
///
/// The caller must keep this value alive until the child exits: the stock
/// client reads the file after spawn, and the file carries the launch bearer.
/// Dropping it early removes the file and can fail the child's startup.
#[derive(Debug)]
pub(crate) struct ClaudeLaunchSettings {
    path: PathBuf,
    /// The session ID OX chose for a new interactive or `-p` session.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) session: Option<String>,
}

impl ClaudeLaunchSettings {
    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ClaudeLaunchSettings {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
pub(crate) struct ClaudeLaunchOptions<'a> {
    pub starred: &'a [&'a str],
    pub config_dir: Option<&'a Path>,
    /// None keeps the normal route-based default. Some(None) selects the
    /// client's default model for a separate classifier request.
    pub client_classifier: Option<Option<&'a str>>,
}

pub(crate) fn claude_config_override() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn native_claude_profile() -> Option<PathBuf> {
    claude_config_override()
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

fn relay_profile_root(home: &Path) -> Result<PathBuf, RelayError> {
    let home = match home.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::path::absolute(home)?,
        Err(error) => return Err(error.into()),
    };
    Ok(home.join("ox-clients"))
}

fn create_profile_dir(directory: &Path) -> Result<(), RelayError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(directory)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(error.into());
    }
    if !std::fs::symlink_metadata(directory)?.is_dir() {
        return Err("the ox client profile must be a directory and cannot be a symlink".into());
    }
    Ok(())
}

/// OpenCode uses four XDG roots. Its cache cleanup must have a separate tree
/// from the persistent settings and session database.
pub(crate) fn opencode_profile(home: &Path) -> Result<PathBuf, RelayError> {
    let root = relay_profile_root(home)?;
    let profile = root.join("opencode");
    create_profile_dir(&root)?;
    create_profile_dir(&profile)?;
    prepare_opencode_profile(&profile)?;
    Ok(profile)
}

pub(crate) fn prepare_opencode_profile(profile: &Path) -> Result<(), RelayError> {
    for name in ["cache", "config", "data", "state"] {
        let root = profile.join(name);
        create_profile_dir(&root)?;
        create_profile_dir(&root.join("opencode"))?;
    }
    Ok(())
}

pub(crate) fn codex_profiles(
    home: &Path,
) -> Result<crate::settings_protocol::CodexProfiles, RelayError> {
    let native = std::path::absolute(home)?;
    let relay = relay_profile_root(home)?.join("codex");
    // The native client creates a rollout directory for saved conversations.
    // An empty installation does not need a catalog-only app-server.
    let existing = |p: PathBuf| p.join("sessions").is_dir().then_some(p);
    Ok(crate::settings_protocol::CodexProfiles {
        subscription: existing(native),
        api: existing(relay),
    })
}

pub(crate) fn claude_profiles(
    home: &Path,
) -> Result<crate::settings_protocol::ClaudeProfiles, RelayError> {
    let subscription =
        std::path::absolute(native_claude_profile().ok_or("set CLAUDE_CONFIG_DIR or HOME")?)?;
    let api = relay_profile_root(home)?.join("claude");
    require_separate_claude_profiles(&subscription, &api)?;
    Ok(crate::settings_protocol::ClaudeProfiles { subscription, api })
}

fn require_separate_claude_profiles(subscription: &Path, api: &Path) -> Result<(), RelayError> {
    if std::path::absolute(subscription)? == api
        || matches!((subscription.canonicalize(), api.canonicalize()), (Ok(a), Ok(b)) if a == b)
    {
        return Err("select separate native and OX API Claude configuration directories".into());
    }
    Ok(())
}

/// Stock clients can save `/model` selections back into their user files.
/// Give each relay client a persistent home so model changes and resumes stay
/// in that profile even if ox or its child is interrupted.
pub(crate) fn relay_client_home(
    codex_home: &Path,
    client: ClientKind,
) -> Result<PathBuf, RelayError> {
    let (name, source, files): (_, _, &[_]) = match client {
        ClientKind::Codex => (
            "codex",
            Some(codex_home.to_path_buf()),
            &[
                "config.toml",
                "AGENTS.md",
                "AGENTS.override.md",
                "requirements.toml",
            ],
        ),
        ClientKind::Claude => (
            "claude",
            native_claude_profile(),
            &["settings.json", "CLAUDE.md"],
        ),
    };
    let root = relay_profile_root(codex_home)?;
    let profile = root.join(name);
    if client == ClientKind::Claude
        && let Some(native) = source.as_deref()
    {
        require_separate_claude_profiles(native, &profile)?;
    }
    for directory in [&root, &profile] {
        create_profile_dir(directory)?;
    }
    let _lock = crate::setup::lock_config(&profile)?;
    if files.iter().any(|file| profile.join(file).is_symlink()) {
        return Err(
            "ox client settings cannot link back to files outside the relay profile".into(),
        );
    }
    let marker = profile.join(".initialized");
    if !marker.try_exists()? {
        if let Some(source) = source {
            for file in files {
                let from = source.join(file);
                let to = profile.join(file);
                if from.try_exists()? && !to.try_exists()? {
                    let bytes = read_limited(&from, "client profile setting")?;
                    let text = std::str::from_utf8(&bytes)
                        .map_err(|_| "could not copy a non-UTF-8 client setting")?;
                    crate::setup::write_config_atomic(&to, text)?;
                }
            }
        }
        crate::setup::write_config_atomic(&marker, "")?;
    }
    Ok(profile)
}

pub(crate) struct ClaudeModelInfo {
    pub can_request_server_review: bool,
    pub context_window: Option<u64>,
}

pub(crate) fn take_client_classifier(
    args: &mut Vec<OsString>,
) -> Result<Option<String>, RelayError> {
    let mut classifier = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            break;
        }
        let Some(arg) = arg.to_str() else {
            index += 1;
            continue;
        };
        let (value, count) = if arg == "--client-classifier" {
            (args.get(index + 1).and_then(|value| value.to_str()), 2)
        } else if let Some(value) = arg.strip_prefix("--client-classifier=") {
            (Some(value), 1)
        } else {
            index += 1;
            continue;
        };
        if classifier.is_some() {
            return Err("--client-classifier can only be specified once".into());
        }
        classifier = Some(
            value
                .filter(|model| valid_model_value(model) && !is_reserved_model_alias(model))
                .ok_or("--client-classifier requires auto or an Anthropic model alias")?
                .to_owned(),
        );
        args.drain(index..index + count);
    }
    Ok(classifier)
}

/// Builds the stock `claude` command for one relay launch.
///
/// `origin` is the relay listener origin (`http://127.0.0.1:<port>`), `token`
/// the fresh bearer minted for this launch only, `aliases` the model aliases the
/// relay serves, and `env_refs` every configured vendor/search key variable.
///
/// Returns the command together with the settings-file guard; keep the guard
/// alive until the child exits. Model selection is expressed through the
/// launch-scoped settings plus environment so an explicit `--model` remains
/// authoritative. Metadata for that model can supply Claude's starting context
/// window.
pub(crate) fn claude_command(
    args: Vec<OsString>,
    origin: &str,
    token: &str,
    aliases: &[String],
    env_refs: &BTreeSet<String>,
    options: ClaudeLaunchOptions<'_>,
    model_info_for: impl FnOnce(&str) -> Option<ClaudeModelInfo>,
) -> Result<(Command, ClaudeLaunchSettings), RelayError> {
    let base_url = anthropic_base_url(origin)?;
    if !valid_token(token) {
        return Err("relay token for Claude must be non-empty printable text".into());
    }
    for alias in aliases {
        if !valid_model_value(alias) {
            return Err(format!("invalid Claude model alias {alias:?}").into());
        }
    }
    let requested = explicit_model(&args);
    if let Some(model) = requested
        && !valid_model_value(model)
    {
        return Err(format!("invalid --model value {model:?} for Claude").into());
    }
    // Built-in Claude aliases are resolved client-side before the request is
    // sent, so they cannot serve as relay aliases. Use the first usable
    // configured alias as the default and for the picker remaps; an explicit
    // --model is still honoured verbatim even when it names a built-in alias.
    let default = aliases
        .iter()
        .map(String::as_str)
        .find(|alias| !is_reserved_model_alias(alias))
        .or(requested)
        .ok_or(concat!(
            "no usable Claude model alias is configured (built-in aliases such as ",
            "\"sonnet\" are rewritten by Claude Code); pass --model <alias> or rename the alias"
        ))?
        .to_owned();
    let pick = |index: usize| {
        aliases
            .get(index)
            .map(String::as_str)
            .filter(|alias| !is_reserved_model_alias(alias))
            .unwrap_or(&default)
    };
    let opus = pick(0);
    let sonnet = pick(1);
    let haiku = pick(2);

    ensure_no_managed_override(&args)?;
    let mut overlay_env = overlay_env(&base_url, token, &default, opus, sonnet, haiku);
    for name in env_refs
        .iter()
        .map(String::as_str)
        .chain([TOKEN_ENV, DEFAULT_SEARCH_ENV_KEY])
    {
        if !overlay_env.keys().any(|key| key.eq_ignore_ascii_case(name)) {
            overlay_env.insert(name.to_owned(), Value::String(String::new()));
        }
    }
    let mut command = Command::new("claude");
    // Vendor and search credentials are relay-only and must never reach the
    // stock client. The one-time relay token is not needed by Claude either.
    for name in env_refs {
        command.env_remove(name);
    }
    command
        .env_remove(TOKEN_ENV)
        .env_remove(DEFAULT_SEARCH_ENV_KEY);
    for name in SCRUBBED_ENV {
        command.env_remove(name);
    }
    if let Some(path) = options.config_dir {
        let key = "CLAUDE_CONFIG_DIR";
        let value = path
            .to_str()
            .ok_or("Claude Code profile path must be UTF-8")?;
        overlay_env.insert(key.to_owned(), Value::String(value.to_owned()));
        command.env(key, path);
    }

    // Claude Code reads this once per process for custom model IDs. Use the
    // starting model's window, leaving user overrides in env/settings in force.
    const CONTEXT_ENV: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
    let starting_model = match requested {
        Some("opus") => Some(opus),
        Some("sonnet") => Some(sonnet),
        Some("haiku") => Some(haiku),
        Some(model) if is_reserved_model_alias(model) => None,
        Some(model) => Some(model),
        None => Some(default.as_str()),
    };
    let model_info = starting_model.and_then(model_info_for);
    if !env_refs
        .iter()
        .any(|name| name.eq_ignore_ascii_case(CONTEXT_ENV))
        && std::env::var_os(CONTEXT_ENV).is_none()
        && let Some(window) = model_info.as_ref().and_then(|model| model.context_window)
    {
        command.env(CONTEXT_ENV, window.to_string());
    }

    const MODEL_DISCOVERY_ENV: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";
    if !env_refs.contains(MODEL_DISCOVERY_ENV) && std::env::var_os(MODEL_DISCOVERY_ENV).is_none() {
        command.env(MODEL_DISCOVERY_ENV, "1");
    }

    command
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_AUTH_TOKEN", token)
        // Predictable default; an explicit --model still outranks this.
        .env("ANTHROPIC_MODEL", &default)
        // Stock-supported single custom picker entry carrying a qualified ID.
        .env("ANTHROPIC_CUSTOM_MODEL_OPTION", &default)
        .env("ANTHROPIC_CUSTOM_MODEL_OPTION_NAME", &default)
        .env(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            "codeox relay model",
        )
        // Remap the built-in picker aliases so they never resolve to a raw
        // Anthropic model ID that the relay cannot route.
        .env("ANTHROPIC_DEFAULT_OPUS_MODEL", opus)
        .env("ANTHROPIC_DEFAULT_SONNET_MODEL", sonnet)
        .env("ANTHROPIC_DEFAULT_HAIKU_MODEL", haiku);

    for name in ["NO_PROXY", "no_proxy"] {
        let existing = std::env::var(name).unwrap_or_default();
        command.env(name, loopback_no_proxy(&existing));
    }

    const SERVER_REVIEW_ENV: &str = "CLAUDE_CODE_AUTO_MODE_SERVER";
    const CLASSIFIER_MODEL_ENV: &str = "CLAUDE_CODE_AUTO_MODE_MODEL";
    if let Some(classifier) = options.client_classifier {
        overlay_env.insert(SERVER_REVIEW_ENV.to_owned(), Value::String("0".to_owned()));
        command.env(SERVER_REVIEW_ENV, "0");
        if let Some(classifier) = classifier {
            overlay_env.insert(
                CLASSIFIER_MODEL_ENV.to_owned(),
                Value::String(classifier.to_owned()),
            );
            command.env(CLASSIFIER_MODEL_ENV, classifier);
        }
    } else if model_info.is_some_and(|model| !model.can_request_server_review)
        && !env_refs.contains(SERVER_REVIEW_ENV)
        && std::env::var_os(SERVER_REVIEW_ENV).is_none()
    {
        // Matching the Messages format alone does not provide server review.
        // Keep Claude Code's own classifier active on other routes by default.
        command.env(SERVER_REVIEW_ENV, "0");
    }
    let (args, settings) = inject_settings(args, overlay_env, options.starred)?;
    command.args(args).kill_on_drop(true);
    Ok((command, settings))
}

/// Builds the settings `env` object: explicit relay values plus empty values for
/// every other scrubbed variable so a settings file cannot re-introduce them.
fn overlay_env(
    base_url: &str,
    token: &str,
    default: &str,
    opus: &str,
    sonnet: &str,
    haiku: &str,
) -> SettingsMap {
    let explicit = [
        ("ANTHROPIC_BASE_URL", base_url),
        ("ANTHROPIC_AUTH_TOKEN", token),
        ("ANTHROPIC_MODEL", default),
        ("ANTHROPIC_CUSTOM_MODEL_OPTION", default),
        ("ANTHROPIC_CUSTOM_MODEL_OPTION_NAME", default),
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            "codeox relay model",
        ),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", haiku),
    ];
    let mut env = Map::new();
    for name in SCRUBBED_ENV {
        if !explicit
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            env.insert((*name).to_owned(), Value::String(String::new()));
        }
    }
    for (key, value) in explicit {
        env.insert(key.to_owned(), Value::String(value.to_owned()));
    }
    env
}

/// Reads the effective user `--settings` document, overlays `env`, writes the
/// private temporary file, and returns the rewritten argv with the overlay
/// first.
fn inject_settings(
    args: Vec<OsString>,
    overlay_env: SettingsMap,
    starred: &[&str],
) -> Result<(Vec<OsString>, ClaudeLaunchSettings), RelayError> {
    let (mut args, user_settings) = take_user_settings(args)?;
    let mut document = user_settings.unwrap_or_default();
    merge_overlay(&mut document, overlay_env)?;
    let options: Vec<_> = starred
        .iter()
        .filter(|alias| !is_reserved_model_alias(alias))
        .map(|alias| serde_json::json!({ "model": alias, "label": alias }))
        .collect();
    if !options.is_empty() {
        document.entry("modelPicker".to_owned()).or_insert_with(
            || serde_json::json!({ "replaceBuiltInOptions": true, "options": options }),
        );
    }
    // The stock helper runs a command and adds its output as `x-api-key`; the
    // relay authenticates with the bearer, so keep the launch credential one
    // source only. An empty helper is ignored (verified on 2.1.330).
    document.insert("apiKeyHelper".to_owned(), Value::String(String::new()));
    let mut settings = write_settings(&document)?;
    let (mut args, session) = claude_session_args(std::mem::take(&mut args))?;
    settings.session = session;
    let mut rewritten = Vec::with_capacity(args.len() + 2);
    rewritten.push(OsString::from("--settings"));
    rewritten.push(settings.path.clone().into_os_string());
    rewritten.append(&mut args);
    Ok((rewritten, settings))
}

/// Root `claude` subcommands of the compatibility floor, Claude Code 2.1.331,
/// read from the command table that builds its `claude --help` (hidden ones
/// included). None of them starts a conversation. This is the only copy:
/// `desktop/tests/claude_subcommands.py` reads it and fails the Claude protocol
/// job when a stock release lists a root command or alias missing here.
const CLAUDE_SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "design-login",
    "doctor",
    "edit-chrome-settings",
    "edit-hook",
    "edit-memory-settings",
    "edit-permission-rules",
    "edit-sandbox-settings",
    "edit-skill-overrides",
    "gateway",
    "help",
    "import",
    "import-conversations",
    "install",
    "kill",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "purge",
    "rc",
    "remote-control",
    "respawn",
    "rm",
    "sandbox",
    "setup-token",
    "stop",
    "ultrareview",
    "update",
    "upgrade",
    "workspace-trust",
    "worktree-hook",
];

/// Options that choose an existing or remote session, or ask for no saved
/// session. The first five are the session flags. The next six attach to,
/// rewind or teleport another session, where a new ID would conflict. A session
/// that is never saved cannot be resumed, so it gets no ID either.
const CLAUDE_SESSION_OPTIONS: &[&str] = &[
    "--resume",
    "--continue",
    "--session-id",
    "--fork-session",
    "--from-pr",
    "--teleport",
    "--remote",
    "--cloud",
    "--attach-serve",
    "--resume-session-at",
    "--rewind-files",
    "--no-session-persistence",
    "--version",
    "--help",
];

/// `-r`, `-c`, `-v`, `-V` and `-h`, alone or in a cluster of short flags.
const CLAUDE_SESSION_SHORT: &[char] = &['r', 'c', 'v', 'V', 'h'];

/// Gives a new Claude Code session an ID that OX knows before the first turn,
/// so its OX workers can be reclaimed later with `--resume <id>`. Stock Claude
/// Code sends no session identity to MCP servers, so this is the only way the
/// launch learns it. Leaves the arguments unchanged when the user already
/// chose a session, asked for version or help, or ran a subcommand. Arguments
/// after `--` are positional and never inspected.
pub(crate) fn claude_session_args(
    args: Vec<OsString>,
) -> Result<(Vec<OsString>, Option<String>), RelayError> {
    for arg in args.iter().take_while(|arg| *arg != "--") {
        let Some(text) = arg.to_str() else {
            continue;
        };
        let name = text.split('=').next().unwrap_or(text);
        let chooses = if let Some(long) = name.strip_prefix("--") {
            !long.is_empty() && CLAUDE_SESSION_OPTIONS.contains(&name)
        } else if let Some(short) = name.strip_prefix('-') {
            !short.is_empty()
                && short.chars().all(|c| c.is_ascii_alphabetic())
                && short.chars().any(|c| CLAUDE_SESSION_SHORT.contains(&c))
        } else {
            // A value or prompt that happens to equal a subcommand only skips
            // the ID; it never adds one to a subcommand.
            CLAUDE_SUBCOMMANDS.contains(&text)
        };
        if chooses {
            return Ok((args, None));
        }
    }
    let id = new_session_id()?;
    let mut rewritten = Vec::with_capacity(args.len() + 2);
    rewritten.push(OsString::from("--session-id"));
    rewritten.push(OsString::from(&id));
    rewritten.extend(args);
    Ok((rewritten, Some(id)))
}

/// A random RFC 9562 version 4 UUID in lowercase hyphenated form.
fn new_session_id() -> Result<String, RelayError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|error| error.to_string())?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|v| format!("{v:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// The settings document that enables Claude Code's sandbox for write workers.
/// The worker contract also pre-approves Bash when assembling the tool arguments.
pub(crate) fn claude_sandbox_document() -> Value {
    crate::claude_worker_contract::sandbox_document()
}

/// Subscription launches own their authentication settings, so they accept
/// one `--settings` only when it is exactly [`claude_sandbox_document`]. Returns
/// the remaining arguments and whether that document was present.
pub(crate) fn take_subscription_sandbox(
    args: &[OsString],
) -> Result<(Vec<OsString>, bool), RelayError> {
    const REJECT: &str = "subscription launches own their authentication settings";
    let sandbox = claude_sandbox_document();
    let matches_sandbox =
        |value: &str| serde_json::from_str::<Value>(value).ok().as_ref() == Some(&sandbox);
    let mut kept = Vec::with_capacity(args.len());
    let mut found = false;
    let mut index = 0;
    while index < args.len() {
        let text = args[index].to_string_lossy();
        if text == "--settings" {
            let value = args.get(index + 1).map(|value| value.to_string_lossy());
            if !value.as_deref().is_some_and(matches_sandbox) {
                return Err(REJECT.into());
            }
            found = true;
            index += 2;
            continue;
        }
        if let Some(value) = text.strip_prefix("--settings=") {
            if !matches_sandbox(value) {
                return Err(REJECT.into());
            }
            found = true;
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    Ok((kept, found))
}

/// Removes every `--settings` option before `--` and loads the last one, which
/// is the only one stock Claude Code honours. Arguments after `--` are
/// positional and are left untouched.
fn take_user_settings(
    args: Vec<OsString>,
) -> Result<(Vec<OsString>, Option<SettingsMap>), RelayError> {
    let mut kept = Vec::with_capacity(args.len());
    let mut last: Option<OsString> = None;
    let mut index = 0;
    while index < args.len() {
        let Some(text) = args[index].to_str() else {
            kept.push(args[index].clone());
            index += 1;
            continue;
        };
        if text == "--" {
            kept.extend(args[index..].iter().cloned());
            break;
        }
        if text == "--settings" {
            let Some(value) = args.get(index + 1) else {
                return Err("--settings needs a file path or JSON object".into());
            };
            last = Some(value.clone());
            index += 2;
            continue;
        }
        if let Some(value) = text.strip_prefix("--settings=") {
            if value.is_empty() {
                return Err("--settings needs a file path or JSON object".into());
            }
            last = Some(OsString::from(value));
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    let document = match last {
        Some(value) => Some(load_settings(&value)?),
        None => None,
    };
    Ok((kept, document))
}

/// Loads a `--settings` value: inline JSON when it starts with `{`, otherwise a
/// file path, matching the stock CLI's file-or-JSON option.
fn load_settings(value: &OsStr) -> Result<SettingsMap, RelayError> {
    if let Some(text) = value.to_str() {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') {
            return parse_settings_object(trimmed, "--settings JSON");
        }
    }
    let path = Path::new(value);
    let bytes = read_limited(path, "--settings file")?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("--settings file {} is not valid UTF-8", path.display()))?;
    parse_settings_object(&text, "--settings file")
}

/// Parses a settings document without echoing its contents, which can carry
/// hooks, helper commands, or credentials.
fn parse_settings_object(text: &str, what: &str) -> Result<SettingsMap, RelayError> {
    let value: Value =
        serde_json::from_str(text).map_err(|_| format!("{what} is not valid JSON"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(format!("{what} must be a JSON object").into()),
    }
}

/// Reads a launch settings file with a hard size bound.
fn read_limited(path: &Path, what: &str) -> Result<Vec<u8>, RelayError> {
    let mut file = File::open(path)
        .map_err(|error| format!("could not read {what} {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    Read::take(&mut file, MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {what} {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(format!(
            "{what} {} is larger than {MAX_SETTINGS_BYTES} bytes",
            path.display()
        )
        .into());
    }
    Ok(bytes)
}

/// Merges relay routing values over a user settings document, preserving every
/// unrelated key. Env names are compared case-insensitively so a lower-case
/// spelling cannot survive beside the overlay value.
fn merge_overlay(document: &mut SettingsMap, overlay_env: SettingsMap) -> Result<(), RelayError> {
    let existing = document.remove("env");
    let mut env = match existing {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map,
        Some(_) => return Err("--settings env must be a JSON object".into()),
    };
    let ours: BTreeSet<String> = overlay_env
        .keys()
        .map(|key| key.to_ascii_uppercase())
        .collect();
    env.retain(|key, _| !ours.contains(&key.to_ascii_uppercase()));
    for (key, value) in overlay_env {
        env.insert(key, value);
    }
    // Settings can supersede the launch environment. Keep the local bearer
    // off proxies and retain bypasses supplied by the caller.
    let bypass = ["NO_PROXY", "no_proxy"]
        .into_iter()
        .filter_map(|name| {
            env.get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| std::env::var(name).ok())
        })
        .collect::<Vec<_>>()
        .join(",");
    let bypass = loopback_no_proxy(&bypass);
    env.insert("NO_PROXY".to_owned(), Value::String(bypass.clone()));
    env.insert("no_proxy".to_owned(), Value::String(bypass));
    document.insert("env".to_owned(), Value::Object(env));
    Ok(())
}

/// Writes the launch settings document to a fresh `0600` file with a random
/// name and returns its lifetime guard.
fn write_settings(document: &SettingsMap) -> Result<ClaudeLaunchSettings, RelayError> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| "could not generate a temporary settings file name")?;
    let path = std::env::temp_dir().join(format!(
        ".codeox-claude-settings-{}.json",
        hex::encode(random)
    ));
    let text = serde_json::to_vec(document)
        .map_err(|_| "could not encode the launch settings document")?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("could not create a temporary Claude settings file: {error}"))?;
    let written = file.write_all(&text).and_then(|()| file.sync_all());
    if let Err(error) = written {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("could not write a temporary Claude settings file: {error}").into());
    }
    Ok(ClaudeLaunchSettings {
        path,
        session: None,
    })
}

/// Refuses a launch whose backend is controlled above the settings overlay.
///
/// Managed settings (and a host-supplied `--managed-settings`) cannot be
/// overridden by `--settings`. If they select a backend, transport, credential,
/// or model, routing the session through the relay is impossible, so fail with
/// an explicit diagnostic rather than launch a session the relay never sees.
pub(crate) fn ensure_no_managed_override(args: &[OsString]) -> Result<(), RelayError> {
    for arg in args {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "--managed-settings" || text.starts_with("--managed-settings=") {
            return Err(concat!(
                "Claude Code was launched with --managed-settings; host-managed settings ",
                "outrank the launch settings overlay, so codeox cannot guarantee relay routing"
            )
            .into());
        }
    }
    let Some(directory) = managed_settings_dir() else {
        return Ok(());
    };
    for file in managed_settings_files(&directory)? {
        inspect_managed_settings(&file)?;
    }
    Ok(())
}

/// The policy folder stock Claude Code reads for this platform.
fn managed_settings_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from("/Library/Application Support/ClaudeCode"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(r"C:\Program Files\ClaudeCode"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some(PathBuf::from("/etc/claude-code"))
    }
}

/// `managed-settings.json` plus sorted `managed-settings.d/*.json` drop-ins.
///
/// An unreadable policy file fails the launch: its contents cannot be verified,
/// and stock Claude Code treats managed settings as authoritative.
fn managed_settings_files(directory: &Path) -> Result<Vec<PathBuf>, RelayError> {
    let mut files = Vec::new();
    let main = directory.join("managed-settings.json");
    match std::fs::metadata(&main) {
        Ok(metadata) if metadata.is_file() => files.push(main),
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(error) => {
            return Err(format!(
                "could not inspect managed Claude settings {}: {error}",
                main.display()
            )
            .into());
        }
    }

    let dropins = directory.join("managed-settings.d");
    match std::fs::read_dir(&dropins) {
        Ok(entries) => {
            let mut names = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|error| {
                    format!(
                        "could not read managed Claude settings folder {}: {error}",
                        dropins.display()
                    )
                })?;
                let path = entry.path();
                if path.extension().and_then(OsStr::to_str) == Some("json") && path.is_file() {
                    names.push(path);
                }
            }
            names.sort();
            files.extend(names);
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) => {}
        Err(error) => {
            return Err(format!(
                "could not read managed Claude settings folder {}: {error}",
                dropins.display()
            )
            .into());
        }
    }
    Ok(files)
}

/// Errors when a managed document sets a variable the overlay needs to own.
fn inspect_managed_settings(path: &Path) -> Result<(), RelayError> {
    let bytes = read_limited(path, "managed Claude settings")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "managed Claude settings {} are not valid JSON",
            path.display()
        )
    })?;
    if value["apiKeyHelper"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return Err(format!(
            "managed Claude settings {} configure an API key helper",
            path.display()
        )
        .into());
    }
    let Some(env) = value.get("env").and_then(Value::as_object) else {
        return Ok(());
    };
    for (key, value) in env {
        let set = match value {
            Value::String(text) => !text.trim().is_empty(),
            Value::Null => false,
            _ => true,
        };
        if set
            && (key.eq_ignore_ascii_case("CLAUDE_CONFIG_DIR")
                || ROUTING_ENV
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(key)))
        {
            return Err(format!(
                concat!(
                    "managed Claude settings {} set {}, which outranks the launch ",
                    "settings overlay; codeox cannot route this launch through its relay"
                ),
                path.display(),
                key
            )
            .into());
        }
    }
    Ok(())
}

/// Returns the normalized loopback origin Claude Code expects, rejecting any
/// origin that could carry the bearer somewhere other than the local relay.
fn anthropic_base_url(origin: &str) -> Result<String, RelayError> {
    let url = reqwest::Url::parse(origin)
        .map_err(|error| format!("invalid relay origin {origin:?}: {error}"))?;
    if url.scheme() != "http" {
        return Err(format!(
            "Claude relay origin must use plain http on loopback, not {:?}",
            url.scheme()
        )
        .into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Claude relay origin must not carry credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Claude relay origin must not carry a query or fragment".into());
    }
    let host = url.host_str().unwrap_or_default();
    let bare_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if !matches!(bare_host, "127.0.0.1" | "localhost" | "::1") {
        return Err(format!("Claude relay origin {origin:?} is not a loopback address").into());
    }
    let path = url.path().trim_end_matches('/');
    if !path.is_empty() && path != "/v1" {
        return Err(
            format!("Claude relay origin {origin:?} must not carry a path other than /v1").into(),
        );
    }
    let mut base = if bare_host.contains(':') {
        format!("http://[{bare_host}]")
    } else {
        format!("http://{bare_host}")
    };
    if let Some(port) = url.port() {
        base.push(':');
        base.push_str(&port.to_string());
    }
    Ok(base)
}

/// Finds an explicit `--model <value>` or `--model=<value>` before `--`.
pub(crate) fn explicit_model(args: &[OsString]) -> Option<&str> {
    for (index, arg) in args.iter().enumerate() {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "--model" {
            return args
                .get(index + 1)
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty());
        }
        if let Some(value) = text.strip_prefix("--model=") {
            return (!value.is_empty()).then_some(value);
        }
    }
    None
}

/// Short model names Claude Code resolves to a real Anthropic catalog entry
/// before sending the request. A relay alias with one of these names would be
/// rewritten and must not be used as the session default or picker value.
fn is_reserved_model_alias(value: &str) -> bool {
    matches!(
        value,
        "default"
            | "best"
            | "fable"
            | "opus"
            | "sonnet"
            | "haiku"
            | "opusplan"
            | "opus[1m]"
            | "sonnet[1m]"
    )
}

fn valid_model_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MODEL_VALUE_BYTES
        && !value.starts_with(['-', '/'])
        && value
            .chars()
            .all(|ch| !ch.is_control() && !ch.is_whitespace() && !matches!(ch, '"' | '\\'))
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .chars()
            .all(|ch| !ch.is_control() && !ch.is_whitespace())
}

/// Appends loopback hosts to an inherited NO_PROXY value without leaving a
/// leading comma when the inherited value is empty.
fn loopback_no_proxy(existing: &str) -> String {
    let existing = existing.trim().trim_matches(',');
    if existing.is_empty() {
        "127.0.0.1,localhost".to_owned()
    } else {
        format!("{existing},127.0.0.1,localhost")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn subscription_accepts_only_the_sandbox_settings_document() {
        let sandbox = claude_sandbox_document().to_string();
        let (kept, found) =
            take_subscription_sandbox(&strings(&["--print", "--settings", &sandbox, "hi"]))
                .unwrap();
        assert!(found);
        assert_eq!(kept, strings(&["--print", "hi"]));
        let inline = format!("--settings={sandbox}");
        let (kept, found) = take_subscription_sandbox(&strings(&[&inline])).unwrap();
        assert!(found && kept.is_empty());
        let (kept, found) = take_subscription_sandbox(&strings(&["--print"])).unwrap();
        assert!(!found && kept == strings(&["--print"]));
        // The desktop and relay can build serde_json with different map ordering.
        let reordered = r#"{ "sandbox": { "enabled":true, "autoAllowBashIfSandboxed": true } }"#;
        assert!(
            take_subscription_sandbox(&strings(&["--settings", reordered]))
                .unwrap()
                .1
        );
        // Anything else could replace the account settings, so it is refused.
        assert!(take_subscription_sandbox(&strings(&["--settings", "{\"env\":{}}"])).is_err());
        assert!(take_subscription_sandbox(&strings(&["--settings"])).is_err());
        let loose = r#"{"sandbox":{"enabled":false}}"#;
        assert!(take_subscription_sandbox(&strings(&["--settings", loose])).is_err());
    }

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn aliases(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn refs(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn launch(
        args: Vec<OsString>,
        origin: &str,
        token: &str,
        aliases: &[String],
        env_refs: &BTreeSet<String>,
    ) -> (Command, ClaudeLaunchSettings) {
        claude_command(
            args,
            origin,
            token,
            aliases,
            env_refs,
            ClaudeLaunchOptions::default(),
            |_| None,
        )
        .unwrap()
    }

    fn env_entry(command: &Command, name: &str) -> Option<Option<String>> {
        command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(name))
            .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    fn env_set(command: &Command, name: &str) -> Option<String> {
        match env_entry(command, name) {
            Some(Some(value)) => Some(value),
            _ => None,
        }
    }

    fn env_removed(command: &Command, name: &str) -> bool {
        matches!(env_entry(command, name), Some(None))
    }

    fn argv(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The forwarded user argv, after the two overlay tokens.
    /// The user's own arguments after the overlay and any session ID OX added.
    fn forwarded(command: &Command) -> Vec<String> {
        let args = argv(command);
        assert_eq!(args.first().map(String::as_str), Some("--settings"));
        let mut rest: Vec<String> = args.into_iter().skip(2).collect();
        if rest.first().map(String::as_str) == Some("--session-id") {
            assert!(is_uuid_v4(&rest[1]), "{rest:?}");
            rest.drain(..2);
        }
        rest
    }

    fn is_uuid_v4(id: &str) -> bool {
        let parts: Vec<&str> = id.split('-').collect();
        parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
            && id
                .chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            && parts[2].starts_with('4')
            && parts[3].starts_with(['8', '9', 'a', 'b'])
    }

    fn session_args(list: &[&str]) -> (Vec<String>, Option<String>) {
        let (args, id) = claude_session_args(strings(list)).unwrap();
        (
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            id,
        )
    }

    #[test]
    fn a_new_claude_session_gets_a_uuid_v4_session_id() {
        for plain in [
            &[][..],
            &["-p", "list files"],
            &["--print", "hi", "--model", "sonnet"],
            &["fix the failing test"],
            &["--permission-mode", "plan"],
            &["--remote-control"],
            // A prompt that merely mentions a subcommand is still a session.
            &["run the doctor checks"],
        ] {
            let (args, id) = session_args(plain);
            let id = id.unwrap_or_else(|| panic!("{plain:?} starts a session"));
            assert!(is_uuid_v4(&id), "{id}");
            assert_eq!(&args[..2], ["--session-id", id.as_str()]);
            assert_eq!(&args[2..], plain);
        }
        assert_ne!(
            session_args(&[]).1,
            session_args(&[]).1,
            "each launch gets its own ID"
        );
    }

    #[test]
    fn the_users_session_choice_subcommands_and_info_flags_are_left_alone() {
        let unchanged: &[&[&str]] = &[
            // Session flags, in every accepted spelling.
            &["--resume"],
            &["--resume", "0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60"],
            &["--resume=0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60"],
            &["-r"],
            &["-r", "name"],
            &["--continue"],
            &["-c"],
            &["-p", "-c", "go on"],
            &["-pc", "go on"],
            &["--session-id", "0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60"],
            &["--session-id=0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60"],
            &["--resume", "x", "--fork-session"],
            &["--fork-session"],
            &["--from-pr"],
            &["--from-pr", "123"],
            &["--from-pr=123"],
            // Options that attach to or rewind another session.
            &["--teleport"],
            &["--remote", "task"],
            &["--cloud"],
            &["--attach-serve", "abc"],
            &["--resume-session-at", "m1"],
            &["--rewind-files", "m1"],
            // A session that is never saved cannot be resumed.
            &["--no-session-persistence"],
            &["-p", "hi", "--no-session-persistence"],
            // Version and help.
            &["--version"],
            &["-v"],
            &["-V"],
            &["--help"],
            &["-h"],
            &["mcp", "--help"],
            // A background-session command with its argument.
            &["logs", "abc"],
        ];
        for list in unchanged {
            let (args, id) = session_args(list);
            assert_eq!(id, None, "{list:?}");
            assert_eq!(args, *list, "{list:?}");
        }
        for command in CLAUDE_SUBCOMMANDS {
            let (args, id) = session_args(&[command]);
            assert_eq!(id, None, "{command}");
            assert_eq!(args, [*command]);
            let (_, id) = session_args(&["--debug", command, "list"]);
            assert_eq!(id, None, "{command} after an option");
        }
        for listed in [
            "mcp",
            "plugin",
            "update",
            "doctor",
            "install",
            "setup-token",
            "auth",
            "agents",
        ] {
            assert!(CLAUDE_SUBCOMMANDS.contains(&listed), "{listed}");
        }
    }

    #[test]
    fn an_unsaved_session_gets_no_session_id() {
        // Subscription launches call the rewrite directly.
        for list in [
            &["-p", "hi", "--no-session-persistence"][..],
            &["--no-session-persistence", "-p", "hi"],
        ] {
            let (args, id) = session_args(list);
            assert_eq!(id, None, "{list:?}");
            assert_eq!(args, list);
        }
        // API launches keep their overlay and add no ID either.
        let (command, settings) = launch(
            strings(&["-p", "hi", "--no-session-persistence"]),
            "http://127.0.0.1:4321/v1",
            "fresh-token-123",
            &aliases(&["alpha/claude"]),
            &refs(&[]),
        );
        assert_eq!(settings.session, None);
        assert!(!argv(&command).iter().any(|a| a == "--session-id"));
        // After `--` the flag is only prompt text and does not stop the ID.
        assert!(
            session_args(&["-p", "--", "--no-session-persistence"])
                .1
                .is_some()
        );
    }

    #[test]
    fn arguments_after_double_dash_never_decide_the_session() {
        // After `--` everything is positional: a flag-like prompt starts a session.
        let (args, id) = session_args(&["-p", "--", "--resume", "-c", "mcp"]);
        let id = id.unwrap();
        assert_eq!(
            args,
            [
                "--session-id",
                id.as_str(),
                "-p",
                "--",
                "--resume",
                "-c",
                "mcp"
            ]
        );
        // The ID is added before `--`, where stock Claude Code reads options.
        let (args, id) = session_args(&["--", "doctor"]);
        assert_eq!(args[..3], ["--session-id", id.as_deref().unwrap(), "--"]);
        // A session flag before `--` still wins.
        assert_eq!(session_args(&["--continue", "--", "x"]).1, None);
    }

    #[test]
    fn an_api_launch_keeps_its_settings_overlay_and_adds_the_session_id() {
        let (command, settings) = launch(
            strings(&["-p", "hi"]),
            "http://127.0.0.1:4321/v1",
            "fresh-token-123",
            &aliases(&["alpha/claude"]),
            &refs(&[]),
        );
        let args = argv(&command);
        let id = settings
            .session
            .clone()
            .expect("a new API session gets an ID");
        assert_eq!(args[0], "--settings");
        assert_eq!(args[2..4], ["--session-id".to_owned(), id]);
        assert_eq!(args[4..], ["-p".to_owned(), "hi".to_owned()]);
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        // A resumed API session keeps the overlay and gets no new ID.
        let (command, settings) = launch(
            strings(&["--resume", "0b8d6f2e-3c41-4f0a-9e57-1d2c3b4a5f60"]),
            "http://127.0.0.1:4321/v1",
            "fresh-token-123",
            &aliases(&["alpha/claude"]),
            &refs(&[]),
        );
        assert_eq!(settings.session, None);
        assert_eq!(argv(&command)[0], "--settings");
        assert!(!argv(&command).iter().any(|a| a == "--session-id"));
    }

    /// The overlay file path from argv, asserting exactly one tokens pair.
    fn overlay_path(command: &Command) -> String {
        let args = argv(command);
        assert_eq!(args.first().map(String::as_str), Some("--settings"));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--settings")
                .count(),
            1,
            "exactly one --settings must reach the child: {args:?}"
        );
        args[1].clone()
    }

    fn overlay_document(settings: &ClaudeLaunchSettings) -> Value {
        let bytes = std::fs::read(settings.path()).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn overlay_env_value(settings: &ClaudeLaunchSettings, name: &str) -> Option<String> {
        overlay_document(settings)["env"][name]
            .as_str()
            .map(str::to_owned)
    }

    /// A unique temporary directory removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).unwrap();
            let path =
                std::env::temp_dir().join(format!("codeox-clients-{tag}-{}", hex::encode(random)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, text: &str) {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn select_consumes_only_the_exact_first_word() {
        assert_eq!(select(Vec::new()), (ClientKind::Codex, Vec::new()));
        assert_eq!(
            select(strings(&["codex", "--model", "a"])),
            (ClientKind::Codex, strings(&["--model", "a"]))
        );
        assert_eq!(
            select(strings(&["claude", "--model", "a"])),
            (ClientKind::Claude, strings(&["--model", "a"]))
        );
        assert_eq!(
            select(strings(&["claude"])),
            (ClientKind::Claude, Vec::new())
        );
        assert_eq!(
            select(strings(&["codex", "claude"])),
            (ClientKind::Codex, strings(&["claude"]))
        );
    }

    #[test]
    fn select_preserves_unprefixed_codex_arguments() {
        assert_eq!(
            select(strings(&["--model", "codex/claude"])),
            (ClientKind::Codex, strings(&["--model", "codex/claude"]))
        );
        assert_eq!(
            select(strings(&["Codex"])),
            (ClientKind::Codex, strings(&["Codex"]))
        );
        assert_eq!(
            select(strings(&["claude-code"])),
            (ClientKind::Codex, strings(&["claude-code"]))
        );
        assert_eq!(select(strings(&[""])), (ClientKind::Codex, strings(&[""])));
    }

    #[cfg(unix)]
    #[test]
    fn select_treats_non_utf8_as_an_unprefixed_codex_argument() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(vec![0xff, b'c']);
        assert_eq!(select(vec![raw.clone()]), (ClientKind::Codex, vec![raw]));
    }

    #[test]
    fn base_url_is_an_origin_without_v1_or_trailing_slash() {
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:4000").unwrap(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:4000/").unwrap(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:4000/v1").unwrap(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:4000/v1/").unwrap(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            anthropic_base_url("http://localhost:9/v1").unwrap(),
            "http://localhost:9"
        );
        assert_eq!(
            anthropic_base_url("http://[::1]:9").unwrap(),
            "http://[::1]:9"
        );
    }

    #[test]
    fn base_url_rejects_non_loopback_or_extra_paths() {
        for origin in [
            "https://127.0.0.1:4000",
            "http://example.com:4000",
            "http://127.0.0.1:4000/api",
            "http://127.0.0.1:4000/v1/messages",
            "http://127.0.0.1:4000?token=x",
            "http://127.0.0.1:4000#fragment",
            "http://user:pass@127.0.0.1:4000",
            "not a url",
        ] {
            assert!(
                anthropic_base_url(origin).is_err(),
                "{origin:?} should be rejected"
            );
        }
    }

    #[test]
    fn loopback_no_proxy_appends_once_without_empty_entries() {
        assert_eq!(loopback_no_proxy(""), "127.0.0.1,localhost");
        assert_eq!(loopback_no_proxy(" , "), "127.0.0.1,localhost");
        assert_eq!(
            loopback_no_proxy("corp.example"),
            "corp.example,127.0.0.1,localhost"
        );
        assert_eq!(loopback_no_proxy("a,b,"), "a,b,127.0.0.1,localhost");
    }

    #[test]
    fn explicit_model_understands_both_forms_and_stops_at_double_dash() {
        assert_eq!(explicit_model(&strings(&["--model", "a/b"])), Some("a/b"));
        assert_eq!(explicit_model(&strings(&["--model=a/b"])), Some("a/b"));
        assert_eq!(
            explicit_model(&strings(&["-p", "hi", "--model", "a/b"])),
            Some("a/b")
        );
        assert_eq!(explicit_model(&strings(&["--", "--model", "a/b"])), None);
        assert_eq!(explicit_model(&strings(&["--model"])), None);
        assert_eq!(explicit_model(&strings(&["--model="])), None);
    }

    #[test]
    fn command_forwards_arguments_and_binds_the_loopback_bearer() {
        let args = strings(&["-p", "list files", "--model", "beta/gpt-5"]);
        let (command, settings) = launch(
            args.clone(),
            "http://127.0.0.1:4321/v1",
            "fresh-token-123",
            &aliases(&["alpha/claude", "beta/gpt-5"]),
            &refs(&[]),
        );

        assert_eq!(
            forwarded(&command),
            words(&["-p", "list files", "--model", "beta/gpt-5"])
        );
        let overlay = overlay_path(&command);
        assert!(!overlay.contains("fresh-token-123"));
        assert!(overlay.ends_with(".json"));
        assert_eq!(
            env_set(&command, "ANTHROPIC_BASE_URL").as_deref(),
            Some("http://127.0.0.1:4321")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("fresh-token-123")
        );
        // The predictable default remains the first configured alias; Claude
        // Code gives the explicit --model argument precedence over it.
        assert_eq!(
            env_set(&command, "ANTHROPIC_MODEL").as_deref(),
            Some("alpha/claude")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_CUSTOM_MODEL_OPTION").as_deref(),
            Some("alpha/claude")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME").as_deref(),
            Some("alpha/claude")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION").as_deref(),
            Some("codeox relay model")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_OPUS_MODEL").as_deref(),
            Some("alpha/claude")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_SONNET_MODEL").as_deref(),
            Some("beta/gpt-5")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_HAIKU_MODEL").as_deref(),
            Some("alpha/claude")
        );

        assert_eq!(
            overlay_env_value(&settings, "ANTHROPIC_BASE_URL").as_deref(),
            Some("http://127.0.0.1:4321")
        );
        assert_eq!(
            overlay_env_value(&settings, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("fresh-token-123")
        );
        assert_eq!(
            overlay_env_value(&settings, "ANTHROPIC_MODEL").as_deref(),
            Some("alpha/claude")
        );
    }

    #[test]
    fn overlay_is_private_lives_only_for_the_launch_and_carries_no_vendor_key() {
        let (command, settings) = launch(
            strings(&["-p", "hi"]),
            "http://127.0.0.1:1",
            "ephemeral-token",
            &aliases(&["a/b"]),
            &refs(&["OPENROUTER_API_KEY"]),
        );
        let path = PathBuf::from(overlay_path(&command));
        assert_eq!(path, settings.path());
        assert!(path.exists());
        assert!(!argv(&command).iter().any(|arg| arg == "ephemeral-token"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let document = overlay_document(&settings);
        let env = document["env"].as_object().unwrap();
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "ephemeral-token");
        assert_eq!(env["OPENROUTER_API_KEY"], "");
        assert_eq!(document["apiKeyHelper"], "");

        drop(settings);
        assert!(!path.exists(), "the guard must remove the settings file");
    }

    #[test]
    fn overlay_neutralizes_every_other_backend_switch_and_credential() {
        let (_, settings) = launch(
            Vec::new(),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["a/b"]),
            &refs(&[]),
        );
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_CUSTOM_HEADERS",
            "ANTHROPIC_UNIX_SOCKET",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
            "CLAUDE_CODE_USE_GATEWAY",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
            "ANTHROPIC_FOUNDRY_BASE_URL",
            "ANTHROPIC_FOUNDRY_API_KEY",
            "ANTHROPIC_AWS_API_KEY",
            "AWS_BEARER_TOKEN_BEDROCK",
            "CLAUDE_CODE_GATEWAY_TOKEN",
        ] {
            assert_eq!(
                overlay_env_value(&settings, name).as_deref(),
                Some(""),
                "{name} must be neutralized in the settings overlay"
            );
        }
    }

    #[test]
    fn overlay_preserves_unrelated_user_settings_and_replaces_routing_values() {
        let user = r#"{
            "permissions":{"deny":["Bash(rm *)"],"allow":["Read"]},
            "hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"true"}]}]},
            "env":{
                "ANTHROPIC_BASE_URL":"http://127.0.0.1:9",
                "ANTHROPIC_AUTH_TOKEN":"user-token",
                "ANTHROPIC_API_KEY":"user-key",
                "VENDOR_API_KEY":"vendor-secret",
                "no_proxy":"example.invalid",
                "SOME_UNRELATED_FLAG":"keep-me",
                "anthropic_model":"lower-case-should-be-replaced"
            }
        }"#;
        let (command, settings) = launch(
            strings(&["--settings", user, "-p", "hi"]),
            "http://127.0.0.1:1",
            "relay-token",
            &aliases(&["a/b"]),
            &refs(&["VENDOR_API_KEY"]),
        );
        assert_eq!(forwarded(&command), words(&["-p", "hi"]));

        let document = overlay_document(&settings);
        assert_eq!(document["permissions"]["deny"][0], "Bash(rm *)");
        assert_eq!(document["permissions"]["allow"][0], "Read");
        assert_eq!(document["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        let env = document["env"].as_object().unwrap();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:1");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "relay-token");
        assert_eq!(env["ANTHROPIC_MODEL"], "a/b");
        assert_eq!(env["SOME_UNRELATED_FLAG"], "keep-me");
        assert_eq!(env["ANTHROPIC_API_KEY"], "");
        assert_eq!(env["VENDOR_API_KEY"], "");
        let bypass = env["no_proxy"].as_str().unwrap();
        assert!(bypass.contains("example.invalid"));
        assert!(bypass.contains("127.0.0.1"));
        assert!(
            !env.keys()
                .any(|key| key.eq_ignore_ascii_case("anthropic_model") && key != "ANTHROPIC_MODEL"),
            "case variants of routing keys must not survive: {env:?}"
        );
        assert!(
            !serde_json::to_string(&document)
                .unwrap()
                .contains("user-token")
        );
        assert!(
            !serde_json::to_string(&document)
                .unwrap()
                .contains("user-key")
        );
    }

    #[test]
    fn overlay_reads_a_user_settings_file_and_the_equals_form() {
        let dir = TempDir::new("user-file");
        dir.write(
            "settings.json",
            r#"{"env":{"ANTHROPIC_MODEL":"vendor/from-file"},"outputStyle":"Explanatory"}"#,
        );
        let path = dir.path().join("settings.json");
        let (command, settings) = launch(
            vec![
                OsString::from(format!("--settings={}", path.display())),
                OsString::from("-p"),
                OsString::from("hi"),
            ],
            "http://127.0.0.1:1",
            "relay-token",
            &aliases(&["a/b"]),
            &refs(&[]),
        );
        assert_eq!(forwarded(&command), words(&["-p", "hi"]));
        let document = overlay_document(&settings);
        assert_eq!(document["outputStyle"], "Explanatory");
        assert_eq!(document["env"]["ANTHROPIC_MODEL"], "a/b");
    }

    #[test]
    fn only_the_last_repeated_settings_value_is_preserved() {
        // Stock Claude Code honours the last --settings only; earlier ones
        // supplied neither env nor other settings in the probe.
        let (command, settings) = launch(
            strings(&[
                "--settings",
                r#"{"permissions":{"deny":["Bash"]}}"#,
                "--settings",
                r#"{"env":{"SOME_UNRELATED_FLAG":"from-last"}}"#,
                "-p",
                "hi",
            ]),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["a/b"]),
            &refs(&[]),
        );
        assert_eq!(forwarded(&command), words(&["-p", "hi"]));
        let document = overlay_document(&settings);
        assert!(document.get("permissions").is_none());
        assert_eq!(document["env"]["SOME_UNRELATED_FLAG"], "from-last");
        assert_eq!(document["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:1");
    }

    #[test]
    fn settings_after_double_dash_stays_positional() {
        let (command, settings) = launch(
            strings(&["--", "--settings", r#"{"permissions":{}}"#, "-p", "hi"]),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["a/b"]),
            &refs(&[]),
        );
        assert_eq!(
            forwarded(&command),
            words(&["--", "--settings", r#"{"permissions":{}}"#, "-p", "hi"])
        );
        assert!(overlay_document(&settings).get("permissions").is_none());
    }

    #[test]
    fn invalid_or_missing_settings_arguments_are_rejected() {
        for args in [
            strings(&["--settings"]),
            strings(&["--settings="]),
            strings(&["--settings", "not json {"]),
            strings(&["--settings", r#"["not","an","object"]"#]),
            strings(&["--settings", r#"{"env":[]}"#]),
        ] {
            assert!(
                claude_command(
                    args,
                    "http://127.0.0.1:1",
                    "t",
                    &aliases(&["a/b"]),
                    &refs(&[]),
                    ClaudeLaunchOptions::default(),
                    |_| None,
                )
                .is_err()
            );
        }
        let dir = TempDir::new("missing");
        let missing = dir.path().join("absent.json");
        assert!(
            claude_command(
                vec![OsString::from("--settings"), missing.into_os_string()],
                "http://127.0.0.1:1",
                "t",
                &aliases(&["a/b"]),
                &refs(&[]),
                ClaudeLaunchOptions::default(),
                |_| None,
            )
            .is_err()
        );
    }

    #[test]
    fn managed_routing_set_is_a_subset_of_the_scrubbed_set() {
        for name in ROUTING_ENV {
            assert!(
                SCRUBBED_ENV.contains(name),
                "{name} must also be scrubbed from the child environment"
            );
        }
        // Model selection is overlaid but not a routing conflict; a managed
        // model still reaches the relay.
        for name in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_CUSTOM_MODEL_OPTION",
        ] {
            assert!(
                !ROUTING_ENV.contains(&name),
                "{name} must not block a launch"
            );
        }
    }

    #[test]
    fn managed_routing_env_fails_the_launch_before_any_settings_file_is_written() {
        let dir = TempDir::new("managed");
        dir.write(
            "managed-settings.json",
            r#"{"permissions":{"deny":["Bash"]}}"#,
        );
        assert_eq!(managed_settings_files(dir.path()).unwrap().len(), 1);
        for file in managed_settings_files(dir.path()).unwrap() {
            inspect_managed_settings(&file).unwrap();
        }

        dir.write(
            "managed-settings.json",
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9"}}"#,
        );
        let error =
            inspect_managed_settings(&dir.path().join("managed-settings.json")).unwrap_err();
        assert!(error.to_string().contains("ANTHROPIC_BASE_URL"));
        assert!(!error.to_string().contains("127.0.0.1:9"));

        dir.write(
            "managed-settings.json",
            r#"{"env":{"ANTHROPIC_MODEL":"corp/model","CLAUDE_CODE_USE_BEDROCK":""}}"#,
        );
        inspect_managed_settings(&dir.path().join("managed-settings.json")).unwrap();
        dir.write(
            "managed-settings.d/10-key.json",
            r#"{"env":{"ANTHROPIC_API_KEY":"admin-key"}}"#,
        );
        let files = managed_settings_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        for file in files {
            let result = inspect_managed_settings(&file);
            if file.ends_with("10-key.json") {
                let error = result.unwrap_err();
                assert!(error.to_string().contains("ANTHROPIC_API_KEY"));
                assert!(!error.to_string().contains("admin-key"));
            } else {
                result.unwrap();
            }
        }

        dir.write("managed-settings.d/10-key.json", "{not json");
        assert!(
            inspect_managed_settings(&dir.path().join("managed-settings.d/10-key.json")).is_err()
        );
    }

    #[test]
    fn host_managed_settings_flag_is_refused() {
        for args in [
            strings(&["--managed-settings", "/tmp/policy.json", "-p", "hi"]),
            strings(&["--managed-settings=/tmp/policy.json"]),
        ] {
            let error = claude_command(
                args,
                "http://127.0.0.1:1",
                "t",
                &aliases(&["a/b"]),
                &refs(&[]),
                ClaudeLaunchOptions::default(),
                |_| None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("managed"));
        }
        // After `--` it is a positional prompt and must not block the launch.
        launch(
            strings(&["--", "--managed-settings"]),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["a/b"]),
            &refs(&[]),
        );
    }

    #[test]
    fn command_maps_three_distinct_aliases_onto_the_stock_picker() {
        let (command, _) = launch(
            Vec::new(),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["one/a", "two/b", "three/c"]),
            &refs(&[]),
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_MODEL").as_deref(),
            Some("one/a")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_OPUS_MODEL").as_deref(),
            Some("one/a")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_SONNET_MODEL").as_deref(),
            Some("two/b")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_HAIKU_MODEL").as_deref(),
            Some("three/c")
        );
    }

    #[test]
    fn command_scrubs_vendor_search_personal_and_relay_credentials() {
        let (command, _) = launch(
            Vec::new(),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["a/b"]),
            &refs(&["OPENROUTER_API_KEY", "BRAVE_SEARCH_API_KEY"]),
        );

        for name in [
            "OPENROUTER_API_KEY",
            "BRAVE_SEARCH_API_KEY",
            TOKEN_ENV,
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_CUSTOM_HEADERS",
            "ANTHROPIC_UNIX_SOCKET",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
            "CLAUDE_CODE_SESSIONS_OAUTH_TOKEN",
            "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
            "CLAUDE_CODE_USE_GATEWAY",
            "ANTHROPIC_BEDROCK_BASE_URL",
            "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
            "ANTHROPIC_VERTEX_BASE_URL",
            "ANTHROPIC_FOUNDRY_BASE_URL",
            "ANTHROPIC_AWS_BASE_URL",
            "ANTHROPIC_GOOGLE_CLOUD_BASE_URL",
            "ANTHROPIC_FOUNDRY_API_KEY",
            "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
            "ANTHROPIC_AWS_API_KEY",
            "AWS_BEARER_TOKEN_BEDROCK",
            "CLAUDE_CODE_GATEWAY_TOKEN",
            "ANTHROPIC_SMALL_FAST_MODEL",
        ] {
            assert!(env_removed(&command, name), "{name} must be removed");
        }
    }

    #[test]
    fn empty_alias_list_accepts_only_an_explicit_model() {
        let error = claude_command(
            Vec::new(),
            "http://127.0.0.1:1",
            "t",
            &[],
            &refs(&[]),
            ClaudeLaunchOptions::default(),
            |_| None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--model"));

        let (command, settings) = launch(
            strings(&["--model", "vendor/model"]),
            "http://127.0.0.1:1",
            "t",
            &[],
            &refs(&[]),
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_MODEL").as_deref(),
            Some("vendor/model")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_OPUS_MODEL").as_deref(),
            Some("vendor/model")
        );
        assert_eq!(forwarded(&command), words(&["--model", "vendor/model"]));
        assert_eq!(
            overlay_env_value(&settings, "ANTHROPIC_MODEL").as_deref(),
            Some("vendor/model")
        );
    }

    #[test]
    fn built_in_alias_collisions_are_skipped_for_defaults_and_picker() {
        let (command, _) = launch(
            Vec::new(),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["sonnet", "alpha/model"]),
            &refs(&[]),
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_MODEL").as_deref(),
            Some("alpha/model")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_CUSTOM_MODEL_OPTION").as_deref(),
            Some("alpha/model")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_OPUS_MODEL").as_deref(),
            Some("alpha/model")
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_DEFAULT_SONNET_MODEL").as_deref(),
            Some("alpha/model")
        );

        let error = claude_command(
            Vec::new(),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["sonnet"]),
            &refs(&[]),
            ClaudeLaunchOptions::default(),
            |_| None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("built-in"));

        // An explicit --model is forwarded and honoured even when it is a
        // built-in alias that Claude Code will resolve itself.
        let (command, _) = launch(
            strings(&["--model", "sonnet"]),
            "http://127.0.0.1:1",
            "t",
            &aliases(&["sonnet"]),
            &refs(&[]),
        );
        assert_eq!(
            env_set(&command, "ANTHROPIC_MODEL").as_deref(),
            Some("sonnet")
        );
        assert_eq!(forwarded(&command), words(&["--model", "sonnet"]));
    }

    #[test]
    fn invalid_aliases_tokens_and_models_are_rejected() {
        for alias in [
            String::new(),
            "has space".to_owned(),
            "has\nnewline".to_owned(),
            "-starts-with-flag".to_owned(),
            "/starts-with-slash".to_owned(),
            "has\"quote".to_owned(),
            "has\\backslash".to_owned(),
            "x".repeat(MAX_MODEL_VALUE_BYTES + 1),
        ] {
            assert!(
                claude_command(
                    Vec::new(),
                    "http://127.0.0.1:1",
                    "t",
                    std::slice::from_ref(&alias),
                    &refs(&[]),
                    ClaudeLaunchOptions::default(),
                    |_| None,
                )
                .is_err(),
                "{alias:?} should be rejected"
            );
        }

        for token in ["", "has space", "has\nnewline"] {
            assert!(
                claude_command(
                    Vec::new(),
                    "http://127.0.0.1:1",
                    token,
                    &aliases(&["a/b"]),
                    &refs(&[]),
                    ClaudeLaunchOptions::default(),
                    |_| None,
                )
                .is_err(),
                "{token:?} should be rejected"
            );
        }

        assert!(
            claude_command(
                strings(&["--model", "bad model"]),
                "http://127.0.0.1:1",
                "t",
                &aliases(&["a/b"]),
                &refs(&[]),
                ClaudeLaunchOptions::default(),
                |_| None,
            )
            .is_err()
        );
    }
}
