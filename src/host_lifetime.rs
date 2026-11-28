//! The host drops the peer socket even on SIGKILL. Each host-owned OX helper
//! is its group's leader and terminates that group if the owner disappears.
use serde_json::{Value, json};
use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    thread,
    time::Duration,
};
static LEASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
const PARENT_LEASE: &str = "OX_HOST_PARENT_LEASE";
const ENDPOINT: &str = "OX_HOST_LIFETIME_SOCKET";

fn request(addr: &std::ffi::OsStr, body: Value) -> io::Result<UnixStream> {
    let mut socket = UnixStream::connect(Path::new(addr))?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;
    write_frame(&mut socket, &body)?;
    let mut ack = [0];
    socket.read_exact(&mut ack)?;
    if ack[0] != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The session host refused this OX helper.",
        ));
    }
    Ok(socket)
}
pub(crate) fn supervise() -> io::Result<()> {
    let Some(addr) = env::var_os(ENDPOINT) else {
        return Ok(());
    };
    if unsafe { libc::getpgrp() != libc::getpid() } {
        return Ok(());
    } // internal probes can share the launch group.
    let id = crate::random_token().map_err(|e| io::Error::other(e.to_string()))?;
    let mut socket = request(
        &addr,
        json!({"type":"lease","id":id,"parent":env::var(PARENT_LEASE).ok()}),
    )?;
    LEASE
        .set(id)
        .map_err(|_| io::Error::other("This helper already has a host lease"))?;
    socket.set_read_timeout(None)?;
    thread::Builder::new()
        .name("ox-host-lifetime".into())
        .spawn(move || {
            let _ = socket.read_exact(&mut [0u8; 1]);
            // Signal this helper's private group. No PID comes from disk or input.
            unsafe {
                libc::kill(0, libc::SIGKILL);
            }
        })?;
    Ok(())
}
pub(crate) fn hosted() -> bool {
    env::var_os(ENDPOINT).is_some()
}

/// This helper's own lease id, once `supervise` has registered it with the host.
pub(crate) fn own_lease() -> Option<&'static str> {
    LEASE.get().map(String::as_str)
}

/// Lets a helper this process starts register under this helper's lease, so the
/// host retires it when this helper goes. Only a hosted helper with a lease passes it.
pub(crate) fn set_parent_lease(command: &mut tokio::process::Command) {
    set_parent_lease_from(command, hosted(), own_lease());
}

fn set_parent_lease_from(command: &mut tokio::process::Command, hosted: bool, lease: Option<&str>) {
    if let (true, Some(lease)) = (hosted, lease) {
        command.env(PARENT_LEASE, lease);
    }
}

pub(crate) fn isolate_stock(command: &mut tokio::process::Command) {
    for key in [
        ENDPOINT,
        PARENT_LEASE,
        "OX_HOST_WORKER_ROUTE",
        "OX_HOST_WORKER_PARENT",
    ] {
        command.env_remove(key);
    }
}

/// A gateway's way to the session host, if one serves this launch. A helper the
/// host started uses its private lifetime endpoint. Any other helper, such as
/// `ox claude` in a terminal, uses the per-user host socket when a host is running.
pub(crate) enum HostLink {
    Hosted {
        addr: std::ffi::OsString,
        scope: Value,
    },
    External {
        socket: std::path::PathBuf,
    },
}

/// Private reporting channel for one native memory operation. Opening or using
/// it is best effort so older hosts and terminal agents still use memory.
pub(crate) struct MemoryReceipt(UnixStream);

impl MemoryReceipt {
    pub(crate) async fn begin(cwd: &Path, thread: Option<&str>) -> Option<Self> {
        let lease = own_lease()?;
        let Some(HostLink::Hosted { addr, scope }) =
            HostLink::find(cwd, crate::provider::WorkersHost::Never).ok()?
        else {
            return None;
        };
        let parent = env::var("OX_HOST_WORKER_PARENT")
            .ok()
            .or_else(|| thread.map(str::to_owned));
        tokio::task::spawn_blocking(move || {
            let socket = request(
                &addr,
                json!({"type":"memory_receipt","scope":scope,"parent":parent,"lease":lease}),
            )
            .ok()?;
            Some(Self(socket))
        })
        .await
        .ok()
        .flatten()
    }

    pub(crate) fn finish(mut self, receipt: ox_memory::Receipt) {
        let _ = write_frame(&mut self.0, &receipt);
    }
}

fn write_frame(socket: &mut UnixStream, value: &impl serde::Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    socket.write_all(&(body.len() as u32).to_be_bytes())?;
    socket.write_all(&body)
}

