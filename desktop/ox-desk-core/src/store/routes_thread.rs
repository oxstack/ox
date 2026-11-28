//! The operator's explicit model choice belongs to one thread on one billing provider.
use super::{Pending, Route, Store};
use crate::runtime::Billing;
use crate::{changed::Changed, codex::request::Call};
use serde::Deserialize;
use std::io;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Effort {
    pub reasoning_effort: String,
    #[serde(default)]
    pub description: String,
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub label: String,
    pub input_modalities: Option<Vec<String>>,
    pub efforts: Vec<Effort>,
    pub default_effort: Option<String>,
    pub default_tier: Option<String>,
    /// Position in the runtime's own model list: that order is its preferred one.
    #[serde(default)]
    pub rank: Option<u32>,
    /// The runtime marks this entry as its default model.
    #[serde(default)]
    pub is_default: bool,
    /// The concrete model id a symbolic entry such as Claude's `default` resolves to.
    #[serde(default)]
    pub resolved: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Options {
    pub model: String,
    pub effort: Option<String>,
    pub tier: Option<String>,
}
#[derive(Clone, Debug, serde::Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", schemars(rename = "ModelChoice"))]
pub enum Choice {
    Model(String),
    Effort(String),
    Speed(Option<String>),
}

impl Store {
    pub fn thread_route(&self, id: Option<&str>) -> Route {
        if let Some(remote) = self.remote() {
            return id
                .and_then(|id| remote.routes.get(id))
                .unwrap_or(&self.profile.route)
                .clone();
        }
        if self.is_session_scoped() || self.is_antigravity() || self.is_opencode() {
            return self.profile.route.clone();
        }
        let Billing::Api(alias) = &self.profile.route.billing else {
            return Route::CODEX_SUBSCRIPTION;
        };
        let current = match id {
            Some(id) => self.authorized_models.get(id),
            None => self.next_options.as_ref().map(|options| &options.model),
        };
        Route::codex_api(current.unwrap_or(alias).clone())
    }
    pub fn options(&self, thread: Option<&str>) -> Options {
        if let Some(r) = self.remote() {
            return thread
                .and_then(|id| r.options.get(id))
                .or(self.next_options.as_ref())
                .cloned()
                .unwrap_or_default();
        }
        if self.is_opencode()
            && let Billing::Api(alias) = &self.profile.route.billing
        {
            return Options {
                model: alias.clone(),
                ..Options::default()
            };
        }
        let mut caps = self.command_caps(thread);
        if caps.model.is_empty()
            && self.is_session_scoped()
            && let Billing::Api(alias) = &self.profile.route.billing
        {
            // The billing route already identifies the model before stock sends its schema.
            caps.model.clone_from(alias);
        }

        match thread.and_then(|id| self.transcript(id)) {
            Some(t) => Options {
                model: thread
                    .and_then(|id| self.resume_models.get(id))
                    .cloned()
                    .or_else(|| t.model.clone())
                    .unwrap_or(caps.model),
                effort: t.effort.clone(),
                tier: t.tier.clone(),
            },
            None => self.next_options.clone().unwrap_or(Options {
                model: caps.model,
                effort: None,
                tier: caps.tier,
            }),
        }
    }
    pub fn same_model_route(&self, model: &str) -> bool {
        match &self.profile.route.billing {
            Billing::Subscription => true,
            Billing::Api(active)
                if self.profile.route.runtime == crate::runtime::Runtime::OpenCode =>
            {
                active == model
            }
            Billing::Api(active)
                if self.is_session_scoped() || self.profile.provider == "codeox" =>
            {
                crate::runtime::same_api_provider(active, model)
            }
            Billing::Api(_) => true, // One directly configured legacy Responses provider.
        }
    }
    pub(super) fn expected_alias(&self, thread: &str) -> Option<&str> {
        if let Some(remote) = self.remote()
            && let Some(route) = remote.routes.get(thread)
        {
            return match &route.billing {
                Billing::Api(alias) => Some(alias),
                Billing::Subscription => None,
            };
        }
        match &self.profile.route.billing {
            Billing::Subscription => None,
            Billing::Api(default) => Some(self.authorized_models.get(thread).unwrap_or(default)),
        }
    }
    pub fn changing_options(&self, id: &str) -> bool {
        if let Some(r) = self.remote() {
            return r.changing.contains(id)
                || r.unconfirmed_options.values().any(|thread| thread == id);
        }
        if self.is_antigravity() {
            return self.antigravity_configuring(id);
        }
        self.pending
            .values()
            .any(|(_, p)| matches!(p,Pending::Options(owner,_) if owner==id))
    }
    pub fn choose(&mut self, owner: Option<&str>, choice: Choice) -> io::Result<()> {
        if self.remote().is_some() {
            return self
                .dispatch_remote(crate::actions::Action::ModelOptions(
                    owner.map(str::to_owned),
                    choice,
                ))
                .map(|_| ());
        }
        if !(self.can_send(owner) || owner.is_some_and(|id| self.can_configure_saved_claude(id)))
            || owner.is_some_and(|id| self.changing_options(id))
        {
            return Err(io::Error::other("Wait for the pending thread settings."));
        }
        if self.is_antigravity() {
            return self.antigravity_choose(owner, choice);
        }
        if self.is_session_scoped() {
            return self.claude_choose(owner, choice);
        }
        let caps = self.command_caps(owner);
        if owner.is_some() && !caps.methods.contains("thread/settings/update") {
            return Err(io::Error::other(
                "The installed runtime cannot change model settings on an open thread.",
            ));
        }
        let mut next = self.options(owner);
        match choice {
            Choice::Model(id) => {
                let info = caps
                    .models
                    .get(&id)
                    .ok_or_else(|| io::Error::other("This model is unavailable."))?;
                if !self.same_model_route(&id) {
                    return Err(io::Error::other(
                        "Open a new session on this model’s billing route.",
                    ));
                }
                next = Options {
                    model: id,
                    effort: info.default_effort.clone(),
                    tier: info.default_tier.clone(),
                };
            }
            Choice::Effort(id) => {
                if !caps
                    .models
                    .get(&next.model)
                    .is_some_and(|m| m.efforts.iter().any(|e| e.reasoning_effort == id))
                {
                    return Err(io::Error::other(
                        "This model does not advertise that reasoning effort.",
                    ));
                }
                next.effort = Some(id);
            }
            Choice::Speed(tier) => {
                if !caps.speed {
                    return Err(io::Error::other(
                        "The installed runtime does not support changing speed.",
                    ));
                }
                if tier.as_deref().is_some_and(|id| {
                    !caps
                        .catalog
                        .get(&next.model)
                        .is_some_and(|tiers| tiers.iter().any(|t| t.id == id))
                }) {
                    return Err(io::Error::other(
                        "This model does not advertise that speed.",
                    ));
                }
                next.tier = tier;
            }
        }
        if let Some(id) = owner {
            self.request(
                Call::Options {
                    thread: id.into(),
                    options: next.clone(),
                },
                Pending::Options(id.into(), next),
            )?;
        } else {
            self.next_options = Some(next);
        }
        Ok(())
    }
    pub(super) fn apply_options(&mut self, id: &str, next: Options) {
        if let Some(t) = self.transcripts.get_mut(id).filter(|t| t.is_ready()) {
            if self.profile.route.is_api() {
                self.authorized_models.insert(id.into(), next.model.clone());
            }
            if let Some(row) = self.catalog.get_mut(id) {
                row.model = Some(next.model.clone());
                self.catalog_revision += 1;
                self.changed.mark(Changed::THREADS);
            }
            t.error = None;
            t.model = Some(next.model);
            t.effort = next.effort;
            t.tier = next.tier;
            t.send_options = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codex::{history::ReadThread, notice::Notification},
        links::codex::{Launch, Link},
        store::{Connection, Profile},
    };
    use serde_json::json;

    fn api_store() -> Store {
        let project = std::env::temp_dir();
        let link = Link::spawn(
            Launch {
                executable: "/bin/cat".into(),
                arguments: vec![],
                directory: project.clone(),
                path: "/usr/bin:/bin".into(),
            },
            |_| {},
        )
        .unwrap();
        let mut store = Store::open(
            Profile {
                project,
                route: Route::codex_api("first/demo".into()),
                provider: "codeox".into(),
            },
            link,
        )
        .unwrap();
        store.state = Connection::Ready;
        store
    }

    #[test]
    fn cold_api_history_requires_saved_model_and_child_binding_requires_known_parent() {
        let mut store = api_store();
        let mut row: crate::codex::history::ThreadRow = serde_json::from_value(json!({
            "id":"root","cwd":store.profile.project,"modelProvider":"codeox"
        }))
        .unwrap();
        store.checked_read(
            "root",
            ReadThread {
                thread: Box::new(row.clone()),
            },
        );
        assert!(
            store
                .transcript("root")
                .unwrap()
                .error()
                .unwrap()
                .contains("unknown")
        );
        assert!(!store.can_send(Some("root")));

        row.id = "child".into();
        row.parent_thread_id = Some("root".into());
        store.record_lineage(&row);
        assert!(!store.authorized_models.contains_key("child"));
        store
            .authorized_models
            .insert("root".into(), "first/alternate".into());
        store.record_lineage(&row);
        assert_eq!(store.expected_alias("child"), Some("first/alternate"));
        row.id = "foreign".into();
        row.model = Some("second/demo".into());
        store.record_lineage(&row);
        assert!(!store.authorized_models.contains_key("foreign"));
    }

    #[test]
    fn a_late_model_acknowledgement_does_not_change_a_closed_owner() {
        let mut store = api_store();
        let thread = store.transcripts.entry("root".into()).or_default();
        thread.ready = true;
        thread.verified = true;
        thread.model = Some("first/demo".into());
        store
            .notification(Notification::Closed("root".into()))
            .unwrap();
        store.apply_options(
            "root",
            Options {
                model: "first/alternate".into(),
                ..Default::default()
            },
        );
        assert!(!store.can_send(Some("root")));
        assert_eq!(store.options(Some("root")).model, "first/demo");
        assert!(
            store
                .transcript("root")
                .unwrap()
                .error()
                .unwrap()
                .contains("closed")
        );
    }
}
