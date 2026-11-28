//! Use the CLI's storage, lock, parser, and catalog for edits made in OX Desktop.
mod advanced;
mod legacy;
mod rows;
mod transfer;
use crate::provider::{
    CodeoxConfig, MAX_CONFIG_BYTES, RelayError, SavedKeys, alias_for, process_env,
};
use crate::settings_protocol::{Credential, Document, Request, Settings};
use crate::{App, BundledModels, MAX_REQUEST_BYTES, picker, setup, upstream_client};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::Path,
};

const REGISTRY: &str = "codeox.toml";
const FAVORITES: &str = "ox-favorites.json";

pub(crate) async fn run(home: &Path) -> Result<u8, RelayError> {
    // Advanced saves carry both the base and the edited document. JSON escaping
    // can take six bytes per input character, plus the request envelope.
    let limit = 13 * MAX_CONFIG_BYTES;
    let mut input = std::io::stdin().lock().take(limit + 1);
    let request = serde_json::from_reader(&mut input);
    if input.limit() == 0 {
        return Err("Settings request is too large.".into());
    }
    let request = request.map_err(|_| "Invalid desktop settings request.")?;
    let response = handle(home, request).await?;
    serde_json::to_writer(std::io::stdout().lock(), &response)?;
    std::io::stdout().flush()?;
    Ok(0)
}

async fn handle(home: &Path, request: Request) -> Result<Value, RelayError> {
    match request {
        Request::CodexProfiles => Ok(serde_json::to_value(crate::clients::codex_profiles(home)?)?),
        Request::ClaudeProfiles => Ok(serde_json::to_value(crate::clients::claude_profiles(
            home,
        )?)?),
        Request::ReadAdvanced => {
            let _lock = setup::lock_config(home)?;
            Ok(serde_json::to_value(advanced::documents(home)?)?)
        }
        Request::SaveAdvanced {
            id,
            revision,
            original,
            text,
        } => {
            let _lock = setup::lock_config(home)?;
            Ok(serde_json::to_value(advanced::save(
                home, &id, &revision, &original, &text,
            )?)?)
        }
        Request::ReadRows => {
            let _lock = setup::lock_config(home)?;
            Ok(serde_json::to_value(rows::read(home)?)?)
        }
        Request::Edit { revision, edit } => {
            let _lock = setup::lock_config(home)?;
            Ok(serde_json::to_value(rows::apply(home, &revision, edit)?)?)
        }
        Request::TestProvider { id, revision } => {
            Ok(json!(rows::test_provider(home, &id, &revision).await?))
        }
        Request::EditKey {
            revision,
            name,
            scope,
            value,
        } => {
            let _lock = setup::lock_config(home)?;
            let current = rows::key_revision(home)?;
            let outcome = if revision == current {
                save_key(
                    home,
                    name,
                    scope,
                    value.map(crate::settings_protocol::model::Secret::into_string),
                )?;
                crate::settings_protocol::model::Outcome::Applied
            } else {
                crate::settings_protocol::model::Outcome::ChangedOnDisk { revision: current }
            };
            Ok(serde_json::to_value(outcome)?)
        }
        Request::ExportKeys { revision, selected } => {
            let _lock = setup::lock_config(home)?;
            Ok(serde_json::to_value(transfer::export(
                home, &revision, selected,
            )?)?)
        }
        Request::ImportKeys { revision, keys } => {
            let _lock = setup::lock_config(home)?;
            Ok(serde_json::to_value(transfer::import(
                home, &revision, keys,
            )?)?)
        }
        Request::Read => Ok(serde_json::to_value(snapshot(home)?.0)?),
        Request::LegacyWorkspace => {
            let layout = setup::read_optional_text_limited(
                &home.join("ox-desktop-workspace.json"),
                "previous desktop workspace",
            )?
            .map(|text| serde_json::from_str::<crate::settings_protocol::ProjectRoute>(&text))
            .transpose()
            .map_err(|_| "Invalid previous desktop workspace")?;
            Ok(serde_json::to_value(layout)?)
        }
        request @ (Request::Models | Request::ProviderModels { .. }) => {
            let provider = match &request {
                Request::ProviderModels { id } => Some(id.as_str()),
                _ => None,
            };
            if !home.join(REGISTRY).try_exists()? {
                return if provider.is_none() {
                    legacy::models(home).await
                } else {
                    Ok(json!([]))
                };
            }
            let config = CodeoxConfig::load(&home.join(REGISTRY))?;
            let keys = SavedKeys::for_config(home, &config)?;
            let app = App::codeox(
                config,
                BundledModels::default(),
                &crate::random_token()?,
                upstream_client()?,
                MAX_REQUEST_BYTES,
                &|name| keys.get(name),
                None,
                crate::Catalog::Shared,
            )
            .await?;
            let mut models: Vec<_> = app
                .launch_models()
                .into_iter()
                .filter(|(_, owner)| provider.is_none_or(|provider| *owner == provider))
                .map(|(alias, _)| alias.to_string())
                .collect();
            prioritize(&mut models, home);
            Ok(json!(models))
        }
        Request::ModelContexts => {
            if !home.join(REGISTRY).try_exists()? {
                return Ok(json!({}));
            }
            let config = CodeoxConfig::load(&home.join(REGISTRY))?;
            let keys = SavedKeys::for_config(home, &config)?;
            let app = App::codeox(
                config,
                BundledModels::default(),
                &crate::random_token()?,
                upstream_client()?,
                MAX_REQUEST_BYTES,
                &|name| keys.get(name),
                None,
                crate::Catalog::Shared,
            )
            .await?;
            Ok(json!(
                app.model_contexts()
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>()
            ))
        }
        Request::Save { id, revision, text } => {
            let _lock = setup::lock_config(home)?;
            if id == legacy::ID {
                legacy::save(home, &revision, &text)?;
                return Ok(Value::Null);
            }
            let current = document(home, &id)?;
            if current.revision != revision {
                return Err("This file changed on disk. Reload it before saving.".into());
            }
            save_document(home, &id, &text)?;
            Ok(Value::Null)
        }
        Request::Key { name, scope, value } => {
            let _lock = setup::lock_config(home)?;
            save_key(home, name, scope, value)?;
            Ok(Value::Null)
        }
    }
}

