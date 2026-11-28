//! Configuration, alias stability, and request routing for codeox.
//!
//! Two configuration modes are supported:
//!
//! * **codeox mode** - `$CODEX_HOME/codeox.toml` describes one or more
//!   Responses API vendors. Every vendor gets a stable, provider-qualified
//!   model alias (`<provider>/<model-id>`), and requests are routed back to the
//!   owning vendor with that vendor's upstream model ID and credentials.
//! * **legacy mode** - when no `codeox.toml` exists, the original
//!   single-provider behaviour is preserved: the provider named by
//!   `model_provider` in `$CODEX_HOME/config.toml` is relayed and upstream
//!   model IDs are used unchanged.
//!
//! Three wire formats are supported. `responses` is relayed unchanged;
//! `anthropic_messages` and `chat_completions` are translated by the adapters
//! in `crate::adapters` and the transport in `crate::upstream`.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};

use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use toml_edit::{Document, Item, TableLike};

use crate::catalog::ModelSpec;

pub type RelayError = Box<dyn Error + Send + Sync>;

/// The one provider ID codeox injects into Codex, for a single launch only.
pub const PROVIDER_ID: &str = "codeox";
/// Environment variable carrying the loopback token into the Codex child.
pub const TOKEN_ENV: &str = "CODEOX_TOKEN";
/// Environment variable carrying the key accepted by the persistent listener.
pub const GATEWAY_KEY_ENV: &str = "OX_GATEWAY_KEY";
/// Optional multi-vendor configuration file inside `$CODEX_HOME`.
pub const CONFIG_FILE: &str = "codeox.toml";
/// Upper bound for reading the configuration file.
pub const MAX_CONFIG_BYTES: u64 = 1 << 20;

pub fn credential_env_error(name: &str) -> Option<&'static str> {
    match name {
        TOKEN_ENV => Some("CODEOX_TOKEN is a reserved relay token variable"),
        GATEWAY_KEY_ENV => Some(
            "OX_GATEWAY_KEY is reserved for the shared gateway key. Use a vendor API key variable such as OPENROUTER_API_KEY",
        ),
        _ if looks_like_credential(name)
            || name.len() > 128
            || !name
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
        {
            Some(
                "expected an environment variable name such as OPENROUTER_API_KEY. Keep the secret value in your shell",
            )
        }
        _ => None,
    }
}

pub fn looks_like_credential(value: &str) -> bool {
    ["sk-", "sk_", "oc_sk_", "xox", "ghp_", "github_pat_", "aiza"]
        .iter()
        .any(|prefix| {
            value
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        })
}

const MAX_PROVIDER_KEY_BYTES: usize = 64;
const MAX_MODEL_ID_BYTES: usize = 200;
const MAX_ALIAS_BYTES: usize = 256;
const MAX_AMBIGUOUS_ALIASES: usize = 4;

/// Reads a configuration value from the environment.
pub type Env<'a> = dyn Fn(&str) -> Option<String> + Sync + 'a;

pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// A key the upstream can accept as a bearer token: non-empty and without any
/// whitespace. Callers trim surrounding whitespace first.
pub(crate) fn usable_key(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_whitespace)
}

/// A local store for keys pasted into the terminal setup. Scope each key to
/// both its variable name and destination so editing a URL cannot forward it.
#[derive(Default, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub(crate) struct SavedKeys(BTreeMap<String, SavedKey>);

#[derive(serde::Deserialize, serde::Serialize)]
struct SavedKey {
    scope: String,
    value: String,
}

impl SavedKeys {
    pub(crate) const BRAVE_SCOPE: &str = "https://api.search.brave.com/";

    pub(crate) fn path(home: &Path) -> PathBuf {
        home.join("ox-api-keys.json")
    }

    pub(crate) fn load(home: &Path) -> Result<Self, RelayError> {
        use std::io::Read;

        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(Self::path(home)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => return Err("could not read the local ox API key file".into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err("ox-api-keys.json must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid takes no arguments and has no memory preconditions.
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(
                    "ox-api-keys.json must be owned by the current user with mode 0600".into(),
                );
            }
            if metadata.mode() & 0o077 != 0 {
                // The file is ours, so tightening it is always safe. Repair
                // it instead of refusing to start.
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(
                        |_| "ox-api-keys.json must be owned by the current user with mode 0600",
                    )?;
                eprintln!(
                    "ox: fixed ox-api-keys.json, which other users on this device could read. It is private again (mode 0600)."
                );
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err("ox-api-keys.json is too large".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "could not parse ox-api-keys.json".into())
    }

    pub(crate) fn for_config(home: &Path, config: &CodeoxConfig) -> Result<Self, RelayError> {
        let mut keys = Self::load(home)?;
        keys.0
            .retain(|name, entry| Self::scope_matches(config, name, &entry.scope));
        Ok(keys)
    }

    pub(crate) fn scope_matches(config: &CodeoxConfig, name: &str, scope: &str) -> bool {
        let vendors = config
            .vendors
            .iter()
            .filter(|vendor| vendor.env_key.as_deref() == Some(name))
            .map(|vendor| vendor.base_url.as_str());
        let search = config
            .search
            .iter()
            .filter(|search| search.env_key == name)
            .map(|_| Self::BRAVE_SCOPE);
        let mut scopes = vendors.chain(search).peekable();
        scopes.peek().is_some() && scopes.all(|destination| destination == scope)
    }

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.resolve(name, process_env(name))
    }

    /// A usable environment value (surrounding whitespace trimmed) wins. A malformed
    /// one falls through to the saved key. With no saved key it is returned as-is so
    /// the caller can report that the variable is set but malformed.
    fn resolve(&self, name: &str, env_value: Option<String>) -> Option<String> {
        let saved = || self.0.get(name).map(|key| key.value.clone());
        match env_value {
            None => saved(),
            Some(value) => {
                let trimmed = value.trim();
                if usable_key(trimmed) {
                    Some(trimmed.to_owned())
                } else {
                    saved().or(Some(value))
                }
            }
        }
    }

    pub(crate) fn scoped_value(&self, name: &str, scope: &str) -> Option<String> {
        self.0
            .get(name)
            .filter(|key| key.scope == scope)
            .map(|key| key.value.clone())
    }

    pub(crate) fn scopes(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.scope.as_str()))
    }

    pub(crate) fn remove(&mut self, name: &str, scope: &str) -> Result<String, RelayError> {
        if !self.0.get(name).is_some_and(|entry| entry.scope == scope) {
            return Err("The saved credential changed. Reload settings and try again.".into());
        }
        self.0.remove(name);
        Ok(serde_json::to_string(self)?)
    }

    pub(crate) fn set(
        &mut self,
        name: &str,
        scope: &str,
        value: String,
    ) -> Result<String, RelayError> {
        self.insert_scoped(name, scope, value);
        serde_json::to_string(self).map_err(|_| "could not encode ox-api-keys.json".into())
    }

    pub(crate) fn insert_scoped(&mut self, name: &str, scope: &str, value: String) {
        self.0.insert(
            name.to_owned(),
            SavedKey {
                scope: scope.to_owned(),
                value,
            },
        );
    }
}

pub fn configured_web_search_policy(
    document: &Document<&str>,
    profile: Option<&str>,
) -> Option<String> {
    let profile = profile.or_else(|| document.get("profile").and_then(Item::as_str));
    let profile_policy = profile.and_then(|profile| {
        document
            .get("profiles")?
            .as_table()?
            .get(profile)?
            .as_table()?
            .get("web_search")?
            .as_str()
    });
    profile_policy
        .or_else(|| document.get("web_search").and_then(Item::as_str))
        .map(normalise_web_search_policy)
}

pub fn normalise_web_search_policy(value: &str) -> String {
    value.trim().trim_matches(['"', '\'']).to_ascii_lowercase()
}

pub fn bearer(token: &str) -> Result<HeaderValue, RelayError> {
    Ok(HeaderValue::from_str(&format!("Bearer {token}"))?)
}

/// A vendor API key kept as text, because the header that carries it depends
/// on the wire protocol (`Authorization: Bearer` versus `x-api-key`).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(String);

impl SecretKey {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The only way to read the secret. Call sites are the outbound request
    /// builders; nothing logs or formats this type.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretKey([redacted])")
    }
}

/// Upstream wire protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Responses,
    AnthropicMessages,
    ChatCompletions,
}

impl Wire {
    fn parse(raw: Option<&str>) -> Result<Self, RelayError> {
        match raw.map(str::trim) {
            None | Some("") | Some("responses") => Ok(Self::Responses),
            Some("anthropic_messages") => Ok(Self::AnthropicMessages),
            Some("chat_completions") => Ok(Self::ChatCompletions),
            Some(other) => Err(format!(
                "unknown wire_api {other:?}; use \"responses\", \"anthropic_messages\" or \"chat_completions\""
            )
            .into()),
        }
    }

    /// Whether Codex requests for this vendor need protocol translation.
    pub fn is_translated(self) -> bool {
        !matches!(self, Self::Responses)
    }

    #[allow(dead_code)] // used by the adapter tests and configuration diagnostics
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::ChatCompletions => "chat_completions",
        }
    }
}

/// How `reasoning.effort` is represented for an Anthropic vendor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Do not request thinking and drop `output_config`. Works with every
    /// Anthropic API version, but signed-thinking replay is never exercised.
    #[default]
    Off,
    /// `thinking: {type: "enabled", budget_tokens: N}`. Claude 3.7+/4 models.
    Manual,
    /// `thinking: {type: "adaptive"}` for models that support it.
    Adaptive,
}

impl ThinkingMode {
    fn parse(raw: Option<&str>) -> Result<Self, RelayError> {
        match raw.map(str::trim) {
            None | Some("") | Some("off") => Ok(Self::Off),
            Some("manual") => Ok(Self::Manual),
            Some("adaptive") => Ok(Self::Adaptive),
            Some(other) => Err(format!(
                "anthropic_thinking must be \"off\", \"manual\" or \"adaptive\", not {other:?}"
            )
            .into()),
        }
    }
}

/// Anthropic Messages options for one vendor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnthropicOptions {
    pub version: String,
    pub(crate) version_pinned: bool,
    pub thinking: ThinkingMode,
    pub thinking_budget_tokens: u64,
    /// Whether an `output_config.effort` hint is sent. Implied by
    /// [`ThinkingMode::Off`] on old API versions; see [`AnthropicOptions::wants_effort`].
    pub effort: bool,
}

impl Default for AnthropicOptions {
    fn default() -> Self {
        Self {
            version: "2023-06-01".to_owned(),
            version_pinned: false,
            thinking: ThinkingMode::Off,
            thinking_budget_tokens: 4096,
            effort: false,
        }
    }
}

impl AnthropicOptions {
    /// API versions from 2026-10-21 remove `temperature`, `top_p` and `top_k`.
    pub fn rejects_sampling(&self) -> bool {
        Self::version_rejects_sampling(&self.version)
    }

    pub(crate) fn version_rejects_sampling(version: &str) -> bool {
        version >= "2026-10-21"
    }

    /// `output_config.effort` is only sent when explicitly requested, because
    /// the 2023-06-01 API does not define it.
    pub fn wants_effort(&self) -> bool {
        self.effort
    }
}

/// Which Chat Completions field carries `max_output_tokens`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MaxTokensField {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

