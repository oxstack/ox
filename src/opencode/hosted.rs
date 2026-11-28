//! Start one stock OpenCode server for a host-owned billing route.

use super::{Gateway, command, launch_config, load};
use crate::{
    Mode, clients,
    opencode_host_protocol::{Launch, Ready, frame, payload_size},
    provider::RelayError,
    serve,
};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io,
    net::{Ipv4Addr, SocketAddrV4},
    os::{
        fd::AsFd as _,
        unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    },
    path::Path,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::UnixStream,
    signal::unix::{SignalKind, signal},
    time::timeout,
};

const BOOT_LIMIT: Duration = Duration::from_secs(30);
const CLOSE_LIMIT: Duration = Duration::from_secs(2);
const DISPOSE_LIMIT: Duration = Duration::from_secs(1);

pub(crate) async fn run(home: &Path) -> Result<u8, RelayError> {
    // SAFETY: getpgrp and getpid only query this process. Require a private
    // group before either this helper or the host can signal that group.
    if unsafe { libc::getpgrp() != libc::getpid() } {
        return Err("the OpenCode host helper requires its own process group".into());
    }
    let input: std::os::unix::net::UnixStream = io::stdin().as_fd().try_clone_to_owned()?.into();
    input.peer_addr()?;
    input.set_nonblocking(true)?;
    let (mut owner, mut ready) = UnixStream::from_std(input)?.into_split();
    let launch = timeout(BOOT_LIMIT, read_launch(&mut owner)).await??;
    let meta = fs::symlink_metadata(&launch.profile)?;
    if launch.profile.canonicalize()? != launch.profile
        || !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        return Err("the OpenCode host profile must be a canonical private directory".into());
    }
    let profile_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(launch.profile.join("owner.lock"))?;
    profile_lock.try_lock()?;
    let Gateway {
        mut relay,
        env_refs,
        token,
    } = load(home).await?;
    let model = launch.model.as_str();
    if !relay
        .launch_models()
        .iter()
        .any(|(alias, _)| *alias == model)
    {
        return Err("the assigned OpenCode model has no configured OX API route".into());
    }
    if let Mode::Codeox {
        routes, catalog, ..
    } = &mut relay.mode
    {
        routes.retain_exact(model);
        if let Some(entries) = catalog["models"].as_array_mut() {
            entries.retain(|entry| entry["slug"] == model);
        }
    }
    relay.prewarm(model);
    clients::prepare_opencode_profile(&launch.profile)?;
    let (listener, config) = launch_config(
        &launch.profile,
        &relay,
        &BTreeSet::from([model]),
        model,
        Some(&token),
    )
    .await?;
    let password = crate::random_token()?;
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let args = ["serve", "--hostname", "127.0.0.1", "--port", "0"].map(Into::into);
    let mut child = command(&args, &launch.profile, env_refs, None, &config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .env("OPENCODE_SERVER_USERNAME", "opencode")
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .spawn()
        .map_err(|error| crate::client_launch_error("opencode", error))?;
    let mut output = child
        .stdout
        .take()
        .ok_or("OpenCode startup output is unavailable")?;
    let mut owner_byte = [0];
    let mut assigned = None;
    let result: Result<(), RelayError> = tokio::select! {
        result = async {
            let address = timeout(BOOT_LIMIT, assigned_address(&mut output)).await??;
            assigned = Some(address);
            ready.write_all(&frame(&Ready { address, password: password.clone() })?).await?;
            // Later stock logs must not fill the startup pipe.
            tokio::spawn(async move { tokio::io::copy(&mut output, &mut tokio::io::sink()).await });
            child.wait().await?;
            Ok(())
        } => result,
        _ = owner.read(&mut owner_byte) => Ok(()),
        _ = term.recv() => Ok(()),
        _ = interrupt.recv() => Ok(()),
        result = serve(listener, Arc::new(relay)) => Err(result.unwrap_err().into()),
    };
    if let Some(address) = assigned {
        // Stock tools can have their own process groups. Ask OpenCode to
        // dispose them before the fallback that reaps our private group.
        let _ = timeout(DISPOSE_LIMIT, dispose(address, &password)).await;
    }
    // SAFETY: this helper is still the verified leader of its private group.
    // Its SIGTERM handler remains alive while the child flushes and exits.
    unsafe { libc::kill(0, libc::SIGTERM) };
    let _ = timeout(CLOSE_LIMIT, child.wait()).await;
    drop(config);
    // SAFETY: the group still belongs to this helper. This also clears any
    // remaining processes in the group if they did not handle SIGTERM.
    unsafe { libc::kill(0, libc::SIGKILL) };
    result.map(|()| 0)
}

async fn dispose(address: SocketAddrV4, password: &str) -> Result<(), reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("http://{address}/global/dispose"))
        .basic_auth("opencode", Some(password))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn read_launch(input: &mut (impl AsyncRead + Unpin)) -> io::Result<Launch> {
    let mut header = [0; 4];
    input.read_exact(&mut header).await?;
    let mut body = vec![0; payload_size(header)?];
    input.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

async fn assigned_address(output: &mut (impl AsyncRead + Unpin)) -> io::Result<SocketAddrV4> {
    let mut output = BufReader::new(output).take(16 * 1024);
    let mut line_buffer = String::new();
    while output.read_line(&mut line_buffer).await? != 0 {
        let line = line_buffer.trim();
        if let Some(address) = line
            .strip_prefix("opencode server listening on http://")
            .or_else(|| line.strip_prefix("server listening on http://"))
        {
            let address: SocketAddrV4 = address
                .parse()
                .map_err(|_| io::Error::other("OpenCode did not report an IPv4 endpoint"))?;
            if *address.ip() == Ipv4Addr::LOCALHOST && address.port() != 0 {
                return Ok(address);
            }
            break;
        }
        line_buffer.clear();
    }
    Err(io::Error::other(
        "OpenCode failed to report its assigned loopback endpoint",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_the_observed_stock_listening_banners() {
        for mut banner in [
            b"opencode server listening on http://127.0.0.1:4123
"
            .as_slice(),
            b"server listening on http://127.0.0.1:4123
"
            .as_slice(),
        ] {
            assert_eq!(
                assigned_address(&mut banner).await.unwrap(),
                "127.0.0.1:4123".parse().unwrap()
            );
        }
    }

    #[tokio::test]
    async fn requires_an_assigned_ipv4_loopback_port() {
        for mut banner in [
            b"server listening on http://0.0.0.0:4123
"
            .as_slice(),
            b"server listening on http://127.0.0.1:0
"
            .as_slice(),
        ] {
            assert!(assigned_address(&mut banner).await.is_err());
        }
    }
}
