use super::connection::{Connection, IDLE};
use super::{ArchiveChunks, CHUNK, Checkout, Delivery, Receipt, Reply, Request};
use crate::{
    inventory,
    paired_hosts::valid_id,
    server::{App, Proof, paired_identity},
    state::private_dir,
};
use axum::{
    extract::{State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use ox_host_service::checkout::{Mirror, check_archive_space, receive_commit};
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::ffi::OsStrExt, path::Path, sync::Arc, time::Duration};
use tokio::{io::AsyncSeekExt, time::timeout};

type WebSocket = Connection<Message>;

pub(crate) async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let id = paired_identity(&app, &headers)?;
    if !app
        .state
        .lock()
        .unwrap()
        .worker_grant(&id)
        .is_some_and(|g| g.may_launch)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let slot = app
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(ws
        .max_message_size(CHUNK)
        .max_frame_size(CHUNK)
        .on_upgrade(move |socket| async move {
            let mut socket = Connection::open(socket);
            let slot = Arc::new(slot);
            if let Err(e) = session(&app, &id, &mut socket, slot).await {
                let _ = reply(
                    &mut socket,
                    Reply::Error {
                        message: e.to_string(),
                    },
                )
                .await;
            }
        }))
}
async fn reply(socket: &mut WebSocket, r: Reply) -> io::Result<()> {
    socket.send(Message::text(serde_json::to_string(&r)?)).await
}
async fn control<T: serde::de::DeserializeOwned>(socket: &mut WebSocket) -> io::Result<T> {
    let Message::Text(text) = socket.next().await? else {
        return Err(io::Error::other("Expected a checkout message."));
    };
    Ok(serde_json::from_str(&text)?)
}
fn allowed(app: &App, id: &str, request: &Checkout) -> bool {
    app.state
        .lock()
        .unwrap()
        .worker_grant(id)
        .is_some_and(|g| g.may_launch && g.source == request.source)
}
async fn session(
    app: &App,
    id: &str,
    socket: &mut WebSocket,
    slot: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> io::Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Open {
        proof: Proof,
        request: String,
    }
    let open: Open = timeout(Duration::from_secs(8), control(socket)).await??;
    let Request::Open(request) = serde_json::from_str(&open.request)? else {
        return Err(io::Error::other("Expected a checkout request."));
    };
    let (directory, mut changes) = {
        let mut s = app.state.lock().unwrap();
        if !s.prove_worker(id, &open.proof.nonce, &open.proof.signature, &open.request)
            || !s
                .worker_grant(id)
                .is_some_and(|g| g.may_launch && g.source == request.source)
            || s.host_id()? != request.target
        {
            return Err(io::Error::other("Worker host link or identity changed."));
        }
        if !valid_id(&request.request_id)
            || request.job == 0
            || request.job >= (1u64 << 52)
            || request.repository.len() != 64
            || !request.repository.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(io::Error::other("Invalid worker checkout request."));
        }
        (s.worker_directory(), s.changed.subscribe())
    };
    let revoked = async {
        loop {
            if !allowed(app, id, &request) || changes.changed().await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        biased;
        _ = revoked => Err(io::Error::other("Worker link revoked or launches disabled.")),
        result = install(app, id, &request, &directory, socket, slot) => result,
    }
}
#[derive(Deserialize, Serialize)]
struct Installed {
    request: Checkout,
    device: String,
    /// Native relative path inside the job checkout. Paths never cross the host link.
    cwd: Vec<u8>,
    receipt: Receipt,
}
fn existing(dest: &Path, request: &Checkout, device: &str) -> io::Result<Option<Receipt>> {
    match fs::File::open(dest.join("receipt.json")) {
        Ok(f) => {
            let saved: Installed = serde_json::from_reader(f)?;
            if &saved.request != request || saved.device != device {
                return Err(io::Error::other(
                    "A different worker request owns this job number.",
                ));
            }
            Ok(Some(saved.receipt))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && !dest.try_exists()? => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(io::Error::other(
            "This job directory has no receipt. Start a new worker and retain the existing checkout for recovery.",
        )),
        Err(e) => Err(e),
    }
}

/// Resolve the engine's cwd from the installed receipt. The source cannot name a destination path.
pub(crate) fn job_checkout(
    root: &Path,
    job: &crate::linked_jobs::JobRequest,
    device: &str,
) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::{ffi::OsStr, path::Component};
    let directory = root
        .join("jobs")
        .join(&job.source)
        .join(job.job.to_string());
    let saved: Installed =
        serde_json::from_reader(fs::File::open(directory.join("receipt.json"))?)?;
    if saved.device != device
        || saved.request.source != job.source
        || saved.request.target != job.target
        || saved.request.job != job.job
        || saved.request.request_id != job.request_id
    {
        return Err(io::Error::other(
            "This worker link does not own the installed job.",
        ));
    }
    let relative = Path::new(OsStr::from_bytes(&saved.cwd));
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::other(
            "Invalid project in the worker checkout receipt.",
        ));
    }
    let root = directory.join("checkout");
    let mut project = root.join(relative);
    // Status and cancellation remain available if the worker moved its project after launch.
    if matches!(job.operation, crate::linked_jobs::Operation::Start { .. }) {
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            return Err(io::Error::other("The installed checkout has moved."));
        }
        let expected = root.canonicalize()?.join(relative);
        project = project.canonicalize()?;
        if project != expected || !project.is_dir() {
            return Err(io::Error::other("The installed worker project has moved."));
        }
    }
    Ok((directory, project))
}

