//! Private startup record exchanged over the host's OpenCode control socket.
use serde::{Deserialize, Serialize};
use std::{io, net::SocketAddrV4, path::PathBuf};

pub const COMMAND: &str = "--ox-opencode-host-v1";
const MAX_FRAME: usize = 16 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub profile: PathBuf,
    pub model: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub address: SocketAddrV4,
    pub password: String,
}

pub fn frame(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; 4];
    serde_json::to_writer(&mut bytes, value)?;
    let size = bytes.len() - 4;
    if size > MAX_FRAME {
        return Err(io::Error::other("OpenCode startup frame is too large"));
    }
    bytes[..4].copy_from_slice(&(size as u32).to_be_bytes());
    Ok(bytes)
}

pub fn payload_size(header: [u8; 4]) -> io::Result<usize> {
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::other("Invalid OpenCode startup frame length"));
    }
    Ok(size)
}
