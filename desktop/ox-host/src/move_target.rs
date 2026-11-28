//! Destination-owned checkout and transcript, verified before fencing the source.
use crate::{
    move_archive::Archive,
    move_bundle::{BundlePlan, Presentation, plan_digest, verified_file},
    move_ledger::{Ledger, Plan, Prepared, Proposal, Stage},
};
use ox_desk_core::{
    codex::history::ThreadRow,
    handoff::{self, HandoffSource, SelectionRefs, SourceHost, SourceItem},
    host_protocol::{Scope, SessionView},
    modes::Decision,
    runtime::Runtime,
};
use ox_host_service::checkout::receive_move;
use ox_host_web::move_manifest::{CHECKOUT, Manifest, PLAN, TRANSCRIPT};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    os::unix::ffi::OsStrExt,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

/// Received from the authenticated upload adapter. A receipt for these bytes
/// allows verification here, but confers no native write authority.
pub(crate) struct Files<'a> {
    pub(crate) directory: &'a Path,
    pub(crate) manifest: &'a Manifest,
    pub(crate) manifest_sha256: [u8; 32],
    pub(crate) expected_plan: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    bundle: BundlePlan,
    manifest_sha256: [u8; 32],
    transcript_sha256: [u8; 32],
    checkout_sha256: [u8; 32],
}

pub(crate) struct VerifiedTarget {
    directory: PathBuf,
    saved: Saved,
}