async fn install(
    app: &App,
    id: &str,
    request: &Checkout,
    directory: &Path,
    socket: &mut WebSocket,
    slot: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> io::Result<()> {
    let jobs = directory.join("jobs").join(&request.source);
    let dest = jobs.join(request.job.to_string());
    let ready = |existing| Reply::Ready {
        target: request.target.clone(),
        source: request.source.clone(),
        existing,
    };
    if let Some(receipt) = existing(&dest, request, id)? {
        return reply(socket, ready(Some(receipt))).await;
    }
    let paths = inventory::projects(app, &request.target).await?;
    let cache = directory.join("mirrors");
    let repository = request.repository.clone();
    let own = directory.to_owned();
    let seed_slot = slot.clone();
    let mirror = tokio::task::spawn_blocking(move || {
        let _slot = seed_slot;
        private_dir(&cache)?;
        Mirror::from_registered(&cache, &paths, &own, &repository).map(Arc::new)
    })
    .await??;
    reply(socket, ready(None)).await?;
    let mut checked_origin = false;
    let (bytes, commit) = loop {
        match control(socket).await? {
            Request::Known { commits } if commits.len() <= 128 => {
                let m = mirror.clone();
                let (mut known, candidates) =
                    tokio::task::spawn_blocking(move || -> io::Result<_> {
                        Ok((m.known(&commits)?, commits))
                    })
                    .await??;
                if !checked_origin && known.len() != candidates.len() {
                    checked_origin = true;
                    let origin = mirror.clone();
                    let permit = slot.clone();
                    let fetch = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        origin.refresh_if_stale()
                    });
                    // Give a nearby origin one round trip. A slow refresh can benefit later jobs.
                    if matches!(timeout(Duration::from_secs(2), fetch).await, Ok(Ok(Ok(())))) {
                        let m = mirror.clone();
                        known = tokio::task::spawn_blocking(move || m.known(&candidates)).await??;
                    }
                }
                let commits = known;
                reply(socket, Reply::Known { commits }).await?;
            }
            Request::Archive { bytes, commit }
                if matches!(commit.len(), 40 | 64)
                    && commit.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                break (bytes, commit);
            }
            _ => return Err(io::Error::other("Invalid checkout negotiation.")),
        }
    };
    private_dir(&jobs)?;
    let stage = tempfile::Builder::new()
        .prefix(".incoming-")
        .tempdir_in(&jobs)?;
    let file = tempfile::tempfile_in(stage.path())?;
    check_archive_space(&file, bytes)?;
    let mut archive = tokio::fs::File::from_std(file);
    let mut chunks = ArchiveChunks::new(bytes);
    while !chunks.complete() {
        let Message::Binary(chunk) = timeout(IDLE, socket.next()).await?? else {
            return Err(io::Error::other("Incomplete checkout archive."));
        };
        chunks.write(&mut archive, &chunk).await?;
    }
    let sha256 = format!("{:x}", chunks.digest());
    if !matches!(control(socket).await?, Request::Finish {sha256: claimed} if claimed == sha256) {
        return Err(io::Error::other("Checkout archive checksum differs."));
    }
    archive.rewind().await?;
    let archive = archive.into_std().await;
    let receipt = Receipt {
        commit,
        archive_bytes: bytes,
        sha256,
    };
    let saved = receipt.clone();
    let req = request.clone();
    let device = id.to_owned();
    let stage = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let _slot = slot;
        let selected =
            receive_commit(stage.path(), req.job, mirror.path(), archive, &saved.commit)?;
        let work = stage.path().join(req.job.to_string());
        let cwd = selected
            .strip_prefix(&work)
            .map_err(io::Error::other)?
            .as_os_str()
            .as_bytes()
            .to_vec();
        let baseline =
            ox_host_service::checkout::results::Baseline::read_checkout(&work, &saved.commit)?;
        crate::worker_result::save_base(
            stage.path(),
            baseline,
            mirror.registered(),
            mirror.path(),
            &req.repository,
        )?;
        fs::rename(work, stage.path().join("checkout"))?;
        let f = fs::File::create(stage.path().join("receipt.json"))?;
        serde_json::to_writer(
            &f,
            &Installed {
                request: req,
                device,
                cwd,
                receipt: saved,
            },
        )?;
        f.sync_all()?;
        fs::File::open(stage.path())?.sync_all()?;
        Ok(stage)
    })
    .await??;
    // Publish while holding the same lock that serializes link revocation. Dropping a
    // cancelled transfer, including an in-flight Git operation, only removes its stage.
    let delivery = {
        let state = app.state.lock().unwrap();
        if !state
            .worker_grant(id)
            .is_some_and(|g| g.may_launch && g.source == request.source)
        {
            return Err(io::Error::other("Worker link was revoked during checkout."));
        }
        match fs::rename(stage.path(), &dest) {
            Ok(()) => Delivery {
                receipt,
                recovered: false,
            },
            Err(e) => Delivery {
                receipt: existing(&dest, request, id)?.ok_or(e)?,
                recovered: true,
            },
        }
    };
    fs::File::open(jobs)?.sync_all()?;
    reply(socket, Reply::Installed(delivery)).await
}

