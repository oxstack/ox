//! Budgeted Tier-1 memory injection for first turns of native sessions and workers.

use crate::runtime::{Route, Runtime};
pub use ox_memory::store_path_for;
use ox_memory::{DEFAULT_BRIEF_CHARS, MemoryStore, Sensitivity, canonical_project_id};
use std::path::{Path, PathBuf};

/// Host-supplied configuration for Tier-1 memory injection on a [`crate::store::Store`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryConfig {
    pub store_path: PathBuf,
    pub host_id: String,
    /// Optional pre-filtered brief supplied by a parent host for a remote worker job.
    pub brief_override: Option<String>,
}

/// The startup context sent with a first turn. Receipt metadata excludes memory text and ids.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub entries: usize,
    pub from_parent_host: bool,
}

impl Attachment {
    pub fn label(&self) -> String {
        let entries = if self.entries == 1 {
            "1 pinned memory".to_owned()
        } else {
            format!("{} pinned memories", self.entries)
        };
        if self.from_parent_host {
            format!("{entries} attached from the parent host")
        } else {
            format!("{entries} attached at session start")
        }
    }
}

/// Safe sensitivity ceiling for a resolved runtime/billing route.
///
/// Defaults to `Private`. `OX_MEMORY_ALLOW_LOCAL_ONLY=1` allows `LocalOnly` on a
/// local inference host. The Antigravity account route always uses Google inference.
/// `OX_MEMORY_MAX_SENSITIVITY` can tighten the ceiling further.
pub fn route_sensitivity_ceiling(route: &Route) -> Sensitivity {
    let allow_local_only = std::env::var("OX_MEMORY_ALLOW_LOCAL_ONLY")
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    let cap = std::env::var("OX_MEMORY_MAX_SENSITIVITY")
        .ok()
        .and_then(|v| Sensitivity::parse(&v).ok());
    sensitivity_ceiling(route.runtime, allow_local_only, cap)
}

fn sensitivity_ceiling(
    runtime: Runtime,
    allow_local_only: bool,
    cap: Option<Sensitivity>,
) -> Sensitivity {
    let base = if allow_local_only && runtime != Runtime::Antigravity {
        Sensitivity::LocalOnly
    } else {
        Sensitivity::Private
    };
    cap.map_or(base, |cap| base.min(cap))
}

/// Maximum UTF-8 bytes for the rendered Tier-1 brief body so that
/// `sender_context + brief + recall hint` always fits inside Claude's 2048-byte context limit,
/// even for multibyte CJK/emoji text.
pub const MAX_BRIEF_BYTES: usize = 1400;

const RECALL_HINT: &str =
    "(Use recall_ox_memory for additional saved preferences or project facts.)";

fn truncate_utf8_with_ellipsis(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let budget = max_bytes.saturating_sub('…'.len_utf8());
    let mut end = 0;
    for (idx, ch) in text.char_indices() {
        if idx + ch.len_utf8() > budget {
            break;
        }
        end = idx + ch.len_utf8();
    }
    let mut truncated = text[..end].to_string();
    truncated.push('…');
    truncated
}

fn fit_utf8_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut lines = text.lines();
    let Some(first_line) = lines.next() else {
        return String::new();
    };
    let mut out = String::new();
    let mut has_body = false;

    if first_line.len() <= max_bytes {
        out.push_str(first_line);
        let mut saw_any_body_line = false;
        for line in lines {
            saw_any_body_line = true;
            if out.len() + 1 + line.len() <= max_bytes {
                out.push('\n');
                out.push_str(line);
                has_body = true;
            } else if !has_body {
                // First body line alone exceeded remaining budget: truncate it instead of returning header-only.
                let remaining = max_bytes.saturating_sub(out.len() + 1);
                if remaining >= 8 {
                    out.push('\n');
                    out.push_str(&truncate_utf8_with_ellipsis(line, remaining));
                    has_body = true;
                }
                break;
            } else {
                break;
            }
        }
        if !saw_any_body_line || has_body {
            return out;
        }
    }
    truncate_utf8_with_ellipsis(text, max_bytes)
}