impl MaxTokensField {
    fn parse(raw: Option<&str>) -> Result<Self, RelayError> {
        match raw.map(str::trim) {
            None | Some("") | Some("max_tokens") => Ok(Self::MaxTokens),
            Some("max_completion_tokens") => Ok(Self::MaxCompletionTokens),
            Some(other) => Err(format!(
                "max_tokens_field must be \"max_tokens\" or \"max_completion_tokens\", not {other:?}"
            )
            .into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReasoningEffortPolicy {
    #[default]
    Drop,
    Forward,
}

impl ReasoningEffortPolicy {
    fn parse(raw: Option<&str>) -> Result<Self, RelayError> {
        match raw.map(str::trim) {
            None | Some("") | Some("drop") => Ok(Self::Drop),
            Some("forward") => Ok(Self::Forward),
            Some(other) => Err(format!(
                "chat_reasoning_effort must be \"drop\" or \"forward\", not {other:?}"
            )
            .into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeveloperRole {
    #[default]
    System,
    Developer,
}

impl DeveloperRole {
    fn parse(raw: Option<&str>) -> Result<Self, RelayError> {
        match raw.map(str::trim) {
            None | Some("") | Some("system") => Ok(Self::System),
            Some("developer") => Ok(Self::Developer),
            Some(other) => Err(format!(
                "developer_role must be \"system\" or \"developer\", not {other:?}"
            )
            .into()),
        }
    }
}

/// Chat Completions options for one vendor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatOptions {
    pub max_tokens_field: MaxTokensField,
    pub reasoning_effort: ReasoningEffortPolicy,
    pub developer_role: DeveloperRole,
    pub include_stream_usage: bool,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::MaxTokens,
            reasoning_effort: ReasoningEffortPolicy::Drop,
            developer_role: DeveloperRole::System,
            include_stream_usage: true,
        }
    }
}

/// A model listed statically in `codeox.toml`, or capability overrides for a
/// discovered model with the same upstream ID.
#[derive(Clone, Debug, Default)]
pub struct ModelEntry {
    pub id: String,
    pub alias: Option<String>,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub input_modalities: Option<Vec<String>>,
    pub reasoning_efforts: Option<Vec<String>>,
    pub default_reasoning_effort: Option<String>,
    pub reasoning_mandatory: Option<bool>,
}

#[derive(Debug)]
pub struct Vendor {
    pub key: String,
    pub name: String,
    pub base_url: Url,
    pub models_url: Url,
    pub env_key: Option<String>,
    pub wire: Wire,
    pub discovery: bool,
    pub models: Vec<ModelEntry>,
    /// OpenRouter's per-request provider routing preferences
    /// (`[providers.<name>.provider]`). Rejected for other providers so a typo
    /// cannot silently do nothing on an endpoint that has no such field.
    pub openrouter_provider: Option<serde_json::Value>,
    /// Anthropic Messages request ceiling. Codex does not send
    /// `max_output_tokens`, but Anthropic requires `max_tokens`.
    pub max_output_tokens: Option<u64>,
    pub anthropic: AnthropicOptions,
    pub chat: ChatOptions,
}

impl Vendor {
    /// Anthropic Messages requires an output budget even when the client omits it.
    pub(crate) fn messages_output_default(&self) -> u64 {
        self.max_output_tokens.unwrap_or(8192)
    }

    /// Resolves this vendor's API key, or `None` when it is not configured. A
    /// vendor without credentials stays listed, but requests for it fail with a
    /// safe error instead of using another vendor's key.
    pub fn api_key(&self, env: &Env<'_>) -> Option<SecretKey> {
        let name = self.env_key.as_deref()?;
        let key = env(name)?;
        let key = key.trim();
        if !usable_key(key) {
            return None;
        }
        Some(SecretKey::new(key.to_owned()))
    }

    /// Build authentication for the selected upstream. The documented hosted
    /// Messages gateways use bearer auth; other Messages providers use x-api-key.
    pub fn auth_headers(&self, key: &SecretKey) -> Result<HeaderMap, RelayError> {
        let mut headers = HeaderMap::new();
        let bearer = self.wire != Wire::AnthropicMessages || self.hosted_messages_uses_bearer();
        let (name, mut value) = if bearer {
            (
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", key.expose()))?,
            )
        } else {
            (
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(key.expose())?,
            )
        };
        value.set_sensitive(true);
        headers.insert(name, value);
        if self.is_openrouter() {
            // Optional attribution for app leaderboards, documented in
            // OpenRouter's quickstart.
            headers.insert(
                HeaderName::from_static("http-referer"),
                HeaderValue::from_static("https://github.com/oxstack/ox"),
            );
            headers.insert(
                HeaderName::from_static("x-openrouter-title"),
                HeaderValue::from_static("OX"),
            );
        }
        if self.wire == Wire::AnthropicMessages {
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_str(&self.anthropic.version)?,
            );
        }
        Ok(headers)
    }

    /// Applies OpenRouter's request extensions that the stock client does not
    /// know about: provider routing preferences and a sticky session id.
    pub(crate) fn decorate_openrouter(&self, body: &mut serde_json::Value, session: Option<&str>) {
        if !self.is_openrouter() {
            return;
        }
        let Some(body) = body.as_object_mut() else {
            return;
        };
        if let Some(preferences) = &self.openrouter_provider
            && !body.contains_key("provider")
        {
            body.insert("provider".to_owned(), preferences.clone());
        }
        if let Some(session) = session
            && !body.contains_key("session_id")
        {
            body.insert("session_id".to_owned(), serde_json::json!(session));
        }
    }

    /// Applies every OpenRouter request rule OX knows: routing preferences, a
    /// sticky session, and the stateless wire contract. Returns true when a
    /// storage field had to be removed or corrected.
    pub(crate) fn prepare_openrouter_request(
        &self,
        body: &mut serde_json::Value,
        session: Option<&str>,
    ) -> bool {
        if !self.is_openrouter() {
            return false;
        }
        self.decorate_openrouter(body, session);
        let Some(object) = body.as_object_mut() else {
            return false;
        };
        // A non-null reference is the only link to earlier turns. Keep it so
        // the provider rejects the request instead of answering from partial
        // context; the caller must resend the full history.
        let removed = match object.get("previous_response_id") {
            Some(serde_json::Value::Null) => object.remove("previous_response_id").is_some(),
            _ => false,
        };
        let corrected = match object.get("store") {
            None | Some(serde_json::Value::Bool(false)) => false,
            Some(_) => {
                object.insert("store".to_owned(), serde_json::json!(false));
                true
            }
        };
        removed || corrected
    }

    /// A stable, non-reversible session key for OpenRouter sticky routing.
    ///
    /// OpenRouter documents `session_id` as the strongest cache pin: every turn
    /// of one conversation reaches the same backend endpoint. The raw client
    /// id never leaves OX; the HMAC is scoped to the provider key, so two
    /// workspaces with the same thread id do not share a session.
    pub(crate) fn openrouter_session_id(
        &self,
        key: &SecretKey,
        incoming: Option<&HeaderMap>,
        payload: &serde_json::Value,
    ) -> Option<String> {
        if !self.is_openrouter() {
            return None;
        }
        let session: std::borrow::Cow<'_, str> = match incoming
            .and_then(openrouter_session_hint)
            .or_else(|| {
                payload
                    .get("prompt_cache_key")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| hint_ok(value))
            })
            .or_else(|| {
                payload
                    .pointer("/client_metadata/session_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| hint_ok(value))
            }) {
            Some(session) => session.into(),
            // A Claude Messages request without a session header names its
            // session only inside the JSON-encoded `metadata.user_id`. The
            // Chat Completions translation drops `prompt_cache_key`, so this
            // is the only sticky-routing hint that route gets.
            None => payload.get("metadata").and_then(claude_session_id)?.into(),
        };
        let hmac = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key.expose().as_bytes());
        let mut context = ring::hmac::Context::with_key(&hmac);
        context.update(b"ox-openrouter-session-v1\0");
        context.update(self.key.as_bytes());
        context.update(b"\0");
        context.update(session.as_bytes());
        Some(format!("ox_{}", hex::encode(context.sign().as_ref())))
    }

    pub(crate) fn discovery_headers(&self, key: &SecretKey) -> Result<HeaderMap, RelayError> {
        if self.wire == Wire::AnthropicMessages
            && alibaba_messages_catalog(&self.base_url).as_ref() == Some(&self.models_url)
        {
            let mut token = bearer(key.expose())?;
            token.set_sensitive(true);
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, token);
            return Ok(headers);
        }
        self.auth_headers(key)
    }

    fn hosted_messages_uses_bearer(&self) -> bool {
        self.base_url.scheme() == "https"
            && self.base_url.port_or_known_default() == Some(443)
            && (is_openrouter_url(&self.base_url)
                || matches!(
                    (
                        self.base_url.host_str(),
                        self.base_url.path().trim_end_matches('/')
                    ),
                    (
                        Some("opencode.ai"),
                        "/inference/anthropic/v1" | "/inference/go/anthropic/v1"
                    )
                ))
    }

    /// Asks the provider to cache the prompt, where it only does so on
    /// request and the request has not already asked.
    ///
    /// OpenRouter caches OpenAI, DeepSeek, Grok and similar models by itself,
    /// but Claude models only when the request carries a top-level
    /// `cache_control`. Neither Codex nor the translators send one, so
    /// without it every turn of a Claude model through OpenRouter is billed
    /// and processed in full. The field is part of OpenRouter's Responses
    /// and Chat Completions request schema. On the Messages wire the
    /// conversation carries Anthropic's own block markers instead.
    pub(crate) fn add_cache_hints(&self, body: &mut serde_json::Value) {
        if self.wire == Wire::AnthropicMessages || !self.is_openrouter() {
            return;
        }
        if let Some(body) = body.as_object_mut()
            && !body.contains_key("cache_control")
        {
            body.insert(
                "cache_control".to_owned(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }
    }

    pub(crate) fn is_openrouter(&self) -> bool {
        is_openrouter_url(&self.base_url)
    }

    /// The in-region host this route uses, if it is an OpenRouter one.
    pub(crate) fn openrouter_region(&self) -> Option<Region> {
        if !self.is_openrouter() {
            return None;
        }
        match self.base_url.host_str() {
            Some("eu.openrouter.ai") => Some(Region::Eu),
            Some("us.openrouter.ai") => Some(Region::Us),
            _ => Some(Region::Global),
        }
    }

    pub(crate) fn is_direct_anthropic(&self) -> bool {
        self.wire == Wire::AnthropicMessages
            && self.base_url.scheme() == "https"
            && self.base_url.host_str() == Some("api.anthropic.com")
            && self.base_url.port_or_known_default() == Some(443)
    }

    /// Attach an origin-scoped routing identifier for OpenCode Go. Derive it
    /// from the client's session ID without sending the raw ID upstream.
    pub(crate) fn inference_headers(
        &self,
        key: &SecretKey,
        incoming: Option<&HeaderMap>,
    ) -> Result<HeaderMap, RelayError> {
        let mut headers = self.auth_headers(key)?;
        if self.base_url.scheme() == "https"
            && self.base_url.host_str() == Some("opencode.ai")
            && self.base_url.port_or_known_default() == Some(443)
            && self.base_url.path().starts_with("/inference/go/")
        {
            let session = incoming.and_then(opencode_session_hint);
            let mut id = [0_u8; 16];
            if let Some(session) = session {
                let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key.expose().as_bytes());
                let mut context = ring::hmac::Context::with_key(&key);
                context.update(b"ox-opencode-session-v1\0");
                context.update(self.key.as_bytes());
                context.update(b"\0");
                context.update(session.as_bytes());
                let digest = context.sign();
                id.copy_from_slice(&digest.as_ref()[..16]);
            } else {
                getrandom::fill(&mut id)
                    .map_err(|_| "could not generate an OpenCode session ID")?;
            }
            headers.insert(
                "x-opencode-session",
                HeaderValue::from_str(&format!("ses_{}", hex::encode(id)))?,
            );
        }
        Ok(headers)
    }

    /// The one endpoint Codex requests are translated to for this vendor.
    pub fn endpoint(&self) -> Url {
        let suffix = match self.wire {
            Wire::Responses => "",
            Wire::AnthropicMessages => "/messages",
            Wire::ChatCompletions => "/chat/completions",
        };
        let mut url = self.base_url.clone();
        url.set_path(&format!(
            "{}{}",
            self.base_url.path().trim_end_matches('/'),
            suffix
        ));
        url.set_query(None);
        url
    }

    pub fn unavailable_reason(&self) -> String {
        match &self.env_key {
            Some(name) => format!("set {name} to enable this provider"),
            None => "no env_key is configured".to_owned(),
        }
    }

    /// Like [`unavailable_reason`](Self::unavailable_reason), but says so when the
    /// variable is already set and only its value was rejected.
    pub fn unavailable_reason_with(&self, env: &Env<'_>) -> String {
        match &self.env_key {
            Some(name) if env(name).is_some() => format!(
                "{name} is set but its value is not a usable key; remove spaces or line breaks from it"
            ),
            _ => self.unavailable_reason(),
        }
    }

    pub fn responses_url(&self, path: &str, query: Option<&str>) -> Url {
        let mut url = self.base_url.clone();
        url.set_path(&format!(
            "{}{}",
            self.base_url.path().trim_end_matches('/'),
            path
        ));
        url.set_query(query);
        url
    }
}

fn opencode_session_hint(headers: &HeaderMap) -> Option<&str> {
    [
        "x-opencode-session",
        "session_id",
        "x-session-id",
        "x-claude-code-session-id",
    ]
    .iter()
    .filter_map(|name| headers.get(*name)?.to_str().ok())
    .find(|id| hint_ok(id))
}

fn openrouter_session_hint(headers: &HeaderMap) -> Option<&str> {
    opencode_session_hint(headers)
}

/// Claude Code's session ID from Messages `metadata`, when it names a valid one.
pub(crate) fn claude_session_id(metadata: &serde_json::Value) -> Option<String> {
    let encoded = metadata.get("user_id")?.as_str()?;
    let user: serde_json::Value = serde_json::from_str(encoded).ok()?;
    let session = user.get("session_id")?.as_str()?;
    let valid = (1..=64).contains(&session.len())
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    valid.then(|| session.to_owned())
}

fn hint_ok(id: &str) -> bool {
    matches!(id.len(), 1..=128)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// OpenRouter serves the same API on its global and in-region hosts.
pub(crate) fn is_openrouter_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url
            .host_str()
            .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
        && url.path().trim_end_matches('/').ends_with("/api/v1")
}

/// Which OpenRouter data boundary a route uses. In-region routing is chosen
/// per request by hostname and cannot be switched silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Global,
    Eu,
    Us,
}

impl Region {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Eu => "EU in-region",
            Self::Us => "US in-region",
        }
    }
}