/// The production host holds its exclusive state-directory lock before starting
/// the web service. Never descend into a published checkout or follow a link.
pub(crate) fn recover(directory: &Path) -> io::Result<()> {
    fn children(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => fs::read_dir(path)?.collect(),
            Ok(_) => Err(io::Error::other("Expected a private checkout directory.")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
    fn discard(path: &Path, prefix: &str) -> io::Result<()> {
        for entry in children(path)? {
            if entry.file_type()?.is_dir()
                && entry.file_name().to_string_lossy().starts_with(prefix)
            {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }
    if !directory.try_exists()? {
        return Ok(());
    }
    private_dir(directory)?;
    discard(&directory.join("mirrors"), ".mirror-")?;
    discard(&directory.join("outgoing"), ".sending-")?;
    for source in children(&directory.join("jobs"))? {
        if source.file_type()?.is_dir() && source.file_name().to_str().is_some_and(valid_id) {
            discard(&source.path(), ".incoming-")?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_cleanup_only_removes_abandoned_stages() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("linked-work");
        private_dir(&base).unwrap();
        let jobs = base.join("jobs/06f2bbe4-12bb-4243-ab20-657f19c02d36");
        for path in [
            jobs.join(".incoming-a"),
            jobs.join("42/checkout/.incoming-a"),
            base.join("mirrors/.mirror-z"),
            base.join("outgoing/.sending-z"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(jobs.join("42"), jobs.join(".incoming-symlink")).unwrap();
        recover(&base).unwrap();
        assert!(jobs.join("42/checkout/.incoming-a").exists());
        assert!(jobs.join(".incoming-symlink").is_symlink());
        assert!(!jobs.join(".incoming-a").exists());
        assert!(!base.join("mirrors/.mirror-z").exists());
        assert!(!base.join("outgoing/.sending-z").exists());
    }
}
