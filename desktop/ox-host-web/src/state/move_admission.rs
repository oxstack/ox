//! One destination-approved plan over an existing authenticated host link.
use super::{State, read_registry, write_registry};
use crate::paired_hosts::valid_id;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    source: String,
    target: String,
    transfer: String,
    plan_digest: [u8; 32],
}

impl Binding {
    pub(crate) fn for_plan(source: &str, target: &str, transfer: &str, plan: &[u8]) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            transfer: transfer.into(),
            plan_digest: Sha256::digest(plan).into(),
        }
    }

    pub(super) fn digest(&self) -> [u8; 32] {
        self.plan_digest
    }

    fn same_transfer(&self, other: &Self) -> bool {
        self.source == other.source
            && self.target == other.target
            && self.transfer == other.transfer
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    device: String,
    binding: Binding,
    #[serde(default)]
    revoked: bool,
}

fn canonical_id(id: &str) -> bool {
    valid_id(id) && !id.bytes().any(|b| b.is_ascii_uppercase())
}

fn admission_name(transfer: &str) -> io::Result<String> {
    if !canonical_id(transfer) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid move transfer ID",
        ));
    }
    Ok(format!("move-admission-{transfer}.json"))
}

fn reservation_name(transfer: &str) -> io::Result<String> {
    canonical_id(transfer)
        .then(|| format!("move-reservation-{transfer}.json"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid move transfer ID"))
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Move admission is not active",
    )
}

impl State {
    fn move_reservation(&self, transfer: &str) -> io::Result<Option<Admission>> {
        let stored: Option<Admission> = read_registry(&self.dir.join(reservation_name(transfer)?))?;
        if stored
            .as_ref()
            .is_some_and(|saved| saved.binding.transfer != transfer)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Move reservation is stored under the wrong ID",
            ));
        }
        Ok(stored)
    }

    /// Record the operator-approved plan before acknowledging the queued move.
    /// It remains available while the current turn runs and across restarts.
    pub(crate) fn reserve_move_plan(&mut self, device: &str, plan: &Binding) -> io::Result<()> {
        self.check_move_peer(device, plan)?;
        if let Some(saved) = self.move_reservation(&plan.transfer)? {
            return if !saved.revoked && saved.device == device && saved.binding == *plan {
                Ok(())
            } else {
                Err(denied())
            };
        }
        if self.move_admission(&plan.transfer)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "This move ID is already in use",
            ));
        }
        write_registry(
            &self.dir,
            &reservation_name(&plan.transfer)?,
            &Admission {
                device: device.into(),
                binding: plan.clone(),
                revoked: false,
            },
        )
    }

    /// Bind the first final manifest to the approved immutable plan.
    pub(crate) fn admit_reserved_move(
        &mut self,
        device: &str,
        plan: &Binding,
        manifest: &Binding,
    ) -> io::Result<()> {
        if !plan.same_transfer(manifest)
            || !self
                .move_reservation(&plan.transfer)?
                .is_some_and(|reserved| {
                    !reserved.revoked && reserved.device == device && reserved.binding == *plan
                })
        {
            return Err(denied());
        }
        self.issue_move_admission(device, manifest)
    }

    fn move_admission(&self, transfer: &str) -> io::Result<Option<Admission>> {
        let stored: Option<Admission> = read_registry(&self.dir.join(admission_name(transfer)?))?;
        if stored
            .as_ref()
            .is_some_and(|saved| saved.binding.transfer != transfer)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Move admission is stored under the wrong transfer ID",
            ));
        }
        Ok(stored)
    }

    pub(crate) fn check_move_peer(&self, device: &str, binding: &Binding) -> io::Result<()> {
        if !canonical_id(&binding.source)
            || !canonical_id(&binding.target)
            || binding.source == binding.target
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid source or destination for the move",
            ));
        }
        if !self
            .worker_grant(device)
            .is_some_and(|grant| grant.may_accept_moves && grant.source == binding.source)
            || !self.host_id()?.eq_ignore_ascii_case(&binding.target)
        {
            return Err(denied());
        }
        Ok(())
    }

    /// Accept an offer only from the pairing that reserved its plan. The
    /// importer checks the plan digest against the bytes in the archive.
    pub(crate) fn admit_move_offer(&mut self, device: &str, manifest: &Binding) -> io::Result<()> {
        let approved = self
            .move_reservation(&manifest.transfer)?
            .ok_or_else(denied)?;
        self.admit_reserved_move(device, &approved.binding, manifest)
    }

    pub(crate) fn check_move_reservation(&self, device: &str, plan: &Binding) -> io::Result<()> {
        self.check_move_peer(device, plan)?;
        if self
            .move_reservation(&plan.transfer)?
            .is_some_and(|saved| saved.device == device && saved.binding == *plan)
        {
            return Ok(());
        }
        Err(denied())
    }

    /// Called by the destination operator after approving this exact plan.
    /// The authenticated worker registration identifies the sender and cannot issue a grant.
    pub(crate) fn issue_move_admission(
        &mut self,
        device: &str,
        binding: &Binding,
    ) -> io::Result<()> {
        self.check_move_peer(device, binding)?;
        if self
            .move_reservation(&binding.transfer)?
            .is_some_and(|reserved| reserved.revoked || reserved.device != device)
        {
            return Err(denied());
        }
        if let Some(saved) = self.move_admission(&binding.transfer)? {
            if saved.revoked {
                return Err(denied());
            }
            if saved.device == device && saved.binding == *binding {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Move transfer ID already belongs to another plan or host link",
            ));
        }
        write_registry(
            &self.dir,
            &admission_name(&binding.transfer)?,
            &Admission {
                device: device.into(),
                binding: binding.clone(),
                revoked: false,
            },
        )
    }

    /// Verify the signed body and the exact destination approval. The caller
    /// computes the digest in `binding` from the parsed plan in `request`.
    pub(crate) fn prove_move_admission(
        &mut self,
        device: &str,
        nonce: &str,
        signature: &str,
        request: &str,
        binding: &Binding,
    ) -> bool {
        self.prove_worker(device, nonce, signature, request)
            && self.check_move_admission(device, binding).is_ok()
    }

    /// Recheck under the State lock at commit, so a revoked link or admission
    /// cannot acquire a destination writer after its last successful request.
    pub(crate) fn check_move_admission(&self, device: &str, binding: &Binding) -> io::Result<()> {
        self.check_move_peer(device, binding)?;
        self.expected_move_plan(device, binding)?;
        self.move_admission(&binding.transfer)?
            .filter(|saved| !saved.revoked && saved.device == device && saved.binding == *binding)
            .ok_or_else(denied)
            .map(|_| ())
    }

    pub(super) fn expected_move_plan(
        &self,
        device: &str,
        manifest: &Binding,
    ) -> io::Result<Option<[u8; 32]>> {
        match self.move_reservation(&manifest.transfer)? {
            Some(reserved)
                if !reserved.revoked
                    && reserved.device == device
                    && reserved.binding.same_transfer(manifest) =>
            {
                Ok(Some(reserved.binding.digest()))
            }
            Some(_) => Err(denied()),
            None => Ok(None),
        }
    }

    /// Retain the original binding so a revoked transfer ID cannot be reused.
    pub(crate) fn revoke_move_admission(&mut self, transfer: &str) -> io::Result<()> {
        if let Some(mut reserved) = self.move_reservation(transfer)?
            && !reserved.revoked
        {
            reserved.revoked = true;
            write_registry(&self.dir, &reservation_name(transfer)?, &reserved)?;
            self.signal();
        }
        if let Some(mut saved) = self.move_admission(transfer)?
            && !saved.revoked
        {
            saved.revoked = true;
            write_registry(&self.dir, &admission_name(transfer)?, &saved)?;
            self.signal();
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        admin::DeviceClass,
        state::{B64, Enrollment, Purpose, digest},
    };
    use base64::Engine as _;
    use p256::{
        ecdsa::{Signature, SigningKey, signature::Signer as _},
        elliptic_curve::rand_core::OsRng,
    };
    use std::{fs, os::unix::fs::PermissionsExt};

    pub(in crate::state) const SOURCE: &str = "74b2c731-b9f0-4b05-8672-3c42062b190a";
    pub(in crate::state) const TARGET: &str = "06f2bbe4-12bb-4243-ab20-657f19c02d36";
    pub(in crate::state) const TRANSFER: &str = "ddcba183-5073-4e9a-9825-e8730784ce5e";
    const OTHER_TRANSFER: &str = "9686915e-959f-4306-9811-67ca77da16a3";
    const LOGIN: &str = "owner@example.test";
    const PLAN: &[u8] = b"the exact proposed move plan";

    pub(in crate::state) fn sign(key: &SigningKey, text: &str) -> String {
        let signature: Signature = key.sign(text.as_bytes());
        B64.encode(signature.to_bytes())
    }

    pub(in crate::state) fn pair(state: &mut State, source: &str) -> (String, SigningKey) {
        let key = SigningKey::random(&mut OsRng);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        let code = state
            .local_worker_invite(Some(LOGIN.into()))
            .unwrap()
            .short_code
            .unwrap();
        let token = state
            .pair(
                LOGIN,
                &code,
                &public,
                &sign(&key, &format!("{code}.host.{source}")),
                "Linked sender",
                Enrollment {
                    class: DeviceClass::Host,
                    source: Some(source),
                },
            )
            .unwrap();
        (state.identity(LOGIN, &token).unwrap(), key)
    }

    pub(in crate::state) fn fixture() -> (tempfile::TempDir, State, Binding) {
        let dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("host-id"), TARGET).unwrap();
        let mut state = State::load(dir.path()).unwrap();
        state
            .set_origin(Some("https://destination.tail.ts.net".into()), 49184)
            .unwrap();
        (
            dir,
            state,
            Binding::for_plan(SOURCE, TARGET, TRANSFER, PLAN),
        )
    }

    pub(in crate::state) fn accept_moves(state: &mut State, device: &str, allow: bool) {
        let before = state.worker_grant(device).unwrap().clone();
        state
            .edit_worker(
                device,
                &before,
                before.may_launch,
                before.ceiling,
                None,
                Some(allow),
            )
            .unwrap();
    }

    #[test]
    fn queued_plan_stays_approved_across_restart_and_can_bind_only_one_manifest() {
        let (dir, mut state, plan) = fixture();
        let (device, _) = pair(&mut state, SOURCE);
        accept_moves(&mut state, &device, true);
        state.reserve_move_plan(&device, &plan).unwrap();
        state.reserve_move_plan(&device, &plan).unwrap();
        drop(state);

        let mut state = State::load(dir.path()).unwrap();
        let manifest = Binding::for_plan(SOURCE, TARGET, TRANSFER, b"final manifest");
        let other_plan = Binding::for_plan(SOURCE, TARGET, TRANSFER, b"changed plan");
        assert!(
            state
                .admit_reserved_move(&device, &other_plan, &manifest)
                .is_err()
        );
        assert!(state.check_move_admission(&device, &manifest).is_err());
        state
            .admit_reserved_move(&device, &plan, &manifest)
            .unwrap();
        state
            .admit_reserved_move(&device, &plan, &manifest)
            .unwrap();
        state.check_move_admission(&device, &manifest).unwrap();
        let other_manifest = Binding::for_plan(SOURCE, TARGET, TRANSFER, b"other files");
        assert!(
            state
                .admit_reserved_move(&device, &plan, &other_manifest)
                .is_err()
        );
        state.revoke_move_admission(TRANSFER).unwrap();
        assert!(
            state
                .admit_reserved_move(&device, &plan, &manifest)
                .is_err()
        );
        assert!(state.check_move_admission(&device, &manifest).is_err());
    }

    #[test]
    fn signed_worker_launch_permission_does_not_grant_moves() {
        let (_dir, mut state, binding) = fixture();
        let (device, key) = pair(&mut state, SOURCE);
        let plan = serde_json::to_string(&binding).unwrap();
        assert!(state.worker_grant(&device).unwrap().may_launch);
        let nonce = state.challenge(&device, Purpose::Worker).unwrap();
        let signature = sign(&key, &format!("{nonce}.{}", digest(&plan)));
        assert!(!state.prove_move_admission(&device, &nonce, &signature, &plan, &binding));
        assert!(state.issue_move_admission(&device, &binding).is_err());
        accept_moves(&mut state, &device, true);
        assert!(state.check_move_admission(&device, &binding).is_err());
        state.issue_move_admission(&device, &binding).unwrap();
        let nonce = state.challenge(&device, Purpose::Worker).unwrap();
        let signature = sign(&key, &format!("{nonce}.{}", digest(&plan)));
        assert!(!state.prove_move_admission(&device, &nonce, &signature, "other plan", &binding));
        let nonce = state.challenge(&device, Purpose::Worker).unwrap();
        let signature = sign(&key, &format!("{nonce}.{}", digest(&plan)));
        assert!(state.prove_move_admission(&device, &nonce, &signature, &plan, &binding));
        assert!(!state.prove_move_admission(&device, &nonce, &signature, &plan, &binding));
    }

    #[test]
    fn retries_use_the_stored_pairing_destination_transfer_and_digest() {
        let (dir, mut state, binding) = fixture();
        let (device, _key) = pair(&mut state, SOURCE);
        accept_moves(&mut state, &device, true);
        let different_source = "a77693d3-2f89-4cde-a3fc-92f080d4af1d";
        let (other, _key) = pair(&mut state, different_source);
        state.issue_move_admission(&device, &binding).unwrap();
        let stored =
            std::fs::read_to_string(dir.path().join(admission_name(TRANSFER).unwrap())).unwrap();
        assert!(!stored.contains(std::str::from_utf8(PLAN).unwrap()));
        drop(state);
        let mut state = State::load(dir.path()).unwrap();
        state.issue_move_admission(&device, &binding).unwrap();
        state.check_move_admission(&device, &binding).unwrap();

        let changed = [
            Binding {
                source: different_source.into(),
                ..binding.clone()
            },
            Binding {
                target: different_source.into(),
                ..binding.clone()
            },
            Binding {
                transfer: OTHER_TRANSFER.into(),
                ..binding.clone()
            },
            Binding::for_plan(SOURCE, TARGET, TRANSFER, b"another move plan"),
            Binding {
                transfer: TRANSFER.to_ascii_uppercase(),
                ..binding.clone()
            },
            Binding {
                transfer: "../incoming-worker-hosts".into(),
                ..binding.clone()
            },
        ];
        for plan in &changed {
            assert!(
                state.check_move_admission(&device, plan).is_err(),
                "{plan:?}"
            );
        }
        assert!(state.check_move_admission(&other, &changed[0]).is_err());
        assert_eq!(
            state
                .issue_move_admission(&device, &changed[3])
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        state.check_move_admission(&device, &binding).unwrap();
    }

    #[test]
    fn revoked_admissions_and_replaced_links_cannot_reuse_old_grants() {
        let (dir, mut state, binding) = fixture();
        let (device, _key) = pair(&mut state, SOURCE);
        accept_moves(&mut state, &device, true);
        state.issue_move_admission(&device, &binding).unwrap();
        accept_moves(&mut state, &device, false);
        assert!(state.check_move_admission(&device, &binding).is_err());
        let (replacement, _key) = pair(&mut state, SOURCE);
        assert!(!state.worker_grant(&replacement).unwrap().may_accept_moves);
        accept_moves(&mut state, &replacement, true);
        assert!(state.check_move_admission(&replacement, &binding).is_err());
        state.revoke(&device).unwrap();
        assert!(state.check_move_admission(&device, &binding).is_err());
        assert!(state.issue_move_admission(&replacement, &binding).is_err());
        let next = Binding::for_plan(SOURCE, TARGET, OTHER_TRANSFER, PLAN);
        state.issue_move_admission(&replacement, &next).unwrap();
        state.revoke_move_admission(OTHER_TRANSFER).unwrap();
        state.revoke_move_admission(OTHER_TRANSFER).unwrap();
        let mut state = State::load(dir.path()).unwrap();
        assert!(state.check_move_admission(&replacement, &binding).is_err());
        assert!(state.issue_move_admission(&replacement, &binding).is_err());
        assert!(state.check_move_admission(&replacement, &next).is_err());
        assert!(state.issue_move_admission(&replacement, &next).is_err());
    }
}
