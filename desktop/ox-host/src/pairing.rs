//! Same-user administration for a host without a desktop session.
use ox_host_service::Paths;
use ox_host_web::{
    access::{self, Operation},
    admin::{self, DeviceClass, Request},
};
use std::{
    io::{self, IsTerminal},
    process::Command,
};

const PAIR_USAGE: &str = "Usage: ox host pair [--terminal | --worker-host | --window] [--for LOGIN] [--port HTTPS_PORT] [--listen-port LOCAL_PORT]
  --terminal          grant terminal access on this host to the device that redeems the invitation
  --worker-host       create an invitation for another OX host to launch workers here
  --window            wait five minutes for any device on the tailnet to pair, with no code to carry
  --for LOGIN         only this Tailscale login may redeem it (default: any login the tailnet lets reach this host)
  --port PORT         Tailscale Serve HTTPS port (default 8443, range 1024..65535)
  --listen-port PORT  local loopback listener port (default 48162, range 1024..65535)";

pub struct PairOptions {
    pub operation: Operation,
    pub https_port: Option<u16>,
    pub loopback_port: Option<u16>,
    pub for_login: Option<String>,
}

fn parse_port(flag: &str, value: &str) -> Result<u16, String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("{flag} takes a port from 1024 to 65535."))?;
    if !ox_host_web::serve::valid_https_port(port) {
        return Err(format!("{flag} takes a port from 1024 to 65535."));
    }
    Ok(port)
}

pub fn parse_pair_args(args: &[String]) -> Result<Option<PairOptions>, String> {
    let mut terminal = false;
    let mut worker = false;
    let mut window = false;
    let mut https_port = None;
    let mut loopback_port = None;
    let mut for_login = None;
    let mut iter = args.iter().map(String::as_str);
    while let Some(arg) = iter.next() {
        match arg {
            "-h" | "--help" => {
                println!("{PAIR_USAGE}");
                return Ok(None);
            }
            "--terminal" => terminal = true,
            "--worker-host" => worker = true,
            "--window" => window = true,
            "--for" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--for takes a Tailscale login.".to_owned())?;
                for_login = Some(parse_login(value)?);
            }
            _ if arg.starts_with("--for=") => {
                for_login = Some(parse_login(&arg["--for=".len()..])?);
            }
            "--port" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--port takes a port from 1024 to 65535.".to_owned())?;
                https_port = Some(parse_port("--port", value)?);
            }
            "--listen-port" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--listen-port takes a port from 1024 to 65535.".to_owned())?;
                loopback_port = Some(parse_port("--listen-port", value)?);
            }
            _ if arg.starts_with("--port=") => {
                https_port = Some(parse_port("--port", &arg["--port=".len()..])?);
            }
            _ if arg.starts_with("--listen-port=") => {
                loopback_port = Some(parse_port("--listen-port", &arg["--listen-port=".len()..])?);
            }
            _ => return Err(PAIR_USAGE.into()),
        }
    }
    if [terminal, worker, window].iter().filter(|on| **on).count() > 1 {
        return Err("Choose one of --terminal, --worker-host or --window.".into());
    }
    access::validate_ports(https_port, loopback_port)?;
    let operation = if window {
        Operation::OpenWindow {
            login: for_login.clone(),
        }
    } else if worker {
        Operation::PairWorker { login: None }
    } else {
        Operation::Pair {
            terminal,
            login: None,
        }
    };
    Ok(Some(PairOptions {
        operation,
        https_port,
        loopback_port,
        for_login,
    }))
}

fn parse_login(value: &str) -> Result<String, String> {
    let login = value.trim();
    if login.is_empty()
        || login.len() > 254
        || !login.is_ascii()
        || login.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return Err("--for takes an ASCII Tailscale login such as you@example.com.".into());
    }
    Ok(login.to_owned())
}

pub fn pair_cli(paths: &Paths, args: &[String]) -> Result<(), String> {
    let Some(opts) = parse_pair_args(args)? else {
        return Ok(());
    };
    pair_with_ports(
        paths,
        opts.operation,
        opts.https_port,
        opts.loopback_port,
        opts.for_login,
    )
}