/// OpenRouter's guardrail- and key-filtered catalog. The public `/models` list
/// ignores the account, so a restricted key is shown models it cannot call.
pub(crate) fn openrouter_models_url(base: &Url) -> Url {
    let mut url = base.clone();
    url.set_path(&format!(
        "{}/models/user",
        base.path().trim_end_matches('/')
    ));
    url
}

/// Default environment variable holding the Brave Search subscription token.
/// The name matches Oxa's existing convention, so an existing environment works
/// without extra configuration.
pub const DEFAULT_SEARCH_ENV_KEY: &str = "BRAVE_SEARCH_API_KEY";
pub const DEFAULT_SEARCH_MAX_RESULTS: u32 = 8;
pub const DEFAULT_SEARCH_TIMEOUT_SECONDS: u64 = 30;
const MAX_SEARCH_RESULTS: u32 = 20;
const MAX_SEARCH_TIMEOUT_SECONDS: u64 = 120;

/// Opt-in live Brave Search for Codex's standalone `web.run` tool.
///
/// This is the top-level `[search]` table in `codeox.toml`. It stores only an
/// environment *reference*: the subscription token itself is read at launch,
/// never written to Codex config, and stripped from every Codex child process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchSettings {
    /// Environment variable holding the Brave subscription token.
    pub env_key: String,
    /// Results requested from Brave per query (1..=20).
    pub max_results: u32,
    /// Whole-request Brave timeout in seconds (1..=120).
    pub timeout_seconds: u64,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            env_key: DEFAULT_SEARCH_ENV_KEY.to_owned(),
            max_results: DEFAULT_SEARCH_MAX_RESULTS,
            timeout_seconds: DEFAULT_SEARCH_TIMEOUT_SECONDS,
        }
    }
}

#[derive(Debug)]
pub struct CodeoxConfig {
    pub vendors: Vec<Vendor>,
    /// Present only when `codeox.toml` opts in with a `[search]` table.
    pub search: Option<SearchSettings>,
    /// `[workers] host`: whether a terminal launch hands workers to a running session host.
    pub workers_host: WorkersHost,
    /// `[workers] yolo_grant_projects`: absolute project directories whose
    /// sessions grant YOLO to their workers and handoffs by default.
    pub yolo_grant_projects: Vec<std::path::PathBuf>,
}

/// Where workers started from a terminal `ox` launch run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkersHost {
    /// Use the OX session host when one is running, otherwise run in this launch.
    #[default]
    Auto,
    /// Always run workers inside the launching gateway, as without a host.
    Never,
}

impl CodeoxConfig {
    pub fn path(codex_home: &Path) -> PathBuf {
        codex_home.join(CONFIG_FILE)
    }

    pub fn load(path: &Path) -> Result<Self, RelayError> {
        let metadata = std::fs::metadata(path).map_err(|_| "could not read codeox.toml")?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err("codeox.toml is too large".into());
        }
        let raw = std::fs::read_to_string(path).map_err(|_| "could not read codeox.toml")?;
        Self::load_from_str(&raw)
    }

    pub fn load_from_str(raw: &str) -> Result<Self, RelayError> {
        let document = Document::parse(raw).map_err(|_| "could not parse codeox.toml")?;
        let providers = document
            .get("providers")
            .ok_or("codeox.toml needs a [providers.<name>] table")?;
        let table = providers
            .as_table()
            .ok_or("`providers` must be a table of provider configurations")?;
        let mut vendors = Vec::new();
        for (key, item) in table.iter() {
            let vendor = parse_vendor(key, item)?;
            for model in &vendor.models {
                if let Some(alias) = model.alias.as_deref()
                    && let Some((prefix, _)) = alias.split_once('/')
                    && prefix != key
                    && table.contains_key(prefix)
                {
                    return Err(format!(
                        "model alias {alias:?} for provider {key:?} uses the reserved prefix of provider {prefix:?}"
                    ).into());
                }
            }
            vendors.push(vendor);
        }
        if vendors.is_empty() {
            return Err("codeox.toml must configure at least one provider".into());
        }
        vendors.sort_by(|left, right| left.key.cmp(&right.key));
        let search = match document.get("search") {
            Some(item) => parse_search(item)?,
            None => None,
        };
        let (workers_host, yolo_grant_projects) = match document.get("workers") {
            Some(item) => parse_workers(item)?,
            None => (WorkersHost::Auto, Vec::new()),
        };
        Ok(Self {
            vendors,
            search,
            workers_host,
            yolo_grant_projects,
        })
    }

    /// Environment variable names that must never reach the Codex child.
    pub fn env_refs(&self) -> BTreeSet<String> {
        let mut refs: BTreeSet<String> = self
            .vendors
            .iter()
            .filter_map(|vendor| vendor.env_key.clone())
            .collect();
        if let Some(search) = &self.search {
            refs.insert(search.env_key.clone());
        }
        refs
    }
}

fn parse_workers(item: &Item) -> Result<(WorkersHost, Vec<std::path::PathBuf>), RelayError> {
    let table = item
        .as_table()
        .ok_or("`workers` must be a table of worker settings")?;
    crate::worker_placement::result_settings(Some(item), crate::worker_placement::RESULT_DEFAULTS)?;
    for key in table.iter().map(|(key, _)| key) {
        if !matches!(
            key,
            "host" | "yolo_grant_projects" | "results" | "result_max_age_days" | "placement"
        ) {
            return Err(format!("workers.{key} is not a recognized setting").into());
        }
    }
    let host = match opt_str(table, "host")?.as_deref() {
        None | Some("auto") => WorkersHost::Auto,
        Some("never") => WorkersHost::Never,
        Some(_) => return Err("workers.host must be \"auto\" or \"never\"".into()),
    };
    let projects = match table.get("yolo_grant_projects") {
        None => Vec::new(),
        Some(item) => item
            .as_array()
            .ok_or("workers.yolo_grant_projects must be a list of absolute project directories")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .ok_or_else(|| {
                        "workers.yolo_grant_projects must be a list of absolute project directories"
                            .into()
                    })
            })
            .collect::<Result<_, RelayError>>()?,
    };
    Ok((host, projects))
}

fn parse_search(item: &Item) -> Result<Option<SearchSettings>, RelayError> {
    let table = item
        .as_table()
        .ok_or("`search` must be a table of Brave Search settings")?;
    for key in table.iter().map(|(key, _)| key) {
        if !matches!(
            key,
            "enabled" | "env_key" | "max_results" | "timeout_seconds"
        ) {
            return Err(format!("search.{key} is not a recognized setting").into());
        }
    }
    if opt_bool(table, "enabled")? == Some(false) {
        return Ok(None);
    }
    let mut settings = SearchSettings::default();
    if let Some(name) = opt_str(table, "env_key")? {
        if let Some(reason) = credential_env_error(&name) {
            return Err(format!("search.env_key: {reason}").into());
        }
        settings.env_key = name;
    }
    if let Some(results) = opt_integer(table, "max_results")? {
        if !(1..=i64::from(MAX_SEARCH_RESULTS)).contains(&results) {
            return Err(
                format!("search.max_results must be between 1 and {MAX_SEARCH_RESULTS}").into(),
            );
        }
        settings.max_results = results as u32;
    }
    if let Some(seconds) = opt_integer(table, "timeout_seconds")? {
        if !(1..=MAX_SEARCH_TIMEOUT_SECONDS as i64).contains(&seconds) {
            return Err(format!(
                "search.timeout_seconds must be between 1 and {MAX_SEARCH_TIMEOUT_SECONDS}"
            )
            .into());
        }
        settings.timeout_seconds = seconds as u64;
    }
    Ok(Some(settings))
}

fn parse_vendor(key: &str, item: &Item) -> Result<Vendor, RelayError> {
    if !valid_provider_key(key) {
        return Err(format!(
            "provider name {key:?} must be lowercase ASCII letters, digits, `_` or `-`"
        )
        .into());
    }
    let table = item
        .as_table()
        .ok_or_else(|| format!("providers.{key} must be a table"))?;
    let name = opt_str(table, "name")?.unwrap_or_else(|| key.to_owned());
    if name.trim().is_empty() {
        return Err(format!("providers.{key}.name must not be empty").into());
    }
    let base_raw =
        opt_str(table, "base_url")?.ok_or_else(|| format!("providers.{key} needs a base_url"))?;
    let mut base_url = parse_provider_url(&base_raw)?;
    if base_url.query().is_some() {
        return Err(format!("providers.{key}.base_url must not contain a query string").into());
    }
    if base_url.fragment().is_some() {
        return Err(format!("providers.{key}.base_url must not contain a fragment").into());
    }
    let wire = Wire::parse(opt_str(table, "wire_api")?.as_deref())?;
    let companion_catalog = (wire == Wire::AnthropicMessages)
        .then(|| alibaba_messages_catalog(&base_url))
        .flatten();
    if companion_catalog.is_some() && base_url.path().trim_end_matches('/') == "/apps/anthropic" {
        base_url.set_path("/apps/anthropic/v1");
    }
    let discovery = opt_bool(table, "discovery")?.unwrap_or(true);
    let models_url = match opt_str(table, "models_url")? {
        Some(raw) => {
            let url = parse_provider_url(&raw)?;
            if url.origin() != base_url.origin() {
                return Err(format!(
                    "providers.{key}.models_url must share the origin of base_url"
                )
                .into());
            }
            url
        }
        None => companion_catalog.unwrap_or_else(|| {
            if is_openrouter_url(&base_url) {
                openrouter_models_url(&base_url)
            } else {
                default_models_url(&base_url)
            }
        }),
    };
    let openrouter_provider = parse_openrouter_provider(key, table)?;
    if openrouter_provider.is_some() && !is_openrouter_url(&base_url) {
        return Err(format!(
            "providers.{key}.provider is an OpenRouter routing setting; remove it or point base_url at an openrouter.ai host"
        )
        .into());
    }
    let env_key = match opt_str(table, "env_key")? {
        Some(name) => {
            if let Some(reason) = credential_env_error(&name) {
                return Err(format!("providers.{key}.env_key: {reason}").into());
            }
            Some(name)
        }
        None => None,
    };
    let models = match table.get("models") {
        Some(item) => parse_models(key, item)?,
        None => Vec::new(),
    };
    let max_output_tokens = match opt_integer(table, "max_output_tokens")? {
        Some(tokens) if tokens > 0 => Some(tokens as u64),
        Some(_) => {
            return Err(
                format!("providers.{key}.max_output_tokens must be a positive integer").into(),
            );
        }
        None => None,
    };

    let anthropic = parse_anthropic_options(key, table, wire)?;
    let chat = parse_chat_options(key, table, wire)?;

    Ok(Vendor {
        key: key.to_owned(),
        name,
        base_url,
        models_url,
        env_key,
        wire,
        discovery,
        models,
        openrouter_provider,
        max_output_tokens,
        anthropic,
        chat,
    })
}

fn parse_anthropic_options(
    key: &str,
    table: &dyn TableLike,
    wire: Wire,
) -> Result<AnthropicOptions, RelayError> {
    let anthropic_keys = [
        "anthropic_version",
        "anthropic_thinking",
        "anthropic_thinking_budget_tokens",
        "output_config_effort",
    ];
    if wire != Wire::AnthropicMessages
        && anthropic_keys.iter().any(|name| table.get(name).is_some())
    {
        return Err(format!(
            "providers.{key} sets adapter options but wire_api is not \"anthropic_messages\""
        )
        .into());
    }
    let mut options = AnthropicOptions::default();
    if let Some(version) = opt_str(table, "anthropic_version")? {
        if !valid_api_version(&version) {
            return Err(
                format!("providers.{key}.anthropic_version must look like YYYY-MM-DD").into(),
            );
        }
        options.version = version;
        options.version_pinned = true;
    }
    options.thinking = ThinkingMode::parse(opt_str(table, "anthropic_thinking")?.as_deref())?;
    if let Some(budget) = opt_integer(table, "anthropic_thinking_budget_tokens")? {
        if budget <= 0 {
            return Err(format!(
                "providers.{key}.anthropic_thinking_budget_tokens must be a positive integer"
            )
            .into());
        }
        options.thinking_budget_tokens = budget as u64;
    }
    if let Some(effort) = opt_bool(table, "output_config_effort")? {
        options.effort = effort;
    }
    if options.effort && options.thinking != ThinkingMode::Off {
        return Err(format!(
            "providers.{key} cannot combine output_config_effort with anthropic_thinking"
        )
        .into());
    }
    Ok(options)
}

