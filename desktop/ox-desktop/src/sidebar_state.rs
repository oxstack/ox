//! Desktop sidebar metadata (`sidebar.json`): pinned threads, user sections, project folders,
//! custom arcade accent colours, explicit thread ordering and local unread/hidden flags.
//!
//! Designed behind a single store record so a host-owned sync engine can later back the same
//! `SidebarState` shape without changing view code.
use crate::appearance::AccentColor;
use ox_desk_core::runtime::Runtime;
use ox_desk_core::store::ThreadRow;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

pub const CURRENT_VERSION: u32 = 1;
const MAX_THREAD_RECORDS: usize = 1000;

/// How the sidebar groups non-pinned threads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupingMode {
    /// Group by user sections and project folders (`ox`, `oxa`, …).
    #[default]
    Folders,
    /// Group by calendar day (`Today`, `Yesterday`, …).
    Timeline,
}

/// Stable identity for a thread's OX sidebar metadata.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadMetaKey {
    pub project: PathBuf,
    pub runtime: Runtime,
    pub id: String,
}

impl ThreadMetaKey {
    pub fn new(project: impl Into<PathBuf>, runtime: Runtime, id: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            runtime,
            id: id.into(),
        }
    }

    pub fn encode(&self) -> String {
        let runtime = match self.runtime {
            Runtime::Codex => "codex",
            Runtime::Claude => "claude",
            Runtime::Antigravity => "antigravity",
            Runtime::OpenCode => "opencode",
        };
        serde_json::to_string(&(&self.project, runtime, &self.id)).unwrap_or_default()
    }

    pub fn decode(raw: &str) -> Option<Self> {
        let (project, runtime, id): (PathBuf, String, String) = serde_json::from_str(raw).ok()?;
        let runtime = match runtime.as_str() {
            "codex" => Runtime::Codex,
            "claude" => Runtime::Claude,
            "antigravity" => Runtime::Antigravity,
            "opencode" => Runtime::OpenCode,
            _ => return None,
        };
        Some(Self {
            project,
            runtime,
            id,
        })
    }
}

/// OX-owned UI metadata for one thread. Never alters native Codex or Claude files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadMeta {
    /// Unix timestamp when pinned; `Some` places the thread in the top `Pinned` section.
    pub pinned_at: Option<i64>,
    /// Optional user section id (`UserSection.id`) overriding the automatic project folder.
    pub section: Option<String>,
    /// Custom per-thread accent colour; falls back to its section or project colour when `None`.
    pub color: Option<AccentColor>,
    /// Explicit position inside its section or `Pinned` group (smaller = higher).
    pub order: Option<i64>,
    /// User-chosen custom title set from OX; protected against automatic title regeneration.
    pub custom_title: Option<String>,
    /// Local unread override (`Some(true)` = marked unread, `Some(false)` = marked read).
    pub unread_override: Option<bool>,
    /// Timestamp of the thread when `unread_override` was set, so newer activity clears `Some(false)`.
    pub unread_override_at: Option<i64>,
    /// Removed from the OX sidebar list without touching the provider's native thread.
    pub hidden: bool,
    /// Cached snapshot so a pinned thread still renders and opens before or without discovery.
    pub cached_title: Option<String>,
    pub cached_provider: Option<String>,
    pub cached_model: Option<String>,
    pub cached_api: Option<bool>,
    pub cached_updated_at: Option<i64>,
}

impl ThreadMeta {
    pub fn is_empty(&self) -> bool {
        self.pinned_at.is_none()
            && self.section.is_none()
            && self.color.is_none()
            && self.order.is_none()
            && self.custom_title.is_none()
            && self.unread_override.is_none()
            && !self.hidden
    }

    pub fn cache_from(&mut self, row: &ThreadRow, api: bool) {
        let title = row.title().trim();
        if !title.is_empty() {
            self.cached_title = Some(title.to_owned());
        }
        if !row.model_provider.is_empty() {
            self.cached_provider = Some(row.model_provider.clone());
        }
        if row.model.is_some() {
            self.cached_model.clone_from(&row.model);
        }
        self.cached_api = Some(api);
        if row.updated_at > 0 {
            self.cached_updated_at = Some(row.updated_at);
        }
    }

