//! Native OX settings and first-run account connection.
use crate::appearance::Step;
mod about;
mod advanced;
pub mod behavior;
mod forms;
mod hosts;
mod key_field;
mod memory;
pub(crate) use hosts::{HostDirectory, Hosts};
mod remote;
mod sessions;
use crate::{
    appearance::Palette,
    platform,
    settings::{
        Credential, Settings,
        model::{self, Cell, Edit, Key, Values},
        protocol::Request,
    },
};
use forms::{CONNECTION, Fields, LEGACY, MODEL, Provider, Registry, SEARCH};
use gpui::{prelude::*, *};
use gpui_component::input::InputState;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Hosts,
    Remote,
    Accounts,
    Advanced,
    Behavior,
    Notifications,
    Providers,
    Search,
    Favorites,
    Credentials,
    Legacy,
    Sessions,
    Memory,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Connection,
    Options,
    Models,
}

struct KeyField {
    credential: Credential,
    input: Entity<key_field::KeyField>,
}
struct Legacy {
    model: Entity<InputState>,
    fields: Fields,
    id: String,
    initial_model: String,
}

pub(crate) struct ContinueSetup(pub Option<String>);
impl EventEmitter<ContinueSetup> for Preferences {}

pub struct Preferences {
    link: platform::HostLink,
    hosts: Entity<hosts::Hosts>,
    focus: FocusHandle,
    about: Entity<about::About>,
    remote: Option<Entity<remote::Remote>>,
    advanced: Option<advanced::Advanced>,
    memory: Option<Entity<memory::Library>>,
    project: PathBuf,
    prefer_providers: bool,
    settings: Settings,
    remote_behavior: Option<Result<behavior::Behavior, String>>,
    page: Page,
    section: Section,
    selected: Option<usize>,
    model: Option<usize>,
    registry: Option<Registry>,
    legacy: Option<Legacy>,
    favorites: Vec<String>,
    alias: Entity<InputState>,
    keys: Vec<KeyField>,
    busy: bool,
    /// A starred-models autosave arrived while another save was running.
    resave: bool,
    message: String,
    error: bool,
    loading_models: bool,
    scroll: ScrollHandle,
    search_revision: String,
    conflict: Option<(Page, String)>,
    credential_provider: Option<String>,
    tested_provider: Option<(String, Option<String>)>,
}