fn document(home: &Path, id: &str) -> Result<Document, RelayError> {
    let (title, starter) = match id {
        REGISTRY => ("Providers & search", ""),
        FAVORITES => ("Favorites", "[]"),
        _ => return Err("Unknown OX settings file.".into()),
    };
    let path = home.join(id);
    if let Ok(meta) = path.symlink_metadata()
        && !meta.is_file()
    {
        return Err(format!("{id} must be a regular file to edit in OX Desktop.").into());
    }
    let raw = setup::read_optional_text_limited(&path, id)?;
    let revision = raw
        .as_ref()
        .map(|text| {
            hex::encode(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref())
        })
        .unwrap_or_default();
    Ok(Document {
        id: id.into(),
        title: title.into(),
        text: raw.unwrap_or_else(|| starter.into()),
        revision,
    })
}

fn snapshot(home: &Path) -> Result<(Settings, Option<String>), RelayError> {
    let registry = document(home, REGISTRY)?;
    let favorites = document(home, FAVORITES)?;
    let keys = SavedKeys::load(home)?;
    let mut scopes: BTreeSet<(String, String)> =
        keys.scopes().map(|(n, s)| (n.into(), s.into())).collect();
    let mut models = Vec::new();
    // Read account and credential status even if the registry needs repair.
    let config = (!registry.revision.is_empty())
        .then(|| CodeoxConfig::load_from_str(&registry.text).ok())
        .flatten();
    if let Some(config) = &config {
        let stored_favorites = picker::read_favorites(home).unwrap_or_default();
        for vendor in &config.vendors {
            if let Some(name) = &vendor.env_key {
                scopes.insert((name.clone(), vendor.base_url.to_string()));
                if process_env(name).is_some()
                    || keys.scopes().any(|(n, s)| {
                        n == name
                            && s == vendor.base_url.as_str()
                            && SavedKeys::scope_matches(config, n, s)
                    })
                {
                    for model in &vendor.models {
                        models.push(alias_for(&vendor.key, &model.id, model.alias.as_deref())?);
                    }
                    if vendor.discovery {
                        // A starred discovered model stays selectable between catalog refreshes.
                        for alias in &stored_favorites {
                            if let Some((key, id)) = alias.split_once('/')
                                && key == vendor.key
                                && alias_for(key, id, None).is_ok()
                            {
                                models.push(alias.clone());
                            }
                        }
                    }
                }
            }
        }
        if let Some(search) = &config.search {
            scopes.insert((search.env_key.clone(), SavedKeys::BRAVE_SCOPE.into()));
        }
    }
    let legacy_key = if registry.revision.is_empty() {
        legacy::key_destination(home).ok().flatten()
    } else {
        None
    };
    scopes.extend(legacy_key.clone());
    let credentials = scopes
        .into_iter()
        .map(|(name, scope)| Credential {
            saved: keys.scopes().any(|(n, s)| n == name && s == scope),
            environment: process_env(&name).is_some(),
            configured: legacy_key
                .as_ref()
                .is_some_and(|(n, s)| n == &name && s == &scope)
                || config
                    .as_ref()
                    .is_some_and(|c| SavedKeys::scope_matches(c, &name, &scope)),
            name,
            scope,
        })
        .collect();
    prioritize(&mut models, home);
    let mut documents = vec![registry, favorites];
    let mut api_provider =
        (!documents[0].revision.is_empty()).then(|| crate::provider::PROVIDER_ID.to_owned());
    let legacy = legacy::projection(home).unwrap_or(None);
    let legacy_id = legacy.as_ref().map(|(_, id, _)| id.clone());
    if let Some((doc, id, model)) = legacy {
        if api_provider.is_none() {
            api_provider = Some(id);
            models.extend(model);
        }
        documents.push(doc);
    }
    Ok((
        Settings {
            version: crate::settings_protocol::VERSION,
            presets: setup::desktop_presets(),
            chatgpt_account: crate::native::saved_subscription(
                crate::clients::ClientKind::Codex,
                home,
            )
            .map(str::to_owned),
            claude_account: crate::native::has_saved_account(
                crate::clients::ClientKind::Claude,
                home,
            ),
            api_provider,
            documents,
            credentials,
            models,
        },
        legacy_id,
    ))
}

