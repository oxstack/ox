use super::{Adapter, Error, Profile, Result};
use std::{
    io::{Read as _, Write as _},
    net::Shutdown,
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt as _},
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "../../../../src/opencode_host_protocol.rs"]
mod protocol;

/// Owns the gateway helper, stock process group, and attested route adapter.
/// Starting or dropping this value may wait for I/O and belongs on a worker.
pub struct Hosted {
    pub adapter: Adapter,
    _process: Process,
}

impl Hosted {
    pub fn start(ox: &Path, profile: Profile) -> Result<Self> {
        if !ox.is_absolute() {
            return Err(Error::InvalidInput(
                "OpenCode host helper must have an absolute path",
            ));
        }
        let (control, peer) = UnixStream::pair()?;
        control.set_write_timeout(Some(Duration::from_secs(5)))?;
        control.set_read_timeout(Some(Duration::from_secs(35)))?;
        let child = Command::new(ox)
            .arg(protocol::COMMAND)
            .current_dir(profile.owner().project())
            .stdin(Stdio::from(OwnedFd::from(peer)))
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()?;
        let mut process = Process { child, control };
        let request = protocol::Launch {
            profile: profile.root().to_owned(),
            model: profile.owner().billing_alias().to_owned(),
        };
        process.control.write_all(&protocol::frame(&request)?)?;
        let mut header = [0; 4];
        process.control.read_exact(&mut header)?;
        let mut bytes = vec![0; protocol::payload_size(header)?];
        process.control.read_exact(&mut bytes)?;
        let reply: protocol::Ready = serde_json::from_slice(&bytes)?;
        let adapter = Adapter::connect(profile, reply.address, reply.password)?;
        Ok(Self {
            adapter,
            _process: process,
        })
    }
}

struct Process {
    child: Child,
    control: UnixStream,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.control.shutdown(Shutdown::Both);
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
        // SAFETY: this PID is the private group leader we created and has not
        // been reaped. Unlike a single-process kill, this includes the server.
        unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}