fn parse_chat_options(
    key: &str,
    table: &dyn TableLike,
    wire: Wire,
) -> Result<ChatOptions, RelayError> {
    let chat_keys = [
        "max_tokens_field",
        "chat_reasoning_effort",
        "developer_role",
        "include_stream_usage",
    ];
    if wire != Wire::ChatCompletions && chat_keys.iter().any(|name| table.get(name).is_some()) {
        return Err(format!(
            "providers.{key} sets adapter options but wire_api is not \"chat_completions\""
        )
        .into());
    }
    let mut options = ChatOptions {
        max_tokens_field: MaxTokensField::parse(opt_str(table, "max_tokens_field")?.as_deref())?,
        reasoning_effort: ReasoningEffortPolicy::parse(
            opt_str(table, "chat_reasoning_effort")?.as_deref(),
        )?,
        developer_role: DeveloperRole::parse(opt_str(table, "developer_role")?.as_deref())?,
        include_stream_usage: true,
    };
    if let Some(include) = opt_bool(table, "include_stream_usage")? {
        options.include_stream_usage = include;
    }
    Ok(options)
}

pub(crate) fn valid_api_version(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn parse_models(provider_key: &str, item: &Item) -> Result<Vec<ModelEntry>, RelayError> {
    let tables = item.as_array_of_tables().ok_or_else(|| {
        format!("providers.{provider_key}.models must be an array of [[providers.{provider_key}.models]] tables")
    })?;
    let mut entries = Vec::new();
    for table in tables.iter() {
        let id = opt_str(table, "id")?
            .ok_or_else(|| format!("providers.{provider_key}.models entries need an id"))?;
        if !valid_model_id(&id) {
            return Err(
                format!("providers.{provider_key}.models has an invalid model id {id:?}").into(),
            );
        }
        let alias = match opt_str(table, "alias")? {
            Some(alias) => {
                if !valid_alias(&alias) {
                    return Err(format!(
                        "providers.{provider_key}.models entry {id:?} has an invalid alias {alias:?}"
                    )
                    .into());
                }
                Some(alias)
            }
            None => None,
        };
        let name = opt_str(table, "name")?;
        if name.as_deref().is_some_and(|name| name.trim().is_empty()) {
            return Err(
                format!("providers.{provider_key}.models entry {id:?} has an empty name").into(),
            );
        }
        let context_window = match opt_integer(table, "context_window")? {
            Some(limit) if limit > 0 => Some(limit as u64),
            Some(_) => {
                return Err(format!(
                    "providers.{provider_key}.models entry {id:?} needs a positive context_window"
                )
                .into());
            }
            None => None,
        };
        let input_modalities = opt_strings(table, "input_modalities")?;
        if input_modalities.as_deref().is_some_and(|values| {
            values
                .iter()
                .any(|value| value != "text" && value != "image")
        }) {
            return Err(format!(
                "providers.{provider_key}.models entry {id:?} supports only \"text\" and \"image\" input modalities"
            )
            .into());
        }
        let reasoning_efforts = opt_strings(table, "reasoning_efforts")?;
        if let Some(efforts) = &reasoning_efforts
            && let Some(unknown) = efforts
                .iter()
                .find(|effort| crate::catalog::reasoning_rank(effort).is_none())
        {
            return Err(format!(
                "providers.{provider_key}.models entry {id:?} has an unknown reasoning effort {unknown:?}; use none, minimal, low, medium, high, xhigh or max"
            )
            .into());
        }
        let default_reasoning_effort = match opt_str(table, "default_reasoning_effort")? {
            Some(effort) => {
                if crate::catalog::reasoning_rank(&effort).is_none() {
                    return Err(format!(
                        "providers.{provider_key}.models entry {id:?} has an unknown default_reasoning_effort {effort:?}"
                    )
                    .into());
                }
                Some(effort)
            }
            None => None,
        };
        let reasoning_mandatory = opt_bool(table, "reasoning_mandatory")?;
        entries.push(ModelEntry {
            id,
            alias,
            name,
            context_window,
            input_modalities,
            reasoning_efforts,
            default_reasoning_effort,
            reasoning_mandatory,
        });
    }
    Ok(entries)
}

/// OpenRouter's request-level routing preferences. Only the documented keys are
/// accepted, with their documented types, so a typo fails at launch instead of
/// being ignored upstream.
fn parse_openrouter_provider(
    provider_key: &str,
    table: &dyn TableLike,
) -> Result<Option<serde_json::Value>, RelayError> {
    let Some(item) = table.get("provider") else {
        return Ok(None);
    };
    let path = format!("providers.{provider_key}.provider");
    let settings = item
        .as_table_like()
        .ok_or_else(|| format!("{path} must be a table"))?;
    let mut output = serde_json::Map::new();
    for (name, value) in settings.iter() {
        let entry = match name {
            "allow_fallbacks" | "zdr" | "require_parameters" | "enforce_distillable_text" => {
                serde_json::Value::Bool(
                    value
                        .as_bool()
                        .ok_or_else(|| format!("{path}.{name} must be true or false"))?,
                )
            }
            "sort" => {
                let sort = value
                    .as_str()
                    .ok_or_else(|| format!("{path}.sort must be a string"))?;
                if !matches!(sort, "price" | "throughput" | "latency") {
                    return Err(format!(
                        "{path}.sort must be \"price\", \"throughput\" or \"latency\""
                    )
                    .into());
                }
                serde_json::Value::String(sort.to_owned())
            }
            "data_collection" => {
                let policy = value
                    .as_str()
                    .ok_or_else(|| format!("{path}.data_collection must be a string"))?;
                if !matches!(policy, "allow" | "deny") {
                    return Err(
                        format!("{path}.data_collection must be \"allow\" or \"deny\"").into(),
                    );
                }
                serde_json::Value::String(policy.to_owned())
            }
            "order" | "only" | "ignore" => {
                let list = value
                    .as_array()
                    .ok_or_else(|| format!("{path}.{name} must be a list of provider slugs"))?;
                let mut slugs = Vec::new();
                for slug in list.iter() {
                    let slug = slug
                        .as_str()
                        .filter(|slug| {
                            !slug.is_empty()
                                && slug.len() <= 128
                                && slug.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'-' | b'_' | b'.' | b'/')
                                })
                        })
                        .ok_or_else(|| {
                            format!("{path}.{name} must contain provider slugs such as \"openai\"")
                        })?;
                    slugs.push(serde_json::Value::String(slug.to_owned()));
                }
                serde_json::Value::Array(slugs)
            }
            "max_price" => {
                let prices = value
                    .as_table_like()
                    .ok_or_else(|| format!("{path}.max_price must be a table"))?;
                let mut limits = serde_json::Map::new();
                for (kind, amount) in prices.iter() {
                    if !matches!(
                        kind,
                        "prompt" | "completion" | "request" | "image" | "audio"
                    ) {
                        return Err(format!(
                            "{path}.max_price.{kind} is not a recognized price kind"
                        )
                        .into());
                    }
                    let amount = amount
                        .as_float()
                        .or_else(|| amount.as_integer().map(|value| value as f64))
                        .filter(|amount| *amount >= 0.0)
                        .ok_or_else(|| {
                            format!("{path}.max_price.{kind} must be a non-negative number")
                        })?;
                    limits.insert(kind.to_owned(), serde_json::json!(amount));
                }
                serde_json::Value::Object(limits)
            }
            other => {
                return Err(format!(
                    "{path}.{other} is not a recognized OpenRouter routing setting"
                )
                .into());
            }
        };
        output.insert(name.to_owned(), entry);
    }
    Ok(Some(serde_json::Value::Object(output)))
}

/// Builds a stable alias. Explicit aliases win; otherwise the alias is
/// `<provider>/<upstream-model-id>`, which keeps Codex sessions resumable as
/// long as the provider key and model ID in `codeox.toml` do not change.
pub fn alias_for(
    vendor_key: &str,
    model_id: &str,
    explicit: Option<&str>,
) -> Result<String, RelayError> {
    if let Some(alias) = explicit {
        if !valid_alias(alias) {
            return Err(format!("invalid model alias {alias:?}").into());
        }
        return Ok(alias.to_owned());
    }
    if !valid_model_id(model_id) {
        return Err(format!("provider {vendor_key:?} has an invalid model id {model_id:?}").into());
    }
    let alias = format!("{vendor_key}/{model_id}");
    if alias.len() > MAX_ALIAS_BYTES {
        return Err(format!("provider {vendor_key:?} has an over-long model id").into());
    }
    Ok(alias)
}

/// One resolved route: which configured vendor receives the request, and which
/// upstream model ID its API expects.
#[derive(Clone, Debug)]
pub struct Route {
    pub vendor: usize,
    pub upstream_model: String,
    pub wire: Wire,
    pub max_output_tokens: Option<u64>,
    pub reasoning: ReasoningPolicy,
}

/// What a routed model accepts for reasoning, when the provider says so.
///
/// An empty `supported` list means "unknown": OX leaves the caller's choice
/// alone and lets the provider answer. A non-empty list is authoritative, and
/// `mandatory` means every way of disabling reasoning is rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReasoningPolicy {
    pub supported: Vec<String>,
    pub default: Option<String>,
    pub mandatory: bool,
}

impl Route {
    /// Keep both explicit requests and the adapter's default within the
    /// selected model's discovered output ceiling. Preserve smaller requests.
    pub fn bound_output(&self, payload: &mut serde_json::Value, field: &str, vendor: &Vendor) {
        let Some(limit) = self.max_output_tokens else {
            return;
        };
        let default = match self.wire {
            Wire::AnthropicMessages => Some(vendor.messages_output_default()),
            Wire::ChatCompletions => vendor.max_output_tokens,
            Wire::Responses => None,
        };
        let requested = match payload.get(field) {
            None | Some(serde_json::Value::Null) => default,
            Some(value) => value.as_u64(),
        };
        if requested.is_some_and(|requested| requested > limit) {
            payload[field] = limit.into();
        }
    }

    /// Removes a reasoning setting this model cannot accept.
    ///
    /// Codex sends `reasoning.effort: "none"` from its own global default long
    /// after the user changed models. Providers that treat reasoning as
    /// mandatory answer that with HTTP 400, so the value is replaced with the
    /// provider's default (or dropped) before the request leaves OX.
    ///
    /// Returns a short description of the change for the one-line notice.
    pub fn normalize_reasoning(&self, payload: &mut serde_json::Value) -> Option<String> {
        self.reasoning.normalize_responses(payload)
    }
}

impl ReasoningPolicy {
    /// Removes a reasoning setting a Responses request cannot use.
    pub fn normalize_responses(&self, payload: &mut serde_json::Value) -> Option<String> {
        let supported = self.supported.clone();
        let mandatory = self.mandatory;
        let reasoning = payload.get_mut("reasoning")?.as_object_mut()?;
        let requested = reasoning
            .get("effort")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let disabled = reasoning
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            == Some(false);
        if requested.is_none() && !disabled {
            return None;
        }
        let supports_none = supported.iter().any(|level| level == "none");
        let wants_off = requested.as_deref() == Some("none") || disabled;
        // Without a published effort list there is nothing to clamp, but an
        // explicit mandatory policy still forbids disabling reasoning.
        if !mandatory && supported.is_empty() {
            return None;
        }
        if wants_off && supports_none && !mandatory {
            return None;
        }
        if !wants_off {
            // An unknown list does not mean every explicit effort is invalid,
            // and a mandatory policy only forbids turning reasoning off.
            if supported.is_empty() {
                return None;
            }
            if let Some(effort) = requested.as_deref()
                && supported.iter().any(|level| level == effort)
            {
                return None;
            }
        }
        let replacement = self
            .default
            .clone()
            .filter(|default| supported.is_empty() || supported.contains(default))
            .or_else(|| {
                if supported.is_empty() {
                    return None;
                }
                crate::catalog::clamp_reasoning_effort(
                    requested.as_deref().unwrap_or("none"),
                    &supported,
                    None,
                )
            });
        if let Some(replacement) = replacement {
            if requested.as_deref() == Some(replacement.as_str()) && !disabled {
                return None;
            }
            reasoning.remove("enabled");
            reasoning.insert(
                "effort".to_owned(),
                serde_json::Value::String(replacement.clone()),
            );
            let notice = match requested.as_deref() {
                Some("none") | None => {
                    format!("reasoning cannot be disabled on this model; using {replacement:?}")
                }
                Some(previous) => {
                    format!("reasoning effort {previous:?} is not supported; using {replacement:?}")
                }
            };
            return Some(notice);
        }
        // A mandatory model with no effort list and no default: drop the block
        // and let the provider apply its own reasoning level.
        if !mandatory {
            return None;
        }
        payload.as_object_mut()?.remove("reasoning");
        Some("reasoning cannot be disabled on this model; using its default".to_owned())
    }

