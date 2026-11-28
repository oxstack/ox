//! A private per-boot lease. A kernel close on host death also reaches children
//! supervised by an OX helper, including workers in their own process groups.
use crate::{
    engine::{Gateway, Message, Replier},
    transport::same_user,
    workers::Caller,
};
use serde::Deserialize;
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    thread,
    time::Duration,
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    WorkerPlacement,
    Lease {
        id: String,
        parent: Option<String>,
    },
    /// A hosted gateway asks the host to own a worker. `start.parent` carries
    /// only the helper's own session hint and is never trusted as the parent.
    Launch {
        scope: ox_desk_core::host_protocol::Scope,
        launch: String,
        start: ox_desk_core::origin::WorkerStart,
    },
    /// One of the parent agent's worker tools.
    Call {
        scope: ox_desk_core::host_protocol::Scope,
        launch: String,
        parent: Option<String>,
        call: serde_json::Value,
    },
    /// A hosted memory call reports its outcome on the same connection after
    /// receiving its turn binding. Terminal callers do not use this endpoint.
    MemoryReceipt {
        scope: ox_desk_core::host_protocol::Scope,
        parent: Option<String>,
        lease: String,
    },
}

pub(crate) struct Endpoint {
    pub path: PathBuf,
    listener: UnixListener,
}
impl Endpoint {
    pub(crate) fn bind(socket: &Path) -> io::Result<Self> {
        let dir = socket
            .parent()
            .ok_or_else(|| io::Error::other("Invalid socket directory"))?;
        // Only discard socket inodes left by a previous incarnation for this same host.
        let stem = socket
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::other("Invalid socket name"))?;
        let prefix = format!(".{stem}-lease-");
        for ent in fs::read_dir(dir)? {
            let ent = ent?;
            let name = ent.file_name();
            if name
                .to_str()
                .and_then(|s| s.strip_prefix(&prefix))
                .is_some_and(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                let m = ent.path().symlink_metadata()?;
                if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } {
                    fs::remove_file(ent.path())?;
                }
            }
        }
        let id = getrandom::u32().map_err(io::Error::other)?;
        let path = dir.join(format!("{prefix}{id:08x}"));
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { path, listener })
    }
    pub(crate) fn listen(&self, tx: SyncSender<Message>) -> io::Result<()> {
        let listener = self.listener.try_clone()?;
        let leases = std::sync::Arc::new(std::sync::Mutex::new(Groups::default()));
        thread::spawn(move || {
            for socket in listener.incoming().flatten() {
                let tx = tx.clone();
                let leases = leases.clone();
                thread::spawn(move || {
                    let _ = serve(socket, tx, &leases);
                });
            }
        });
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
#[derive(Default)]
struct Groups(std::collections::HashMap<String, (Option<String>, UnixStream)>);
impl Groups {
    fn register(
        &mut self,
        id: &str,
        parent: Option<String>,
        socket: &UnixStream,
    ) -> io::Result<()> {
        if id.is_empty()
            || id.len() > 128
            || self.0.contains_key(id)
            || parent.as_ref().is_some_and(|p| !self.0.contains_key(p))
        {
            return Err(io::Error::other("The launching OX process has closed"));
        }
        self.0.insert(id.to_owned(), (parent, socket.try_clone()?));
        Ok(())
    }
    fn retire(&mut self, id: &str) {
        let mut remove = vec![id.to_owned()];
        while let Some(id) = remove.pop() {
            if let Some((_, stream)) = self.0.remove(&id) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                remove.extend(
                    self.0
                        .iter()
                        .filter(|(_, (parent, _))| parent.as_ref() == Some(&id))
                        .map(|(k, _)| k.clone()),
                );
            }
        }
    }
}
fn reply(socket: &mut UnixStream, result: Result<serde_json::Value, String>) -> io::Result<()> {
    let body = serde_json::to_vec(&match result {
        Ok(value) => serde_json::json!({"result": value}),
        Err(error) => serde_json::json!({"error": error}),
    })?;
    socket.write_all(&(body.len() as u32).to_be_bytes())?;
    socket.write_all(&body)
}
fn answer(
    socket: &mut UnixStream,
    tx: &SyncSender<Message>,
    caller: Caller,
    request: Gateway,
    seconds: u64,
) -> io::Result<()> {
    let (done, result) = mpsc::sync_channel(1);
    tx.send(Message::Gateway {
        caller,
        request: Box::new(request),
        reply: Replier::Channel(done),
    })
    .map_err(io::Error::other)?;
    let result = result
        .recv_timeout(Duration::from_secs(seconds))
        .unwrap_or_else(|_| Err("The session host did not answer in time.".into()));
    reply(socket, result)
}
fn serve(
    mut socket: UnixStream,
    tx: SyncSender<Message>,
    groups: &std::sync::Mutex<Groups>,
) -> io::Result<()> {
    same_user(&socket)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    match frame(&mut socket, 16384)? {
        Request::WorkerPlacement => socket.write_all(&[1])?,
        Request::Lease { id, parent } => {
            groups.lock().unwrap().register(&id, parent, &socket)?;
            let result = (|| {
                socket.write_all(&[1])?;
                socket.set_read_timeout(None)?;
                socket.read(&mut [0]).map(|_| ())
            })();
            groups.lock().unwrap().retire(&id);
            result?;
        }
        Request::Launch {
            scope,
            launch,
            mut start,
        } => {
            let hint = Some(std::mem::take(&mut start.parent)).filter(|p| !p.is_empty());
            let caller = Caller::Hosted {
                scope,
                hint,
                launch,
            };
            answer(&mut socket, &tx, caller, Gateway::Launch(start), 75)?;
        }
        Request::Call {
            scope,
            launch,
            parent,
            call,
        } => {
            let caller = Caller::Hosted {
                scope,
                hint: parent,
                launch,
            };
            let request = crate::workers::parent_call(call).map(Gateway::Call);
            match request {
                // Transcript loading and live peer discovery can outlast a local call.
                Ok(
                    request @ Gateway::Call(
                        ox_desk_core::origin::ParentCall::ReadThread { .. }
                        | ox_desk_core::origin::ParentCall::ListHosts,
                    ),
                ) => answer(&mut socket, &tx, caller, request, 45)?,
                Ok(request) => answer(&mut socket, &tx, caller, request, 10)?,
                Err(error) => reply(&mut socket, Err(error))?,
            }
        }
        Request::MemoryReceipt {
            scope,
            parent,
            lease,
        } => {
            if !groups.lock().unwrap().0.contains_key(&lease) {
                return Ok(());
            }
            let (done, result) = mpsc::sync_channel(1);
            tx.send(Message::MemoryBegin {
                scope,
                hint: parent,
                reply: done,
            })
            .map_err(io::Error::other)?;
            let Ok(Some(turn)) = result.recv_timeout(Duration::from_secs(2)) else {
                return Ok(());
            };
            socket.write_all(&[1])?;
            socket.set_read_timeout(Some(Duration::from_secs(30)))?;
            let outcome: ox_memory::Receipt = frame(&mut socket, 512)?;
            if groups.lock().unwrap().0.contains_key(&lease)
                && !matches!(outcome, ox_memory::Receipt::Recalled { count } if count > 20)
            {
                tx.send(Message::MemoryFinish(turn, outcome))
                    .map_err(io::Error::other)?;
            }
        }
    }
    Ok(())
}

fn frame<T: serde::de::DeserializeOwned>(socket: &mut UnixStream, limit: usize) -> io::Result<T> {
    let mut size = [0; 4];
    socket.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size > limit {
        return Err(io::Error::other("Worker lease frame too large"));
    }
    let mut body = vec![0; size];
    socket.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loss_of_a_helper_retires_only_its_descendants() {
        let (a, _) = UnixStream::pair().unwrap();
        let (b, mut child) = UnixStream::pair().unwrap();
        let mut groups = Groups::default();
        groups.register("root", None, &a).unwrap();
        groups.register("child", Some("root".into()), &b).unwrap();
        groups
            .register("grandchild", Some("child".into()), &a)
            .unwrap();
        groups.register("other", None, &a).unwrap();
        groups.retire("root");
        assert_eq!(child.read(&mut [0]).unwrap(), 0);
        assert_eq!(groups.0.len(), 1);
        assert!(groups.0.contains_key("other"));
        assert!(groups.register("late", Some("child".into()), &a).is_err());
    }
}