impl Preferences {
    pub fn new(
        link: platform::HostLink,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let alias = forms::input("", "provider/model", window, cx);
        cx.subscribe(&alias, |_, _, _: &gpui_component::input::InputEvent, cx| {
            cx.notify()
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut ui = Self {
            link,
            hosts: cx.new(|cx| hosts::Hosts::new(window, cx)),
            focus,
            advanced: None,
            memory: None,
            about: cx.new(about::About::new),
            remote: None,
            project,
            prefer_providers: false,
            settings: Settings::default(),
            remote_behavior: None,
            page: Page::Providers,
            section: Section::Connection,
            selected: None,
            model: None,
            registry: None,
            legacy: None,
            favorites: vec![],
            alias,
            keys: vec![],
            busy: false,
            resave: false,
            message: String::new(),
            error: false,
            loading_models: false,
            scroll: ScrollHandle::new(),
            search_revision: String::new(),
            conflict: None,
            credential_provider: None,
            tested_provider: None,
        };
        ui.reload(window, cx);
        ui
    }
    pub fn providers(
        link: platform::HostLink,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::new(link, project, window, cx);
        view.prefer_providers = true;
        view
    }

    pub(crate) fn open_session_on_phone(
        &mut self,
        scope: &ox_desk_core::host_protocol::Scope,
        thread: &str,
        title: String,
        cx: &mut Context<Self>,
    ) {
        self.open_remote(cx);
        if let Some(remote) = &self.remote {
            remote.update(cx, |view, cx| view.set_session(scope, thread, title, cx));
        }
    }

    /// Show the Remote access page (the sidebar's phone control).
    pub fn open_remote(&mut self, cx: &mut Context<Self>) {
        // Stay here when the first load lands, instead of jumping to Accounts.
        self.prefer_providers = true;
        if self.page != Page::Remote {
            self.goto(Page::Remote, cx);
        }
    }
    pub(crate) fn open_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.link.remote().is_some() {
            return;
        }
        self.prefer_providers = true;
        if self.memory.is_none() {
            self.memory = Some(cx.new(|cx| memory::Library::new(self.project.clone(), window, cx)));
        }
        self.goto(Page::Memory, cx);
    }

    fn goto(&mut self, page: Page, cx: &mut Context<Self>) {
        let page = if page == Page::Remote && self.link.remote().is_some() {
            Page::Hosts
        } else {
            page
        };
        self.remote = (page == Page::Remote)
            .then(|| cx.new(|cx| remote::Remote::new(self.project.clone(), cx)));
        self.page = page;
        if page == Page::Accounts && self.link.remote().is_some() {
            self.refresh_remote_behavior(cx);
        }
        if page == Page::Favorites {
            self.fetch_models(cx);
        }
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }
    pub(crate) fn worker_hosts(
        link: platform::HostLink,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = link.remote().cloned();
        let mut view = Self::new(link, project, window, cx);
        view.prefer_providers = true;
        view.goto(Page::Hosts, cx);
        view.hosts
            .update(cx, |hosts, cx| hosts.open_worker_links(source, cx));
        view
    }

    fn report(&mut self, result: Result<(), String>, success: &str, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                self.message = success.into();
                self.error = false
            }
            Err(e) => {
                self.message = e;
                self.error = true
            }
        }
        cx.notify();
    }
    fn reload(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        let link = self.link.clone();
        cx.spawn_in(w, async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { link.settings() })
                .await;
            let _ = view.update_in(cx, |s, w, cx| {
                s.busy = false;
                match result {
                    Ok(data) => {
                        s.conflict = None;
                        s.install(data, None, w, cx);
                    }
                    Err(e) => s.report(Err(e), "", cx),
                }
            });
        })
        .detach();
    }
    fn install(
        &mut self,
        mut data: Settings,
        page: Option<Page>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let all = page.is_none();
        let advanced = page == Some(Page::Advanced);
        // A text save can change several native forms at once. Refresh each clean
        // form and keep any unsaved native draft on its original revision.
        let registry = self.registry.as_ref();
        let providers = all
            || page == Some(Page::Providers)
            || advanced
                && registry.is_none_or(
                    |r| matches!(r.edit(cx), Ok(Edit::Providers { changes }) if changes.is_empty()),
                );
        let search = all
            || page == Some(Page::Search)
            || advanced
                && registry.is_none_or(|r| {
                    r.search
                        .patch(&[SEARCH], false, cx)
                        .is_ok_and(|p| p.is_empty())
                });
        let favorites = all
            || page == Some(Page::Favorites)
            || advanced && self.favorites == self.settings.favorites;
        let legacy = all
            || page == Some(Page::Legacy)
            || advanced
                && self.legacy.as_ref().is_none_or(|l| {
                    l.model.read(cx).value().trim() == l.initial_model
                        && l.fields
                            .patch(&[LEGACY], false, cx)
                            .is_ok_and(|p| p.is_empty())
                });
        if providers || search {
            match (providers, self.registry.as_mut(), data.registry.take()) {
                (false, Some(draft), Some(rows)) => {
                    draft.search = Fields::new(rows.search, &[SEARCH], w, cx);
                }
                (_, previous, Some(rows)) => {
                    let mut loaded = Registry::new(rows, w, cx);
                    if !search && let Some(draft) = previous {
                        std::mem::swap(&mut loaded.search, &mut draft.search);
                    }
                    self.registry = Some(loaded);
                }
                (_, _, None) => self.registry = None,
            }
        }
        if search {
            self.search_revision = data.revisions.registry.clone();
        }
        if providers {
            self.selected = None;
            self.model = None;
            self.section = Section::Connection;
        } else {
            data.revisions.registry = self.settings.revisions.registry.clone();
        }
        if favorites {
            self.favorites.clone_from(&data.favorites);
        } else {
            data.favorites.clone_from(&self.settings.favorites);
            data.revisions.favorites = self.settings.revisions.favorites.clone()
        }
        if legacy {
            self.legacy = data.legacy.take().map(|l| {
                let initial_model = l.model.as_ref().map(forms::display).unwrap_or_default();
                Legacy {
                    id: l.id,
                    model: forms::input(&initial_model, "Default model ID", w, cx),
                    initial_model,
                    fields: Fields::new(l.fields, &[LEGACY], w, cx),
                }
            })
        } else {
            data.revisions.legacy = self.settings.revisions.legacy.clone()
        }
        if !all && page != Some(Page::Credentials) {
            data.revisions.keys = self.settings.revisions.keys.clone();
        }
        let mut old = std::mem::take(&mut self.keys);
        self.keys = data
            .credentials
            .iter()
            .map(|c| KeyField {
                credential: c.clone(),
                input: old
                    .iter()
                    .position(|k| k.credential.name == c.name && k.credential.scope == c.scope)
                    .map(|i| old.remove(i).input)
                    .unwrap_or_else(|| cx.new(key_field::KeyField::new)),
            })
            .collect();
        if all && data.api_provider.is_none() && !self.prefer_providers {
            self.goto(Page::Accounts, cx)
        }
        let message = data.issues.join("\n");
        self.error = !message.is_empty();
        self.message = message;
        self.settings = data;
        cx.notify()
    }
    fn submit(
        &mut self,
        request: Request,
        after: Option<Page>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            // Star edits autosave; one made during a save is saved right after it.
            if self.page == Page::Favorites && matches!(request, Request::Edit { .. }) {
                self.resave = true;
            }
            return;
        }
        let page = self.page;
        let provider = (after == Some(Page::Credentials))
            .then(|| {
                let index = self.selected?;
                let provider = self.registry.as_ref()?.providers.get(index)?;
                Some(provider.id.read(cx).value().trim().to_owned())
            })
            .flatten();
        let link = self.link.clone();
        self.busy = true;
        self.message.clear();
        cx.notify();
        cx.spawn_in(w, async move |view, cx| {
            let (request, result) = cx.background_executor().spawn(async move {
                let result = link.apply(&request);
                (request, result)
            }).await;
            let _ = view.update_in(cx, |s, w, cx| {
                s.busy = false;
                let resave = std::mem::take(&mut s.resave) && s.page == Page::Favorites;
                let local = resave.then(|| s.favorites.clone());
                if !matches!(&result, Ok(Ok(_))) {
                    s.restore_key(request, cx);
                }
                match result {
                    Ok(Ok(data)) => {
                        s.conflict = None;
                        s.install(data, Some(page), w, cx);
                        if let Some(next) = after {
                            s.credential_provider = provider;
                            s.goto(next, cx);
                        }
                        if page == Page::Credentials || page == Page::Providers {
                            s.tested_provider = None;
                        }
                        if !s.error {
                            let success = if page == Page::Credentials { "Credential updated." } else { "Changes saved." };
                            s.report(Ok(()), success, cx);
                        }
                    }
                    Ok(Err(revision)) => {
                        s.conflict = Some((page, revision));
                        s.report(Err("Changed on disk. Your draft is still here. Save again to apply your edits to the new revision, or discard the draft and reload.".into()), "", cx);
                    }
                    Err(message) => s.report(Err(message), "", cx),
                }
                if let Some(local) = local {
                    // Keep the stars changed during the save and save them on the new revision.
                    s.favorites = local;
                    s.save(w, cx);
                }
            });
        }).detach();
    }
    fn restore_key(&self, request: Request, cx: &mut Context<Self>) {
        if let Request::EditKey {
            name,
            scope,
            value: Some(value),
            ..
        } = request
            && let Some(field) = self
                .keys
                .iter()
                .find(|k| k.credential.name == name && k.credential.scope == scope)
        {
            field.input.update(cx, |f, cx| f.restore(value, cx));
        }
    }
    fn revision(&self, page: Page) -> String {
        if let Some((p, r)) = &self.conflict
            && *p == page
        {
            return r.clone();
        }
        match page {
            Page::Providers => &self.settings.revisions.registry,
            Page::Search => &self.search_revision,
            Page::Favorites => &self.settings.revisions.favorites,
            Page::Legacy => &self.settings.revisions.legacy,
            Page::Credentials => &self.settings.revisions.keys,
            Page::Hosts
            | Page::Remote
            | Page::Accounts
            | Page::Behavior
            | Page::Notifications
            | Page::Advanced
            | Page::Sessions
            | Page::Memory => {
                return String::new();
            }
        }
        .clone()
    }
    fn save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let draft = (|| -> Result<Edit, String> {
            Ok(match self.page {
                Page::Providers => self
                    .registry
                    .as_ref()
                    .ok_or("Could not read providers.")?
                    .edit(cx)?,
                Page::Search => Edit::Search {
                    fields: self
                        .registry
                        .as_ref()
                        .ok_or("Could not read search settings.")?
                        .search
                        .patch(&[SEARCH], false, cx)?,
                },
                Page::Legacy => {
                    let l = self.legacy.as_ref().ok_or("No legacy provider selected.")?;
                    let mut fields = l.fields.patch(&[LEGACY], false, cx)?;
                    let model = l.model.read(cx).value().trim().to_owned();
                    if model != l.initial_model {
                        fields.insert(Key::Model, (!model.is_empty()).then_some(Cell::Text(model)));
                    }
                    Edit::Legacy {
                        id: l.id.clone(),
                        fields,
                    }
                }
                Page::Favorites => Edit::Favorites {
                    models: self.favorites.clone(),
                },
                _ => return Err("Select a settings page.".into()),
            })
        })();
        match draft {
            Ok(edit) => {
                let next = (self.page == Page::Providers && self.selected.is_some())
                    .then_some(Page::Credentials);
                self.submit(
                    Request::Edit {
                        revision: self.revision(self.page),
                        edit,
                    },
                    next,
                    w,
                    cx,
                )
            }
            Err(e) => self.report(Err(e), "", cx),
        }
    }
    fn add_provider(
        &mut self,
        mut row: model::ProviderRow,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(registry) = self.registry.as_mut() else {
            return;
        };
        let base = row.id.clone();
        let mut n = 2;
        while registry
            .providers
            .iter()
            .any(|p| p.id.read(cx).value().as_ref() == row.id)
        {
            row.id = format!("{base}-{n}");
            n += 1;
        }
        if row.id != base {
            row.fields.insert(
                Key::CredentialEnv,
                Cell::Text(format!(
                    "{}_API_KEY",
                    row.id.to_uppercase().replace('-', "_")
                )),
            );
        }
        registry.providers.push(Provider::new(row, true, w, cx));
        self.selected = Some(registry.providers.len() - 1);
        self.model = None;
        self.section = Section::Connection;
        self.goto(Page::Providers, cx);
    }
    fn key(&mut self, i: usize, remove: bool, w: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(k) = self.keys.get(i) else { return };
        let req = Request::EditKey {
            revision: self.revision(Page::Credentials),
            name: k.credential.name.clone(),
            scope: k.credential.scope.clone(),
            value: (!remove).then(|| k.input.update(cx, |f, cx| f.take(cx))),
        };
        self.submit(req, None, w, cx)
    }
    fn test_provider(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(provider) = self.registry.as_ref().and_then(|r| r.providers.get(i)) else {
            return;
        };
        let Some(id) = provider.original.clone() else {
            self.report(Err("Save this provider before testing.".into()), "", cx);
            return;
        };
        if !matches!(provider.edit(cx), Ok(None)) {
            self.report(
                Err("Save your provider changes before testing.".into()),
                "",
                cx,
            );
            return;
        }
        let revision = self.settings.revisions.registry.clone();
        let link = self.link.clone();
        let continuing = self.credential_provider.as_ref() == Some(&id);
        self.busy = true;
        self.tested_provider = None;
        self.message.clear();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let (id, result, model) = cx
                .background_executor()
                .spawn(async move {
                    let result = link.request::<usize>(Request::TestProvider {
                        id: id.clone(),
                        revision,
                    });
                    let model = (continuing && result.as_ref().is_ok_and(|n| *n > 0))
                        .then(|| {
                            link.request::<Vec<String>>(Request::ProviderModels { id: id.clone() })
                                .ok()
                                .and_then(|models| models.into_iter().next())
                        })
                        .flatten();
                    (id, result, model)
                })
                .await;
            let _ = view.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok(n) => {
                        let models = if continuing && n > 0 && model.is_none() {
                            "Choose one of this provider's models from the workbench.".to_owned()
                        } else if n == 1 {
                            "1 coding model available.".to_owned()
                        } else {
                            format!("{n} coding models available.")
                        };
                        if continuing && n > 0 {
                            s.tested_provider = Some((id, model));
                        }
                        s.report(Ok(()), &format!("Connection succeeded. {models}"), cx);
                    }
                    Err(e) => s.report(Err(e), "", cx),
                }
            });
        })
        .detach();
    }
    fn fetch_models(&mut self, cx: &mut Context<Self>) {
        if self.loading_models {
            return;
        }
        self.loading_models = true;
        let link = self.link.clone();
        cx.spawn(async move |s, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { link.models() })
                .await;
            let _ = s.update(cx, |s, cx| {
                s.loading_models = false;
                match result {
                    Ok(models) => {
                        s.settings.models =
                            ox_desk_core::ports::ox::merge_catalog(&s.settings.models, models);
                        s.report(Ok(()), "Model catalog updated.", cx)
                    }
                    Err(e) => s.report(Err(e), "", cx),
                }
            });
        })
        .detach();
        cx.notify();
    }
    fn account_view(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let card = |title: &str, sub: &str| {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .border_1()
                .border_color(rgb(p.line))
                .rounded_lg()
                .bg(rgb(p.panel))
                .child(div().text_size(p.text(Step::Title)).child(title.to_owned()))
                .child(
                    div()
                        .text_size(p.text(Step::Ui))
                        .text_color(rgb(p.muted))
                        .child(sub.to_owned()),
                )
        };
        let codex = match self.settings.chatgpt_account.as_deref() {
            Some("configured_keyring") => "Codex checks your system keyring when a session opens.",
            Some(_) => "A native Codex sign-in is saved on this computer.",
            None => "Use your ChatGPT plan with stock Codex.",
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(card("ChatGPT", codex).child(div().child(crate::kit::button(
                p,
                "chatgpt-sign-in",
                "Sign in with ChatGPT",
                true,
                true,
                cx.listener(|s, _, _, cx| {
                    let result = s.link.remote().map_or_else(
                        || platform::launch_chatgpt_login(&s.project),
                        |h| {
                            crate::terminal::open_remote(
                                h.clone(),
                                ox_host_web::terminal_protocol::OpenTerminal::CodexLogin,
                                cx,
                            )
                        },
                    );
                    s.report(
                        result,
                        "Complete sign-in in the terminal, then return to your session.",
                        cx,
                    );
                }),
            ))))
            .child(
                card(
                    "API provider",
                    "Choose a provider and add its key to use API models in Codex.",
                )
                .child(div().child(crate::kit::button(
                    p,
                    "setup-api",
                    "Configure provider",
                    false,
                    true,
                    cx.listener(|s, _, _, cx| {
                        s.selected = None;
                        s.goto(Page::Providers, cx)
                    }),
                ))),
            )
            .child(
                card(
                    "Claude Code",
                    if self.settings.claude_account {
                        "A native Claude sign-in is saved on this computer."
                    } else {
                        "Use your Claude subscription in the installed Claude Code terminal."
                    },
                )
                .child(div().child(crate::kit::button(
                    p,
                    "claude-sign-in",
                    "Sign in with Claude",
                    false,
                    true,
                    cx.listener(|s, _, _, cx| {
                        let result = s.link.remote().map_or_else(
                            || platform::launch_claude_login(&s.project),
                            |h| {
                                crate::terminal::open_remote(
                                    h.clone(),
                                    ox_host_web::terminal_protocol::OpenTerminal::ClaudeLogin,
                                    cx,
                                )
                            },
                        );
                        s.report(result, "Complete Claude Code sign-in in the terminal.", cx);
                    }),
                ))),
            )
            .child(
                card(
                    "Antigravity",
                    "Use your Google account with the official Antigravity CLI.",
                )
                .child(div().child(crate::kit::button(
                    p,
                    "antigravity-sign-in",
                    "Sign in with Google",
                    false,
                    true,
                    cx.listener(|s, _, _, cx| {
                        let result = s.link.remote().map_or_else(
                            || {
                                platform::launch_login(
                                    ox_desk_core::runtime::Runtime::Antigravity,
                                    &s.project,
                                )
                            },
                            |h| {
                                crate::terminal::open_remote(
                                    h.clone(),
                                    ox_host_web::terminal_protocol::OpenTerminal::AntigravityLogin,
                                    cx,
                                )
                            },
                        );
                        s.report(result, "Complete Antigravity sign-in in the terminal.", cx);
                    }),
                ))),
            )
            .child(self.agents_card(p, cx))
            .when(self.link.remote().is_none(), |v| {
                v.child(self.about.clone())
            })
    }
    fn providers_view(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let mut view = div().flex().flex_col().gap_4();
        let Some(registry) = &self.registry else {
            return view.child("The provider file could not be loaded.");
        };
        if let Some(i) = self.selected
            && let Some(provider) = registry.providers.get(i)
        {
            let mut sections = div().flex().gap_2().flex_wrap();
            for (s, title) in [
                (Section::Connection, "Connection"),
                (Section::Options, "Adapter options"),
                (Section::Models, "Models"),
            ] {
                if s == Section::Options && provider.wire_fields(cx).is_empty() {
                    continue;
                }
                sections = sections.child(crate::kit::button(
                    p,
                    title,
                    title,
                    self.section == s,
                    true,
                    cx.listener(move |v, _, _, cx| {
                        v.section = s;
                        v.scroll.set_offset(point(px(0.), px(0.)));
                        cx.notify()
                    }),
                ));
            }
            view = view
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(crate::kit::button(
                            p,
                            "all-providers",
                            "‹  All providers",
                            false,
                            true,
                            cx.listener(|v, _, _, cx| {
                                v.selected = None;
                                v.model = None;
                                cx.notify()
                            }),
                        ))
                        .child(crate::kit::button(
                            p,
                            "save-provider",
                            "Save provider",
                            true,
                            true,
                            cx.listener(|s, _, w, cx| s.save(w, cx)),
                        )),
                )
                .child(sections)
                .child(div().child(crate::kit::button(
                    p,
                    "test-provider",
                    "Test saved connection",
                    false,
                    provider.original.is_some() && matches!(provider.edit(cx), Ok(None)),
                    cx.listener(move |s, _, _, cx| s.test_provider(i, cx)),
                )));
            match self.section {
                Section::Connection => {
                    view = view
                        .child(input_row("Provider ID", &provider.id, p))
                        .child(provider.fields.render(CONNECTION, "provider", p, cx))
                }
                Section::Options => {
                    view = view.child(provider.fields.render(
                        provider.wire_fields(cx),
                        "adapter",
                        p,
                        cx,
                    ))
                }
                Section::Models => {
                    if let Some(m) = self.model
                        && let Some(model) = provider.models.get(m)
                    {
                        return view
                            .child(crate::kit::button(
                                p,
                                "all-models",
                                "‹  Model list",
                                false,
                                true,
                                cx.listener(|s, _, _, cx| {
                                    s.model = None;
                                    cx.notify()
                                }),
                            ))
                            .child(model.render(MODEL, "model", p, cx))
                            .child(div().child(crate::kit::button(
                                p,
                                "remove-model",
                                "Remove model from draft",
                                false,
                                true,
                                cx.listener(move |s, _, _, cx| {
                                    if let Some(r) = s.registry.as_mut() {
                                        r.providers[i].models.remove(m);
                                    }
                                    s.model = None;
                                    cx.notify()
                                }),
                            )));
                    }
                    for (m, model) in provider.models.iter().enumerate() {
                        let name = model.value(Key::Id, cx);
                        view = view.child(crate::kit::button(
                            p,
                            ("model", m),
                            if name.is_empty() {
                                "Untitled model".into()
                            } else {
                                name
                            },
                            false,
                            true,
                            cx.listener(move |s, _, _, cx| {
                                s.model = Some(m);
                                cx.notify()
                            }),
                        ));
                    }
                    view = view.child(div().child(crate::kit::button(
                        p,
                        "add-model",
                        "Add model",
                        false,
                        true,
                        cx.listener(move |s, _, w, cx| {
                            let models = &mut s.registry.as_mut().unwrap().providers[i].models;
                            models.push(Fields::new(Values::new(), &[MODEL], w, cx));
                            s.model = Some(models.len() - 1);
                            cx.notify();
                        }),
                    )));
                }
            }
            view.child(div().child(crate::kit::button(
                p,
                "remove-provider",
                "Remove provider from draft",
                false,
                true,
                cx.listener(move |s, _, _, cx| {
                    s.registry.as_mut().unwrap().providers.remove(i);
                    s.selected = None;
                    s.model = None;
                    s.report(
                        Ok(()),
                        "Provider removed from draft. Save changes to apply.",
                        cx,
                    );
                }),
            )))
        } else {
            if !registry.providers.is_empty() {
                view = view.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(p.text(Step::Title))
                                .child("Configured providers"),
                        )
                        .child(crate::kit::button(
                            p,
                            "save-routes",
                            "Save changes",
                            true,
                            true,
                            cx.listener(|s, _, w, cx| s.save(w, cx)),
                        )),
                );
            } else if !self.settings.revisions.registry.is_empty() {
                view = view.child(div().child(crate::kit::button(
                    p,
                    "save-empty",
                    "Save changes",
                    true,
                    true,
                    cx.listener(|s, _, w, cx| s.save(w, cx)),
                )));
            }
            for (i, provider) in registry.providers.iter().enumerate() {
                let id = provider.id.read(cx).value().to_string();
                let title = provider.fields.value(Key::Name, cx);
                view = view.child(
                    div()
                        .id(("provider", i))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(p.line))
                        .bg(rgb(p.panel))
                        .p_3()
                        .cursor_pointer()
                        .hover(move |s| s.bg(rgb(p.hover)))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(if title.is_empty() {
                            id
                        } else {
                            format!("{title}  ·  {id}")
                        })
                        .child(
                            div()
                                .text_size(p.text(Step::Ui))
                                .text_color(rgb(p.muted))
                                .child("Edit  ›"),
                        )
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.selected = Some(i);
                            s.section = Section::Connection;
                            s.model = None;
                            cx.notify()
                        })),
                );
            }
            let mut presets = div().flex().flex_wrap().gap_2();
            for (i, (name, template)) in self.settings.presets.iter().enumerate() {
                let template = template.clone();
                presets = presets.child(crate::kit::button(
                    p,
                    ("new-provider", i),
                    name.clone(),
                    false,
                    true,
                    cx.listener(move |s, _, w, cx| s.add_provider(template.clone(), w, cx)),
                ));
            }
            view.child(div().text_size(p.text(Step::Title)).child("Add provider"))
                .child(presets)
        }
    }
    fn credentials_view(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let mut view = div().flex().flex_col().gap_3();
        if let Some(id) = &self.credential_provider
            && let Some(index) = self.registry.as_ref().and_then(|registry| {
                registry
                    .providers
                    .iter()
                    .position(|provider| provider.original.as_ref() == Some(id))
            })
        {
            let host = self
                .link
                .remote()
                .map_or("this computer", |host| host.name.as_str());
            view = view.child(
                crate::kit::col()
                    .gap_2()
                    .p_4()
                    .bg(rgb(p.panel))
                    .child(
                        div()
                            .text_size(p.text(Step::Title))
                            .child(format!("Connect {id} on {host}")),
                    )
                    .child(
                        "Save the provider's API key below, then check that its catalog responds.",
                    )
                    .child(
                        crate::kit::row()
                            .gap_2()
                            .flex_wrap()
                            .child(crate::kit::button(
                                p,
                                "credential-test-provider",
                                "Test connection",
                                false,
                                true,
                                cx.listener(move |s, _, _, cx| s.test_provider(index, cx)),
                            ))
                            .when_some(
                                self.tested_provider
                                    .as_ref()
                                    .filter(|(tested, _)| tested == id),
                                |row, (_, model)| {
                                    let label = if model.is_some() {
                                        "Continue to session"
                                    } else {
                                        "Back to workbench"
                                    };
                                    let model = model.clone();
                                    row.child(crate::kit::button(
                                        p,
                                        "provider-continue",
                                        label,
                                        true,
                                        true,
                                        cx.listener(move |_, _, _, cx| {
                                            cx.emit(ContinueSetup(model.clone()))
                                        }),
                                    ))
                                },
                            ),
                    ),
            );
        }
        if self.keys.is_empty() {
            return view
                .child("Save a provider connection to add its API key.")
                .child(div().child(crate::kit::button(
                    p,
                    "configure-keys",
                    "Add provider",
                    true,
                    true,
                    cx.listener(|s, _, _, cx| s.goto(Page::Providers, cx)),
                )));
        }
        for (i, k) in self.keys.iter().enumerate() {
            let c = &k.credential;
            let status = match (c.configured, c.environment, c.saved) {
                (false, _, true) => "Saved · configure this endpoint to use the key",
                (false, _, false) => "Inactive destination",
                (_, true, true) => "Environment key overrides saved key",
                (_, true, false) => "Using environment key",
                (_, false, true) => "Saved locally",
                _ => "Key required",
            };
            let mut row = div().flex().items_center().flex_wrap().gap_2();
            if c.configured {
                row = row
                    .child(div().min_w(px(160.)).flex_1().child(k.input.clone()))
                    .child(crate::kit::button(
                        p,
                        ("key-save", i),
                        "Save key",
                        true,
                        true,
                        cx.listener(move |s, _, w, cx| s.key(i, false, w, cx)),
                    ));
            }
            if c.saved {
                row = row.child(crate::kit::button(
                    p,
                    ("key-remove", i),
                    "Remove",
                    false,
                    true,
                    cx.listener(move |s, _, w, cx| s.key(i, true, w, cx)),
                ));
            }
            view = view.child(
                div()
                    .p_4()
                    .bg(rgb(p.panel))
                    .rounded_lg()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(c.name.clone())
                            .child(
                                div()
                                    .text_size(p.text(Step::Ui))
                                    .text_color(rgb(p.muted))
                                    .child(status),
                            ),
                    )
                    .child(
                        div()
                            .text_size(p.text(Step::Ui))
                            .text_color(rgb(p.muted))
                            .child(c.scope.clone()),
                    )
                    .child(row),
            );
        }
        view
    }
    fn favorites_view(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let mut view = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child("Starred models"),
            )
            .child(
                div()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(p.muted))
                    .child("Starred models appear first; any model can be used."),
            );
        for (i, alias) in self.favorites.iter().enumerate() {
            let mut actions = div().flex().gap_1();
            if i > 0 {
                actions = actions.child(crate::kit::button(
                    p,
                    ("up", i),
                    "↑",
                    false,
                    true,
                    cx.listener(move |s, _, w, cx| {
                        s.favorites.swap(i, i - 1);
                        s.save(w, cx)
                    }),
                ));
            }
            if i + 1 < self.favorites.len() {
                actions = actions.child(crate::kit::button(
                    p,
                    ("down", i),
                    "↓",
                    false,
                    true,
                    cx.listener(move |s, _, w, cx| {
                        s.favorites.swap(i, i + 1);
                        s.save(w, cx)
                    }),
                ));
            }
            actions = actions.child(crate::kit::button(
                p,
                ("unstar", i),
                "Remove",
                false,
                true,
                cx.listener(move |s, _, w, cx| {
                    s.favorites.remove(i);
                    s.save(w, cx)
                }),
            ));
            view = view.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(alias.clone())
                    .child(actions),
            );
        }
        view = view.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_1().child(crate::kit::input(&self.alias)))
                .child(crate::kit::button(
                    p,
                    "add-favorite",
                    "Add alias",
                    false,
                    true,
                    cx.listener(|s, _, w, cx| {
                        let alias = s.alias.read(cx).value().trim().to_owned();
                        if alias.is_empty() {
                            s.report(Err("Enter a model alias.".into()), "", cx);
                            return;
                        }
                        if !s.favorites.contains(&alias) {
                            s.favorites.push(alias);
                        }
                        s.alias.update(cx, |field, cx| field.set_value("", w, cx));
                        s.save(w, cx);
                    }),
                )),
        );
        view = view.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child("Available models")
                .child(crate::kit::button(
                    p,
                    "refresh-catalog",
                    if self.loading_models {
                        "Loading…"
                    } else {
                        "Refresh catalog"
                    },
                    false,
                    true,
                    cx.listener(|s, _, _, cx| s.fetch_models(cx)),
                )),
        );
        let filter = self.alias.read(cx).value().to_lowercase();
        // The entry is both a filter and a way to add valid models absent from a catalog.
        for (i, alias) in self
            .settings
            .models
            .iter()
            .enumerate()
            .filter(|(_, m)| m.to_lowercase().contains(&filter) && !self.favorites.contains(m))
            .take(50)
        {
            let alias = alias.clone();
            view = view.child(crate::kit::button(
                p,
                ("star-model", i),
                format!("＋  {alias}"),
                false,
                true,
                cx.listener(move |s, _, w, cx| {
                    if !s.favorites.contains(&alias) {
                        s.favorites.push(alias.clone())
                    }
                    s.save(w, cx)
                }),
            ));
        }
        view
    }
}