    /// Removes a reasoning setting a native Anthropic Messages request cannot
    /// use (`thinking` disabled, or an unsupported `output_config.effort`).
    pub fn normalize_anthropic(&self, payload: &mut serde_json::Value) -> Option<String> {
        let object = payload.as_object_mut()?;
        let mut notice = None;
        if object
            .get("thinking")
            .and_then(|thinking| thinking.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("disabled")
        {
            let supports_none = self.supported.iter().any(|level| level == "none");
            if self.mandatory || (!self.supported.is_empty() && !supports_none) {
                object.remove("thinking");
                notice = Some(
                    "reasoning cannot be disabled on this model; using its default".to_owned(),
                );
            }
        }
        let requested = object
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if let Some(requested) = requested {
            let wants_off = requested == "none";
            let unsupported = !self.supported.is_empty() && !self.supported.contains(&requested);
            if (wants_off && self.mandatory) || unsupported {
                let replacement = self
                    .default
                    .clone()
                    .filter(|default| self.supported.is_empty() || self.supported.contains(default))
                    .or_else(|| {
                        if self.supported.is_empty() {
                            return None;
                        }
                        crate::catalog::clamp_reasoning_effort(&requested, &self.supported, None)
                    });
                if let Some(output) = object
                    .get_mut("output_config")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    match replacement {
                        Some(replacement) => {
                            output.insert(
                                "effort".to_owned(),
                                serde_json::Value::String(replacement.clone()),
                            );
                            notice = Some(format!(
                                "reasoning effort is not supported; using {replacement:?}"
                            ));
                        }
                        None => {
                            // No known level to use: drop only the disabling
                            // effort and let the provider apply its default.
                            output.remove("effort");
                            if output.is_empty() {
                                object.remove("output_config");
                            }
                            notice = Some(
                                "reasoning cannot be disabled on this model; using its default"
                                    .to_owned(),
                            );
                        }
                    }
                }
            }
        }
        notice
    }
}

/// A static entry can make an already-discovered model mandatory after
/// discovery filtered its defaults, so the final policy is enforced here:
/// `none` is neither offered nor used as the default.
fn reasoning_policy(model: &ModelSpec) -> ReasoningPolicy {
    let mut supported = model.reasoning_efforts.clone();
    let mut default = model.default_reasoning_effort.clone();
    if model.reasoning_mandatory {
        supported.retain(|effort| effort != "none");
        if default.as_deref() == Some("none") {
            default = None;
        }
    }
    ReasoningPolicy {
        supported,
        default,
        mandatory: model.reasoning_mandatory,
    }
}

pub enum Resolution {
    Route(Route),
    Ambiguous(Vec<String>),
    Unknown,
}

/// Alias-to-vendor routing table. Deterministic for a given configuration.
#[derive(Debug)]
pub struct Routes {
    by_alias: BTreeMap<String, Route>,
    by_upstream: BTreeMap<String, Vec<String>>,
    prefix_vendors: BTreeMap<String, (usize, Wire)>,
}

impl Routes {
    /// An OX-owned process uses the exact model on its billing route.
    pub(crate) fn retain_exact(&mut self, alias: &str) {
        self.by_alias.retain(|key, _| key == alias);
        self.by_upstream.clear();
        self.prefix_vendors.clear();
    }

    pub fn build(vendors: &[Vendor], models: &[Vec<ModelSpec>]) -> Result<Self, RelayError> {
        let mut by_alias: BTreeMap<String, Route> = BTreeMap::new();
        let mut by_upstream: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut prefix_vendors = BTreeMap::new();
        for (index, vendor) in vendors.iter().enumerate() {
            prefix_vendors.insert(vendor.key.clone(), (index, vendor.wire));
            for model in models.get(index).map(Vec::as_slice).unwrap_or_default() {
                let alias = alias_for(&vendor.key, &model.id, model.alias.as_deref())?;
                let route = Route {
                    vendor: index,
                    upstream_model: model.id.clone(),
                    max_output_tokens: model.max_output_tokens,
                    wire: vendor.wire,
                    reasoning: reasoning_policy(model),
                };
                if let Some(previous) = by_alias.insert(alias.clone(), route) {
                    return Err(format!(
                        "model alias {alias:?} is used by both provider {:?} and provider {:?}; give one entry an explicit `alias`",
                        vendors[previous.vendor].key, vendor.key
                    )
                    .into());
                }
                by_upstream.entry(model.id.clone()).or_default().push(alias);
            }
        }
        Ok(Self {
            by_alias,
            by_upstream,
            prefix_vendors,
        })
    }

    /// Resolves a `model` value from a Responses request.
    ///
    /// Order: exact alias, then `<known-provider>/<upstream-id>` (which keeps
    /// sessions resumable even when discovery did not list the model), then an
    /// unambiguous bare upstream ID.
    pub fn resolve(&self, requested: &str) -> Resolution {
        if requested.is_empty() || requested.len() > MAX_ALIAS_BYTES || !valid_alias(requested) {
            return Resolution::Unknown;
        }
        if let Some(route) = self.by_alias.get(requested) {
            return Resolution::Route(route.clone());
        }
        if let Some((prefix, rest)) = requested.split_once('/')
            && valid_model_id(rest)
            && let Some((vendor, wire)) = self.prefix_vendors.get(prefix)
        {
            if let Some(route) = self
                .by_upstream
                .get(rest)
                .into_iter()
                .flatten()
                .map(|alias| &self.by_alias[alias])
                .find(|route| route.vendor == *vendor)
            {
                return Resolution::Route(route.clone());
            }
            return Resolution::Route(Route {
                vendor: *vendor,
                upstream_model: rest.to_owned(),
                max_output_tokens: None,
                wire: *wire,
                reasoning: ReasoningPolicy::default(),
            });
        }
        match self.by_upstream.get(requested) {
            Some(aliases) if aliases.len() == 1 => {
                Resolution::Route(self.by_alias[&aliases[0]].clone())
            }
            Some(aliases) => Resolution::Ambiguous(
                aliases
                    .iter()
                    .take(MAX_AMBIGUOUS_ALIASES)
                    .cloned()
                    .collect(),
            ),
            None => Resolution::Unknown,
        }
    }

    #[cfg(test)]
    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.by_alias.keys().map(String::as_str)
    }
}

/// Legacy single-provider configuration, read from `config.toml` exactly as
/// before multi-vendor support existed.
#[derive(Debug)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub env_key: String,
    pub upstream_auth: HeaderValue,
    pub models_url: Url,
    inference_url: String,
}

impl Provider {
    pub fn load(codex_home: &Path, env: &Env<'_>) -> Result<Self, RelayError> {
        Self::load_if_configured(codex_home, env)?
            .ok_or_else(|| "no custom provider selected in config.toml".into())
    }

    pub fn load_if_configured(
        codex_home: &Path,
        env: &Env<'_>,
    ) -> Result<Option<Self>, RelayError> {
        let raw = match std::fs::read_to_string(codex_home.join("config.toml")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Self::from_config(&raw, |name, scope| {
            if let Some(value) = env(name) {
                return Ok(Some(value));
            }
            Ok(SavedKeys::load(codex_home)?.scoped_value(name, scope))
        })
    }

    pub(crate) fn from_config(
        raw: &str,
        key_for: impl FnOnce(&str, &str) -> Result<Option<String>, RelayError>,
    ) -> Result<Option<Self>, RelayError> {
        let config = Document::parse(raw).map_err(|_| "could not parse Codex config.toml")?;
        let Some(id) = config.get("model_provider").and_then(Item::as_str) else {
            return Ok(None);
        };
        let Some(provider) = config
            .get("model_providers")
            .and_then(|table| table.get(id))
        else {
            return Ok(None);
        };
        if config.get("model_catalog_json").is_some() {
            return Err("remove model_catalog_json to enable live model discovery".into());
        }
        if id.is_empty()
            || !id
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
        {
            return Err(
                "set model_provider to a provider ID using letters, numbers, _ or -".into(),
            );
        }
        let setting = |name: &str| provider.get(name).and_then(Item::as_str);
        let raw_base = setting("base_url").ok_or("the selected provider needs a base_url")?;
        let base = parse_provider_url(raw_base)?;
        if base.query().is_some() {
            return Err("base_url must not contain a query string".into());
        }
        let inference_url = base.as_str().trim_end_matches('/').to_owned();
        let models_url = match setting("model_catalog_url") {
            Some(url) => {
                let url = parse_provider_url(url)?;
                if url.origin() != base.origin() {
                    return Err("model_catalog_url must have the same origin as base_url".into());
                }
                url
            }
            None => Url::parse(&format!("{inference_url}/models"))?,
        };
        let env_key = setting("env_key")
            .filter(|name| !name.is_empty())
            .ok_or("set env_key to the name of the provider API key environment variable")?;
        if let Some(reason) = credential_env_error(env_key) {
            return Err(reason.into());
        }
        let upstream_auth = key_for(env_key, base.as_str())?
            .and_then(bearer_from_key)
            .ok_or_else(|| format!("set a valid API key in {env_key}"))?;
        Ok(Some(Self {
            id: id.to_owned(),
            name: setting("name").unwrap_or(id).to_owned(),
            env_key: env_key.to_owned(),
            upstream_auth,
            models_url,
            inference_url,
        }))
    }

    pub fn responses_url(&self, path: &str, query: Option<&str>) -> Url {
        let mut url = Url::parse(&format!("{}{path}", self.inference_url))
            .expect("the configured base URL and the response path are valid");
        url.set_query(query);
        url
    }
}

fn bearer_from_key(key: String) -> Option<HeaderValue> {
    if key.is_empty() || key.chars().any(char::is_whitespace) {
        return None;
    }
    let mut value = bearer(&key).ok()?;
    value.set_sensitive(true);
    Some(value)
}

fn default_models_url(base: &Url) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}/models", base.path().trim_end_matches('/')));
    url
}

pub(crate) fn alibaba_chat_catalog_path(host: &str) -> Option<&'static str> {
    Some(match host {
        "coding-intl.dashscope.aliyuncs.com" => "/v1/models",
        "dashscope.aliyuncs.com"
        | "dashscope-intl.aliyuncs.com"
        | "dashscope-us.aliyuncs.com"
        | "cn-hongkong.dashscope.aliyuncs.com" => "/compatible-mode/v1/models",
        host if host.ends_with(".maas.aliyuncs.com") => "/compatible-mode/v1/models",
        _ => return None,
    })
}

/// The Anthropic-compatible path has no catalog. Use the OpenAI-compatible
/// catalog on the very same documented Model Studio origin and plan.
fn alibaba_messages_catalog(base: &Url) -> Option<Url> {
    if base.scheme() != "https"
        || base.port_or_known_default() != Some(443)
        || !matches!(
            base.path().trim_end_matches('/'),
            "/apps/anthropic" | "/apps/anthropic/v1"
        )
    {
        return None;
    }
    let path = alibaba_chat_catalog_path(base.host_str()?)?;
    let mut catalog = base.clone();
    catalog.set_path(path);
    Some(catalog)
}

fn parse_provider_url(raw: &str) -> Result<Url, RelayError> {
    let url = Url::parse(raw).map_err(|_| "expected an absolute provider URL")?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err("provider URLs must be HTTPS or HTTP on loopback".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("provider URLs cannot contain credentials or fragments".into());
    }
    Ok(url)
}

fn opt_str(table: &dyn TableLike, key: &str) -> Result<Option<String>, RelayError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("`{key}` must be a string").into()),
    }
}

fn opt_bool(table: &dyn TableLike, key: &str) -> Result<Option<bool>, RelayError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a boolean").into()),
    }
}

fn opt_integer(table: &dyn TableLike, key: &str) -> Result<Option<i64>, RelayError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => item
            .as_integer()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be an integer").into()),
    }
}

fn opt_strings(table: &dyn TableLike, key: &str) -> Result<Option<Vec<String>>, RelayError> {
    match table.get(key) {
        None => Ok(None),
        Some(item) => {
            let array = item
                .as_array()
                .ok_or_else(|| format!("`{key}` must be an array of strings"))?;
            let mut values = Vec::new();
            for value in array.iter() {
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("`{key}` must be an array of strings"))?;
                values.push(value.to_owned());
            }
            Ok(Some(values))
        }
    }
}

fn valid_provider_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_PROVIDER_KEY_BYTES
        && key
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_' || ch == b'-')
        && !key.starts_with(['-', '_'])
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && !id.starts_with('-')
        && id
            .chars()
            .all(|ch| !ch.is_control() && !ch.is_whitespace() && !matches!(ch, '"' | '\\'))
}

fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= MAX_ALIAS_BYTES
        && !alias.starts_with(['-', '/'])
        && alias
            .chars()
            .all(|ch| !ch.is_control() && !ch.is_whitespace() && !matches!(ch, '"' | '\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).unwrap();
            let path = std::env::temp_dir().join(format!("codeox-provider-{}", hex::encode(bytes)));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn spec(id: &str) -> ModelSpec {
        ModelSpec {
            id: id.to_owned(),
            alias: None,
            name: None,
            context_window: None,
            max_output_tokens: None,
            input_modalities: vec!["text".to_owned()],
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            reasoning_mandatory: false,
            endpoints: Vec::new(),
            supports_tools: None,
        }
    }

    fn openrouter_vendor(base: &str, wire: Wire) -> Vendor {
        let url = Url::parse(base).unwrap();
        Vendor {
            key: "openrouter".to_owned(),
            name: "OpenRouter".to_owned(),
            models_url: openrouter_models_url(&url),
            base_url: url,
            env_key: None,
            wire,
            discovery: false,
            models: Vec::new(),
            openrouter_provider: None,
            max_output_tokens: None,
            anthropic: AnthropicOptions::default(),
            chat: ChatOptions::default(),
        }
    }

    #[test]
    fn openrouter_hosts_include_the_in_region_domains() {
        for (url, region) in [
            ("https://openrouter.ai/api/v1", Region::Global),
            ("https://eu.openrouter.ai/api/v1", Region::Eu),
            ("https://us.openrouter.ai/api/v1", Region::Us),
        ] {
            assert!(is_openrouter_url(&Url::parse(url).unwrap()), "{url}");
            let vendor = openrouter_vendor(url, Wire::Responses);
            assert_eq!(vendor.openrouter_region(), Some(region), "{url}");
        }
        for url in [
            "https://openrouter.ai.example/api/v1",
            "https://openrouter.ai/other",
            "http://openrouter.ai/api/v1",
        ] {
            assert!(!is_openrouter_url(&Url::parse(url).unwrap()), "{url}");
        }
        assert_eq!(Region::Global.label(), "global");
        assert_eq!(Region::Eu.label(), "EU in-region");
        assert_eq!(Region::Us.label(), "US in-region");
    }

    #[test]
    fn openrouter_discovers_from_the_keyed_catalog() {
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.openrouter]
base_url = "https://eu.openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
"#,
        )
        .unwrap();
        assert_eq!(
            config.vendors[0].models_url.as_str(),
            "https://eu.openrouter.ai/api/v1/models/user"
        );
        // An explicit models_url is never rewritten.
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
models_url = "https://openrouter.ai/api/v1/models?scope=public"
"#,
        )
        .unwrap();
        assert_eq!(
            config.vendors[0].models_url.as_str(),
            "https://openrouter.ai/api/v1/models?scope=public"
        );
    }

    #[test]
    fn openrouter_provider_preferences_are_validated() {
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"

[providers.openrouter.provider]
sort = "throughput"
zdr = true
data_collection = "deny"
only = ["anthropic", "openai"]
max_price = { prompt = 1.5, completion = 8 }
"#,
        )
        .unwrap();
        let preferences = config.vendors[0]
            .openrouter_provider
            .as_ref()
            .expect("preferences");
        assert_eq!(preferences["sort"], "throughput");
        assert_eq!(preferences["zdr"], true);
        assert_eq!(
            preferences["only"],
            serde_json::json!(["anthropic", "openai"])
        );
        assert_eq!(preferences["max_price"]["completion"], 8.0);

        for (extra, expected) in [
            (
                "[providers.openrouter.provider]\nsoort = \"price\"\n",
                "soort",
            ),
            (
                "[providers.openrouter.provider]\nsort = \"fastest\"\n",
                "sort",
            ),
            (
                "[providers.openrouter.provider]\ndata_collection = \"maybe\"\n",
                "data_collection",
            ),
        ] {
            let error = CodeoxConfig::load_from_str(&format!(
                "[providers.openrouter]\nbase_url = \"https://openrouter.ai/api/v1\"\n{extra}"
            ))
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "{error}");
        }

        // The table is meaningless on another provider, so it is an error.
        let error = CodeoxConfig::load_from_str(
            r#"
[providers.alpha]
base_url = "https://alpha.example/v1"

[providers.alpha.provider]
sort = "price"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("OpenRouter"), "{error}");
    }

    #[test]
    fn a_claude_session_in_metadata_is_an_openrouter_session_hint() {
        let vendor = openrouter_vendor("https://openrouter.ai/api/v1", Wire::ChatCompletions);
        let key = SecretKey::new("sk-openrouter-test".to_owned());
        let user_id = serde_json::json!({"session_id": "claude-session-1"}).to_string();
        let claude = serde_json::json!({"metadata": {"user_id": user_id}});
        let session = vendor
            .openrouter_session_id(&key, Some(&HeaderMap::new()), &claude)
            .expect("Claude's metadata names the session");
        // The same conversation keyed through Codex's field lands on the same
        // backend, so the two paths agree on one sticky session.
        let codex = serde_json::json!({"prompt_cache_key": "claude-session-1"});
        assert_eq!(
            Some(session),
            vendor.openrouter_session_id(&key, None, &codex)
        );
        // Metadata without a valid session ID gives no hint.
        let bad = serde_json::json!({"metadata": {"user_id": "not json"}});
        assert_eq!(vendor.openrouter_session_id(&key, None, &bad), None);
    }

    #[test]
    fn openrouter_requests_get_preferences_a_session_and_the_stateless_contract() {
        let mut vendor = openrouter_vendor("https://eu.openrouter.ai/api/v1", Wire::Responses);
        vendor.openrouter_provider = Some(serde_json::json!({"sort": "price"}));
        let key = SecretKey::new("sk-openrouter-test".to_owned());
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("thread-123"));
        let payload = serde_json::json!({"prompt_cache_key": "thread-123"});
        let session = vendor
            .openrouter_session_id(&key, Some(&headers), &payload)
            .expect("a session");
        assert!(session.starts_with("ox_"), "{session}");
        assert_eq!(
            Some(session.clone()),
            vendor.openrouter_session_id(&key, Some(&headers), &payload)
        );
        let other = SecretKey::new("sk-other".to_owned());
        assert_ne!(
            Some(session.clone()),
            vendor.openrouter_session_id(&other, Some(&headers), &payload)
        );

        let mut body = serde_json::json!({
            "model": "gpt",
            "store": true,
            "previous_response_id": "resp_1",
            "input": [],
        });
        assert!(vendor.prepare_openrouter_request(&mut body, Some(&session)));
        assert_eq!(body["store"], false);
        // A real continuation keeps its only reference; OpenRouter rejects it
        // rather than letting OX answer from incomplete context.
        assert_eq!(body["previous_response_id"], "resp_1");
        assert_eq!(body["provider"]["sort"], "price");
        assert_eq!(body["session_id"], session);
        assert_eq!(body["input"], serde_json::json!([]));

        // A null reference is not context and can go.
        let mut body = serde_json::json!({"model": "gpt", "previous_response_id": null});
        assert!(vendor.prepare_openrouter_request(&mut body, None));
        assert!(body.get("previous_response_id").is_none());
    }

    #[test]
    fn openrouter_requests_carry_optional_attribution() {
        let vendor = openrouter_vendor("https://openrouter.ai/api/v1", Wire::Responses);
        let key = SecretKey::new("sk-openrouter-test".to_owned());
        let headers = vendor.auth_headers(&key).unwrap();
        assert_eq!(
            headers.get("http-referer").unwrap(),
            "https://github.com/oxstack/ox"
        );
        assert_eq!(headers.get("x-openrouter-title").unwrap(), "OX");

        let other = openrouter_vendor("https://alpha.example/v1", Wire::Responses);
        let headers = other.auth_headers(&key).unwrap();
        assert!(headers.get("http-referer").is_none());
    }

    fn reasoning_route(supported: &[&str], default: Option<&str>, mandatory: bool) -> Route {
        Route {
            vendor: 0,
            upstream_model: "gpt".to_owned(),
            wire: Wire::Responses,
            max_output_tokens: None,
            reasoning: ReasoningPolicy {
                supported: supported.iter().map(|level| (*level).to_owned()).collect(),
                default: default.map(str::to_owned),
                mandatory,
            },
        }
    }

    #[test]
    fn a_mandatory_model_never_keeps_effort_none() {
        let route = reasoning_route(&["low", "medium", "high"], Some("medium"), true);
        let mut payload = serde_json::json!({
            "model": "alpha/gpt",
            "reasoning": {"effort": "none", "summary": "auto"},
        });
        let notice = route.normalize_reasoning(&mut payload).expect("a notice");
        assert_eq!(payload["reasoning"]["effort"], "medium");
        assert_eq!(payload["reasoning"]["summary"], "auto");
        assert!(notice.contains("cannot be disabled"), "{notice}");

        // `enabled: false` is the other way a client asks for no reasoning.
        let mut payload = serde_json::json!({"reasoning": {"enabled": false}});
        assert!(route.normalize_reasoning(&mut payload).is_some());
        assert!(payload["reasoning"].get("enabled").is_none());
        assert_eq!(payload["reasoning"]["effort"], "medium");
    }

    #[test]
    fn a_mandatory_model_without_a_default_uses_the_lowest_level() {
        let route = reasoning_route(&["low", "high"], None, true);
        let mut payload = serde_json::json!({"reasoning": {"effort": "none"}});
        assert!(route.normalize_reasoning(&mut payload).is_some());
        // The closest supported level to "none" is the lowest one.
        assert_eq!(payload["reasoning"]["effort"], "low");
    }

    #[test]
    fn native_messages_reasoning_follows_the_same_policy() {
        let route = reasoning_route(&["low", "high"], Some("high"), true);
        let mut payload = serde_json::json!({"thinking": {"type": "disabled"}});
        assert!(route.reasoning.normalize_anthropic(&mut payload).is_some());
        assert!(payload.get("thinking").is_none());

        let mut payload = serde_json::json!({
            "output_config": {"effort": "none"},
            "messages": [],
        });
        assert!(route.reasoning.normalize_anthropic(&mut payload).is_some());
        assert_eq!(payload["output_config"]["effort"], "high");

        let mut payload = serde_json::json!({"output_config": {"effort": "low"}});
        assert!(route.reasoning.normalize_anthropic(&mut payload).is_none());
        assert_eq!(payload["output_config"]["effort"], "low");

        // A mandatory policy with no list still corrects a disabling effort.
        let route = reasoning_route(&[], Some("high"), true);
        let mut payload = serde_json::json!({"output_config": {"effort": "none"}});
        assert!(route.reasoning.normalize_anthropic(&mut payload).is_some());
        assert_eq!(payload["output_config"]["effort"], "high");

        let route = reasoning_route(&[], None, true);
        let mut payload = serde_json::json!({"output_config": {"effort": "none"}});
        assert!(route.reasoning.normalize_anthropic(&mut payload).is_some());
        assert!(payload.get("output_config").is_none(), "no level to use");

        // An unknown list and no policy leaves the caller's choice alone.
        let route = reasoning_route(&[], None, false);
        let mut payload = serde_json::json!({
            "thinking": {"type": "disabled"},
            "output_config": {"effort": "none"},
        });
        assert!(route.reasoning.normalize_anthropic(&mut payload).is_none());
        assert_eq!(payload["thinking"]["type"], "disabled");
    }

    #[test]
    fn an_override_that_makes_a_model_mandatory_drops_none_from_the_route() {
        let model = ModelSpec {
            reasoning_efforts: vec!["none".to_owned(), "low".to_owned(), "high".to_owned()],
            default_reasoning_effort: Some("none".to_owned()),
            reasoning_mandatory: true,
            ..spec("gpt")
        };
        let policy = reasoning_policy(&model);
        assert_eq!(policy.supported, ["low", "high"]);
        assert_eq!(policy.default, None);
        let route = Route {
            vendor: 0,
            upstream_model: "gpt".to_owned(),
            wire: Wire::Responses,
            max_output_tokens: None,
            reasoning: policy,
        };
        let mut payload = serde_json::json!({"reasoning": {"effort": "none"}});
        assert!(route.normalize_reasoning(&mut payload).is_some());
        assert_eq!(payload["reasoning"]["effort"], "low");
    }

    #[test]
    fn a_mandatory_policy_without_an_effort_list_still_enforces_it() {
        // A static entry can declare `reasoning_mandatory` without listing the
        // efforts; the policy, not the list, is what forbids disabling.
        let route = reasoning_route(&[], Some("high"), true);
        let mut payload = serde_json::json!({"reasoning": {"effort": "none"}});
        assert!(route.normalize_reasoning(&mut payload).is_some());
        assert_eq!(payload["reasoning"]["effort"], "high");

        let route = reasoning_route(&[], None, true);
        let mut payload = serde_json::json!({"reasoning": {"enabled": false}});
        assert!(route.normalize_reasoning(&mut payload).is_some());
        assert!(
            payload.get("reasoning").is_none(),
            "no level to use, so drop it"
        );

        // Without the policy an unknown model keeps the caller's value.
        let route = reasoning_route(&[], Some("high"), false);
        let mut payload = serde_json::json!({"reasoning": {"effort": "none"}});
        assert!(route.normalize_reasoning(&mut payload).is_none());
        assert_eq!(payload["reasoning"]["effort"], "none");
    }

    #[test]
    fn an_unsupported_effort_is_clamped_to_the_provider_default() {
        let route = reasoning_route(&["low", "medium", "high"], Some("medium"), false);
        let mut payload = serde_json::json!({"reasoning": {"effort": "max"}});
        assert!(route.normalize_reasoning(&mut payload).is_some());
        assert_eq!(payload["reasoning"]["effort"], "medium");
    }

    #[test]
    fn supported_choices_are_left_untouched() {
        let route = reasoning_route(&["none", "low", "high"], Some("low"), false);
        for effort in ["none", "low", "high"] {
            let mut payload = serde_json::json!({"reasoning": {"effort": effort}});
            assert!(
                route.normalize_reasoning(&mut payload).is_none(),
                "{effort}"
            );
        }
        // An unknown model keeps the caller's value; the provider decides.
        let route = reasoning_route(&[], None, false);
        let mut payload = serde_json::json!({"reasoning": {"effort": "none"}});
        assert!(route.normalize_reasoning(&mut payload).is_none());
    }

    #[test]
    fn static_entries_declare_reasoning_policy() {
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.alpha]
base_url = "https://alpha.example/v1"