/// Build the Tier-1 pinned memory brief to attach once on the first turn of a session or worker.
///
/// Returns `None` if no eligible pinned memories exist or if the store cannot be read.
pub fn first_turn_brief(config: &MemoryConfig, project: &Path, route: &Route) -> Option<String> {
    let brief = if route.runtime.supports_workers()
        && let Some(override_brief) = config
            .brief_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        override_brief.chars().take(DEFAULT_BRIEF_CHARS).collect()
    } else {
        let store = MemoryStore::open(&config.store_path);
        let project_id = canonical_project_id(project);
        let ceiling = route_sensitivity_ceiling(route);
        store
            .tier1_brief(
                Some(&project_id),
                Some(&config.host_id),
                ceiling,
                DEFAULT_BRIEF_CHARS,
            )
            .ok()??
    };
    let text = fit_utf8_bytes(&brief, MAX_BRIEF_BYTES);
    if route.runtime == Runtime::Antigravity {
        Some(text)
    } else {
        Some(format!("{text}\n{RECALL_HINT}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_memory::{Kind, NewMemory, Scope};

    #[test]
    fn route_ceiling_never_trusts_alias_prefix_for_local_only() {
        assert_eq!(
            route_sensitivity_ceiling(&Route::CODEX_SUBSCRIPTION),
            Sensitivity::Private
        );
        assert_eq!(
            route_sensitivity_ceiling(&Runtime::Claude.route(None)),
            Sensitivity::Private
        );
        assert_eq!(
            route_sensitivity_ceiling(&Route::codex_api("ollama/qwen3".into())),
            Sensitivity::Private
        );
        assert_eq!(
            route_sensitivity_ceiling(&Route::codex_api("local/llama".into())),
            Sensitivity::Private
        );
    }

    #[test]
    fn antigravity_stays_below_local_only_even_when_the_host_allows_local_inference() {
        assert_eq!(
            sensitivity_ceiling(Runtime::Antigravity, true, None),
            Sensitivity::Private
        );
        assert_eq!(
            sensitivity_ceiling(Runtime::Antigravity, true, Some(Sensitivity::Public)),
            Sensitivity::Public
        );
        assert_eq!(
            sensitivity_ceiling(Runtime::Codex, true, None),
            Sensitivity::LocalOnly
        );
    }

    #[test]
    fn first_turn_brief_respects_route_sensitivity_and_override() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.json");
        let store = MemoryStore::open(&store_path);
        store
            .upsert(NewMemory {
                id: None,
                scope: Scope::Global,
                kind: Kind::RoutingPolicy,
                sensitivity: Sensitivity::Private,
                text: "Use Claude subscription for private work.".into(),
                tags: vec!["routing".into()],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();
        store
            .upsert(NewMemory {
                id: None,
                scope: Scope::Host("host-a".into()),
                kind: Kind::Fact,
                sensitivity: Sensitivity::LocalOnly,
                text: "Local-only GPU calibration note.".into(),
                tags: vec![],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();

        let cfg = MemoryConfig {
            store_path,
            host_id: "host-a".into(),
            brief_override: None,
        };

        // Default ceiling gets Private policy but NOT LocalOnly fact (even for ollama/ alias)
        let brief =
            first_turn_brief(&cfg, dir.path(), &Route::codex_api("ollama/qwen3".into())).unwrap();
        assert!(brief.contains("Use Claude subscription for private work."));
        assert!(!brief.contains("Local-only GPU calibration note."));
        assert!(brief.ends_with(RECALL_HINT));

        let antigravity =
            first_turn_brief(&cfg, dir.path(), &Runtime::Antigravity.route(None)).unwrap();
        assert!(antigravity.contains("Use Claude subscription for private work."));
        assert!(!antigravity.contains("Local-only GPU calibration note."));
        assert!(!antigravity.contains(RECALL_HINT));

        // Brief override takes precedence (used by remote worker jobs)
        let override_cfg = MemoryConfig {
            brief_override: Some("OX memory (pinned):\n- [policy/global] Custom brief.".into()),
            ..cfg
        };
        let brief =
            first_turn_brief(&override_cfg, dir.path(), &Route::CODEX_SUBSCRIPTION).unwrap();
        assert!(brief.contains("Custom brief."));
        let antigravity =
            first_turn_brief(&override_cfg, dir.path(), &Runtime::Antigravity.route(None)).unwrap();
        assert!(antigravity.contains("Use Claude subscription for private work."));
        assert!(!antigravity.contains("Custom brief."));
    }

    #[test]
    fn single_large_multibyte_memory_truncates_content_instead_of_returning_header_only() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.json");
        let store = MemoryStore::open(&store_path);
        let cjk = "あ".repeat(480); // 1440 bytes > MAX_BRIEF_BYTES
        store
            .upsert(NewMemory {
                id: None,
                scope: Scope::Global,
                kind: Kind::RoutingPolicy,
                sensitivity: Sensitivity::Private,
                text: cjk,
                tags: vec![],
                pin: true,
                source_host: "host-a".into(),
            })
            .unwrap();

        let cfg = MemoryConfig {
            store_path,
            host_id: "host-a".into(),
            brief_override: None,
        };
        let rendered = first_turn_brief(&cfg, dir.path(), &Runtime::Claude.route(None)).unwrap();
        assert!(rendered.len() <= MAX_BRIEF_BYTES + RECALL_HINT.len() + 1);
        assert!(rendered.contains("- [policy/global] あ"));
        assert!(rendered.contains("…"));
        assert!(rendered.ends_with(RECALL_HINT));
    }
}