fn input_row(label: &'static str, input: &Entity<InputState>, p: Palette) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(162.))
                .text_size(p.text(Step::Ui))
                .text_color(rgb(p.muted))
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(crate::kit::input(input)))
}
impl Preferences {
    /// Test-facing state of the open settings page for the desk stats probe.
    pub fn probe_state(&self, cx: &App) -> crate::platform::layout_probe::SettingsProbe {
        crate::platform::layout_probe::SettingsProbe {
            page: format!("{:?}", self.page),
            remote: self.remote.as_ref().map(|r| r.read(cx).probe_state()),
        }
    }
}

impl Render for Preferences {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::app::guard(cx, |cx| self.draw(cx).into_any_element())
    }
}

impl Preferences {
    fn draw(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::get(cx);
        let mut header = div()
            .p_5()
            .pb_3()
            .flex()
            .flex_col()
            .gap_3()
            .border_b_1()
            .border_color(rgb(p.line))
            .child(
                div().text_size(p.text(Step::Display)).child(
                    self.link
                        .remote()
                        .map_or_else(|| "Settings".into(), |h| format!("Settings · {}", h.name)),
                ),
            );
        let mut tabs = div().flex().flex_wrap().gap_1();
        for (page, title) in [
            (Page::Accounts, "Accounts"),
            (Page::Hosts, "Hosts"),
            (Page::Remote, "Remote access"),
            (Page::Providers, "Providers & models"),
            (Page::Credentials, "API keys"),
            (Page::Search, "Web search"),
            (Page::Favorites, "Starred models"),
            (Page::Behavior, "Appearance & input"),
            (Page::Notifications, "Notifications"),
            (Page::Sessions, "Sessions"),
            (Page::Legacy, "Legacy Codex provider"),
            (Page::Advanced, "Advanced configuration"),
            (Page::Memory, "Memory"),
        ] {
            if self.link.remote().is_some()
                && matches!(
                    page,
                    Page::Behavior | Page::Sessions | Page::Remote | Page::Memory
                )
            {
                continue;
            }
            if page == Page::Legacy && self.legacy.is_none() {
                continue;
            }
            tabs = tabs.child(crate::kit::button(
                p,
                title,
                title,
                page == self.page,
                true,
                cx.listener(move |s, _, w, cx| {
                    if page == Page::Advanced {
                        s.open_advanced(w, cx)
                    } else if page == Page::Memory {
                        s.open_memory(w, cx)
                    } else {
                        s.goto(page, cx)
                    }
                }),
            ));
        }
        header = header.child(tabs);
        if !self.message.is_empty() {
            header = header.child(
                div()
                    .text_size(p.text(Step::Ui))
                    .text_color(rgb(if self.error { p.error } else { p.muted }))
                    .child(self.message.clone())
                    .when(self.error, |notice| {
                        notice.child(crate::kit::copy_message(
                            p,
                            "copy-settings-error",
                            &self.message,
                        ))
                    }),
            )
        }
        let content = if self.busy {
            div().child("Loading settings…")
        } else {
            match self.page {
                Page::Hosts => div().child(self.hosts.clone()),
                Page::Remote => div().children(self.remote.clone()),
                Page::Accounts => self.account_view(p, cx),
                Page::Advanced => self.advanced_page(p, cx),
                Page::Behavior => self.behavior_page(p, cx),
                Page::Notifications => self.notification_page(p, cx),
                Page::Sessions => self.sessions_page(p, cx),
                Page::Memory => div().children(self.memory.clone()),
                Page::Providers => self.providers_view(p, cx),
                Page::Credentials => self.credentials_view(p, cx),
                Page::Favorites => self.favorites_view(p, cx),
                Page::Search => {
                    let mut view = div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(div().text_size(p.text(Step::Title)).child("Web search"));
                    if let Some(registry) = &self.registry {
                        view = view
                            .child(registry.search.render(SEARCH, "brave", p, cx))
                            .child(div().child(crate::kit::button(
                                p,
                                "save-search",
                                "Save search settings",
                                true,
                                true,
                                cx.listener(|s, _, w, cx| s.save(w, cx)),
                            )))
                    }
                    view
                }
                Page::Legacy => {
                    let mut view = div().flex().flex_col().gap_4();
                    if let Some(l) = &self.legacy {
                        view = view
                            .child(
                                div()
                                    .text_size(p.text(Step::Title))
                                    .child(format!("Legacy · {}", l.id)),
                            )
                            .child(input_row("Default model", &l.model, p))
                            .child(l.fields.render(LEGACY, "legacy", p, cx))
                            .child(div().child(crate::kit::button(
                                p,
                                "save-legacy",
                                "Save connection",
                                true,
                                true,
                                cx.listener(|s, _, w, cx| s.save(w, cx)),
                            )));
                    }
                    view
                }
            }
        };
        let mut body = div()
            .id("settings-body")
            .track_scroll(&self.scroll)
            .overflow_y_scroll()
            .min_h_0()
            .flex_1()
            .p_5()
            .child(content);
        if self.error && !self.busy && self.page != Page::Advanced {
            body = body.child(div().mt_4().child(crate::kit::button(
                p,
                "reload-settings",
                "Discard drafts and reload",
                false,
                true,
                cx.listener(|s, _, w, cx| s.reload(w, cx)),
            )))
        }
        div()
            .track_focus(&self.focus)
            .size_full()
            .bg(rgb(p.bg))
            .text_color(rgb(p.fg))
            .flex()
            .flex_col()
            .child(header)
            .child(body)
    }
}