impl HostLink {
    /// `policy` applies only to a helper the host did not start: a helper the host
    /// runs always hands its workers to that host.
    pub(crate) fn find(
        cwd: &Path,
        policy: crate::provider::WorkersHost,
    ) -> Result<Option<Self>, String> {
        if let Some(addr) = env::var_os(ENDPOINT) {
            if LEASE.get().is_none() {
                return Err("This OX worker gateway is not attached to a live host".into());
            }
            let project = cwd.canonicalize().map_err(|e| e.to_string())?;
            let route: Value = serde_json::from_str(
                &env::var("OX_HOST_WORKER_ROUTE")
                    .map_err(|_| "This helper is not attached to a host billing route")?,
            )
            .map_err(|e| e.to_string())?;
            return Ok(Some(Self::Hosted {
                addr,
                scope: json!({"project": project, "route": route}),
            }));
        }
        if policy == crate::provider::WorkersHost::Never {
            return Ok(None);
        }
        let socket = host_socket();
        Ok(socket
            .filter(|s| UnixStream::connect(s).is_ok())
            .map(|socket| Self::External { socket }))
    }

    /// Send one launch (`call` is None) or parent call and wait for the host's answer.
    /// `thread` is the calling native thread reported by the stock client in the
    /// MCP request metadata. A session-scoped helper's own session takes precedence.
    pub(crate) async fn send(
        self,
        launch: String,
        caller: Value,
        thread: Option<String>,
        start: Option<Value>,
        call: Option<Value>,
    ) -> Result<Value, String> {
        let hint = env::var("OX_HOST_WORKER_PARENT").ok().or(thread);
        let needs_host_lookup = call
            .as_ref()
            .is_some_and(|c| matches!(c["op"].as_str(), Some("read_thread" | "list_hosts")));
        let wait = Duration::from_secs(if start.is_some() {
            80
        } else if needs_host_lookup {
            50
        } else {
            15
        });
        let placement = start.as_ref().is_some_and(|s| {
            !s["placement"].is_null()
                || !s["needs"].is_null()
                || s["host"]
                    .as_str()
                    .is_some_and(|h| h == "auto" || h.starts_with("label:"))
        });
        let explicit_parent = start
            .as_ref()
            .is_some_and(|s| s["host"] == "parent" && !placement);
        let update =
            "Update the parent OX host to choose a worker host by requirements or project rules.";
        tokio::task::spawn_blocking(move || match self {
            Self::Hosted { addr, scope } => {
                let mut start = start;
                if (placement || explicit_parent)
                    && request(&addr, json!({"type":"worker_placement"})).is_err()
                {
                    if placement {
                        return Err(update.into());
                    }
                    if let Some(v) = start.as_mut().and_then(Value::as_object_mut) {
                        v.remove("host");
                    }
                }
                let body = match (start, call) {
                    (Some(mut start), _) => {
                        start["parent"] = hint.unwrap_or_default().into();
                        json!({"type":"launch","scope":scope,"launch":launch,"start":start})
                    }
                    (None, call) => json!({"type":"call","scope":scope,"launch":launch,
                        "parent":hint,"call":call}),
                };
                let mut socket =
                    UnixStream::connect(Path::new(&addr)).map_err(|e| e.to_string())?;
                socket
                    .set_read_timeout(Some(wait))
                    .map_err(|e| e.to_string())?;
                let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
                socket
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .map_err(|e| e.to_string())?;
                socket.write_all(&body).map_err(|e| e.to_string())?;
                let mut size = [0; 4];
                socket
                    .read_exact(&mut size)
                    .map_err(|_| "The session host closed the worker request.")?;
                let n = u32::from_be_bytes(size) as usize;
                if n > 16 * 1024 * 1024 {
                    return Err("The session host sent an oversized worker reply.".into());
                }
                let mut data = vec![0; n];
                socket.read_exact(&mut data).map_err(|e| e.to_string())?;
                decode(serde_json::from_slice(&data).map_err(|e| e.to_string())?)
            }
            Self::External { socket } => {
                let mut stream = UnixStream::connect(&socket).map_err(|e| e.to_string())?;
                stream
                    .set_read_timeout(Some(wait))
                    .map_err(|e| e.to_string())?;
                let id = format!(
                    "worker-{}",
                    crate::random_token().map_err(|e| e.to_string())?
                );
                let frame = match (start, call) {
                    (Some(start), _) => {
                        json!({"type":"worker_launch","id":id,"caller":caller,"start":start})
                    }
                    (None, call) => {
                        json!({"type":"worker_call","id":id,"caller":caller,"call":call})
                    }
                };
                stream
                    .write_all(b"{\"type\":\"hello\",\"version\":1}\n")
                    .map_err(|e| e.to_string())?;
                let mut reader = io::BufReader::new(stream);
                let mut frame = frame;
                if placement || explicit_parent {
                    let mut hello = String::new();
                    io::BufRead::read_line(&mut reader, &mut hello).map_err(|e| e.to_string())?;
                    let hello: Value = serde_json::from_str(&hello).map_err(|e| e.to_string())?;
                    if hello["type"] != "hello" {
                        return Err(hello["message"]
                            .as_str()
                            .unwrap_or("The session host did not return a hello.")
                            .to_owned());
                    }
                    if !hello["capabilities"]
                        .as_array()
                        .is_some_and(|caps| caps.iter().any(|c| c == "worker_placement"))
                    {
                        if placement {
                            return Err(update.into());
                        }
                        if let Some(s) = frame["start"].as_object_mut() {
                            s.remove("host");
                        }
                    }
                }
                let mut bytes = serde_json::to_vec(&frame).map_err(|e| e.to_string())?;
                bytes.push(b'\n');
                reader
                    .get_mut()
                    .write_all(&bytes)
                    .map_err(|e| e.to_string())?;
                for line in io::BufRead::lines(reader) {
                    let line =
                        line.map_err(|_| "The session host did not answer the worker request.")?;
                    let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    match frame["type"].as_str() {
                        Some("worker_reply") if frame["id"] == id.as_str() => {
                            return decode(frame);
                        }
                        Some("error") if frame["id"].is_null() || frame["id"] == id.as_str() => {
                            return Err(frame["message"]
                                .as_str()
                                .unwrap_or("The session host refused the worker request.")
                                .into());
                        }
                        _ => continue,
                    }
                }
                Err("The session host closed the worker request.".into())
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn decode(reply: Value) -> Result<Value, String> {
    match reply.get("error").and_then(Value::as_str) {
        Some(error) => Err(error.to_owned()),
        None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// The per-user host socket, computed exactly as the host computes it.
fn host_socket() -> Option<std::path::PathBuf> {
    crate::ps::socket_path()
}

#[cfg(test)]
mod parent_lease_tests {
    use super::*;

    fn parent_env(command: &tokio::process::Command) -> Option<String> {
        command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == PARENT_LEASE)
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn older_host_keeps_parent_starts_and_refuses_advanced_placement() {
        use std::{io::BufRead, os::unix::net::UnixListener};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            for advanced in [false, true] {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                let mut reader = io::BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                writeln!(
                    reader.get_mut(),
                    "{}",
                    json!({"type":"hello","capabilities":[]})
                )
                .unwrap();
                line.clear();
                let length = reader.read_line(&mut line).unwrap();
                if advanced {
                    assert_eq!(length, 0);
                    continue;
                }
                let start: Value = serde_json::from_str(&line).unwrap();
                assert!(start["start"].get("host").is_none());
                writeln!(
                    reader.get_mut(),
                    "{}",
                    json!({"type":"worker_reply","id":start["id"],"result":{"ok":true}})
                )
                .unwrap();
            }
        });
        for advanced in [false, true] {
            let target = if advanced { "auto" } else { "parent" };
            let result = HostLink::External {
                socket: path.clone(),
            }
            .send(
                "fixture".into(),
                json!({}),
                None,
                Some(json!({"host":target})),
                None,
            )
            .await;
            if advanced {
                assert!(result.unwrap_err().contains("Update the parent"));
            } else {
                assert_eq!(result.unwrap(), json!({"ok":true}));
            }
        }
        server.join().unwrap();
    }

    #[test]
    fn child_gets_own_lease_only_when_hosted() {
        let mut command = tokio::process::Command::new("ox");
        set_parent_lease_from(&mut command, true, Some("gateway-lease"));
        assert_eq!(parent_env(&command).as_deref(), Some("gateway-lease"));

        let mut unhosted = tokio::process::Command::new("ox");
        set_parent_lease_from(&mut unhosted, false, Some("gateway-lease"));
        assert_eq!(parent_env(&unhosted), None);

        let mut unleased = tokio::process::Command::new("ox");
        set_parent_lease_from(&mut unleased, true, None);
        assert_eq!(parent_env(&unleased), None);
    }

    #[test]
    fn isolate_stock_removes_parent_lease() {
        let mut command = tokio::process::Command::new("claude");
        set_parent_lease_from(&mut command, true, Some("gateway-lease"));
        isolate_stock(&mut command);
        assert_eq!(parent_env(&command), None);
    }
}