pub fn pair_with_ports(
    paths: &Paths,
    operation: Operation,
    https_port: Option<u16>,
    loopback_port: Option<u16>,
    for_login: Option<String>,
) -> Result<(), String> {
    let enable = if https_port.is_some() || loopback_port.is_some() {
        Operation::EnableOn {
            https_port,
            loopback_port,
        }
    } else {
        Operation::Enable
    };
    for _ in 0..2 {
        let report = access::run(paths, enable.clone())?;
        let Some(help) = report.denied else {
            break;
        };
        if !io::stdin().is_terminal() {
            return Err(format!(
                "{} Run ox host pair in an interactive terminal to authorize Tailscale Serve.",
                help.error
            ));
        }
        eprintln!(
            "Tailscale requires permission to enable private HTTPS. Running {}",
            help.suggested()
        );
        let lease = help.prepare()?;
        let status = Command::new("sudo")
            .args(help.args())
            .status()
            .map_err(|e| e.to_string())?;
        if let Some(error) = help.finish(status.code(), Some(lease))?.notice {
            return Err(error);
        }
    }
    let terminal = matches!(operation, Operation::Pair { terminal: true, .. });
    let worker = matches!(operation, Operation::PairWorker { .. });
    let window = matches!(operation, Operation::OpenWindow { .. });
    let reply = access::run_with(paths, operation, for_login)?.host;
    if window {
        let window = reply
            .window
            .ok_or("The host did not open a pairing window")?;
        println!(
            "Waiting five minutes for a device to pair. On the other machine, choose this host under Add host, or paste {} as a worker host.",
            reply.origin.as_deref().unwrap_or("its Serve address")
        );
        println!(
            "A request waits for you here: approve it in OX Desktop or on a paired phone, or run `ox host pending` and `ox host approve <id>`."
        );
        if let Some(login) = window.login.as_deref() {
            println!("Only {login} may pair.");
        } else {
            println!("Any device your tailnet lets reach this host may pair.");
        }
        return Ok(());
    }
    let pairing = reply
        .pairing
        .ok_or("The host did not issue an invitation")?;
    if let Some(login) = pairing.invited_login.as_deref() {
        println!("Only {login} may redeem this invitation.");
    } else {
        println!(
            "Anyone your tailnet lets reach this host can redeem this invitation. Pass --for LOGIN to name one."
        );
    }
    if terminal {
        println!(
            "This invitation grants terminal access on this host to the device that redeems it."
        );
    }
    if worker {
        println!(
            "Paste this invitation in Settings → Remote access → Worker host links on the sending host. It expires in five minutes.\n{}",
            pairing.url
        );
    } else {
        let qr = qrcode::QrCode::new(&pairing.url).map_err(|e| e.to_string())?;
        println!(
            "{}",
            qr.render::<qrcode::render::unicode::Dense1x2>()
                .module_dimensions(1, 1)
                .build()
        );
        println!(
            "Scan on your phone or paste this invitation into Add host on your desktop. It expires in five minutes.\n{}",
            pairing.url
        );
    }
    if let (Some(origin), Some(code)) = (reply.origin, pairing.short_code) {
        println!("Host  {origin}\nCode  {code}");
    }
    Ok(())
}

pub fn pending(paths: &Paths) -> Result<(), String> {
    let rows = access::status(paths).map_err(|e| e.to_string())?.pending;
    if rows.is_empty() {
        println!("No device is waiting to pair.");
    }
    for row in rows {
        println!(
            "{}  {}  {}  {}{}",
            row.id,
            row.label,
            row.login,
            match row.class {
                DeviceClass::Host => "worker host",
                DeviceClass::Phone => "phone",
                DeviceClass::Desktop => "desktop",
            },
            if row.approved {
                "  accepted, waiting for the device"
            } else {
                ""
            }
        );
    }
    Ok(())
}

pub fn approve(paths: &Paths, id: &str) -> Result<(), String> {
    access::run(paths, Operation::ApprovePending { id: id.to_owned() })
        .map_err(|e| e.to_string())?;
    println!("Accepted. The device collects its credential within a minute.");
    Ok(())
}