impl VerifiedTarget {
    pub(crate) fn reserve_plan(
        workspace: &Path,
        ledger: &Ledger,
        source: Uuid,
        target: Uuid,
        transfer: Uuid,
        proposal: Proposal,
    ) -> io::Result<Plan> {
        let relative = Path::new(OsStr::from_bytes(&proposal.project_in_checkout));
        if proposal.project_in_checkout.contains(&0)
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid(
                "The project must be inside its transferred checkout.",
            ));
        }
        let checkout_name = proposal
            .source
            .scope
            .project
            .ancestors()
            .nth(relative.components().count())
            .and_then(Path::file_name)
            .unwrap_or_else(|| OsStr::new("project"));
        let mut project = workspace
            .canonicalize()?
            .join(transfer.to_string())
            .join("checkout")
            .join(checkout_name);
        // The web projection keys scopes by their serialized path. Keep a
        // checkout-root move identical to the path the actor can canonicalize.
        project.extend(relative.components());
        let route = proposal.source.scope.route.clone();
        let plan = Plan {
            id: transfer,
            session: proposal.session,
            source: proposal.source,
            target_host: target,
            target_scope: Scope { project, route },
        };
        ledger.prepared_receiver(source, &plan)?;
        Ok(plan)
    }

    pub(crate) fn plan(&self) -> &Plan {
        &self.saved.bundle.plan
    }

    pub(crate) fn matches_prepared(&self, prepared: &Prepared) -> bool {
        prepared.plan == self.saved.bundle.plan
            && prepared.stage.digest == self.saved.manifest_sha256
    }

    pub(crate) fn check_activation(&self, prepared: &Prepared) -> io::Result<()> {
        if !self.matches_prepared(prepared) {
            return Err(invalid("The destination files belong to another move."));
        }
        self.check_project()?;
        self.history()?;
        Ok(())
    }

    /// Each move receives a fresh checkout in the destination state directory.
    /// The host actor serializes this step and reserves the scope in preflight.
    pub(crate) fn install(
        workspace: &Path,
        ledger: &Ledger,
        authenticated_source: Uuid,
        files: Files<'_>,
    ) -> io::Result<Self> {
        if files.manifest.sha256()? != files.manifest_sha256 {
            return Err(invalid("The files do not match the staged manifest."));
        }
        let bundle: BundlePlan = serde_json::from_reader(verified_file(
            &files.directory.join(PLAN),
            &files.manifest.plan,
        )?)?;
        let plan = &bundle.plan;
        if let Some(expected) = files.expected_plan
            && plan_digest(plan)? != expected
        {
            return Err(invalid("The source changed the approved session move."));
        }
        let prepared = ledger.prepared_receiver(authenticated_source, plan)?;
        if prepared
            .as_ref()
            .is_some_and(|receipt| receipt.stage.digest != files.manifest_sha256)
        {
            return Err(invalid(
                "The destination is already prepared for other files.",
            ));
        }
        let workspace = workspace.canonicalize()?;
        let directory = workspace.join(plan.id.to_string());
        let project = plan
            .target_scope
            .project
            .strip_prefix(directory.join("checkout"))
            .map_err(|_| invalid("The checkout is outside the reserved move directory."))?;
        if !project
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return Err(invalid("The destination checkout path is invalid."));
        }
        let checkout_name = project
            .components()
            .next()
            .ok_or_else(|| invalid("The destination checkout needs a folder name."))?
            .as_os_str()
            .to_owned();
        let saved = Saved {
            bundle,
            manifest_sha256: files.manifest_sha256,
            transcript_sha256: files.manifest.transcript.sha256,
            checkout_sha256: files.manifest.checkout.sha256,
        };
        let target = Self { directory, saved };
        match target.directory.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {
                let file = File::open(target.directory.join("receipt.json"))?;
                if serde_json::from_reader::<_, Saved>(file)? != target.saved {
                    return Err(invalid("This directory belongs to another move."));
                }
                target.history()?;
                target.check_project()?;
                File::open(workspace)?.sync_all()?;
                return Ok(target);
            }
            Ok(_) => return Err(invalid("The reserved move directory is already in use.")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if prepared.is_some() {
            return Err(invalid("The prepared destination files are missing."));
        }
        let stage = tempfile::Builder::new()
            .prefix(".receiving-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&workspace)?;
        let transcript = stage.path().join(TRANSCRIPT);
        let mut source = verified_file(
            &files.directory.join(TRANSCRIPT),
            &files.manifest.transcript,
        )?;
        let mut retained = File::create_new(&transcript)?;
        io::copy(&mut source, &mut retained)?;
        retained.sync_all()?;
        Archive::open(
            File::open(transcript)?,
            &target.saved.bundle.plan,
            target.saved.transcript_sha256,
        )?;

        let checkout = verified_file(&files.directory.join(CHECKOUT), &files.manifest.checkout)?;
        let staged_checkouts = stage.path().join("checkout");
        fs::create_dir(&staged_checkouts)?;
        let installed = receive_move(
            &staged_checkouts,
            1,
            checkout,
            &target.saved.bundle.checkout_commit,
        )?;
        let transient_checkout = staged_checkouts.join("1");
        let relative = installed
            .strip_prefix(&transient_checkout)
            .map_err(|_| invalid("The checkout is outside the move directory."))?;
        let fixed_checkout = staged_checkouts.join(&checkout_name);
        let destination_checkout = target.directory.join("checkout").join(checkout_name);
        if destination_checkout.join(relative) != target.saved.bundle.plan.target_scope.project {
            return Err(invalid(
                "The checkout project differs from the reserved destination.",
            ));
        }
        if transient_checkout != fixed_checkout {
            fs::rename(transient_checkout, &fixed_checkout)?;
        }
        sync_checkout(&fixed_checkout)?;
        File::open(staged_checkouts)?.sync_all()?;
        let receipt = File::create_new(stage.path().join("receipt.json"))?;
        serde_json::to_writer(&receipt, &target.saved)?;
        receipt.sync_all()?;
        File::open(stage.path())?.sync_all()?;
        fs::rename(stage.path(), &target.directory)?;
        File::open(workspace)?.sync_all()?;
        Ok(target)
    }

    pub(crate) fn load(workspace: &Path, plan: &Plan) -> io::Result<Self> {
        let directory = workspace.canonicalize()?.join(plan.id.to_string());
        let saved =
            serde_json::from_reader::<_, Saved>(File::open(directory.join("receipt.json"))?)?;
        if saved.bundle.plan != *plan {
            return Err(invalid("The archive belongs to another session move."));
        }
        Ok(Self { directory, saved })
    }

    fn history(&self) -> io::Result<Archive> {
        Archive::open(
            File::open(self.directory.join(TRANSCRIPT))?,
            &self.saved.bundle.plan,
            self.saved.transcript_sha256,
        )
    }

    /// Build the checkpoint input for a fresh Codex writer in the verified checkout.
    /// The bounded items are injected before the operator submits the next turn.
    pub(crate) fn codex_checkpoint(
        &self,
        window: Option<u64>,
    ) -> io::Result<Vec<serde_json::Value>> {
        let plan = &self.saved.bundle.plan;
        if plan.target_scope.route.runtime != Runtime::Codex {
            return Err(invalid("Codex context requires the verified Codex route."));
        }
        let recent = self.history()?.continuation_items()?;
        let selection =
            handoff::select_for_continuation(&recent, window).map_err(io::Error::other)?;
        let source = self.history_source(selection.refs.clone());
        Ok(handoff::codex_items(&source, &selection))
    }

    pub(crate) fn claude_request(
        &self,
        ledger: &Ledger,
        scope: &Scope,
        native: &str,
        request: &str,
    ) -> io::Result<String> {
        if scope.route.runtime != Runtime::Claude {
            return Err(invalid(
                "Claude context requires the verified Claude route.",
            ));
        }
        let items = self
            .history_for(ledger, scope, native)?
            .continuation_items()?;
        let selection = handoff::select(&items, request, None).map_err(io::Error::other)?;
        let source = self.history_source(selection.refs.clone());
        Ok(handoff::claude_turn(&source, &selection, request))
    }

    fn history_source(&self, selection: SelectionRefs) -> HandoffSource {
        let plan = self.plan();
        HandoffSource {
            history_broker: None,
            host: Some(SourceHost {
                id: plan.source.host.to_string(),
                name: plan.source.host.to_string(),
            }),
            scope: plan.source.scope.clone(),
            session: plan.session.to_string(),
            native: Some(plan.source.native.clone()),
            title: self.saved.bundle.presentation.title.clone(),
            runtime: plan.source.scope.route.runtime,
            route: plan.source.scope.route.clone(),
            source_access: None,
            source_level: None,
            decision: Decision::Inherited,
            selection,
        }
    }

    pub(crate) fn moved_history_for(
        &self,
        ledger: &Ledger,
        scope: &Scope,
        native: &str,
    ) -> io::Result<(HandoffSource, Vec<SourceItem>)> {
        let items = self
            .history_for(ledger, scope, native)?
            .continuation_items()?;
        let selection = handoff::select_for_continuation(&items, None).map_err(io::Error::other)?;
        Ok((self.history_source(selection.refs), items))
    }

    /// The session resolved by the destination actor must be the one that
    /// owns this exact continuation. A verified upload alone cannot read it.
    pub(crate) fn history_for(
        &self,
        ledger: &Ledger,
        scope: &Scope,
        native: &str,
    ) -> io::Result<Archive> {
        self.verify_owner(ledger, scope, native)?;
        self.history()
    }

    pub(crate) fn presentation_for(
        &self,
        ledger: &Ledger,
        scope: &Scope,
        native: &str,
    ) -> io::Result<&Presentation> {
        self.verify_owner(ledger, scope, native)?;
        Ok(&self.saved.bundle.presentation)
    }

    pub(crate) fn catalog_row_for(
        &self,
        ledger: &Ledger,
        scope: &Scope,
        native: &str,
    ) -> io::Result<SessionView> {
        let presentation = self.presentation_for(ledger, scope, native)?;
        let row = ThreadRow {
            worker: false,
            parent_thread_id: None,
            agent_nickname: None,
            id: native.into(),
            cwd: scope.project.clone(),
            model_provider: presentation.model_provider.clone(),
            model: None,
            name: Some(presentation.title.clone()),
            preview: String::new(),
            generated_title: None,
            updated_at: presentation.updated_at,
            status: None,
            unread: false,
            activity_order: 0,
        };
        Ok(SessionView {
            id: native.into(),
            title: presentation.title.clone(),
            parent: None,
            status: "idle".into(),
            updated_at: presentation.updated_at,
            capabilities: vec!["open".into()],
            detail: serde_json::json!({
                "row": row,
                "owned": true,
                "logical_session_id": self.plan().session,
                "last_result": presentation.last_result,
            }),
        })
    }

    fn verify_owner(&self, ledger: &Ledger, scope: &Scope, native: &str) -> io::Result<()> {
        let plan = &self.saved.bundle.plan;
        let allowed = ledger.owner(plan.session)?.is_some_and(|owner| {
            owner.host == plan.target_host
                && owner.generation == plan.source.generation + 1
                && owner.scope == *scope
                && owner.native == native
        });
        let verified = ledger
            .prepared_receiver(plan.source.host, plan)?
            .is_some_and(|receipt| receipt.stage.digest == self.saved.manifest_sha256);
        if !allowed || !verified {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "The move history belongs to another session or has not arrived.",
            ));
        }
        Ok(())
    }

    /// Call after reserving an idle native session in the verified checkout.
    /// Recording Prepared still keeps that native session fenced until activation.
    pub(crate) fn prepare(
        &self,
        ledger: &mut Ledger,
        authenticated_source: Uuid,
        native: String,
    ) -> io::Result<Prepared> {
        self.check_project()?;
        ledger.prepare_receiver(
            authenticated_source,
            &Prepared {
                plan: self.saved.bundle.plan.clone(),
                stage: Stage {
                    native,
                    digest: self.saved.manifest_sha256,
                },
            },
        )
    }

    fn check_project(&self) -> io::Result<()> {
        let project = &self.saved.bundle.plan.target_scope.project;
        if project.canonicalize()? == *project && project.is_dir() {
            return Ok(());
        }
        Err(invalid("The reserved destination checkout has moved."))
    }
}

// Keep directory entries as well as file contents durable before replying.
// The checkout may contain symlinks, which belong to the directory entry.
fn sync_checkout(path: &Path) -> io::Result<()> {
    let mut directories = vec![path.to_path_buf()];
    let mut seen = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() {
                File::open(entry.path())?.sync_all()?;
            }
        }
        seen.push(directory);
    }
    for directory in seen.into_iter().rev() {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