[[providers.alpha.models]]
id = "gpt"
reasoning_efforts = ["low", "high"]
default_reasoning_effort = "high"
reasoning_mandatory = true
"#,
        )
        .unwrap();
        let model = &config.vendors[0].models[0];
        assert_eq!(model.default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(model.reasoning_mandatory, Some(true));

        let error = CodeoxConfig::load_from_str(
            r#"
[providers.alpha]
base_url = "https://alpha.example/v1"

[[providers.alpha.models]]
id = "gpt"
default_reasoning_effort = "extreme"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("default_reasoning_effort"), "{error}");
    }

    #[test]
    fn workers_host_defaults_to_auto_and_accepts_only_known_values() {
        let base = "[providers.alpha]\nname = \"Alpha\"\nbase_url = \"https://alpha.example/v1\"\nenv_key = \"ALPHA_KEY\"\n";
        let load = |extra: &str| CodeoxConfig::load_from_str(&format!("{base}{extra}"));
        assert_eq!(load("").unwrap().workers_host, WorkersHost::Auto);
        assert_eq!(
            load("[workers]\nhost = \"auto\"\n").unwrap().workers_host,
            WorkersHost::Auto
        );
        assert_eq!(
            load("[workers]\nhost = \"never\"\n").unwrap().workers_host,
            WorkersHost::Never
        );
        assert!(load("[workers]\nhost = \"always\"\n").is_err());
        assert_eq!(
            load("[workers]\nyolo_grant_projects = [\"/srv/app\"]\n")
                .unwrap()
                .yolo_grant_projects,
            vec![std::path::PathBuf::from("/srv/app")]
        );
        assert!(load("[workers]\nyolo_grant_projects = [\"relative\"]\n").is_err());
        assert!(load("[workers]\nyolo_grant_projects = true\n").is_err());
        assert!(load("[workers]\nhosted = \"never\"\n").is_err());
        assert!(CodeoxConfig::load_from_str(&format!("workers = \"never\"\n{base}")).is_err());
    }

    fn two_vendors(extra_alpha: &str, extra_beta: &str) -> CodeoxConfig {
        CodeoxConfig::load_from_str(&format!(
            r#"
[providers.alpha]
name = "Alpha"
base_url = "https://alpha.example/v1"
env_key = "ALPHA_KEY"
{extra_alpha}

[providers.beta]
name = "Beta"
base_url = "https://beta.example/v1"
env_key = "BETA_KEY"
{extra_beta}
"#
        ))
        .unwrap()
    }

    #[test]
    fn aliases_are_provider_qualified_and_stable() {
        let config = two_vendors("", "");
        let alpha = config
            .vendors
            .iter()
            .position(|v| v.key == "alpha")
            .unwrap();
        let beta = config.vendors.iter().position(|v| v.key == "beta").unwrap();
        let mut models = vec![Vec::new(); config.vendors.len()];
        models[alpha].push(spec("shared-model"));
        models[beta].push(spec("shared-model"));
        let routes = Routes::build(&config.vendors, &models).unwrap();
        let aliases = routes.aliases().collect::<Vec<_>>();
        assert_eq!(aliases, ["alpha/shared-model", "beta/shared-model"]);

        match routes.resolve("alpha/shared-model") {
            Resolution::Route(route) => {
                assert_eq!(route.vendor, alpha);
                assert_eq!(route.upstream_model, "shared-model");
            }
            _ => panic!("expected an exact alias route"),
        }
        match routes.resolve("beta/shared-model") {
            Resolution::Route(route) => assert_eq!(route.vendor, beta),
            _ => panic!("expected an exact alias route"),
        }
        match routes.resolve("shared-model") {
            Resolution::Ambiguous(aliases) => {
                assert_eq!(aliases, ["alpha/shared-model", "beta/shared-model"]);
            }
            _ => panic!("the same model id on two vendors must be ambiguous"),
        }
    }

    #[test]
    fn identical_explicit_aliases_are_rejected() {
        let config = two_vendors(
            "\n[[providers.alpha.models]]\nid = \"x\"\nalias = \"shared\"\n",
            "\n[[providers.beta.models]]\nid = \"x\"\nalias = \"shared\"\n",
        );
        let models = config
            .vendors
            .iter()
            .map(|vendor| crate::catalog::merge_models(None, &vendor.models))
            .collect::<Vec<_>>();
        let error = Routes::build(&config.vendors, &models)
            .unwrap_err()
            .to_string();
        assert!(error.contains("\"shared\""), "{error}");
        assert!(error.contains("\"alpha\""), "{error}");
        assert!(error.contains("\"beta\""), "{error}");
    }

    #[test]
    fn explicit_alias_keeps_resume_slug_stable() {
        let config = two_vendors(
            "\n[[providers.alpha.models]]\nid = \"new-upstream-name\"\nalias = \"alpha/stable\"\n",
            "",
        );
        let models = config
            .vendors
            .iter()
            .map(|vendor| crate::catalog::merge_models(None, &vendor.models))
            .collect::<Vec<_>>();
        let routes = Routes::build(&config.vendors, &models).unwrap();
        assert_eq!(routes.aliases().collect::<Vec<_>>(), ["alpha/stable"]);
        match routes.resolve("alpha/stable") {
            Resolution::Route(route) => assert_eq!(route.upstream_model, "new-upstream-name"),
            _ => panic!("expected explicit alias to route"),
        }
    }

    #[test]
    fn prefixed_aliases_route_models_missing_from_discovery() {
        let config = two_vendors("", "");
        let models = vec![Vec::new(); config.vendors.len()];
        let routes = Routes::build(&config.vendors, &models).unwrap();
        match routes.resolve("beta/never-listed") {
            Resolution::Route(route) => {
                assert_eq!(route.vendor, 1);
                assert_eq!(route.upstream_model, "never-listed");
            }
            _ => panic!("expected prefix routing to a configured provider"),
        }
        assert!(matches!(
            routes.resolve("gamma/anything"),
            Resolution::Unknown
        ));
        assert!(matches!(
            routes.resolve("unknown-model"),
            Resolution::Unknown
        ));
    }

    #[test]
    fn hosted_route_keeps_only_the_exact_alias() {
        let config = two_vendors("", "");
        let mut models = vec![Vec::new(); config.vendors.len()];
        models[0].push(spec("local"));
        models[1].push(spec("alternate"));
        let mut routes = Routes::build(&config.vendors, &models).unwrap();
        routes.retain_exact("alpha/local");
        assert_eq!(routes.aliases().collect::<Vec<_>>(), ["alpha/local"]);
        assert!(matches!(
            routes.resolve("alpha/local"),
            Resolution::Route(_)
        ));
        for other in ["local", "alpha/unlisted", "beta/alternate"] {
            assert!(matches!(routes.resolve(other), Resolution::Unknown));
        }
    }

    #[test]
    fn bare_upstream_ids_resolve_when_unique() {
        let config = two_vendors("", "");
        let alpha = config
            .vendors
            .iter()
            .position(|v| v.key == "alpha")
            .unwrap();
        let mut models = vec![Vec::new(); config.vendors.len()];
        models[alpha].push(spec("only-on-alpha"));
        let routes = Routes::build(&config.vendors, &models).unwrap();
        match routes.resolve("only-on-alpha") {
            Resolution::Route(route) => assert_eq!(route.vendor, alpha),
            _ => panic!("expected a unique bare id to route"),
        }
    }

    #[test]
    fn explicit_wire_formats_are_wired_and_aliases_are_rejected() {
        for (wire, expected) in [
            ("responses", Wire::Responses),
            ("anthropic_messages", Wire::AnthropicMessages),
            ("chat_completions", Wire::ChatCompletions),
        ] {
            let config = CodeoxConfig::load_from_str(&format!(
                "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"{wire}\"\n"
            ))
            .unwrap();
            assert_eq!(config.vendors[0].wire, expected);
            assert_eq!(config.vendors[0].wire.as_str(), wire);
        }
        for wire in ["anthropic", "chat", "messages"] {
            let error = CodeoxConfig::load_from_str(&format!(
                "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"{wire}\"\n"
            ))
            .unwrap_err()
            .to_string();
            assert!(error.contains("unknown wire_api"), "{error}");
        }
        let error = CodeoxConfig::load_from_str(
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"grpc\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown wire_api"), "{error}");
    }

    #[test]
    fn translated_wire_endpoints_and_auth_headers_are_scheme_specific() {
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.alpha]
base_url = "https://alpha.example/v1"
wire_api = "anthropic_messages"
env_key = "ALPHA_KEY"
anthropic_version = "2026-10-21"
anthropic_thinking = "manual"
anthropic_thinking_budget_tokens = 2048
max_output_tokens = 4096