pub fn deny(paths: &Paths, id: &str) -> Result<(), String> {
    access::run(paths, Operation::DenyPending { id: id.to_owned() }).map_err(|e| e.to_string())?;
    println!("Declined.");
    Ok(())
}

pub fn devices(paths: &Paths) -> Result<(), String> {
    let reply = access::status(paths)?;
    if reply.devices.is_empty() {
        println!("No paired devices. Run ox host pair to add one.");
    }
    for device in reply.devices {
        let class = match device.class {
            DeviceClass::Host => "worker host",
            DeviceClass::Phone => "phone",
            DeviceClass::Desktop => "desktop",
        };
        if let Some(worker) = &device.worker {
            println!(
                "{}  {}  {class}  {}  launches {}  ceiling {}  memory sync {}",
                device.id,
                device.label,
                device.login,
                if worker.may_launch { "on" } else { "off" },
                worker.ceiling.name(),
                if worker.may_sync_memory { "on" } else { "off" }
            );
            continue;
        }
        let grant = if device.terminal {
            "terminal enabled"
        } else {
            "terminal off"
        };
        println!(
            "{}  {}  {class}  {}  {grant}",
            device.id, device.label, device.login
        );
    }
    Ok(())
}
fn match_device<'a>(rows: &'a [admin::Device], id: &str) -> Result<&'a admin::Device, String> {
    let mut matches = rows
        .iter()
        .filter(|d| d.id.starts_with(id) || d.label.eq_ignore_ascii_case(id));
    let device = matches
        .next()
        .filter(|_| !id.is_empty())
        .ok_or("Unknown device. Run ox host devices to list names and IDs.")?;
    if matches.next().is_some() {
        return Err("Several devices match. Use a longer ID prefix.".into());
    }
    Ok(device)
}
pub fn terminal(paths: &Paths, id: &str, enabled: bool) -> Result<(), String> {
    let devices = access::status(paths)?.devices;
    let device = match_device(&devices, id)?;
    admin::request(
        &paths.state_dir,
        &Request::TerminalGrant {
            id: device.id.clone(),
            enabled,
        },
    )
    .map_err(|e| e.to_string())?;
    println!(
        "Terminal access {} for {}.",
        if enabled { "enabled" } else { "disabled" },
        device.label
    );
    Ok(())
}
pub fn revoke(paths: &Paths, id: &str) -> Result<(), String> {
    let devices = access::status(paths)?.devices;
    let device = match_device(&devices, id)?;
    admin::request(
        &paths.state_dir,
        &Request::Revoke {
            id: device.id.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    println!("Revoked {}.", device.label);
    Ok(())
}

/// Grant or revoke a paired worker host's access to this host's non-local memories.
pub fn memory_sync(paths: &Paths, id: &str, enabled: bool) -> Result<(), String> {
    let devices = access::status(paths)?.devices;
    let device = match_device(&devices, id)?;
    let grant = device
        .worker
        .clone()
        .ok_or("That device is not a worker host link.")?;
    admin::request(
        &paths.state_dir,
        &Request::WorkerAccess {
            id: device.id.clone(),
            may_launch: grant.may_launch,
            ceiling: grant.ceiling,
            before: grant,
            may_sync_memory: Some(enabled),
            may_accept_moves: None,
        },
    )
    .map_err(|e| e.to_string())?;
    println!(
        "Memory sync {} for {}.",
        if enabled { "enabled" } else { "disabled" },
        device.label
    );
    Ok(())
}

pub fn profile(paths: &Paths, kind: &str, args: &[String]) -> io::Result<()> {
    use ox_host_web::host_profile::Edit;
    if matches!(args, [flag] if matches!(flag.as_str(), "-h" | "--help")) {
        println!("Usage: ox host name [NAME] or ox host labels [set LABEL…]");
        return Ok(());
    }
    access::status(paths).map_err(io::Error::other)?;
    let request = |edit| {
        admin::request(&paths.state_dir, &Request::HostProfile { edit })?
            .profile
            .ok_or_else(|| {
                io::Error::other("Update the running OX host to edit its name and labels.")
            })
    };
    let before = request(None)?;
    let mut after = before.clone();
    match (kind, args) {
        ("name", [name]) => after.name = name.clone(),
        ("name", []) | ("labels", []) => (),
        ("labels", [set, labels @ ..]) if set == "set" => after.labels = labels.to_vec(),
        _ => {
            return Err(io::Error::other(
                "Usage: ox host name [NAME] or ox host labels [set LABEL…]",
            ));
        }
    }
    let profile = if after == before {
        before
    } else {
        request(Some(Edit {
            before,
            after: after.normalized()?,
        }))?
    };
    if kind == "name" {
        println!(
            "{}",
            if profile.name.is_empty() {
                sysinfo::System::host_name().unwrap_or_default()
            } else {
                profile.name
            }
        );
    } else {
        println!("{}", profile.labels.join(", "));
    }
    Ok(())
}

pub fn mode_policy(paths: &Paths, args: &[String]) -> io::Result<()> {
    use ox_desk_core::host_policy::HostPolicy;
    const HELP: &str = "ox host policy [set POLICY]

Policies: follow-interface (default), always-read, always-write, always-classifier, always-yolo, yolo-asks-every-time.";
    let policy = match args {
        [help] if help == "--help" || help == "-h" => {
            println!("{HELP}");
            return Ok(());
        }
        [] => HostPolicy::read(&paths.state_dir)?,
        [set, name] if set == "set" => {
            let policy = HostPolicy::parse(name)?;
            policy.save(&paths.state_dir)?;
            policy
        }
        _ => return Err(io::Error::other(HELP)),
    };
    println!("{}", policy.label());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_cli_parses_ports_and_rejects_conflicting_or_privileged_values() {
        let args = |words: &[&str]| {
            parse_pair_args(&words.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        };
        let plain = args(&[]).unwrap().unwrap();
        assert!(matches!(
            plain.operation,
            Operation::Pair {
                terminal: false,
                login: None
            }
        ));
        assert_eq!(plain.for_login, None);
        assert_eq!((plain.https_port, plain.loopback_port), (None, None));

        let custom = args(&["--terminal", "--port", "9443", "--listen-port=49100"])
            .unwrap()
            .unwrap();
        assert!(matches!(
            custom.operation,
            Operation::Pair {
                terminal: true,
                login: None
            }
        ));
        assert_eq!(
            (custom.https_port, custom.loopback_port),
            (Some(9443), Some(49100))
        );

        let worker = args(&["--worker-host", "--port=10443"]).unwrap().unwrap();
        assert!(matches!(
            worker.operation,
            Operation::PairWorker { login: None }
        ));
        assert_eq!(worker.https_port, Some(10443));

        let named = args(&["--worker-host", "--for", "you@example.com"])
            .unwrap()
            .unwrap();
        assert_eq!(named.for_login.as_deref(), Some("you@example.com"));
        assert_eq!(
            args(&["--for=you@example.com"]).unwrap().unwrap().for_login,
            Some("you@example.com".into())
        );
        assert!(matches!(
            args(&["--for", "you@example.com"])
                .unwrap()
                .unwrap()
                .operation,
            Operation::Pair {
                terminal: false,
                login: None
            }
        ));
        assert!(args(&["--for"]).is_err());
        assert!(args(&["--for", "two words"]).is_err());
        assert!(args(&["--for", ""]).is_err());

        let window = args(&["--window", "--for", "you@example.com"])
            .unwrap()
            .unwrap();
        assert!(matches!(
            window.operation,
            Operation::OpenWindow { login: Some(ref login) } if login == "you@example.com"
        ));
        assert_eq!(
            args(&["--window"]).unwrap().unwrap().for_login,
            None,
            "an open window needs no login"
        );
        assert!(args(&["--window", "--terminal"]).is_err());
        assert!(args(&["--window", "--worker-host"]).is_err());

        assert!(args(&["--help"]).unwrap().is_none());
        assert!(args(&["--terminal", "--worker-host"]).is_err());
        assert!(args(&["--port", "443"]).is_err());
        assert!(args(&["--listen-port", "80"]).is_err());
        assert!(args(&["--port", "9443", "--listen-port", "9443"]).is_err());
        assert!(args(&["--port"]).is_err());
    }
}