    pub fn cached_row(&self, key: &ThreadMetaKey) -> ThreadRow {
        ThreadRow {
            id: key.id.clone(),
            model_provider: self
                .cached_provider
                .clone()
                .unwrap_or_else(|| match key.runtime {
                    Runtime::Codex => "openai".into(),
                    Runtime::Claude => "anthropic".into(),
                    Runtime::Antigravity => "antigravity".into(),
                    Runtime::OpenCode => "opencode".into(),
                }),
            cwd: key.project.clone(),
            model: self.cached_model.clone(),
            parent_thread_id: None,
            agent_nickname: None,
            name: self
                .custom_title
                .clone()
                .or_else(|| self.cached_title.clone()),
            generated_title: None,
            preview: String::new(),
            updated_at: self
                .cached_updated_at
                .or(self.pinned_at)
                .unwrap_or_default(),
            status: None,
            worker: false,
            unread: self.unread_override.unwrap_or(false),
            activity_order: 0,
        }
    }
}

/// A user-created named section ("Inbox", "This week", "Review queue") that can hold threads
/// from any project.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserSection {
    pub id: String,
    pub name: String,
    pub color: Option<AccentColor>,
    pub collapsed: bool,
    pub order: i64,
}

/// Per-project folder settings (collapsed state, accent colour, and display order).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectFolderMeta {
    /// Unix timestamp when starred; `Some` pins this project folder to the top of the project list.
    pub starred_at: Option<i64>,
    pub color: Option<AccentColor>,
    pub collapsed: bool,
    pub order: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarState {
    pub version: u32,
    pub grouping: GroupingMode,
    pub pinned_collapsed: bool,
    pub sections: Vec<UserSection>,
    pub projects: BTreeMap<PathBuf, ProjectFolderMeta>,
    pub threads: BTreeMap<String, ThreadMeta>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            grouping: GroupingMode::Folders,
            pinned_collapsed: false,
            sections: Vec::new(),
            projects: BTreeMap::new(),
            threads: BTreeMap::new(),
        }
    }
}

impl SidebarState {
    pub fn load(name: &str) -> Self {
        let mut state: Self = crate::workspace_state::read_preference(name).unwrap_or_default();
        state.migrate();
        state
    }

    pub fn save_at(path: &Path, state: Self) -> io::Result<()> {
        crate::workspace_state::update_preference_at(path, Self::default, move |_| {
            let mut next = state;
            next.migrate();
            next
        })
    }

    pub fn migrate(&mut self) {
        if self.version == 0 {
            self.version = CURRENT_VERSION;
        }
        self.sections.sort_by_key(|s| (s.order, s.id.clone()));
        let valid_sections: std::collections::HashSet<&str> =
            self.sections.iter().map(|s| s.id.as_str()).collect();
        for meta in self.threads.values_mut() {
            if meta
                .section
                .as_deref()
                .is_some_and(|id| !valid_sections.contains(id))
            {
                meta.section = None;
            }
        }
        self.threads.retain(|_, meta| !meta.is_empty());
        self.cap_growth();
    }

    fn cap_growth(&mut self) {
        if self.threads.len() <= MAX_THREAD_RECORDS {
            return;
        }
        // Never evict pinned threads or threads placed in a user section.
        let mut removable: Vec<(String, i64)> = self
            .threads
            .iter()
            .filter(|(_, m)| m.pinned_at.is_none() && m.section.is_none())
            .map(|(k, m)| (k.clone(), m.cached_updated_at.unwrap_or(0)))
            .collect();
        removable.sort_by_key(|(_, ts)| *ts);
        let to_remove = self.threads.len().saturating_sub(MAX_THREAD_RECORDS);
        for (key, _) in removable.into_iter().take(to_remove) {
            self.threads.remove(&key);
        }
    }

    pub fn thread(&self, key: &ThreadMetaKey) -> Option<&ThreadMeta> {
        self.threads.get(&key.encode())
    }

    pub fn thread_mut(&mut self, key: &ThreadMetaKey) -> &mut ThreadMeta {
        self.threads.entry(key.encode()).or_default()
    }

    /// Clean up an entry if all of its metadata fields were cleared, else cap growth.
    pub fn compact_thread(&mut self, key: &ThreadMetaKey) {
        let encoded = key.encode();
        if self.threads.get(&encoded).is_some_and(ThreadMeta::is_empty) {
            self.threads.remove(&encoded);
        }
        self.cap_growth();
    }

    #[cfg(test)]
    pub fn is_pinned(&self, key: &ThreadMetaKey) -> bool {
        self.thread(key).and_then(|m| m.pinned_at).is_some()
    }

