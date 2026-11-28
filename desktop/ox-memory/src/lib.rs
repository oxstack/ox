//! Fast, pure-Rust multi-host memory engine for OX.
//!
//! Stores concise, atomic memories (`routing_policy`, `preference`, `fact`) scoped to
//! `global`, `project:<canonical_id>`, or `host:<host_id>` with explicit sensitivity
//! boundaries (`public`, `private`, `local_only`).

mod search;
mod store;

pub use search::{DEFAULT_BRIEF_CHARS, RecallHit, RecallQuery, format_tier1_brief, rank_memories};
pub use store::{MemoryStore, MergeStats, UpsertOutcome};

use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::RandomState,
    fmt,
    hash::BuildHasher,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Maximum characters allowed in a single memory entry.
pub const MAX_TEXT_CHARS: usize = 500;
/// Maximum number of tags stored on a single memory entry.
pub const MAX_TAGS: usize = 12;
/// Maximum characters allowed per tag.
pub const MAX_TAG_CHARS: usize = 32;

/// Memory visibility and sync boundary.
///
/// Ordered from least sensitive to most sensitive so `entry.sensitivity <= ceiling`
/// safely gates both model injection and remote host synchronization.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// Safe to inject into any provider route and sync across paired hosts.
    Public,
    /// Safe for trusted/private routes and paired hosts, excluded from public-only routes.
    #[default]
    Private,
    /// Strictly stays on this host; never synced over Tailscale or sent to remote workers.
    LocalOnly,
}

impl Sensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::LocalOnly => "local_only",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, MemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "public" => Ok(Self::Public),
            "private" => Ok(Self::Private),
            "local_only" | "local-only" | "local" => Ok(Self::LocalOnly),
            other => Err(MemoryError::InvalidInput(format!(
                "Unknown sensitivity '{other}'. Expected public, private, or local_only."
            ))),
        }
    }
}

impl fmt::Display for Sensitivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Category of stored memory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Provider, model, or billing preference tied to data classes or tasks.
    RoutingPolicy,
    /// User workflow, coding style, or communication preference.
    Preference,
    /// Durable technical, repository, or environment fact.
    #[default]
    Fact,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoutingPolicy => "routing_policy",
            Self::Preference => "preference",
            Self::Fact => "fact",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::RoutingPolicy => "policy",
            Self::Preference => "pref",
            Self::Fact => "fact",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, MemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "routing_policy" | "routing-policy" | "policy" | "routing" => Ok(Self::RoutingPolicy),
            "preference" | "pref" => Ok(Self::Preference),
            "fact" => Ok(Self::Fact),
            other => Err(MemoryError::InvalidInput(format!(
                "Unknown memory kind '{other}'. Expected routing_policy, preference, or fact."
            ))),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a memory applies.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Global,
    Project(String),
    Host(String),
}

impl Scope {
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project(_) => "project",
            Self::Host(_) => "host",
        }
    }

    pub fn display_key(&self) -> String {
        match self {
            Self::Global => "global".to_string(),
            Self::Project(p) => format!("project:{p}"),
            Self::Host(h) => format!("host:{h}"),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_key())
    }
}

/// The outcome of an agent's memory call, safe to show with its session.
/// These variants carry counts and scope categories, without entry data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Receipt {
    Recalled { count: u8 },
    Saved { scope: ScopeKind },
    Updated { scope: ScopeKind },
    Deduplicated { scope: ScopeKind },
    Forgotten { scope: ScopeKind },
}

impl Receipt {
    pub fn label(self) -> String {
        match self {
            Self::Recalled { count: 0 } => "No eligible memories found".into(),
            Self::Recalled { count: 1 } => "Recalled 1 memory".into(),
            Self::Recalled { count } => format!("Recalled {count} memories"),
            Self::Saved { scope } => format!("Saved a {} memory", scope.label()),
            Self::Updated { scope } => format!("Updated a {} memory", scope.label()),
            Self::Deduplicated { scope } => format!("{} already saved", scope.memory_label()),
            Self::Forgotten { scope } => format!("Forgot a {} memory", scope.label()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Global,
    Project,
    Host,
}

impl ScopeKind {
    fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Host => "host",
        }
    }

    fn memory_label(self) -> &'static str {
        match self {
            Self::Global => "Global memory",
            Self::Project => "Project memory",
            Self::Host => "Host memory",
        }
    }
}

