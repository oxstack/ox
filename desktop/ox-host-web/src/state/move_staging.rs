//! Stages one authenticated move's files without interpreting or activating them.
use super::{State, move_admission::Binding, private_dir, read_registry, write_registry};
use crate::{
    move_manifest::{CHECKOUT, FileSpec, Manifest, PLAN, TRANSCRIPT},
    server::Proof,
    worker_checkout::{
        ArchiveChunks, CHUNK,
        connection::{Connection, IDLE},
    },
};
use axum::{body::Bytes, extract::ws::Message};
use ox_host_service::checkout::check_archive_space;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::time::timeout;

const DIRECTORY: &str = "move-staging";
const RECEIPT: &str = "receipt.json";

impl Manifest {
    fn files(&self) -> [(&str, &FileSpec); 3] {
        [
            (PLAN, &self.plan),
            (TRANSCRIPT, &self.transcript),
            (CHECKOUT, &self.checkout),
        ]
    }

    fn total_bytes(&self) -> io::Result<u64> {
        self.files()
            .into_iter()
            .try_fold(0u64, |sum, (_, spec)| sum.checked_add(spec.bytes))
            .ok_or_else(|| invalid("Session move size overflows"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub source: String,
    pub target: String,
    pub transfer: String,
    pub manifest: Manifest,
}

impl Offer {
    /// A destination operator approves the manifest, including the digest of
    /// the plan bytes and the two archives inspected by the receiving actor.
    pub(crate) fn binding(&self) -> io::Result<Binding> {
        self.manifest.total_bytes()?;
        if self.manifest.plan.bytes == 0 {
            return Err(invalid("Session move is missing its plan"));
        }
        Ok(Binding::for_plan(
            &self.source,
            &self.target,
            &self.transfer,
            &serde_json::to_vec(&self.manifest)?,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub transfer: String,
    pub manifest_sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    offer: Offer,
    device: String,
}

#[derive(Debug)]
pub struct StagedMove {
    pub offer: Offer,
    pub receipt: Receipt,
    pub plan: PathBuf,
    pub transcript: PathBuf,
    pub checkout: PathBuf,
    pub expected_plan: Option<[u8; 32]>,
    pub recovered: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct PreparedReply<T> {
    prepared: T,
}

pub(crate) enum Chunk {
    Bytes(Bytes),
    Finish,
}

#[derive(Deserialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Finish,
}

pub(crate) trait Frames: Send {
    fn request_upload(&mut self) -> impl Future<Output = io::Result<()>> + Send {
        async { Ok(()) }
    }

    fn next(&mut self) -> impl Future<Output = io::Result<Chunk>> + Send;
}

impl Frames for Connection<Message> {
    async fn request_upload(&mut self) -> io::Result<()> {
        self.send(Message::text(r#"{"step":"upload"}"#)).await
    }

    async fn next(&mut self) -> io::Result<Chunk> {
        match Connection::next(self).await? {
            Message::Binary(bytes) => Ok(Chunk::Bytes(bytes)),
            Message::Text(text) => match serde_json::from_str(&text)? {
                Control::Finish => Ok(Chunk::Finish),
            },
            _ => Err(invalid("Expected a session move chunk")),
        }
    }
}

pub(crate) struct Upload {
    state: Arc<Mutex<State>>,
    binding: Binding,
    offer: Offer,
    device: String,
    directory: PathBuf,
    expected_plan: Option<[u8; 32]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation<T> {
    offer: Offer,
    commit: T,
}

pub struct VerifiedActivation<T> {
    state: Arc<Mutex<State>>,
    device: String,
    binding: Binding,
    source: String,
    transfer: String,
    commit: T,
}

impl<T: serde::de::DeserializeOwned> VerifiedActivation<T> {
    pub(crate) fn open(
        state: Arc<Mutex<State>>,
        device: &str,
        request: &str,
        proof: &Proof,
    ) -> io::Result<Self> {
        let Activation { offer, commit }: Activation<T> = serde_json::from_str(request)?;
        let binding = offer.binding()?;
        if !state.lock().unwrap().prove_move_admission(
            device,
            &proof.nonce,
            &proof.signature,
            request,
            &binding,
        ) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Session move is not approved on this host",
            ));
        }
        Ok(Self {
            state,
            binding,
            device: device.into(),
            source: offer.source,
            transfer: offer.transfer,
            commit,
        })
    }

    /// Called on the destination actor. The same lock guards link and grant
    /// revocation. The actor checks its Prepared record before acquisition.
    pub fn commit<R>(
        self,
        activate: impl FnOnce(&str, &str, [u8; 32], T) -> io::Result<R>,
    ) -> io::Result<R> {
        let state = self.state.lock().unwrap();
        state.check_move_admission(&self.device, &self.binding)?;
        activate(
            &self.source,
            &self.transfer,
            self.binding.digest(),
            self.commit,
        )
    }
}

impl Upload {
    /// The transport authenticates `device` from the paired connection, then
    /// passes the raw signed JSON offer and its proof here.
    pub(crate) fn open(
        state: Arc<Mutex<State>>,
        device: &str,
        request: &str,
        proof: &Proof,
    ) -> io::Result<Self> {
        let offer: Offer = serde_json::from_str(request)?;
        let binding = offer.binding()?;
        let (directory, expected_plan) = {
            let mut state = state.lock().unwrap();
            if !state.prove_worker(device, &proof.nonce, &proof.signature, request) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Session move is not approved on this host",
                ));
            }
            if state.expected_move_plan(device, &binding)?.is_some() {
                state.admit_move_offer(device, &binding)?;
            }
            state.check_move_admission(device, &binding)?;
            (
                state.dir.join(DIRECTORY),
                state.expected_move_plan(device, &binding)?,
            )
        };
        Ok(Self {
            state,
            binding,
            offer,
            device: device.into(),
            directory,
            expected_plan,
        })
    }

    fn check(&self) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .check_move_admission(&self.device, &self.binding)
    }

    fn staged(&self, recovered: bool) -> StagedMove {
        let path = self.directory.join(&self.offer.transfer);
        StagedMove {
            offer: self.offer.clone(),
            receipt: Receipt {
                transfer: self.offer.transfer.clone(),
                manifest_sha256: self.binding.digest(),
            },
            plan: path.join(PLAN),
            transcript: path.join(TRANSCRIPT),
            checkout: path.join(CHECKOUT),
            expected_plan: self.expected_plan,
            recovered,
        }
    }

    /// Verify previously published files before recovering a lost receipt.
    async fn existing(&self) -> io::Result<Option<StagedMove>> {
        self.check()?;
        let root = self.directory.clone();
        let path = root.join(&self.offer.transfer);
        let offer = self.offer.clone();
        let device = self.device.clone();
        let found = tokio::task::spawn_blocking(move || {
            if !private_directory_exists(&root)? || !private_directory_exists(&path)? {
                return Ok(false);
            }
            let saved = read_registry::<Option<Saved>>(&path.join(RECEIPT))?
                .ok_or_else(|| invalid("Staged session move has no receipt"))?;
            if saved.offer != offer || saved.device != device {
                return Err(invalid("Staged files belong to a different session move"));
            }
            for (name, spec) in offer.manifest.files() {
                verify_file(&path.join(name), spec)?;
            }
            File::open(root)?.sync_all()?;
            Ok(true)
        })
        .await??;
        self.check()?;
        Ok(found.then(|| self.staged(true)))
    }

    /// The three declared payloads arrive in manifest order. Only a Finish
    /// after their exact lengths and hashes allows the stage to be published.
    async fn receive(&self, frames: &mut impl Frames) -> io::Result<StagedMove> {
        let mut changes = self.state.lock().unwrap().changed.subscribe();
        self.check()?;
        let stage = self.stage(frames);
        tokio::pin!(stage);
        let stage = loop {
            tokio::select! {
                biased;
                change = changes.changed() => {
                    change.map_err(io::Error::other)?;
                    self.check()?;
                }
                result = &mut stage => break result?,
            }
        };

        // Serialize publication with registration and grant revocation, as for
        // a worker checkout. An existing destination belongs to its first receipt.
        {
            let state = self.state.lock().unwrap();
            state.check_move_admission(&self.device, &self.binding)?;
            let dest = self.directory.join(&self.offer.transfer);
            match fs::symlink_metadata(&dest) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::rename(stage.path(), &dest)?;
                    File::open(&self.directory)?.sync_all()?;
                    return Ok(self.staged(false));
                }
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        self.existing()
            .await?
            .ok_or_else(|| invalid("Staged session move disappeared"))
    }

    /// The destination actor validates and imports the local files before an
    /// actor preparation can be reported. A retry still needs that decision.
    pub(crate) async fn prepare_with<T: Send + 'static>(
        &self,
        frames: &mut impl Frames,
        prepare: impl FnOnce(StagedMove) -> io::Result<T> + Send + 'static,
    ) -> io::Result<PreparedReply<T>> {
        let staged = match self.existing().await? {
            Some(staged) => staged,
            None => {
                self.check()?;
                frames.request_upload().await?;
                self.receive(frames).await?
            }
        };
        self.check()?;
        let prepared = tokio::task::spawn_blocking(move || prepare(staged)).await??;
        self.check()?;
        Ok(PreparedReply { prepared })
    }

    async fn stage(&self, frames: &mut impl Frames) -> io::Result<tempfile::TempDir> {
        private_dir(&self.directory)?;
        File::open(self.directory.parent().expect("state directory"))?.sync_all()?;
        let stage = tempfile::Builder::new()
            .prefix(".incoming-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&self.directory)?;
        let bytes = self.offer.manifest.total_bytes()?;
        check_archive_space(&File::open(stage.path())?, bytes).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("The destination needs room for {bytes} move bytes"),
            )
        })?;
        for (name, spec) in self.offer.manifest.files() {
            let file = OpenOptions::new()
                .mode(0o600)
                .create_new(true)
                .write(true)
                .open(stage.path().join(name))?;
            let mut file = tokio::fs::File::from_std(file);
            let mut chunks = ArchiveChunks::new(spec.bytes);
            while !chunks.complete() {
                let Chunk::Bytes(chunk) = timeout(IDLE, frames.next()).await?? else {
                    return Err(invalid("Incomplete session move file"));
                };
                chunks.write(&mut file, &chunk).await?;
            }
            if <[u8; 32]>::from(chunks.digest()) != spec.sha256 {
                return Err(invalid("Session move file checksum differs"));
            }
            file.sync_all().await?;
        }
        if !matches!(timeout(IDLE, frames.next()).await??, Chunk::Finish) {
            return Err(invalid("Expected the end of the session move"));
        }
        write_registry(
            stage.path(),
            RECEIPT,
            &Saved {
                offer: self.offer.clone(),
                device: self.device.clone(),
            },
        )?;
        Ok(stage)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    pub(crate) request: String,
    pub(crate) proof: Proof,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
enum StageReply<T> {
    Upload,
    Error {
        message: String,
    },
    Prepared {
        transfer: String,
        manifest_sha256: [u8; 32],
        prepared: T,
    },
}

/// Serve a single transfer after the HTTP layer has verified the paired
/// identity. This connection does not grant a move or activate the receiver.
pub(crate) async fn stage_socket<T: Serialize + Send + 'static>(
    state: Arc<Mutex<State>>,
    device: &str,
    connection: &mut Connection<Message>,
    prepare: impl FnOnce(StagedMove) -> io::Result<T> + Send + 'static,
) -> io::Result<()> {
    let Message::Text(text) = timeout(IDLE, Connection::next(connection)).await?? else {
        return Err(invalid("Expected a signed session move offer"));
    };
    let open: Open = serde_json::from_str(&text)?;
    let upload = Upload::open(state, device, &open.request, &open.proof)?;
    let prepared = upload.prepare_with(connection, prepare).await?;
    let reply = StageReply::Prepared {
        transfer: upload.offer.transfer.clone(),
        manifest_sha256: upload.binding.digest(),
        prepared: prepared.prepared,
    };
    connection
        .send(Message::text(serde_json::to_string(&reply)?))
        .await
}