    pub fn toggle_pin(
        &mut self,
        key: &ThreadMetaKey,
        row: &ThreadRow,
        api: bool,
        now: i64,
    ) -> bool {
        let meta = self.thread_mut(key);
        let pinned = if meta.pinned_at.is_some() {
            meta.pinned_at = None;
            false
        } else {
            meta.pinned_at = Some(now.max(1));
            meta.cache_from(row, api);
            true
        };
        self.compact_thread(key);
        pinned
    }

    pub fn set_thread_color(&mut self, key: &ThreadMetaKey, color: Option<AccentColor>) {
        self.thread_mut(key).color = color;
        self.compact_thread(key);
    }

    /// Effective accent colour for a thread: its own colour, then its user section's colour,
    /// then its project folder's colour.
    pub fn effective_color(&self, key: &ThreadMetaKey) -> Option<AccentColor> {
        let meta = self.thread(key);
        if let Some(color) = meta.and_then(|m| m.color) {
            return Some(color);
        }
        if let Some(section_id) = meta.and_then(|m| m.section.as_deref())
            && let Some(section) = self.sections.iter().find(|s| s.id == section_id)
            && section.color.is_some()
        {
            return section.color;
        }
        self.projects.get(&key.project).and_then(|p| p.color)
    }

    pub fn move_to_section(
        &mut self,
        key: &ThreadMetaKey,
        section_id: Option<String>,
        row: &ThreadRow,
        api: bool,
    ) {
        let meta = self.thread_mut(key);
        meta.section = section_id;
        meta.cache_from(row, api);
        self.compact_thread(key);
    }

    pub fn set_custom_title(&mut self, key: &ThreadMetaKey, title: Option<String>) {
        let clean = title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
        let meta = self.thread_mut(key);
        meta.custom_title = clean.clone();
        if let Some(name) = clean {
            meta.cached_title = Some(name);
        }
        self.compact_thread(key);
    }

    pub fn set_unread_override(&mut self, key: &ThreadMetaKey, unread: Option<bool>) {
        self.set_unread_override_at(key, unread, 0);
    }

    pub fn set_unread_override_at(
        &mut self,
        key: &ThreadMetaKey,
        unread: Option<bool>,
        updated_at: i64,
    ) {
        let meta = self.thread_mut(key);
        meta.unread_override = unread;
        meta.unread_override_at = unread.map(|_| updated_at);
        self.compact_thread(key);
    }

    /// Returns the active unread override for `key`, expiring `Some(false)` ("Mark read")
    /// once the thread has newer activity (`updated_at > unread_override_at`).
    pub fn effective_unread_override(&self, key: &ThreadMetaKey, updated_at: i64) -> Option<bool> {
        let meta = self.thread(key)?;
        match meta.unread_override {
            Some(false) if meta.unread_override_at.is_some_and(|at| updated_at > at) => None,
            other => other,
        }
    }

    pub fn set_hidden(&mut self, key: &ThreadMetaKey, hidden: bool) {
        let meta = self.thread_mut(key);
        meta.hidden = hidden;
        if hidden {
            meta.pinned_at = None;
        }
        self.compact_thread(key);
    }

    pub fn create_section(
        &mut self,
        name: impl Into<String>,
        color: Option<AccentColor>,
    ) -> String {
        let name = name.into().trim().to_owned();
        let slug: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let base = if slug.trim_matches('-').is_empty() {
            "section".to_owned()
        } else {
            slug.trim_matches('-').to_owned()
        };
        let mut id = base.clone();
        let mut suffix = 2;
        while self.sections.iter().any(|s| s.id == id) {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        let order = self.sections.iter().map(|s| s.order).max().unwrap_or(0) + 10;
        self.sections.push(UserSection {
            id: id.clone(),
            name,
            color,
            collapsed: false,
            order,
        });
        id
    }

    pub fn rename_section(&mut self, id: &str, new_name: impl Into<String>) -> bool {
        let new_name = new_name.into().trim().to_owned();
        if new_name.is_empty() {
            return false;
        }
        if let Some(section) = self.sections.iter_mut().find(|s| s.id == id) {
            section.name = new_name;
            true
        } else {
            false
        }
    }

    pub fn set_section_color(&mut self, id: &str, color: Option<AccentColor>) {
        if let Some(section) = self.sections.iter_mut().find(|s| s.id == id) {
            section.color = color;
        }
    }

    pub fn toggle_section_collapsed(&mut self, id: &str) {
        if let Some(section) = self.sections.iter_mut().find(|s| s.id == id) {
            section.collapsed = !section.collapsed;
        }
    }

    pub fn move_section(&mut self, id: &str, delta: i32) {
        let Some(pos) = self.sections.iter().position(|s| s.id == id) else {
            return;
        };
        let target = (pos as i32 + delta).clamp(0, self.sections.len() as i32 - 1) as usize;
        if pos == target {
            return;
        }
        self.sections.swap(pos, target);
        for (idx, section) in self.sections.iter_mut().enumerate() {
            section.order = (idx as i64 + 1) * 10;
        }
    }

    /// Delete a user section and safely move all its threads back to their project groups.
    pub fn delete_section(&mut self, id: &str) {
        self.sections.retain(|s| s.id != id);
        let keys: Vec<String> = self
            .threads
            .iter()
            .filter(|(_, m)| m.section.as_deref() == Some(id))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            if let Some(meta) = self.threads.get_mut(&k) {
                meta.section = None;
            }
            if self.threads.get(&k).is_some_and(ThreadMeta::is_empty) {
                self.threads.remove(&k);
            }
        }
    }