impl From<&Scope> for ScopeKind {
    fn from(scope: &Scope) -> Self {
        match scope {
            Scope::Global => Self::Global,
            Scope::Project(_) => Self::Project,
            Scope::Host(_) => Self::Host,
        }
    }
}

/// A single atomic memory record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub scope: Scope,
    pub kind: Kind,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub pin: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub source_host: String,
    /// Strictly increasing local change sequence assigned under the store write lock.
    #[serde(default)]
    pub seq: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Input payload for creating or updating a memory.
#[derive(Clone, Debug)]
pub struct NewMemory {
    pub id: Option<String>,
    pub scope: Scope,
    pub kind: Kind,
    pub sensitivity: Sensitivity,
    pub text: String,
    pub tags: Vec<String>,
    pub pin: bool,
    pub source_host: String,
}

/// Errors returned by memory operations.
#[derive(Debug)]
pub enum MemoryError {
    InvalidInput(String),
    NotFound(String),
    Io(std::io::Error),
    Serde(serde_json::Error),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) => f.write_str(msg),
            Self::NotFound(msg) => f.write_str(msg),
            Self::Io(err) => write!(f, "memory storage I/O error: {err}"),
            Self::Serde(err) => write!(f, "memory storage JSON error: {err}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<std::io::Error> for MemoryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

/// Current wall-clock timestamp in milliseconds since UNIX epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate a collision-resistant 12-character lowercase hex identifier using OS-seeded hashing.
pub fn generate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let state = RandomState::new();
    let hash = state.hash_one((now_ms(), std::process::id(), seq));
    format!("{:012x}", hash & 0x0000_ffff_ffff_ffff)
}

/// Normalize and validate memory text.
pub fn normalize_text(raw: &str) -> Result<String, MemoryError> {
    let collapsed = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return Err(MemoryError::InvalidInput(
            "Memory text cannot be empty.".to_string(),
        ));
    }
    if collapsed.chars().count() > MAX_TEXT_CHARS {
        return Err(MemoryError::InvalidInput(format!(
            "Memory text must be at most {MAX_TEXT_CHARS} characters; save concise atomic facts."
        )));
    }
    Ok(collapsed)
}

/// Normalize, deduplicate, and bound memory tags.
pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in tags {
        let tag = raw
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
            .take(MAX_TAG_CHARS)
            .collect::<String>();
        if !tag.is_empty() && !out.contains(&tag) {
            out.push(tag);
            if out.len() >= MAX_TAGS {
                break;
            }
        }
    }
    out
}

/// Resolve the default on-disk memory store path.
///
/// Precedence:
/// 1. `OX_MEMORY_PATH` (explicit file path, useful for tests or custom setups)
/// 2. `OX_HOST_STATE_DIR/memory.json` (aligns with `ox-host` state isolation)
/// 3. `$XDG_STATE_HOME/ox/host/memory.json` (or OS default state dir)
pub fn default_store_path() -> Result<PathBuf, MemoryError> {
    if let Some(path) = non_empty_env_path("OX_MEMORY_PATH") {
        return Ok(path);
    }
    if let Some(state_dir) = absolute_env_path("OX_HOST_STATE_DIR") {
        return Ok(state_dir.join("memory.json"));
    }
    let base = if let Some(xdg) = absolute_env_path("XDG_STATE_HOME") {
        xdg
    } else {
        let home = absolute_env_path("HOME").ok_or_else(|| {
            MemoryError::InvalidInput("Set HOME or OX_MEMORY_PATH to locate OX memory.".to_string())
        })?;
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join(".local/state")
        }
    };
    Ok(base.join("ox/host/memory.json"))
}

/// Resolve the memory store path for a given host state directory, honoring `OX_MEMORY_PATH`.
pub fn store_path_for(state_dir: &Path) -> PathBuf {
    non_empty_env_path("OX_MEMORY_PATH").unwrap_or_else(|| state_dir.join("memory.json"))
}

fn non_empty_env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn absolute_env_path(key: &str) -> Option<PathBuf> {
    non_empty_env_path(key).filter(|p| p.is_absolute())
}