[providers.beta]
base_url = "https://beta.example/v1"
wire_api = "chat_completions"
env_key = "BETA_KEY"
max_tokens_field = "max_completion_tokens"
chat_reasoning_effort = "forward"
developer_role = "developer"
include_stream_usage = false
"#,
        )
        .unwrap();
        let alpha = &config.vendors[0];
        assert_eq!(alpha.wire, Wire::AnthropicMessages);
        assert_eq!(
            alpha.endpoint().as_str(),
            "https://alpha.example/v1/messages"
        );
        assert_eq!(alpha.anthropic.version, "2026-10-21");
        assert_eq!(alpha.anthropic.thinking, ThinkingMode::Manual);
        assert_eq!(alpha.anthropic.thinking_budget_tokens, 2048);
        assert!(alpha.anthropic.rejects_sampling());
        assert_eq!(alpha.max_output_tokens, Some(4096));
        let key = alpha
            .api_key(&|_: &str| Some("anthropic-secret".to_owned()))
            .unwrap();
        let headers = alpha.auth_headers(&key).unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "anthropic-secret");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2026-10-21");
        assert!(headers.get(AUTHORIZATION).is_none());

        let beta = &config.vendors[1];
        assert_eq!(beta.wire, Wire::ChatCompletions);
        assert_eq!(
            beta.endpoint().as_str(),
            "https://beta.example/v1/chat/completions"
        );
        assert_eq!(
            beta.chat.max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        );
        assert_eq!(beta.chat.reasoning_effort, ReasoningEffortPolicy::Forward);
        assert_eq!(beta.chat.developer_role, DeveloperRole::Developer);
        assert!(!beta.chat.include_stream_usage);
        let key = beta
            .api_key(&|_: &str| Some("chat-secret".to_owned()))
            .unwrap();
        let headers = beta.auth_headers(&key).unwrap();
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer chat-secret");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn alibaba_messages_discovers_models_from_the_same_origin() {
        for (base_url, expected_path, companion) in [
            (
                "https://fixture.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
                "/compatible-mode/v1/models",
                true,
            ),
            (
                "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic/v1",
                "/compatible-mode/v1/models",
                true,
            ),
            (
                "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic/v1",
                "/v1/models",
                true,
            ),
            (
                "https://fake.maas.aliyuncs.com.example/apps/anthropic/v1",
                "/apps/anthropic/v1/models",
                false,
            ),
        ] {
            let config = CodeoxConfig::load_from_str(&format!(
                "[providers.fixture]\nwire_api = \"anthropic_messages\"\nbase_url = {base_url:?}\n"
            ))
            .unwrap();
            let vendor = &config.vendors[0];
            let key = SecretKey::new("fixture-key".into());
            assert_eq!(vendor.models_url.origin(), vendor.base_url.origin());
            assert_eq!(vendor.models_url.path(), expected_path);
            assert_eq!(vendor.endpoint().path(), "/apps/anthropic/v1/messages");
            assert_eq!(
                vendor
                    .discovery_headers(&key)
                    .unwrap()
                    .contains_key(AUTHORIZATION),
                companion
            );
            assert_eq!(
                vendor.auth_headers(&key).unwrap()["x-api-key"],
                "fixture-key"
            );
        }
    }

    #[test]
    fn openrouter_is_asked_to_cache_and_other_providers_are_left_alone() {
        let hinted = |base_url: &str, wire: &str, body: serde_json::Value| {
            let config = CodeoxConfig::load_from_str(&format!(
                "[providers.fixture]\nbase_url = {base_url:?}\nwire_api = {wire:?}\n"
            ))
            .unwrap();
            let mut body = body;
            config.vendors[0].add_cache_hints(&mut body);
            body
        };
        let ephemeral = serde_json::json!({"type": "ephemeral"});
        let plain = serde_json::json!({"model": "m"});
        for wire in ["responses", "chat_completions"] {
            let body = hinted("https://openrouter.ai/api/v1", wire, plain.clone());
            assert_eq!(body["cache_control"], ephemeral, "{wire}");
            // Any other host may reject a field it does not know.
            for base_url in [
                "https://api.openai.com/v1",
                "https://openrouter.ai.example/api/v1",
                "http://127.0.0.1:8080/v1",
            ] {
                assert_eq!(hinted(base_url, wire, plain.clone()), plain, "{base_url}");
            }
        }
        // Messages requests carry block markers, and a caller's own choice wins.
        let messages = hinted(
            "https://openrouter.ai/api/v1",
            "anthropic_messages",
            plain.clone(),
        );
        assert_eq!(messages, plain);
        let chosen = serde_json::json!({"cache_control": {"type": "ephemeral", "ttl": "1h"}});
        assert_eq!(
            hinted("https://openrouter.ai/api/v1", "responses", chosen.clone()),
            chosen
        );
    }

    #[test]
    fn hosted_messages_bearer_auth_is_scoped_to_documented_endpoints() {
        for (base_url, bearer) in [
            ("https://openrouter.ai/api/v1", true),
            ("https://opencode.ai/inference/go/anthropic/v1", true),
            ("https://api.anthropic.com/v1", false),
            ("https://openrouter.ai:8443/api/v1", false),
            ("https://openrouter.ai.example/api/v1", false),
        ] {
            let config = CodeoxConfig::load_from_str(&format!(
                "[providers.fixture]\nbase_url = {base_url:?}\nwire_api = \"anthropic_messages\"\n"
            ))
            .unwrap();
            let headers = config.vendors[0]
                .auth_headers(&SecretKey::new("fixture-key".into()))
                .unwrap();
            assert_eq!(headers.contains_key(AUTHORIZATION), bearer, "{base_url}");
            assert_eq!(headers.contains_key("x-api-key"), !bearer, "{base_url}");
            assert_eq!(headers["anthropic-version"], "2023-06-01");
        }
    }

    #[test]
    fn opencode_session_is_stable_and_scoped_to_the_go_origin() {
        let config = CodeoxConfig::load_from_str(
            r#"
[providers.go]
base_url = "https://opencode.ai/inference/go/openai/v1"

[providers.other]
base_url = "https://other.example/inference/go/openai/v1"
"#,
        )
        .unwrap();
        let go = &config.vendors[0];
        let key = SecretKey::new("vendor-credential-fixture".to_owned());
        let mut incoming = HeaderMap::new();
        incoming.insert("session_id", HeaderValue::from_static("private-thread-a"));
        let header = "x-opencode-session";
        let first = go.inference_headers(&key, Some(&incoming)).unwrap()[header].clone();
        assert!(first.to_str().unwrap().starts_with("ses_"));
        assert_eq!(first.as_bytes().len(), 36);
        assert_ne!(first, incoming["session_id"]);
        assert_eq!(
            first,
            go.inference_headers(&key, Some(&incoming)).unwrap()[header]
        );
        incoming.insert("session_id", HeaderValue::from_static("private-thread-b"));
        assert_ne!(
            first,
            go.inference_headers(&key, Some(&incoming)).unwrap()[header]
        );
        assert!(!go.auth_headers(&key).unwrap().contains_key(header));
        assert!(
            !config.vendors[1]
                .inference_headers(&key, Some(&incoming))
                .unwrap()
                .contains_key(header)
        );
    }

    #[test]
    fn adapter_options_are_rejected_on_the_wrong_wire() {
        for text in [
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"chat_completions\"\nanthropic_thinking = \"manual\"\n",
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"anthropic_messages\"\nmax_tokens_field = \"max_completion_tokens\"\n",
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"anthropic_messages\"\nanthropic_version = \"latest\"\n",
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nwire_api = \"anthropic_messages\"\nanthropic_thinking = \"manual\"\noutput_config_effort = true\n",
        ] {
            assert!(
                CodeoxConfig::load_from_str(text).is_err(),
                "accepted {text:?}"
            );
        }
    }

    #[test]
    fn provider_keys_must_be_lowercase() {
        let error = CodeoxConfig::load_from_str(
            "[providers.Alpha]\nbase_url = \"https://alpha.example/v1\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("lowercase ASCII"), "{error}");
    }

    #[test]
    fn cross_origin_catalog_and_insecure_urls_are_rejected() {
        let error = CodeoxConfig::load_from_str(
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nmodels_url = \"https://other.example/v1/models\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("share the origin"), "{error}");

        let error = CodeoxConfig::load_from_str(
            "[providers.alpha]\nbase_url = \"http://alpha.example/v1\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("HTTPS or HTTP on loopback"), "{error}");

        let error = CodeoxConfig::load_from_str(
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1?key=secret\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("query string"), "{error}");

        let error = CodeoxConfig::load_from_str(
            "[providers.alpha]\nbase_url = \"https://user:pass@alpha.example/v1\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("credentials"), "{error}");
    }

    #[test]
    fn relay_token_variable_is_reserved() {
        let error = CodeoxConfig::load_from_str(&format!(
            "[providers.alpha]\nbase_url = \"https://alpha.example/v1\"\nenv_key = \"{TOKEN_ENV}\"\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("reserved relay token"), "{error}");
    }

    #[test]
    fn whitespace_in_an_env_key_does_not_hide_the_saved_key() {
        let config = two_vendors("", "");
        let vendor = config.vendors.iter().find(|v| v.key == "alpha").unwrap();
        // Surrounding whitespace (a trailing space or newline) is trimmed away.
        let env = |name: &str| (name == "ALPHA_KEY").then(|| " fixture-key\n".to_owned());
        assert_eq!(vendor.api_key(&env).unwrap().expose(), "fixture-key");

        // A value that is still malformed after trimming is rejected, and the
        // reason says the variable is set rather than naming the wrong fix.
        let env = |name: &str| (name == "ALPHA_KEY").then(|| "fixture key".to_owned());
        assert!(vendor.api_key(&env).is_none());
        let reason = vendor.unavailable_reason_with(&env);
        assert!(reason.contains("ALPHA_KEY is set"), "{reason}");
        assert!(!reason.contains("set ALPHA_KEY to enable"), "{reason}");

        // The saved-key lookup falls through a malformed env value to the saved key.
        let saved: SavedKeys = serde_json::from_value(serde_json::json!({
            "ALPHA_KEY": {"scope": "alpha", "value": "saved-key"}
        }))
        .unwrap();
        assert_eq!(
            saved
                .resolve("ALPHA_KEY", Some("fixture key".to_owned()))
                .as_deref(),
            Some("saved-key")
        );
        assert_eq!(
            saved
                .resolve("ALPHA_KEY", Some(" env-key ".to_owned()))
                .as_deref(),
            Some("env-key")
        );
        assert_eq!(
            saved.resolve("ALPHA_KEY", None).as_deref(),
            Some("saved-key")
        );
        // With no saved key, the malformed value is still reported to the caller.
        let none = SavedKeys::default();
        assert_eq!(
            none.resolve("ALPHA_KEY", Some("fixture key".to_owned()))
                .as_deref(),
            Some("fixture key")
        );
        assert_eq!(none.resolve("ALPHA_KEY", None), None);
    }

    #[test]
    fn missing_credentials_leave_the_vendor_unavailable() {
        let config = two_vendors("", "");
        let vendor = config.vendors.iter().find(|v| v.key == "alpha").unwrap();
        let env = |_: &str| None;
        assert!(vendor.api_key(&env).is_none());
        assert!(vendor.unavailable_reason().contains("ALPHA_KEY"));
        assert!(
            vendor
                .unavailable_reason_with(&env)
                .contains("set ALPHA_KEY")
        );
        let env = |name: &str| (name == "ALPHA_KEY").then(|| "fixture-key".to_owned());
        let key = vendor.api_key(&env).unwrap();
        assert_eq!(key.expose(), "fixture-key");
        assert!(!format!("{key:?}").contains("fixture-key"));
        let auth = vendor.auth_headers(&key).unwrap();
        assert_eq!(auth.get(AUTHORIZATION).unwrap(), "Bearer fixture-key");
        assert!(auth.get(AUTHORIZATION).unwrap().is_sensitive());
    }

    #[test]
    fn legacy_config_still_loads_a_single_provider() {
        let home = TempHome::new();
        std::fs::write(
            home.0.join("config.toml"),
            r#"
model = "demo-coding"
model_provider = "demo"

[model_providers.demo]
name = "Demo Provider"
base_url = "https://api.example.com/v1"
env_key = "DEMO_API_KEY"
wire_api = "responses"
"#,
        )
        .unwrap();
        let env = |name: &str| (name == "DEMO_API_KEY").then(|| "fixture-key".to_owned());
        let provider = Provider::load(&home.0, &env).unwrap();
        assert_eq!(provider.id, "demo");
        assert_eq!(provider.name, "Demo Provider");
        assert_eq!(
            provider.models_url.as_str(),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            provider.responses_url("/responses", None).as_str(),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            provider.upstream_auth.to_str().unwrap(),
            "Bearer fixture-key"
        );
    }

    #[test]
    fn legacy_config_rejects_cross_origin_catalog() {
        let home = TempHome::new();
        std::fs::write(
            home.0.join("config.toml"),
            r#"
model_provider = "demo"

[model_providers.demo]
base_url = "https://api.example.com/v1"
model_catalog_url = "https://other.example/models"
env_key = "DEMO_API_KEY"
"#,
        )
        .unwrap();
        let env = |_: &str| Some("fixture-key".to_owned());
        let error = Provider::load(&home.0, &env).unwrap_err().to_string();
        assert!(error.contains("same origin"), "{error}");
    }

    fn search_config() -> String {
        r#"
[providers.alpha]
base_url = "https://alpha.example/v1"

[[providers.alpha.models]]
id = "alpha-model"

[search]
env_key = "BRAVE_SEARCH_API_KEY"
"#
        .to_owned()
    }

    #[test]
    fn search_settings_are_typed_and_defaulted() {
        let config = CodeoxConfig::load_from_str(&search_config()).unwrap();
        let search = config.search.as_ref().unwrap();
        assert_eq!(search.env_key, DEFAULT_SEARCH_ENV_KEY);
        assert_eq!(search.max_results, DEFAULT_SEARCH_MAX_RESULTS);
        assert_eq!(search.timeout_seconds, DEFAULT_SEARCH_TIMEOUT_SECONDS);
        assert!(config.env_refs().contains("BRAVE_SEARCH_API_KEY"));
    }

    #[test]
    fn search_settings_reject_unknown_or_out_of_range_values() {
        for (extra, expected) in [
            ("enabled = false\n", ""),
            ("bogus = 1\n", "not a recognized"),
            ("max_results = 0\n", "between 1 and 20"),
            ("timeout_seconds = 121\n", "between 1 and 120"),
            ("env_key = \"CODEOX_TOKEN\"\n", "reserved"),
        ] {
            let raw = if extra.starts_with("env_key") {
                search_config().replace("env_key = \"BRAVE_SEARCH_API_KEY\"", extra.trim())
            } else {
                format!("{}{extra}", search_config())
            };
            let parsed = CodeoxConfig::load_from_str(&raw);
            if expected.is_empty() {
                assert!(parsed.unwrap().search.is_none(), "enabled=false opts out");
            } else {
                let error = parsed.unwrap_err().to_string();
                assert!(error.contains(expected), "{extra:?} -> {error}");
            }
        }
    }
}