    pub fn toggle_project_collapsed(&mut self, project: &Path) {
        let entry = self.projects.entry(project.to_path_buf()).or_default();
        entry.collapsed = !entry.collapsed;
    }

    pub fn is_project_collapsed(&self, project: &Path) -> bool {
        self.projects.get(project).is_some_and(|p| p.collapsed)
    }

    pub fn set_project_color(&mut self, project: &Path, color: Option<AccentColor>) {
        self.projects
            .entry(project.to_path_buf())
            .or_default()
            .color = color;
    }

    pub fn project_color(&self, project: &Path) -> Option<AccentColor> {
        self.projects.get(project).and_then(|p| p.color)
    }

    pub fn is_project_starred(&self, project: &Path) -> bool {
        self.projects
            .get(project)
            .and_then(|p| p.starred_at)
            .is_some()
    }

    pub fn project_starred_at(&self, project: &Path) -> Option<i64> {
        self.projects.get(project).and_then(|p| p.starred_at)
    }

    pub fn toggle_project_star(&mut self, project: &Path, now: i64) -> bool {
        let entry = self.projects.entry(project.to_path_buf()).or_default();
        if entry.starred_at.is_some() {
            entry.starred_at = None;
            false
        } else {
            entry.starred_at = Some(now.max(1));
            true
        }
    }

    pub fn project_order(&self, project: &Path) -> Option<i64> {
        self.projects.get(project).and_then(|p| p.order)
    }

    /// Move a project folder up (`-1`) or down (`1`) within the current project folder order.
    /// Crossing the boundary between starred and unstarred folders promotes or demotes the
    /// folder's starred state so the swap always takes effect visually.
    pub fn step_project_order(&mut self, ordered_projects: &[PathBuf], target: &Path, delta: i32) {
        let Some(pos) = ordered_projects.iter().position(|p| p == target) else {
            return;
        };
        let next = (pos as i32 + delta).clamp(0, ordered_projects.len() as i32 - 1) as usize;
        if pos == next {
            return;
        }
        let other = &ordered_projects[next];
        let target_starred = self.is_project_starred(target);
        let other_starred = self.is_project_starred(other);
        if delta < 0 && !target_starred && other_starred {
            let ts = self.project_starred_at(other).unwrap_or(1).max(1) + 1;
            self.projects
                .entry(target.to_path_buf())
                .or_default()
                .starred_at = Some(ts);
        } else if delta > 0 && target_starred && !other_starred {
            self.projects
                .entry(target.to_path_buf())
                .or_default()
                .starred_at = None;
        }
        let mut ordered = ordered_projects.to_vec();
        ordered.swap(pos, next);
        for (idx, project) in ordered.into_iter().enumerate() {
            self.projects.entry(project).or_default().order = Some((idx as i64 + 1) * 10);
        }
    }

    /// Move a thread up (`-1`) or down (`1`) within its current section or `Pinned` group.
    pub fn step_thread_order(
        &mut self,
        group_keys: &[ThreadMetaKey],
        target: &ThreadMetaKey,
        delta: i32,
    ) {
        let Some(pos) = group_keys.iter().position(|k| k == target) else {
            return;
        };
        let next = (pos as i32 + delta).clamp(0, group_keys.len() as i32 - 1) as usize;
        if pos == next {
            return;
        }
        let mut ordered = group_keys.to_vec();
        ordered.swap(pos, next);
        for (idx, key) in ordered.into_iter().enumerate() {
            self.thread_mut(&key).order = Some(idx as i64 + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(id: &str, title: &str) -> ThreadRow {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "cwd": "/work/ox",
            "modelProvider": "openai",
            "name": title,
            "updatedAt": 1700000000
        }))
        .unwrap()
    }