fn prioritize(models: &mut Vec<String>, home: &Path) {
    let favorites = picker::read_favorites(home).unwrap_or_default();
    let ranks: std::collections::HashMap<_, _> = favorites
        .iter()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    models.sort_unstable_by(|a, b| {
        ranks
            .get(a.as_str())
            .unwrap_or(&usize::MAX)
            .cmp(ranks.get(b.as_str()).unwrap_or(&usize::MAX))
            .then(a.cmp(b))
    });
    models.dedup();
}

fn save_document(home: &Path, id: &str, text: &str) -> Result<(), RelayError> {
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err("The settings file exceeds the 1 MiB limit.".into());
    }
    match id {
        REGISTRY if text.trim().is_empty() => {
            if home.join(REGISTRY).try_exists()? {
                fs::remove_file(home.join(REGISTRY))?;
            }
            return Ok(());
        }
        REGISTRY => setup::check_alias_collisions(&CodeoxConfig::load_from_str(text)?)?,
        FAVORITES => {
            let values: Vec<String> = serde_json::from_str(text)
                .map_err(|_| "Favorites must be a JSON array of model aliases.")?;
            if values
                .iter()
                .any(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
            {
                return Err("Favorites must contain valid model aliases.".into());
            }
        }
        _ => unreachable!(),
    }
    setup::write_config_atomic(&home.join(id), text)?;
    Ok(())
}
fn save_key(
    home: &Path,
    name: String,
    scope: String,
    value: Option<String>,
) -> Result<(), RelayError> {
    if let Some(value) = value {
        if value.is_empty() || value.len() > 8192 || !value.bytes().all(|ch| ch.is_ascii_graphic())
        {
            return Err("Enter an API key without spaces or line breaks.".into());
        }
        if home.join(REGISTRY).try_exists()? {
            let config = CodeoxConfig::load(&home.join(REGISTRY))?;
            setup::save_key(home, &config, &name, &scope, value)?;
        } else {
            if legacy::key_destination(home)? != Some((name.clone(), scope.clone())) {
                return Err(
                    "Legacy provider credential destination changed. Reload settings.".into(),
                );
            }
            let encoded = SavedKeys::load(home)?.set(&name, &scope, value)?;
            setup::write_config_atomic(&SavedKeys::path(home), &encoded)?;
        }
    } else {
        let encoded = SavedKeys::load(home)?.remove(&name, &scope)?;
        setup::write_config_atomic(&SavedKeys::path(home), &encoded)?;
    }
    Ok(())
}