/// Best-effort stable local host label for `source_host` when `ox-host` identity is not supplied.
pub fn default_host_id() -> String {
    if let Ok(explicit) = std::env::var("OX_HOST_ID") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(store_path) = default_store_path()
        && let Some(dir) = store_path.parent()
        && let Ok(host_id) = std::fs::read_to_string(dir.join("host-id"))
    {
        let trimmed = host_id.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(hostname) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = hostname.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "local".to_string()
}

/// Derive a cross-host stable canonical project identifier from a working directory.
///
/// If `cwd` is inside a git repository (including linked worktrees) with a configured
/// `origin` remote, returns the normalized remote slug (e.g. `github.com/oxstack/ox`)
/// so Linux, macOS, and worktree checkouts share the same project memory scope.
/// Otherwise falls back to the canonicalized repository root or `cwd`.
pub fn canonical_project_id(cwd: &Path) -> String {
    if let Some((repo_root, git_common_dir)) = find_git_dirs(cwd) {
        if let Some(remote) = read_git_origin_url(&git_common_dir)
            && let Some(normalized) = normalize_git_remote(&remote)
        {
            return normalized;
        }
        return std::fs::canonicalize(&repo_root)
            .unwrap_or(repo_root)
            .to_string_lossy()
            .into_owned();
    }
    std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn find_git_dirs(start: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some((dir, dot_git));
        }
        if dot_git.is_file()
            && let Ok(contents) = std::fs::read_to_string(&dot_git)
            && let Some(raw_gitdir) = contents.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("gitdir:")
                    .map(|rest| rest.trim().to_string())
            })
        {
            let gitdir = {
                let p = PathBuf::from(raw_gitdir);
                if p.is_absolute() { p } else { dir.join(p) }
            };
            let common = gitdir
                .join("commondir")
                .is_file()
                .then(|| std::fs::read_to_string(gitdir.join("commondir")).ok())
                .flatten()
                .map(|rel| {
                    let rel = rel.trim();
                    let p = PathBuf::from(rel);
                    if p.is_absolute() { p } else { gitdir.join(p) }
                })
                .unwrap_or(gitdir);
            return Some((dir, common));
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_git_origin_url(git_common_dir: &Path) -> Option<String> {
    let config = std::fs::read_to_string(git_common_dir.join("config")).ok()?;
    let mut in_origin = false;
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_origin = line.eq_ignore_ascii_case("[remote \"origin\"]");
            continue;
        }
        if in_origin
            && let Some((key, val)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            let url = val.trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Normalize SSH/HTTPS git remotes to a host/path slug (e.g. `github.com/oxstack/ox`).
pub fn normalize_git_remote(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("ssh://"))
        .or_else(|| trimmed.strip_prefix("git://"));
    let host_and_path = if let Some(rest) = stripped {
        let rest = rest.split_once('@').map_or(rest, |(_, after)| after);
        rest.to_string()
    } else if let Some((left, right)) = trimmed.split_once(':') {
        // SCP-like syntax: git@github.com:org/repo.git
        let host = left.split_once('@').map_or(left, |(_, after)| after);
        format!("{host}/{right}")
    } else {
        return None;
    };
    let without_query = host_and_path
        .split(['?', '#'])
        .next()
        .unwrap_or(&host_and_path);
    let cleaned = without_query
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_git_remotes_to_same_slug() {
        assert_eq!(
            normalize_git_remote("git@github.com:oxstack/ox.git").as_deref(),
            Some("github.com/oxstack/ox")
        );
        assert_eq!(
            normalize_git_remote("https://github.com/oxstack/ox.git").as_deref(),
            Some("github.com/oxstack/ox")
        );
        assert_eq!(
            normalize_git_remote("https://user@github.com/OxStack/OX/").as_deref(),
            Some("github.com/oxstack/ox")
        );
        assert_eq!(
            normalize_git_remote("ssh://git@gitlab.com/team/sub/repo.git").as_deref(),
            Some("gitlab.com/team/sub/repo")
        );
    }

    #[test]
    fn sensitivity_ordering_is_monotonic() {
        assert!(Sensitivity::Public < Sensitivity::Private);
        assert!(Sensitivity::Private < Sensitivity::LocalOnly);
    }

    #[test]
    fn normalize_text_rejects_empty_and_overlong() {
        assert!(normalize_text("   \n\t  ").is_err());
        let long = "a".repeat(MAX_TEXT_CHARS + 1);
        assert!(normalize_text(&long).is_err());
        assert_eq!(
            normalize_text("  line one \n  line two ").unwrap(),
            "line one line two"
        );
    }
}