    #[test]
    fn sidebar_state_round_trips_and_migrates_orphaned_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidebar.json");
        let mut state = SidebarState::default();
        let sec = state.create_section("Review queue", Some(AccentColor::Amber));
        let key = ThreadMetaKey::new("/work/ox", Runtime::Codex, "thread-1");
        let row = sample_row("thread-1", "Fix catalog");
        assert!(state.toggle_pin(&key, &row, false, 12345));
        state.move_to_section(&key, Some(sec.clone()), &row, false);
        state.set_project_color(Path::new("/work/ox"), Some(AccentColor::Cobalt));
        // Before thread-level colour is set, effective colour inherits from section, then project.
        assert_eq!(state.effective_color(&key), Some(AccentColor::Amber));
        state.set_thread_color(&key, Some(AccentColor::Emerald));
        assert_eq!(state.effective_color(&key), Some(AccentColor::Emerald));

        SidebarState::save_at(&path, state.clone()).unwrap();
        let loaded: SidebarState =
            serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
        assert_eq!(loaded, state);

        // Deleting a section returns its threads to their project folder without losing pin or colour.
        let mut after = loaded;
        after.delete_section(&sec);
        assert!(after.sections.is_empty());
        assert!(after.is_pinned(&key));
        assert_eq!(after.thread(&key).unwrap().section, None);
        after.set_thread_color(&key, None);
        assert_eq!(after.effective_color(&key), Some(AccentColor::Cobalt));
    }

    #[test]
    fn pinned_thread_synthesizes_cached_row_when_absent_from_catalog() {
        let mut state = SidebarState::default();
        let key = ThreadMetaKey::new("/work/oxa", Runtime::Claude, "claude-99");
        let row = sample_row("claude-99", "Offline pinned thread");
        state.toggle_pin(&key, &row, true, 5000);
        let meta = state.thread(&key).unwrap();
        let cached = meta.cached_row(&key);
        assert_eq!(cached.id, "claude-99");
        assert_eq!(cached.title(), "Offline pinned thread");
        assert_eq!(cached.cwd, PathBuf::from("/work/oxa"));
        assert_eq!(meta.cached_api, Some(true));
    }

    #[test]
    fn mark_read_override_expires_when_newer_activity_arrives() {
        let mut state = SidebarState::default();
        let key = ThreadMetaKey::new("/work/ox", Runtime::Codex, "thread-unread");
        state.set_unread_override_at(&key, Some(false), 1000);
        assert_eq!(state.effective_unread_override(&key, 1000), Some(false));
        assert_eq!(state.effective_unread_override(&key, 1005), None);
        state.set_unread_override(&key, None);
        assert!(state.thread(&key).is_none());
    }

    #[test]
    fn project_folder_star_and_order_step() {
        let mut state = SidebarState::default();
        let p_a = PathBuf::from("/work/alpha");
        let p_b = PathBuf::from("/work/beta");
        let p_c = PathBuf::from("/work/gamma");
        assert!(!state.is_project_starred(&p_b));
        assert!(state.toggle_project_star(&p_b, 100));
        assert!(state.is_project_starred(&p_b));

        // Rendered order has starred p_b first, followed by unstarred p_a and p_c.
        let list = vec![p_b.clone(), p_a.clone(), p_c.clone()];
        state.step_project_order(&list, &p_c, -1);
        assert!(state.project_order(&p_c) < state.project_order(&p_a));

        // Moving unstarred p_c up across starred p_b promotes p_c to starred.
        let list2 = vec![p_b.clone(), p_c.clone(), p_a.clone()];
        state.step_project_order(&list2, &p_c, -1);
        assert!(state.is_project_starred(&p_c));
        assert!(state.project_order(&p_c) < state.project_order(&p_b));

        // Moving starred p_b down across unstarred p_a demotes p_b to unstarred.
        let list3 = vec![p_c.clone(), p_b.clone(), p_a.clone()];
        state.step_project_order(&list3, &p_b, 1);
        assert!(!state.is_project_starred(&p_b));
        assert!(state.project_order(&p_a) < state.project_order(&p_b));
    }
}