pub(crate) async fn send_files_for_prepare<T: serde::de::DeserializeOwned>(
    socket: &mut Connection<tokio_tungstenite::tungstenite::Message>,
    offer: &Offer,
    directory: &Path,
) -> io::Result<T> {
    use tokio::io::AsyncReadExt;
    use tokio_tungstenite::tungstenite::Message as Frame;

    async fn reply<T: serde::de::DeserializeOwned>(
        socket: &mut Connection<tokio_tungstenite::tungstenite::Message>,
    ) -> io::Result<StageReply<T>> {
        let Frame::Text(text) = socket.next().await? else {
            return Err(invalid("Expected a session move reply"));
        };
        Ok(serde_json::from_str(&text)?)
    }
    let mut state = reply(socket).await?;
    if matches!(state, StageReply::Upload) {
        let mut buffer = vec![0; CHUNK];
        for (name, spec) in offer.manifest.files() {
            let mut file = tokio::fs::File::open(directory.join(name)).await?;
            let mut remaining = spec.bytes;
            let mut hash = Sha256::new();
            while remaining > 0 {
                let len = remaining.min(CHUNK as u64) as usize;
                file.read_exact(&mut buffer[..len]).await?;
                hash.update(&buffer[..len]);
                socket
                    .send(Frame::Binary(Bytes::copy_from_slice(&buffer[..len])))
                    .await?;
                remaining -= len as u64;
            }
            if file.read(&mut buffer[..1]).await? != 0
                || <[u8; 32]>::from(hash.finalize()) != spec.sha256
            {
                return Err(invalid(
                    "The source files changed while sending the session.",
                ));
            }
        }
        socket.send(Frame::text(r#"{"step":"finish"}"#)).await?;
        state = reply(socket).await?;
    }
    match state {
        StageReply::Prepared {
            transfer,
            manifest_sha256,
            prepared,
        } if transfer == offer.transfer && manifest_sha256 == offer.manifest.sha256()? => {
            Ok(prepared)
        }
        StageReply::Error { message } => Err(io::Error::other(message)),
        _ => Err(invalid(
            "The destination prepared a different session move.",
        )),
    }
}

pub(crate) async fn send_error(connection: &Connection<Message>, error: &io::Error) {
    let payload = StageReply::<()>::Error {
        message: error.to_string(),
    };
    if let Ok(text) = serde_json::to_string(&payload) {
        let _ = connection.send(Message::text(text)).await;
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn private_directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => private_dir(path).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Run under the host's startup lock before accepting uploads.
pub(crate) fn recover(state: &Path) -> io::Result<()> {
    let directory = state.join(DIRECTORY);
    if private_directory_exists(&directory)? {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".incoming-")
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
        File::open(&directory)?.sync_all()?;
    }
    Ok(())
}

fn verify_file(path: &Path, spec: &FileSpec) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() != spec.bytes
    {
        return Err(invalid("Staged session move file changed"));
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if <[u8; 32]>::from(hash.finalize()) != spec.sha256 {
        return Err(invalid("Staged session move file checksum differs"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        Purpose, digest,
        move_admission::tests::{SOURCE, TARGET, TRANSFER, accept_moves, fixture, pair, sign},
    };
    use p256::ecdsa::SigningKey;

    const P: &[u8] = br#"{"session":"move-fixture"}"#;
    const T: &[u8] = b"archived conversation";
    const C: &[u8] = b"portable checkout";

    struct Fixture {
        dir: tempfile::TempDir,
        state: Arc<Mutex<State>>,
        device: String,
        key: SigningKey,
        offer: Offer,
    }

    impl Fixture {
        fn approved() -> Self {
            let (dir, mut state, _) = fixture();
            let (device, key) = pair(&mut state, SOURCE);
            accept_moves(&mut state, &device, true);
            let spec = |bytes: &[u8]| FileSpec {
                bytes: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
            };
            let offer = Offer {
                source: SOURCE.into(),
                target: TARGET.into(),
                transfer: TRANSFER.into(),
                manifest: Manifest {
                    plan: spec(P),
                    transcript: spec(T),
                    checkout: spec(C),
                },
            };
            let plan = Binding::for_plan(SOURCE, TARGET, TRANSFER, P);
            state.reserve_move_plan(&device, &plan).unwrap();
            state
                .admit_reserved_move(&device, &plan, &offer.binding().unwrap())
                .unwrap();
            Self {
                dir,
                state: Arc::new(Mutex::new(state)),
                device,
                key,
                offer,
            }
        }

        fn open(&self, offer: &Offer) -> io::Result<Upload> {
            let request = serde_json::to_string(offer)?;
            let proof = self.proof(&request);
            Upload::open(self.state.clone(), &self.device, &request, &proof)
        }

        fn proof(&self, request: &str) -> Proof {
            let nonce = self
                .state
                .lock()
                .unwrap()
                .challenge(&self.device, Purpose::Worker)
                .unwrap();
            let signature = sign(&self.key, &format!("{nonce}.{}", digest(request)));
            Proof { nonce, signature }
        }
    }

    fn chunks() -> Vec<Chunk> {
        [&P[..3], &P[3..], T, C]
            .into_iter()
            .map(|part| Chunk::Bytes(Bytes::from_static(part)))
            .chain([Chunk::Finish])
            .collect()
    }

    struct TestFrames<F>(F);

    impl<F: FnMut() -> io::Result<Chunk> + Send> Frames for TestFrames<F> {
        async fn next(&mut self) -> io::Result<Chunk> {
            (self.0)()
        }
    }

    async fn receive(upload: &Upload, chunks: Vec<Chunk>) -> io::Result<StagedMove> {
        let mut chunks = chunks.into_iter();
        let mut frames = TestFrames(|| Ok(chunks.next().expect("planned chunk")));
        fn require_send<T: Send>(value: T) -> T {
            value
        }
        require_send(upload.receive(&mut frames)).await
    }

    #[tokio::test]
    async fn durable_stage_recovers_the_same_receipt_even_when_two_uploads_race() {
        let mut f = Fixture::approved();
        let original = f.open(&f.offer).unwrap();
        let duplicate = f.open(&f.offer).unwrap();
        assert!(original.existing().await.unwrap().is_none());
        let (a, b) = tokio::join!(receive(&original, chunks()), receive(&duplicate, chunks()));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_ne!(a.recovered, b.recovered);
        assert_eq!(a.receipt, b.receipt);
        for (file, expected) in [(&a.plan, P), (&a.transcript, T), (&a.checkout, C)] {
            assert_eq!(fs::read(file).unwrap(), expected);
            assert_eq!(fs::metadata(file).unwrap().mode() & 0o077, 0);
        }
        let abandoned = f.dir.path().join(DIRECTORY).join(".incoming-abandoned");
        fs::create_dir(&abandoned).unwrap();
        recover(f.dir.path()).unwrap();
        assert!(!abandoned.exists());
        f.state = Arc::new(Mutex::new(State::load(f.dir.path()).unwrap()));
        let retry = f.open(&f.offer).unwrap();
        assert_eq!(retry.existing().await.unwrap().unwrap().receipt, a.receipt);
        fs::write(&a.checkout, vec![0; C.len()]).unwrap();
        assert!(retry.existing().await.is_err());
    }

    #[tokio::test]
    async fn rejects_changed_manifest_or_bytes_and_drops_failed_stages() {
        let f = Fixture::approved();
        let mut changed = f.offer.clone();
        changed.manifest.checkout.sha256[0] ^= 1;
        assert!(f.open(&changed).is_err());
        let mut wrong_digest = chunks();
        wrong_digest[2] = Chunk::Bytes(vec![0; T.len()].into());
        let mut extra = chunks();
        extra.insert(extra.len() - 1, Chunk::Bytes(Bytes::from_static(b"extra")));
        for bad in [
            vec![Chunk::Finish],
            vec![Chunk::Bytes(vec![0; CHUNK + 1].into())],
            vec![Chunk::Bytes(vec![0; P.len() + 1].into())],
            wrong_digest,
            extra,
        ] {
            let upload = f.open(&f.offer).unwrap();
            assert!(receive(&upload, bad).await.is_err());
            assert!(upload.existing().await.unwrap().is_none());
            assert_eq!(
                fs::read_dir(f.dir.path().join(DIRECTORY)).unwrap().count(),
                0
            );
        }
    }

    #[tokio::test]
    async fn revocation_before_publication_denies_the_existing_upload() {
        let f = Fixture::approved();
        let upload = f.open(&f.offer).unwrap();
        let mut chunks = chunks().into_iter();
        let mut frames = TestFrames(|| {
            let chunk = chunks.next().unwrap();
            if matches!(chunk, Chunk::Finish) {
                f.state.lock().unwrap().revoke_move_admission(TRANSFER)?;
            }
            Ok(chunk)
        });
        let result = upload.receive(&mut frames).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(upload.existing().await.is_err());
        assert_eq!(
            fs::read_dir(f.dir.path().join(DIRECTORY)).unwrap().count(),
            0
        );
    }

    #[tokio::test]
    async fn a_staged_receipt_cannot_stand_in_for_the_receiving_actors_preparation() {
        #[derive(Debug, Serialize)]
        struct ActorPreparation {
            native: String,
        }

        let f = Fixture::approved();
        let upload = f.open(&f.offer).unwrap();
        let mut payload = chunks().into_iter();
        let mut frames = TestFrames(|| Ok(payload.next().unwrap()));
        let rejected = upload
            .prepare_with(&mut frames, |staged| -> io::Result<ActorPreparation> {
                assert!(!staged.recovered);
                assert_eq!(fs::read(staged.plan)?, P);
                Err(invalid("Destination actor rejected the checkout"))
            })
            .await
            .unwrap_err();
        assert_eq!(
            rejected.to_string(),
            "Destination actor rejected the checkout"
        );
        assert!(upload.existing().await.unwrap().is_some());

        let retry = f.open(&f.offer).unwrap();
        let mut no_upload = TestFrames(|| panic!("a staged retry does not need the files again"));
        let result = retry
            .prepare_with(&mut no_upload, |staged| {
                assert!(staged.recovered);
                assert_eq!(fs::read(staged.checkout)?, C);
                Ok(ActorPreparation {
                    native: "imported-on-destination".into(),
                })
            })
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({"prepared":{"native":"imported-on-destination"}})
        );
    }

    #[tokio::test]
    async fn signed_websocket_stage_retries_without_sending_the_files_again() {
        use axum::{Router, extract::WebSocketUpgrade, routing::get};
        use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

        let f = Fixture::approved();
        let state = f.state.clone();
        let device = f.device.clone();
        let app = Router::new().route(
            "/",
            get(move |ws: WebSocketUpgrade| async move {
                ws.max_message_size(CHUNK).max_frame_size(CHUNK).on_upgrade(
                    move |socket| async move {
                        let mut connection = Connection::open(socket);
                        stage_socket(state, &device, &mut connection, |staged| {
                            assert_eq!(staged.expected_plan, Some(Sha256::digest(P).into()));
                            assert_eq!(fs::read(staged.transcript)?, T);
                            assert_eq!(fs::read(staged.checkout)?, C);
                            Ok(serde_json::json!({
                                "transfer": staged.receipt.transfer,
                                "has_working_checkout": true,
                            }))
                        })
                        .await
                        .unwrap();
                        connection.finish().await;
                    },
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let running = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let sender_files = tempfile::tempdir().unwrap();
        for (name, bytes) in [(PLAN, P), (TRANSCRIPT, T), (CHECKOUT, C)] {
            fs::write(sender_files.path().join(name), bytes).unwrap();
        }
        for first_upload in [true, false] {
            let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
            let mut socket = Connection::open(socket);
            let request = serde_json::to_string(&f.offer).unwrap();
            let proof = f.proof(&request);
            socket
                .send(ClientMessage::text(
                    serde_json::json!({
                        "request": request,
                        "proof": {"nonce": proof.nonce, "signature": proof.signature},
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            let reply: serde_json::Value =
                send_files_for_prepare(&mut socket, &f.offer, sender_files.path())
                    .await
                    .unwrap();
            assert_eq!(
                reply,
                serde_json::json!({"transfer":TRANSFER,"has_working_checkout":true})
            );
            if first_upload {
                for name in [PLAN, TRANSCRIPT, CHECKOUT] {
                    fs::remove_file(sender_files.path().join(name)).unwrap();
                }
            } else {
                assert!(matches!(
                    socket.next().await.unwrap(),
                    ClientMessage::Close(_)
                ));
            }
        }
        running.abort();
    }

    #[test]
    fn destination_commit_rechecks_the_exact_signed_move_under_the_revocation_lock() {
        let f = Fixture::approved();
        let make_request =
            |commit| serde_json::json!({"offer": f.offer, "commit": commit}).to_string();
        let request = make_request("source released");
        let open = |proof: &Proof| {
            VerifiedActivation::<String>::open(f.state.clone(), &f.device, &request, proof)
        };
        assert!(open(&f.proof(&make_request("different operation"))).is_err());
        let accepted = open(&f.proof(&request)).unwrap();
        let checked = accepted
            .commit(|source, transfer, digest, commit| {
                assert_eq!(source, SOURCE);
                assert_eq!(transfer, TRANSFER);
                assert_eq!(digest, f.offer.manifest.sha256().unwrap());
                Ok(commit)
            })
            .unwrap();
        assert_eq!(checked, "source released");

        let accepted = open(&f.proof(&request)).unwrap();
        f.state
            .lock()
            .unwrap()
            .revoke_move_admission(TRANSFER)
            .unwrap();
        let rejected =
            accepted.commit::<()>(|_, _, _, _| panic!("revocation must stop activation"));
        assert_eq!(
            rejected.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn revocation_during_import_withholds_the_actors_reply() {
        let f = Fixture::approved();
        let upload = f.open(&f.offer).unwrap();
        let mut payload = chunks().into_iter();
        let mut frames = TestFrames(|| Ok(payload.next().unwrap()));
        let state = f.state.clone();
        let result = upload
            .prepare_with(&mut frames, move |_| {
                state.lock().unwrap().revoke_move_admission(TRANSFER)?;
                Ok("actor prepared")
            })
            .await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn a_published_path_cannot_redirect_to_another_directory() {
        let f = Fixture::approved();
        let root = f.dir.path().join(DIRECTORY);
        private_dir(&root).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.join(TRANSFER)).unwrap();
        let upload = f.open(&f.offer).unwrap();
        assert!(upload.existing().await.is_err());
        assert!(receive(&upload, chunks()).await.is_err());
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }
}
